#!/usr/bin/env bash
# Records the README demo (docs/demo.gif and docs/demo.mp4): a 1280x720 Xvfb desktop (xfwm4 with
# the Arc-Dark theme, a gradient wallpaper, an xterm and a Chrome window) streamed by tilt to the
# tilt client, which Chrome shows framed (e2e/demo/showcase.html) on a second Xvfb display that
# ffmpeg films at 60 fps. Playwright types, drags and scrolls.
# Needs Xvfb, xfwm4, xfconf, arc-theme, hsetroot, dbus-run-session, xterm, Google Chrome (or
# CHROME=<a Chrome for Testing binary>), ffmpeg, and `npm ci` in e2e/.
# Usage: scripts/demo.sh [tilt binary]      (default: target/release/tilt)
set -euo pipefail
cd "$(dirname "$0")/.."
tilt=${1:-target/release/tilt}
chrome=${CHROME:-google-chrome}
display=:97   # the remote desktop
stage=:98     # where the viewer is filmed
port=6197
work=$(mktemp -d)
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; sleep 0.5; rm -rf "$work" 2>/dev/null || true; }
trap cleanup EXIT

Xvfb $display -screen 0 1280x720x24 -nolisten tcp >/dev/null 2>&1 & pids+=($!)
Xvfb $stage -screen 0 1280x800x24 -nolisten tcp >/dev/null 2>&1 & pids+=($!)
sleep 1
# xfwm4 reads its settings over D-Bus; the session and its apps form one process group.
DISPLAY=$display setsid dbus-run-session -- sh -c "
  xfconf-query -c xfwm4 -p /general/theme -n -t string -s Arc-Dark
  xfconf-query -c xfwm4 -p /general/title_font -n -t string -s 'Inter SemiBold 10'
  xfconf-query -c xfwm4 -p /general/button_layout -n -t string -s 'CMH|'
  xfconf-query -c xfwm4 -p /general/unredirect_overlays -n -t bool -s false
  xfwm4 & sleep 1
  hsetroot -cover '$PWD/e2e/demo/wallpaper.jpg'
  xterm -geometry 62x20+48+44 -T Terminal -fa 'DejaVu Sans Mono' -fs 11 -bg '#17142b' -fg '#ecebf5' \\
    -xrm 'XTerm*internalBorder: 14' -xrm 'XTerm*cursorColor: #ff8a4c' -xrm 'XTerm*scrollBar: false' \\
    -e bash --rcfile '$PWD/e2e/demo/bashrc' -i &
  '$chrome' --no-sandbox --test-type --user-data-dir='$work/chrome' --no-first-run \\
    --no-default-browser-check --disable-gpu --password-store=basic \\
    --window-position=690,74 --window-size=548,580 --app='file://$PWD/e2e/demo/page.html' &
  wait" >/dev/null 2>&1 &
session=$!
trap 'kill -- -$session 2>/dev/null; cleanup' EXIT
sleep 5

"$tilt" --display $display --token demo --bind 127.0.0.1:$port 2>"$work/tilt.log" & pids+=($!)
sleep 1
(cd e2e && DISPLAY=$stage CHROME=${CHROME:-} node demo/record.mjs "http://127.0.0.1:$port/#token=demo&control=1" "$work/demo.mkv")

# docs/demo.mp4: the full 60 fps. docs/demo.gif: what the README shows, 30 fps and 1000 wide,
# with one palette for the whole clip.
ffmpeg -loglevel error -y -i "$work/demo.mkv" -c:v libx264 -preset slow -crf 22 -pix_fmt yuv420p \
  -movflags +faststart docs/demo.mp4
ffmpeg -loglevel error -y -i "$work/demo.mkv" \
  -vf "fps=30,scale=1000:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=192:stats_mode=diff[p];[b][p]paletteuse=dither=sierra2_4a:diff_mode=rectangle" \
  docs/demo.gif
echo "docs/demo.mp4: $(du -h docs/demo.mp4 | cut -f1)  docs/demo.gif: $(du -h docs/demo.gif | cut -f1)"
