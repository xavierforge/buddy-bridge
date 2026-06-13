# buddy-bridge
<img width="500" alt="buddy-bridge" src="https://github.com/user-attachments/assets/a75d8b86-f989-4fa2-8579-529b5650397a" />


Push **terminal Claude Code** activity to a Claude desk-pet (M5StickC Plus
**S3**): approve permission prompts from the device's physical buttons, and
let it react to your sessions — background music while a task runs, a jingle
when it finishes.

The Claude desktop app already does this for sessions *it* runs — but a plain
`claude` in your terminal is a separate process the desktop app can't see, so
nothing reaches the Stick. This bridge fills that gap: it becomes the BLE
central in the desktop app's place and feeds it from Claude Code hooks.

The Stick firmware is **not modified** — this speaks the same Hardware Buddy
BLE protocol the desktop app does (see the firmware repo's
[REFERENCE.md](https://github.com/xavierforge/claude-desktop-buddy-s3/blob/main/REFERENCE.md)).

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

* **`bridged`** — a background daemon (LaunchAgent, autostarts at login). It
  owns the one BLE connection to the Stick, sends keepalive snapshots, pushes
  permission prompts and waits for the button press, and tracks per-session
  run state so the device knows when a task is running, finished, or was
  interrupted.
* **`buddy-gate`** — a tiny std-only hook client (fast cold start), invoked
  from several hook events and dispatching on `hook_event_name`:

  | Hook | What it sends |
  |------|---------------|
  | `PreToolUse` (Bash) | asks the daemon for an A/B decision, blocks on it |
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
  signal that can't be faked: when acks stop, the daemon exits and launchd's
  `KeepAlive` brings up a fresh process with a clean CoreBluetooth stack,
  reconnecting in ~2s. No manual restart needed.

## Install

```bash
BUDDY_OWNER="YourName" ./install.sh
```

This builds the binaries, installs the LaunchAgent, and registers all five
hooks in `~/.claude/settings.json` (a timestamped backup is made first). It is
idempotent — safe to re-run after editing the code; existing buddy-gate
entries are de-duped and unrelated hooks are preserved.

> New hooks load when a Claude Code session **starts**, so open a fresh
> terminal session for them to take effect.

Then, once:

1. **Forget the Stick** in the Claude desktop app's Hardware Buddy window —
   only one BLE central can own it at a time.
2. **Wake the Stick.** macOS pops a passkey dialog on first connect; type the
   6-digit code shown on the Stick screen. The bond is remembered after that.
3. **Grant Bluetooth** if macOS asks (System Settings → Privacy & Security →
   Bluetooth). The binary is unsigned, so macOS re-asks after each rebuild —
   click Allow once per rebuild.
4. Confirm it's up: `tail -f /tmp/buddy-bridged.log` → look for `[ble] session up`.

Now run a Bash command needing approval in any terminal Claude Code session —
it shows on the Stick. **A = approve, B = deny.**

## Scope / tradeoff

The button gate fires on **all Bash tool calls** while the Stick is connected —
it runs before Claude Code's permission engine, so it can't tell which calls
would otherwise have been auto-allowed. To narrow or widen, edit the `matcher`
for `PreToolUse` in `~/.claude/settings.json` (`"Bash"` → e.g.
`"Bash|Write|Edit"`, or `"*"`).

The other hooks (run state, tokens) are session-wide and not gated.

## Uninstall

```bash
./uninstall.sh
```

Removes the LaunchAgent and all five hooks (with a backup), preserving any
unrelated hooks that shared an event. Re-enable the Stick in the desktop app
if you want the original behavior back.

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
