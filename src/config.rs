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
    /// Address for HTTP (TCP) and the stream (WebTransport, UDP, same port)
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

    /// UDP port tilt listens on for the stream (WebTransport) [default: the --bind port]
    #[arg(long, env = "TILT_UDP_PORT", value_name = "PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub udp_port: Option<u16>,

    /// Where browsers send the stream's UDP, as PORT or HOST:PORT, when that is not the page's
    /// host and --udp-port: a NAT that forwards another port, or a sandbox's public UDP
    /// listener [default: the page's host and --udp-port]
    #[arg(long, env = "TILT_UDP_PUBLIC", value_name = "[HOST:]PORT", value_parser = parse_udp_public)]
    pub udp_public: Option<UdpPublic>,

    /// Let control viewers give the screen their window's shape, at least MIN (WxH) on each
    /// side. Xvfb resizes only within its startup screen, so start it large
    #[arg(long, env = "TILT_RESIZE", value_name = "MIN", value_parser = parse_size)]
    pub resize: Option<(u32, u32)>,

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

    /// No --token, --token-file or --no-auth: tilt keeps its own token (auth::kept_token).
    pub fn uses_kept_token(&self) -> bool {
        !self.no_auth && self.token.is_none() && self.token_file.is_none()
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

/// `--udp-public`: the host browsers dial (None: the page's own), and the port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpPublic {
    pub host: Option<String>,
    pub port: u16,
}

fn parse_udp_public(s: &str) -> Result<UdpPublic, String> {
    let (host, port) = match s.rsplit_once(':') {
        // An IPv6 address alone has colons but no port after a `]`.
        Some((host, port)) if !host.contains(':') || host.ends_with(']') => {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            (Some(host.to_owned()), port)
        }
        _ => (None, s),
    };
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|&p| p > 0)
        .ok_or_else(|| format!("{s:?} is not PORT or HOST:PORT"))?;
    if host
        .as_ref()
        .is_some_and(|h| h.is_empty() || h.contains(['/', ' ', '#', '?', '@']))
    {
        return Err(format!("{s:?} is not PORT or HOST:PORT"));
    }
    Ok(UdpPublic { host, port })
}

/// `WxH`, each side 16 to 4096.
fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let side = |v: &str| v.parse::<u32>().ok().filter(|v| (16..=4096).contains(v));
    match s.split_once(['x', 'X']) {
        Some((w, h)) => side(w).zip(side(h)),
        None => None,
    }
    .ok_or_else(|| format!("{s:?} is not WxH with sides from 16 to 4096"))
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
        assert_eq!(c.poll_ms, 1000);
    }

    #[test]
    fn resize_takes_a_minimum_size() {
        assert_eq!(parse(&["--no-auth"]).unwrap().resize, None);
        let c = parse(&["--no-auth", "--resize", "1024x720"]).unwrap();
        assert_eq!(c.resize, Some((1024, 720)));
        for bad in ["1024", "0x720", "1024x", "8x8", "5000x720", "axb"] {
            assert!(parse(&["--no-auth", "--resize", bad]).is_err(), "{bad}");
        }
    }

    #[test]
    fn udp_port_and_public_address() {
        let c = parse(&["--no-auth"]).unwrap();
        assert_eq!((c.udp_port, c.udp_public), (None, None));
        let c = parse(&["--no-auth", "--udp-port", "6091"]).unwrap();
        assert_eq!(c.udp_port, Some(6091));
        assert!(parse(&["--no-auth", "--udp-port", "0"]).is_err());
        let public =
            |s: &str| parse(&["--no-auth", "--udp-public", s]).map(|c| c.udp_public.unwrap());
        let up = |host: Option<&str>, port| UdpPublic {
            host: host.map(String::from),
            port,
        };
        assert_eq!(public("7090").unwrap(), up(None, 7090));
        assert_eq!(
            public("udp-3f2a.sail.box:41234").unwrap(),
            up(Some("udp-3f2a.sail.box"), 41234)
        );
        assert_eq!(
            public("203.0.113.7:9033").unwrap(),
            up(Some("203.0.113.7"), 9033)
        );
        assert_eq!(
            public("[2001:db8::1]:9033").unwrap(),
            up(Some("2001:db8::1"), 9033)
        );
        for bad in [
            "0",
            "host:",
            ":9033",
            "host:99999",
            "a/b:9033",
            "2001:db8::1",
            "x",
        ] {
            assert!(public(bad).is_err(), "{bad}");
        }
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
    fn without_auth_flags_tilt_keeps_its_own_token() {
        // The environment may provide credentials; only assert when it does not.
        if ["TILT_TOKEN", "TILT_TOKEN_FILE", "TILT_NO_AUTH"]
            .iter()
            .any(|v| std::env::var_os(v).is_some())
        {
            return;
        }
        assert!(parse(&[]).unwrap().uses_kept_token());
        assert!(parse(&["--view-token", "v"]).unwrap().uses_kept_token());
        assert!(!parse(&["--no-auth"]).unwrap().uses_kept_token());
        assert!(!parse(&["--token-file", "/nonexistent/token"])
            .unwrap()
            .uses_kept_token());
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
