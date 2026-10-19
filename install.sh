#!/bin/sh
# Installs tilt, which keeps itself up to date whenever it starts:
#
#   curl -fsSL https://raw.githubusercontent.com/cloudycotton/tilt/main/install.sh | sh
#
# To TILT_INSTALL_DIR, by default ~/.local/bin (/usr/local/bin as root). Linux on x86_64 or
# arm64; needs curl and sha256sum (or shasum).
set -eu
releases=${TILT_RELEASES:-https://github.com/cloudycotton/tilt/releases}
dir=${TILT_INSTALL_DIR:-}
if [ -z "$dir" ]; then
  if [ "$(id -u)" = 0 ]; then dir=/usr/local/bin; else dir=$HOME/.local/bin; fi
fi
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64 | Linux/amd64) triple=x86_64-unknown-linux-musl ;;
  Linux/aarch64 | Linux/arm64) triple=aarch64-unknown-linux-musl ;;
  *) echo "install.sh: tilt runs on Linux, x86_64 or arm64" >&2; exit 1 ;;
esac
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }
mkdir -p "$dir"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$releases/latest/download/tilt-$triple" -o "$tmp/tilt"
curl -fsSL "$releases/latest/download/SHA256SUMS" -o "$tmp/sums"
want=$(awk -v n="tilt-$triple" '$2 == n { print $1 }' "$tmp/sums")
[ -n "$want" ] && [ "$want" = "$(sha256 "$tmp/tilt")" ] || { echo "install.sh: checksum mismatch" >&2; exit 1; }
chmod 755 "$tmp/tilt"
mv "$tmp/tilt" "$dir/tilt"
echo "installed $dir/tilt"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "add $dir to your PATH, or run $dir/tilt" ;;
esac
