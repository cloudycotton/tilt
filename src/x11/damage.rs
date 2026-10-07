//! DAMAGE on the root as a wake-up signal, RandR screen-change selection, and event
//! classification for the capture loop.

use anyhow::Context;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{ConnectionExt as _, Damage, ReportLevel};
use x11rb::protocol::randr::{self, ConnectionExt as _, NotifyMask};
use x11rb::protocol::xfixes::{ConnectionExt as _, Region};
use x11rb::protocol::xproto::Window;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XEventKind {
    /// Something on the root was drawn.
    Damage,
    /// The root changed size.
    ScreenChange,
    Other,
}

pub struct DamageTracker {
    damage: Damage,
    region: Region,
    randr: bool,
}

impl DamageTracker {
    /// Creates a NON_EMPTY damage on `root` and selects RandR screen-change events when the
    /// extension is present. The damage starts out non-empty and unreported: nothing is
    /// reported before the first `subtract`, so the caller grabs once first.
    pub fn new(conn: &RustConnection, root: Window) -> anyhow::Result<DamageTracker> {
        // Both extensions refuse other requests until the client has announced its version.
        conn.damage_query_version(1, 1)?.reply().context("DAMAGE")?;
        conn.xfixes_query_version(5, 0)?.reply().context("XFIXES")?;
        let damage = conn.generate_id()?;
        // NON_EMPTY: one DamageNotify whenever the damage goes from empty to non-empty, so the
        // event rate is bounded by our subtracts rather than by how busy the screen is.
        conn.damage_create(damage, root, ReportLevel::NON_EMPTY)?
            .check()
            .context("DamageCreate")?;
        // Creating it reports the whole root, and that check was a round trip, so the report
        // is queued by now. Dropped: read later by the capture loop, it would come after the
        // first grab and mark that grab stale.
        while let Some(event) = conn.poll_for_event()? {
            if classify(&event) != XEventKind::Damage {
                tracing::debug!("X event before capture started: {event:?}");
            }
        }
        let region = conn.generate_id()?;
        conn.xfixes_create_region(region, &[])?
            .check()
            .context("XFixesCreateRegion")?;
        let randr = conn
            .extension_information(randr::X11_EXTENSION_NAME)?
            .is_some();
        if randr {
            conn.randr_select_input(root, NotifyMask::SCREEN_CHANGE)?
                .check()
                .context("RRSelectInput")?;
        }
        Ok(DamageTracker {
            damage,
            region,
            randr,
        })
    }

    /// Re-arms the damage with `damage_subtract(damage, NONE, region)`; does not wait for a reply.
    pub fn subtract(&self, conn: &RustConnection) -> anyhow::Result<()> {
        // An error would arrive as an event. The flush puts the request on the wire even if no
        // reply wait follows it, since waiting for events does not flush.
        conn.damage_subtract(self.damage, x11rb::NONE, self.region)?;
        conn.flush()?;
        Ok(())
    }

    /// Re-arms the damage like `subtract` and returns the row spans `[top, bottom)` of what was
    /// damaged since the last subtract, one per rectangle of the region, unsorted and possibly
    /// overlapping. Costs one round trip.
    pub fn subtract_rows(&self, conn: &RustConnection) -> anyhow::Result<Vec<(u32, u32)>> {
        conn.damage_subtract(self.damage, x11rb::NONE, self.region)?;
        let region = conn
            .xfixes_fetch_region(self.region)?
            .reply()
            .context("XFixesFetchRegion")?;
        Ok(region
            .rectangles
            .iter()
            .filter(|r| r.width > 0 && r.height > 0)
            .map(|r| {
                let top = i32::from(r.y).max(0) as u32;
                (top, (i32::from(r.y) + i32::from(r.height)).max(0) as u32)
            })
            .filter(|(top, bottom)| top < bottom)
            .collect())
    }

    /// Whether screen-change events were selected.
    pub fn has_randr(&self) -> bool {
        self.randr
    }
}

pub fn classify(event: &Event) -> XEventKind {
    match event {
        Event::DamageNotify(_) => XEventKind::Damage,
        Event::RandrScreenChangeNotify(_) => XEventKind::ScreenChange,
        _ => XEventKind::Other,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod x_tests {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ChangeGCAux, ConnectionExt as _, CreateGCAux, Rectangle};
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;

    use super::{classify, DamageTracker, XEventKind};
    use crate::x11::conn::connect;
    use crate::x11::grab::Grabber;
    use crate::x11::testutil::{events_within, Xvfb};

    const BLOCK: Rectangle = Rectangle {
        x: 10,
        y: 10,
        width: 20,
        height: 20,
    };

    fn damage_events(conn: &RustConnection, wait: Duration) -> usize {
        events_within(conn, wait)
            .iter()
            .filter(|e| classify(e) == XEventKind::Damage)
            .count()
    }

    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn notifies_once_per_subtract_and_not_when_idle() {
        let x = Xvfb::start("640x480x24", &[]);
        let (conn, screen) = connect(&x.display).unwrap();
        let damage = DamageTracker::new(&conn, conn.setup().roots[screen].root).unwrap();
        // Creating the damage reports the whole root, but `new` drops that report.
        assert_eq!(damage_events(&conn, Duration::from_millis(200)), 0);
        // Already non-empty, so drawing adds nothing until a subtract re-arms it.
        x.fill(0xff0000, BLOCK);
        assert_eq!(damage_events(&conn, Duration::from_millis(200)), 0);

        damage.subtract(&conn).unwrap();
        assert_eq!(damage_events(&conn, Duration::from_millis(300)), 0, "idle");
        x.fill(0x00ff00, BLOCK);
        x.fill(0x0000ff, BLOCK);
        assert_eq!(damage_events(&conn, Duration::from_millis(200)), 1);

        damage.subtract(&conn).unwrap();
        x.fill(0xffffff, BLOCK);
        assert_eq!(damage_events(&conn, Duration::from_millis(200)), 1);
    }

    /// The capture loop's model: `tilt-xevents` blocks in wait_for_event while `tilt-capture`
    /// subtracts and grabs on the same connection whenever damage is reported. With the
    /// subtract sent before the grab, every drawing either lands in a grab or re-arms a
    /// notification, so the last grab always shows the last drawing.
    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn subtract_then_grab_never_misses_the_last_drawing() {
        let x = Xvfb::start("320x240x24", &[]);
        let (conn, screen) = connect(&x.display).unwrap();
        let conn = Arc::new(conn);
        let damage = DamageTracker::new(&conn, conn.setup().roots[screen].root).unwrap();
        let mut grabber = Grabber::new(&conn, screen).unwrap();
        // Armed, as after the capture loop's first grab.
        damage.subtract(&conn).unwrap();

        let (events_tx, events) = mpsc::channel();
        let reader = Arc::clone(&conn);
        std::thread::spawn(move || {
            // Ends when the server goes away with the fixture.
            while let Ok(event) = reader.wait_for_event() {
                let _ = events_tx.send(classify(&event));
            }
        });

        // Another client fills the screen with colour after colour for 400 ms, as fast as the
        // server allows, and returns the last one.
        let drawer = {
            let display = x.display.clone();
            std::thread::spawn(move || {
                let (conn, screen) = connect(&display).unwrap();
                let root = conn.setup().roots[screen].root;
                let gc = conn.generate_id().unwrap();
                conn.create_gc(gc, root, &CreateGCAux::new()).unwrap();
                let screen_rect = Rectangle {
                    x: 0,
                    y: 0,
                    width: 320,
                    height: 240,
                };
                let end = Instant::now() + Duration::from_millis(400);
                let mut colour = 0;
                while Instant::now() < end {
                    colour += 1;
                    conn.change_gc(gc, &ChangeGCAux::new().foreground(colour))
                        .unwrap();
                    conn.poly_fill_rectangle(root, gc, &[screen_rect]).unwrap();
                    conn.get_input_focus().unwrap().reply().unwrap();
                }
                colour
            })
        };

        let (mut grabs, mut last) = (0, 0);
        loop {
            match events.recv_timeout(Duration::from_millis(500)) {
                Ok(XEventKind::Damage) => {
                    damage.subtract(&conn).unwrap();
                    grabber.grab(&conn).unwrap();
                    let p = &grabber.pixels()[..3];
                    last = u32::from_le_bytes([p[0], p[1], p[2], 0]);
                    grabs += 1;
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) if drawer.is_finished() => break,
                Err(RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("event reader stopped: {e}"),
            }
        }
        assert_eq!(last, drawer.join().unwrap(), "after {grabs} grabs");
        assert!(grabs > 2, "only {grabs} grabs");
    }

    #[test]
    #[ignore = "needs Xvfb and xrandr; run in tilt-dev with --ignored"]
    fn reports_xrandr_mode_switches() {
        let x = Xvfb::start("1920x1080x24", &[]);
        x.add_720p_mode();
        let (conn, screen) = connect(&x.display).unwrap();
        let damage = DamageTracker::new(&conn, conn.setup().roots[screen].root).unwrap();
        assert!(damage.has_randr());

        for (mode, size) in [("tilt-720p", (1280, 720)), ("1920x1080", (1920, 1080))] {
            x.set_mode(mode);
            let events = events_within(&conn, Duration::from_millis(300));
            let changes: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    Event::RandrScreenChangeNotify(e) => Some((e.width, e.height)),
                    _ => None,
                })
                .collect();
            // One xrandr call can bring several notifications, some with the size it passes
            // through on the way; the capture loop re-reads the geometry on each.
            assert_eq!(changes.last(), Some(&size), "{mode}: {changes:?}");
            let kinds: Vec<_> = events.iter().map(classify).collect();
            assert!(kinds.contains(&XEventKind::ScreenChange), "{mode}");
        }
    }
}
