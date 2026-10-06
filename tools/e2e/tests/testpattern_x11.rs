//! Drives tilt-testpattern on a private Xvfb with xdotool and checks both its event log and
//! the pixels on screen (root GetImage). The tests are `#[ignore]`d because they need Xvfb and
//! xdotool: run them in the tilt-dev image with `cargo test -- --include-ignored`.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tilt_e2e::{nearest_color, BACKGROUND};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, CreateWindowAux, ImageFormat, WindowClass};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT};

/// A private Xvfb for one test, so the tests can run in parallel; killed on drop.
struct Xvfb {
    child: Child,
    /// `:<number>`
    display: String,
}

impl Xvfb {
    fn start() -> Xvfb {
        // -displayfd: Xvfb takes the first free display and writes its number to stdout once
        // it accepts clients. -noreset keeps state when short-lived clients disconnect.
        let mut child = Command::new("Xvfb")
            .args(["-displayfd", "1", "-screen", "0", "1024x768x24"])
            .args(["-nolisten", "tcp", "-noreset"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("cannot run Xvfb");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("Xvfb stdout"))
            .read_line(&mut line)
            .expect("read Xvfb's display number");
        let Ok(number) = line.trim().parse::<u32>() else {
            let _ = child.kill();
            panic!("Xvfb did not start: {line:?}");
        };
        Xvfb {
            child,
            display: format!(":{number}"),
        }
    }

    fn xdotool(&self, args: &[&str]) {
        let status = Command::new("xdotool")
            .args(args)
            .env("DISPLAY", &self.display)
            .status()
            .expect("run xdotool");
        assert!(status.success(), "xdotool {args:?} failed");
    }

    fn connect(&self) -> (RustConnection, usize) {
        x11rb::connect(Some(&self.display)).expect("connect to Xvfb")
    }

    /// `width` root pixels of row `y` as RGB.
    fn root_row(&self, y: i16, width: u16) -> Vec<(u8, u8, u8)> {
        let (conn, screen) = self.connect();
        let root = conn.setup().roots[screen].root;
        let img = conn
            .get_image(ImageFormat::Z_PIXMAP, root, 0, y, width, 1, !0)
            .expect("GetImage")
            .reply()
            .expect("GetImage reply");
        // Depth 24 comes back as 32 bpp B, G, R, X (little-endian server).
        let (pixels, _) = img.data.as_chunks::<4>();
        pixels.iter().map(|&[b, g, r, _]| (r, g, b)).collect()
    }

    fn root_pixel(&self, x: u16, y: i16) -> (u8, u8, u8) {
        self.root_row(y, x + 1)[usize::from(x)]
    }

    fn wait_marker(&self, index: usize) {
        wait_until(&format!("marker colour {index}"), || {
            nearest_color(self.root_pixel(192, 192)) == Some(index)
        });
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A running tilt-testpattern, killed on drop.
struct Pattern {
    child: Child,
    log: PathBuf,
}

impl Pattern {
    fn start(x: &Xvfb, name: &str, extra: &[&str]) -> Pattern {
        let log = std::env::temp_dir().join(format!("tp-{name}-{}.log", std::process::id()));
        let child = Command::new(env!("CARGO_BIN_EXE_tilt-testpattern"))
            .arg("--log")
            .arg(&log)
            .args(extra)
            .env("DISPLAY", &x.display)
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn tilt-testpattern");
        let p = Pattern { child, log };
        p.wait_log(|l| l.contains(r#""ev":"ready""#), "ready line");
        p
    }

    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn wait_log(&self, pred: impl Fn(&str) -> bool, what: &str) -> String {
        let mut found = None;
        wait_until(what, || {
            found = self.lines().into_iter().find(|l| pred(l));
            found.is_some()
        });
        found.expect("found")
    }
}

impl Drop for Pattern {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.log);
    }
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "needs Xvfb and xdotool; run in tilt-dev with --include-ignored"]
fn marker_follows_keys_and_clicks() {
    let x = Xvfb::start();
    let p = Pattern::start(&x, "input", &[]);

    let first = &p.lines()[0];
    assert!(
        first.starts_with(r#"{"ms":"#) && first.contains(r#""ev":"ready","w":"#),
        "{first}"
    );
    // "ready" is only logged after a round trip, so the marker is already on screen.
    assert_eq!(nearest_color(x.root_pixel(192, 192)), Some(0));
    assert_eq!(x.root_pixel(10, 10), BACKGROUND);
    assert_eq!(x.root_pixel(330, 330), BACKGROUND);

    x.xdotool(&["key", "space"]);
    p.wait_log(
        |l| l.contains(r#""ev":"key_press""#) && l.contains(r#""keysym":32,"#),
        "space press",
    );
    p.wait_log(
        |l| l.contains(r#""ev":"key_release""#) && l.contains(r#""keysym":32,"#),
        "space release",
    );
    x.wait_marker(1);

    // Start elsewhere so the move to (100,100) is a real motion even on a reused display.
    x.xdotool(&["mousemove", "400", "400"]);
    // scripts/e2e.sh greps for exactly this button_press line after tilt-probe's click.
    x.xdotool(&["mousemove", "100", "100", "click", "1"]);
    p.wait_log(
        |l| l.contains(r#""ev":"motion","x":100,"y":100}"#),
        "motion",
    );
    p.wait_log(
        |l| l.contains(r#""ev":"button_press","button":1,"x":100,"y":100}"#),
        "click",
    );
    p.wait_log(
        |l| l.contains(r#""ev":"button_release","button":1,"x":100,"y":100}"#),
        "release",
    );
    x.wait_marker(2);

    // Other buttons are logged but leave the marker alone.
    x.xdotool(&["click", "3"]);
    p.wait_log(
        |l| l.contains(r#""ev":"button_press","button":3,"#),
        "right click",
    );
    thread::sleep(Duration::from_millis(100));
    assert_eq!(nearest_color(x.root_pixel(192, 192)), Some(2));

    // Shift_L is a key press of its own (marker 3); "a" under Shift resolves to "A" (marker 0).
    x.xdotool(&["key", "shift+a"]);
    p.wait_log(
        |l| l.contains(r#""ev":"key_press""#) && l.contains(r#""keysym":65505,"#),
        "Shift_L",
    );
    let a = p.wait_log(
        |l| l.contains(r#""ev":"key_press""#) && l.contains(r#""keysym":65,"#),
        "A",
    );
    assert!(
        a.ends_with(r#""state":1}"#),
        "Shift missing from state: {a}"
    );
    x.wait_marker(0);
}

#[test]
#[ignore = "needs Xvfb and xdotool; run in tilt-dev with --include-ignored"]
fn stays_above_windows_mapped_later() {
    let x = Xvfb::start();
    let _p = Pattern::start(&x, "raise", &[]);

    // A plain managed-style window over the marker, as a late panel or dialog would be.
    let (conn, screen) = x.connect();
    let root = conn.setup().roots[screen].root;
    let other = conn.generate_id().expect("id");
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        other,
        root,
        150,
        150,
        100,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        COPY_FROM_PARENT,
        &CreateWindowAux::new().background_pixel(0x00ff_ff00),
    )
    .expect("CreateWindow");
    conn.map_window(other).expect("MapWindow");
    conn.sync().expect("sync");

    x.wait_marker(0);
    conn.destroy_window(other).expect("DestroyWindow");
    conn.sync().expect("sync");
}

#[test]
#[ignore = "needs Xvfb and xdotool; run in tilt-dev with --include-ignored"]
fn animated_bar_moves() {
    let x = Xvfb::start();
    let _p = Pattern::start(&x, "animate", &["--animate"]);
    let width = {
        let (conn, screen) = x.connect();
        conn.setup().roots[screen].width_in_pixels
    };

    // White columns in the middle of the band y 400..560.
    let bar = || -> Vec<usize> {
        let row = x.root_row(480, width);
        (0..row.len())
            .filter(|&x| row[x] == (255, 255, 255))
            .collect()
    };
    let samples: Vec<Vec<usize>> = (0..10)
        .map(|_| {
            thread::sleep(Duration::from_millis(50));
            bar()
        })
        .collect();
    assert!(
        samples.iter().all(|s| !s.is_empty()),
        "bar missing: {samples:?}"
    );
    assert!(samples.windows(2).any(|w| w[0] != w[1]), "bar did not move");
    // A sample can land between "draw new bar" and "erase old strip" (72 px) or near the wrap
    // (clipped at the right edge), but most show the plain 64 px bar.
    let plain = samples
        .iter()
        .filter(|s| s.len() == 64 && s[63] - s[0] == 63)
        .count();
    assert!(
        plain >= 5,
        "bar widths {:?}",
        samples.iter().map(Vec::len).collect::<Vec<_>>()
    );
    // Outside the band the pattern is untouched.
    assert_eq!(x.root_pixel(5, 380), BACKGROUND);
    assert_eq!(nearest_color(x.root_pixel(192, 192)), Some(0));
}
