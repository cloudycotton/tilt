//! Command line and environment configuration (brief section 3).

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::builder::{BoolishValueParser, RangedI64ValueParser, RangedU64ValueParser};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::flow::FlowConfig;
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
        let qp = self.qp_min;
        if cabac && qp < MIN_CABAC_QP {
            return Err((
                ErrorKind::ArgumentConflict,
                format!(
                    "--qp-min {qp} is below {MIN_CABAC_QP}, the lowest that is safe with \
                     --profile high: OpenH264's CABAC writer can overrun its buffer below \
                     it. Use --qp-min {MIN_CABAC_QP} or more, or --profile baseline"
                ),
            ));
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

    /// `--profile`, with `auto` resolved for a box of `cpus` CPUs: never Auto.
    pub fn resolved_profile(&self, cpus: usize) -> Profile {
        match self.profile {
            Profile::Auto if cpus <= 2 => Profile::Baseline,
            Profile::Auto => Profile::High,
            p => p,
        }
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
            cabac: profile == Profile::High,
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

/// The CPUs this process may run on (its affinity mask), at least 1.
pub fn available_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
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
    }

    #[test]
    fn auto_profile_picks_cavlc_on_small_boxes() {
        let c = parse(&["--no-auth", "--profile", "auto"]).unwrap();
        assert_eq!(c.resolved_profile(1), Profile::Baseline);
        assert_eq!(c.resolved_profile(2), Profile::Baseline);
        assert_eq!(c.resolved_profile(3), Profile::High);
        for p in ["high", "baseline"] {
            let c = parse(&["--no-auth", "--qp-min", "20", "--profile", p]).unwrap();
            assert_eq!(c.resolved_profile(1), c.profile, "{p} stays {p}");
        }
        assert!(c.encoder_settings(64, 64, 1_000_000, Profile::High).cabac);
        assert!(
            !c.encoder_settings(64, 64, 1_000_000, Profile::Baseline)
                .cabac
        );
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
            // Flags of features that were never built are gone.
            &["--cpu-budget", "1"],
            &["--tail-stop-bytes", "0"],
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
