//! Process-wide monotonic clock in microseconds, shared by capture, input and the encoders.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

static START: OnceLock<Instant> = OnceLock::new();

/// Pins the epoch. Call first thing in `main` so timestamps count from process start.
pub fn init() {
    START.get_or_init(Instant::now);
}

/// Monotonic microseconds since process start.
pub fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// The Instant at which `now_us()` was `us`, for a time passed between threads as an atomic.
pub fn instant_at(us: u64) -> Instant {
    *START.get_or_init(Instant::now) + Duration::from_micros(us)
}

/// Frame pacing on a grid: after a frame at `now` that was due at `slot` (None for the first),
/// the next one is due one `interval` after `slot`. A frame that went out late, because a
/// timer woke late (often a millisecond or more in a VM) or the frame itself came late, so
/// takes its lateness back from the next interval instead of lowering the frame rate; but
/// the grid moves by at most half an interval, so two frames are never closer than that.
pub fn next_slot(slot: Option<Instant>, now: Instant, interval: Duration) -> Instant {
    let earliest = now.checked_sub(interval / 2).unwrap_or(now);
    slot.map_or(now, |s| s.max(earliest)) + interval
}

/// Rate limit for repeated log lines: lets one through per period and counts the rest.
#[derive(Debug)]
pub struct LogEvery {
    period: Duration,
    last: Option<Instant>,
    suppressed: u64,
}

impl LogEvery {
    pub const fn new(period: Duration) -> LogEvery {
        LogEvery {
            period,
            last: None,
            suppressed: 0,
        }
    }

    /// Some(number suppressed since the last one) when this occurrence should be logged.
    pub fn ready(&mut self, now: Instant) -> Option<u64> {
        if self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) < self.period)
        {
            self.suppressed += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_slot_keeps_the_grid_and_limits_catching_up() {
        let t0 = Instant::now();
        let fi = Duration::from_millis(16);
        let ms = Duration::from_millis(1);
        assert_eq!(next_slot(None, t0, fi), t0 + fi);
        // On time, or up to half an interval late: the grid holds.
        assert_eq!(next_slot(Some(t0), t0, fi), t0 + fi);
        assert_eq!(next_slot(Some(t0), t0 + 3 * ms, fi), t0 + fi);
        assert_eq!(next_slot(Some(t0), t0 + 8 * ms, fi), t0 + fi);
        // Later than that (a pause, a slow frame): half an interval from now.
        assert_eq!(next_slot(Some(t0), t0 + 100 * ms, fi), t0 + 108 * ms);
        // Early (a pulled-forward frame): the slot it took is used up.
        assert_eq!(next_slot(Some(t0 + fi), t0 + 8 * ms, fi), t0 + 2 * fi);
    }

    #[test]
    fn clock_is_monotonic() {
        init();
        let a = now_us();
        std::thread::sleep(Duration::from_millis(2));
        assert!(now_us() >= a + 2000);
        let (before, at, after) = (Instant::now(), instant_at(now_us()), Instant::now());
        assert!(before.saturating_duration_since(at) <= Duration::from_micros(1) && at <= after);
    }

    #[test]
    fn log_every_passes_one_per_period() {
        let t0 = Instant::now();
        let mut l = LogEvery::new(Duration::from_secs(5));
        assert_eq!(l.ready(t0), Some(0));
        assert_eq!(l.ready(t0 + Duration::from_secs(1)), None);
        assert_eq!(l.ready(t0 + Duration::from_secs(4)), None);
        assert_eq!(l.ready(t0 + Duration::from_secs(5)), Some(2));
        assert_eq!(l.ready(t0 + Duration::from_secs(6)), None);
    }
}
