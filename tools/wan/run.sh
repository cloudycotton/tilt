#!/usr/bin/env bash
# Measures motion smoothness through tilt over an imitated WAN link (docs/performance.md,
# "Over a lossy link"). Starts Xvfb (:99, 1280x720) if needed, the relay, and tilt from
# target/release, then runs measure.mjs.
#
#   tools/wan/run.sh "--delay 25 --jitter 4 --mbps 30 --loss 0.01" [SECONDS] [STEP_PX]
#
# Needs Xvfb, python3, curl, node, and Playwright with Chromium (cd e2e && npm ci).
# TILT=path picks another binary.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tilt=${TILT:-$here/../../target/release/tilt}
export DISPLAY=${DISPLAY_WAN:-:99}
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; }
trap cleanup EXIT

if ! xdpyinfo >/dev/null 2>&1 && ! [ -e "/tmp/.X11-unix/X${DISPLAY#:}" ]; then
  Xvfb "$DISPLAY" -screen 0 1280x720x24 -nolisten tcp >/dev/null 2>&1 &
  pids+=($!)
  sleep 1
fi
# shellcheck disable=SC2086
python3 "$here/relay.py" 7091 6091 ${1:-} &
pids+=($!)
"$tilt" --display "$DISPLAY" --no-auth --bind 127.0.0.1:6090 --udp-port 6091 \
  --udp-public 127.0.0.1:7091 >/dev/null 2>&1 &
pids+=($!)
for _ in $(seq 50); do
  curl -sf http://127.0.0.1:6090/healthz >/dev/null && break
  sleep 0.2
done
node "$here/measure.mjs" http://127.0.0.1:6090/ "${2:-10}" "${3:-12}" 2>/dev/null | grep '^{'
