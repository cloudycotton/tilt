# Dev image for building tilt and running its X-dependent tests on Linux.
# The source tree is bind-mounted at /src; nothing is copied in.
FROM rust:1-bookworm

# nasm: OpenH264's x86 assembly (without it the build silently falls back to C).
# xvfb, x11-utils, x11-xserver-utils, xdotool, xterm, xfwm4: a headless display to test against.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      nasm xvfb x11-utils x11-xserver-utils xdotool xterm xfwm4 procps ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
