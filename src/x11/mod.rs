//! X11 access over x11rb's pure-Rust connection.

pub mod conn;
pub mod cursor;
pub mod damage;
pub mod grab;

/// Fixtures for the X tests, which are `#[ignore]`d because they need `Xvfb` and the X client
/// tools (xsetroot, xrandr, xdotool) installed: run them in the tilt-dev image with
/// `cargo test -- --ignored`.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod testutil {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateGCAux, Rectangle};
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;

    use super::conn::connect;

    /// A private Xvfb for one test, so tests can run in parallel and change the screen freely.
    pub(crate) struct Xvfb {
        child: Child,
        /// `:<number>`
        pub(crate) display: String,
    }

    impl Xvfb {
        /// `screen` is Xvfb's `WxHxD`; `extra` are further server arguments.
        pub(crate) fn start(screen: &str, extra: &[&str]) -> Xvfb {
            // -displayfd: Xvfb takes the first free display and writes its number to stdout
            // once it accepts clients. -noreset keeps state when short-lived clients like
            // xsetroot are the last to disconnect.
            let mut child = Command::new("Xvfb")
                .args(["-displayfd", "1", "-screen", "0", screen])
                .args(["-nolisten", "tcp", "-noreset"])
                .args(extra)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("cannot run Xvfb");
            let mut line = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let Ok(number) = line.trim().parse::<u32>() else {
                let _ = child.kill();
                panic!("Xvfb did not start: {line:?}");
            };
            Xvfb {
                child,
                display: format!(":{number}"),
            }
        }

        /// Runs an X client such as xsetroot against this server and checks that it succeeded.
        pub(crate) fn run(&self, program: &str, args: &[&str]) {
            let status = Command::new(program)
                .args(args)
                .env("DISPLAY", &self.display)
                .stdout(Stdio::null())
                .status()
                .unwrap_or_else(|e| panic!("cannot run {program}: {e}"));
            assert!(status.success(), "{program} {args:?}: {status}");
        }

        /// Fills `rect` on the root with `rgb` (0xRRGGBB) from a client of its own, returning
        /// once the server has drawn it.
        pub(crate) fn fill(&self, rgb: u32, rect: Rectangle) {
            let (conn, screen) = connect(&self.display).unwrap();
            let root = conn.setup().roots[screen].root;
            let gc = conn.generate_id().unwrap();
            conn.create_gc(gc, root, &CreateGCAux::new().foreground(rgb))
                .unwrap();
            conn.poly_fill_rectangle(root, gc, &[rect]).unwrap();
            // Requests run in order, so this reply means the fill is done.
            conn.get_input_focus().unwrap().reply().unwrap();
        }

        /// Adds the 1280x720 mode "tilt-720p" for set_mode. Xvfb only switches between modes
        /// that fit in its startup screen.
        pub(crate) fn add_720p_mode(&self) {
            let modeline = [
                "74.48", "1280", "1336", "1472", "1664", "720", "721", "724", "746",
            ];
            self.run(
                "xrandr",
                &[&["--newmode", "tilt-720p"][..], &modeline].concat(),
            );
            self.run("xrandr", &["--addmode", "screen", "tilt-720p"]);
        }

        /// Switches the screen to `mode` with xrandr, as a user would; the startup mode is
        /// named after its size, e.g. "1920x1080".
        pub(crate) fn set_mode(&self, mode: &str) {
            self.run("xrandr", &["--output", "screen", "--mode", mode]);
        }
    }

    impl Drop for Xvfb {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Collects the events `conn` receives within `wait`.
    pub(crate) fn events_within(conn: &RustConnection, wait: Duration) -> Vec<Event> {
        let end = Instant::now() + wait;
        let mut events = Vec::new();
        while Instant::now() < end {
            match conn.poll_for_event().unwrap() {
                Some(event) => events.push(event),
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        events
    }

    /// Polls `check` every 5 ms until it holds or `wait` runs out.
    pub(crate) fn eventually(wait: Duration, mut check: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + wait;
        loop {
            if check() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
