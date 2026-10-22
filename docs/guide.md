# tilt guide

Pick what you need. Every section stands on its own.

- [Start](#start): one command
- [Docker](#docker): try it with a demo desktop
- [Run the binary](#run-the-binary): on your own machine or VM
- [Network](#network): what has to reach the VM (Firecracker and other VMs)
- [Sail](#sail): sandboxes with a public UDP port
- [Flags](#flags)
- [Tips and limits](#tips-and-limits)
- [Build](#build)

## Start

On the Linux machine or VM whose screen you want:

```sh
npx tilt-live
```

Open the link it prints, then click the cursor button to take control.

- No Node? `curl -fsSL https://raw.githubusercontent.com/cloudycotton/tilt/main/install.sh | sh`, then `tilt`.
- Another display: `tilt --display :1`. Every [flag](#flags) works.
- **Always up to date.** Every time tilt starts it checks for a newer release and, if there is one,
  downloads it (checksum-verified), replaces itself and runs that. Offline, or without write access
  to its own file, it just runs. `TILT_NO_UPDATE=1` turns this off.
- Without `--token`, `--token-file` or `--no-auth`, tilt keeps a token in `~/.config/tilt/token`
  and prints the link with it.
- Static binaries for x86_64 and arm64 are on [GitHub releases](https://github.com/cloudycotton/tilt/releases).

## Docker

```sh
docker compose up
```

Open http://localhost:6090/#token=devtoken (view-only: `#token=viewtoken`).

**Using the viewer:** the cursor button takes control. The toolbar hides while you're in control:
move to the top edge (or tap it) to bring it back. Stream stats are in the `···` menu, or add
`&stats=1` to the URL.

## Run the binary

```sh
export TILT_TOKEN="$(head -c 16 /dev/urandom | od -An -tx1 | tr -dc 0-9a-f)"
echo "$TILT_TOKEN"
tilt --display :0
```

Open `http://<host>:6090/#token=<token>`.

<details>
<summary>Logs, health, status, shutdown</summary>

- `TILT_TOKEN` keeps the token out of the process list (unlike `--token`).
- Logs go to stderr. More: `TILT_LOG=debug` (or `RUST_LOG`).
- `GET /healthz` answers `ok`.
- `GET /api/status` with `Authorization: Bearer <control token>` lists viewers with frame rate,
  bitrate and RTT.
- SIGTERM, SIGINT, SIGHUP or SIGQUIT closes every session (1001), releases held keys and
  buttons, and exits.
</details>

## Network

tilt serves the page over HTTP and streams over WebTransport (QUIC, UDP), on port 6090 for
both by default (`--udp-port` moves the UDP side). Each frame travels on its own, so a lost packet delays only that frame, never
the cursor, your input or the frames after it. There is one protocol and nothing to fall back to:

- The viewer's browser must reach the port over **TCP and UDP**: TCP for the page, UDP for the
  stream. An HTTPS gateway or proxy cannot carry the stream.
- **Chrome, Edge, Firefox 125+ or Safari 26.4+** (WebTransport with certificate hashes). Other
  browsers get a clear message.
- No certificate needed: tilt makes a short-lived self-signed one and tells the page its hash.

Key press to drawn in Chrome, 1080p, with the stream through a UDP relay as on Sail:

| link | p50 / p95 |
|---|---|
| local | 4 / 7 ms |
| 50 ms round trip | 55 / 56 ms |
| 50 ms round trip, 2% packet loss | 54 / 106 ms |

**Firecracker (or any VM behind NAT).** Forward the port for both protocols. If browsers dial a
different UDP port (or host) than tilt's, say which with `--udp-public`:

```sh
# on the host: public 6090 -> VM 172.16.0.2:6090, TCP and UDP
for proto in tcp udp; do
  iptables -t nat -A PREROUTING -p $proto --dport 6090 -j DNAT --to-destination 172.16.0.2:6090
  iptables -A FORWARD -p $proto -d 172.16.0.2 --dport 6090 -j ACCEPT
done
# in the VM
tilt --display :0              # or --udp-public 7090 if the host maps UDP 7090 to it
```

Allow both in the host's firewall and any cloud security group.

## Sail

A systemd service needs a systemd-enabled Sail image: use
`sail.Image.debian("arm64", install_python=False).with_systemd()` or a Sail devbox
image. The plain Debian image does not include or run systemd. Devboxes already run
a small Xvfb display on `:0`; use a separate display (for example `:99`) for your
desktop and set `TILT_DISPLAY` to match.

A Sailbox publishes the page as an `http` listener and the stream as a `udp` listener. A port
can't be both, so give tilt's UDP side its own port, and tell tilt the public address Sail
gives it:

```python
import sail

sb = ...                                   # your running Sailbox, with Xvfb on :0
page = sb.expose(6090)                     # https://sb-<id>-6090.sail.box
udp = sb.expose(6091, "udp")               # a public host and port for the stream
sb.wait_for_listener(6091, protocol="udp", timeout=60)
public = f"{udp.endpoint.host}:{udp.endpoint.port}"

sb.exec(f"echo TILT_UDP_PUBLIC={public} > /etc/tilt.env && systemctl restart tilt").wait()
```

Sail images don't run `ENTRYPOINT`/`CMD`, so run tilt as a unit next to your Xvfb unit. The
token is made the first time the service starts (never during image boot) and kept across
restarts.

```ini
# /etc/systemd/system/tilt.service
[Unit]
Description=tilt desktop streamer
After=xvfb.service
Wants=xvfb.service

[Service]
# The account that runs Xvfb.
User=user
Environment=TILT_DISPLAY=:0
Environment=TILT_UDP_PORT=6091
Environment=TILT_TOKEN_FILE=/run/tilt/token
# TILT_UDP_PUBLIC=<host>:<port>, written after `expose` (above).
EnvironmentFile=-/etc/tilt.env
RuntimeDirectory=tilt
RuntimeDirectoryPreserve=restart
ExecStartPre=/bin/sh -c 'umask 077; [ -s /run/tilt/token ] || head -c 16 /dev/urandom | od -An -tx1 | tr -dc 0-9a-f > /run/tilt/token'
ExecStart=/usr/local/bin/tilt
Restart=always
RestartSec=1

[Install]
WantedBy=multi-user.target
```

Read the token with `cat /run/tilt/token` and open `https://sb-<id>-6090.sail.box/#token=<token>`.
Sail keeps a UDP port's public address while your org holds it, but can hand out a new one
(another Sailbox took it over): write the address again whenever `expose` returns another.

## Flags

Every flag also has an environment variable. With none of `--token`, `--token-file` or
`--no-auth`, tilt keeps its own token in `~/.config/tilt/token` and prints the link.

| flag | env | default | what it does |
|---|---|---|---|
| `--display <DISPLAY>` | `TILT_DISPLAY` | `$DISPLAY`, else `:0` | X display to stream (Xvfb's abstract socket works) |
| `--bind <ADDR>` | `TILT_BIND` | `0.0.0.0:6090` | address to listen on, TCP and UDP |
| `--udp-port <PORT>` | `TILT_UDP_PORT` | the `--bind` port | UDP port for the stream, when it can't share the page's ([Sail](#sail)) |
| `--udp-public <[HOST:]PORT>` | `TILT_UDP_PUBLIC` | the page's host, `--udp-port` | where browsers send the stream's UDP: a NAT's forwarded port, or a sandbox's public UDP listener ([network](#network)) |
| `--token <TOKEN>` | `TILT_TOKEN` | | token for viewers who can take control |
| `--view-token <TOKEN>` | `TILT_VIEW_TOKEN` | | token for view-only viewers |
| `--token-file <PATH>` | `TILT_TOKEN_FILE` | | control token from a file, re-read on every connection |
| `--no-auth` | `TILT_NO_AUTH` | off | everyone gets control. Local testing only |
| `--max-viewers <N>` | `TILT_MAX_VIEWERS` | 4 | viewers at once (each one costs an encoder) |
| `--max-fps <N>` | `TILT_MAX_FPS` | 60 | frame rate cap per viewer, 1 to 60 |
| `--resize <MIN>` | `TILT_RESIZE` | off | the screen takes each control viewer's window shape, at least MIN (`WxH`) per side. Xvfb resizes only within its startup screen: start it large, e.g. `-screen 0 2560x1600x24` |

<details>
<summary>More flags</summary>

| flag | env | default | |
|---|---|---|---|
| `--token-file <PATH>` | `TILT_TOKEN_FILE` | | control token = first non-empty line of the file (a UTF-8 byte order mark is skipped), re-read on every connection; while it is missing or empty, viewers get `not_ready` |
| `--no-auth` | `TILT_NO_AUTH` | off | everyone who can open the page gets control; local testing only |
| `--bitrate-kbps <N>` | `TILT_BITRATE_KBPS` | 8000 | starting bitrate |
| `--min-bitrate-kbps <N>` | `TILT_MIN_BITRATE_KBPS` | 1000 | lowest bitrate under congestion |
| `--max-bitrate-kbps <N>` | `TILT_MAX_BITRATE_KBPS` | 20000 | highest bitrate |
| `--qp-min <N>` | `TILT_QP_MIN` | 20 | lowest quantizer (best quality), 12 to 51; at least 20 with `--profile high` |
| `--qp-max <N>` | `TILT_QP_MAX` | 28 | highest quantizer, up to 51; not below `--qp-min`. Higher keeps a busy screen within the bitrate, but text that moved stays blurred after it stops (see [Sharp text or low bitrate](#tips-and-limits)) |
| `--profile <high\|baseline>` | `TILT_PROFILE` | `high` | `high` = CABAC; `baseline` = CAVLC Constrained Baseline |
| `--tail-frames <N>` | `TILT_TAIL_FRAMES` | 30 | most refinement frames after the screen goes still; none follow a frame coded at `--qp-min`, and the tail ends once a re-encode at `--qp-min` comes out unchanged |
| `--rc-frame-skip` | `TILT_RC_FRAME_SKIP` | off | let OpenH264 rate control skip frames |
| `--poll-ms <N>` | `TILT_POLL_MS` | 1000 | safety full-frame compare while viewers wait, backing off to 8× while it finds nothing; 0 = off |

Usage errors (a missing token, an inconsistent quantizer or bitrate range) exit with code 2 at
startup. `tilt serve [flags]` is the same as `tilt [flags]`.
</details>

## Tips and limits

- **Keep xfwm4 compositing on** (xfce's default), and run
  `xfconf-query -c xfwm4 -p /general/unredirect_overlays -s false`. Without compositing, a fast
  scrolling terminal can freeze every viewer's picture for up to a second.
- **Small VM (2 vCPUs)?** Use `--max-viewers 2`, and `--max-fps 30` if the desktop's own apps
  need the CPU. Firecracker or another CPU-only microVM: see [performance](performance.md).
- **Slow link?** It adapts by itself: bitrate first, then frame rate. Frames are never dropped
  once sent, and each one is the newest screen.
- **Sizes:** screens up to 9.4 Mpixel (e.g. 4096×2304). Pasted text over 4 KB is sent in pieces.

<details>
<summary>CPU and memory, measured</summary>

Nobody watching: tilt sleeps at about 0.03% of a core and 14 MB at 1080p (frames and buffers are
given back when the last viewer leaves). A viewer of a still screen costs about 0.1%. Typing two
keys a second on a 1080p screen took 3% of a core (x86-64, one viewer), because each change is
grabbed only where it happened.

Each viewer has its own encoder, so a busy screen costs CPU per viewer. A terminal scrolling over
most of the screen (Apple M4, Docker, arm64; 100% = one core):

| screen | tilt per viewer, 60 fps | with `--max-fps 30` |
|---|---|---|
| 1920×1080 | 52–56% | 27–29% |
| 1280×720 | 24–28% | not measured |

Xvfb took another 80–85% to draw that terminal. On 2 vCPUs, two viewers of the busy 1080p screen
got 52–54 fps (tilt 95%, Xvfb 73%); with `--max-fps 30` they held 30 fps at 56%. Two 720p viewers
held 59–60 fps. Each extra busy 1080p viewer costs about half a core (a quarter at 30 fps).
x86-64 cloud vCPUs are often slower per core than this laptop, so leave headroom.

Why compositing matters: with xfwm4 compositing off, a terminal scrolling at full speed kept
Xvfb at 100% CPU and stalled tilt's screen grabs for 100–660 ms (once 1.25 s). With it on, grabs
took 3.6 ms (median) and 7.5 ms (p95). By default xfwm4 stops compositing while one window
covers the whole screen, and in our tests it didn't start again when that window shrank, hence
the `unredirect_overlays` setting.
</details>

<details>
<summary>Sharp text or low bitrate (<code>--qp-max</code>)</summary>

Moving content is coded at up to `--qp-max`. Once the screen is still, refinement frames sharpen
it, but OpenH264 stops refining after a few frames, so text that moved stays as coarse as that
ceiling allows until it changes again.

A terminal that had scrolled `ls -R /usr` ended at 41.7 dB PSNR with the default 28, against
37.8–39.0 dB with 32, 34.3–39.5 with 36 and 33.6–34.2 with 40 (where faint ghosts of scrolled-away
text stayed). A higher ceiling keeps a busy 1920×1080 screen within `--max-bitrate-kbps` (about
20 Mbit/s with 32 or 36, 13–16 with 40, against 25–31 with 28) but didn't lower latency on a fast
link. Raise it only for links too slow for that bitrate.
</details>

<details>
<summary>All limits</summary>

- One TEXT message types at most 4096 bytes: the server cuts longer text at a character boundary
  and logs a warning. The web client sends a longer paste as several messages. No client message
  may exceed 64 KiB.
- The largest screen is 36,864 macroblocks (9.4 Mpixel, e.g. 4096×2304); viewers of a larger one
  get `unsupported_size`.
- tilt keeps only a few unacknowledged frames per viewer and never drops one it has sent. The
  bitrate follows the link down to `--min-bitrate-kbps`; below that, at `--qp-max`, the frame rate
  drops instead. Busy content can exceed `--max-bitrate-kbps`: a terminal scrolling over most of
  a 1920×1080 screen took 25–31 Mbit/s at the default `--qp-max 28`. A keyframe is sent once,
  however long it takes: a 1600×900 web page's was 142 KB, 7.6 s at 150 kbit/s.
</details>

## Build

```sh
cargo build --release              # target/release/tilt
cargo test --workspace
scripts/build-static.sh            # static musl binaries in dist/
```

OpenH264 is compiled from source. On x86_64, install `nasm` first, or the build quietly falls
back to plain C.

<details>
<summary>Run the X tests in the dev container</summary>

```sh
docker build -t tilt-dev -f docker/dev.Dockerfile docker/
docker run --rm -v "$PWD":/src -v tilt-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/tmp/t tilt-dev cargo test --workspace -- --include-ignored
```
</details>

<details>
<summary>How it works</summary>

tilt streams an X11 desktop (e.g. Xvfb) to a browser or mobile WebView, and one viewer at a time
can take the mouse and keyboard. It replaces x11vnc + noVNC.

- One port: HTTP serves the web client, WebTransport (QUIC, UDP) carries the stream.
- Each viewer gets its own H.264 encoder (OpenH264, built in), decoded by WebCodecs in the page.
- Capture is demand-driven (DAMAGE + MIT-SHM): a still screen costs no CPU, and only changed rows
  are read and converted.
- Flow control is ack-based: frames are never dropped, the newest frame goes out when credit
  returns, and the bitrate follows the measured link.
- Tokens are checked in the first message of the stream, never in a URL.

The wire protocol is in [protocol.md](protocol.md).
</details>
