//! The CPU-budget governor: one per server, shared by every encoder worker, it lowers the frame
//! rate for new content while tilt uses more than `--cpu-budget` of the box.
//!
//! Phase 0: the interface the workers, the server and main code against. It never lowers the
//! frame rate: every interval is `1 / --max-fps` and tail frames are always allowed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{Config, CpuBudgetArg};
use crate::sysres::CpuCapacity;

/// Why the frame rate cap is what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GovReason {
    /// Not lowered: the cap is `--max-fps`.
    None,
    /// Lowered because tilt or the box uses too much CPU.
    #[expect(dead_code, reason = "phase 0 stub: set by the governor rules (G1 A2)")]
    Cpu,
    /// Lowered because the encoder's quantizer stays high (`--gov-qp`).
    #[expect(dead_code, reason = "phase 0 stub: set by the QP step (G1 A6)")]
    Qp,
    /// `--cpu-budget off`.
    Off,
}

/// The governor's state, for stats and /api/status.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct GovSnapshot {
    /// The frame rate cap in force for new content.
    pub fps_cap: u32,
    pub reason: GovReason,
    /// The box's CPU capacity in cores.
    pub cpus: f32,
    /// tilt's budget in cores; 0 with `--cpu-budget off`.
    pub budget_cores: f32,
    /// tilt's use over the last window, in cores.
    pub tilt_cores: f32,
    /// The box's use over the last window, in cores.
    pub box_cores: f32,
    /// The throttled share of CFS periods over the last window.
    pub throttled_share: f32,
}

pub struct Governor {
    cap: CpuCapacity,
    max_fps: u32,
    budget: Option<f32>,
    frame_interval: Duration,
}

impl Governor {
    pub fn new(cfg: &Config, cap: CpuCapacity) -> Arc<Governor> {
        let budget = match cfg.cpu_budget {
            CpuBudgetArg::Auto => Some((0.5 * cap.cpus).max(0.35)),
            CpuBudgetArg::Off => None,
            CpuBudgetArg::Cores(cores) => Some(cores),
        };
        Arc::new(Governor {
            cap,
            max_fps: cfg.max_fps,
            budget,
            frame_interval: cfg.frame_interval(),
        })
    }

    /// The CPU capacity the governor was built with (for `Config::resolved_profile`).
    pub fn capacity(&self) -> CpuCapacity {
        self.cap
    }

    /// The minimum spacing between new-content frames now; never below `1 / --max-fps`.
    #[expect(
        dead_code,
        reason = "phase 0 stub: used by the worker schedule (G1 A2)"
    )]
    pub fn frame_interval(&self, _now: Instant) -> Duration {
        self.frame_interval
    }

    /// Records one encode: its wall time, whether it was new content (not tail or refresh) and
    /// its average QP.
    #[expect(dead_code, reason = "phase 0 stub: called by the worker (G1 A2)")]
    pub fn note_encode(&self, _encode_us: u32, _new_content: bool, _avg_qp: u8, _now: Instant) {}

    /// False while tilt is over its budget: refinement tail frames are skipped then.
    #[expect(
        dead_code,
        reason = "phase 0 stub: used by the worker schedule (G1 A2)"
    )]
    pub fn tail_allowed(&self) -> bool {
        true
    }

    pub fn snapshot(&self) -> GovSnapshot {
        GovSnapshot {
            fps_cap: self.max_fps,
            reason: if self.budget.is_some() {
                GovReason::None
            } else {
                GovReason::Off
            },
            cpus: self.cap.cpus,
            budget_cores: self.budget.unwrap_or(0.0),
            tilt_cores: 0.0,
            box_cores: 0.0,
            throttled_share: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn governor(args: &[&str], cpus: f32) -> Arc<Governor> {
        let cfg = Config::try_load_from(["tilt", "--no-auth"].iter().chain(args).copied()).unwrap();
        let cap = CpuCapacity {
            cpus,
            quota_cpus: None,
            affinity: 4,
        };
        Governor::new(&cfg, cap)
    }

    #[test]
    fn budget_follows_the_flag_and_the_box() {
        let budget = |args: &[&str], cpus| governor(args, cpus).snapshot().budget_cores;
        assert_eq!(budget(&[], 4.0), 2.0);
        assert_eq!(budget(&[], 1.0), 0.5);
        assert_eq!(budget(&[], 0.5), 0.35, "never below 0.35 of a core");
        assert_eq!(budget(&["--cpu-budget", "0.8"], 1.0), 0.8);
        assert_eq!(budget(&["--cpu-budget", "off"], 1.0), 0.0);
    }

    #[test]
    fn snapshot_reports_the_max_fps_cap() {
        let s = governor(&["--max-fps", "30"], 2.0).snapshot();
        assert_eq!((s.fps_cap, s.reason, s.cpus), (30, GovReason::None, 2.0));
        let s = governor(&["--cpu-budget", "off"], 2.0).snapshot();
        assert_eq!((s.fps_cap, s.reason), (60, GovReason::Off));
        assert_eq!(
            serde_json::to_value(s.reason).unwrap(),
            serde_json::json!("off")
        );
    }

    #[test]
    fn governor_is_shared_across_threads() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Governor>();
    }
}
