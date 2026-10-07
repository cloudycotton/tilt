//! The cursor thread `tilt-cursor`: XFixes shape and QueryPointer position on its own
//! connection, published through a watch channel.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::watch;
use tracing::error;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{ConnectionExt as _, CursorNotifyMask, GetCursorImageReply};
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use super::conn::connect;

/// Browsers ignore CSS cursor images larger than this.
const MAX_SIDE: u16 = 128;
/// Position polling while viewers watch and the pointer moved within MOVING_FOR.
const ACTIVE_POLL: Duration = Duration::from_millis(8);
/// While viewers watch a pointer that has been still for MOVING_FOR: the first move after a
/// rest shows up at most this late, and the rest of it at ACTIVE_POLL.
const RESTING_POLL: Duration = Duration::from_millis(50);
const MOVING_FOR: Duration = Duration::from_millis(500);
/// With no viewers.
const IDLE_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    /// XFixes cursor serial; it changes whenever the image does.
    pub serial: u32,
    pub width: u16,
    pub height: u16,
    pub xhot: u16,
    pub yhot: u16,
    /// `width * height * 4` bytes of straight-alpha RGBA, at most 128x128.
    pub rgba: Arc<Vec<u8>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CursorState {
    /// None until the first XFixes read.
    pub shape: Option<CursorShape>,
    pub x: i32,
    pub y: i32,
}

/// Connects to `display` and starts `tilt-cursor`. The position is polled every 8 ms while
/// `watching > 0` and the pointer moves, every 50 ms while it rests, and every 250 ms with
/// nobody watching; the returned receiver sees only changes.
pub fn spawn_cursor_thread(
    display: String,
    watching: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<(watch::Receiver<CursorState>, JoinHandle<()>)> {
    let (tx, rx) = watch::channel(CursorState::default());
    let (conn, screen) = connect(&display)?;
    let root = conn.setup().roots[screen].root;
    // XFIXES refuses other requests until the client has announced its version.
    conn.xfixes_query_version(5, 0)?.reply().context("XFIXES")?;
    conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)?
        .check()
        .context("XFixesSelectCursorInput")?;
    let thread = std::thread::Builder::new()
        .name("tilt-cursor".into())
        .spawn(move || {
            if let Err(e) = track(&conn, root, &tx, &watching, &shutdown) {
                error!("cursor tracking stopped: {e:#}");
            }
        })
        .context("cannot start tilt-cursor")?;
    Ok((rx, thread))
}

fn track(
    conn: &RustConnection,
    root: Window,
    tx: &watch::Sender<CursorState>,
    watching: &AtomicUsize,
    shutdown: &AtomicBool,
) -> anyhow::Result<()> {
    // Read after selecting CursorNotify, so no change can fall in between.
    let mut shape = Some(read_shape(conn)?);
    let mut moved_at = Instant::now();
    while !shutdown.load(Ordering::Relaxed) {
        let mut cursor_changed = false;
        while let Some(event) = conn.poll_for_event()? {
            cursor_changed |= matches!(event, Event::XfixesCursorNotify(_));
        }
        if cursor_changed {
            shape = Some(read_shape(conn)?);
        }
        let pointer = conn.query_pointer(root)?.reply()?;
        let now = Instant::now();
        if publish(
            tx,
            shape.take(),
            pointer.root_x.into(),
            pointer.root_y.into(),
        ) {
            moved_at = now;
        }
        std::thread::sleep(if watching.load(Ordering::Relaxed) == 0 {
            IDLE_POLL
        } else if now.saturating_duration_since(moved_at) < MOVING_FOR {
            ACTIVE_POLL
        } else {
            RESTING_POLL
        });
    }
    Ok(())
}

/// Stores a newly read shape and the position, waking receivers only if either changed;
/// returns whether one did.
fn publish(tx: &watch::Sender<CursorState>, shape: Option<CursorShape>, x: i32, y: i32) -> bool {
    tx.send_if_modified(|state| {
        let mut changed = false;
        if let Some(shape) = shape {
            if state.shape.as_ref() != Some(&shape) {
                state.shape = Some(shape);
                changed = true;
            }
        }
        if (state.x, state.y) != (x, y) {
            (state.x, state.y) = (x, y);
            changed = true;
        }
        changed
    })
}

fn read_shape(conn: &RustConnection) -> anyhow::Result<CursorShape> {
    let image = conn
        .xfixes_get_cursor_image()?
        .reply()
        .context("XFixesGetCursorImage")?;
    Ok(cursor_shape(&image))
}

/// Converts an XFixes cursor image to at most MAX_SIDE x MAX_SIDE of straight-alpha RGBA. A
/// fully transparent cursor, which is how applications hide the pointer, becomes the 0x0
/// shape that the protocol reads as hidden.
fn cursor_shape(image: &GetCursorImageReply) -> CursorShape {
    let serial = image.cursor_serial;
    if image.cursor_image.iter().all(|&argb| argb >> 24 == 0) {
        return CursorShape {
            serial,
            width: 0,
            height: 0,
            xhot: 0,
            yhot: 0,
            rgba: Arc::default(),
        };
    }
    let (x0, width, xhot) = crop(image.width, image.xhot);
    let (y0, height, yhot) = crop(image.height, image.yhot);
    let mut rgba = Vec::with_capacity(usize::from(width) * usize::from(height) * 4);
    let rows = image.cursor_image.chunks_exact(image.width.into());
    for row in rows.skip(y0.into()).take(height.into()) {
        for &argb in &row[usize::from(x0)..][..usize::from(width)] {
            rgba.extend_from_slice(&unpremultiply(argb));
        }
    }
    CursorShape {
        serial,
        width,
        height,
        xhot,
        yhot,
        rgba: Arc::new(rgba),
    }
}

/// The span kept along one axis of `len` pixels with the hotspot at `hot`, as (start, length,
/// hotspot within the span): everything up to MAX_SIDE, otherwise MAX_SIDE pixels from the
/// start, moved along just enough to keep the hotspot.
fn crop(len: u16, hot: u16) -> (u16, u16, u16) {
    let hot = hot.min(len.saturating_sub(1));
    if len <= MAX_SIDE {
        return (0, len, hot);
    }
    let start = hot.saturating_sub(MAX_SIDE - 1);
    (start, MAX_SIDE, hot - start)
}

/// Render's premultiplied ARGB32 to straight-alpha R, G, B, A bytes.
fn unpremultiply(argb: u32) -> [u8; 4] {
    let [b, g, r, a] = argb.to_le_bytes();
    if a == 0 {
        return [0; 4];
    }
    let a32 = u32::from(a);
    // Rounded; a colour above its alpha is malformed input and saturates.
    let straight = |c: u8| ((u32::from(c) * 255 + a32 / 2) / a32).min(255) as u8;
    [straight(r), straight(g), straight(b), a]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u16, height: u16, xhot: u16, yhot: u16, argb: Vec<u32>) -> GetCursorImageReply {
        GetCursorImageReply {
            sequence: 0,
            length: 0,
            x: 0,
            y: 0,
            width,
            height,
            xhot,
            yhot,
            cursor_serial: 7,
            cursor_image: argb,
        }
    }

    #[test]
    fn unpremultiplies_argb() {
        assert_eq!(unpremultiply(0xff11_2233), [0x11, 0x22, 0x33, 0xff]);
        assert_eq!(unpremultiply(0x8040_2010), [0x80, 0x40, 0x20, 0x80]);
        assert_eq!(unpremultiply(0x0000_0000), [0, 0, 0, 0]);
        assert_eq!(unpremultiply(0x10ff_0000), [0xff, 0, 0, 0x10]);
    }

    #[test]
    fn keeps_small_cursors_whole() {
        let shape = cursor_shape(&image(2, 1, 1, 0, vec![0xff00_00ff, 0x8000_0080]));
        let expected = CursorShape {
            serial: 7,
            width: 2,
            height: 1,
            xhot: 1,
            yhot: 0,
            rgba: Arc::new(vec![0, 0, 0xff, 0xff, 0, 0, 0xff, 0x80]),
        };
        assert_eq!(shape, expected);
    }

    #[test]
    fn crops_large_cursors_to_keep_the_hotspot() {
        // 200x150, opaque, with each pixel's x in red and y in green.
        let argb = (0..150u32)
            .flat_map(|y| (0..200u32).map(move |x| 0xff00_0000 | x << 16 | y << 8))
            .collect();
        let shape = cursor_shape(&image(200, 150, 180, 20, argb));
        assert_eq!((shape.width, shape.height), (128, 128));
        // Across, the hotspot is past the first 128 columns, so the span ends at it; down, the
        // span starts at the top.
        assert_eq!((shape.xhot, shape.yhot), (127, 20));
        assert_eq!(shape.rgba.len(), 128 * 128 * 4);
        let at = |x: usize, y: usize| &shape.rgba[(y * 128 + x) * 4..][..4];
        assert_eq!(at(0, 0), [53, 0, 0, 0xff]);
        assert_eq!(at(127, 127), [180, 127, 0, 0xff]);
    }

    #[test]
    fn clamps_a_hotspot_outside_the_image() {
        let shape = cursor_shape(&image(2, 2, 9, 9, vec![0xff00_0000; 4]));
        assert_eq!((shape.xhot, shape.yhot), (1, 1));
    }

    #[test]
    fn reports_a_fully_transparent_cursor_as_hidden() {
        let shape = cursor_shape(&image(16, 16, 8, 8, vec![0x00ff_ffff; 256]));
        let hidden = CursorShape {
            serial: 7,
            width: 0,
            height: 0,
            xhot: 0,
            yhot: 0,
            rgba: Arc::default(),
        };
        assert_eq!(shape, hidden);
    }

    fn position(rx: &mut watch::Receiver<CursorState>) -> (i32, i32) {
        let state = rx.borrow_and_update();
        (state.x, state.y)
    }

    #[test]
    fn publishes_only_changes() {
        let (tx, mut rx) = watch::channel(CursorState::default());
        assert!(!publish(&tx, None, 0, 0));
        assert!(!rx.has_changed().unwrap());
        assert!(publish(&tx, None, 3, 4));
        assert!(rx.has_changed().unwrap());
        assert_eq!(position(&mut rx), (3, 4));

        let shape = cursor_shape(&image(1, 1, 0, 0, vec![0xff00_0000]));
        assert!(publish(&tx, Some(shape.clone()), 3, 4));
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().shape.as_ref(), Some(&shape));
        // CursorNotify fires on every cursor switch, including back to the shape last sent.
        assert!(!publish(&tx, Some(shape), 3, 4));
        assert!(!rx.has_changed().unwrap());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod x_tests {
    use std::time::Instant;

    use x11rb::protocol::xproto::{ChangeWindowAttributesAux, CreateGCAux, Rectangle};

    use super::*;
    use crate::x11::testutil::{eventually, Xvfb};

    fn position(rx: &watch::Receiver<CursorState>) -> (i32, i32) {
        // One borrow: holding two would take the channel's read lock twice.
        let state = rx.borrow();
        (state.x, state.y)
    }

    /// Moves the pointer with xdotool, which goes through XTest like tilt's input thread, and
    /// returns the median time until a move is published; the median shrugs off the stalls of
    /// a loaded machine.
    fn median_lag(x: &Xvfb, rx: &watch::Receiver<CursorState>, start: i32) -> Duration {
        let mut lags: Vec<_> = (1..=9)
            .map(|i| {
                let (px, py) = (start + 20 * i, start + 10 * i);
                x.run("xdotool", &["mousemove", &px.to_string(), &py.to_string()]);
                let moved = Instant::now();
                assert!(eventually(Duration::from_secs(5), || position(rx) == (px, py)));
                moved.elapsed()
            })
            .collect();
        lags.sort();
        lags[lags.len() / 2]
    }

    #[test]
    #[ignore = "needs Xvfb and xdotool; run in tilt-dev with --ignored"]
    fn follows_the_pointer_and_the_cursor_shape() {
        let x = Xvfb::start("640x480x24", &[]);
        let viewers = Arc::new(AtomicUsize::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (mut rx, thread) = spawn_cursor_thread(
            x.display.clone(),
            Arc::clone(&viewers),
            Arc::clone(&shutdown),
        )
        .unwrap();
        assert_eq!(thread.thread().name(), Some("tilt-cursor"));
        let wait = Duration::from_secs(2);

        // The initial read: Xvfb's root cursor, the X from the cursor font.
        assert!(eventually(wait, || rx.borrow().shape.is_some()));
        let initial = rx.borrow_and_update().shape.clone().unwrap();
        assert!(initial.width > 0 && initial.height > 0, "{initial:?}");
        assert_eq!(
            initial.rgba.len(),
            usize::from(initial.width) * usize::from(initial.height) * 4
        );

        let active = median_lag(&x, &rx, 0);

        // Nothing changes, so nothing is published.
        rx.borrow_and_update();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!rx.has_changed().unwrap(), "published without a change");

        // Another client puts the left_ptr arrow (cursor font glyph 68, mask 69) on the root.
        let (client, screen) = connect(&x.display).unwrap();
        let root = client.setup().roots[screen].root;
        let font = client.generate_id().unwrap();
        client.open_font(font, b"cursor").unwrap();
        let arrow = client.generate_id().unwrap();
        client
            .create_glyph_cursor(arrow, font, font, 68, 69, 0, 0, 0, 0xffff, 0xffff, 0xffff)
            .unwrap();
        let set_cursor = |cursor| {
            client
                .change_window_attributes(root, &ChangeWindowAttributesAux::new().cursor(cursor))
                .unwrap();
            client.get_input_focus().unwrap().reply().unwrap();
        };
        set_cursor(arrow);
        assert!(eventually(wait, || {
            rx.borrow().shape.as_ref().map(|s| s.serial) != Some(initial.serial)
        }));
        let shape = rx.borrow_and_update().shape.clone().unwrap();
        assert!(shape.width > 0 && shape.rgba != initial.rgba, "{shape:?}");

        // An all-transparent cursor, as applications use to hide the pointer.
        let blank = client.generate_id().unwrap();
        let pixmap = client.generate_id().unwrap();
        client.create_pixmap(1, pixmap, root, 1, 1).unwrap();
        let gc = client.generate_id().unwrap();
        client
            .create_gc(gc, pixmap, &CreateGCAux::new().foreground(0))
            .unwrap();
        let dot = Rectangle {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        client.poly_fill_rectangle(pixmap, gc, &[dot]).unwrap();
        client
            .create_cursor(blank, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0)
            .unwrap();
        set_cursor(blank);
        assert!(eventually(wait, || {
            rx.borrow()
                .shape
                .as_ref()
                .is_some_and(|s| s.serial != shape.serial)
        }));
        let hidden = rx.borrow().shape.clone().unwrap();
        assert_eq!((hidden.width, hidden.height), (0, 0), "{hidden:?}");
        assert!(hidden.rgba.is_empty());

        // Polling every 8 ms with a viewer and every 250 ms without; as each move follows a
        // publish, its lag is close to a whole interval. Compared rather than held to fixed
        // bounds, as a loaded machine slows both alike.
        viewers.store(0, Ordering::Relaxed);
        let idle = median_lag(&x, &rx, 200);
        assert!(active * 2 < idle, "active {active:?}, idle {idle:?}");

        shutdown.store(true, Ordering::Relaxed);
        let stopping = Instant::now();
        thread.join().unwrap();
        assert!(
            stopping.elapsed() < Duration::from_secs(1),
            "{:?}",
            stopping.elapsed()
        );
    }
}
