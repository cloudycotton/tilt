#!/usr/bin/env bash
# Records the README demo, docs/demo.webp (and docs/demo.mp4): a 1280x800 Linux desktop (xfwm4,
# a terminal and Chrome on a sunset wallpaper) streamed by tilt into Chrome, where xdotool types,
# drags and scrolls while ffmpeg films the viewer's screen at 60 fps.
# Needs Xvfb, xfwm4, xfconf, dbus-run-session, xterm, xdotool, google-chrome, ffmpeg (with
# libx264 and libwebp), curl, node, and python3 with Pillow and numpy.
# Usage: scripts/demo.sh [tilt binary]      (default: target/release/tilt)
set -euo pipefail
cd "$(dirname "$0")/.."
tilt=${1:-target/release/tilt}
remote=:97   # the Linux desktop tilt streams
viewer=:98   # the screen of the person watching it
port=6197
size=1280x800
stage=1520x1000
work=$(mktemp -d)
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; sleep 0.5; rm -rf "$work" 2>/dev/null || true; }
trap cleanup EXIT

python3 e2e/demo/assets.py "$work/look" "${size%x*}" "${size#*x}"

# The remote desktop.
Xvfb $remote -screen 0 ${size}x24 -nolisten tcp >/dev/null 2>&1 & pids+=($!)
sleep 1
# xfwm4 reads its settings over D-Bus; the session and its apps form one process group.
DISPLAY=$remote XCURSOR_THEME=Adwaita XCURSOR_SIZE=24 setsid dbus-run-session -- sh -c "
  python3 e2e/demo/setroot.py '$work/look/wallpaper.png'
  xfconf-query -c xfwm4 -p /general/theme -n -t string -s tilt
  xfconf-query -c xfwm4 -p /general/button_layout -n -t string -s '|HMC'
  xfconf-query -c xfwm4 -p /general/title_font -n -t string -s 'Inter Semi-Bold 10'
  xfconf-query -c xfwm4 -p /general/use_compositing -n -t bool -s true
  xfconf-query -c xfwm4 -p /general/unredirect_overlays -n -t bool -s false
  XDG_DATA_HOME='$work/look' xfwm4 & sleep 1
  xsetroot -cursor_name left_ptr
  export LANG=C.UTF-8
  xterm -geometry 66x20+56+64 -fa 'DejaVu Sans Mono' -fs 11 -b 18 -bg '#18181b' -fg '#e4e4e7' \\
    -cr '#e4e4e7' -T Terminal -e bash --rcfile e2e/demo/bashrc &
  google-chrome --no-sandbox --test-type --user-data-dir='$work/chrome-remote' --no-first-run \\
    --no-default-browser-check --disable-gpu --password-store=basic \\
    --window-position=700,92 --window-size=540,640 'file://$PWD/e2e/demo/page.html' &
  wait" >/dev/null 2>&1 &
session=$!
trap 'kill -- -$session 2>/dev/null; cleanup' EXIT
sleep 5

"$tilt" --display $remote --token demo --bind 127.0.0.1:$port 2>"$work/tilt.log" & pids+=($!)
sleep 1

# The viewer: Chrome in kiosk mode showing the tilt client framed by e2e/demo/stage.html.
Xvfb $viewer -screen 0 ${stage}x24 -nolisten tcp >/dev/null 2>&1 & pids+=($!)
sleep 1
url=$(node -e "console.log(encodeURIComponent(process.argv[1]))" "http://127.0.0.1:$port/#token=demo&control=1")
DISPLAY=$viewer google-chrome --no-sandbox --test-type --user-data-dir="$work/chrome-viewer" \
  --no-first-run --no-default-browser-check --password-store=basic --kiosk --hide-scrollbars \
  --window-position=0,0 --window-size=${stage/x/,} "file://$PWD/e2e/demo/stage.html#$url" \
  >/dev/null 2>&1 & pids+=($!)
for _ in $(seq 60); do
  curl -fsS -H 'Authorization: Bearer demo' "http://127.0.0.1:$port/api/status" 2>/dev/null \
    | grep -q '"controller":true' && break
  sleep 0.5
done
sleep 2

# Film the viewer's screen while the script plays. The iframe's top-left is the stage's padding
# (stage.html: 44px top, 14px bezel), centred.
left=$(( (${stage%x*} - ${size%x*} - 28) / 2 + 14 )) top=$(( 44 + 14 ))
ffmpeg -loglevel error -y -f x11grab -framerate 60 -video_size $stage -draw_mouse 1 -i $viewer \
  -c:v libx264 -preset ultrafast -qp 0 "$work/raw.mkv" </dev/null & rec=$!
sleep 0.5
DISPLAY=$viewer node e2e/demo/record.mjs $left $top
kill -INT $rec; wait $rec || true

# docs/demo.mp4 at full size and 60 fps; docs/demo.webp, the README's, smaller at 30 fps.
# Both drop the empty strip under the screen's chin.
crop="crop=iw:ih-30:0:0"
ffmpeg -loglevel error -y -i "$work/raw.mkv" -vf "$crop" -c:v libx264 -preset slow -crf 18 \
  -pix_fmt yuv420p -movflags +faststart docs/demo.mp4
ffmpeg -loglevel error -y -i "$work/raw.mkv" -vf "$crop,fps=30,scale=1140:-1:flags=lanczos" \
  -c:v libwebp_anim -lossless 0 -quality 85 -compression_level 6 -loop 0 docs/demo.webp
echo "docs/demo.mp4: $(du -h docs/demo.mp4 | cut -f1), docs/demo.webp: $(du -h docs/demo.webp | cut -f1)"
