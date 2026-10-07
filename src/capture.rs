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
use crate::video::convert::{bgrx_rows_to_i420, bgrx_to_i420, FramePool};
use crate::x11::conn;
use crate::x11::damage::{self, DamageTracker, XEventKind};
use crate::x11::grab::{GrabError, Grabber};

pub struct CaptureHandle {
    /// The grab path in use (Grabber::method), for the startup log.
    pub method: &'static str,
    pub threads: Vec<JoinHandle<()>>,
}

/// Damaged rows closer than this are grabbed as one band: a request per band costs more than
/// converting a few extra rows.
const MERGE_GAP: u32 = 32;
/// More bands than this are grabbed as one, from the first damaged row to the last.
const MAX_BANDS: usize = 4;
/// While safety polls keep finding nothing, each waits twice as long as the last, up to this
/// many times `--poll-ms`; damage starts over at `--poll-ms`.
const POLL_BACKOFF_MAX: u32 = 8;
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
        poll_gap: cfg.poll_interval().unwrap_or_default(),
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
        full_next: true,
        idle: false,
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
    /// The wait before the next safety poll: `poll`, backed off while polls find nothing.
    poll_gap: Duration,
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
    /// The next grab must be a full one: a failed grab took damage it never delivered.
    full_next: bool,
    /// No session is subscribed, and the memory for frames has been given back.
    idle: bool,
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
            let unwatched = self.hub.unwatched();
            if unwatched && !self.idle {
                self.go_idle();
            }
            self.idle = unwatched;
            let now = Instant::now();
            let mut due = None;
            if self.hub.demand() {
                let since_last = |gap| self.last_grab.map_or(now, |t| t + gap);
                let next = match (self.damage_pending, self.poll) {
                    (true, _) => Some((GrabKind::Damage, self.paced_grab_at(now))),
                    (false, Some(_)) => Some((GrabKind::Poll, since_last(self.poll_gap))),
                    (false, None) => None,
                };
                if let Some((kind, at)) = next {
                    let at = if self.failures >= REBUILD_AFTER {
                        since_last(self.rebuild_wait)
                    } else {
                        at
                    };
                    if now >= at {
                        self.grab(kind, now);
                        continue;
                    }
                    due = Some(at);
                }
            }
            // With nothing due this sleeps until X, a viewer or shutdown (hub.wake_capture)
            // has something: a still screen with no viewer waiting costs no wake-ups.
            let timer = due.map_or_else(crossbeam_channel::never, crossbeam_channel::at);
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
                recv(timer) -> _ => {}
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

    /// The last viewer left: hold no frame, no spare buffers and no grab memory until the next
    /// one comes, which gets a full grab at once.
    fn go_idle(&mut self) {
        self.hub.invalidate();
        self.pool.trim();
        self.grabber.release_pages();
        self.damage_pending = true;
        self.full_next = true;
        crate::release_memory();
    }

    fn on_event(&mut self, kind: XEventKind) {
        match kind {
            XEventKind::Damage => {
                self.damage_pending = true;
                self.poll_gap = self.poll.unwrap_or_default();
            }
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
        let (w, h) = self.grabber.size();
        let (w, h) = (w & !1, h & !1);
        // A damage grab reads only the damaged rows and takes the rest from the last frame, as
        // long as that shows the screen as of the last subtract: it has the same size and no
        // grab failed since.
        let base = match kind {
            GrabKind::Damage if !self.full_next => self
                .hub
                .latest()
                .filter(|f| (f.image.width, f.image.height) == (w, h)),
            _ => None,
        };
        // Subtract first, then grab: damage after this point raises a new notify, so no
        // change can slip between the grab and the re-arm.
        let bands = match &base {
            Some(_) => match self.damage.subtract_rows(&self.conn) {
                Ok(spans) => row_bands(spans, h),
                Err(e) => return self.failed(e.context("damage_subtract"), now),
            },
            None => match self.damage.subtract(&self.conn) {
                Ok(()) => None,
                Err(e) => return self.failed(e.context("damage_subtract"), now),
            },
        };
        if bands.as_ref().is_some_and(Vec::is_empty) {
            // Nothing was drawn since the last subtract (a poll grab took that damage).
            self.damage_pending = false;
            return;
        }
        let grabbed = match &bands {
            Some(bands) => self.grabber.grab_rows(&self.conn, bands),
            None => self.grabber.grab(&self.conn),
        };
        match grabbed {
            Ok(()) => {}
            Err(GrabError::GeometryChanged) => {
                // Normally the next grab, after the resize, succeeds and clears this; if it
                // keeps happening, the grabber gets rebuilt like for any other failure.
                debug!("screen geometry changed under the grab");
                self.failures += 1;
                self.full_next = true;
                self.resize_pending = true;
                return;
            }
            Err(GrabError::Other(e)) => return self.failed(e, now),
        }
        let grabbed_us = clock::now_us();
        if w == 0 || h == 0 {
            self.damage_pending = false;
            return;
        }
        let mut image = self.pool.get(w, h);
        let converted = match (&bands, &base) {
            (Some(bands), Some(base)) => {
                image.copy_from(&base.image);
                bands.iter().try_for_each(|&(top, bottom)| {
                    let pixels = self.grabber.pixels();
                    bgrx_rows_to_i420(pixels, self.grabber.stride(), &mut image, top, bottom)
                })
            }
            _ => bgrx_to_i420(self.grabber.pixels(), self.grabber.stride(), &mut image),
        };
        if let Err(e) = converted {
            return self.failed(e.context("converting to I420"), now);
        }
        // Only now: if anything above failed, the damage is still owed to the viewers.
        self.damage_pending = false;
        self.full_next = false;
        self.failures = 0;
        self.rebuild_wait = BROKEN_RETRY;
        if kind == GrabKind::Poll {
            let unchanged = self.hub.latest().is_some_and(|f| *f.image == *image);
            if unchanged {
                if let Some(poll) = self.poll {
                    self.poll_gap = (self.poll_gap * 2).min(poll * POLL_BACKOFF_MAX);
                }
                return;
            }
            debug!("safety poll found a change that damage did not report");
            self.poll_gap = self.poll.unwrap_or_default();
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
            rows = bands.map_or(h, |b| b.iter().map(|(t, b)| b - t).sum()),
            grab_us = grabbed_us - start_us,
            convert_us = converted_us - grabbed_us,
            "captured"
        );
    }

    fn failed(&mut self, e: anyhow::Error, now: Instant) {
        self.failures += 1;
        self.full_next = true;
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

/// The row bands a damage grab of an `height`-row frame (even) reads, from the damaged row spans
/// `[top, bottom)`: merged when close, widened to even rows (4:2:0 chroma covers row pairs) and
/// clipped to the frame. None when a full grab is about as cheap. Empty when nothing was
/// damaged.
fn row_bands(mut spans: Vec<(u32, u32)>, height: u32) -> Option<Vec<(u32, u32)>> {
    spans.sort_unstable();
    let mut bands: Vec<(u32, u32)> = Vec::new();
    for (top, bottom) in spans {
        if top >= bottom {
            continue;
        }
        let (top, bottom) = (top & !1, bottom.saturating_add(1).min(height) & !1);
        if top >= bottom {
            continue;
        }
        match bands.last_mut() {
            Some(last) if top <= last.1.saturating_add(MERGE_GAP) => last.1 = last.1.max(bottom),
            _ => bands.push((top, bottom)),
        }
    }
    if bands.len() > MAX_BANDS {
        bands = vec![(bands[0].0, bands[bands.len() - 1].1)];
    }
    let rows: u32 = bands.iter().map(|(top, bottom)| bottom - top).sum();
    (rows * 4 < height * 3).then_some(bands)
}

#[cfg(test)]
mod band_tests {
    use super::*;

    #[test]
    fn damaged_rows_become_even_merged_bands() {
        assert_eq!(row_bands(vec![], 1080), Some(vec![]));
        // A caret: rows widened to even bounds.
        assert_eq!(row_bands(vec![(101, 118)], 1080), Some(vec![(100, 118)]));
        assert_eq!(row_bands(vec![(100, 117)], 1080), Some(vec![(100, 118)]));
        // Close spans merge, overlapping ones too, in any order; far ones stay apart.
        assert_eq!(
            row_bands(vec![(500, 510), (100, 120), (130, 140), (105, 110)], 1080),
            Some(vec![(100, 140), (500, 510)])
        );
        // Clipped to the (even) frame; a span wholly outside it is dropped.
        assert_eq!(
            row_bands(vec![(1070, 1090)], 1080),
            Some(vec![(1070, 1080)])
        );
        assert_eq!(row_bands(vec![(1080, 1090)], 1080), Some(vec![]));
        assert_eq!(row_bands(vec![(9, 9)], 1080), Some(vec![]));
        assert_eq!(row_bands(vec![(0, u32::MAX)], 1080), None);
        // Too many bands: one from the first damaged row to the last.
        let many: Vec<_> = (0..6).map(|i| (i * 100, i * 100 + 4)).collect();
        assert_eq!(row_bands(many, 1080), Some(vec![(0, 504)]));
        // Most of the screen: a full grab.
        assert_eq!(row_bands(vec![(0, 810)], 1080), None);
        assert_eq!(row_bands(vec![(0, 808)], 1080), Some(vec![(0, 808)]));
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

    /// The screen as a full grab and conversion see it now.
    fn full_frame(
        grabber: &mut Grabber,
        conn: &RustConnection,
    ) -> crate::video::convert::I420Frame {
        grabber.refresh_geometry(conn).unwrap();
        grabber.grab(conn).unwrap();
        let (w, h) = grabber.size();
        let pool = FramePool::new();
        let mut frame = pool.get(w & !1, h & !1);
        bgrx_to_i420(grabber.pixels(), grabber.stride(), &mut frame).unwrap();
        frame.clone()
    }

    /// Asks for frames until the newest one equals `want`.
    fn settles_to(
        hub: &FrameHub,
        rx: &Receiver<()>,
        want: &crate::video::convert::I420Frame,
    ) -> bool {
        eventually(WAIT, || {
            hub.set_wants(1, true);
            let _ = rx.recv_timeout(Duration::from_millis(20));
            hub.latest().is_some_and(|f| *f.image == *want)
        })
    }

    /// A screen size change between damage grabs: the frame before it cannot be the base of
    /// the next, which must be a full grab at the new size.
    #[test]
    #[ignore = "needs Xvfb and xrandr; run in tilt-dev with --ignored"]
    fn frames_follow_resizes_with_full_grabs() {
        use crate::x11::conn::connect;

        let x = Xvfb::start("1920x1080x24", &[]);
        x.add_720p_mode();
        let args = [
            "tilt",
            "--no-auth",
            "--poll-ms",
            "0",
            "--display",
            x.display.as_str(),
        ];
        let cfg = Arc::new(Config::try_load_from(args).unwrap());
        let (hub, wake) = FrameHub::new();
        let (input, _input_rx) = InputHandle::detached();
        let shutdown = Arc::new(AtomicBool::new(false));
        let capture =
            spawn_capture(cfg, Arc::clone(&hub), wake, input, Arc::clone(&shutdown)).unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        hub.subscribe(1, tx);
        let (conn, screen) = connect(&x.display).unwrap();
        let mut reference = Grabber::new(&conn, screen).unwrap();
        let block = |x0, y0| Rectangle {
            x: x0,
            y: y0,
            width: 40,
            height: 30,
        };

        for (i, (mode, size)) in [
            ("tilt-720p", (1280, 720)),
            ("1920x1080", (1920, 1080)),
            ("tilt-720p", (1280, 720)),
        ]
        .into_iter()
        .enumerate()
        {
            x.fill(0x10_2030 * (i as u32 + 1), block(1500, 900));
            assert!(
                settles_to(&hub, &rx, &full_frame(&mut reference, &conn)),
                "{mode}: before"
            );
            x.set_mode(mode);
            x.fill(0xff_8000 >> i, block(100, 100));
            let want = full_frame(&mut reference, &conn);
            assert_eq!((want.width, want.height), size);
            assert!(settles_to(&hub, &rx, &want), "{mode}: after");
            assert_eq!(hub.screen_size(), size);
        }

        shutdown.store(true, Ordering::Relaxed);
        hub.wake_capture();
        for thread in capture.threads {
            thread.join().unwrap();
        }
    }

    /// Frames built from damaged row bands on top of the last frame must show exactly what a
    /// full grab shows, whatever gets drawn: on the root, in a child window, by moving, mapping
    /// and unmapping that window, in many places at once, or all over. The safety poll is off,
    /// so it cannot paper over a band that missed something.
    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn damage_band_frames_match_full_grabs() {
        use x11rb::protocol::xproto::{ChangeGCAux, ConfigureWindowAux, CreateGCAux};

        use crate::x11::conn::connect;

        let (w, h) = (640u32, 480u32);
        let x = Xvfb::start("640x480x24", &[]);
        let args = [
            "tilt",
            "--no-auth",
            "--poll-ms",
            "0",
            "--display",
            x.display.as_str(),
        ];
        let cfg = Arc::new(Config::try_load_from(args).unwrap());
        let (hub, wake) = FrameHub::new();
        let (input, _input_rx) = InputHandle::detached();
        let shutdown = Arc::new(AtomicBool::new(false));
        let capture =
            spawn_capture(cfg, Arc::clone(&hub), wake, input, Arc::clone(&shutdown)).unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        hub.subscribe(1, tx);

        let (conn, screen) = connect(&x.display).unwrap();
        let root = conn.setup().roots[screen].root;
        let child = conn.generate_id().unwrap();
        conn.create_window(
            0,
            child,
            root,
            50,
            60,
            200,
            150,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().background_pixel(0x336699),
        )
        .unwrap();
        conn.map_window(child).unwrap();
        let gc = conn.generate_id().unwrap();
        conn.create_gc(
            gc,
            root,
            &CreateGCAux::new()
                .subwindow_mode(x11rb::protocol::xproto::SubwindowMode::CLIP_BY_CHILDREN),
        )
        .unwrap();
        let mut reference = Grabber::new(&conn, screen).unwrap();

        let mut seed = 0x2545_f491_u32;
        let mut rand = |n: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % n
        };
        for step in 0..80 {
            let fill = |target, colour, rects: &[Rectangle]| {
                conn.change_gc(gc, &ChangeGCAux::new().foreground(colour))
                    .unwrap();
                conn.poly_fill_rectangle(target, gc, rects).unwrap();
            };
            let colour = rand(0x100_0000);
            let rect = |r: &mut dyn FnMut(u32) -> u32, max_w: u32, max_h: u32| {
                let (x0, y0) = (r(max_w) as i16, r(max_h) as i16);
                Rectangle {
                    x: x0,
                    y: y0,
                    width: 1 + r(60) as u16,
                    height: 1 + r(40) as u16,
                }
            };
            match step % 7 {
                0 => fill(root, colour, &[rect(&mut rand, w, h)]),
                1 => fill(child, colour, &[rect(&mut rand, 200, 150)]),
                2 => {
                    let aux = ConfigureWindowAux::new()
                        .x(rand(w - 100) as i32)
                        .y(rand(h - 100) as i32);
                    conn.configure_window(child, &aux).unwrap();
                }
                3 => {
                    if step % 2 == 0 {
                        conn.unmap_window(child).unwrap();
                    } else {
                        conn.map_window(child).unwrap();
                    }
                }
                4 => {
                    // Scattered: more bands than are grabbed one by one.
                    let rects: Vec<_> = (0..8).map(|_| rect(&mut rand, w, h)).collect();
                    fill(root, colour, &rects);
                }
                5 => {
                    // Two far apart, single rows at odd offsets.
                    let rects = [
                        Rectangle {
                            x: 3,
                            y: 7,
                            width: 30,
                            height: 1,
                        },
                        Rectangle {
                            x: 300,
                            y: 401,
                            width: 7,
                            height: 1,
                        },
                    ];
                    fill(root, colour, &rects);
                }
                _ => {
                    let all = Rectangle {
                        x: 0,
                        y: 0,
                        width: w as u16,
                        height: h as u16,
                    };
                    fill(root, colour, &[all]);
                }
            }
            conn.get_input_focus().unwrap().reply().unwrap();

            // The screen is still now: the newest frame must come to match a full grab.
            let want = full_frame(&mut reference, &conn);
            assert!(
                settles_to(&hub, &rx, &want),
                "step {step}: the frame differs from a full grab"
            );
        }

        shutdown.store(true, Ordering::Relaxed);
        hub.wake_capture();
        for thread in capture.threads {
            thread.join().unwrap();
        }
    }

    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn the_next_viewer_starts_from_a_fresh_grab() {
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

        // With nobody watching nothing is held, even when nothing was drawn; the next viewer
        // gets a fresh grab at once, without waiting for a safety poll.
        hub.unsubscribe(2);
        assert!(
            eventually(WAIT, || hub.latest().is_none()),
            "a frame outlived the last session"
        );
        let (tx, rx) = crossbeam_channel::bounded(1);
        hub.subscribe(3, tx);
        let asked = Instant::now();
        assert!(next_frame(&hub, &rx, 3) > fresh);
        assert!(
            asked.elapsed() < Duration::from_millis(500),
            "{:?}",
            asked.elapsed()
        );
        // The grab memory given back while idle is filled anew: the frame is the screen.
        let (conn, screen) = crate::x11::conn::connect(&x.display).unwrap();
        let want = full_frame(&mut Grabber::new(&conn, screen).unwrap(), &conn);
        assert!(*hub.latest().unwrap().image == want);

        shutdown.store(true, Ordering::Relaxed);
        hub.wake_capture();
        for thread in capture.threads {
            thread.join().unwrap();
        }
    }
}
