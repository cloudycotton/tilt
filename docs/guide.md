# tilt guide

Pick what you need. Every section stands on its own.

- [Start](#start): one command
- [Docker](#docker): try it with a demo desktop
- [Run the binary](#run-the-binary): on your own machine or VM
- [Behind a proxy](#behind-a-proxy): a remote VM at `https://gw.example.com/vm1/`
- [E2B](#e2b) · [Sail](#sail): sandboxes
- [Flags](#flags)
- [Tips and limits](#tips-and-limits)
- [Build](#build)

## Start

```sh
npx tilt-live
```

Open the link it prints, then click the cursor button to take control.

- No Node? `curl -fsSL https://raw.githubusercontent.com/cloudycotton/tilt/main/install.sh | sh`, then `tilt-live`.
- Another display: `tilt-live --display :1`. Every [flag](#flags) works.
- It always runs the latest release and updates itself on start.

<details>
<summary>More about tilt-live</summary>

- Downloads static binaries (x86_64, arm64) from [GitHub releases](https://github.com/cloudycotton/tilt/releases), checksum-verified.
- `TILT_VERSION=0.2.0` pins a release. `TILT_NO_UPDATE=1` skips the update check.
- Without `--token`, `--token-file` or `--no-auth`, it keeps a token in `~/.config/tilt-live/token`.
- Want the bare binary? Download `tilt-<arch>-unknown-linux-musl` from a release.
</details>

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

## Behind a proxy

Run tilt on the VM bound to loopback, and point a path (or host) of your HTTPS gateway at it.

```sh
tilt --display :0 --bind 127.0.0.1:6090 --token-file /run/tilt/token
```

**nginx**

```nginx
location /vm1/ {
    proxy_pass http://127.0.0.1:6090/;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_read_timeout 1h;
}
```

**Caddy:** `handle_path /vm1/* { reverse_proxy 127.0.0.1:6090 }`

Open `https://gw.example.com/vm1/#token=<token>`.

Rules:
- Any path prefix works. A missing trailing slash is added for you.
- The gateway must speak **HTTP/1.1** to tilt (no WebSocket over HTTP/2), and must not
  compress or buffer the stream.
- Only one TCP port is needed, no UDP. The token travels in the URL fragment and then inside the
  WebSocket, so it never shows up in proxy logs.

<details>
<summary>Latency through a gateway</summary>

tilt adds about 10 ms between a change on screen and its frame leaving the server (grab 0.6 ms,
conversion 0.8 ms, encoding 6–10 ms at 1080p). The page draws each frame as soon as it's
decoded; the rest is your network's round trip. Key press to drawn, 1080p, through
[e2e/proxy.mjs](../e2e/proxy.mjs):

| round trip added | Chrome p50 / p95 | WebKit p50 / p95 |
|---|---|---|
| 0 ms | 22 / 27 ms | 38 / 47 ms |
| 50 ms | 70 / 80 ms | 85 / 98 ms |

An animated screen kept 60 fps at 100 ms round trip (frame gaps p50 16.7 ms, p95 21 ms): flow
control keeps about 1.5 bandwidth-delay products in flight, so frame rate doesn't drop as
latency grows.

Measure your own gateway:

```sh
node e2e/proxy.mjs --upstream 127.0.0.1:6090 --prefix /desk/ --delay-ms 25
cd e2e && TILT_URL=http://localhost:6190/desk/ npx playwright test
```
</details>

<details>
<summary>Using <code>--no-auth</code> behind a proxy</summary>

With `--no-auth`, a browser page's `Origin` must match the `Host` (or `X-Forwarded-Host`, or
`Forwarded: host=`) or be listed in `--allow-origin`.

- E2B's edge passes the public `Host` through, or sets `X-Forwarded-Host` with
  `maskRequestHost`, so the page served from the sandbox URL just works.
- A custom domain in front of E2B arrives as the sandbox's own host (E2B drops any
  `X-Forwarded-Host` it receives). Sail doesn't document the headers its edge sends.
- An app's WebView loading the client from its own files sends that origin (`null` for `file:`).

If the log says `refused a /stream upgrade from another origin`, pass `--allow-origin` with the
page's origin, or use a token. Origin checks stop neither DNS rebinding nor non-browser clients:
with `--no-auth`, **anyone who reaches the port has control**.
</details>

## E2B

Upload the static x86_64 binary into an `@e2b/desktop` sandbox and point it at display `:0`.
Make the token after `create`, never in a template.

```js
import { Sandbox } from '@e2b/desktop'
import { randomBytes } from 'node:crypto'
import { readFile } from 'node:fs/promises'

const desktop = await Sandbox.create({ timeoutMs: 30 * 60_000 })
const token = randomBytes(16).toString('hex')

await desktop.files.write('/home/user/tilt', new Blob([await readFile('dist/tilt-x86_64-unknown-linux-musl')]))
await desktop.files.write('/home/user/.tilt-token', token)   // re-read on every connection
await desktop.commands.run('chmod 755 /home/user/tilt && chmod 600 /home/user/.tilt-token')
await desktop.commands.run(
  '/home/user/tilt --display :0 --token-file /home/user/.tilt-token',
  { background: true },
)

const url = `https://${desktop.getHost(6090)}/#token=${encodeURIComponent(token)}`
```

- Rotate the token any time by rewriting the file. Connected sessions stay connected.
- Streaming doesn't keep an E2B sandbox alive: extend its timeout while someone is watching.

## Sail

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
Environment=TILT_TOKEN_FILE=/run/tilt/token
RuntimeDirectory=tilt
RuntimeDirectoryPreserve=restart
ExecStartPre=/bin/sh -c 'umask 077; [ -s /run/tilt/token ] || head -c 16 /dev/urandom | od -An -tx1 | tr -dc 0-9a-f > /run/tilt/token'
ExecStart=/usr/local/bin/tilt
Restart=always
RestartSec=1

[Install]
WantedBy=multi-user.target
```

Publish port 6090 as an `http` listener, read the token with `cat /run/tilt/token`, and open
`https://sb-<uuid>-6090.sail.box/#token=<token>`.

## Flags

Every flag also has an environment variable. You need one of `--token`, `--token-file` or
`--no-auth`.

| flag | env | default | what it does |
|---|---|---|---|
| `--display <DISPLAY>` | `TILT_DISPLAY` | `$DISPLAY`, else `:0` | X display to stream (Xvfb's abstract socket works) |
| `--bind <ADDR>` | `TILT_BIND` | `0.0.0.0:6090` | address to listen on |
| `--token <TOKEN>` | `TILT_TOKEN` | | token for viewers who can take control |
| `--view-token <TOKEN>` | `TILT_VIEW_TOKEN` | | token for view-only viewers |
| `--token-file <PATH>` | `TILT_TOKEN_FILE` | | control token from a file, re-read on every connection |
| `--no-auth` | `TILT_NO_AUTH` | off | everyone gets control. Local testing only |
| `--max-viewers <N>` | `TILT_MAX_VIEWERS` | 4 | viewers at once (each one costs an encoder) |
| `--max-fps <N>` | `TILT_MAX_FPS` | 60 | frame rate cap per viewer, 1 to 60 |

<details>
<summary>More flags</summary>

| flag | env | default | |
|---|---|---|---|
| `--token-file <PATH>` | `TILT_TOKEN_FILE` | | control token = first non-empty line of the file (a UTF-8 byte order mark is skipped), re-read on every connection; while it is missing or empty, viewers get `not_ready` |
| `--no-auth` | `TILT_NO_AUTH` | off | everyone gets control; local testing only. A browser on another site's page is refused: its `Origin` must match `Host`, or `X-Forwarded-Host` or `Forwarded: host=` behind a proxy that rewrites `Host`, or be listed in `--allow-origin` |
| `--allow-origin <ORIGINS>` | `TILT_ALLOW_ORIGIN` | | with `--no-auth`, browser pages on these origins may connect too: comma-separated, written as the browser does (`https://desk.example.com`), or `*` for any |
| `--bitrate-kbps <N>` | `TILT_BITRATE_KBPS` | 8000 | starting bitrate |
| `--min-bitrate-kbps <N>` | `TILT_MIN_BITRATE_KBPS` | 1000 | lowest bitrate under congestion |
| `--max-bitrate-kbps <N>` | `TILT_MAX_BITRATE_KBPS` | 20000 | highest bitrate |
| `--qp-min <N>` | `TILT_QP_MIN` | 20 | lowest quantizer (best quality), 12 to 51; at least 20 with `--profile high` |
| `--qp-max <N>` | `TILT_QP_MAX` | 28 | highest quantizer, up to 51; not below `--qp-min`. Higher keeps a busy screen within the bitrate, but text that moved stays blurred after it stops (see [Sharp text or low bitrate](#tips-and-limits)) |
| `--profile <high\|baseline>` | `TILT_PROFILE` | `high` | `high` = CABAC; `baseline` = CAVLC Constrained Baseline |
| `--tail-frames <N>` | `TILT_TAIL_FRAMES` | 30 | most refinement frames after the screen goes still; none follow a frame coded at `--qp-min`, and the tail ends once a re-encode at `--qp-min` comes out unchanged |
| `--rc-frame-skip` | `TILT_RC_FRAME_SKIP` | off | let OpenH264 rate control skip frames |
| `--max-msg-bytes <N>` | `TILT_MAX_MSG_BYTES` | 65536 | larger video frames are split into fragments; small ones keep a slow link's client hearing from the server while a keyframe arrives |
| `--notsent-lowat <BYTES>` | `TILT_NOTSENT_LOWAT` | 32768 | TCP_NOTSENT_LOWAT, 0 = off (Linux only) |
| `--poll-ms <N>` | `TILT_POLL_MS` | 1000 | safety full-frame compare while viewers wait, backing off to 8× while it finds nothing; 0 = off |

Usage errors (a missing token, an inconsistent quantizer or bitrate range) exit with code 2 at
startup. `tilt serve [flags]` is the same as `tilt [flags]`.
</details>

## Tips and limits

- **Keep xfwm4 compositing on** (xfce's default), and run
  `xfconf-query -c xfwm4 -p /general/unredirect_overlays -s false`. Without compositing, a fast
  scrolling terminal can freeze every viewer's picture for up to a second.
- **Small VM (2 vCPUs)?** Use `--max-viewers 2`, and `--max-fps 30` if the desktop's own apps
  need the CPU.
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
can take the mouse and keyboard. It replaces x11vnc + noVNC in sandboxes like E2B and Sail.

- One HTTP/1.1 port serves the web client and a binary WebSocket at `/stream`.
- Each viewer gets its own H.264 encoder (OpenH264, built in), decoded by WebCodecs in the page.
- Capture is demand-driven (DAMAGE + MIT-SHM): a still screen costs no CPU, and only changed rows
  are read and converted.
- Flow control is ack-based: frames are never dropped, the newest frame goes out when credit
  returns, and the bitrate follows the measured link.
- Tokens are checked in the first WebSocket message, never in the URL query.

The wire protocol is in [protocol.md](protocol.md).
</details>
