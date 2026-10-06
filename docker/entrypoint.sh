#!/bin/bash
# Desktop image entrypoint: Xvfb with E2B's flags, an optional xfce session and
# tilt-testpattern, then tilt in the foreground.
#
#   SCREEN=1920x1080x24     Xvfb screen (E2B's default is 1024x768x24)
#   X_ABSTRACT_ONLY=0|1     1 adds -nolisten unix, as E2B's SDK does: only @/tmp/.X11-unix/X0
#   DESKTOP=xfce|none
#   TESTPATTERN=0|1|animate
#   TILT_*                  read by tilt itself
#
# Arguments go to tilt. A command instead (e.g. `bash`, `xdotool ...`) runs in tilt's place
# with the desktop up, which is handy for debugging.
set -euo pipefail

SCREEN=${SCREEN:-1920x1080x24}
DESKTOP=${DESKTOP:-xfce}
TESTPATTERN=${TESTPATTERN:-0}
export DISPLAY=:0

die() {
  echo "entrypoint: $*" >&2
  exit 1
}

# Polls `$@` every 0.2 s for up to $timeout seconds.
wait_for() {
  local timeout=$1
  shift
  for _ in $(seq $((timeout * 5))); do
    "$@" && return 0
    sleep 0.2
  done
  return 1
}

listen=(-nolisten tcp)
if [ "${X_ABSTRACT_ONLY:-0}" = 1 ]; then
  listen+=(-nolisten unix)
fi
Xvfb :0 -ac -screen 0 "$SCREEN" -dpi 96 "${listen[@]}" &
xvfb=$!

# xdpyinfo (libxcb) tries the abstract socket first, so this works with -nolisten unix too.
x_up() {
  kill -0 "$xvfb" 2>/dev/null || die "Xvfb exited"
  xdpyinfo >/dev/null 2>&1
}
wait_for 10 x_up || die "Xvfb did not accept connections within 10 s"

case "$DESKTOP" in
  xfce)
    dbus-launch --exit-with-session startxfce4 >/tmp/xfce.log 2>&1 &
    # The same readiness check as E2B's SDK, plus the panel.
    xfce_up() {
      local p
      for p in xfce4-session xfwm4 xfdesktop xfce4-panel; do
        pgrep -x "$p" >/dev/null || return 1
      done
    }
    wait_for 60 xfce_up || die "xfce did not start within 60 s; see /tmp/xfce.log"
    ;;
  none) ;;
  *) die "DESKTOP must be xfce or none, not '$DESKTOP'" ;;
esac

log=/tmp/testpattern.log
case "$TESTPATTERN" in
  0) ;;
  1) tilt-testpattern --log "$log" & ;;
  animate) tilt-testpattern --animate --log "$log" & ;;
  *) die "TESTPATTERN must be 0, 1 or animate, not '$TESTPATTERN'" ;;
esac
if [ "$TESTPATTERN" != 0 ]; then
  # "ready" means the marker is drawn and focused, so input can be injected right away.
  pattern_ready() { grep -qs '"ev":"ready"' "$log"; }
  wait_for 10 pattern_ready || die "tilt-testpattern did not get ready within 10 s"
fi

if [ $# -gt 0 ] && [ "${1#-}" = "$1" ] && [ "$1" != serve ] && command -v "$1" >/dev/null; then
  exec "$@"
fi
exec tilt "$@"
