# tilt on CPU-only VMs

Notes for running tilt inside small CPU-only VMs, Firecracker microVMs in particular: what costs
CPU, what helps, what was tried and doesn't, and how to measure an encoder change.

Measured in a Firecracker microVM (4 vCPUs, Intel Xeon with AVX-512, kvm-clock) on a 1080p Xvfb
desktop with Chromium and xterm, tilt pinned to one vCPU and the viewer on another.

## What tilt costs while you work

One viewer at 1080p, in % of one vCPU:

| activity | tilt | of which encoding |
|---|---|---|
| typing in a terminal | 11% | 9% |
| scrolling a web page | 26% | 21% |
| dragging a window over a page | 38% | 31% |
| nobody watching, or a still screen | ~0% | 0% |

Encoding one 1080p frame with OpenH264 takes 3–4 ms here. Grabbing and converting only the
damaged rows takes about 1 ms. Every viewer has its own encoder.

## On a small VM

- **Give the VM 2 vCPUs or more.** On one vCPU, tilt's encoder and the desktop's apps take turns,
  and frames wait for the app.
- **Keep AVX2 in the CPU template.** OpenH264 picks its SIMD code (SSE2 to AVX2) at run time.
  Firecracker's T2/C3 templates keep AVX2; a template that hides it makes encoding several times
  slower.
- **`--profile baseline`** (CAVLC) encodes 10–25% faster than the default High (CABAC) for 10–15%
  more bits. `--profile auto` picks it on 1–2 vCPUs.
- **`--max-fps 30`** halves encoding CPU while the screen is busy; a still screen costs nothing
  either way.
- **A smaller screen, not a scaled stream.** A 1280×720 Xvfb encodes in about 40–60% of the
  time of 1080p. Scaling a 1080p screen down before encoding does not pay (see below).

## Tried and not worth it

Each measured on recordings of typing, scrolling and dragging (the benchmark below):

| idea | result |
|---|---|
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
