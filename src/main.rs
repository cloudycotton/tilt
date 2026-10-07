//! tilt: stream a Linux X11 desktop to a browser over WebSocket + WebCodecs H.264,
//! and let one viewer at a time take control.

mod assets;
mod auth;
mod capture;
mod clock;
mod config;
mod control;
mod flow;
mod hub;
mod input;
mod protocol;
mod server;
mod session;
mod video;
mod worker;
mod x11;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::oneshot;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::auth::TokenSource;
use crate::config::Config;
use crate::hub::FrameHub;
use crate::server::{AppParts, AppState};
use crate::session::Sessions;

// musl's allocator serialises on one lock; mimalloc keeps the static binary fast.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// How long shutdown waits for each permanent thread.
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    clock::init();
    // Exits with code 2 on usage errors, including a missing token.
    let cfg = Arc::new(Config::load());
    init_tracing();
    match run(cfg) {
        Ok(code) => code,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Log filter from TILT_LOG, else RUST_LOG, else `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_env("TILT_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    // Colours only on a terminal: under systemd or in a log file they are noise.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// Why the server is stopping.
enum Stop {
    Signal(&'static str),
    /// A permanent thread ended (or flagged a fatal error) before shutdown was requested.
    Fatal(String),
}

fn run(cfg: Arc<Config>) -> anyhow::Result<ExitCode> {
    info!("tilt {}", env!("CARGO_PKG_VERSION"));
    let tokens = TokenSource::from_config(&cfg);
    if tokens.no_auth() {
        warn!(
            "--no-auth: ANYONE who can reach {} can watch AND control this desktop. \
             Use it for local testing only.",
            cfg.bind
        );
    }
    assets::init();
    let shutdown = Arc::new(AtomicBool::new(false));

    // Input first: capture needs its handle for the pull-forward after injected input.
    let (input, input_thread) =
        input::spawn_input_thread(cfg.display.clone(), Arc::clone(&shutdown))
            .context("starting input injection")?;
    let (hub, wake) = FrameHub::new();
    let capture = capture::spawn_capture(
        Arc::clone(&cfg),
        Arc::clone(&hub),
        wake,
        input.clone(),
        Arc::clone(&shutdown),
    )
    .context("starting screen capture")?;
    let sessions = Sessions::new(cfg.max_viewers);
    let (cursor, cursor_thread) = x11::cursor::spawn_cursor_thread(
        cfg.display.clone(),
        sessions.viewer_counter(),
        Arc::clone(&shutdown),
    )
    .context("starting cursor tracking")?;
    let (w, h) = hub.screen_size();
    info!(
        bind = %cfg.bind,
        display = %cfg.display,
        screen = %format_args!("{w}x{h}"),
        capture = capture.method,
        auth = auth_mode(&cfg),
        "ready"
    );

    let state = AppState::new(AppParts {
        cfg: Arc::clone(&cfg),
        tokens,
        hub,
        input,
        cursor,
        sessions,
        cpus: config::available_cpus(),
    });
    let mut threads: Vec<(&'static str, JoinHandle<()>)> = vec![("tilt-cursor", cursor_thread)];
    threads.extend(
        ["tilt-capture", "tilt-xevents"]
            .into_iter()
            .zip(capture.threads),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("tilt-rt")
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    let (stop, served) = rt.block_on(async {
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let mut server = tokio::spawn(server::serve(Arc::clone(&state), async move {
            let _ = stop_rx.await;
        }));
        let stop = tokio::select! {
            stop = wait_for_stop(&threads, &input_thread, &shutdown) => stop,
            served = &mut server => {
                // Normally a bind failure; nothing is listening, so stop the threads too.
                let why = match &served {
                    Ok(Ok(())) => "the HTTP server stopped".to_owned(),
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(e) => format!("the HTTP server task failed: {e}"),
                };
                error!("{why}; shutting down");
                return (Stop::Fatal(why), None);
            }
        };
        match &stop {
            Stop::Signal(name) => info!("{name}: shutting down"),
            Stop::Fatal(why) => error!("{why}; shutting down"),
        }
        let _ = stop_tx.send(());
        (stop, Some(server.await))
    });
    rt.shutdown_timeout(Duration::from_secs(1));

    // Sessions are closed and their input released; now the threads, input last so that it
    // can restore autorepeat and unbind spare keycodes.
    shutdown.store(true, Ordering::Relaxed);
    // Capture sleeps until something is due; this gets it to see the flag.
    state.hub.wake_capture();
    let deadline = Instant::now() + JOIN_TIMEOUT;
    for (name, handle) in threads {
        join_by(name, handle, deadline);
    }
    join_by("tilt-input", input_thread, Instant::now() + JOIN_TIMEOUT);
    drop(state);

    let served_ok = match served {
        Some(Ok(Ok(()))) | None => true,
        Some(Ok(Err(e))) => {
            error!("{e:#}");
            false
        }
        Some(Err(e)) => {
            error!("the HTTP server task failed: {e}");
            false
        }
    };
    info!("stopped");
    Ok(match stop {
        Stop::Signal(_) if served_ok => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

fn auth_mode(cfg: &Config) -> &'static str {
    match (cfg.no_auth, cfg.token.is_some(), cfg.token_file.is_some()) {
        (true, _, _) => "none (--no-auth)",
        (_, true, true) => "token + token file",
        (_, true, false) => "token",
        _ => "token file",
    }
}

/// The signals that stop tilt cleanly. SIGHUP (a closed terminal, a supervisor) and SIGQUIT
/// would otherwise end it at once, leaving keys held and autorepeat off on the display.
#[cfg(unix)]
struct StopSignals {
    int: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl StopSignals {
    fn new() -> std::io::Result<StopSignals> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(StopSignals {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
            quit: signal(SignalKind::quit())?,
        })
    }

    /// The name of the next one to arrive.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.int.recv() => "SIGINT",
            _ = self.term.recv() => "SIGTERM",
            _ = self.hup.recv() => "SIGHUP",
            _ = self.quit.recv() => "SIGQUIT",
        }
    }
}

/// Resolves on SIGINT, SIGTERM, SIGHUP or SIGQUIT, or when a permanent thread dies or raises
/// the shutdown flag.
async fn wait_for_stop(
    threads: &[(&'static str, JoinHandle<()>)],
    input: &JoinHandle<()>,
    shutdown: &AtomicBool,
) -> Stop {
    #[cfg(unix)]
    let mut signals = match StopSignals::new() {
        Ok(s) => s,
        Err(e) => return Stop::Fatal(format!("cannot handle signals: {e}")),
    };
    let mut check = tokio::time::interval(Duration::from_millis(250));
    loop {
        #[cfg(unix)]
        tokio::select! {
            name = signals.recv() => return Stop::Signal(name),
            _ = check.tick() => {}
        }
        #[cfg(not(unix))]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Stop::Signal("Ctrl-C"),
            _ = check.tick() => {}
        }
        let dead = threads
            .iter()
            .find(|(_, h)| h.is_finished())
            .map(|(name, _)| *name)
            .or_else(|| input.is_finished().then_some("tilt-input"));
        if let Some(name) = dead {
            return Stop::Fatal(format!("thread {name} stopped unexpectedly"));
        }
        if shutdown.load(Ordering::Relaxed) {
            return Stop::Fatal("a fatal error was reported".to_owned());
        }
    }
}

/// Joins `handle` if it finishes by `deadline`; otherwise leaves it behind.
fn join_by(name: &str, handle: JoinHandle<()>, deadline: Instant) {
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            warn!("{name} did not stop in time; exiting without it");
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if handle.join().is_err() {
        error!("{name} panicked");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hangup_and_quit_stop_tilt_cleanly() {
        let mut signals = StopSignals::new().unwrap();
        for (signal, name) in [(libc::SIGHUP, "SIGHUP"), (libc::SIGQUIT, "SIGQUIT")] {
            // SAFETY: raise only sends this process a signal, one that `signals` now catches
            // (unhandled, it would end the test run).
            assert_eq!(unsafe { libc::raise(signal) }, 0);
            let got = tokio::time::timeout(Duration::from_secs(5), signals.recv()).await;
            assert_eq!(got, Ok(name));
        }
    }
}
