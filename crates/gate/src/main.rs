//! buddy-gate — the Claude Code hook client for the desk-pet bridge. One
//! binary, three jobs, dispatched on `hook_event_name`:
//!
//!   * PermissionRequest (any tool) — fired only when the call actually needs
//!     approval (not already allowed); forward it to the local `bridged`
//!     daemon, block until the Stick's A/B button comes back, emit the
//!     decision.
//!   * Stop / SubagentStop — read the session transcript, total this session's
//!     output tokens (cumulative + today), and report them to the daemon so
//!     the device's token counters reflect the currently-connected sessions.
//!   * UserPromptSubmit / PostToolUse / Stop / SessionEnd — report turn
//!     lifecycle (start / heartbeat / stop / exit) so the device knows when a
//!     task is running (BGM) and when it just finished (done jingle).
//!
//! Fail-open everywhere: any error => print nothing, exit 0. For
//! PermissionRequest that means Claude Code's normal terminal y/n prompt takes
//! over; for the rest it just means the device misses one update this turn.
//! You can always work without the device on hand.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone};

/// Slightly under the hook `timeout` in settings.json so the daemon resolves
/// to a decision (or our own read times out) before Claude Code kills us.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(50);

fn main() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let payload: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return,
    };

    match payload["hook_event_name"].as_str().unwrap_or("") {
        "UserPromptSubmit" => report_run(&payload, "start"),
        "Stop" => {
            report_stats(&payload);
            report_run(&payload, "stop");
        }
        "SubagentStop" => report_stats(&payload),
        // No hook fires on user interrupt (Esc/Ctrl+C), so a "start" can be
        // orphaned. Two mitigations: SessionEnd (exit//clear) sends an
        // explicit stop, and every tool call heartbeats so the daemon can
        // expire turns that stopped beating (interrupted mid-run).
        "SessionEnd" => report_run(&payload, "end"), // stop WITHOUT the done jingle
        "PostToolUse" => report_run(&payload, "beat"),
        // PermissionRequest fires only when a permission dialog would appear —
        // i.e. the command isn't already allow-listed — so the Stick only
        // lights up for calls that genuinely need a decision.
        "PermissionRequest" => gate_tool(&payload),
        _ => {}
    }
}

fn socket() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(Path::new(&home).join(".claude").join("buddy-bridge.sock"))
}

// ---------------------------------------------------------------------------
// PermissionRequest: gate a tool call on the physical button. Tool-agnostic —
// the firmware just renders whatever `tool`/`hint` we send, so the matcher in
// settings.json decides which tools reach here.
// ---------------------------------------------------------------------------

fn gate_tool(payload: &serde_json::Value) {
    let tool = payload["tool_name"].as_str().unwrap_or("");
    if tool.is_empty() {
        return; // defer
    }
    // One-line hint, picked from whichever input field best identifies the
    // call: command (Bash), file_path (Edit/Write/Read), url (WebFetch)…
    let input = &payload["tool_input"];
    let raw = input["command"]
        .as_str()
        .or_else(|| input["file_path"].as_str())
        .or_else(|| input["path"].as_str())
        .or_else(|| input["url"].as_str())
        .unwrap_or("");
    let hint = raw.lines().next().unwrap_or("").trim().to_string();

    match ask_device(tool, &hint) {
        Some("allow") => emit("allow"),
        Some("deny") => emit("deny"),
        _ => {} // defer: normal permission flow
    }
}

/// Round-trip one approval request to the daemon. None on any failure (defer).
fn ask_device(tool: &str, hint: &str) -> Option<&'static str> {
    let mut stream = UnixStream::connect(socket()?).ok()?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;

    let req = serde_json::json!({
        "tool": tool,
        "hint": hint,
        "timeout_ms": 45_000u64,
    })
    .to_string();
    stream.write_all(req.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let resp: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match resp["decision"].as_str()? {
        "allow" => Some("allow"),
        "deny" => Some("deny"),
        _ => None,
    }
}

fn emit(decision: &str) {
    // PermissionRequest decisions use `decision.behavior` ("allow"/"deny"),
    // unlike PreToolUse's `permissionDecision`.
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": { "behavior": decision },
        }
    });
    println!("{out}");
}

// ---------------------------------------------------------------------------
// UserPromptSubmit / Stop: report turn lifecycle so the device knows when a
// task is running (BGM) and when it just finished (done jingle).
// ---------------------------------------------------------------------------

fn report_run(payload: &serde_json::Value, state: &str) {
    let session = payload["session_id"].as_str().unwrap_or("");
    if session.is_empty() {
        return;
    }
    // Best-effort fire-and-forget, same contract as report_stats.
    let _ = (|| -> Option<()> {
        let mut stream = UnixStream::connect(socket()?).ok()?;
        stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
        // transcript path lets the daemon watch for the
        // "[Request interrupted by user]" marker — the only prompt signal
        // that an Esc/Ctrl-C interrupt leaves behind (no hook fires).
        let msg = serde_json::json!({
            "run": {
                "session": session,
                "state": state,
                "transcript": payload["transcript_path"].as_str().unwrap_or(""),
            }
        })
        .to_string();
        stream.write_all(msg.as_bytes()).ok()?;
        stream.write_all(b"\n").ok()?;
        stream.flush().ok()?;
        Some(())
    })();
}

// ---------------------------------------------------------------------------
// Stop: total this session's output tokens and report to the daemon.
// ---------------------------------------------------------------------------

fn report_stats(payload: &serde_json::Value) {
    let session = payload["session_id"].as_str().unwrap_or("");
    let transcript = payload["transcript_path"].as_str().unwrap_or("");
    if session.is_empty() || transcript.is_empty() {
        return;
    }
    let (tokens, today) = match sum_output_tokens(transcript) {
        Some(t) => t,
        None => return,
    };

    // Best-effort fire-and-forget; the daemon may be down (fine, just skip).
    let _ = (|| -> Option<()> {
        let mut stream = UnixStream::connect(socket()?).ok()?;
        stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
        let msg = serde_json::json!({
            "stats": { "session": session, "tokens": tokens, "today": today }
        })
        .to_string();
        stream.write_all(msg.as_bytes()).ok()?;
        stream.write_all(b"\n").ok()?;
        stream.flush().ok()?;
        Some(())
    })();
}

/// Sum `message.usage.output_tokens` across all assistant turns in a transcript
/// JSONL file. Returns (cumulative, since-local-midnight).
fn sum_output_tokens(path: &str) -> Option<(u64, u64)> {
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let midnight = Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|naive| Local.from_local_datetime(&naive).single())?;

    let mut total: u64 = 0;
    let mut today: u64 = 0;
    for line in reader.lines().map_while(Result::ok) {
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let out = v["message"]["usage"]["output_tokens"].as_u64();
        let out = match out {
            Some(n) => n,
            None => continue,
        };
        total += out;
        if let Some(ts) = v["timestamp"].as_str() {
            if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
                if dt.with_timezone(&Local) >= midnight {
                    today += out;
                }
            }
        }
    }
    Some((total, today))
}
