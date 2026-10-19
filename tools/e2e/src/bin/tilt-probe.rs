//! tilt-probe: a headless tilt client for end-to-end checks (brief section 7). It decodes the
//! stream with OpenH264, times key-to-decoded latency against tilt-testpattern's marker,
//! measures the frame rate, and exits non-zero when a threshold is missed.
//!
//! Exit codes: 0 all checks passed, 1 the probe itself failed (connect, auth, no marker),
//! 3 the run completed but a check or threshold failed. `--json` prints a report either way.

use std::process::ExitCode;
use std::sync::{mpsc as std_mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use clap::Parser;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use serde::Serialize;
use serde_json::{json, Value};
use tilt_e2e::{nearest_color, yuv_to_rgb, COLORS};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, sleep_until};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

#[derive(Parser, Debug)]
#[command(
    name = "tilt-probe",
    version,
    about = "Headless tilt client: decodes the stream and measures input latency and frame rate"
)]
struct Args {
    /// tilt stream URL
    #[arg(long, default_value = "ws://127.0.0.1:6090/stream")]
    url: String,

    /// Control token sent in hello
    #[arg(long, env = "TILT_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Length of the frame-rate phase, in seconds (0 skips it)
    #[arg(long, default_value_t = 5.0, value_parser = parse_seconds)]
    duration_s: f64,

    /// Number of key-to-decoded latency trials
    #[arg(long, default_value_t = 30)]
    latency_trials: u32,

    /// Screen pixel sampled for the testpattern marker colour
    #[arg(long, default_value = "192,192", value_parser = parse_point)]
    marker: (u32, u32),

    /// Screen pixel clicked (MOVE + button 1) by the click check
    #[arg(long, default_value = "100,100", value_parser = parse_point)]
    click: (u32, u32),

    /// Skip the click check
    #[arg(long)]
    no_click: bool,

    /// After the input phases, wait for the refinement tail to end, then count VIDEO access
    /// units for this many seconds (0 skips it)
    #[arg(long, default_value_t = 0.0, value_parser = parse_seconds)]
    idle_s: f64,

    /// Print the report as one JSON object
    #[arg(long)]
    json: bool,

    /// Exit non-zero when the measured fps is lower
    #[arg(long)]
    min_fps: Option<f64>,

    /// Exit non-zero when the p95 key-to-decoded latency is higher, in ms
    #[arg(long)]
    max_p95_ms: Option<f64>,

    /// Exit non-zero when the p50 key-to-decoded latency is higher, in ms
    #[arg(long)]
    max_p50_ms: Option<f64>,

    /// Exit non-zero when the idle phase sees more VIDEO access units
    #[arg(long)]
    max_idle_frames: Option<u64>,
}

/// The `--json` output. Totals cover the whole run; fps, kbps and gaps the frame-rate phase.
#[derive(Debug, Default, Serialize)]
struct Report {
    frames: u64,
    keyframes: u64,
    decode_errors: u64,
    fps: f64,
    kbps: f64,
    latency_ms: Latency,
    gaps_ms: Gaps,
    screen: Screen,
    access_units: u64,
    protocol_errors: u64,
    /// Median PING round trip.
    rtt_ms: f64,
    click: Option<Click>,
    idle: Option<Idle>,
    /// The last `stats` message from the server.
    server_stats: Option<Value>,
    ok: bool,
    failures: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
struct Latency {
    p50: f64,
    p95: f64,
    max: f64,
    n: usize,
    /// Trials that saw no marker change within the timeout.
    failed: u32,
}

#[derive(Debug, Default, Serialize)]
struct Gaps {
    p50: f64,
    p95: f64,
    max: f64,
}

#[derive(Debug, Default, Serialize)]
struct Screen {
    w: u32,
    h: u32,
}

#[derive(Debug, Serialize)]
struct Click {
    x: u32,
    y: u32,
    ok: bool,
    ms: Option<f64>,
}

#[derive(Debug, Serialize)]
struct Idle {
    seconds: f64,
    /// VIDEO access units received during the idle window.
    frames: u64,
    /// False when the stream never went quiet before the window started.
    settled: bool,
}

/// The space key: the trials press it, and the pattern advances on every key press.
const XK_SPACE: u32 = 0x20;
/// Bound on the handshake, taking control and the first marker sighting.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one trial (or the click) may take to show the next marker colour.
const TRIAL_TIMEOUT: Duration = Duration::from_secs(2);
const TRIAL_PAUSE: Duration = Duration::from_millis(100);
/// Give up on the trials after this many timeouts in a row: the marker is not reacting.
const MAX_CONSECUTIVE_TIMEOUTS: u32 = 3;
/// Silence that marks the end of the server's refinement tail.
const TAIL_QUIET: Duration = Duration::from_secs(1);
const TAIL_WAIT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(1);
const IDR_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

fn parse_point(s: &str) -> Result<(u32, u32), String> {
    let (x, y) = s.split_once(',').ok_or("expected X,Y")?;
    let coord = |v: &str| v.trim().parse::<u32>().map_err(|e| format!("{v:?}: {e}"));
    Ok((coord(x)?, coord(y)?))
}

fn parse_seconds(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => Ok(v),
        _ => Err(format!("{s:?} is not a non-negative number of seconds")),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args).await {
        Ok(report) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("report serializes")
                );
            } else {
                print_summary(&report);
            }
            if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(3)
            }
        }
        Err(e) => {
            if args.json {
                println!("{}", json!({ "ok": false, "error": format!("{e:#}") }));
            }
            eprintln!("tilt-probe: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &Args) -> anyhow::Result<Report> {
    let mut session = Session::connect(args).await?;
    let role = session.handshake(args.token.as_deref()).await?;
    let (w, h) = session.screen();
    eprintln!(
        "tilt-probe: connected to {} as {role}, screen {w}x{h}",
        args.url
    );

    let wants_input = args.latency_trials > 0 || !args.no_click;
    if wants_input {
        if role != "control" {
            bail!(
                "the token has the {role} role; trials and the click need control \
                 (use --latency-trials 0 --no-click)"
            );
        }
        for (name, (x, y)) in [("marker", args.marker), ("click", args.click)] {
            if x >= w || y >= h {
                bail!("--{name} {x},{y} is outside the {w}x{h} screen");
            }
        }
        session.take_control().await?;
    }
    // Measure from the first picture on, not from the connect.
    let deadline = Instant::now() + SETUP_TIMEOUT;
    let first = if wants_input {
        session.wait_sample(deadline, |s| s.color.is_some()).await?
    } else {
        session.wait_sample(deadline, |_| true).await?
    };
    if first.is_none() {
        let (x, y) = args.marker;
        if wants_input {
            bail!(
                "no decoded frame showed a marker colour at {x},{y} within {SETUP_TIMEOUT:?}; \
                 is tilt-testpattern running?"
            );
        }
        bail!("no frame decoded within {SETUP_TIMEOUT:?}");
    }

    let mut report = Report::default();
    if args.duration_s > 0.0 {
        let window = session
            .observe(Duration::from_secs_f64(args.duration_s))
            .await?;
        report.fps = round2(window.fps);
        report.kbps = round1(window.kbps);
        report.gaps_ms = Gaps {
            p50: round1(percentile(&window.gaps_ms, 50.0)),
            p95: round1(percentile(&window.gaps_ms, 95.0)),
            max: round1(window.gaps_ms.last().copied().unwrap_or(0.0)),
        };
    }
    if args.latency_trials > 0 {
        let (mut latencies, failed) = latency_trials(&mut session, args.latency_trials).await?;
        latencies.sort_by(f64::total_cmp);
        report.latency_ms = Latency {
            p50: round1(percentile(&latencies, 50.0)),
            p95: round1(percentile(&latencies, 95.0)),
            max: round1(latencies.last().copied().unwrap_or(0.0)),
            n: latencies.len(),
            failed,
        };
    }
    if !args.no_click {
        report.click = Some(click(&mut session, args.click).await?);
    }
    if args.idle_s > 0.0 {
        report.idle = Some(session.idle(args.idle_s).await?);
    }
    session.close().await;

    let stats = session.lock();
    report.frames = stats.decoded.len() as u64;
    report.keyframes = stats.keyframes;
    report.decode_errors = stats.decode_errors;
    report.access_units = stats.units.len() as u64;
    report.protocol_errors = stats.protocol_errors;
    let mut rtts = stats.rtt_ms.clone();
    rtts.sort_by(f64::total_cmp);
    report.rtt_ms = round1(percentile(&rtts, 50.0));
    report.server_stats = stats.server_stats.clone();
    report.screen = Screen {
        w: stats.screen.0,
        h: stats.screen.1,
    };
    drop(stats);
    report.failures = failures(args, &report);
    report.ok = report.failures.is_empty();
    Ok(report)
}

/// The checks behind exit code 3, as "what: measured" lines.
fn failures(args: &Args, r: &Report) -> Vec<String> {
    let mut out = Vec::new();
    if r.decode_errors > 0 {
        out.push(format!("decode errors: {}", r.decode_errors));
    }
    if r.protocol_errors > 0 {
        out.push(format!(
            "malformed or incomplete server messages: {}",
            r.protocol_errors
        ));
    }
    if r.latency_ms.failed > 0 {
        out.push(format!(
            "latency trials without a marker change: {}",
            r.latency_ms.failed
        ));
    }
    if let Some(min) = args.min_fps {
        if r.fps < min {
            out.push(format!("fps: {} < {min}", r.fps));
        }
    }
    for (name, limit, value) in [
        ("p50", args.max_p50_ms, r.latency_ms.p50),
        ("p95", args.max_p95_ms, r.latency_ms.p95),
    ] {
        match limit {
            Some(_) if r.latency_ms.n == 0 => out.push(format!("latency {name}: no samples")),
            Some(max) if value > max => out.push(format!("latency {name}: {value} ms > {max} ms")),
            _ => {}
        }
    }
    if let Some(c) = r.click.as_ref().filter(|c| !c.ok) {
        out.push(format!("click at {},{}: marker did not advance", c.x, c.y));
    }
    if let (Some(max), Some(idle)) = (args.max_idle_frames, &r.idle) {
        if idle.frames > max {
            out.push(format!("idle access units: {} > {max}", idle.frames));
        }
    }
    out
}

fn print_summary(r: &Report) {
    println!("screen     {}x{}", r.screen.w, r.screen.h);
    println!(
        "frames     {} decoded, {} keyframes, {} decode errors, {} access units",
        r.frames, r.keyframes, r.decode_errors, r.access_units
    );
    println!(
        "fps        {} ({} kbps), gaps p50 {} p95 {} max {} ms",
        r.fps, r.kbps, r.gaps_ms.p50, r.gaps_ms.p95, r.gaps_ms.max
    );
    let l = &r.latency_ms;
    println!(
        "latency    p50 {} p95 {} max {} ms ({} trials, {} failed)",
        l.p50, l.p95, l.max, l.n, l.failed
    );
    if let Some(c) = &r.click {
        let ms = c.ms.map_or("-".into(), |ms| format!("{ms} ms"));
        println!(
            "click      {},{} {} ({ms})",
            c.x,
            c.y,
            if c.ok { "ok" } else { "FAILED" }
        );
    }
    if let Some(i) = &r.idle {
        let note = if i.settled { "" } else { " (never went quiet)" };
        println!(
            "idle       {} access units in {} s{note}",
            i.frames, i.seconds
        );
    }
    println!("rtt        {} ms", r.rtt_ms);
    for f in &r.failures {
        println!("FAIL       {f}");
    }
}

/// Sends KEY down/up for space and times until the decoded marker shows the next colour.
/// Returns the latencies in ms and the number of trials that timed out.
async fn latency_trials(session: &mut Session, trials: u32) -> anyhow::Result<(Vec<f64>, u32)> {
    let mut latencies = Vec::new();
    let (mut failed, mut in_a_row) = (0, 0);
    for _ in 0..trials {
        let seen = match session.color() {
            Some(current) => {
                let want = (current + 1) % COLORS.len();
                let sent = Instant::now();
                session.send(wire::key(true, XK_SPACE));
                session.send(wire::key(false, XK_SPACE));
                let pred = |s: &Sample| s.at >= sent && s.color == Some(want);
                session
                    .wait_sample(sent + TRIAL_TIMEOUT, pred)
                    .await?
                    .map(|s| s.at - sent)
            }
            // Something covers the marker: wait as long as a trial would, it may clear.
            None => {
                sleep(TRIAL_TIMEOUT).await;
                None
            }
        };
        match seen {
            Some(latency) => {
                latencies.push(ms(latency));
                in_a_row = 0;
            }
            None => {
                failed += 1;
                in_a_row += 1;
                if in_a_row == MAX_CONSECUTIVE_TIMEOUTS {
                    eprintln!(
                        "tilt-probe: {in_a_row} trials in a row saw no marker change; stopping"
                    );
                    break;
                }
            }
        }
        sleep(TRIAL_PAUSE).await;
    }
    Ok((latencies, failed))
}

/// Moves to `(x, y)` and clicks button 1 there; the pattern advances its marker on the press.
async fn click(session: &mut Session, (x, y): (u32, u32)) -> anyhow::Result<Click> {
    let (w, h) = session.screen();
    let (nx, ny) = (wire::normalize(x, w), wire::normalize(y, h));
    let want = session.color().map(|c| (c + 1) % COLORS.len());
    let sent = Instant::now();
    session.send(wire::pointer_move(nx, ny));
    session.send(wire::button(1, true, nx, ny));
    session.send(wire::button(1, false, nx, ny));
    let seen = match want {
        Some(want) => {
            let pred = |s: &Sample| s.at >= sent && s.color == Some(want);
            session.wait_sample(sent + TRIAL_TIMEOUT, pred).await?
        }
        None => None,
    };
    Ok(Click {
        x,
        y,
        ok: seen.is_some(),
        ms: seen.map(|s| round1(ms(s.at - sent))),
    })
}

/// The newest decoded picture's marker colour (index into COLORS; None if no colour matched).
#[derive(Clone, Copy, Debug)]
struct Sample {
    color: Option<usize>,
    /// When the picture came out of the decoder.
    at: Instant,
}

/// Written by the reader task and the decode thread, read by the phases.
#[derive(Default)]
struct Stats {
    /// Arrival (last fragment) and wire size of every VIDEO access unit.
    units: Vec<(Instant, usize)>,
    keyframes: u64,
    /// Output time of every decoded picture.
    decoded: Vec<Instant>,
    decode_errors: u64,
    protocol_errors: u64,
    rtt_ms: Vec<f64>,
    server_stats: Option<Value>,
    screen: (u32, u32),
}

/// One complete access unit on its way to the decode thread.
struct Unit {
    seq: u32,
    key: bool,
    data: Vec<u8>,
    received: Instant,
}

/// What the frame-rate phase measured.
struct Window {
    fps: f64,
    kbps: f64,
    /// Sorted inter-frame gaps between decoded pictures.
    gaps_ms: Vec<f64>,
}

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A connected probe: a reader task, a writer task, a pinger and a decode thread around one
/// WebSocket.
struct Session {
    out: mpsc::UnboundedSender<Message>,
    /// Every JSON message from the server, in order.
    events: mpsc::UnboundedReceiver<Value>,
    samples: watch::Receiver<Option<Sample>>,
    /// Why the connection ended, once it has.
    closed: watch::Receiver<Option<String>>,
    stats: Arc<Mutex<Stats>>,
}

impl Session {
    async fn connect(args: &Args) -> anyhow::Result<Session> {
        // No Nagle: input messages are tiny and latency is what is being measured.
        // tungstenite's error already prints its IO cause, so it is not kept as a source too.
        let (ws, _) = tokio_tungstenite::connect_async_with_config(args.url.as_str(), None, true)
            .await
            .map_err(|e| anyhow!("connecting to {}: {e}", args.url))?;
        let (sink, stream) = ws.split();
        let start = Instant::now();
        let stats = Arc::new(Mutex::new(Stats::default()));
        let (out, out_rx) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (samples_tx, samples) = watch::channel(None);
        let (closed_tx, closed) = watch::channel(None);
        let (units_tx, units_rx) = std_mpsc::channel();

        tokio::spawn(write_loop(sink, out_rx));
        tokio::spawn(read_loop(
            stream,
            units_tx,
            events_tx,
            stats.clone(),
            closed_tx,
            start,
        ));
        let pinger = out.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PING_INTERVAL);
            loop {
                tick.tick().await;
                if pinger
                    .send(Message::binary(wire::ping(millis_since(start))))
                    .is_err()
                {
                    break;
                }
            }
        });
        let (decoder_out, decoder_stats, marker) = (out.clone(), stats.clone(), args.marker);
        thread::Builder::new()
            .name("decode".into())
            .spawn(move || decode_loop(units_rx, decoder_out, decoder_stats, samples_tx, marker))?;

        Ok(Session {
            out,
            events,
            samples,
            closed,
            stats,
        })
    }

    /// Sends hello and waits for welcome; returns the role.
    async fn handshake(&mut self, token: Option<&str>) -> anyhow::Result<String> {
        let mut hello = json!({
            "t": "hello",
            "v": 1,
            "client": concat!("tilt-probe/", env!("CARGO_PKG_VERSION")),
        });
        if let Some(token) = token {
            hello["token"] = token.into();
        }
        self.send_json(&hello);
        let deadline = Instant::now() + SETUP_TIMEOUT;
        loop {
            let ev = self.next_event(deadline, "welcome").await?;
            match ev["t"].as_str() {
                Some("welcome") => {
                    let dim =
                        |k: &str| ev["screen"][k].as_u64().and_then(|v| u32::try_from(v).ok());
                    let (Some(w), Some(h)) = (dim("w"), dim("h")) else {
                        bail!("welcome without a screen size: {ev}");
                    };
                    self.lock().screen = (w, h);
                    return Ok(ev["role"].as_str().unwrap_or("?").to_owned());
                }
                Some("error") => bail!("server refused the connection: {ev}"),
                _ => {}
            }
        }
    }

    async fn take_control(&mut self) -> anyhow::Result<()> {
        self.send_json(&json!({ "t": "control", "take": true }));
        let deadline = Instant::now() + SETUP_TIMEOUT;
        loop {
            let ev = self.next_event(deadline, "control").await?;
            match ev["t"].as_str() {
                Some("control") if ev["you"] == true => return Ok(()),
                Some("error") => bail!("taking control failed: {ev}"),
                _ => {}
            }
        }
    }

    /// Counts decoded pictures and received bytes for `length`.
    async fn observe(&mut self, length: Duration) -> anyhow::Result<Window> {
        let from = Instant::now();
        sleep(length).await;
        self.check_open()?;
        let to = Instant::now();
        let secs = (to - from).as_secs_f64();
        let stats = self.lock();
        let inside = |t: &Instant| (from..to).contains(t);
        let decoded: Vec<Instant> = stats.decoded.iter().copied().filter(inside).collect();
        let bytes: usize = stats
            .units
            .iter()
            .filter(|(t, _)| inside(t))
            .map(|(_, n)| n)
            .sum();
        let mut gaps_ms: Vec<f64> = decoded.windows(2).map(|w| ms(w[1] - w[0])).collect();
        gaps_ms.sort_by(f64::total_cmp);
        Ok(Window {
            fps: decoded.len() as f64 / secs,
            kbps: bytes as f64 * 8.0 / 1000.0 / secs,
            gaps_ms,
        })
    }

    /// Waits for the refinement tail to end, then counts access units for `seconds`.
    async fn idle(&mut self, seconds: f64) -> anyhow::Result<Idle> {
        let give_up = Instant::now() + TAIL_WAIT;
        let settled = loop {
            let last = self.lock().units.last().map(|(t, _)| *t);
            if last.is_none_or(|t| t.elapsed() >= TAIL_QUIET) {
                break true;
            }
            if Instant::now() >= give_up {
                break false;
            }
            sleep(Duration::from_millis(50)).await;
        };
        let from = Instant::now();
        sleep(Duration::from_secs_f64(seconds)).await;
        self.check_open()?;
        let frames = self.lock().units.iter().filter(|(t, _)| *t >= from).count() as u64;
        Ok(Idle {
            seconds,
            frames,
            settled,
        })
    }

    /// Closes with 1000 and gives the server a moment to answer.
    async fn close(&mut self) {
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "probe done".into(),
        };
        if self.out.send(Message::Close(Some(frame))).is_ok() {
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                self.closed.wait_for(Option::is_some),
            )
            .await;
        }
    }

    fn send(&self, msg: Vec<u8>) {
        // A send after the connection ended is noticed by the waits that follow it.
        let _ = self.out.send(Message::binary(msg));
    }

    fn send_json(&self, v: &Value) {
        let _ = self.out.send(Message::text(v.to_string()));
    }

    fn lock(&self) -> MutexGuard<'_, Stats> {
        lock(&self.stats)
    }

    fn screen(&self) -> (u32, u32) {
        self.lock().screen
    }

    fn color(&self) -> Option<usize> {
        self.samples.borrow().and_then(|s| s.color)
    }

    fn close_reason(&self) -> String {
        self.closed
            .borrow()
            .clone()
            .unwrap_or_else(|| "connection lost".into())
    }

    fn check_open(&self) -> anyhow::Result<()> {
        match self.closed.borrow().as_ref() {
            Some(reason) => Err(anyhow!("connection ended: {reason}")),
            None => Ok(()),
        }
    }

    async fn next_event(&mut self, deadline: Instant, what: &str) -> anyhow::Result<Value> {
        let ev = tokio::select! {
            ev = self.events.recv() => ev,
            () = sleep_until(deadline.into()) => bail!("timed out waiting for {what}"),
        };
        ev.ok_or_else(|| {
            anyhow!(
                "connection ended while waiting for {what}: {}",
                self.close_reason()
            )
        })
    }

    /// Waits until the newest decoded picture satisfies `pred`; None at the deadline.
    async fn wait_sample(
        &mut self,
        deadline: Instant,
        pred: impl Fn(&Sample) -> bool,
    ) -> anyhow::Result<Option<Sample>> {
        loop {
            let newest = *self.samples.borrow_and_update();
            if let Some(s) = newest.filter(|s| pred(s)) {
                return Ok(Some(s));
            }
            let alive = tokio::select! {
                changed = self.samples.changed() => changed.is_ok(),
                () = sleep_until(deadline.into()) => return Ok(None),
            };
            if !alive {
                bail!("stream ended: {}", self.close_reason());
            }
        }
    }
}

fn lock(stats: &Mutex<Stats>) -> MutexGuard<'_, Stats> {
    // Nothing that holds the lock can panic halfway through an update.
    stats.lock().unwrap_or_else(|e| e.into_inner())
}

async fn write_loop(
    mut sink: SplitSink<WsStream, Message>,
    mut rx: mpsc::UnboundedReceiver<Message>,
) {
    while let Some(msg) = rx.recv().await {
        let last = matches!(msg, Message::Close(_));
        if sink.send(msg).await.is_err() || last {
            break;
        }
    }
}

async fn read_loop(
    mut stream: SplitStream<WsStream>,
    units: std_mpsc::Sender<Unit>,
    events: mpsc::UnboundedSender<Value>,
    stats: Arc<Mutex<Stats>>,
    closed: watch::Sender<Option<String>>,
    start: Instant,
) {
    let mut joiner = wire::Reassembler::default();
    let reason = loop {
        let msg = match stream.next().await {
            Some(Ok(msg)) => msg,
            Some(Err(e)) => break format!("connection error: {e}"),
            None => break "connection closed without a close frame".to_owned(),
        };
        match msg {
            Message::Binary(b) => match b.first() {
                Some(&wire::MSG_VIDEO) => {
                    let unit = joiner.push(&b);
                    let received = Instant::now();
                    let mut s = lock(&stats);
                    s.protocol_errors += std::mem::take(&mut joiner.dropped);
                    match unit {
                        Ok(Some(au)) => {
                            s.units.push((received, au.wire_bytes));
                            s.keyframes += u64::from(au.header.key());
                            drop(s);
                            let unit = Unit {
                                seq: au.header.seq,
                                key: au.header.key(),
                                data: au.data,
                                received,
                            };
                            if units.send(unit).is_err() {
                                break "decode thread stopped".to_owned();
                            }
                        }
                        Ok(None) => {}
                        Err(wire::Malformed) => s.protocol_errors += 1,
                    }
                }
                Some(&wire::MSG_PONG) => match wire::pong(&b) {
                    Some(t) => {
                        let rtt = millis_since(start).wrapping_sub(t);
                        lock(&stats).rtt_ms.push(f64::from(rtt));
                    }
                    None => lock(&stats).protocol_errors += 1,
                },
                Some(&(wire::MSG_CURSOR_SHAPE | wire::MSG_CURSOR_POS)) => {}
                _ => lock(&stats).protocol_errors += 1,
            },
            Message::Text(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => {
                    match v["t"].as_str() {
                        Some("stats") => lock(&stats).server_stats = Some(v.clone()),
                        Some("screen") => {
                            let dim = |k: &str| v[k].as_u64().and_then(|n| u32::try_from(n).ok());
                            if let (Some(w), Some(h)) = (dim("w"), dim("h")) {
                                lock(&stats).screen = (w, h);
                            }
                        }
                        _ => {}
                    }
                    let _ = events.send(v);
                }
                Err(_) => lock(&stats).protocol_errors += 1,
            },
            Message::Close(frame) => {
                break match frame {
                    Some(f) => format!("closed by server: {} {}", u16::from(f.code), f.reason),
                    None => "closed by server".to_owned(),
                }
            }
            _ => {}
        }
    };
    let _ = closed.send(Some(reason));
}

/// Decodes access units in arrival order, samples the marker and acks every unit.
fn decode_loop(
    units: std_mpsc::Receiver<Unit>,
    out: mpsc::UnboundedSender<Message>,
    stats: Arc<Mutex<Stats>>,
    samples: watch::Sender<Option<Sample>>,
    marker: (u32, u32),
) {
    let mut decoder = None;
    // Like the web client: nothing decodes until a keyframe, and a decode error drops back here.
    let mut need_key = true;
    let mut last_idr_request: Option<Instant> = None;
    for unit in units {
        need_key &= !unit.key;
        // Every unit is acked, decoded or dropped, so the server's credit keeps flowing. As in
        // the web client, a unit dropped while waiting for a keyframe is acked with 0 ms.
        let mut decode_ms = 0;
        if !need_key {
            match decode(&mut decoder, &unit.data, marker) {
                Ok(Some(sample)) => {
                    lock(&stats).decoded.push(sample.at);
                    samples.send_replace(Some(sample));
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("tilt-probe: decode error at seq {}: {e:#}", unit.seq);
                    lock(&stats).decode_errors += 1;
                    need_key = true;
                }
            }
            decode_ms = u16::try_from(unit.received.elapsed().as_millis()).unwrap_or(u16::MAX);
        }
        let _ = out.send(Message::binary(wire::ack(unit.seq, decode_ms)));
        if need_key && last_idr_request.is_none_or(|t| t.elapsed() >= IDR_REQUEST_INTERVAL) {
            let _ = out.send(Message::text(r#"{"t":"idr"}"#));
            last_idr_request = Some(Instant::now());
        }
    }
}

/// Decodes one access unit and samples the marker. On error the decoder is dropped, so the
/// next keyframe starts from a fresh one.
///
/// OpenH264's decoder holds back the picture of a stream that is not Baseline (tilt's default
/// is High) until the next access unit arrives; on a still screen that is the refinement tail,
/// 100 ms later. The decoder therefore flushes whenever a unit yields no picture, so latency is
/// measured to the picture of the unit just received, as a browser shows it.
fn decode(
    slot: &mut Option<Decoder>,
    data: &[u8],
    marker: (u32, u32),
) -> anyhow::Result<Option<Sample>> {
    let mut decoder = match slot.take() {
        Some(d) => d,
        None => Decoder::with_api_config(
            OpenH264API::from_source(),
            DecoderConfig::new().flush_after_decode(Flush::Flush),
        )?,
    };
    let sample = decoder.decode(data)?.map(|yuv| Sample {
        color: marker_color(&yuv, marker),
        at: Instant::now(),
    });
    *slot = Some(decoder);
    Ok(sample)
}

fn marker_color(yuv: &impl YUVSource, (x, y): (u32, u32)) -> Option<usize> {
    let (w, h) = yuv.dimensions();
    let (x, y) = (x as usize, y as usize);
    if x >= w || y >= h {
        return None;
    }
    let (y_stride, u_stride, v_stride) = yuv.strides();
    let rgb = yuv_to_rgb(
        yuv.y()[y * y_stride + x],
        yuv.u()[y / 2 * u_stride + x / 2],
        yuv.v()[y / 2 * v_stride + x / 2],
    );
    nearest_color(rgb)
}

/// The probe's PING clock: ms since connect, wrapping like the protocol's u32.
fn millis_since(start: Instant) -> u32 {
    start.elapsed().as_millis() as u32
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Nearest-rank percentile of ascending `sorted`; 0 when empty.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Wire protocol v1 from the client's side (brief section 4). Integers are little-endian.
mod wire {
    pub const MSG_VIDEO: u8 = 0x01;
    pub const MSG_CURSOR_SHAPE: u8 = 0x02;
    pub const MSG_CURSOR_POS: u8 = 0x03;
    pub const MSG_PONG: u8 = 0x04;
    const MSG_ACK: u8 = 0x10;
    const MSG_PING: u8 = 0x11;
    const MSG_MOVE: u8 = 0x20;
    const MSG_BUTTON: u8 = 0x21;
    const MSG_KEY: u8 = 0x30;
    const FLAG_KEY: u8 = 1 << 0;
    const FLAG_MORE: u8 = 1 << 1;
    /// VIDEO header length, type byte included.
    const VIDEO_HEADER_LEN: usize = 18;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VideoHeader {
        pub flags: u8,
        pub seq: u32,
        pub capture_us: u64,
        pub width: u16,
        pub height: u16,
    }

    impl VideoHeader {
        /// Splits a VIDEO message into header and payload.
        pub fn parse(msg: &[u8]) -> Option<(VideoHeader, &[u8])> {
            if msg.len() < VIDEO_HEADER_LEN || msg[0] != MSG_VIDEO {
                return None;
            }
            let header = VideoHeader {
                flags: msg[1],
                seq: u32::from_le_bytes(msg[2..6].try_into().ok()?),
                capture_us: u64::from_le_bytes(msg[6..14].try_into().ok()?),
                width: u16::from_le_bytes(msg[14..16].try_into().ok()?),
                height: u16::from_le_bytes(msg[16..18].try_into().ok()?),
            };
            Some((header, &msg[VIDEO_HEADER_LEN..]))
        }

        pub fn key(&self) -> bool {
            self.flags & FLAG_KEY != 0
        }

        pub fn more(&self) -> bool {
            self.flags & FLAG_MORE != 0
        }
    }

    /// One reassembled access unit.
    #[derive(Debug, PartialEq, Eq)]
    pub struct AccessUnit {
        /// The first fragment's header.
        pub header: VideoHeader,
        /// Annex B payload of all fragments.
        pub data: Vec<u8>,
        /// Bytes received for it, headers included.
        pub wire_bytes: usize,
    }

    /// A VIDEO message too short for its header.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Malformed;

    /// Joins VIDEO fragments. Fragments of one seq are contiguous on the socket, so at most one
    /// access unit is ever pending.
    #[derive(Default)]
    pub struct Reassembler {
        pending: Option<AccessUnit>,
        /// Incomplete units abandoned because a different seq started (a server bug).
        pub dropped: u64,
    }

    impl Reassembler {
        /// Adds one VIDEO message; returns the access unit once its last fragment is in.
        pub fn push(&mut self, msg: &[u8]) -> Result<Option<AccessUnit>, Malformed> {
            let (header, payload) = VideoHeader::parse(msg).ok_or(Malformed)?;
            let mut unit = match self.pending.take() {
                Some(unit) if unit.header.seq == header.seq => unit,
                stale => {
                    self.dropped += u64::from(stale.is_some());
                    AccessUnit {
                        header,
                        data: Vec::new(),
                        wire_bytes: 0,
                    }
                }
            };
            unit.data.extend_from_slice(payload);
            unit.wire_bytes += msg.len();
            if header.more() {
                self.pending = Some(unit);
                Ok(None)
            } else {
                Ok(Some(unit))
            }
        }
    }

    /// ACK: cumulative, for every seq up to `seq`.
    pub fn ack(seq: u32, decode_ms: u16) -> Vec<u8> {
        [&[MSG_ACK][..], &seq.to_le_bytes(), &decode_ms.to_le_bytes()].concat()
    }

    pub fn ping(t: u32) -> Vec<u8> {
        [&[MSG_PING][..], &t.to_le_bytes()].concat()
    }

    /// The `t` a PONG echoes.
    pub fn pong(msg: &[u8]) -> Option<u32> {
        match msg {
            [MSG_PONG, t @ ..] => Some(u32::from_le_bytes(t.try_into().ok()?)),
            _ => None,
        }
    }

    pub fn key(down: bool, keysym: u32) -> Vec<u8> {
        [&[MSG_KEY, u8::from(down)][..], &keysym.to_le_bytes()].concat()
    }

    pub fn pointer_move(x: u16, y: u16) -> Vec<u8> {
        [&[MSG_MOVE][..], &x.to_le_bytes(), &y.to_le_bytes()].concat()
    }

    pub fn button(button: u8, down: bool, x: u16, y: u16) -> Vec<u8> {
        [
            &[MSG_BUTTON, button, u8::from(down)][..],
            &x.to_le_bytes(),
            &y.to_le_bytes(),
        ]
        .concat()
    }

    /// The normalized coordinate of pixel `px` on an axis of `extent` pixels:
    /// `round(px * 65535 / (extent - 1))`, which the server's `round(v * (extent - 1) / 65535)`
    /// maps back to exactly `px`.
    pub fn normalize(px: u32, extent: u32) -> u16 {
        if extent < 2 {
            return 0;
        }
        let last = u64::from(extent - 1);
        let px = u64::from(px).min(last);
        ((px * 65535 + last / 2) / last) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::wire::*;
    use super::*;

    fn video(flags: u8, seq: u32, payload: &[u8]) -> Vec<u8> {
        let mut m = vec![0x01, flags];
        m.extend_from_slice(&seq.to_le_bytes());
        m.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        m.extend_from_slice(&1920u16.to_le_bytes());
        m.extend_from_slice(&1080u16.to_le_bytes());
        m.extend_from_slice(payload);
        m
    }

    #[test]
    fn video_header_parse() {
        let msg = video(0b01, 0xA1B2_C3D4, &[0, 0, 0, 1, 0x67]);
        assert_eq!(msg.len(), 18 + 5);
        let (h, payload) = VideoHeader::parse(&msg).unwrap();
        assert_eq!(
            h,
            VideoHeader {
                flags: 1,
                seq: 0xA1B2_C3D4,
                capture_us: 0x0102_0304_0506_0708,
                width: 1920,
                height: 1080,
            }
        );
        assert!(h.key() && !h.more());
        assert_eq!(payload, &[0, 0, 0, 1, 0x67]);
        // A header with no payload is still well-formed.
        assert!(VideoHeader::parse(&msg[..18]).is_some());
        assert!(VideoHeader::parse(&msg[..17]).is_none());
        let mut wrong_type = msg.clone();
        wrong_type[0] = 0x02;
        assert!(VideoHeader::parse(&wrong_type).is_none());
        assert!(VideoHeader::parse(&[]).is_none());
    }

    #[test]
    fn reassembles_fragments() {
        let mut r = Reassembler::default();
        // seq 7: KEY, three fragments.
        assert_eq!(r.push(&video(0b11, 7, b"abc")), Ok(None));
        assert_eq!(r.push(&video(0b11, 7, b"def")), Ok(None));
        let au = r.push(&video(0b01, 7, b"gh")).unwrap().unwrap();
        assert_eq!(au.data, b"abcdefgh");
        assert_eq!(au.header.seq, 7);
        assert!(au.header.key());
        assert_eq!(au.wire_bytes, 3 * 18 + 8);
        // seq 8: a single unfragmented delta.
        let au = r.push(&video(0, 8, b"p")).unwrap().unwrap();
        assert_eq!((au.data.as_slice(), au.header.key()), (&b"p"[..], false));
        assert_eq!(r.dropped, 0);
    }

    #[test]
    fn interrupted_unit_is_dropped_and_counted() {
        let mut r = Reassembler::default();
        assert_eq!(r.push(&video(0b10, 9, b"half")), Ok(None));
        // seq 10 starts before seq 9 finished: 9 is abandoned, 10 decodes normally.
        let au = r.push(&video(0, 10, b"whole")).unwrap().unwrap();
        assert_eq!((au.header.seq, au.data.as_slice()), (10, &b"whole"[..]));
        assert_eq!(r.dropped, 1);
        assert_eq!(r.push(&[0x01, 0, 1]), Err(Malformed));
    }

    #[test]
    fn client_message_layouts() {
        assert_eq!(ack(0x0102_0304, 0x0506), [0x10, 4, 3, 2, 1, 6, 5]);
        assert_eq!(ping(0xAABB_CCDD), [0x11, 0xDD, 0xCC, 0xBB, 0xAA]);
        assert_eq!(key(true, 0x20), [0x30, 1, 0x20, 0, 0, 0]);
        assert_eq!(key(false, 0xFF08), [0x30, 0, 0x08, 0xFF, 0, 0]);
        assert_eq!(pointer_move(0x0102, 0x0304), [0x20, 2, 1, 4, 3]);
        assert_eq!(
            button(1, true, 0x0102, 0xFFFF),
            [0x21, 1, 1, 2, 1, 0xFF, 0xFF]
        );
        assert_eq!(pong(&[0x04, 0xDD, 0xCC, 0xBB, 0xAA]), Some(0xAABB_CCDD));
        assert_eq!(pong(&[0x04, 1, 2]), None);
        assert_eq!(pong(&[0x03, 1, 2, 3, 4]), None);
    }

    #[test]
    fn normalized_coordinates_map_back_to_the_same_pixel() {
        // The server's mapping, brief section 4.3.
        let server =
            |v: u16, extent: u32| (f64::from(v) * f64::from(extent - 1) / 65535.0).round() as u32;
        for extent in [2, 480, 768, 1024, 1080, 1920, 3840] {
            for px in 0..extent {
                assert_eq!(
                    server(normalize(px, extent), extent),
                    px,
                    "px {px} of {extent}"
                );
            }
        }
        assert_eq!(normalize(0, 1920), 0);
        assert_eq!(normalize(1919, 1920), 65535);
        assert_eq!(normalize(5000, 1920), 65535);
        assert_eq!(normalize(100, 1920), 3415);
        assert_eq!(normalize(3, 1), 0);
    }

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), 10.0);
        assert_eq!(percentile(&v, 95.0), 19.0);
        assert_eq!(percentile(&v, 100.0), 20.0);
        assert_eq!(percentile(&v[..1], 95.0), 1.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    /// A decoded picture in BT.709 limited range: marker colours classify at the marker point.
    struct Picture {
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    }

    impl YUVSource for Picture {
        fn dimensions_i32(&self) -> (i32, i32) {
            (16, 8)
        }
        fn dimensions(&self) -> (usize, usize) {
            (16, 8)
        }
        fn strides(&self) -> (usize, usize, usize) {
            (16, 8, 8)
        }
        fn strides_i32(&self) -> (i32, i32, i32) {
            (16, 8, 8)
        }
        fn y(&self) -> &[u8] {
            &self.y
        }
        fn u(&self) -> &[u8] {
            &self.u
        }
        fn v(&self) -> &[u8] {
            &self.v
        }
    }

    #[test]
    fn marker_sampling_reads_the_right_plane_offsets() {
        // Mid grey everywhere except pixel (10, 5), which is the marker green (40, 200, 60) in
        // limited-range BT.709: Y 150, Cb 83, Cr 63. Chroma is subsampled 2x2.
        let mut p = Picture {
            y: vec![126; 16 * 8],
            u: vec![128; 8 * 4],
            v: vec![128; 8 * 4],
        };
        p.y[5 * 16 + 10] = 150;
        p.u[2 * 8 + 5] = 83;
        p.v[2 * 8 + 5] = 63;
        assert_eq!(marker_color(&p, (10, 5)), Some(1));
        assert_eq!(marker_color(&p, (0, 0)), None);
        assert_eq!(marker_color(&p, (16, 0)), None);
    }
}
