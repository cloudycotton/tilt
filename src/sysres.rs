//! What the box gives tilt: CPU capacity and load from the cgroup (v2 or v1) and /proc, memory
//! limits and usage, and returning freed memory to the system.
//!
//! Phase 0: the signatures that the governor, capture and admission code against. Readers
//! report no quota, no load and no limits until the cgroup and /proc parsers land.

use std::time::{Duration, Instant};

/// The CPUs tilt may use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuCapacity {
    /// min(quota, affinity) in cores, or the affinity count without a quota.
    pub cpus: f32,
    /// The cgroup `cpu.max` (v2) or `cpu.cfs_quota_us / cpu.cfs_period_us` (v1) quota in cores.
    pub quota_cpus: Option<f32>,
    /// CPUs in the affinity mask.
    pub affinity: usize,
}

/// Reads the CPU quota and affinity.
pub fn cpu_capacity() -> CpuCapacity {
    let affinity = std::thread::available_parallelism().map_or(1, usize::from);
    CpuCapacity {
        cpus: affinity as f32,
        quota_cpus: None,
        affinity,
    }
}

/// CPU use over one sampling window, in cores.
#[expect(
    dead_code,
    reason = "phase 0 stub: built by CpuMeter::sample (G3), read by G1"
)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuLoad {
    pub window: Duration,
    /// tilt itself: /proc/self/stat utime + stime.
    pub tilt_cores: f32,
    /// The whole box: cgroup `cpu.stat usage_usec` with a quota, else /proc/stat non-idle.
    pub box_cores: Option<f32>,
    /// Throttled share of the cgroup's CFS periods over the window.
    pub throttled_share: Option<f32>,
}

/// Turns successive CPU counter readings into [`CpuLoad`]s.
#[expect(dead_code, reason = "phase 0 stub: used by the governor (G1)")]
#[derive(Debug, Default)]
pub struct CpuMeter {}

#[expect(dead_code, reason = "phase 0 stub: used by the governor (G1)")]
impl CpuMeter {
    pub fn new() -> CpuMeter {
        CpuMeter::default()
    }

    /// The load since the previous call; None on the first call.
    pub fn sample(&mut self, _now: Instant) -> Option<CpuLoad> {
        None
    }
}

/// Memory limit and use, in bytes.
#[expect(dead_code, reason = "phase 0 stub: used by admission (G4)")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemInfo {
    /// cgroup v2 `memory.max` or v1 `memory.limit_in_bytes`; None when unlimited.
    pub limit: Option<u64>,
    /// cgroup v2 `memory.current` or v1 `memory.usage_in_bytes`.
    pub current: Option<u64>,
    /// /proc/meminfo MemAvailable.
    pub mem_available: Option<u64>,
}

/// Reads the memory limit and use.
#[expect(dead_code, reason = "phase 0 stub: used by admission (G4)")]
pub fn mem_info() -> MemInfo {
    MemInfo::default()
}

/// What one more viewer of a `w`x`h` screen is expected to add to tilt's memory: about 26 MiB at
/// 1920x1080, scaled by area, plus 8 MiB.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "phase 0 stub: used by admission (G4)")
)]
pub fn viewer_bytes_estimate(w: u32, h: u32) -> u64 {
    const MIB: u64 = 1024 * 1024;
    26 * MIB * u64::from(w) * u64::from(h) / (1920 * 1080) + 8 * MIB
}

/// Returns memory freed inside tilt to the system (mi_collect).
#[expect(dead_code, reason = "phase 0 stub: called by capture (G2)")]
pub fn release_memory() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_estimate_scales_with_the_screen() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(viewer_bytes_estimate(1920, 1080), 34 * MIB);
        assert_eq!(viewer_bytes_estimate(0, 0), 8 * MIB);
        assert_eq!(viewer_bytes_estimate(960, 540), 8 * MIB + 26 * MIB / 4);
    }
}
