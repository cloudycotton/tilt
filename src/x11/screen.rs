//! Resizing the screen with RandR 1.2, for `--resize`: a viewer asks for its window's size and the
//! screen takes that shape. Xvfb switches only between sizes that fit in its startup screen, so a
//! server meant to follow viewers starts large (`-screen 0 2560x1600x24`).

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::randr::{self, ConnectionExt as _, ModeInfo};
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

/// Modes tilt creates are named `tilt-WxH`, so it removes only its own.
const MODE_PREFIX: &str = "tilt-";
/// The encoder takes nothing smaller (see `ErrorCode::UnsupportedSize`).
const MIN_SIDE: u32 = 16;
/// Millimetres per pixel at 96 DPI, so fonts keep their size whatever the screen's.
const MM_PER_PX: f64 = 25.4 / 96.0;

/// The screen size for a viewer whose window is `want` (CSS pixels): `want` scaled up until it
/// is at least `min` on both sides, then scaled down into `max`, keeping its shape, with even
/// sides as the encoder codes them.
pub fn fit(want: (u32, u32), min: (u32, u32), max: (u32, u32)) -> (u16, u16) {
    let (w, h) = (f64::from(want.0.max(1)), f64::from(want.1.max(1)));
    let up = 1f64.max(f64::from(min.0) / w).max(f64::from(min.1) / h);
    let (w, h) = (w * up, h * up);
    let down = 1f64.min(f64::from(max.0) / w).min(f64::from(max.1) / h);
    let side = |v: f64, limit: u32| {
        let v = (v * down).floor() as u32;
        (v.clamp(MIN_SIDE, limit.max(MIN_SIDE)) & !1) as u16
    };
    (side(w, max.0), side(h, max.1))
}

/// The largest screen the server can take.
pub fn max_size(conn: &RustConnection, root: Window) -> Result<(u32, u32), ReplyError> {
    let range = conn.randr_get_screen_size_range(root)?.reply()?;
    Ok((u32::from(range.max_width), u32::from(range.max_height)))
}

/// Sets the screen to `size` on a server with one output in use, as Xvfb has; returns false and
/// changes nothing on another server or when the screen already has that size.
pub fn set_size(
    conn: &RustConnection,
    root: Window,
    (width, height): (u16, u16),
) -> Result<bool, ReplyError> {
    let res = conn.randr_get_screen_resources_current(root)?.reply()?;
    let mut active = Vec::new();
    for &output in &res.outputs {
        let info = conn
            .randr_get_output_info(output, res.config_timestamp)?
            .reply()?;
        if info.crtc != x11rb::NONE {
            active.push((output, info));
        }
    }
    let [(output, info)] = active.as_slice() else {
        tracing::warn!(
            "resize: {} outputs in use; tilt resizes a screen with exactly one",
            active.len()
        );
        return Ok(false);
    };
    let crtc = info.crtc;
    let geometry = conn.get_geometry(root)?.reply()?;
    let current = (geometry.width, geometry.height);
    if current == (width, height) {
        return Ok(false);
    }

    let names = mode_names(&res.modes, &res.names);
    let name = format!("{MODE_PREFIX}{width}x{height}");
    let mode = match names.iter().find(|(_, n)| *n == name) {
        Some(&(mode, _)) => mode,
        None => {
            let info = mode_info(width, height, name.len());
            conn.randr_create_mode(root, info, name.as_bytes())?
                .reply()?
                .mode
        }
    };
    if !info.modes.contains(&mode) {
        conn.randr_add_output_mode(*output, mode)?.check()?;
    }

    // The screen must hold the CRTC at every step: grow it first, shrink it last.
    let mm = |px: u16| (f64::from(px) * MM_PER_PX).round() as u32;
    let both = (current.0.max(width), current.1.max(height));
    if both != current {
        conn.randr_set_screen_size(root, both.0, both.1, mm(both.0), mm(both.1))?
            .check()?;
    }
    conn.randr_set_crtc_config(
        crtc,
        x11rb::CURRENT_TIME,
        res.config_timestamp,
        0,
        0,
        mode,
        randr::Rotation::ROTATE0,
        &[*output],
    )?
    .reply()?;
    if both != (width, height) {
        conn.randr_set_screen_size(root, width, height, mm(width), mm(height))?
            .check()?;
    }

    for &(old, ref n) in &names {
        if old != mode && n.starts_with(MODE_PREFIX) {
            conn.randr_delete_output_mode(*output, old)?.check()?;
            conn.randr_destroy_mode(old)?.check()?;
        }
    }
    conn.flush()?;
    Ok(true)
}

/// Each mode's id with its name, which `names` holds back to back.
fn mode_names(modes: &[ModeInfo], names: &[u8]) -> Vec<(randr::Mode, String)> {
    let mut at = 0;
    modes
        .iter()
        .map(|m| {
            let end = (at + usize::from(m.name_len)).min(names.len());
            let name = String::from_utf8_lossy(&names[at..end]).into_owned();
            at = end;
            (m.id, name)
        })
        .collect()
}

/// A 60 Hz mode without blanking: Xvfb has no monitor that would care.
fn mode_info(width: u16, height: u16, name_len: usize) -> ModeInfo {
    ModeInfo {
        id: 0,
        width,
        height,
        dot_clock: u32::from(width) * u32::from(height) * 60,
        hsync_start: width,
        hsync_end: width,
        htotal: width,
        hskew: 0,
        vsync_start: height,
        vsync_end: height,
        vtotal: height,
        name_len: name_len as u16,
        mode_flags: randr::ModeFlag::from(0u32),
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    const MIN: (u32, u32) = (1024, 720);
    const MAX: (u32, u32) = (2560, 1600);

    #[test]
    fn a_large_window_keeps_its_size() {
        assert_eq!(fit((1600, 900), MIN, MAX), (1600, 900));
    }

    #[test]
    fn a_narrow_panel_grows_to_the_minimum_width() {
        assert_eq!(fit((600, 900), MIN, MAX), (1024, 1536));
    }

    #[test]
    fn a_short_wide_panel_grows_to_the_minimum_height() {
        assert_eq!(fit((1100, 500), MIN, MAX), (1584, 720));
    }

    #[test]
    fn a_phone_shrinks_back_into_the_largest_screen() {
        assert_eq!(fit((390, 844), MIN, MAX), (738, 1600));
    }

    #[test]
    fn a_small_server_keeps_the_shape() {
        assert_eq!(fit((600, 900), MIN, (1280, 720)), (480, 720));
    }

    #[test]
    fn sides_are_even() {
        let (w, h) = fit((1001, 777), (0, 0), MAX);
        assert_eq!((w % 2, h % 2), (0, 0));
    }

    #[test]
    fn a_zero_window_is_the_minimum() {
        assert_eq!(fit((0, 0), MIN, MAX), (1024, 1024));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod x_tests {
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::ConnectionExt as _;

    use super::{max_size, set_size, MODE_PREFIX};
    use crate::x11::conn::connect;
    use crate::x11::testutil::Xvfb;

    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn resizes_within_the_startup_screen_and_keeps_one_mode() {
        let x = Xvfb::start("2560x1600x24", &[]);
        let (conn, screen) = connect(&x.display).unwrap();
        let root = conn.setup().roots[screen].root;
        conn.randr_query_version(1, 2).unwrap().reply().unwrap();
        assert_eq!(max_size(&conn, root).unwrap(), (2560, 1600));

        for size in [(1024, 1536), (1600, 900), (1280, 720)] {
            assert!(set_size(&conn, root, size).unwrap());
            let geometry = conn.get_geometry(root).unwrap().reply().unwrap();
            assert_eq!((geometry.width, geometry.height), size);
        }
        assert!(!set_size(&conn, root, (1280, 720)).unwrap());

        let res = conn
            .randr_get_screen_resources_current(root)
            .unwrap()
            .reply()
            .unwrap();
        let names = String::from_utf8_lossy(&res.names).into_owned();
        assert_eq!(names.matches(MODE_PREFIX).count(), 1, "{names}");
    }
}
