#!/usr/bin/env bash
# Prepares a Claude Code cloud session (claude.ai/code, Ubuntu 24.04 x86_64) to build and test
# tilt. Two parts:
#
#   system   what docker/dev.Dockerfile installs (nasm for OpenH264's x86 assembly; Xvfb and
#            X tools for the X-dependent tests) plus the Playwright browsers e2e/ uses (Google
#            Chrome and WebKit). Needs no checkout.
#            Paste this whole file into the cloud environment's "Setup script" box: it runs
#            before Claude starts and the resulting VM is cached, so it costs nothing later.
#   project  cargo fetch and the npm packages of e2e/ and e2e/mock. Run every session by the
#            SessionStart hook in .claude/settings.json (cloud sessions only).
#
# Usage: scripts/cloud-setup.sh [system|project|all]     (default: system)
# Every step is skipped when already done, so it is safe to repeat.
#
# The browser download hosts are not on the cloud's default "Trusted" allowlist. Use network
# access "Custom" with the defaults plus cdn.playwright.dev, playwright.download.prss.microsoft.com,
# playwright.azureedge.net and dl.google.com, or the browser step is skipped with a warning and
# only the Rust side is set up.
set -euo pipefail

PLAYWRIGHT_VERSION=1.63.0   # e2e/package.json and e2e/mock/package.json
mode=${1:-system}

log() { printf '[cloud-setup] %s\n' "$*" >&2; }
as_root() { if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi; }

setup_system() {
  local packages=(nasm xvfb x11-utils x11-xserver-utils xdotool xterm xfwm4 procps jq pkg-config)
  local missing=() p
  for p in "${packages[@]}"; do
    dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p")
  done
  if [ ${#missing[@]} -gt 0 ]; then
    log "apt install ${missing[*]}"
    as_root apt-get update -qq
    as_root env DEBIAN_FRONTEND=noninteractive \
      apt-get install -y -qq --no-install-recommends "${missing[@]}"
  fi

  # shellcheck disable=SC1091
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  if ! command -v cargo >/dev/null; then
    log "installing rustup"
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  fi

  if command -v npx >/dev/null; then
    log "playwright $PLAYWRIGHT_VERSION: chrome, webkit"
    npx --yes "playwright@$PLAYWRIGHT_VERSION" install --with-deps chrome webkit \
      || log "WARNING: Playwright's browser download failed; allow its hosts (see the header)"
  fi
}

setup_project() {
  if [ -n "${CLAUDE_PROJECT_DIR:-}" ]; then
    cd "$CLAUDE_PROJECT_DIR"
  else
    cd "$(dirname "$0")/.."
  fi
  [ -f Cargo.toml ] || { log "no Cargo.toml in $PWD"; exit 1; }
  # rustup installs into ~/.cargo/bin, which a fresh shell may not have on its PATH.
  # shellcheck disable=SC1091
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

  log "cargo fetch"
  cargo fetch --locked

  local dir
  for dir in e2e e2e/mock; do
    [ -f "$dir/package.json" ] || continue
    # npm ci wipes node_modules first, so only run it when the lock file changed since the last.
    if [ -f "$dir/package-lock.json" ] && ! cmp -s "$dir/package-lock.json" "$dir/node_modules/.cloud-setup-lock"; then
      log "npm ci in $dir"
      (cd "$dir" && npm ci --no-audit --no-fund --loglevel=error && cp package-lock.json node_modules/.cloud-setup-lock)
    elif [ ! -f "$dir/package-lock.json" ] && [ ! -d "$dir/node_modules" ]; then
      log "npm install in $dir"
      (cd "$dir" && npm install --no-package-lock --no-audit --no-fund --loglevel=error)
    fi
  done
}

case "$mode" in
  system) setup_system ;;
  project) setup_project ;;
  all) setup_system; setup_project ;;
  *) echo "usage: $0 [system|project|all]" >&2; exit 2 ;;
esac
log "$mode: done"
