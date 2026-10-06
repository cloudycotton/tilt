#!/usr/bin/env bash
# Builds the static (musl) tilt binaries with docker buildx (docker/Dockerfile, stage
# "artifact"), cross-compiled on the host's own architecture, and checks that each one is
# statically linked and under 8 MiB:
#   dist/tilt-x86_64-unknown-linux-musl
#   dist/tilt-aarch64-unknown-linux-musl
# Usage: scripts/build-static.sh [amd64] [arm64]     (default: both)
set -euo pipefail
cd "$(dirname "$0")/.."

max_bytes=$((8 * 1024 * 1024))
[ $# -gt 0 ] || set -- amd64 arm64
mkdir -p dist
for arch in "$@"; do
  case "$arch" in
    amd64) triple=x86_64-unknown-linux-musl ;;
    arm64) triple=aarch64-unknown-linux-musl ;;
    *) echo "unknown architecture '$arch' (expected amd64 or arm64)" >&2; exit 2 ;;
  esac
  # One platform per build: the local exporter then writes a plain /tilt, on any buildx driver.
  out=$(mktemp -d)
  docker buildx build --platform "linux/$arch" --file docker/Dockerfile --target artifact \
    --output "type=local,dest=$out" .
  bin=dist/tilt-$triple
  mv "$out/tilt" "$bin"
  rmdir "$out"

  info=$(file -b "$bin")
  size=$(wc -c <"$bin" | tr -d ' ')
  echo "$bin: $size bytes, $info"
  case "$info" in
    *"statically linked"* | *"static-pie linked"*) ;;
    *) echo "$bin is not statically linked" >&2; exit 1 ;;
  esac
  if [ "$size" -gt "$max_bytes" ]; then
    echo "$bin is larger than 8 MiB" >&2
    exit 1
  fi
done
