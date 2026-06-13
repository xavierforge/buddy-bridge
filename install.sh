#!/usr/bin/env bash
# Build buddy-bridge, install the daemon as a per-user background service, and
# register the buddy-gate PermissionRequest hook in ~/.claude/settings.json
# (all tools).
#
# Supported: macOS (launchd LaunchAgent) and Linux (systemd --user service).
# Idempotent: safe to re-run after editing the code. Override the owner name
# shown on the device with:  BUDDY_OWNER="Felix" ./install.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BRIDGED="$ROOT/target/release/bridged"
GATE="$ROOT/target/release/buddy-gate"
LABEL="com.buddy.bridged"        # launchd label (macOS)
UNIT="buddy-bridged.service"     # systemd unit name (Linux)
LOG="/tmp/buddy-bridged.log"
SETTINGS="$HOME/.claude/settings.json"
OS="$(uname -s)"

# Owner name shown on the device: explicit override, else the OS's full-name
# field, first word only.
case "$OS" in
  Darwin) OWNER="${BUDDY_OWNER:-$(id -F 2>/dev/null | awk '{print $1}')}" ;;
  Linux)  OWNER="${BUDDY_OWNER:-$(getent passwd "$(id -un)" 2>/dev/null \
            | cut -d: -f5 | cut -d, -f1 | awk '{print $1}')}" ;;
  *)      OWNER="${BUDDY_OWNER:-}" ;;
esac

echo "==> Building release binaries"
( cd "$ROOT" && cargo build --release )

# ---------------------------------------------------------------------------
# Background service: launchd on macOS, systemd --user on Linux. Both run the
# daemon in the user's session, restart it on crash, and start it at login.
# ---------------------------------------------------------------------------
echo "==> Installing background service for $OS  (owner: ${OWNER:-<none>})"
case "$OS" in
  Darwin)
    PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
    mkdir -p "$HOME/Library/LaunchAgents"
    cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array><string>$BRIDGED</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>BUDDY_OWNER</key><string>$OWNER</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
PLIST_EOF
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$PLIST"
    launchctl kickstart -k "gui/$(id -u)/$LABEL" 2>/dev/null || true
    SERVICE_DESC="LaunchAgent $LABEL (autostarts at login)"
    ;;
  Linux)
    UNIT_DIR="$HOME/.config/systemd/user"
    UNIT_FILE="$UNIT_DIR/$UNIT"
    mkdir -p "$UNIT_DIR"
    # RunAtLoad+KeepAlive ≈ enable (start at login) + Restart=always. Logs go
    # to the same file macOS uses so `tail -f $LOG` works on both.
    cat > "$UNIT_FILE" <<UNIT_EOF
[Unit]
Description=buddy-bridge BLE daemon for Claude Code
After=bluetooth.target

[Service]
ExecStart=$BRIDGED
Environment=BUDDY_OWNER=$OWNER
Restart=always
RestartSec=2
StandardOutput=append:$LOG
StandardError=append:$LOG

[Install]
WantedBy=default.target
UNIT_EOF
    systemctl --user daemon-reload
    systemctl --user enable "$UNIT" 2>/dev/null || true
    systemctl --user restart "$UNIT"
    SERVICE_DESC="systemd --user $UNIT (autostarts at login)"
    ;;
  *)
    echo "!! Unsupported OS '$OS' — only macOS and Linux ship a service installer." >&2
    echo "   The binaries built fine; start the daemon yourself: $BRIDGED" >&2
    SERVICE_DESC="(no service installed — run $BRIDGED manually)"
    ;;
esac

# ---------------------------------------------------------------------------
# Hook registration in ~/.claude/settings.json — identical on every platform.
# ---------------------------------------------------------------------------
echo "==> Registering hooks in $SETTINGS"
echo "    PermissionRequest:* (button gate, all tools), Stop (stats + run stop),"
echo "    UserPromptSubmit (run start), PostToolUse (run heartbeat),"
echo "    SessionEnd (run stop on exit)"
mkdir -p "$HOME/.claude"
[ -f "$SETTINGS" ] || echo '{}' > "$SETTINGS"
cp "$SETTINGS" "$SETTINGS.bak.$(date +%s)"
tmp="$(mktemp)"
jq --arg cmd "$GATE" '
  # Drop any existing entries pointing at our binary, then append the
  # current set — idempotent and preserves other hooks (e.g. notifiers).
  def dedupe(ev): [ (ev // [])[]
    | select(((.hooks // []) | map(.command) | any(. == $cmd)) | not) ];
  def entry(t): { hooks: [ { type: "command", command: $cmd, timeout: t } ] };

  .hooks = (.hooks // {})
  # PermissionRequest (all tools) — gate the call on the device button. This
  # fires only when the call actually needs approval (not already allow-listed),
  # so the matcher can safely be "*": auto-approved calls never reach here, and
  # the firmware renders whatever tool name we send (Bash, Edit, Write, …).
  | .hooks.PermissionRequest = (dedupe(.hooks.PermissionRequest)
      + [ entry(60) + { matcher: "*" } ])
  # Drop any stale PreToolUse gate from older installs (we moved to
  # PermissionRequest); leave other PreToolUse hooks untouched.
  | .hooks.PreToolUse = dedupe(.hooks.PreToolUse)
  | (if (.hooks.PreToolUse | length) == 0 then del(.hooks.PreToolUse) else . end)
  # Stop — token totals + run-state stop (plays the done jingle).
  | .hooks.Stop = (dedupe(.hooks.Stop) + [ entry(10) ])
  # UserPromptSubmit — run-state start (starts the BGM).
  | .hooks.UserPromptSubmit = (dedupe(.hooks.UserPromptSubmit) + [ entry(10) ])
  # PostToolUse — per-tool-call heartbeat; lets the daemon expire turns
  # that were interrupted with Esc/Ctrl-C (no hook fires for those).
  | .hooks.PostToolUse = (dedupe(.hooks.PostToolUse) + [ entry(10) ])
  # SessionEnd — exiting the REPL stops the music without the done jingle.
  | .hooks.SessionEnd = (dedupe(.hooks.SessionEnd) + [ entry(10) ])
' "$SETTINGS" > "$tmp" && mv "$tmp" "$SETTINGS"

cat <<DONE

==> Installed.

Daemon : $BRIDGED  ($SERVICE_DESC)
Hook   : $GATE  (PermissionRequest / * all tools, timeout 60s)
Logs   : $LOG
Backup : $SETTINGS.bak.*
DONE

# OS-specific pairing / next steps.
case "$OS" in
  Darwin)
    cat <<'DONE'

Next steps (macOS):
  1. In the Claude desktop app, FORGET the Stick in the Hardware Buddy window
     (only one BLE central can own it at a time).
  2. Wake the Stick. On first connect macOS pops a passkey dialog — type the
     6-digit code shown on the Stick screen. The bond is then remembered.
  3. Grant Bluetooth access if macOS asks
     (System Settings > Privacy & Security > Bluetooth).
DONE
    ;;
  Linux)
    cat <<'DONE'

Next steps (Linux):
  1. Make sure nothing else owns the Stick over BLE (only one central at a time).
  2. Build/runtime deps: BlueZ at runtime, and libdbus-1-dev + pkg-config to
     build (e.g. apt install bluez libdbus-1-dev pkg-config).
  3. Pair once with bluetoothctl, typing the 6-digit code shown on the Stick:
       bluetoothctl
         scan on            # wait for "Claude-XXXX", note its MAC
         pair  AA:BB:CC:DD:EE:FF
         trust AA:BB:CC:DD:EE:FF
         quit
  4. The service runs only while you are logged in. To keep it alive across
     logouts/reboots without a session:  sudo loginctl enable-linger "$USER"
DONE
    ;;
esac

cat <<DONE

  Watch it come up:   tail -f $LOG
  You want to see "[ble] session up".

  Then in any terminal Claude Code session, run a command that needs approval —
  the prompt appears on the Stick; press A to approve, B to deny.

If the Stick is off or out of range, approvals fall back to the normal terminal
y/n prompt automatically. Nothing blocks on missing hardware.
DONE
