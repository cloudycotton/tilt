//! tilt-testpattern: a full-screen X11 test pattern for end-to-end checks (brief section 7).
//! A 256x256 marker changes colour on every key press and left click, `--animate` adds a bar
//! that moves at 60 Hz, and every input event is logged as one JSON line.

use std::fs::File;
use std::io::{self, LineWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use clap::Parser;
use tilt_e2e::{BACKGROUND, COLORS, MARKER_ORIGIN, MARKER_SIZE};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeGCAux, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _,
    CreateGCAux, CreateWindowAux, EventMask, Gcontext, InputFocus, KeyButMask, Keycode, Mapping,
    NotifyDetail, NotifyMode, PropMode, Rectangle, Screen, StackMode, VisualClass, Window,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};

#[derive(Parser, Debug)]
#[command(
    name = "tilt-testpattern",
    version,
    about = "Full-screen X11 test pattern with an input-driven marker and a JSON event log"
)]
struct Args {
    /// Move a white bar 8 px per 60 Hz tick, for frame-rate tests
    #[arg(long)]
    animate: bool,

    /// Event log (JSON lines, also echoed to stdout)
    #[arg(long, default_value = "/tmp/testpattern.log")]
    log: PathBuf,

    /// X display
    #[arg(long, env = "DISPLAY", default_value = ":0")]
    display: String,
}

/// The `--animate` bar: 64 px wide in the band y 400..560, moving 8 px per tick and wrapping.
const BAR_TOP: i16 = 400;
const BAR_HEIGHT: u16 = 160;
const BAR_WIDTH: u16 = 64;
const BAR_STEP: u16 = 8;
const TICK: Duration = Duration::from_micros(16_667);
const BAR_COLOR: (u8, u8, u8) = (255, 255, 255);

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let (conn, screen_num) = connect(&args.display)?;
    let conn = Arc::new(conn);
    let screen = conn.setup().roots[screen_num].clone();
    let pixel = pixel_format(&screen)?;
    let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);

    let win = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win,
        screen.root,
        0,
        0,
        width,
        height,
        0,
        WindowClass::INPUT_OUTPUT,
        COPY_FROM_PARENT,
        &CreateWindowAux::new()
            .background_pixel(pixel.of(BACKGROUND))
            // Override-redirect: the window manager must not frame, move or stack it.
            .override_redirect(1)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::FOCUS_CHANGE
                    | EventMask::STRUCTURE_NOTIFY,
            ),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"tilt-testpattern",
    )?;
    let marker_gc = new_gc(&conn, win)?;
    // Hear about every top-level window that maps after us (a slow panel, a dialog), so the
    // pattern can raise itself back over it.
    conn.change_window_attributes(
        screen.root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
    )?;
    conn.map_window(win)?;
    conn.flush()?;

    let mut log = EventLog::open(&args.log)?;
    let mut keymap = Keymap::fetch(&conn)?;
    let mut counter: u32 = 0;
    let mut ready = false;

    if args.animate {
        let gc = new_gc(&conn, win)?;
        let (conn, bar) = (conn.clone(), pixel.of(BAR_COLOR));
        let bg = pixel.of(BACKGROUND);
        thread::Builder::new()
            .name("animate".into())
            .spawn(move || {
                if let Err(e) = animate(&conn, win, gc, width, bar, bg) {
                    eprintln!("tilt-testpattern: animation stopped: {e:#}");
                }
            })?;
    }

    loop {
        let event = conn.wait_for_event().context("X connection lost")?;
        match event {
            // Focus is only settable once the window is viewable, i.e. after MapNotify.
            Event::MapNotify(e) if e.window == win => focus(&conn, win)?,
            // Raising only on maps, not periodically: a no-op restack still makes a compositing
            // window manager repaint, and that damage would break idle-stream checks.
            Event::MapNotify(_) => {
                conn.configure_window(
                    win,
                    &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                )?;
            }
            Event::Expose(e) if e.count == 0 => {
                draw_marker(
                    &conn,
                    win,
                    marker_gc,
                    pixel.of(COLORS[counter as usize % 4]),
                )?;
                if !ready {
                    // A round trip, so "ready" means the server has drawn the marker and set
                    // the focus: harnesses may inject input as soon as they read this line.
                    conn.get_input_focus()?.reply()?;
                    ready = true;
                    log.write(format_args!(r#""ev":"ready","w":{width},"h":{height}"#))?;
                }
            }
            Event::KeyPress(e) => {
                let keysym = keymap.keysym(e.detail, e.state.contains(KeyButMask::SHIFT));
                let state = u16::from(e.state);
                log.write(format_args!(
                    r#""ev":"key_press","keycode":{},"keysym":{keysym},"state":{state}"#,
                    e.detail
                ))?;
                counter = counter.wrapping_add(1);
                draw_marker(
                    &conn,
                    win,
                    marker_gc,
                    pixel.of(COLORS[counter as usize % 4]),
                )?;
            }
            Event::KeyRelease(e) => {
                let keysym = keymap.keysym(e.detail, e.state.contains(KeyButMask::SHIFT));
                let state = u16::from(e.state);
                log.write(format_args!(
                    r#""ev":"key_release","keycode":{},"keysym":{keysym},"state":{state}"#,
                    e.detail
                ))?;
            }
            Event::ButtonPress(e) => {
                log.write(format_args!(
                    r#""ev":"button_press","button":{},"x":{},"y":{}"#,
                    e.detail, e.root_x, e.root_y
                ))?;
                focus(&conn, win)?;
                if e.detail == 1 {
                    counter = counter.wrapping_add(1);
                    draw_marker(
                        &conn,
                        win,
                        marker_gc,
                        pixel.of(COLORS[counter as usize % 4]),
                    )?;
                }
            }
            Event::ButtonRelease(e) => {
                log.write(format_args!(
                    r#""ev":"button_release","button":{},"x":{},"y":{}"#,
                    e.detail, e.root_x, e.root_y
                ))?;
            }
            Event::MotionNotify(e) => {
                log.write(format_args!(
                    r#""ev":"motion","x":{},"y":{}"#,
                    e.root_x, e.root_y
                ))?;
            }
            // A window manager or a newly mapped app took the focus away: take it back, so
            // injected keys keep reaching the marker. Grab-related changes undo themselves.
            Event::FocusOut(e)
                if e.mode == NotifyMode::NORMAL
                    && e.detail != NotifyDetail::INFERIOR
                    && e.detail != NotifyDetail::POINTER =>
            {
                log.write(format_args!(r#""ev":"refocus""#))?;
                focus(&conn, win)?;
            }
            // tilt binds keysyms to spare keycodes on the fly; refresh so the log stays right.
            Event::MappingNotify(e) if e.request == Mapping::KEYBOARD => {
                keymap = Keymap::fetch(&conn)?;
            }
            Event::Error(e) => eprintln!("tilt-testpattern: X error: {e:?}"),
            _ => {}
        }
        conn.flush()?;
    }
}

/// Opens `display`; on Linux falls back to Xvfb's abstract socket `@/tmp/.X11-unix/XN`, which
/// x11rb does not try and which is the only socket under `-nolisten unix` (E2B's flags).
fn connect(display: &str) -> anyhow::Result<(RustConnection, usize)> {
    let err = match x11rb::connect(Some(display)) {
        Ok(c) => return Ok(c),
        Err(e) => e,
    };
    #[cfg(target_os = "linux")]
    if let Some(n) = display_number(display) {
        return connect_abstract(n).with_context(|| {
            format!(
                "connecting to {display}: {err}; abstract socket @/tmp/.X11-unix/X{n} also failed"
            )
        });
    }
    Err(err).with_context(|| format!("connecting to {display}"))
}

/// N for ":N", ":N.S", "unix:N" and "unix:N.S".
#[cfg(target_os = "linux")]
fn display_number(display: &str) -> Option<u16> {
    let rest = display
        .strip_prefix("unix:")
        .or_else(|| display.strip_prefix(':'))?;
    rest.split('.').next()?.parse().ok()
}

#[cfg(target_os = "linux")]
fn connect_abstract(display: u16) -> anyhow::Result<(RustConnection, usize)> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixStream};
    use x11rb::reexports::x11rb_protocol::xauth::get_auth;
    use x11rb::rust_connection::DefaultStream;

    let addr = SocketAddr::from_abstract_name(format!("/tmp/.X11-unix/X{display}"))?;
    let (stream, (family, address)) =
        DefaultStream::from_unix_stream(UnixStream::connect_addr(&addr)?)?;
    let (name, data) = get_auth(family, &address, display)
        .ok()
        .flatten()
        .unwrap_or_default();
    let conn = RustConnection::connect_to_stream_with_auth_info(stream, 0, name, data)?;
    Ok((conn, 0))
}

/// How an 8-bit RGB triple becomes a pixel value in the root visual.
#[derive(Clone, Copy)]
struct PixelFormat {
    masks: [u32; 3],
}

impl PixelFormat {
    fn of(&self, (r, g, b): (u8, u8, u8)) -> u32 {
        let channel = |v: u8, mask: u32| {
            let bits = mask.count_ones().min(8);
            (u32::from(v) >> (8 - bits)) << mask.trailing_zeros()
        };
        channel(r, self.masks[0]) | channel(g, self.masks[1]) | channel(b, self.masks[2])
    }
}

fn pixel_format(screen: &Screen) -> anyhow::Result<PixelFormat> {
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == screen.root_visual)
        .context("root visual not found")?;
    if visual.class != VisualClass::TRUE_COLOR {
        bail!("root visual is {:?}, need TrueColor", visual.class);
    }
    Ok(PixelFormat {
        masks: [visual.red_mask, visual.green_mask, visual.blue_mask],
    })
}

fn new_gc(conn: &RustConnection, win: Window) -> anyhow::Result<Gcontext> {
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))?;
    Ok(gc)
}

fn fill(
    conn: &RustConnection,
    win: Window,
    gc: Gcontext,
    color: u32,
    r: Rectangle,
) -> anyhow::Result<()> {
    conn.change_gc(gc, &ChangeGCAux::new().foreground(color))?;
    conn.poly_fill_rectangle(win, gc, &[r])?;
    Ok(())
}

fn draw_marker(conn: &RustConnection, win: Window, gc: Gcontext, color: u32) -> anyhow::Result<()> {
    let r = Rectangle {
        x: MARKER_ORIGIN,
        y: MARKER_ORIGIN,
        width: MARKER_SIZE,
        height: MARKER_SIZE,
    };
    fill(conn, win, gc, color, r)
}

fn focus(conn: &RustConnection, win: Window) -> anyhow::Result<()> {
    conn.set_input_focus(InputFocus::POINTER_ROOT, win, CURRENT_TIME)?;
    Ok(())
}

/// Moves the bar one step per 60 Hz tick. The new bar is drawn before the uncovered strip of
/// the old one is erased, so a frame grabbed between the two requests never misses the bar.
fn animate(
    conn: &RustConnection,
    win: Window,
    gc: Gcontext,
    width: u16,
    bar: u32,
    bg: u32,
) -> anyhow::Result<()> {
    let rect = |x: u16, w: u16| Rectangle {
        x: x as i16,
        y: BAR_TOP,
        width: w,
        height: BAR_HEIGHT,
    };
    let mut x: u16 = 0;
    let mut next = Instant::now();
    loop {
        let old = x;
        x = (x + BAR_STEP) % width;
        fill(conn, win, gc, bar, rect(x, BAR_WIDTH))?;
        if x > old && x - old < BAR_WIDTH {
            fill(conn, win, gc, bg, rect(old, x - old))?;
        } else {
            fill(conn, win, gc, bg, rect(old, BAR_WIDTH))?;
        }
        conn.flush()?;

        next += TICK;
        let now = Instant::now();
        match next.checked_duration_since(now) {
            Some(wait) => thread::sleep(wait),
            // Fell behind (stopped VM, overloaded host): restart the schedule, don't burst.
            None => next = now,
        }
    }
}

/// The core keyboard mapping, for resolving logged keycodes to keysyms.
struct Keymap {
    min_keycode: Keycode,
    per_keycode: usize,
    keysyms: Vec<u32>,
}

impl Keymap {
    fn fetch(conn: &RustConnection) -> anyhow::Result<Keymap> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        Ok(Keymap {
            min_keycode: min,
            per_keycode: usize::from(reply.keysyms_per_keycode),
            keysyms: reply.keysyms,
        })
    }

    /// Level 1 when Shift is held (falling back to level 0 when level 1 is NoSymbol, as Xlib
    /// does), else level 0. 0 when the keycode has no keysym.
    fn keysym(&self, keycode: Keycode, shift: bool) -> u32 {
        let Some(index) = keycode.checked_sub(self.min_keycode) else {
            return 0;
        };
        let start = usize::from(index) * self.per_keycode;
        let Some(syms) = self.keysyms.get(start..start + self.per_keycode) else {
            return 0;
        };
        let level0 = syms.first().copied().unwrap_or(0);
        match syms.get(1).copied() {
            Some(level1) if shift && level1 != NONE => level1,
            _ => level0,
        }
    }
}

/// JSON-lines event log, mirrored to stdout; every line carries a monotonic `ms` stamp.
struct EventLog {
    file: LineWriter<File>,
    start: Instant,
}

impl EventLog {
    fn open(path: &Path) -> anyhow::Result<EventLog> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        Ok(EventLog {
            file: LineWriter::new(file),
            start: Instant::now(),
        })
    }

    /// Writes `{"ms":<ms>,<fields>}`.
    fn write(&mut self, fields: std::fmt::Arguments) -> anyhow::Result<()> {
        let line = format!(r#"{{"ms":{},{fields}}}"#, self.start.elapsed().as_millis());
        writeln!(self.file, "{line}")?;
        // The stdout copy is for `docker logs`; a closed stdout must not stop the pattern.
        let _ = writeln!(io::stdout(), "{line}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_values_follow_the_visual_masks() {
        let rgb888 = PixelFormat {
            masks: [0xff0000, 0x00ff00, 0x0000ff],
        };
        assert_eq!(rgb888.of((0x20, 0x20, 0x20)), 0x202020);
        assert_eq!(rgb888.of((230, 40, 40)), 0xe62828);
        let rgb565 = PixelFormat {
            masks: [0xf800, 0x07e0, 0x001f],
        };
        assert_eq!(rgb565.of((255, 255, 255)), 0xffff);
        assert_eq!(rgb565.of((255, 0, 0)), 0xf800);
    }

    #[test]
    fn keysym_levels() {
        // keycode 38: a/A; keycode 65: space with an empty level 1.
        let mut keysyms = vec![0; 2 * 4 * (66 - 8)];
        let at = |kc: usize, level: usize| (kc - 8) * 4 + level;
        keysyms[at(38, 0)] = 0x61;
        keysyms[at(38, 1)] = 0x41;
        keysyms[at(65, 0)] = 0x20;
        let map = Keymap {
            min_keycode: 8,
            per_keycode: 4,
            keysyms,
        };
        assert_eq!(map.keysym(38, false), 0x61);
        assert_eq!(map.keysym(38, true), 0x41);
        assert_eq!(map.keysym(65, true), 0x20);
        assert_eq!(map.keysym(7, false), 0);
        assert_eq!(map.keysym(255, false), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn display_numbers() {
        assert_eq!(display_number(":0"), Some(0));
        assert_eq!(display_number(":12.0"), Some(12));
        assert_eq!(display_number("unix:3"), Some(3));
        assert_eq!(display_number("localhost:0"), None);
        assert_eq!(display_number(":x"), None);
    }
}
