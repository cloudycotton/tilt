# syntax=docker/dockerfile:1
#
# tilt images (build context: the repository root).
#   build     static musl binaries for $TARGETARCH, cross-compiled on the build host's own
#             architecture with cargo-zigbuild (no emulation)
#   artifact  just /tilt:  scripts/build-static.sh exports it with --output
#   desktop   ubuntu:22.04 + Xvfb + xfce, like E2B's desktop template, plus tilt, tilt-probe and
#             tilt-testpattern for the end-to-end runs (docker-compose.yml, scripts/e2e.sh)
#
# Both musl targets link OpenH264 statically with its assembly (NEON on aarch64, NASM on
# x86_64); zig substitutes its static libc++ for openh264's -lstdc++. Verified with zig 0.16.0
# and cargo-zigbuild 0.23.4; zig 0.17 is untested.

FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG BUILDARCH

# nasm: without it openh264-sys2 silently builds x86_64 without assembly (gated below).
RUN apt-get update \
 && apt-get install -y --no-install-recommends nasm xz-utils \
 && rm -rf /var/lib/apt/lists/*

# zig 0.16.0 and cargo-zigbuild 0.23.4 for the build host, pinned by checksum.
RUN set -eu; \
    case "$BUILDARCH" in \
      amd64) arch=x86_64; \
             zig_sha=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00; \
             czb_sha=6b69963040818ca1ef573d8135a95aa902e17cf7f873c32c6426642dadfe7f59 ;; \
      arm64) arch=aarch64; \
             zig_sha=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17; \
             czb_sha=25cf2cdc6cde848b33a761ff2cf46d1efe6ba7e1adfc4b6f3ee2d570cacb5419 ;; \
      *) echo "unsupported build platform: $BUILDARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/zig.tar.xz "https://ziglang.org/download/0.16.0/zig-$arch-linux-0.16.0.tar.xz"; \
    echo "$zig_sha  /tmp/zig.tar.xz" | sha256sum -c -; \
    mkdir /opt/zig; \
    tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1; \
    czb="cargo-zigbuild-$arch-unknown-linux-gnu"; \
    curl -fsSL -o /tmp/czb.tar.xz "https://github.com/rust-cross/cargo-zigbuild/releases/download/v0.23.4/$czb.tar.xz"; \
    echo "$czb_sha  /tmp/czb.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/czb.tar.xz -C /usr/local/cargo/bin --strip-components=1 "$czb/cargo-zigbuild"; \
    rm /tmp/zig.tar.xz /tmp/czb.tar.xz
ENV PATH=/opt/zig:$PATH
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY web web
COPY tools tools

# Declared this late so that the toolchain layers above are shared by both targets.
ARG TARGETARCH

# The registry cache is shared by both architectures' builds, so they take turns on it; cargo's
# own lock lives outside the mount and cannot keep two containers apart. zig's cache keeps the
# libc++, compiler-rt and musl objects it builds for the target on first link.
RUN --mount=type=cache,id=tilt-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=tilt-target-$TARGETARCH,target=/src/target \
    --mount=type=cache,id=tilt-zig-$TARGETARCH,target=/root/.cache/zig \
    set -eu; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported target platform: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    cargo zigbuild --release --locked --target "$target" -p tilt -p tilt-e2e; \
    if [ "$TARGETARCH" = amd64 ]; then \
      output=$(ls -t target/"$target"/release/build/openh264-sys2-*/output | head -n 1); \
      [ -n "$output" ] || { echo "no openh264-sys2 build output to check" >&2; exit 1; }; \
      if grep -q "Failed to compile NASM" "$output"; then \
        echo "OpenH264 was built without its x86 assembly (NASM failed); see $output" >&2; exit 1; \
      fi; \
    fi; \
    mkdir /out; \
    cp target/"$target"/release/tilt target/"$target"/release/tilt-probe \
       target/"$target"/release/tilt-testpattern /out/

FROM scratch AS artifact
COPY --from=build /out/tilt /tilt

FROM ubuntu:22.04 AS desktop
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      xvfb xfce4 xfce4-terminal dbus-x11 x11-xserver-utils x11-utils xdotool xterm \
      fonts-dejavu-core ca-certificates curl procps \
 && rm -rf /var/lib/apt/lists/*

# E2B runs the desktop as "user". Seeding the panel config skips the panel's first-start
# "default or empty panel?" dialog, which would otherwise sit on top of the screen. The socket
# directories are root-owned and sticky, as the X server and ICE expect from a non-root user.
RUN useradd --create-home --uid 1000 --shell /bin/bash user \
 && mkdir -p -m 1777 /tmp/.X11-unix /tmp/.ICE-unix \
 && mkdir -p /home/user/.config/xfce4/xfconf/xfce-perchannel-xml \
 && cp /etc/xdg/xfce4/panel/default.xml \
       /home/user/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml \
 && chown -R user:user /home/user/.config

COPY --from=build /out/tilt /out/tilt-probe /out/tilt-testpattern /usr/local/bin/
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/entrypoint.sh

USER user
WORKDIR /home/user
EXPOSE 6090
HEALTHCHECK --interval=2s --timeout=2s --start-period=60s --retries=3 \
  CMD curl -fs http://localhost:6090/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
