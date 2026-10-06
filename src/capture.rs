//! The capture thread `tilt-capture` and its X event reader `tilt-xevents`: demand-driven,
//! damage-woken, frame-paced grabs published to the FrameHub (brief section 5.2).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender};
use tracing::{debug, error, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, Window,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::clock::{self, LogEvery};
use crate::config::Config;
use crate::hub::{Frame, FrameHub};
use crate::input::InputHandle;
use crate::video::convert::{bgrx_to_i420, FramePool};
use crate::x11::conn;
use crate::x11::damage::{self, DamageTracker, XEventKind};
use crate::x11::grab::{GrabError, Grabber};

pub struct CaptureHandle {
    /// The grab path in use (Grabber::method), for the startup log.
    pub method: &'static str,
    pub threads: Vec<JoinHandle<()>>,
}

/// Longest sleep, so shutdown is noticed promptly.
const MAX_WAIT: Duration = Duration::from_millis(250);
/// Consecutive failed grabs before the Grabber is rebuilt (brief 5.2).
const REBUILD_AFTER: u32 = 5;
/// The first wait before rebuilding a failing grabber; it doubles on every rebuild up to
/// MAX_REBUILD_WAIT and resets after a good grab, so a broken X server is not hammered with
/// segment setups.
const BROKEN_RETRY: Duration = Duration::from_secs(1);
const MAX_REBUILD_WAIT: Duration = Duration::from_secs(30);

/// Opens the capture connection to `cfg.display`, sets up damage and the grabber, records the
/// screen size in `hub`, then starts both threads. `wake` is the receiver from FrameHub::new.
pub fn spawn_capture(
    cfg: Arc<Config>,
    hub: Arc<FrameHub>,
    wake: Receiver<()>,
    input: InputHandle,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<CaptureHandle> {
    let (conn, screen) = conn::connect(&cfg.display)?;
    let conn = Arc::new(conn);
    let root = conn
        .setup()
        .roots
        .get(screen)
        .with_context(|| format!("display {} has no screen {screen}", cfg.display))?
        .root;
    let damage = DamageTracker::new(&conn, root).context("setting up DAMAGE on the root window")?;
    let grabber = Grabber::new(&conn, screen).context("setting up screen capture")?;
    let (width, height) = grabber.size();
    hub.set_screen_size(width, height);
    let method = grabber.method();
    if !damage.has_randr() {
        warn!("RandR is unavailable: screen size changes are noticed only when a grab fails");
    }

    // tilt-xevents blocks in wait_for_event; at shutdown capture sends this window (and so this
    // connection) a ClientMessage to wake it up.
    let doorbell = conn.generate_id()?;
    conn.create_window(
        0,
        doorbell,
        root,
        -1,
        -1,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )?;
    conn.flush()?;

    // Damage arrives at most once per subtract, so this never fills in practice; when it does,
    // the reader simply waits.
    let (events_tx, events_rx) = crossbeam_channel::bounded(256);
    let xevents = std::thread::Builder::new()
        .name("tilt-xevents".into())
        .spawn({
            let conn = Arc::clone(&conn);
            let shutdown = Arc::clone(&shutdown);
            move || xevents_loop(&conn, &events_tx, &shutdown)
        })?;
    let capture = Capture {
        fi: cfg.frame_interval(),
        poll: cfg.poll_interval(),
        cfg,
        conn,
        hub,
        input,
        shutdown,
        damage,
        grabber,
        pool: FramePool::new(),
        doorbell,
        seq: 0,
        damage_pending: true,
        resize_pending: false,
        last_grab: None,
        last_grab_us: 0,
        next_slot: None,
        failures: 0,
        rebuild_wait: BROKEN_RETRY,
        grab_errors: LogEvery::new(Duration::from_secs(5)),
    };
    let capture = std::thread::Builder::new()
        .name("tilt-capture".into())
        .spawn(move || capture.run(&events_rx, &wake))?;
    Ok(CaptureHandle {
        method,
        threads: vec![capture, xevents],
    })
}

/// Forwards the events capture cares about until shutdown or the connection fails.
fn xevents_loop(conn: &RustConnection, tx: &Sender<XEventKind>, shutdown: &AtomicBool) {
    loop {
        let event = match conn.wait_for_event() {
            Ok(event) => event,
            Err(e) => {
                if !shutdown.load(Ordering::Relaxed) {
                    error!("the capture X connection failed: {e}");
                }
                return;
            }
        };
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        match damage::classify(&event) {
            XEventKind::Other => {
                if let Event::Error(e) = event {
                    debug!("X error on the capture connection: {e:?}");
                }
            }
            kind => {
                if tx.send(kind).is_err() {
                    return;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrabKind {
    /// Damage said the screen changed: always publish.
    Damage,
    /// Safety poll: publish only if the Y plane differs from `latest`.
    Poll,
}

struct Capture {
    cfg: Arc<Config>,
    fi: Duration,
    poll: Option<Duration>,
    conn: Arc<RustConnection>,
    hub: Arc<FrameHub>,
    input: InputHandle,
    shutdown: Arc<AtomicBool>,
    damage: DamageTracker,
    grabber: Grabber,
    pool: FramePool,
    doorbell: Window,
    /// Seq of the last published frame.
    seq: u64,
    damage_pending: bool,
    resize_pending: bool,
    /// When the last grab attempt started, in both clocks.
    last_grab: Option<Instant>,
    last_grab_us: u64,
    /// When the next damage grab is due without input pull-forward.
    next_slot: Option<Instant>,
    /// Consecutive failed grabs.
    failures: u32,
    rebuild_wait: Duration,
    grab_errors: LogEvery,
}

impl Capture {
    fn run(mut self, events: &Receiver<XEventKind>, wake: &Receiver<()>) {
        info!(
            display = %self.cfg.display,
            fps = self.cfg.max_fps,
            "capture running"
        );
        while !self.shutdown.load(Ordering::Relaxed) {
            if self.resize_pending {
                self.resize();
            }
            let now = Instant::now();
            let mut due = None;
            if self.hub.demand() {
                let since_last = |gap| self.last_grab.map_or(now, |t| t + gap);
                let (kind, at) = if self.damage_pending {
                    (Some(GrabKind::Damage), self.paced_grab_at(now))
                } else {
                    (
                        self.poll.map(|_| GrabKind::Poll),
                        since_last(self.poll.unwrap_or(MAX_WAIT)),
                    )
                };
                let at = if self.failures >= REBUILD_AFTER {
                    since_last(self.rebuild_wait)
                } else {
                    at
                };
                if let Some(kind) = kind {
                    if now >= at {
                        self.grab(kind, now);
                        continue;
                    }
                    due = Some(at);
                }
            }
            let deadline = due.map_or(now + MAX_WAIT, |t| t.min(now + MAX_WAIT));
            crossbeam_channel::select! {
                recv(events) -> event => match event {
                    Ok(kind) => self.on_event(kind),
                    Err(_) => {
                        // tilt-xevents is gone: the X connection failed. Nothing more can be
                        // captured, so bring the whole server down.
                        if !self.shutdown.swap(true, Ordering::Relaxed) {
                            error!("capture stopped: lost the X server");
                        }
                        return;
                    }
                },
                recv(wake) -> _ => {}
                default(deadline.saturating_duration_since(now)) => {}
            }
            while let Ok(kind) = events.try_recv() {
                self.on_event(kind);
            }
            if self.damage_pending && !self.hub.demand() {
                // Nobody is waiting, so nothing is grabbed (brief 5.2), but `latest` no longer
                // shows the screen: with no session at all, the next viewer should start from
                // a fresh grab; connected sessions keep it, marked stale (see screen_changed).
                // This looks at damage_pending, not at whether damage just arrived: damage that
                // came while the last session was still connected is reported once (NON_EMPTY,
                // and nothing subtracts it now), and the hub wakes this loop when that session
                // leaves.
                self.hub.screen_changed();
            }
        }
        self.ring_doorbell();
        debug!("capture stopped");
    }

    fn on_event(&mut self, kind: XEventKind) {
        match kind {
            XEventKind::Damage => self.damage_pending = true,
            XEventKind::ScreenChange => self.resize_pending = true,
            XEventKind::Other => {}
        }
    }

    /// Frame pacing (brief 5.2): the next slot of a one-interval grid (clock::next_slot), so
    /// timers that wake late do not lower the frame rate; right after injected input, half an
    /// interval after the last grab (input pull-forward).
    fn paced_grab_at(&self, now: Instant) -> Instant {
        let slot = self.next_slot.unwrap_or(now);
        match self.last_grab {
            Some(t) if self.input.last_input_us() > self.last_grab_us => slot.min(t + self.fi / 2),
            _ => slot,
        }
    }

    fn resize(&mut self) {
        self.resize_pending = false;
        self.damage_pending = true;
        match self.grabber.refresh_geometry(&self.conn) {
            Ok(changed) => {
                let (w, h) = self.grabber.size();
                if changed || (w, h) != self.hub.screen_size() {
                    info!("screen size is now {w}x{h}");
                    self.hub.set_screen_size(w, h);
                    self.hub.invalidate();
                }
            }
            Err(e) => self.failed(e.context("re-reading the screen geometry"), Instant::now()),
        }
    }

    fn grab(&mut self, kind: GrabKind, now: Instant) {
        let start_us = clock::now_us();
        self.last_grab = Some(now);
        self.last_grab_us = start_us;
        self.next_slot = Some(clock::next_slot(self.next_slot, now, self.fi));
        if self.failures >= REBUILD_AFTER && !self.rebuild() {
            return;
        }
        // Subtract first, then grab: damage after this point raises a new notify, so no
        // change can slip between the grab and the re-arm.
        if let Err(e) = self.damage.subtract(&self.conn) {
            return self.failed(e.context("damage_subtract"), now);
        }
        match self.grabber.grab(&self.conn) {
            Ok(()) => {}
            Err(GrabError::GeometryChanged) => {
                // Normally the next grab, after the resize, succeeds and clears this; if it
                // keeps happening, the grabber gets rebuilt like for any other failure.
                debug!("screen geometry changed under the grab");
                self.failures += 1;
                self.resize_pending = true;
                return;
            }
            Err(GrabError::Other(e)) => return self.failed(e, now),
        }
        let grabbed_us = clock::now_us();
        let (w, h) = self.grabber.size();
        let (w, h) = (w & !1, h & !1);
        if w == 0 || h == 0 {
            self.damage_pending = false;
            return;
        }
        let mut image = self.pool.get(w, h);
        if let Err(e) = bgrx_to_i420(self.grabber.pixels(), self.grabber.stride(), &mut image) {
            return self.failed(e.context("converting to I420"), now);
        }
        // Only now: if anything above failed, the damage is still owed to the viewers.
        self.damage_pending = false;
        self.failures = 0;
        self.rebuild_wait = BROKEN_RETRY;
        if kind == GrabKind::Poll {
            let unchanged = self
                .hub
                .latest()
                .is_some_and(|f| (f.image.width, f.image.height) == (w, h) && f.image.y == image.y);
            if unchanged {
                return;
            }
            debug!("safety poll found a change that damage did not report");
        }
        self.seq += 1;
        let converted_us = clock::now_us();
        self.hub.publish(Frame {
            seq: self.seq,
            capture_us: start_us,
            image,
        });
        debug!(
            seq = self.seq,
            grab_us = grabbed_us - start_us,
            convert_us = converted_us - grabbed_us,
            "captured"
        );
    }

    fn failed(&mut self, e: anyhow::Error, now: Instant) {
        self.failures += 1;
        if let Some(suppressed) = self.grab_errors.ready(now) {
            warn!(failures = self.failures, suppressed, "grab failed: {e:#}");
        }
    }

    /// Replaces the grabber after repeated failures; false if that failed too.
    fn rebuild(&mut self) -> bool {
        self.rebuild_wait = (self.rebuild_wait * 2).min(MAX_REBUILD_WAIT);
        // Unlike a fresh Grabber::new, this also detaches the old segment on the server.
        match self.grabber.rebuild(&self.conn) {
            Ok(()) => {
                info!(
                    method = self.grabber.method(),
                    "rebuilt the grabber after {} failed grabs", self.failures
                );
                self.failures = 0;
                self.resize_pending = true;
                true
            }
            Err(e) => {
                warn!("rebuilding the grabber failed: {e:#}");
                false
            }
        }
    }

    fn ring_doorbell(&self) {
        let event = ClientMessageEvent::new(32, self.doorbell, AtomEnum::NONE, [0u32; 5]);
        // With an empty mask the event goes to the window's creator: this connection.
        let _ = self
            .conn
            .send_event(false, self.doorbell, EventMask::NO_EVENT, event);
        let _ = self.conn.destroy_window(self.doorbell);
        let _ = self.conn.flush();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use x11rb::protocol::xproto::Rectangle;

    use super::*;
    use crate::x11::testutil::{eventually, Xvfb};

    const WAIT: Duration = Duration::from_secs(5);

    /// A viewer that waits for the next frame, as a worker does.
    fn next_frame(hub: &FrameHub, rx: &Receiver<()>, sid: u64) -> u64 {
        hub.set_wants(sid, true);
        rx.recv_timeout(WAIT).expect("no frame published");
        hub.latest().expect("published, so there is a latest").seq
    }

    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn the_next_viewer_starts_from_a_fresh_grab_once_the_screen_changed() {
        let x = Xvfb::start("640x480x24", &[]);
        let args = ["tilt", "--no-auth", "--display", x.display.as_str()];
        let cfg = Arc::new(Config::try_load_from(args).unwrap());
        let (hub, wake) = FrameHub::new();
        let (input, _input_rx) = InputHandle::detached();
        let shutdown = Arc::new(AtomicBool::new(false));
        let capture =
            spawn_capture(cfg, Arc::clone(&hub), wake, input, Arc::clone(&shutdown)).unwrap();

        let (tx, rx) = crossbeam_channel::bounded(1);
        hub.subscribe(1, tx);
        let first = next_frame(&hub, &rx, 1);
        // The screen changes while session 1 is connected but asks for nothing (between
        // frames, or out of credit): it keeps `latest`, the newest frame there is for it, but
        // marked stale, so that it can ask for a fresh grab once it has credit again.
        assert!(!hub.latest_is_stale());
        x.fill(
            0xff0000,
            Rectangle {
                x: 10,
                y: 10,
                width: 20,
                height: 20,
            },
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(hub.latest().map(|f| f.seq), Some(first));
        assert!(hub.latest_is_stale());
        // Then it leaves. The damage was reported while it was there, and not again since, but
        // the next viewer must still not start from a screen that has changed since.
        hub.unsubscribe(1);
        assert!(
            eventually(WAIT, || hub.latest().is_none()),
            "a stale latest outlived the last session"
        );
        let (tx, rx) = crossbeam_channel::bounded(1);
        hub.subscribe(2, tx);
        let fresh = next_frame(&hub, &rx, 2);
        assert!(fresh > first);

        // With nothing drawn since, `latest` is still the screen and the next viewer gets it.
        hub.unsubscribe(2);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(hub.latest().map(|f| f.seq), Some(fresh));

        shutdown.store(true, Ordering::Relaxed);
        for thread in capture.threads {
            thread.join().unwrap();
        }
    }
}
