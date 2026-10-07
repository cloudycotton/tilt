#!/usr/bin/env bash
# Records docs/demo.gif, the README's demo: a 1280x720 Xvfb desktop (xfwm4, an xterm and a Chrome
# window) streamed by tilt to Chrome under Playwright, which types, drags and scrolls.
# Needs Xvfb, xfwm4, dbus-run-session, xterm, xsetroot, google-chrome, ffmpeg, and `npm ci`
# in e2e/.
# Usage: scripts/demo.sh [tilt binary]      (default: target/release/tilt)
set -euo pipefail
cd "$(dirname "$0")/.."
tilt=${1:-target/release/tilt}
display=:97
port=6197
work=$(mktemp -d)
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; sleep 0.5; rm -rf "$work" 2>/dev/null || true; }
trap cleanup EXIT

Xvfb $display -screen 0 1280x720x24 -nolisten tcp >/dev/null 2>&1 & pids+=($!)
sleep 1
export DISPLAY=$display
# xfwm4 reads its settings over D-Bus; the session and its apps form one process group.
setsid dbus-run-session -- sh -c "
  xfwm4 & sleep 1
  xsetroot -solid '#0f172a'
  xterm -geometry 74x22+40+40 -fa 'DejaVu Sans Mono' -fs 11 -bg '#0b1020' -fg '#e2e8f0' &
  google-chrome --no-sandbox --test-type --user-data-dir='$work/chrome' --no-first-run \\
    --no-default-browser-check --disable-gpu --password-store=basic \\
    --window-position=660,110 --window-size=580,560 --app='file://$PWD/e2e/demo/page.html' &
  wait" >/dev/null 2>&1 &
session=$!
trap 'kill -- -$session 2>/dev/null; cleanup' EXIT
sleep 5

"$tilt" --display $display --token demo --bind 127.0.0.1:$port 2>"$work/tilt.log" & pids+=($!)
sleep 1
(cd e2e && node demo/record.mjs "http://127.0.0.1:$port/#token=demo&control=1&stats=1" "$work/demo.webm")

# A 960-wide, 20 fps GIF with one palette for the whole clip.
ffmpeg -loglevel error -y -i "$work/demo.webm" \
  -vf "fps=20,scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=160:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle" \
  docs/demo.gif
echo "docs/demo.gif: $(du -h docs/demo.gif | cut -f1)"
