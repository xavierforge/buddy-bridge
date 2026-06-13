#!/usr/bin/env bash
# Remove the background service (launchd on macOS, systemd --user on Linux) and
# all buddy-gate hooks. Leaves the built binaries and your settings backups in
# place.
set -euo pipefail

LABEL="com.buddy.bridged"
UNIT="buddy-bridged.service"
SETTINGS="$HOME/.claude/settings.json"
OS="$(uname -s)"

echo "==> Removing background service ($OS)"
case "$OS" in
  Darwin)
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
    ;;
  Linux)
    systemctl --user disable --now "$UNIT" 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/$UNIT"
    systemctl --user daemon-reload 2>/dev/null || true
    ;;
  *)
    echo "   Unknown OS '$OS' — stop the daemon manually if it is running." >&2
    ;;
esac

if [ -f "$SETTINGS" ]; then
  echo "==> Removing buddy-gate hooks from $SETTINGS"
  echo "    (PermissionRequest, Stop, UserPromptSubmit, PostToolUse, SessionEnd)"
  cp "$SETTINGS" "$SETTINGS.bak.$(date +%s)"
  tmp="$(mktemp)"
  jq '
    def strip(k):
      if (.hooks[k]) then
        .hooks[k] = [ .hooks[k][]
          | select(((.hooks // []) | map(.command)
                    | any(test("buddy-gate"))) | not) ]
        | (if (.hooks[k] | length) == 0 then del(.hooks[k]) else . end)
      else . end;
    strip("PermissionRequest") | strip("PreToolUse") | strip("Stop")
    | strip("UserPromptSubmit") | strip("PostToolUse") | strip("SessionEnd")
  ' "$SETTINGS" > "$tmp" && mv "$tmp" "$SETTINGS"
fi

rm -f "$HOME/.claude/buddy-bridge.sock"
echo "==> Done. Re-pair the Stick (Claude desktop app on macOS, or bluetoothctl on Linux) if you want it back."
