# tilt

tilt streams a Linux X11 desktop (Xvfb) to a browser or mobile WebView and lets one viewer at a
time take control of the mouse and keyboard. It is a single static binary that replaces
x11vnc + noVNC in sandboxes such as E2B and Sail:

- one HTTP/1.1 port serves the web client and a binary WebSocket at `/stream`;
- each viewer gets its own H.264 encoder (OpenH264, built in), decoded by WebCodecs in the page;
- capture is demand-driven (DAMAGE + MIT-SHM): a still screen costs no CPU, and only the rows
  that changed are read and converted;
- flow control is ack-based: frames are never dropped, the newest frame is sent when credit
  returns, and the bitrate follows the measured link;
- tokens are checked in the first WebSocket message, never in the URL query; a token file is
  re-read on every connection, so it can be written after the sandbox starts.

The wire protocol is in [docs/protocol.md](docs/protocol.md).

## Quick start (Docker)

```sh
docker compose up
```

Then open http://localhost:6090/#token=devtoken (view-only: `#token=viewtoken`). Press
**Control** in the toolbar to take over the desktop. The toolbar is a small strip of icons that
slides away while you control the desktop (move the pointer to the top edge, or tap it, to bring
it back); stream statistics are in its settings menu, or add `&stats=1` to the URL.

## Build

```sh
cargo build --release              # target/release/tilt
cargo test --workspace
scripts/build-static.sh            # static musl binaries: dist/tilt-{x86_64,aarch64}-unknown-linux-musl
```

OpenH264 is compiled from source; on x86_64 install `nasm` first, or the build silently falls
back to plain C. The X-dependent tests run on Linux, e.g. in the dev container:

```sh
docker build -t tilt-dev -f docker/dev.Dockerfile docker/
docker run --rm -v "$PWD":/src -v tilt-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/tmp/t tilt-dev cargo test --workspace -- --include-ignored
```

## Run

```sh
export TILT_TOKEN="$(head -c 16 /dev/urandom | od -An -tx1 | tr -dc 0-9a-f)"
echo "$TILT_TOKEN"
tilt --display :0
```

Open `http://<host>:6090/#token=<token>` with the printed token. Passing it in `TILT_TOKEN`
rather than `--token` keeps it out of the process list. Logs go to stderr; set
`TILT_LOG=debug` (or `RUST_LOG`) for more. `GET /healthz` answers `ok`; `GET /api/status` with
`Authorization: Bearer <control token>` lists viewers and their frame rate, bitrate and RTT.
SIGTERM, SIGINT, SIGHUP or SIGQUIT closes every session (1001), releases the keys and buttons
viewers held, and exits.

| flag | env | default | |
|---|---|---|---|
| `--bind <ADDR>` | `TILT_BIND` | `0.0.0.0:6090` | HTTP and WebSocket address |
| `--display <DISPLAY>` | `TILT_DISPLAY` | `$DISPLAY`, else `:0` | X display; Xvfb's abstract socket works too |
| `--token <TOKEN>` | `TILT_TOKEN` | | control-role token |
| `--view-token <TOKEN>` | `TILT_VIEW_TOKEN` | | view-only token |
| `--token-file <PATH>` | `TILT_TOKEN_FILE` | | control token = first non-empty line of the file (a UTF-8 byte order mark is skipped), re-read on every connection; while it is missing or empty, viewers get `not_ready` |
| `--no-auth` | `TILT_NO_AUTH` | off | everyone gets control; local testing only. A browser on another site's page is refused: its `Origin` must match `Host`, or `X-Forwarded-Host` or `Forwarded: host=` behind a proxy that rewrites `Host`, or be listed in `--allow-origin` |
| `--allow-origin <ORIGINS>` | `TILT_ALLOW_ORIGIN` | | with `--no-auth`, browser pages on these origins may connect too: comma-separated, written as the browser does (`https://desk.example.com`), or `*` for any |
| `--max-fps <N>` | `TILT_MAX_FPS` | 60 | per-viewer frame rate cap, 1 to 60 |
| `--bitrate-kbps <N>` | `TILT_BITRATE_KBPS` | 8000 | starting bitrate |
| `--min-bitrate-kbps <N>` | `TILT_MIN_BITRATE_KBPS` | 1000 | lowest bitrate under congestion |
| `--max-bitrate-kbps <N>` | `TILT_MAX_BITRATE_KBPS` | 20000 | highest bitrate |
| `--qp-min <N>` | `TILT_QP_MIN` | 20 | lowest quantizer (best quality), 12 to 51; at least 20 with `--profile high` |
| `--qp-max <N>` | `TILT_QP_MAX` | 28 | highest quantizer, up to 51; not below `--qp-min`. Higher values keep a busy screen within the bitrate, but text that moved stays blurred after it stops (see [Deployment](#deployment)) |
| `--profile <high\|baseline>` | `TILT_PROFILE` | `high` | `high` = CABAC; `baseline` = CAVLC Constrained Baseline |
| `--tail-frames <N>` | `TILT_TAIL_FRAMES` | 30 | most refinement frames after the screen goes still; the tail ends sooner once a re-encode at `--qp-min` comes out unchanged, as nothing is left to refine |
| `--rc-frame-skip` | `TILT_RC_FRAME_SKIP` | off | let OpenH264 rate control skip frames |
| `--max-viewers <N>` | `TILT_MAX_VIEWERS` | 4 | simultaneous viewers; each costs one encoder |
| `--max-msg-bytes <N>` | `TILT_MAX_MSG_BYTES` | 65536 | larger video frames are split into fragments; small ones keep a slow link's client hearing from the server while a keyframe arrives |
| `--notsent-lowat <BYTES>` | `TILT_NOTSENT_LOWAT` | 32768 | TCP_NOTSENT_LOWAT, 0 = off (Linux only) |
| `--poll-ms <N>` | `TILT_POLL_MS` | 1000 | safety full-frame compare while viewers wait, 0 = off |

At least one of `--token`, `--token-file` or `--no-auth` is required. Usage errors, such as a
missing token or an inconsistent quantizer or bitrate range, exit with code 2 at startup.
`tilt serve [flags]` is the same as `tilt [flags]`.

## Deployment

**Keep xfwm4 compositing on** (xfce's default). With it off, a terminal scrolling at full speed
kept Xvfb at 100% CPU and stalled tilt's screen grabs for 100–660 ms, once 1.25 s, freezing every
viewer's picture; with it on, grabs took 3.6 ms (median) and 7.5 ms (p95). Also run
`xfconf-query -c xfwm4 -p /general/unredirect_overlays -s false`: by default xfwm4 stops
compositing while one window covers the whole screen, and in our tests it did not start again
when that window shrank.

**CPU.** A still screen costs nothing, and an idle viewer under 1% of a core. Typing costs little:
with two key presses a second on a 1080p screen, tilt took 6% of a core (x86-64, one viewer),
since each change is grabbed only where it happened and the refinement tail stops as soon as
the encoder has nothing left to sharpen. Each viewer has
its own encoder, so a busy screen costs CPU per viewer. With a terminal scrolling over most of
the screen (Apple M4, Docker, arm64; 100% = one core):

| screen | tilt per viewer, 60 fps | with `--max-fps 30` |
|---|---|---|
| 1920×1080 | 52–56% | 27–29% |
| 1280×720 | 24–28% | not measured |

Xvfb took another 80–85% to draw that terminal. In a container limited to 2 vCPUs, two
viewers of the busy 1080p screen got 52–54 fps with tilt at 95% and Xvfb at 73%; with
`--max-fps 30` they held 30 fps and tilt used 56%. Two 720p viewers held 59–60 fps there. On 2
vCPUs, pass `--max-viewers 2`, since each further busy 1080p viewer costs about half a core
more (a quarter at 30 fps), and `--max-fps 30` when the desktop's own programs need the CPU.
x86-64 cloud vCPUs are often slower per core than this laptop, so leave more headroom there.

**Behind an HTTPS proxy (E2B, Sail).** tilt needs one TCP port that carries HTTP/1.1 and a
WebSocket, no UDP, so the sandbox's HTTPS edge is enough: the page loads from
`https://<sandbox host>/` and connects to `wss://` on the same host. With a token nothing else
needs setting up. The token travels in the URL fragment, which browsers never send, and then in
the first WebSocket message, so it stays out of the proxy's request logs.

`--no-auth` behind an HTTPS proxy: E2B's edge passes the public `Host` through, or with
`maskRequestHost` sets `X-Forwarded-Host` to it, so the page served from the sandbox URL
connects as is. A custom domain in front of E2B arrives as the sandbox's own host (E2B's
proxy drops any `X-Forwarded-Host` it receives), and Sail does not document the headers its
edge sends. An app whose WebView loads the client from its own files sends that origin
(`null` for a `file:` page). If the log says `refused a /stream upgrade from another origin`,
pass `--allow-origin` with the page's origin, or use a token. Origin checks stop neither DNS
rebinding nor non-browser clients: with `--no-auth`, whoever reaches the port has control.

**Sharp text or bounded bitrate (`--qp-max`).** Moving content is coded at up to `--qp-max`;
once the screen is still, the refinement tail sharpens it, but OpenH264 stops refining after a
few frames, so text that moved stays as coarse as that ceiling allows until it changes again. A
terminal that had scrolled `ls -R /usr` ended at a PSNR of 41.7 dB with the default 28, against
37.8–39.0 dB with 32, 34.3–39.5 with 36 and 33.6–34.2 with 40, where faint ghosts of the
scrolled-away text stayed in the blank space. A higher ceiling keeps a busy 1920×1080 screen
within `--max-bitrate-kbps` (about 20 Mbit/s with 32 or 36, 13–16 with 40, against 25–31 with
28) but did not lower latency on a fast link. Raise it only for links too slow for that
bitrate, where a smoother picture matters more than sharp text.

**Limits.**
- One TEXT message types at most 4096 bytes: the server cuts longer text at a character
  boundary and logs a warning. The web client sends a longer paste as several messages. No
  client message may exceed 64 KiB.
- The largest screen is 36,864 macroblocks (9.4 Mpixel, e.g. 4096×2304); viewers of a larger
  one get `unsupported_size`.
- Slow links: tilt keeps only a few unacknowledged frames per viewer and never drops one it has
  sent. The bitrate follows the measured link down to `--min-bitrate-kbps`; when the encoder
  cannot go lower at `--qp-max`, the frame rate drops instead, and each frame sent is the newest
  screen. Busy content can exceed `--max-bitrate-kbps`: a terminal scrolling over most of a
  1920×1080 screen took 25–31 Mbit/s at the default `--qp-max 28`. A keyframe is sent once,
  however long it takes: a 1600×900 web page's was 142 KB, 7.6 s at 150 kbit/s.

## E2B: attach to a running desktop sandbox

Upload the static x86_64 binary into an `@e2b/desktop` sandbox and point it at the display the
SDK started (`:0`, abstract socket only). Generate the token after `create`, never in a template.

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

Rewrite the token file at any time to rotate the token; sessions that are already connected stay
connected. Extend the sandbox timeout while someone is watching: streaming does not keep an E2B
sandbox alive.

## Sail: systemd unit

Sail images do not run `ENTRYPOINT`/`CMD`; run tilt as an enabled unit next to your Xvfb unit.
The token is generated when the service first starts in a Sailbox (never during image boot) and
kept across restarts; read it with `cat /run/tilt/token`.

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

Publish port 6090 as an `http` listener and open `https://sb-<uuid>-6090.sail.box/#token=<token>`.
