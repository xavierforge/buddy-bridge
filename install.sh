#!/usr/bin/env bash
# Build buddy-bridge, install the daemon as a login LaunchAgent, and register
# the buddy-gate PermissionRequest hook in ~/.claude/settings.json (all tools).
#
# Idempotent: safe to re-run after editing the code. Override the owner name
# shown on the device with:  BUDDY_OWNER="Felix" ./install.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BRIDGED="$ROOT/target/release/bridged"
GATE="$ROOT/target/release/buddy-gate"
LABEL="com.buddy.bridged"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
SETTINGS="$HOME/.claude/settings.json"
OWNER="${BUDDY_OWNER:-$(id -F 2>/dev/null | awk '{print $1}')}"

echo "==> Building release binaries"
( cd "$ROOT" && cargo build --release )

echo "==> Installing LaunchAgent -> $PLIST  (owner: ${OWNER:-<none>})"
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
  <key>StandardOutPath</key><string>/tmp/buddy-bridged.log</string>
  <key>StandardErrorPath</key><string>/tmp/buddy-bridged.log</string>
</dict>
</plist>
PLIST_EOF

echo "==> (Re)loading the LaunchAgent"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl kickstart -k "gui/$(id -u)/$LABEL" 2>/dev/null || true

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

Daemon : $BRIDGED  (LaunchAgent $LABEL, autostarts at login)
Hook   : $GATE  (PermissionRequest / * all tools, timeout 60s)
Logs   : /tmp/buddy-bridged.log
Backup : $SETTINGS.bak.*

Next steps:
  1. In the Claude desktop app, FORGET the Stick in the Hardware Buddy window
     (only one BLE central can own it at a time).
  2. Wake the Stick. On first connect macOS pops a passkey dialog — type the
     6-digit code shown on the Stick screen. The bond is then remembered.
  3. Grant Bluetooth access if macOS asks
     (System Settings > Privacy & Security > Bluetooth).
  4. Watch it come up:   tail -f /tmp/buddy-bridged.log
     You want to see "[ble] session up".
  5. In any terminal Claude Code session, run a Bash command that needs
     approval — the prompt appears on the Stick; press A to approve, B to deny.

If the Stick is off or out of range, Bash approvals fall back to the normal
terminal y/n prompt automatically. Nothing blocks on missing hardware.
DONE
