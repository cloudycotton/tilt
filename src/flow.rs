//! Per-session flow control: credit that gates the encoder, and the bitrate controller
//! (brief section 5.4). Pure logic over caller-supplied instants, so it unit-tests without a network.
//!
//! Every hop between us and the viewer (E2B's proxies, the browser) buffers, so the kernel socket
//! shows backpressure only once seconds of video are queued. The client therefore acks each access
//! unit, and we encode only while the unacked video stays within about one bandwidth-delay product.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowConfig {
    pub fps: u32,
    pub start_bps: u32,
    pub min_bps: u32,
    pub max_bps: u32,
}

/// A snapshot for the `stats` message and /api/status.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FlowStats {
    pub rtt_ms: f32,
    pub min_rtt_ms: f32,
    pub queue_ms: f32,
    pub inflight_frames: u32,
    pub inflight_bytes: usize,
    pub est_bw_bps: u64,
    pub bitrate_bps: u32,
    pub blocked_ms_last_s: u32,
}

const DEFAULT_MIN_RTT: Duration = Duration::from_millis(50);
const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);
const BW_WINDOW: Duration = Duration::from_secs(2);
const STALL_AFTER: Duration = Duration::from_secs(5);
/// Each stall reset with no ACK since doubles the next wait, up to 2^this times STALL_AFTER.
const STALL_DOUBLINGS_MAX: u32 = 2;
const MIN_FRAME_CAP: u32 = 3;
const MAX_FRAME_CAP: u32 = 32;
const MIN_BYTE_BUDGET: f64 = 64.0 * 1024.0;

const EVAL_EVERY: Duration = Duration::from_millis(500);
const CONGESTED_QUEUE_MS: f64 = 60.0;
const CONGESTED_ACKS: u32 = 3;
const CALM_QUEUE_MS: f64 = 25.0;
const CALM_FOR: Duration = Duration::from_secs(2);
const INCREASE_HOLD_OFF: Duration = Duration::from_millis(1500);
const USAGE_WINDOW: Duration = Duration::from_secs(1);
/// A drain probe ends anyway after this long, so a lost ACK cannot pin the session at one frame.
const PROBE_MAX: Duration = Duration::from_secs(1);

/// One access unit awaiting its ack, with the delivery counters as they were when it was sent.
struct InFlight {
    seq: u32,
    bytes: usize,
    sent_at: Instant,
    delivered: u64,
    delivered_at: Instant,
    /// Sent into an empty pipe, so its RTT sample carries none of our own queue.
    clean: bool,
    /// The link, not the content, limited what we sent: the frame filled the byte budget, or
    /// the content uses most of the target bitrate. Only then does its delivery rate measure
    /// the link (BBR's "not app-limited"); light content measures only itself. Filling the
    /// frame cap does not count: a few small frames do that whenever the RTT rises.
    link_limited: bool,
}

/// Windowed minimum or maximum of timestamped samples (a monotonic deque: the front is the
/// extreme, and values get less extreme towards the back, which is the newest sample).
struct Windowed {
    samples: VecDeque<(Instant, f64)>,
    window: Duration,
    max: bool,
}

impl Windowed {
    fn new(window: Duration, max: bool) -> Windowed {
        Windowed {
            samples: VecDeque::new(),
            window,
            max,
        }
    }

    fn push(&mut self, at: Instant, v: f64) {
        while let Some(&(_, back)) = self.samples.back() {
            let dominated = if self.max { back <= v } else { back >= v };
            if !dominated {
                break;
            }
            self.samples.pop_back();
        }
        self.samples.push_back((at, v));
        while self
            .samples
            .front()
            .is_some_and(|&(t, _)| at.saturating_duration_since(t) > self.window)
        {
            self.samples.pop_front();
        }
    }

    /// The extreme over the window ending at `now`.
    fn get(&self, now: Instant) -> Option<f64> {
        self.samples
            .iter()
            .find(|&&(t, _)| now.saturating_duration_since(t) <= self.window)
            .map(|&(_, v)| v)
    }

    /// The newest sample, which re-seeds the minimum once the window has expired.
    fn newest(&self) -> Option<f64> {
        self.samples.back().map(|&(_, v)| v)
    }
}

/// Credit and bitrate state for one session's video.
pub struct FlowControl {
    cfg: FlowConfig,
    /// Latest instant any call has seen, for `stats()`.
    now: Instant,

    inflight: VecDeque<InFlight>,
    inflight_bytes: usize,
    /// Bytes acked so far, and when the last of them was.
    delivered: u64,
    delivered_at: Instant,

    srtt_ms: Option<f64>,
    min_rtt_ms: Windowed,
    /// The lowest RTT confirmed since the last drain probe ended (see `on_ack`).
    min_rtt_ref: Option<f64>,
    /// Delivery rate samples in bytes/s.
    bw: Windowed,
    /// Only the samples that measure the link (InFlight::link_limited, or higher than those),
    /// for the congestion response: during light content the others measure our own rate.
    link_bw: Windowed,
    /// When any ACK last came, including one that retired nothing: a slow link still
    /// delivering frames from before a reset is alive.
    last_ack: Option<Instant>,
    /// Stall resets in a row with no ACK in between.
    stalls: u32,

    bitrate_bps: u32,
    high_queue_acks: u32,
    queue_congested: bool,
    /// While Some, a frame has been waiting for credit since then.
    blocked_since: Option<Instant>,
    blocked: VecDeque<(Instant, Instant)>,
    blocked_ms_last_s: u32,
    sent: VecDeque<(Instant, usize)>,
    last_eval: Option<Instant>,
    last_congestion: Option<Instant>,
    increase_after: Option<Instant>,
    /// While Some, a drain probe is running (see `on_ack`): send only into an empty pipe.
    probe_since: Option<Instant>,
    probes: u32,
}

impl FlowControl {
    pub fn new(cfg: FlowConfig, now: Instant) -> Self {
        FlowControl {
            cfg,
            now,
            inflight: VecDeque::new(),
            inflight_bytes: 0,
            delivered: 0,
            delivered_at: now,
            srtt_ms: None,
            min_rtt_ms: Windowed::new(MIN_RTT_WINDOW, false),
            min_rtt_ref: None,
            bw: Windowed::new(BW_WINDOW, true),
            link_bw: Windowed::new(BW_WINDOW, true),
            last_ack: None,
            stalls: 0,
            bitrate_bps: cfg
                .start_bps
                .clamp(cfg.min_bps, cfg.max_bps.max(cfg.min_bps)),
            high_queue_acks: 0,
            queue_congested: false,
            blocked_since: None,
            blocked: VecDeque::new(),
            blocked_ms_last_s: 0,
            sent: VecDeque::new(),
            last_eval: None,
            last_congestion: None,
            increase_after: None,
            probe_since: None,
            probes: 0,
        }
    }

    /// Records access unit `seq`, `bytes` long, as in flight.
    pub fn on_sent(&mut self, seq: u32, bytes: usize, now: Instant) {
        self.see(now);
        let clean = self.inflight.is_empty();
        // After an idle gap, measure delivery from this send, not from the last ack before the gap.
        if clean {
            self.delivered_at = now;
        }
        self.inflight_bytes += bytes;
        self.sent.push_back((now, bytes));
        let link_limited =
            self.inflight_bytes as f64 >= self.byte_budget(now) || self.uses_budget(now);
        self.inflight.push_back(InFlight {
            seq,
            bytes,
            sent_at: now,
            delivered: self.delivered,
            delivered_at: self.delivered_at,
            clean,
            link_limited,
        });
        self.end_blocked(now);
    }

    /// Cumulative: retires every in-flight seq <= `seq`, sampling RTT and delivery rate.
    pub fn on_ack(&mut self, seq: u32, decode_ms: u16, now: Instant) {
        self.see(now);
        self.last_ack = Some(now);
        self.stalls = 0;
        let mut newest = None;
        while self.inflight.front().is_some_and(|f| seq_le(f.seq, seq)) {
            let f = self.inflight.pop_front().expect("front exists");
            self.inflight_bytes -= f.bytes;
            self.delivered += f.bytes as u64;
            newest = Some(f);
        }
        // Duplicate, stale (from before a reset) or bogus acks retire nothing and teach nothing.
        let Some(f) = newest else { return };
        self.delivered_at = now;

        // The client's decode time is not path delay.
        let rtt = now
            .saturating_duration_since(f.sent_at)
            .saturating_sub(Duration::from_millis(decode_ms.into()))
            .max(Duration::from_millis(1));
        let rtt_ms = rtt.as_secs_f64() * 1000.0;
        self.srtt_ms = Some(self.srtt_ms.map_or(rtt_ms, |s| s + (rtt_ms - s) / 8.0));
        self.min_rtt_ms.push(now, rtt_ms);
        self.min_rtt_ref = Some(self.min_rtt_ref.map_or(rtt_ms, |r| r.min(rtt_ms)));
        // Drain probe (addition to the brief, after BBR's PROBE_RTT). Once the window's minimum
        // has risen well above the lowest RTT confirmed so far, whether in one step or in many
        // small ones as low samples expire, the samples left may all carry the standing queue
        // that our own credit allows (1.5 BDP + 2 frames). Taking them for the path's RTT would
        // grow the credit, and with it the queue, window after window. Instead, send only into
        // an empty pipe until one frame sent that way is acked: its sample is free of our
        // queue, and the window's minimum becomes the new reference.
        if self.probe_since.is_some_and(|t| {
            (f.clean && f.sent_at >= t) || now.saturating_duration_since(t) > PROBE_MAX
        }) {
            self.probe_since = None;
            self.min_rtt_ref = self.min_rtt_ms.get(now);
        } else if self.probe_since.is_none() {
            if let (Some(base), Some(min)) = (self.min_rtt_ref, self.min_rtt_ms.get(now)) {
                if min - base > (base / 4.0).max(10.0) {
                    self.probe_since = Some(now);
                    self.probes += 1;
                }
            }
        }
        if self.queue_ms(now) > CONGESTED_QUEUE_MS {
            self.high_queue_acks += 1;
            if self.high_queue_acks >= CONGESTED_ACKS {
                self.queue_congested = true;
            }
        } else {
            self.high_queue_acks = 0;
        }

        let interval = now
            .saturating_duration_since(f.delivered_at)
            .max(Duration::from_millis(1));
        let rate = (self.delivered - f.delivered) as f64 / interval.as_secs_f64();
        self.bw.push(now, rate);
        // A rate above the link estimate is a lower bound on the link even when the content
        // limited it (BBR keeps such samples too).
        if f.link_limited || self.link_bw.get(now).is_some_and(|b| rate > b) {
            self.link_bw.push(now, rate);
        }

        if self.blocked_since.is_some() && self.can_send(now) {
            self.end_blocked(now);
        }
    }

    /// Whether there is credit to encode and send another frame.
    pub fn can_send(&self, now: Instant) -> bool {
        if self.inflight.is_empty() {
            return true;
        }
        if self.probe_since.is_some() {
            return false;
        }
        (self.inflight.len() as u32) < self.frame_cap(now)
            && (self.inflight_bytes as f64) < self.byte_budget(now)
    }

    /// Records that a frame was waiting with no credit; feeds congestion detection.
    pub fn note_blocked(&mut self, now: Instant) {
        self.see(now);
        // Waiting out a drain probe says nothing about the link's capacity.
        if self.probe_since.is_none() {
            self.blocked_since.get_or_insert(now);
        }
    }

    /// Nothing in flight, waiting or recently sent: `tick` would change nothing, so the worker
    /// need not run it.
    pub fn quiet(&self) -> bool {
        self.inflight.is_empty()
            && self.sent.is_empty()
            && self.blocked.is_empty()
            && self.blocked_since.is_none()
            && !self.queue_congested
    }

    /// Runs the bitrate controller; Some(bps) when the target bitrate changed.
    pub fn tick(&mut self, now: Instant) -> Option<u32> {
        self.see(now);
        let horizon = now.checked_sub(USAGE_WINDOW);
        while horizon.is_some_and(|h| self.sent.front().is_some_and(|&(t, _)| t < h)) {
            self.sent.pop_front();
        }
        while horizon.is_some_and(|h| self.blocked.front().is_some_and(|&(_, end)| end < h)) {
            self.blocked.pop_front();
        }
        self.blocked_ms_last_s = self.blocked_within(now, USAGE_WINDOW).as_millis() as u32;

        if self
            .last_eval
            .is_some_and(|t| now.saturating_duration_since(t) < EVAL_EVERY)
        {
            return None;
        }
        self.last_eval = Some(now);

        let before = self.bitrate_bps;
        let queue_congested = std::mem::take(&mut self.queue_congested);
        let mostly_blocked = self.blocked_within(now, EVAL_EVERY) * 2 > EVAL_EVERY;
        if queue_congested || mostly_blocked {
            let mut target = f64::from(self.bitrate_bps) * 0.75;
            // Down to what the link was seen to carry, if it was: what light content was sent
            // at says nothing about the link (a latency spike while typing is no reason to
            // drop to the typing's bitrate).
            if let Some(link) = self.link_bw.get(now) {
                target = target.min(link * 8.0 * 0.85);
            }
            self.bitrate_bps = (target as u32).max(self.cfg.min_bps);
            self.last_congestion = Some(now);
            self.increase_after = Some(now + INCREASE_HOLD_OFF);
        } else {
            let calm = self
                .last_congestion
                .is_none_or(|t| now.saturating_duration_since(t) >= CALM_FOR)
                && self.increase_after.is_none_or(|t| now >= t);
            // Only raise a budget the content actually uses; static screens leave it alone.
            if calm && self.queue_ms(now) < CALM_QUEUE_MS && self.uses_budget(now) {
                let raised = (f64::from(self.bitrate_bps) * 1.1) as u32;
                self.bitrate_bps = raised.min(self.cfg.max_bps).max(self.bitrate_bps);
            }
        }
        (self.bitrate_bps != before).then_some(self.bitrate_bps)
    }

    /// True when a frame is in flight and for 5 s nothing has moved: no ACK of any kind came
    /// and no video was written to the socket (`last_write`, the writer's last completed
    /// VIDEO message), counting from when the oldest frame was queued. A keyframe that takes
    /// longer than that over a slow link is still being written, or acked late.
    /// After a reset with no ACK since, the wait doubles, up to 20 s.
    pub fn stalled(&self, now: Instant, last_write: Option<Instant>) -> bool {
        let Some(front) = self.inflight.front() else {
            return false;
        };
        let moved = [last_write, self.last_ack]
            .into_iter()
            .flatten()
            .fold(front.sent_at, Instant::max);
        now.saturating_duration_since(moved)
            > STALL_AFTER * (1 << self.stalls.min(STALL_DOUBLINGS_MAX))
    }

    /// Forgets everything in flight after a stall, restoring credit. Late ACKs for those
    /// frames then retire nothing but still count as signs of life (`stalled`).
    pub fn reset(&mut self, now: Instant) {
        self.forget(now);
        self.stalls = self.stalls.saturating_add(1);
    }

    /// The client paused video. Forgets what is in flight: a client that froze holds those
    /// frames and acks them when it thaws, and RTT and rate samples spanning the pause would
    /// read as heavy congestion. Also ends any wait for credit, as no frame waits while paused.
    pub fn pause(&mut self, now: Instant) {
        self.forget(now);
    }

    fn forget(&mut self, now: Instant) {
        self.see(now);
        self.end_blocked(now);
        self.inflight.clear();
        self.inflight_bytes = 0;
        self.high_queue_acks = 0;
        self.queue_congested = false;
        self.probe_since = None;
    }

    pub fn stats(&self) -> FlowStats {
        let now = self.now;
        FlowStats {
            rtt_ms: self.srtt_ms.unwrap_or(0.0) as f32,
            min_rtt_ms: if self.srtt_ms.is_some() {
                self.min_rtt(now).as_secs_f32() * 1000.0
            } else {
                0.0
            },
            queue_ms: self.queue_ms(now) as f32,
            inflight_frames: self.inflight.len() as u32,
            inflight_bytes: self.inflight_bytes,
            est_bw_bps: (self.est_bw(now) * 8.0) as u64,
            bitrate_bps: self.bitrate_bps,
            blocked_ms_last_s: self.blocked_ms_last_s,
        }
    }

    fn see(&mut self, now: Instant) {
        self.now = self.now.max(now);
    }

    fn frame_interval_s(&self) -> f64 {
        1.0 / f64::from(self.cfg.fps.max(1))
    }

    /// Whether the last second's frames add up to most of the target bitrate.
    fn uses_budget(&self, now: Instant) -> bool {
        let horizon = now.checked_sub(USAGE_WINDOW);
        let sent: usize = self
            .sent
            .iter()
            .filter(|&&(t, _)| horizon.is_none_or(|h| t >= h))
            .map(|&(_, b)| b)
            .sum();
        sent as f64 >= 0.6 * f64::from(self.bitrate_bps) / 8.0
    }

    /// About 1.5 bandwidth-delay products plus two frames, at the larger of the measured
    /// bandwidth and the target bitrate.
    fn byte_budget(&self, now: Instant) -> f64 {
        let min_rtt = self.min_rtt(now).as_secs_f64();
        let fi = self.frame_interval_s();
        let bw = self.est_bw(now).max(f64::from(self.bitrate_bps) / 8.0);
        MIN_BYTE_BUDGET.max(1.5 * bw * (min_rtt + 2.0 * fi))
    }

    /// Enough frames to cover one RTT at full frame rate, plus one in the encoder.
    fn frame_cap(&self, now: Instant) -> u32 {
        let fi = self.frame_interval_s();
        let cap = ((self.min_rtt(now).as_secs_f64() + fi) / fi).ceil() as u32 + 1;
        cap.clamp(MIN_FRAME_CAP, MAX_FRAME_CAP)
    }

    fn min_rtt(&self, now: Instant) -> Duration {
        self.min_rtt_ms
            .get(now)
            .or_else(|| self.min_rtt_ms.newest())
            .map_or(DEFAULT_MIN_RTT, |ms| Duration::from_secs_f64(ms / 1000.0))
    }

    /// Bytes/s; 0 without a recent sample.
    fn est_bw(&self, now: Instant) -> f64 {
        self.bw.get(now).unwrap_or(0.0)
    }

    fn queue_ms(&self, now: Instant) -> f64 {
        self.srtt_ms.map_or(0.0, |s| {
            (s - self.min_rtt(now).as_secs_f64() * 1000.0).max(0.0)
        })
    }

    fn end_blocked(&mut self, now: Instant) {
        if let Some(since) = self.blocked_since.take() {
            self.blocked.push_back((since, now));
        }
    }

    /// Time spent waiting for credit within the `window` ending at `now`.
    fn blocked_within(&self, now: Instant, window: Duration) -> Duration {
        let from = now.checked_sub(window).unwrap_or(now);
        let open = self.blocked_since.map(|s| (s, now));
        self.blocked
            .iter()
            .copied()
            .chain(open)
            .map(|(s, e)| e.min(now).saturating_duration_since(s.max(from)))
            .sum()
    }
}

/// `a <= b` for sequence numbers that may wrap.
pub fn seq_le(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) >= 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn cfg() -> FlowConfig {
        FlowConfig {
            fps: 60,
            start_bps: 8_000_000,
            min_bps: 1_000_000,
            max_bps: 20_000_000,
        }
    }

    #[test]
    fn acks_are_cumulative_and_sample_rtt_net_of_decode() {
        let t0 = Instant::now();
        let mut f = FlowControl::new(cfg(), t0);
        f.on_sent(1, 1000, t0);
        f.on_sent(2, 2000, t0 + 10 * MS);
        f.on_sent(3, 3000, t0 + 20 * MS);
        assert_eq!(
            (f.stats().inflight_frames, f.stats().inflight_bytes),
            (3, 6000)
        );
        // seq 2 sent at +10 ms, acked at +80 ms after 20 ms of decoding: 50 ms of path.
        f.on_ack(2, 20, t0 + 80 * MS);
        let s = f.stats();
        assert_eq!((s.inflight_frames, s.inflight_bytes), (1, 3000));
        assert!((s.rtt_ms - 50.0).abs() < 0.01 && (s.min_rtt_ms - 50.0).abs() < 0.01);
        f.on_ack(2, 0, t0 + 90 * MS);
        f.on_ack(1, 0, t0 + 90 * MS);
        assert_eq!(
            f.stats().inflight_frames,
            1,
            "duplicate and stale acks retire nothing"
        );
        assert!((f.stats().rtt_ms - 50.0).abs() < 0.01);
        // A decode time longer than the round trip floors the sample at 1 ms.
        f.on_ack(3, 60_000, t0 + 100 * MS);
        let s = f.stats();
        assert_eq!((s.inflight_frames, s.inflight_bytes), (0, 0));
        assert!((s.min_rtt_ms - 1.0).abs() < 0.01);
        assert!((s.rtt_ms - (50.0 + (1.0 - 50.0) / 8.0)).abs() < 0.01);
    }

    #[test]
    fn sequence_numbers_wrap() {
        assert!(seq_le(u32::MAX, 0) && seq_le(5, 5) && seq_le(1, 2));
        assert!(!seq_le(0, u32::MAX) && !seq_le(3, 2));
        let t0 = Instant::now();
        let mut f = FlowControl::new(cfg(), t0);
        f.on_sent(u32::MAX, 10, t0);
        f.on_sent(0, 10, t0);
        f.on_sent(1, 10, t0);
        f.on_ack(0, 0, t0 + 5 * MS);
        assert_eq!(f.stats().inflight_frames, 1);
    }

    fn frame_cap_at(rtt: Duration) -> u32 {
        let t0 = Instant::now();
        let mut f = FlowControl::new(cfg(), t0);
        f.on_sent(1, 100, t0);
        f.on_ack(1, 0, t0 + rtt);
        f.frame_cap(t0 + rtt)
    }

    #[test]
    fn frame_cap_grows_with_rtt() {
        let caps: Vec<u32> = [1, 20, 50, 100, 200, 400, 1000]
            .iter()
            .map(|&ms| frame_cap_at(ms * MS))
            .collect();
        assert_eq!(caps, [3, 4, 5, 8, 14, 26, 32]);
        // Before any sample the 50 ms default applies.
        let t0 = Instant::now();
        assert_eq!(FlowControl::new(cfg(), t0).frame_cap(t0), 5);
    }

    #[test]
    fn credit_counts_frames_and_bytes() {
        // Small frames: the frame cap binds (5 at the 50 ms default).
        let t0 = Instant::now();
        let mut f = FlowControl::new(cfg(), t0);
        let mut n = 0;
        while f.can_send(t0) {
            n += 1;
            f.on_sent(n, 1000, t0);
        }
        assert_eq!(n, 5);
        // Large frames: the byte budget binds. 1.5 * 1 MB/s * (50 + 33.3) ms = 125 KB.
        let mut f = FlowControl::new(cfg(), t0);
        let mut n = 0;
        while f.can_send(t0) {
            n += 1;
            f.on_sent(n, 60_000, t0);
        }
        assert_eq!(n, 3);
        // An empty pipe always has credit, however large the frame.
        let mut f = FlowControl::new(cfg(), t0);
        assert!(f.can_send(t0));
        f.on_sent(1, 10_000_000, t0);
        assert!(!f.can_send(t0));
    }

    const SEC: Duration = Duration::from_secs(1);

    #[test]
    fn stall_and_reset() {
        let t0 = Instant::now();
        let mut f = FlowControl::new(cfg(), t0);
        assert!(!f.stalled(t0 + 60 * SEC, None));
        f.on_sent(1, 500, t0);
        f.on_sent(2, 500, t0 + SEC);
        assert!(!f.stalled(t0 + 5 * SEC, None));
        assert!(f.stalled(t0 + 5 * SEC + MS, None));
        f.reset(t0 + 5 * SEC + MS);
        assert!(!f.stalled(t0 + 9 * SEC, None));
        assert!(f.can_send(t0));
        assert_eq!(f.stats().inflight_bytes, 0);
        // A late ack for a forgotten frame is harmless, and newer frames track normally.
        f.on_sent(3, 700, t0 + Duration::from_secs(6));
        f.on_ack(2, 0, t0 + Duration::from_secs(7));
        assert_eq!(f.stats().inflight_frames, 1);
        f.on_ack(3, 0, t0 + Duration::from_secs(7));
        assert_eq!(f.stats().inflight_frames, 0);
    }

    #[test]
    fn a_stall_counts_from_the_last_write_and_any_ack() {
        // A large frame still being written to a slow socket is not stalled (protocol P3).
        let t0 = Instant::now();
        let mut f = FlowControl::new(cfg(), t0);
        f.on_sent(1, 200_000, t0);
        let written = Some(t0 + 4 * SEC);
        assert!(!f.stalled(t0 + 6 * SEC, written));
        assert!(!f.stalled(t0 + 9 * SEC, written));
        assert!(f.stalled(t0 + 9 * SEC + MS, written));

        // A 668 KB keyframe on a 60 KB/s link takes 11.1 s to arrive (pipeline P4). The reset
        // at 5 s forgets it; frames sent after it queue behind it; then its late ACK, which
        // retires nothing, and theirs keep the session out of further resets.
        let mut f = FlowControl::new(cfg(), t0);
        f.on_sent(1, 668_000, t0);
        let reset_at = t0 + 5 * SEC + MS;
        assert!(f.stalled(reset_at, None));
        f.reset(reset_at);
        f.on_sent(2, 3_000, reset_at);
        // With no ACK since the reset, the next one waits twice as long.
        assert!(!f.stalled(reset_at + 9 * SEC, None));
        f.on_ack(1, 2, t0 + 11_100 * MS);
        assert_eq!(f.stats().inflight_frames, 1);
        assert!(!f.stalled(t0 + 16 * SEC, None));
        f.on_ack(2, 2, t0 + 11_200 * MS);
        f.on_sent(3, 3_000, t0 + 11_300 * MS);
        assert!(!f.stalled(t0 + 16 * SEC, None));
        assert!(
            f.stalled(t0 + 16_301 * MS, None),
            "back to 5 s once ACKs flow"
        );
        // Without any ACK the waits double up to 20 s.
        let mut f = FlowControl::new(cfg(), t0);
        let mut t = t0;
        for wait in [5, 10, 20, 20] {
            f.on_sent(1, 1_000, t);
            assert!(!f.stalled(t + wait * SEC, None));
            t += wait * SEC + MS;
            assert!(f.stalled(t, None));
            f.reset(t);
        }
    }

    #[test]
    fn pausing_forgets_frames_in_flight_and_ends_a_wait_for_credit() {
        // Out of credit when the client pauses (pipeline P1): the bitrate must not fall while
        // nothing waits to be sent.
        let t0 = Instant::now();
        let mut f = FlowControl::new(cfg(), t0);
        let mut seq = 0;
        while f.can_send(t0) {
            seq += 1;
            f.on_sent(seq, 1_000, t0);
        }
        f.note_blocked(t0);
        f.pause(t0 + 10 * MS);
        assert!(f.can_send(t0 + 10 * MS));
        for i in 1..=50 {
            assert_eq!(f.tick(t0 + i * 100 * MS), None);
        }
        assert_eq!(f.stats().bitrate_bps, 8_000_000);
        assert_eq!(f.stats().blocked_ms_last_s, 0);

        // A client that froze after pausing acks what it held when it thaws, 3 s later
        // (pipeline X1): those ACKs span the pause and must teach nothing.
        let mut f = FlowControl::new(cfg(), t0);
        for seq in 1..=20 {
            let t = t0 + seq * 20 * MS;
            f.on_sent(seq, 5_000, t);
            f.on_ack(seq, 1, t + 50 * MS);
        }
        let before = f.stats();
        let t = t0 + SEC;
        for seq in 21..=26 {
            f.on_sent(seq, 5_000, t + seq * MS);
        }
        f.pause(t + 30 * MS);
        for seq in 21..=26 {
            f.on_ack(seq, 1, t + 3 * SEC);
        }
        assert_eq!(f.tick(t + 3 * SEC), None);
        let after = f.stats();
        assert_eq!(
            (after.rtt_ms, after.min_rtt_ms),
            (before.rtt_ms, before.min_rtt_ms)
        );
        assert_eq!(after.bitrate_bps, 8_000_000);
    }

    #[test]
    fn static_content_keeps_the_bitrate() {
        let t0 = Instant::now();
        let mut f = FlowControl::new(cfg(), t0);
        for i in 0..100 {
            assert_eq!(f.tick(t0 + i * 100 * MS), None);
        }
        assert_eq!(f.stats().bitrate_bps, 8_000_000);
    }

    /// A bottleneck link: a FIFO drained at `rate` bytes/s, `one_way` propagation each way, and
    /// a client that decodes in `decode` before acking.
    struct Link {
        rate: f64,
        one_way: Duration,
        /// Added to the path of frames sent while it is set: a radio's latency spike.
        extra: Duration,
        decode: Duration,
        free_at: Instant,
        acks: VecDeque<(Instant, u32)>,
    }

    impl Link {
        fn new(rate: f64, rtt: Duration, t0: Instant) -> Link {
            Link {
                rate,
                one_way: rtt / 2,
                extra: Duration::ZERO,
                decode: 2 * MS,
                free_at: t0,
                acks: VecDeque::new(),
            }
        }

        fn send(&mut self, now: Instant, seq: u32, bytes: usize) {
            let done = self.free_at.max(now) + Duration::from_secs_f64(bytes as f64 / self.rate);
            self.free_at = done;
            let at = done + self.one_way + self.extra + self.decode + self.one_way;
            // In order, like TCP.
            let at = self.acks.back().map_or(at, |&(last, _)| at.max(last));
            self.acks.push_back((at, seq));
        }

        fn queue_delay(&self, now: Instant) -> Duration {
            self.free_at.saturating_duration_since(now)
        }
    }

    #[derive(Default)]
    struct Phase {
        max_queue: Duration,
        max_inflight_bytes: usize,
        delivered_bytes: usize,
        frames: u32,
        bitrate_at_end: u32,
        min_bitrate: u32,
        max_bitrate: u32,
        ms: u64,
    }

    impl Phase {
        /// Shown with `cargo test -- --nocapture`.
        fn print(&self, name: &str) {
            eprintln!(
                "{name}: max_queue={}ms max_inflight={}B goodput={:.0}B/s fps={:.1} bitrate min/max/end={}/{}/{}",
                self.max_queue.as_millis(),
                self.max_inflight_bytes,
                self.delivered_bytes as f64 * 1000.0 / self.ms as f64,
                f64::from(self.frames) * 1000.0 / self.ms as f64,
                self.min_bitrate,
                self.max_bitrate,
                self.bitrate_at_end,
            );
        }
    }

    /// Drives a sender that offers a new frame every frame interval (latest frame wins: while it
    /// has no credit it keeps waiting, then sends one frame), in 1 ms steps.
    struct Sim {
        t0: Instant,
        step: u64,
        flow: FlowControl,
        link: Link,
        seq: u32,
        last_send: Option<Instant>,
        bitrate: u32,
    }

    impl Sim {
        fn new(rate: f64, rtt: Duration) -> Sim {
            let t0 = Instant::now();
            Sim {
                t0,
                step: 0,
                flow: FlowControl::new(cfg(), t0),
                link: Link::new(rate, rtt, t0),
                seq: 0,
                last_send: None,
                bitrate: cfg().start_bps,
            }
        }

        fn run(&mut self, secs: u64, frame_bytes: impl Fn(u32) -> usize) -> Phase {
            self.run_ms(secs * 1000, frame_bytes)
        }

        fn run_ms(&mut self, ms: u64, frame_bytes: impl Fn(u32) -> usize) -> Phase {
            let fi = Duration::from_secs(1) / cfg().fps;
            let mut p = Phase {
                min_bitrate: u32::MAX,
                ms,
                ..Phase::default()
            };
            for _ in 0..ms {
                self.step += 1;
                let now = self.t0 + self.step as u32 * MS;
                while let Some(&(at, seq)) = self.link.acks.front() {
                    if at > now {
                        break;
                    }
                    self.link.acks.pop_front();
                    self.flow.on_ack(seq, 2, now);
                }
                if self.step.is_multiple_of(100) {
                    if let Some(bps) = self.flow.tick(now) {
                        self.bitrate = bps;
                    }
                }
                if self.last_send.is_none_or(|t| now >= t + fi) {
                    if self.flow.can_send(now) {
                        let bytes = frame_bytes(self.bitrate);
                        self.seq += 1;
                        self.flow.on_sent(self.seq, bytes, now);
                        self.link.send(now, self.seq, bytes);
                        self.last_send = Some(now);
                        p.frames += 1;
                        p.delivered_bytes += bytes;
                    } else {
                        self.flow.note_blocked(now);
                    }
                }
                p.max_queue = p.max_queue.max(self.link.queue_delay(now));
                p.max_inflight_bytes = p.max_inflight_bytes.max(self.flow.stats().inflight_bytes);
                p.min_bitrate = p.min_bitrate.min(self.bitrate);
                p.max_bitrate = p.max_bitrate.max(self.bitrate);
            }
            p.bitrate_at_end = self.bitrate;
            p
        }

        fn set_rate(&mut self, rate: f64) {
            self.link.rate = rate;
        }
    }

    #[test]
    fn overload_keeps_the_queue_bounded_and_the_link_busy() {
        // 100 KB frames offered at 60 fps (6 MB/s) into a 2 MB/s, 40 ms RTT bottleneck, for 60 s.
        // The frames never shrink (think of an encoder already at its maximum QP), so only the
        // credit stands between the sender and an ever-growing queue.
        let mut sim = Sim::new(2_000_000.0, 40 * MS);
        let warmup = sim.run(5, |_| 100_000);
        warmup.print("overload warmup 0-5 s");
        assert!(
            warmup.min_bitrate < 8_000_000,
            "no back-off: {}",
            warmup.min_bitrate
        );
        let mut phases = Vec::new();
        for i in 0..5 {
            let p = sim.run(11, |_| 100_000);
            p.print(&format!("overload {}-{} s", 5 + 11 * i, 16 + 11 * i));
            phases.push(p);
        }
        for p in std::iter::once(&warmup).chain(&phases) {
            // Without credit the queue would grow by 2 s every second.
            assert!(
                p.max_queue < Duration::from_millis(300),
                "queue {:?}",
                p.max_queue
            );
            assert!(
                p.max_inflight_bytes <= 500_000,
                "inflight {}",
                p.max_inflight_bytes
            );
        }
        for p in &phases {
            // No ratchet from one min-RTT window to the next.
            assert!(
                p.max_queue <= phases[0].max_queue + 10 * MS,
                "queue grew to {:?}",
                p.max_queue
            );
            // Credit sized to the bandwidth-delay product still keeps the link (nearly) saturated.
            let goodput = p.delivered_bytes as f64 / 11.0;
            assert!(goodput > 0.85 * 2_000_000.0, "goodput {goodput}");
            // Frames that cannot shrink keep the sender blocked: the target sits at the floor.
            assert_eq!(p.bitrate_at_end, cfg().min_bps);
        }
        assert!(sim.flow.probes >= 4, "probes {}", sim.flow.probes);
    }

    #[test]
    fn bitrate_backs_off_under_congestion_and_recovers() {
        // Frames sized by the bitrate, like the encoder's rate control does.
        let sized = |bps: u32| (bps / 8 / 60) as usize;
        let mut sim = Sim::new(2_000_000.0, 40 * MS);

        // 16 Mbit/s of capacity: climb from 8 Mbit/s and settle around the link rate.
        let roomy = sim.run(15, sized);
        roomy.print("bitrate 2 MB/s 0-15 s");
        assert!(
            roomy.max_bitrate > 14_000_000,
            "never climbed: {}",
            roomy.max_bitrate
        );
        assert!(
            roomy.max_queue < Duration::from_millis(200),
            "queue {:?}",
            roomy.max_queue
        );

        // Capacity drops to 4 Mbit/s: back off below it within a few seconds, queue bounded.
        sim.set_rate(500_000.0);
        let squeezed_start = sim.run(4, sized);
        squeezed_start.print("bitrate 0.5 MB/s 15-19 s");
        let squeezed = sim.run(10, sized);
        squeezed.print("bitrate 0.5 MB/s 19-29 s");
        assert!(
            squeezed_start.max_queue < Duration::from_millis(500),
            "queue {:?}",
            squeezed_start.max_queue
        );
        assert!(
            squeezed.max_bitrate <= 4_400_000,
            "still {}",
            squeezed.max_bitrate
        );
        assert!(
            squeezed.max_queue < Duration::from_millis(250),
            "queue {:?}",
            squeezed.max_queue
        );
        let goodput = squeezed.delivered_bytes as f64 / 10.0;
        assert!(goodput > 0.5 * 500_000.0, "goodput {goodput}");

        // Capacity returns: the bitrate climbs back.
        sim.set_rate(2_000_000.0);
        let recovered = sim.run(15, sized);
        recovered.print("bitrate 2 MB/s again 29-44 s");
        assert!(
            recovered.bitrate_at_end > 10_000_000,
            "only {}",
            recovered.bitrate_at_end
        );
    }

    #[test]
    fn a_slowly_rising_minimum_still_triggers_the_drain_probe() {
        // 10 KB frames offered at 60 fps into a 250 KB/s, 100 ms RTT link (pipeline X2): a
        // clean sample is about 142 ms. The standing queue our credit allows makes the window
        // minimum creep up in steps smaller than the probe threshold once old samples expire;
        // without a probe min_rtt, credit and queue ratchet up every 10 s, to over a second.
        let mut sim = Sim::new(250_000.0, 100 * MS);
        let mut worst = Duration::ZERO;
        for i in 0..6 {
            let p = sim.run(10, |_| 10_000);
            p.print(&format!("250 KB/s {}-{} s", 10 * i, 10 * i + 10));
            if i > 0 {
                worst = worst.max(p.max_queue);
            }
        }
        let s = sim.flow.stats();
        assert!(s.rtt_ms < 2.0 * 142.0, "srtt {} ms", s.rtt_ms);
        assert!(
            s.min_rtt_ms < 1.25 * 142.0 + 10.0,
            "min_rtt {} ms",
            s.min_rtt_ms
        );
        assert!(worst < Duration::from_millis(250), "queue {worst:?}");
        assert!(sim.flow.probes >= 5, "probes {}", sim.flow.probes);
    }

    #[test]
    fn a_latency_spike_during_light_content_costs_at_most_a_quarter() {
        // 2 KB frames at 60 fps (about 1 Mbit/s, typing) on a fast 40 ms path; the radio adds
        // 100 ms for 300 ms (pipeline P3). The rate samples measure the typing, not the link,
        // so the congestion response must not clamp the target to them.
        let mut sim = Sim::new(10_000_000.0, 40 * MS);
        sim.run(5, |_| 2_000);
        sim.link.extra = 100 * MS;
        let spike = sim.run_ms(300, |_| 2_000);
        sim.link.extra = Duration::ZERO;
        let after = sim.run(3, |_| 2_000);
        spike.print("spike");
        after.print("after the spike");
        assert!(
            after.min_bitrate < 8_000_000,
            "the spike was not seen as congestion"
        );
        assert!(
            after.min_bitrate >= 6_000_000,
            "fell to {}",
            after.min_bitrate
        );
    }

    #[test]
    fn longer_paths_get_more_frames_in_flight() {
        // 60 fps of small frames over a 300 ms path: a 3-frame window would cap at ~10 fps.
        let mut sim = Sim::new(10_000_000.0, 300 * MS);
        sim.run(2, |_| 5_000);
        let p = sim.run(5, |_| 5_000);
        p.print("300 ms path 2-7 s");
        assert!(p.frames >= 5 * 58, "only {} frames in 5 s", p.frames);
    }
}
