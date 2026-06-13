//! buddy-bridged — a host-side BLE bridge that lets terminal Claude Code
//! sessions push permission prompts to a Claude desk-pet (M5StickC Plus S3).
//!
//! It plays the role the Claude desktop app plays for the device: it is the
//! single BLE central, holds a persistent connection to the Stick, sends
//! keepalive snapshots, and — when a `PreToolUse` hook asks — pushes a
//! `prompt` and waits for the physical A/B button decision to come back.
//!
//! Architecture:
//!   * One long-lived Unix-socket listener accepts approval requests from the
//!     `buddy-gate` hook (one connection per request).
//!   * A reconnecting BLE session task owns the link to the Stick. While the
//!     link is up it runs a sender loop (keepalive + prompt push) and a reader
//!     loop (parses `{"cmd":"permission",...}` decisions).
//!   * Shared state ties them together; requests resolve via a oneshot channel.
//!
//! If the Stick is absent the listener still answers every request with
//! `defer` immediately, so terminal Claude Code falls back to its normal
//! y/n prompt and nothing ever blocks on missing hardware.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use btleplug::api::{
    Central, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Manager, Peripheral};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Notify};
use uuid::Uuid;

const NUS_SERVICE: &str = "6e400001-b5a3-f393-e0a9-e50e24dcca9e";
const NUS_RX: &str = "6e400002-b5a3-f393-e0a9-e50e24dcca9e"; // write: host -> device
const NUS_TX: &str = "6e400003-b5a3-f393-e0a9-e50e24dcca9e"; // notify: device -> host

/// Keepalive cadence. The firmware treats >30s of silence as "disconnected"
/// and >15s as "bt inactive", so stay comfortably under both. Short on
/// purpose: each keepalive is acked by the firmware, so this is also the
/// liveness probe — 3s cadence lets the watchdog declare death in ~10s.
const KEEPALIVE: Duration = Duration::from_secs(3);
/// Largest single ATT write we send. macOS negotiates ~185-byte MTU; the
/// firmware reassembles by accumulating until '\n', so chunking mid-line is
/// fine as long as the newline only lands at the very end.
const WRITE_CHUNK: usize = 180;
/// A session counts as "currently connected" if it reported a turn within this
/// window. Its tokens drop out of the device counter once it goes quiet (e.g.
/// the terminal was closed).
const STATS_WINDOW: Duration = Duration::from_secs(15 * 60);
/// How long `"completed": true` stays in snapshots after a turn finishes.
/// The firmware's done jingle is edge-triggered, so duration only affects
/// how long the celebrate face shows; the first keepalive after expiry
/// clears the flag (firmware resets `recentlyCompleted` when absent).
const COMPLETED_PULSE: Duration = Duration::from_secs(4);
/// A turn with no `Stop` AND no PostToolUse heartbeat for this long was
/// interrupted (Esc/Ctrl-C — no hook fires for that). Short on purpose:
/// false demotions self-correct because the next heartbeat re-promotes.
const RUN_STALE: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, PartialEq)]
enum Decision {
    Allow,
    Deny,
    Defer,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Defer => "defer",
        }
    }
}

struct Pending {
    id: String,
    tool: String,
    hint: String,
    responder: oneshot::Sender<Decision>,
}

/// Per-session output-token tally, fed by the `Stop` hook each turn.
struct SessionStat {
    tokens: u64,
    today: u64,
    last_seen: Instant,
    /// Mid-turn: `UserPromptSubmit` seen, `Stop` not yet. Drives the snapshot
    /// `running` count (the firmware starts/stops BGM on its edges).
    running: bool,
    /// When `running` last flipped on — lets us expire crashed turns.
    run_started: Instant,
    /// Transcript JSONL path + how far we've scanned it. While running, the
    /// interrupt watcher tails this for "[Request interrupted by user]" —
    /// the only immediate signal an Esc/Ctrl-C interrupt produces.
    transcript: String,
    t_offset: u64,
}

impl SessionStat {
    fn new() -> Self {
        SessionStat {
            tokens: 0,
            today: 0,
            last_seen: Instant::now(),
            running: false,
            run_started: Instant::now(),
            transcript: String::new(),
            t_offset: 0,
        }
    }
}

struct Inner {
    /// At most one prompt is shown on the device at a time. A second concurrent
    /// request while this is `Some` is answered `defer` (falls back to terminal).
    pending: Option<Pending>,
    /// Recent command hints, newest first — surfaced as the snapshot `entries`.
    entries: VecDeque<String>,
    /// Output-token tallies keyed by session id; only recently-active ones
    /// (within STATS_WINDOW) are summed into the snapshot.
    sessions: HashMap<String, SessionStat>,
    /// While `Some` and in the future, snapshots carry `"completed": true`
    /// (the firmware plays the done jingle on the rising edge).
    completed_until: Option<Instant>,
}

struct Shared {
    inner: std::sync::Mutex<Inner>,
    /// Wakes the BLE sender loop to push a snapshot immediately (on a new prompt
    /// or when one clears) instead of waiting for the next keepalive tick.
    poke: Notify,
    connected: AtomicBool,
    /// Latched (not cleared on session end, unlike `connected`): this
    /// process reached `session up` at least once. After a real session
    /// dies we exit rather than rescan — see the note in main().
    session_was_up: AtomicBool,
    req_seq: AtomicU64,
    /// Last time any bytes arrived FROM the device. The firmware acks every
    /// keepalive, so while the link is truly alive this refreshes every ~7s.
    /// This is the only liveness signal CoreBluetooth can't fake.
    last_rx: std::sync::Mutex<Instant>,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Shared {
            inner: std::sync::Mutex::new(Inner {
                pending: None,
                entries: VecDeque::new(),
                sessions: HashMap::new(),
                completed_until: None,
            }),
            poke: Notify::new(),
            connected: AtomicBool::new(false),
            session_was_up: AtomicBool::new(false),
            req_seq: AtomicU64::new(1),
            last_rx: std::sync::Mutex::new(Instant::now()),
        })
    }
}

fn socket_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::Path::new(&home)
        .join(".claude")
        .join("buddy-bridge.sock")
}

#[tokio::main]
async fn main() -> Result<()> {
    let shared = Shared::new();

    // The IPC listener lives for the whole process, independent of BLE state.
    let sock = socket_path();
    if let Some(dir) = sock.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let _ = std::fs::remove_file(&sock); // clear a stale socket from a crash
    let listener = UnixListener::bind(&sock)?;
    eprintln!("[ipc] listening on {}", sock.display());

    let ipc_shared = shared.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let s = ipc_shared.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_request(stream, s).await {
                            eprintln!("[ipc] request error: {e:#}");
                        }
                    });
                }
                Err(e) => eprintln!("[ipc] accept error: {e}"),
            }
        }
    });

    // BLE connect loop owns the link to the Stick. In-process re-scanning
    // after a real session death is what wedges (the dead CBCentralManager
    // poisons the whole CoreBluetooth stack and the new scan never finds
    // anything) — so once a session that reached "session up" ends, exit
    // instead: launchd restarts us immediately and a fresh process connects
    // in 2-5s. The in-process retry only serves the never-connected case
    // (Stick off / out of range), where scanning is healthy.
    let ble_shared = shared.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = ble_loop(ble_shared.clone()).await {
                eprintln!("[ble] session ended: {e:#}");
            }
            ble_shared.connected.store(false, Ordering::SeqCst);
            if ble_shared.session_was_up.load(Ordering::SeqCst) {
                eprintln!("[ble] restarting process for a fresh BLE stack");
                std::process::exit(1);
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });

    // Interrupt watcher: no hook fires when the user interrupts a turn with
    // Esc/Ctrl-C — the only immediate trace is a "[Request interrupted by
    // user]" entry appended to the session transcript. Tail the transcripts
    // of running sessions (1s cadence) and demote on the marker, so the BGM
    // stops within ~2s instead of waiting out RUN_STALE.
    let watch_shared = shared.clone();
    tokio::spawn(async move {
        use std::io::{Read, Seek, SeekFrom};
        const MARKER: &[u8] = b"Request interrupted by user";
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let targets: Vec<(String, String, u64)> = {
                let g = watch_shared.inner.lock().unwrap();
                g.sessions
                    .iter()
                    .filter(|(_, s)| s.running && !s.transcript.is_empty())
                    .map(|(k, s)| (k.clone(), s.transcript.clone(), s.t_offset))
                    .collect()
            };
            for (key, path, off) in targets {
                let Ok(meta) = std::fs::metadata(&path) else { continue };
                let size = meta.len();
                if size <= off {
                    continue;
                }
                // Bounded read of just the appended bytes (a 1s slice of a
                // transcript is small; 4MB cap keeps a pathological turn
                // from stalling the loop — the rest is picked up next tick).
                let want = (size - off).min(4 << 20) as usize;
                let Ok(mut f) = std::fs::File::open(&path) else { continue };
                if f.seek(SeekFrom::Start(off)).is_err() {
                    continue;
                }
                let mut buf = vec![0u8; want];
                let n = f.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let hit = buf[..n].windows(MARKER.len()).any(|w| w == MARKER);
                let mut g = watch_shared.inner.lock().unwrap();
                if let Some(s) = g.sessions.get_mut(&key) {
                    s.t_offset = off + n as u64;
                    if hit && s.running {
                        s.running = false; // interrupted — no done jingle
                        eprintln!("[run] interrupt detected for {key}");
                        drop(g);
                        watch_shared.poke.notify_one();
                    }
                }
            }
        }
    });

    // Process-level backstop, independent of the BLE task: btleplug's macOS
    // tasks have been observed to die silently (no panic in the log, workers
    // idle), leaving `connected` stuck true and nothing running to recover.
    // No task inside the BLE machinery can be trusted to catch that — so the
    // main task watches device silence and exits; launchd's KeepAlive brings
    // up a fresh process with a fresh CBCentralManager.
    let backstop = {
        let shared = shared.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let quiet = shared.last_rx.lock().unwrap().elapsed();
                let connected = shared.connected.load(Ordering::SeqCst);
                // 20s: past the in-session watchdog's 10s, so this only fires
                // when that clean path is already dead.
                if connected && quiet > Duration::from_secs(20) {
                    eprintln!(
                        "[ble] zombie: 'connected' but device silent {}s — restarting process",
                        quiet.as_secs()
                    );
                    break;
                }
                // Disconnected with no device data for 2 min: either the
                // Stick is off (restart is harmless, we just rescan) or the
                // scan machinery wedged — observed in practice: after the
                // peer reboots, Manager::new()/start_scan can hang forever in
                // the poisoned CoreBluetooth stack, logging nothing. A
                // process restart every 2 min while the Stick is away costs
                // nothing; staying wedged costs the reconnect.
                if !connected && quiet > Duration::from_secs(120) {
                    eprintln!("[ble] no device for 2m — restarting process to refresh BLE state");
                    break;
                }
            }
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = backstop => {
            let _ = std::fs::remove_file(&sock);
            std::process::exit(1);
        }
    }
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

// ---------------------------------------------------------------------------
// IPC: one connection == one approval request.
// ---------------------------------------------------------------------------

async fn handle_request(stream: UnixStream, shared: Arc<Shared>) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);

    // A `Stop`-hook stats update — record tokens and return (no decision). This
    // is handled before the connected check so tallies survive a BLE blip.
    if let Some(stats) = req.get("stats") {
        let session = stats["session"].as_str().unwrap_or("");
        if !session.is_empty() {
            let mut g = shared.inner.lock().unwrap();
            let entry = g
                .sessions
                .entry(session.to_string())
                .or_insert_with(SessionStat::new);
            entry.tokens = stats["tokens"].as_u64().unwrap_or(0);
            entry.today = stats["today"].as_u64().unwrap_or(0);
            entry.last_seen = Instant::now();
            g.sessions.retain(|_, s| s.last_seen.elapsed() < STATS_WINDOW);
        }
        shared.poke.notify_one(); // refresh the device counter promptly
        return Ok(()); // stats are fire-and-forget; just close the connection
    }

    // A turn-lifecycle update from the hooks: `UserPromptSubmit` => start,
    // `Stop` => stop. Drives the snapshot `running` count and the
    // `completed` pulse that the firmware turns into BGM / done jingle.
    if let Some(run) = req.get("run") {
        let session = run["session"].as_str().unwrap_or("");
        let state = run["state"].as_str().unwrap_or("");
        if !session.is_empty() {
            let mut g = shared.inner.lock().unwrap();
            let entry = g
                .sessions
                .entry(session.to_string())
                .or_insert_with(SessionStat::new);
            entry.last_seen = Instant::now();
            let transcript = run["transcript"].as_str().unwrap_or("");
            if !transcript.is_empty() && entry.transcript != transcript {
                entry.transcript = transcript.to_string();
                entry.t_offset = 0;
            }
            match state {
                "start" => {
                    entry.running = true;
                    entry.run_started = Instant::now();
                    // Watch only from this turn onward — history contains
                    // old interrupt markers that must not retrigger.
                    entry.t_offset = std::fs::metadata(transcript)
                        .map(|m| m.len())
                        .unwrap_or(0);
                }
                // Tool-call heartbeat: proves the turn is alive. Re-promotes
                // a session that RUN_STALE demoted (long thinking stretch
                // with no tools) — the BGM restarts on that edge.
                "beat" => {
                    if !entry.running {
                        // Re-promotion: skip transcript history accumulated
                        // while we weren't watching, same as "start".
                        entry.t_offset = std::fs::metadata(transcript)
                            .map(|m| m.len())
                            .unwrap_or(0);
                    }
                    entry.running = true;
                    entry.run_started = Instant::now();
                }
                // "stop" = turn finished (Stop hook): done jingle if it was
                // actually running. "end" = session exited//cleared: just
                // stop the music, finishing nothing is nothing to celebrate.
                "stop" | "end" => {
                    let was_running = entry.running;
                    entry.running = false;
                    if state == "stop" && was_running {
                        g.completed_until = Some(Instant::now() + COMPLETED_PULSE);
                    }
                }
                _ => {}
            }
        }
        shared.poke.notify_one();
        return Ok(());
    }

    let tool = req["tool"].as_str().unwrap_or("Tool").to_string();
    let hint = req["hint"].as_str().unwrap_or("").to_string();
    let timeout_ms = req["timeout_ms"].as_u64().unwrap_or(45_000);

    // No Stick? Answer immediately so the hook falls back to the terminal.
    if !shared.connected.load(Ordering::SeqCst) {
        return respond(reader.into_inner(), Decision::Defer).await;
    }

    let id = format!("req_{}", shared.req_seq.fetch_add(1, Ordering::SeqCst));
    let (tx, rx) = oneshot::channel();

    // Register the pending prompt under the lock, then drop the guard *before*
    // any await (the std MutexGuard is not Send and can't cross an await point).
    let busy = {
        let mut g = shared.inner.lock().unwrap();
        if g.pending.is_some() {
            true // device already showing a prompt — don't clobber it
        } else {
            let stamp = chrono::Local::now().format("%H:%M");
            g.entries.push_front(format!("{stamp} {}", short(&hint, 80)));
            while g.entries.len() > 5 {
                g.entries.pop_back();
            }
            g.pending = Some(Pending {
                id: id.clone(),
                tool: tool.clone(),
                hint: short(&hint, 80),
                responder: tx,
            });
            false
        }
    };
    if busy {
        return respond(reader.into_inner(), Decision::Defer).await;
    }
    shared.poke.notify_one(); // push the prompt to the device now

    let decision = match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
        Ok(Ok(d)) => d,
        _ => Decision::Defer, // timed out or device disconnected mid-prompt
    };

    // If we timed out the prompt is still ours — clear it so the device screen
    // returns to idle on the next snapshot.
    {
        let mut g = shared.inner.lock().unwrap();
        if g.pending.as_ref().map(|p| p.id == id).unwrap_or(false) {
            g.pending = None;
        }
    }
    shared.poke.notify_one();

    respond(reader.into_inner(), decision).await
}

async fn respond(mut stream: UnixStream, decision: Decision) -> Result<()> {
    let msg = json!({ "decision": decision.as_str() }).to_string();
    stream.write_all(msg.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// BLE: scan -> connect -> run session, reconnecting forever.
// ---------------------------------------------------------------------------

async fn ble_loop(shared: Arc<Shared>) -> Result<()> {
    let manager = Manager::new().await?;
    let central = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Bluetooth adapter"))?;

    let svc = Uuid::parse_str(NUS_SERVICE)?;
    central
        .start_scan(ScanFilter { services: vec![svc] })
        .await?;
    eprintln!("[ble] scanning for a Claude device…");

    let peripheral = find_device(&central).await?;
    let name = peripheral
        .properties()
        .await?
        .and_then(|p| p.local_name)
        .unwrap_or_else(|| "Claude".into());
    central.stop_scan().await.ok();
    eprintln!("[ble] connecting to {name}…");

    // CoreBluetooth's connect() pends forever if the device vanished between
    // scan and connect (e.g. it rebooted into the bootloader) — without a
    // timeout the whole reconnect loop wedges here and never retries.
    let connect = async {
        peripheral.connect().await?;
        peripheral.discover_services().await?;
        Ok::<(), anyhow::Error>(())
    };
    match tokio::time::timeout(Duration::from_secs(15), connect).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = tokio::time::timeout(Duration::from_secs(3), peripheral.disconnect()).await;
            return Err(e);
        }
        Err(_) => {
            // Cancel the pending CoreBluetooth connect or it lingers.
            let _ = tokio::time::timeout(Duration::from_secs(3), peripheral.disconnect()).await;
            return Err(anyhow!("connect timed out"));
        }
    }
    eprintln!("[ble] connected; pairing may prompt for the passkey on screen");

    run_session(&peripheral, shared).await
}

async fn find_device(central: &btleplug::platform::Adapter) -> Result<Peripheral> {
    for _ in 0..60 {
        for p in central.peripherals().await? {
            if let Some(props) = p.properties().await? {
                if props
                    .local_name
                    .as_deref()
                    .map(|n| n.starts_with("Claude"))
                    .unwrap_or(false)
                {
                    return Ok(p);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(anyhow!("no Claude device found while scanning"))
}

async fn run_session(peripheral: &Peripheral, shared: Arc<Shared>) -> Result<()> {
    let chars = peripheral.characteristics();
    let rx = chars
        .iter()
        .find(|c| c.uuid == Uuid::parse_str(NUS_RX).unwrap())
        .ok_or_else(|| anyhow!("device has no NUS RX characteristic"))?
        .clone();
    let tx = chars
        .iter()
        .find(|c| c.uuid == Uuid::parse_str(NUS_TX).unwrap())
        .ok_or_else(|| anyhow!("device has no NUS TX characteristic"))?
        .clone();

    // Subscribing to the encrypted TX is the first secured GATT access — this
    // is what triggers macOS to pop the passkey pairing dialog on first run.
    // Generous timeout: first-run pairing waits on the human typing a passkey.
    tokio::time::timeout(Duration::from_secs(60), peripheral.subscribe(&tx))
        .await
        .map_err(|_| anyhow!("subscribe timed out"))??;
    let mut notifs = peripheral.notifications().await?;

    // One-shot on connect: time sync + owner name.
    let now = chrono::Local::now();
    let tz = now.offset().local_minus_utc();
    write_line(
        peripheral,
        &rx,
        &json!({ "time": [now.timestamp(), tz] }).to_string(),
    )
    .await?;
    if let Ok(owner) = std::env::var("BUDDY_OWNER") {
        if !owner.is_empty() {
            write_line(
                peripheral,
                &rx,
                &json!({ "cmd": "owner", "name": owner }).to_string(),
            )
            .await?;
        }
    }

    shared.connected.store(true, Ordering::SeqCst);
    shared.session_was_up.store(true, Ordering::SeqCst);
    *shared.last_rx.lock().unwrap() = Instant::now();
    eprintln!("[ble] session up");

    // Reader and sender share the link; whichever ends first ends the session.
    let reader = async {
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        while let Some(n) = notifs.next().await {
            *shared.last_rx.lock().unwrap() = Instant::now();
            buf.extend_from_slice(&n.value);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                handle_device_line(&line[..line.len() - 1], &shared);
            }
        }
        Ok::<(), anyhow::Error>(()) // notification stream ended == disconnected
    };

    let sender = async {
        loop {
            let snapshot = {
                let g = shared.inner.lock().unwrap();
                build_snapshot(&g)
            };
            write_line(peripheral, &rx, &snapshot).await?;
            tokio::select! {
                _ = tokio::time::sleep(KEEPALIVE) => {}
                _ = shared.poke.notified() => {}
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };

    // In-session liveness: the firmware acks every received line, so a live
    // link refreshes last_rx at the keepalive cadence (3s). After a device
    // reboot macOS fakes everything else — is_connected stays true, writes
    // resolve fine, the notification stream stays open. Silence from the
    // device is the only honest signal. The clean path ends the session here;
    // the zombie path (these tasks silently dying) is caught by the
    // process-level backstop in main().
    let watchdog = async {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let quiet = shared.last_rx.lock().unwrap().elapsed();
            if quiet > Duration::from_secs(10) {
                return Err::<(), anyhow::Error>(anyhow!(
                    "no data from device for {}s",
                    quiet.as_secs()
                ));
            }
        }
    };

    let result = tokio::select! {
        r = reader => r,
        r = sender => r,
        r = watchdog => r,
    };
    shared.connected.store(false, Ordering::SeqCst);
    // disconnect() on a dead link can hang forever in the poisoned
    // CoreBluetooth stack — observed in practice. Without the timeout this
    // never returns and the fast restart-on-session-death path never runs.
    let _ = tokio::time::timeout(Duration::from_secs(3), peripheral.disconnect()).await;
    result
}

/// Parse a single newline-delimited JSON line coming from the device. The only
/// thing we act on is a permission decision; acks and anything else are ignored.
fn handle_device_line(line: &[u8], shared: &Arc<Shared>) {
    let v: Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    if v["cmd"] != "permission" {
        return;
    }
    let id = v["id"].as_str().unwrap_or("");
    let decision = match v["decision"].as_str() {
        Some("once") => Decision::Allow,
        Some("deny") => Decision::Deny,
        _ => return,
    };

    let mut g = shared.inner.lock().unwrap();
    if g.pending.as_ref().map(|p| p.id == id).unwrap_or(false) {
        let pending = g.pending.take().unwrap();
        drop(g);
        let _ = pending.responder.send(decision);
        shared.poke.notify_one(); // clear the prompt off the device screen
        eprintln!("[ble] decision {} -> {}", id, decision.as_str());
    }
}

fn build_snapshot(inner: &Inner) -> String {
    let entries: Vec<&String> = inner.entries.iter().collect();

    // Sum tokens over sessions still within the "connected" window. The
    // firmware fields are uint32, so clamp.
    let (mut tokens, mut today) = (0u64, 0u64);
    for s in inner.sessions.values() {
        if s.last_seen.elapsed() < STATS_WINDOW {
            tokens += s.tokens;
            today += s.today;
        }
    }
    let tokens = tokens.min(u32::MAX as u64);
    let today = today.min(u32::MAX as u64);
    // `total` reflects how many live sessions are currently connected.
    let total = inner
        .sessions
        .values()
        .filter(|s| s.last_seen.elapsed() < STATS_WINDOW)
        .count();

    // Sessions mid-turn (UserPromptSubmit seen, Stop not yet). The firmware
    // starts BGM on 0→N and stops it on N→0, so stale "running" sessions
    // (crashed mid-turn, never sent Stop) must drop out eventually.
    let running = inner
        .sessions
        .values()
        .filter(|s| s.running && s.run_started.elapsed() < RUN_STALE)
        .count();
    // While true the firmware plays the done jingle (rising edge) and shows
    // the celebrate face; the first snapshot after the pulse clears it.
    let completed = inner
        .completed_until
        .map(|t| Instant::now() < t)
        .unwrap_or(false);

    let mut obj = match &inner.pending {
        Some(p) => json!({
            "total": total.max(1), "running": running, "waiting": 1,
            "completed": completed,
            "msg": short(&format!("approve: {}", p.tool), 23),
            "entries": entries,
            "tokens": tokens, "tokens_today": today,
            "prompt": { "id": p.id, "tool": p.tool, "hint": p.hint },
        }),
        None => json!({
            "total": total, "running": running, "waiting": 0,
            "completed": completed,
            "msg": if running > 0 { "working" } else { "idle" },
            "entries": entries,
            "tokens": tokens, "tokens_today": today,
        }),
    };
    // Defensive: keep snapshots single-line.
    if let Some(s) = obj.as_object_mut().and_then(|o| o.get_mut("msg")) {
        if let Some(text) = s.as_str() {
            *s = Value::String(text.replace('\n', " "));
        }
    }
    obj.to_string()
}

/// Write a JSON line to the device, newline-terminated, chunked under the MTU.
///
/// WithResponse on purpose: it forces an ATT round-trip per chunk, which is
/// our real liveness check. After the Stick reboots, macOS keeps accepting
/// WithoutResponse writes AND keeps `is_connected` true — the only thing a
/// dead link can't fake is an acked write. An un-acked chunk (5s) errors the
/// sender loop, which tears the session down and re-enters the scan loop.
async fn write_line(peripheral: &Peripheral, ch: &btleplug::api::Characteristic, s: &str) -> Result<()> {
    let mut data = s.replace('\n', " ").into_bytes();
    data.push(b'\n');
    for chunk in data.chunks(WRITE_CHUNK) {
        tokio::time::timeout(
            Duration::from_secs(5),
            peripheral.write(ch, chunk, WriteType::WithResponse),
        )
        .await
        .map_err(|_| anyhow!("write un-acked — link dead"))??;
    }
    Ok(())
}

/// Truncate to `max` chars on a char boundary (the firmware fields are small).
fn short(s: &str, max: usize) -> String {
    let one_line = s.split('\n').next().unwrap_or("").trim();
    if one_line.chars().count() <= max {
        one_line.to_string()
    } else {
        one_line.chars().take(max).collect()
    }
}
