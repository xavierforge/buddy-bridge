# buddy-bridge

> An unofficial host-side bridge built on top of Anthropic's
> [Hardware Buddy BLE API](https://github.com/anthropics/claude-desktop-buddy),
> extending it from desktop-app sessions to terminal Claude Code.

<img width="500" alt="A vintage black-and-white photo of jazz drummer Buddy Rich playing drums, with an M5StickC Plus S3 (showing the buddy pet face) photoshopped over his head and a Bluetooth icon nearby — a visual pun on buddy-rich/buddy-bridge." src="https://github.com/user-attachments/assets/a75d8b86-f989-4fa2-8579-529b5650397a" />


Push **terminal Claude Code** activity to a Claude desk-pet (M5StickC Plus
**S3**): approve permission prompts from the device's physical buttons, and
let it react to your sessions — background music while a task runs, a jingle
when it finishes.

The Claude desktop app already does this for sessions *it* runs — but a plain
`claude` in your terminal is a separate process the desktop app can't see, so
nothing reaches the Stick. This bridge fills that gap: it becomes the BLE
central in the desktop app's place and feeds it from Claude Code hooks.

https://github.com/user-attachments/assets/cdb2fd58-93c4-4315-945a-ec31459c1302

> 🔊 Sound on! The music carries the demo.


## How it works

```
  terminal claude #1 ─┐
  terminal claude #2 ─┤  hooks (buddy-gate)
  terminal claude #3 ─┘            │ Unix socket ~/.claude/buddy-bridge.sock
                                   ▼
                        bridged (LaunchAgent, always running)
                         • single BLE central to the Stick
                         • keepalive snapshots every 3s
                         • pushes prompt, awaits A/B button
                         • tracks run state → BGM / done jingle
                                   │ BLE (Nordic UART, encrypted/bonded)
                                   ▼
                              M5StickC Plus S3
```

* **`bridged`** — a background daemon (launchd LaunchAgent on macOS, systemd
  `--user` service on Linux; autostarts at login). It
  owns the one BLE connection to the Stick, sends keepalive snapshots, pushes
  permission prompts and waits for the button press, and tracks per-session
  run state so the device knows when a task is running, finished, or was
  interrupted.
* **`buddy-gate`** — a tiny std-only hook client (fast cold start), invoked
  from several hook events and dispatching on `hook_event_name`:

  | Hook | What it sends |
  |------|---------------|
  | `PermissionRequest` (any tool) | asks the daemon for an A/B decision, blocks on it |
  | `UserPromptSubmit` | turn **started** → device starts BGM |
  | `Stop` | turn **finished** → token totals + done jingle |
  | `PostToolUse` | heartbeat → keeps the turn marked alive |
  | `SessionEnd` | REPL exited → stop the music (no jingle) |

**Fail-open:** if the daemon isn't running or the Stick is off/out of range,
the hook prints nothing and exits 0 — Claude Code falls back to its normal
terminal y/n prompt. You're never blocked by missing hardware.

### Interrupts and reboots

* **Esc / Ctrl-C** fires no hook, so a turn can be left looking "running."
  The daemon tails the running session's transcript for the
  `[Request interrupted by user]` marker and stops the music within ~2s; a
  tool-call heartbeat timeout is the backstop.
* **Stick reboots** don't cleanly end the BLE link on macOS — `is_connected`
  stays true, writes still resolve, the notification stream stays open. The
  firmware **acks every line it receives**, so silence is the one liveness
  signal that can't be faked: when acks stop, the daemon exits and the service
  manager (launchd `KeepAlive` / systemd `Restart=always`) brings up a fresh
  process with a clean BLE stack, reconnecting in ~2s. No manual restart needed.
  (The `is_connected`-stays-true quirk is CoreBluetooth-specific; the ack-based
  liveness check works regardless of backend.)

## Hardware

This README assumes you're running the
[claude-desktop-buddy-s3](https://github.com/xavierforge/claude-desktop-buddy-s3)
firmware on an **M5StickC Plus S3** — the unofficial S3 port that adds the
music engine the bridge drives. The bridge itself only speaks the standard
Hardware Buddy BLE protocol (see
[REFERENCE.md](https://github.com/xavierforge/claude-desktop-buddy-s3/blob/main/REFERENCE.md)),
so it also works with:

- The original [anthropics/claude-desktop-buddy](https://github.com/anthropics/claude-desktop-buddy)
  on M5StickC Plus (no BGM/jingle — those need the S3 firmware's music engine)
- Any other firmware that implements the same protocol

The Stick firmware is **not modified** by the bridge — both sides speak the
documented wire protocol.

## Platform support

- **macOS** — verified, primary development platform.
- **Linux** — implemented but not yet hardware-tested. `install.sh` auto-detects
  the OS and installs a systemd `--user` service; the code is portable
  (Unix-socket IPC, and `btleplug` pulls its BlueZ backend on Linux, confirmed
  via the dependency tree). Nothing has run against a real BlueZ stack yet — the
  likely rough edge is first-time pairing (use `bluetoothctl`). Reports welcome.
- **Windows** — not supported. Different BLE stack (WinRT), no Unix domain
  sockets, and a different daemon model — it would be a separate implementation,
  not a port. PRs welcome.

## Install

```bash
BUDDY_OWNER="YourName" ./install.sh
```

This builds the binaries, installs a per-user background service, and registers
all five hooks in `~/.claude/settings.json` (a timestamped backup is made
first). The installer detects your OS — **macOS** (launchd LaunchAgent) or
**Linux** (systemd `--user` service). It is idempotent — safe to re-run after
editing the code; existing buddy-gate entries are de-duped and unrelated hooks
are preserved.

> On Linux you also need BlueZ at runtime and `libdbus-1-dev` + `pkg-config` to
> build (e.g. `apt install bluez libdbus-1-dev pkg-config`).

> New hooks load when a Claude Code session **starts**, so open a fresh
> terminal session for them to take effect.

Then pair the Stick once. Only one BLE central can own it at a time, so first
**forget it** wherever it's currently bonded (e.g. the Claude desktop app's
Hardware Buddy window). Then:

* **macOS** — wake the Stick; macOS pops a passkey dialog on first connect, type
  the 6-digit code shown on the Stick screen. Grant Bluetooth if asked (System
  Settings → Privacy & Security → Bluetooth). The unsigned binary's identity
  changes on rebuild, so macOS re-asks once per rebuild — click Allow.
* **Linux** — pair via `bluetoothctl`, typing the 6-digit code from the Stick:

  ```
  bluetoothctl
    scan on            # wait for "Claude-XXXX", note its MAC
    pair  AA:BB:CC:DD:EE:FF
    trust AA:BB:CC:DD:EE:FF
    quit
  ```

  The service runs only while you're logged in; `sudo loginctl enable-linger
  "$USER"` keeps it alive across logout/reboot.

Confirm it's up: `tail -f /tmp/buddy-bridged.log` → look for `[ble] session up`
(on Linux you can also use `journalctl --user -u buddy-bridged -f`).

Now run a command needing approval in any terminal Claude Code session — it
shows on the Stick. **A = approve, B = deny.**

## Scope / tradeoff

The button gate fires on **any tool call that actually needs approval** — it
runs on `PermissionRequest`, the hook Claude Code raises only when it would
otherwise pop a permission prompt, so anything already allow-listed in your
settings skips the Stick entirely. The default `matcher` is `"*"` (all tools);
the firmware shows whichever tool name arrives (`Bash`, `Edit`, `Write`, …). To
narrow it, edit the `matcher` for `PermissionRequest` in
`~/.claude/settings.json` (e.g. `"Bash"`, or `"Bash|Write|Edit"`).

The other hooks (run state, tokens) are session-wide and not gated.

## Uninstall

```bash
./uninstall.sh
```

Removes the background service (launchd or systemd) and all five hooks (with a
backup), preserving any unrelated hooks that shared an event. Re-pair the Stick
(Claude desktop app on macOS, or `bluetoothctl` on Linux) if you want it back.

## Troubleshooting

* **No `[ble] session up`** — make sure the desktop app has *forgotten* the
  Stick (BLE is single-central), the Stick is awake, and Bluetooth permission
  is granted to `bridged`.
* **Pairing dialog never appears** — the Stick requires LE Secure Connections
  bonding; subscribing to its encrypted characteristic is what triggers the
  macOS passkey prompt. If it's stuck, factory-reset the Stick's bonds
  (device: hold A → settings → reset) and reconnect.
* **Bluetooth prompt after an update** — expected: the unsigned binary's
  identity changes on rebuild, so macOS re-asks for Bluetooth access. Until
  you click Allow, scanning finds nothing. Grant it and it reconnects.
* **Music won't stop / start** — BGM and the done jingle are driven by the
  run-state hooks, which only load in sessions started *after* install. Open a
  new terminal session. Hold **B** on the Stick to skip a jingle or stop BGM.
* **Prompts not clearing** — the daemon includes the pending prompt in every
  keepalive and clears it on decision/timeout; check the log for `decision`.
* **Restart the daemon** — `launchctl kickstart -k gui/$(id -u)/com.buddy.bridged`
  (rarely needed — it self-restarts on a dead link).
