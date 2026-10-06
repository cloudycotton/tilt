//! Command line and environment configuration (brief section 3).

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::builder::{BoolishValueParser, RangedI64ValueParser, RangedU64ValueParser};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::flow::FlowConfig;
use crate::sysres::CpuCapacity;
use crate::video::encoder::{EncoderSettings, MAX_QP, MIN_CABAC_QP, MIN_QP};

// `tilt [serve] [OPTIONS]`: options may follow `serve` or stand alone, since serve is the default.
#[derive(Parser, Debug)]
#[command(name = "tilt", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    serve: Config,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Stream the X display (the default when no subcommand is given)
    Serve(Config),
}

/// H.264 profile of the encoded stream.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// High profile with CABAC entropy coding
    High,
    /// Constrained Baseline with CAVLC
    Baseline,
    /// Baseline on boxes with 2 CPUs or fewer, where CAVLC's lower cost matters most; else High
    Auto,
}

/// `--cpu-budget`: how much CPU tilt may use before the governor lowers the frame rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CpuBudgetArg {
    /// Half the box's CPUs, at least 0.35 of a core.
    Auto,
    /// No governor.
    Off,
    /// This many cores.
    Cores(f32),
}

/// How capture finds what changed on the screen.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageModeArg {
    /// Grab only the damaged rectangles, as DAMAGE reports them
    Delta,
    /// Grab the damaged region fetched from the server
    Fetch,
    /// Grab the whole screen on any damage
    Full,
}

/// Server settings. Every flag can also be set through its `TILT_*` environment variable.
#[derive(Args, Debug, Clone)]
pub struct Config {
    /// Address for HTTP and the /stream WebSocket
    #[arg(long, env = "TILT_BIND", default_value = "0.0.0.0:6090")]
    pub bind: SocketAddr,

    /// X display to capture [default: $DISPLAY, else :0]
    #[arg(long, env = "TILT_DISPLAY", default_value_t = default_display(), hide_default_value = true)]
    pub display: String,

    /// Token that grants the control role
    #[arg(long, env = "TILT_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    /// Token that grants the view-only role
    #[arg(long, env = "TILT_VIEW_TOKEN", hide_env_values = true)]
    pub view_token: Option<String>,

    /// File whose first non-empty line is the control token, re-read on every connection
    #[arg(long, env = "TILT_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,

    /// Let everyone in with the control role (local testing only)
    #[arg(long, env = "TILT_NO_AUTH", value_parser = BoolishValueParser::new())]
    pub no_auth: bool,

    /// With --no-auth, also let browser pages on these origins connect (comma-separated, such
    /// as https://desk.example.com; * for any)
    #[arg(
        long,
        env = "TILT_ALLOW_ORIGIN",
        value_delimiter = ',',
        value_name = "ORIGINS"
    )]
    pub allow_origin: Vec<String>,

    /// Frame rate cap per viewer (OpenH264 caps at 60)
    #[arg(long, env = "TILT_MAX_FPS", default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=60))]
    pub max_fps: u32,

    /// Starting video bitrate
    #[arg(long, env = "TILT_BITRATE_KBPS", default_value_t = 8000, value_parser = clap::value_parser!(u32).range(1..))]
    pub bitrate_kbps: u32,

    /// Lowest bitrate the congestion controller may choose
    #[arg(long, env = "TILT_MIN_BITRATE_KBPS", default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..))]
    pub min_bitrate_kbps: u32,

    /// Highest bitrate the congestion controller may choose
    #[arg(long, env = "TILT_MAX_BITRATE_KBPS", default_value_t = 20000, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_bitrate_kbps: u32,

    /// Lowest quantizer the encoder may use, 12 to 51; at least 20 with --profile high
    #[arg(long, env = "TILT_QP_MIN", default_value_t = 20, value_parser = qp_parser())]
    pub qp_min: u8,

    /// Highest quantizer the encoder may use, 12 to 51, not below --qp-min
    #[arg(long, env = "TILT_QP_MAX", default_value_t = 28, value_parser = qp_parser())]
    pub qp_max: u8,

    /// H.264 profile
    #[arg(long, env = "TILT_PROFILE", value_enum, default_value_t = Profile::High)]
    pub profile: Profile,

    /// Refinement frames encoded after the screen goes still
    #[arg(long, env = "TILT_TAIL_FRAMES", default_value_t = 30)]
    pub tail_frames: u32,

    /// End the refinement tail early once a tail frame is smaller than this many bytes, 0 = off
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 0 stub: read by the worker (G1 A1)")
    )]
    #[arg(long, env = "TILT_TAIL_STOP_BYTES", default_value_t = 128)]
    pub tail_stop_bytes: u32,

    /// Quantizer of the refresh frame sent once the screen goes still after motion, 0 = off
    #[arg(long, env = "TILT_REFRESH_QP", default_value_t = 20, value_parser = qp_or_off)]
    pub refresh_qp: u8,

    /// Send the refresh frame only when motion was coded above this quantizer
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 0 stub: read by the worker (G1 A1)")
    )]
    #[arg(long, env = "TILT_REFRESH_ABOVE_QP", default_value_t = 23, value_parser = qp_parser())]
    pub refresh_above_qp: u8,

    /// Let OpenH264's background detection skip blocks it judges unchanged
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "phase 0 stub: read by the encoder settings (G1 A1)"
        )
    )]
    #[arg(long, env = "TILT_BGD", value_parser = BoolishValueParser::new())]
    pub bgd: bool,

    /// CPU tilt may use before it lowers the frame rate: auto (half the CPUs, at least 0.35),
    /// off, or a number of cores
    #[arg(long, env = "TILT_CPU_BUDGET", default_value = "auto", value_parser = cpu_budget_parser)]
    pub cpu_budget: CpuBudgetArg,

    /// Lowest frame rate the CPU governor may choose (capped at --max-fps)
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 0 stub: read by the governor (G1 A2)")
    )]
    #[arg(long, env = "TILT_MIN_FPS", default_value_t = 15, value_parser = clap::value_parser!(u32).range(1..=60))]
    pub min_fps: u32,

    /// After input, the next frame is sent at once, whatever the governor's rate, for this long
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "phase 0 stub: read by the worker schedule (G1 A2)"
        )
    )]
    #[arg(long, env = "TILT_INPUT_BOOST_MS", default_value_t = 300)]
    pub input_boost_ms: u64,

    /// Cap the frame rate at 30 while moving content is coded above this quantizer, 0 = off
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 0 stub: read by the governor (G1 A6)")
    )]
    #[arg(long, env = "TILT_GOV_QP", default_value_t = 38, value_parser = qp_or_off)]
    pub gov_qp: u8,

    /// Let OpenH264's rate control skip frames
    #[arg(long, env = "TILT_RC_FRAME_SKIP", value_parser = BoolishValueParser::new())]
    pub rc_frame_skip: bool,

    /// Maximum simultaneous viewers
    #[arg(long, env = "TILT_MAX_VIEWERS", default_value_t = 4, value_parser = RangedU64ValueParser::<usize>::new().range(1..))]
    pub max_viewers: usize,

    /// Largest WebSocket message; bigger access units are split into fragments
    #[arg(long, env = "TILT_MAX_MSG_BYTES", default_value_t = 65_536, value_parser = RangedU64ValueParser::<usize>::new().range(1024..))]
    pub max_msg_bytes: usize,

    /// TCP_NOTSENT_LOWAT in bytes, 0 = off (Linux only)
    #[arg(long, env = "TILT_NOTSENT_LOWAT", default_value_t = 32_768)]
    pub notsent_lowat: u32,

    /// Safety full-frame compare interval while viewers wait, in ms, 0 = off
    #[arg(long, env = "TILT_POLL_MS", default_value_t = 1000)]
    pub poll_ms: u64,

    /// How capture reads damaged screen areas; full is the fallback
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 0 stub: read by capture (G2 B1)")
    )]
    #[arg(long, env = "TILT_DAMAGE_MODE", value_enum, default_value_t = DamageModeArg::Delta)]
    pub damage_mode: DamageModeArg,

    /// Check every damage-built frame against a full grab (slow; for tests)
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 0 stub: read by capture (G2 B1)")
    )]
    #[arg(long, env = "TILT_CAPTURE_VERIFY", hide = true, value_parser = BoolishValueParser::new())]
    pub capture_verify: bool,

    /// Refuse a viewer unless the memory limit leaves room for it plus this many MiB, 0 = off
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 0 stub: read by admission (G4 D2)")
    )]
    #[arg(long, env = "TILT_MEM_RESERVE_MB", default_value_t = 32)]
    pub mem_reserve_mb: u32,
}

impl Config {
    /// Parses the process arguments and environment. Prints help, the version, or a usage
    /// error and exits (code 2 on errors) when that is the outcome.
    pub fn load() -> Config {
        Self::try_load_from(std::env::args_os()).unwrap_or_else(|e| e.exit())
    }

    /// Like [`Config::load`] for an explicit argument list whose first item is the program name.
    pub fn try_load_from<I, T>(args: I) -> Result<Config, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let cli = Cli::try_parse_from(args)?;
        let cfg = match cli.command {
            Some(Command::Serve(cfg)) => cfg,
            None => cli.serve,
        };
        cfg.validate()
            .map_err(|(kind, msg)| Cli::command().error(kind, msg))?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), (ErrorKind, String)> {
        if !self.no_auth && self.token.is_none() && self.token_file.is_none() {
            return Err((
                ErrorKind::MissingRequiredArgument,
                "no control token configured: pass --token (TILT_TOKEN) or --token-file \
                 (TILT_TOKEN_FILE), or --no-auth for local testing"
                    .into(),
            ));
        }
        for (flag, token) in [("--token", &self.token), ("--view-token", &self.view_token)] {
            if token.as_deref().is_some_and(|t| t.trim().is_empty()) {
                return Err((ErrorKind::InvalidValue, format!("{flag} must not be empty")));
            }
        }
        if self.qp_min > self.qp_max {
            return Err((
                ErrorKind::ArgumentConflict,
                format!(
                    "--qp-min {} must not exceed --qp-max {}",
                    self.qp_min, self.qp_max
                ),
            ));
        }
        // Checked again when each encoder is created; here it fails at startup, not per viewer.
        // `auto` may resolve to High, so it is held to High's floor.
        let cabac = self.profile != Profile::Baseline;
        for (flag, qp) in [("--qp-min", self.qp_min), ("--refresh-qp", self.refresh_qp)] {
            if cabac && qp != 0 && qp < MIN_CABAC_QP {
                return Err((
                    ErrorKind::ArgumentConflict,
                    format!(
                        "{flag} {qp} is below {MIN_CABAC_QP}, the lowest that is safe with \
                         --profile high: OpenH264's CABAC writer can overrun its buffer below \
                         it. Use {flag} {MIN_CABAC_QP} or more, or --profile baseline"
                    ),
                ));
            }
        }
        if self.min_bitrate_kbps > self.bitrate_kbps || self.bitrate_kbps > self.max_bitrate_kbps {
            return Err((
                ErrorKind::ArgumentConflict,
                "--bitrate-kbps must lie within [--min-bitrate-kbps, --max-bitrate-kbps]".into(),
            ));
        }
        Ok(())
    }

    /// Minimum spacing between frames, 1 / max_fps.
    pub fn frame_interval(&self) -> Duration {
        Duration::from_secs(1) / self.max_fps
    }

    /// The safety-poll interval, or None when `--poll-ms 0` disables it.
    pub fn poll_interval(&self) -> Option<Duration> {
        (self.poll_ms > 0).then(|| Duration::from_millis(self.poll_ms))
    }

    /// `--profile`, with `auto` resolved for a box of `cap`: never Auto.
    pub fn resolved_profile(&self, cap: &CpuCapacity) -> Profile {
        match self.profile {
            Profile::Auto if cap.cpus <= 2.0 => Profile::Baseline,
            Profile::Auto => Profile::High,
            p => p,
        }
    }

    /// Whether `profile` (a resolved one) codes with CABAC.
    pub fn cabac_for(&self, profile: Profile) -> bool {
        profile == Profile::High
    }

    pub fn flow_config(&self) -> FlowConfig {
        FlowConfig {
            fps: self.max_fps,
            start_bps: kbps_to_bps(self.bitrate_kbps),
            min_bps: kbps_to_bps(self.min_bitrate_kbps),
            max_bps: kbps_to_bps(self.max_bitrate_kbps),
        }
    }

    /// Encoder settings for a `width`x`height` stream starting at `bitrate_bps`, in `profile`
    /// (see [`Config::resolved_profile`]).
    pub fn encoder_settings(
        &self,
        width: u32,
        height: u32,
        bitrate_bps: u32,
        profile: Profile,
    ) -> EncoderSettings {
        EncoderSettings {
            width,
            height,
            max_fps: self.max_fps as f32,
            bitrate_bps,
            min_qp: self.qp_min,
            max_qp: self.qp_max,
            cabac: self.cabac_for(profile),
            frame_skip: self.rc_frame_skip,
        }
    }
}

fn kbps_to_bps(kbps: u32) -> u32 {
    kbps.saturating_mul(1000)
}

/// QPs that OpenH264 honours under rate control: it quietly raises anything below MIN_QP.
fn qp_parser() -> RangedI64ValueParser<u8> {
    RangedI64ValueParser::<u8>::new().range(i64::from(MIN_QP)..=i64::from(MAX_QP))
}

/// A QP as [`qp_parser`] takes it, or 0 for off.
fn qp_or_off(s: &str) -> Result<u8, String> {
    match s.parse::<u8>() {
        Ok(qp) if qp == 0 || (MIN_QP..=MAX_QP).contains(&qp) => Ok(qp),
        _ => Err(format!("expected 0 (off) or a QP in {MIN_QP}..={MAX_QP}")),
    }
}

fn cpu_budget_parser(s: &str) -> Result<CpuBudgetArg, String> {
    match s {
        "auto" => Ok(CpuBudgetArg::Auto),
        "off" => Ok(CpuBudgetArg::Off),
        _ => match s.parse::<f32>() {
            Ok(cores) if cores.is_finite() && cores > 0.0 => Ok(CpuBudgetArg::Cores(cores)),
            _ => Err("expected auto, off or a number of cores above 0".to_owned()),
        },
    }
}

fn default_display() -> String {
    std::env::var("DISPLAY")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| ":0".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, clap::Error> {
        Config::try_load_from(std::iter::once("tilt").chain(args.iter().copied()))
    }

    #[test]
    fn defaults_match_the_brief() {
        let c = parse(&["--token", "t", "--display", ":5"]).unwrap();
        assert_eq!(c.bind, "0.0.0.0:6090".parse().unwrap());
        assert_eq!(c.display, ":5");
        assert_eq!(c.token.as_deref(), Some("t"));
        assert!(c.view_token.is_none() && c.token_file.is_none() && !c.no_auth);
        assert_eq!(
            (
                c.max_fps,
                c.bitrate_kbps,
                c.min_bitrate_kbps,
                c.max_bitrate_kbps
            ),
            (60, 8000, 1000, 20000)
        );
        assert_eq!((c.qp_min, c.qp_max, c.profile), (20, 28, Profile::High));
        assert_eq!(
            (c.tail_frames, c.rc_frame_skip, c.max_viewers),
            (30, false, 4)
        );
        assert_eq!(
            (c.max_msg_bytes, c.notsent_lowat, c.poll_ms),
            (65_536, 32_768, 1000)
        );
        assert_eq!(
            (c.tail_stop_bytes, c.refresh_qp, c.refresh_above_qp, c.bgd),
            (128, 20, 23, false)
        );
        assert_eq!(
            (c.cpu_budget, c.min_fps, c.input_boost_ms, c.gov_qp),
            (CpuBudgetArg::Auto, 15, 300, 38)
        );
        assert_eq!(
            (c.damage_mode, c.capture_verify, c.mem_reserve_mb),
            (DamageModeArg::Delta, false, 32)
        );
    }

    #[test]
    fn governor_and_capture_flags() {
        let c = parse(&[
            "--no-auth",
            "--cpu-budget",
            "0.75",
            "--min-fps",
            "10",
            "--input-boost-ms",
            "0",
            "--gov-qp",
            "0",
            "--refresh-qp",
            "0",
            "--refresh-above-qp",
            "30",
            "--tail-stop-bytes",
            "0",
            "--bgd",
            "--damage-mode",
            "full",
            "--capture-verify",
            "--mem-reserve-mb",
            "0",
        ])
        .unwrap();
        assert_eq!(
            (c.cpu_budget, c.min_fps, c.input_boost_ms, c.gov_qp),
            (CpuBudgetArg::Cores(0.75), 10, 0, 0)
        );
        assert_eq!(
            (c.refresh_qp, c.refresh_above_qp, c.tail_stop_bytes, c.bgd),
            (0, 30, 0, true)
        );
        assert_eq!(
            (c.damage_mode, c.capture_verify, c.mem_reserve_mb),
            (DamageModeArg::Full, true, 0)
        );
        let budget = |v: &str| parse(&["--no-auth", "--cpu-budget", v]).map(|c| c.cpu_budget);
        assert_eq!(budget("off").unwrap(), CpuBudgetArg::Off);
        assert_eq!(budget("auto").unwrap(), CpuBudgetArg::Auto);
        assert_eq!(budget("2").unwrap(), CpuBudgetArg::Cores(2.0));
        for bad in ["0", "-1", "inf", "NaN", "half", ""] {
            assert!(budget(bad).is_err(), "--cpu-budget {bad:?}");
        }
        let mode = |v: &str| parse(&["--no-auth", "--damage-mode", v]).map(|c| c.damage_mode);
        assert_eq!(mode("fetch").unwrap(), DamageModeArg::Fetch);
        assert!(mode("all").is_err());
    }

    #[test]
    fn auto_profile_picks_cavlc_on_small_boxes() {
        let cap = |cpus| CpuCapacity {
            cpus,
            quota_cpus: None,
            affinity: 8,
        };
        let c = parse(&["--no-auth", "--profile", "auto"]).unwrap();
        assert_eq!(c.resolved_profile(&cap(1.0)), Profile::Baseline);
        assert_eq!(c.resolved_profile(&cap(2.0)), Profile::Baseline);
        assert_eq!(c.resolved_profile(&cap(2.5)), Profile::High);
        for p in ["high", "baseline"] {
            let c = parse(&["--no-auth", "--qp-min", "20", "--profile", p]).unwrap();
            assert_eq!(c.resolved_profile(&cap(1.0)), c.profile, "{p} stays {p}");
        }
        assert!(c.cabac_for(Profile::High) && !c.cabac_for(Profile::Baseline));
        // Auto may resolve to High, so it is held to High's QP floor.
        assert!(parse(&["--no-auth", "--profile", "auto", "--qp-min", "12"]).is_err());
    }

    #[test]
    fn allowed_origins_are_a_comma_separated_list() {
        let c = parse(&["--no-auth"]).unwrap();
        assert!(c.allow_origin.is_empty());
        let c = parse(&[
            "--no-auth",
            "--allow-origin",
            "https://a.example,https://b.example:8443",
        ])
        .unwrap();
        assert_eq!(
            c.allow_origin,
            ["https://a.example", "https://b.example:8443"]
        );
    }

    #[test]
    fn serve_subcommand_takes_the_same_options() {
        let c = parse(&["serve", "--token", "t", "--max-fps", "30", "--no-auth"]).unwrap();
        assert_eq!(c.max_fps, 30);
        assert!(c.no_auth);
    }

    #[test]
    fn rejects_out_of_range_and_inconsistent_values() {
        for bad in [
            &["--max-fps", "0"][..],
            &["--max-fps", "61"],
            &["--qp-min", "0"],
            &["--qp-max", "52"],
            &["--qp-min", "30", "--qp-max", "28"],
            // Below OpenH264's floor, whatever the profile.
            &["--qp-min", "11", "--profile", "baseline"],
            &["--qp-min", "12", "--qp-max", "11", "--profile", "baseline"],
            // Below the CABAC floor with the High profile (the default).
            &["--qp-min", "19"],
            &["--qp-min", "12", "--profile", "high"],
            &["--refresh-qp", "19"],
            &[
                "--refresh-qp",
                "11",
                "--qp-min",
                "12",
                "--profile",
                "baseline",
            ],
            &["--refresh-qp", "52"],
            &["--gov-qp", "5"],
            &["--refresh-above-qp", "0"],
            &["--min-fps", "0"],
            &["--min-fps", "61"],
            &["--bitrate-kbps", "500"],
            &["--bitrate-kbps", "30000"],
            &["--max-viewers", "0"],
            &["--profile", "main"],
            &["--bind", "localhost"],
        ] {
            let args: Vec<&str> = ["--token", "t"].iter().chain(bad).copied().collect();
            let err = parse(&args).expect_err(&format!("{bad:?} should be rejected"));
            assert_eq!(err.exit_code(), 2);
        }
        assert!(parse(&["--token", " "]).is_err());
        assert!(parse(&["--token", "t", "--view-token", ""]).is_err());
    }

    #[test]
    fn qp_bounds_follow_the_profile() {
        let qp = |args: &[&str]| {
            let all: Vec<&str> = ["--token", "t"].iter().chain(args).copied().collect();
            parse(&all).map(|c| (c.qp_min, c.qp_max))
        };
        // CAVLC checks for room in its buffer, so Baseline may go down to OpenH264's floor.
        assert_eq!(
            qp(&["--qp-min", "12", "--profile", "baseline"]).unwrap(),
            (12, 28)
        );
        assert_eq!(qp(&["--qp-min", "20"]).unwrap(), (20, 28));
        assert_eq!(qp(&["--qp-min", "51", "--qp-max", "51"]).unwrap(), (51, 51));
        let err = qp(&["--qp-min", "15"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        let msg = err.to_string();
        assert!(
            msg.contains("--qp-min 15") && msg.contains("--profile baseline"),
            "{msg}"
        );
        let msg = qp(&["--qp-min", "5", "--profile", "baseline"])
            .unwrap_err()
            .to_string();
        assert!(msg.contains("12..=51"), "{msg}");
    }

    #[test]
    fn requires_some_form_of_auth() {
        // The environment may legitimately provide credentials; only assert when it does not.
        if ["TILT_TOKEN", "TILT_TOKEN_FILE", "TILT_NO_AUTH"]
            .iter()
            .any(|v| std::env::var_os(v).is_some())
        {
            return;
        }
        let err = parse(&[]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(parse(&["--view-token", "v"]).is_err());
        assert!(parse(&["--no-auth"]).is_ok());
        assert!(parse(&["--token-file", "/nonexistent/token"]).is_ok());
    }

    #[test]
    fn derived_settings() {
        let c = parse(&[
            "--token",
            "t",
            "--profile",
            "baseline",
            "--max-fps",
            "50",
            "--rc-frame-skip",
        ])
        .unwrap();
        assert_eq!(c.frame_interval(), Duration::from_millis(20));
        assert_eq!(c.poll_interval(), Some(Duration::from_secs(1)));
        assert_eq!(
            c.flow_config(),
            FlowConfig {
                fps: 50,
                start_bps: 8_000_000,
                min_bps: 1_000_000,
                max_bps: 20_000_000
            }
        );
        assert_eq!(
            c.encoder_settings(1024, 768, 5_000_000, c.profile),
            EncoderSettings {
                width: 1024,
                height: 768,
                max_fps: 50.0,
                bitrate_bps: 5_000_000,
                min_qp: 20,
                max_qp: 28,
                cabac: false,
                frame_skip: true,
            }
        );
        let c = parse(&["--token", "t", "--poll-ms", "0"]).unwrap();
        assert!(c.encoder_settings(64, 64, 1_000_000, c.profile).cabac);
        assert_eq!(c.poll_interval(), None);
    }
}
