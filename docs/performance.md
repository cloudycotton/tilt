# tilt on CPU-only VMs

Notes for running tilt inside small CPU-only VMs, Firecracker microVMs in particular: what costs
CPU, what helps, what was tried and doesn't, and how to measure an encoder change.

Measured in a Firecracker microVM (4 vCPUs, Intel Xeon with AVX-512, kvm-clock) on a 1080p Xvfb
desktop with Chromium and xterm, tilt pinned to one vCPU and the viewer on another.

## What tilt costs while you work

One viewer at 1080p, in % of one vCPU:

| activity | tilt | before 0.2.0 |
|---|---|---|
| typing in a terminal | 2–3% | 11% |
| scrolling a web page | 23% | 27–30% |
| dragging a window over a page | 35% | 44% |
| nobody watching, or a still screen | ~0% | ~0% |

Encoding one 1080p frame with OpenH264 takes about 3 ms here, and most of that is the same
whatever changed: OpenH264 works through all 8160 macroblocks of the screen, and a still
frame costs as much as a typed character. So small changes skip the encoder (below). Every
viewer has its own encoder.

## Small changes are patches

When what changed since the viewer's last update covers at most an eighth of the screen, and
its pixels, run-length coded, fit in a frame's share of the bitrate (8 KiB at least), tilt
sends those pixels instead of an H.264 frame (`src/video/patch.rs`, docs/protocol.md). A
typed character is a patch of about 300 bytes that takes 6 µs to make, where a frame took
3–5 ms to encode and as long again to decode in the browser. Key press to drawn fell from
12 ms to 3 ms (p50, Chrome, local). Scrolls, window moves and video still go through H.264:
as pixels they would cost far more bytes.

Capture grabs only the damaged rows, and since 0.2.0 it also copies only the rows changed
since the buffer it reuses, rather than the whole last frame (3 MB at 1080p).

## Encoder settings

Background detection and the deblocking filter are off. Both are camera tools; on the
recordings below, turning both off makes frames 10–25% faster to encode and 15–25% smaller, at
1–3 dB more PSNR, at 8 Mbps and at 800 kbps alike. With background detection on, the
refinement of a still screen also stalled 2–3 dB short, since it skipped the macroblocks it
took for background.

## On a small VM

- **Give the VM 2 vCPUs or more.** On one vCPU, tilt's encoder and the desktop's apps take turns,
  and frames wait for the app.
- **Keep AVX2 in the CPU template.** OpenH264 picks its SIMD code (SSE2 to AVX2) at run time.
  Firecracker's T2/C3 templates keep AVX2; a template that hides it makes encoding several times
  slower.
- **`--max-fps 30`** halves encoding CPU while the screen is busy; a still screen costs nothing
  either way.
- **A smaller screen, not a scaled stream.** A 1280×720 Xvfb encodes in about 40–60% of the
  time of 1080p. Scaling a 1080p screen down before encoding does not pay (see below).

## Tried and not worth it

Each measured on recordings of typing, scrolling and dragging (the benchmark below):

| idea | result |
|---|---|
| `--profile baseline` with the settings above | no faster than High any more, for 10–15% more bits |
| mimalloc keeping freed memory for 1 s (`purge_delay`) | no change |
| OpenH264 screen-content mode | 3× the CPU per frame (10–15 ms) for 20–40% fewer bits |
| 2 or 4 encoder threads (one slice each) | at best 10–20% faster frames on idle vCPUs, slower when typing, 5–25% more bits |
| scaling 1080p to 720p or 900p before encoding | only 20–30% faster to encode, but 2–8× the bits: scrolls and window moves stop being whole-pixel motion |
| OpenH264's own downscaler | as slow as encoding 1080p, and 3–4× the bits |
| raising tilt's priority (nice −10, autogroup nice, SCHED_RR) | no change in latency with the vCPU saturated by other processes |

A halted vCPU wakes fast here: about 0.5 ms of key-to-picture latency, so polling to keep vCPUs
awake would not pay either.

## Frame pacing

Capture grabs on its own 60 Hz grid (with an input pull-forward). The encoder used to keep a
second grid, and once the two drifted apart, which an input pull-forward was enough for, every
frame of an animation waited for the encoder's slot: 9.5 ms on average between grab and encode,
on every frame, for as long as the screen kept changing. The encoder now limits the frame rate
with a rate limiter instead (`Schedule::new_slot` in `src/worker.rs`): a new capture is encoded
as soon as it arrives, and over any stretch at most one per frame interval goes out (plus one).
Grab to encode is now about 1 ms, and key-to-picture p95 on an animated screen stays at
17–22 ms where it ranged 18–34 ms. The frame rate cap, and with it the CPU, is unchanged.

## Over a lossy link

On a path that loses packets, as across the internet to a Sail sandbox, QUIC resends the lost
packet about one round trip later. Its frame waits for it, and so does every frame after it,
since the browser decodes them in order, so their ACKs come a round trip late too. Credit used
to cover one round trip of frames, so each loss also stopped the encoder: the picture froze,
then jumped. Credit now covers two round trips while the path holds no queue of ours, and
falls back to one once a queue builds (`frame_cap` in `src/flow.rs`), so a slow link queues no
more than before.

Continuous scrolling in Chrome at 60 fps, 720p, through `tools/wan/run.sh` (a UDP relay with
delay, jitter, loss and a bandwidth cap), share of the viewer's refreshes that showed no new
frame:

| link | before | after |
|---|---|---|
| 50 ms round trip | 3.3% | 0.6% |
| 50 ms round trip, 1% loss | 15% | 8% |
| 100 ms round trip, 0.5% loss | 31% | 17% |
| 50 ms round trip, 3 Mbit/s | ~75% | ~75% |

Latency is unchanged on all four. What remains under loss is the resend itself: a frame one
round trip late is longer than the 45 ms the page buffers to smooth jitter. Only forward error
correction would hide it without adding that round trip of delay.

```sh
cargo build --release
tools/wan/run.sh "--delay 25 --jitter 4 --mbps 30 --loss 0.01" 20
```

## Measuring an encoder change

Record each workload losslessly, keep one frame per screen change, and convert to raw I420:

```sh
ffmpeg -f x11grab -framerate 60 -video_size 1920x1080 -i :99 -t 10 -c:v ffv1 scroll.mkv
mkdir -p /dev/shm/rec
ffmpeg -i scroll.mkv -vf mpdecimate=hi=1:lo=1:frac=0 -fps_mode passthrough \
  -pix_fmt yuv420p -f rawvideo /dev/shm/rec/scroll.yuv
```

Then run the benchmark (`src/video/bench.rs`) pinned to one vCPU:

```sh
TILT_BENCH_DIR=/dev/shm/rec taskset -c 1 \
  cargo test --release bench_recordings -- --ignored --nocapture
```

It prints encode time per frame (mean, p50, p95, max), bitrate at 60 fps and mean QP for each
`--profile`. Add a variant in `bench.rs` to try a new setting. `TILT_BENCH_OUT=dir` writes each
stream, to compare quality against the source with ffmpeg's `psnr` filter.
