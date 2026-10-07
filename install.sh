#!/bin/sh
# Installs tilt-live, which runs the latest tilt release and updates it whenever it starts:
#
#   curl -fsSL https://raw.githubusercontent.com/cloudycotton/tilt/main/install.sh | sh
#
# Then `tilt-live` streams this machine's X display (flags pass through to tilt). It goes to
# TILT_INSTALL_DIR, by default ~/.local/bin (/usr/local/bin as root). Needs curl and sha256sum.
set -eu
releases=${TILT_RELEASES:-https://github.com/cloudycotton/tilt/releases}
dir=${TILT_INSTALL_DIR:-}
if [ -z "$dir" ]; then
  if [ "$(id -u)" = 0 ]; then dir=/usr/local/bin; else dir=$HOME/.local/bin; fi
fi
for tool in curl sha256sum; do
  command -v "$tool" >/dev/null || { echo "install.sh: needs $tool" >&2; exit 1; }
done
mkdir -p "$dir"
curl -fsSL "$releases/latest/download/tilt-live" -o "$dir/tilt-live.part"
chmod 755 "$dir/tilt-live.part"
mv "$dir/tilt-live.part" "$dir/tilt-live"
# The first run downloads tilt itself.
"$dir/tilt-live" --version
echo "installed $dir/tilt-live"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "add $dir to your PATH, or run $dir/tilt-live" ;;
esac
