# buddy-bridge

Push **terminal Claude Code** permission prompts to a Claude desk-pet
(M5StickC Plus **S3**) and approve them from the device's physical buttons.

The Claude desktop app already does this for sessions *it* runs — but a plain
`claude` in your terminal is a separate process the desktop app can't see, so
its prompts never reach the Stick. This bridge fills that gap: it becomes the
BLE central in the desktop app's place and feeds prompts straight from a
`PreToolUse` hook.

The Stick firmware is **not modified** — this speaks the same Hardware Buddy
BLE protocol the desktop app does (see `claude-desktop-buddy-s3/REFERENCE.md`).

## How it works

```
  terminal claude #1 ─┐
  terminal claude #2 ─┤  PreToolUse hook (buddy-gate)
  terminal claude #3 ─┘            │ Unix socket ~/.claude/buddy-bridge.sock
                                   ▼
                        bridged (LaunchAgent, always running)
                         • single BLE central to the Stick
                         • keepalive snapshots every 7s
                         • pushes prompt, awaits A/B button
                                   │ BLE (Nordic UART, encrypted/bonded)
                                   ▼
                              M5StickC Plus S3
```

* **`bridged`** — a background daemon (LaunchAgent, autostarts at login). It
  owns the one BLE connection to the Stick, sends keepalives, and on request
  pushes a permission prompt then waits for the button press.
* **`buddy-gate`** — a tiny `PreToolUse` hook (std-only, fast cold start). On
  every Bash tool call it asks the daemon for a decision over a Unix socket.

**Fail-open:** if the daemon isn't running or the Stick is off/out of range,
the hook prints nothing and exits 0 — Claude Code falls back to its normal
terminal y/n prompt. You're never blocked by missing hardware.

## Install

```bash
BUDDY_OWNER="YourName" ./install.sh
```

This builds the binaries, installs the LaunchAgent, and adds the `PreToolUse`
hook to `~/.claude/settings.json` (a timestamped backup is made first).

Then, once:

1. **Forget the Stick** in the Claude desktop app's Hardware Buddy window —
   only one BLE central can own it at a time.
2. **Wake the Stick.** macOS pops a passkey dialog on first connect; type the
   6-digit code shown on the Stick screen. The bond is remembered after that.
3. **Grant Bluetooth** if macOS asks (System Settings → Privacy & Security →
   Bluetooth).
4. Confirm it's up: `tail -f /tmp/buddy-bridged.log` → look for `[ble] session up`.

Now run a Bash command needing approval in any terminal Claude Code session —
it shows on the Stick. **A = approve, B = deny.**

## Scope / tradeoff

The hook gates **all Bash tool calls** while the Stick is connected — it fires
before Claude Code's permission engine, so it can't tell which calls would
otherwise have been auto-allowed. To narrow or widen, edit the `matcher` in
`~/.claude/settings.json` (`"Bash"` → e.g. `"Bash|Write|Edit"`, or `"*"`).

## Uninstall

```bash
./uninstall.sh
```

Removes the LaunchAgent and the hook (with a backup), then re-enable the Stick
in the desktop app if you want the original behavior back.

## Troubleshooting

* **No `[ble] session up`** — make sure the desktop app has *forgotten* the
  Stick (BLE is single-central), the Stick is awake, and Bluetooth permission
  is granted to `bridged`.
* **Pairing dialog never appears** — the Stick requires LE Secure Connections
  bonding; subscribing to its encrypted characteristic is what triggers the
  macOS passkey prompt. If it's stuck, factory-reset the Stick's bonds
  (device: hold A → settings → reset) and reconnect.
* **Prompts not clearing** — the daemon includes the pending prompt in every
  keepalive and clears it on decision/timeout; check the log for `decision`.
* **Restart the daemon** — `launchctl kickstart -k gui/$(id -u)/com.buddy.bridged`
