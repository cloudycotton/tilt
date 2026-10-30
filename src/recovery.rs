//! Optional XOR recovery over four independent reliable data streams and one parity stream.
//! Retransmission remains the fallback when more than one shard is late. Never buffer frames
//! to build a code block: all five shards describe the same VIDEO message.

use std::time::{Duration, Instant};
use wtransport::quinn::PathStats;

pub const SHARD: u8 = 0x05;
pub const DATA_SHARDS: usize = 4;
pub const HEADER: usize = 10;
pub const MAX_FRAME: usize = 128 * 1024;
const MIN_FRAME: usize = 4096;
const SAMPLE: Duration = Duration::from_secs(1);
const BACKOFF: Duration = Duration::from_secs(5);

/// Uses QUIC packet RTT, not application ACK RTT (which includes receiver reordering).
pub struct Recovery {
    sampled: Instant,
    sent: u64,
    lost: u64,
    loss_until: Instant,
    backoff_until: Instant,
}

impl Recovery {
    pub fn new(now: Instant) -> Self {
        Self {
            sampled: now,
            sent: 0,
            lost: 0,
            loss_until: now,
            backoff_until: now,
        }
    }

    pub fn protect(&mut self, bytes: usize, path: &PathStats, now: Instant) -> bool {
        if now.duration_since(self.sampled) >= SAMPLE {
            let sent = path.sent_packets.saturating_sub(self.sent);
            let lost = path.lost_packets.saturating_sub(self.lost);
            if sent > 0 && lost > 0 && lost as f64 / sent as f64 >= 0.002 {
                self.loss_until = now + BACKOFF;
            }
            self.sampled = now;
            self.sent = path.sent_packets;
            self.lost = path.lost_packets;
        }
        // Extra parity must not compete with useful video on a filling bottleneck. Suppress
        // it immediately and allow the link five seconds to drain before trying again.
        if path.rtt.saturating_sub(path.min_rtt) > Duration::from_millis(15) {
            self.backoff_until = now + BACKOFF;
        }
        (MIN_FRAME..=MAX_FRAME).contains(&bytes)
            && path.min_rtt >= Duration::from_millis(80)
            && now < self.loss_until
            && now >= self.backoff_until
    }
}

/// Zero-pad the last shard; XOR recovers any single absent data shard. The extra payload is
/// ceil(frame/4), up to three padding bytes, and five small shard headers.
pub fn shards(video: &[u8]) -> Vec<Vec<u8>> {
    let size = video.len().div_ceil(DATA_SHARDS);
    let mut out = vec![vec![0; HEADER + size]; DATA_SHARDS + 1];
    for (i, shard) in out.iter_mut().enumerate() {
        shard[0] = SHARD;
        shard[1..5].copy_from_slice(&video[2..6]);
        shard[5..9].copy_from_slice(&(video.len() as u32).to_le_bytes());
        shard[9] = i as u8;
    }
    for (i, chunk) in video.chunks(size).enumerate() {
        out[i][HEADER..HEADER + chunk.len()].copy_from_slice(chunk);
        for (j, byte) in chunk.iter().enumerate() {
            out[DATA_SHARDS][HEADER + j] ^= byte;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_single_erasure_recovers_uneven_frames() {
        for len in [4096, 4097, 8191, MAX_FRAME] {
            let original: Vec<u8> = (0..len).map(|i| ((i * 37 + i / 251) % 256) as u8).collect();
            let encoded = shards(&original);
            assert!(encoded.iter().map(Vec::len).sum::<usize>() <= len + len.div_ceil(4) + 53);
            for missing in 0..DATA_SHARDS {
                let mut recovered = encoded[DATA_SHARDS][HEADER..].to_vec();
                for (i, shard) in encoded[..DATA_SHARDS].iter().enumerate() {
                    if i != missing {
                        for (a, b) in recovered.iter_mut().zip(&shard[HEADER..]) {
                            *a ^= b;
                        }
                    }
                }
                let from = missing * recovered.len();
                let end = (from + recovered.len()).min(len);
                assert_eq!(&recovered[..end - from], &original[from..end]);
            }
        }
    }

    #[test]
    fn recovery_requires_loss_and_backs_off_on_queue_growth() {
        let start = Instant::now();
        let mut recovery = Recovery::new(start);
        let mut path = PathStats::default();
        path.rtt = Duration::from_millis(250);
        path.min_rtt = path.rtt;
        path.sent_packets = 1000;
        assert!(!recovery.protect(8000, &path, start + SAMPLE));
        path.sent_packets += 1000;
        path.lost_packets = 10;
        let now = start + 2 * SAMPLE;
        assert!(recovery.protect(8000, &path, now));
        assert!(!recovery.protect(300, &path, now));
        assert!(!recovery.protect(MAX_FRAME + 1, &path, now));
        path.rtt += Duration::from_millis(16);
        assert!(!recovery.protect(8000, &path, now));
        path.rtt = path.min_rtt;
        path.sent_packets += 1000;
        path.lost_packets += 10;
        assert!(!recovery.protect(8000, &path, now + SAMPLE));
        path.sent_packets += 1000;
        path.lost_packets += 10;
        assert!(recovery.protect(8000, &path, now + BACKOFF));
        assert!(!recovery.protect(8000, &path, now + 2 * BACKOFF));
    }
}
