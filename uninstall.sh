#!/usr/bin/env bash
# Remove the LaunchAgent and all buddy-gate hooks. Leaves the built binaries
# and your settings backups in place.
set -euo pipefail

LABEL="com.buddy.bridged"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
SETTINGS="$HOME/.claude/settings.json"
GATE_BASENAME="buddy-gate"

echo "==> Unloading LaunchAgent"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
rm -f "$PLIST"

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
echo "==> Done. Re-enable the Stick in the Claude desktop app's Hardware Buddy window if you want it back."
