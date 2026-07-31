#!/usr/bin/env bash
# Drive MeshApp's UI over SSH and capture just its window.
#
# Two things make this work, and both were learned the hard way:
#
#   * `click at {x,y}` reports the element under the cursor but does NOT
#     reliably change selection in a SwiftUI List. Setting `selected` on
#     the row through the accessibility API does.
#   * The window moves. Any hardcoded screen coordinate goes stale, so the
#     window position is read on every call.
#
# Requires, in System Settings → Privacy & Security, for
# /usr/libexec/sshd-keygen-wrapper:
#   * Screen Recording  (to capture at all)
#   * Accessibility     (to select rows)
#
# Usage:
#   ui-select.sh channel <row> <out.png>   # sidebar (group 1)
#   ui-select.sh thread  <row> <out.png>   # threads column (group 2)
#   ui-select.sh shot    <out.png>         # capture without selecting
set -uo pipefail

APP_LOG=${APP_LOG:-/tmp/app4.log}

window_id() { grep -o "window: [0-9]*" "$APP_LOG" | head -1 | awk '{print $2}'; }

capture() {
    local out=$1 wid
    wid=$(window_id)
    [ -n "$wid" ] || { echo "no window id in $APP_LOG" >&2; return 1; }
    screencapture -l"$wid" -o "$out" && echo "captured $out"
}

select_row() { # select_row <group-index> <row>
    osascript <<APPLESCRIPT >/dev/null 2>&1
tell application "System Events" to tell process "MeshApp"
  set frontmost to true
  delay 0.3
  tell outline 1 of scroll area 1 of group $1 of splitter group 1 of group 1 of window 1
    set selected of row $2 to true
  end tell
end tell
APPLESCRIPT
    sleep 1.2
}

case "${1:-}" in
    channel) select_row 1 "$2"; capture "$3" ;;
    thread)  select_row 2 "$2"; capture "$3" ;;
    shot)    capture "$2" ;;
    *) echo "usage: $0 {channel|thread} <row> <out.png> | $0 shot <out.png>" >&2; exit 2 ;;
esac
