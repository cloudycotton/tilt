//! Encoder benchmark over recorded desktop activity (ignored; run by hand, see
//! docs/performance.md for how to record the input).
//!
//! TILT_BENCH_DIR holds raw I420 recordings, `<name>.yuv`, at TILT_BENCH_SIZE (default
//! 1920x1080), one frame per screen change. Each variant in TILT_BENCH_VARIANTS (default
//! `high,baseline`, the two `--profile`s) encodes every recording once, at TILT_BENCH_KBPS
//! (default 8000), and prints one line per run. TILT_BENCH_ONLY=a,b picks recordings. With
//! TILT_BENCH_OUT set, each run's stream is written there as `<name>.<variant>.h264` for a
//! quality check against the source (ffmpeg's psnr filter).
//!
//!   TILT_BENCH_DIR=/dev/shm/rec cargo test --release bench_recordings -- --ignored --nocapture

use std::time::Instant;

use super::convert::I420Frame;
use super::encoder::{EncoderSettings, H264Encoder};

fn variant(name: &str, w: u32, h: u32, bitrate_bps: u32) -> Option<EncoderSettings> {
    Some(EncoderSettings {
        width: w,
        height: h,
        max_fps: 60.0,
        bitrate_bps,
        min_qp: 20,
        max_qp: 28,
        cabac: match name {
            "high" => true,
            "baseline" => false,
            _ => return None,
        },
        frame_skip: false,
    })
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

#[test]
#[ignore = "benchmark: run by hand with recordings in TILT_BENCH_DIR"]
fn bench_recordings() {
    // CI runs ignored tests too; without recordings there is nothing to measure.
    let Some(dir) = env("TILT_BENCH_DIR") else {
        eprintln!("bench_recordings: TILT_BENCH_DIR is not set, skipping");
        return;
    };
    let size = env("TILT_BENCH_SIZE").unwrap_or_else(|| "1920x1080".into());
    let (w, h): (u32, u32) = size
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .expect("TILT_BENCH_SIZE is WxH");
    let kbps: u32 = env("TILT_BENCH_KBPS").map_or(8000, |k| k.parse().expect("kbps"));
    let variants = env("TILT_BENCH_VARIANTS").unwrap_or_else(|| "high,baseline".into());
    let out = env("TILT_BENCH_OUT");
    let only = env("TILT_BENCH_ONLY");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("TILT_BENCH_DIR is readable")
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.extension()? != "yuv" {
                return None;
            }
            p.file_stem()?.to_str().map(String::from)
        })
        .filter(|n| only.as_deref().is_none_or(|o| o.split(',').any(|x| x == n)))
        .collect();
    names.sort();
    let (luma, chroma) = ((w * h) as usize, (w * h / 4) as usize);
    println!("recording  variant    frames  mean_ms  p50_ms  p95_ms  max_ms    kbps60  mean_qp");
    for name in &names {
        let raw = std::fs::read(format!("{dir}/{name}.yuv")).expect("recording is readable");
        let frames: Vec<I420Frame> = raw
            .chunks_exact(luma + 2 * chroma)
            .map(|c| I420Frame {
                width: w,
                height: h,
                y: c[..luma].to_vec(),
                u: c[luma..luma + chroma].to_vec(),
                v: c[luma + chroma..].to_vec(),
            })
            .collect();
        assert!(frames.len() > 1, "{name}: needs at least two {size} frames");
        for v in variants.split(',') {
            let s = variant(v, w, h, kbps * 1000).unwrap_or_else(|| panic!("unknown variant {v}"));
            let mut enc = H264Encoder::new(&s).unwrap();
            let mut times = Vec::with_capacity(frames.len());
            let (mut bytes, mut qp) = (0usize, 0u64);
            let mut stream = Vec::new();
            for (i, f) in frames.iter().enumerate() {
                let t = Instant::now();
                let e = enc.encode(f, i as u64 * 1000 / 60, i == 0).unwrap();
                times.push(t.elapsed().as_secs_f64() * 1000.0);
                let e = e.expect("frame skipping is off");
                bytes += e.data.len();
                qp += u64::from(e.qp);
                stream.extend_from_slice(&e.data);
            }
            // The first frame is the IDR: report the steady state.
            let mut sorted = times[1..].to_vec();
            sorted.sort_by(f64::total_cmp);
            let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
            let rate = bytes as f64 * 8.0 / (frames.len() as f64 / 60.0) / 1000.0;
            println!(
                "{name:<10} {v:<9} {:>7}  {mean:>7.2}  {:>6.2}  {:>6.2}  {:>6.2}  {rate:>8.0}  {:>7.1}",
                frames.len(),
                pct(&sorted, 0.5),
                pct(&sorted, 0.95),
                sorted[sorted.len() - 1],
                qp as f64 / frames.len() as f64,
            );
            if let Some(out) = &out {
                std::fs::write(format!("{out}/{name}.{v}.h264"), &stream).unwrap();
            }
        }
    }
}
