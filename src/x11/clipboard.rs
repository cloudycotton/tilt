//! The clipboard thread `tilt-clipboard`: the X CLIPBOARD selection, both ways, on its own
//! connection.
//!
//! Text a viewer pastes becomes the CLIPBOARD's content: the thread owns the selection with it
//! and answers the requests of applications that paste. When another client takes the selection
//! (an application copied), XFixes says so and the thread reads it as UTF8_STRING and publishes
//! it through a watch channel, for the sessions to send to their viewer. Only text up to
//! MAX_CLIPBOARD_BYTES goes either way; larger content (and images) stays on its side.

use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender};
use tokio::sync::watch;
use tracing::{debug, error, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use super::conn::connect;
use crate::protocol::MAX_CLIPBOARD_BYTES;

/// How long the thread sleeps at most, so that it sees the shutdown flag.
const SHUTDOWN_POLL: Duration = Duration::from_millis(250);
/// The longest a paste waits for the clipboard to be ours before its key goes anyway.
const SET_TIMEOUT: Duration = Duration::from_millis(500);

/// The text most recently copied on the X display; None until something is copied while tilt
/// runs. Every copy is a change, the same text again too.
pub type Copied = Option<Arc<str>>;

/// Hands pasted text to the clipboard thread.
#[derive(Clone)]
pub struct ClipboardHandle {
    tx: Sender<(String, Sender<()>)>,
    wake: Arc<UnixStream>,
}

impl ClipboardHandle {
    /// Makes `text` the CLIPBOARD's content, returning once the X server has made tilt its owner
    /// (or after SET_TIMEOUT): a paste key sent after this pastes `text`.
    pub fn set(&self, text: String) {
        let (done, wait) = crossbeam_channel::bounded(1);
        if self.tx.send((text, done)).is_err() {
            return;
        }
        let _ = (&*self.wake).write(&[1]);
        if wait.recv_timeout(SET_TIMEOUT).is_err() {
            warn!("clipboard: the pasted text was not ready in time");
        }
    }
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8: Atom,
    text: Atom,
    plain_utf8: Atom,
    incr: Atom,
    /// Where converted selections arrive on our window.
    property: Atom,
}

/// Connects to `display` and starts `tilt-clipboard`. It is not a permanent thread: a display
/// without XFixes, or a lost connection, only ends clipboard sharing.
pub fn spawn_clipboard_thread(
    display: &str,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<(ClipboardHandle, watch::Receiver<Copied>)> {
    let (conn, screen) = connect(display)?;
    let root = conn.setup().roots[screen].root;
    conn.xfixes_query_version(5, 0)?.reply().context("XFIXES")?;
    let window = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        root,
        -1,
        -1,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new(),
    )?;
    let intern =
        |name: &[u8]| -> anyhow::Result<Atom> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
    let atoms = Atoms {
        clipboard: intern(b"CLIPBOARD")?,
        targets: intern(b"TARGETS")?,
        utf8: intern(b"UTF8_STRING")?,
        text: intern(b"TEXT")?,
        plain_utf8: intern(b"text/plain;charset=utf-8")?,
        incr: intern(b"INCR")?,
        property: intern(b"_TILT_CLIPBOARD")?,
    };
    conn.xfixes_select_selection_input(
        window,
        atoms.clipboard,
        SelectionEventMask::SET_SELECTION_OWNER,
    )?
    .check()
    .context("XFixesSelectSelectionInput")?;
    let (wake_rx, wake_tx) = UnixStream::pair()?;
    wake_rx.set_nonblocking(true)?;
    wake_tx.set_nonblocking(true)?;
    let (tx, rx) = crossbeam_channel::unbounded();
    let (copied_tx, copied) = watch::channel(None);
    let mut clip = Clip {
        conn,
        window,
        atoms,
        ours: None,
        copied: copied_tx,
    };
    std::thread::Builder::new()
        .name("tilt-clipboard".into())
        .spawn(move || {
            if let Err(e) = clip.run(&rx, &wake_rx, &shutdown) {
                error!("clipboard sharing stopped: {e:#}");
            }
        })
        .context("cannot start tilt-clipboard")?;
    let handle = ClipboardHandle {
        tx,
        wake: Arc::new(wake_tx),
    };
    Ok((handle, copied))
}

struct Clip {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    /// The pasted text, while we own the CLIPBOARD.
    ours: Option<Vec<u8>>,
    copied: watch::Sender<Copied>,
}

impl Clip {
    fn run(
        &mut self,
        rx: &Receiver<(String, Sender<()>)>,
        wake: &UnixStream,
        shutdown: &AtomicBool,
    ) -> anyhow::Result<()> {
        while !shutdown.load(Ordering::Relaxed) {
            // Commands first: waiting for their replies can read events, which then wait in
            // the connection's buffer where polling the socket would not see them.
            for (text, done) in rx.try_iter() {
                self.own(text)?;
                let _ = done.send(());
            }
            while let Some(event) = self.conn.poll_for_event()? {
                self.event(event)?;
            }
            self.conn.flush()?;
            wait_readable(&[self.conn.stream().as_raw_fd(), wake.as_raw_fd()])?;
            // Drain the wake-ups; the commands are in `rx`.
            let mut buf = [0u8; 64];
            while matches!((&*wake).read(&mut buf), Ok(n) if n > 0) {}
        }
        Ok(())
    }

    /// Takes the CLIPBOARD with `text`, and waits for the server to have done it.
    fn own(&mut self, text: String) -> anyhow::Result<()> {
        self.ours = Some(text.into_bytes());
        let c = self.atoms.clipboard;
        self.conn
            .set_selection_owner(self.window, c, CURRENT_TIME)?;
        if self.conn.get_selection_owner(c)?.reply()?.owner != self.window {
            warn!("clipboard: could not take the CLIPBOARD selection");
            self.ours = None;
        }
        Ok(())
    }

    fn event(&mut self, event: Event) -> anyhow::Result<()> {
        match event {
            // Someone copied: ask for it as text. Our own takeover is reported too.
            Event::XfixesSelectionNotify(e) if e.owner != self.window && e.owner != NONE => {
                self.conn.convert_selection(
                    self.window,
                    self.atoms.clipboard,
                    self.atoms.utf8,
                    self.atoms.property,
                    e.selection_timestamp,
                )?;
            }
            Event::SelectionNotify(e) if e.requestor == self.window => self.read(e.property)?,
            Event::SelectionRequest(e) => self.answer(&e)?,
            Event::SelectionClear(e) if e.owner == self.window => self.ours = None,
            // A requestor that went away before we answered.
            Event::Error(e) => debug!("clipboard: the X server rejected a request: {e:?}"),
            _ => {}
        }
        Ok(())
    }

    /// Reads a converted selection and publishes it, if it is text that fits.
    fn read(&mut self, property: Atom) -> anyhow::Result<()> {
        if property == NONE {
            debug!("clipboard: the copied content is not text");
            return Ok(());
        }
        let max_words = (MAX_CLIPBOARD_BYTES as u32).div_ceil(4);
        let reply = self
            .conn
            .get_property(true, self.window, property, AtomEnum::ANY, 0, max_words)?
            .reply()?;
        if reply.type_ == self.atoms.incr
            || reply.bytes_after > 0
            || reply.value.len() > MAX_CLIPBOARD_BYTES
        {
            self.conn.delete_property(self.window, property)?;
            debug!("clipboard: the copied text is over {MAX_CLIPBOARD_BYTES} bytes; not shared");
            return Ok(());
        }
        self.copied
            .send_replace(Some(String::from_utf8_lossy(&reply.value).into()));
        Ok(())
    }

    /// Gives a pasting application our text, or tells it we have none in its format.
    fn answer(&mut self, e: &SelectionRequestEvent) -> anyhow::Result<()> {
        let a = &self.atoms;
        // Clients from before ICCCM 2 name no property: they mean the target.
        let property = if e.property == NONE {
            e.target
        } else {
            e.property
        };
        let given = match &self.ours {
            Some(_) if e.selection != a.clipboard => false,
            Some(_) if e.target == a.targets => {
                let targets = [
                    a.targets,
                    a.utf8,
                    a.plain_utf8,
                    a.text,
                    AtomEnum::STRING.into(),
                ];
                self.conn.change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )?;
                true
            }
            Some(text) if [a.utf8, a.plain_utf8, a.text].contains(&e.target) => {
                let type_ = if e.target == a.plain_utf8 {
                    a.plain_utf8
                } else {
                    a.utf8
                };
                self.conn.change_property8(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    type_,
                    text,
                )?;
                true
            }
            Some(text) if e.target == u32::from(AtomEnum::STRING) => {
                let latin1: Vec<u8> = String::from_utf8_lossy(text)
                    .chars()
                    .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                    .collect();
                self.conn.change_property8(
                    PropMode::REPLACE,
                    e.requestor,
                    property,
                    AtomEnum::STRING,
                    &latin1,
                )?;
                true
            }
            _ => false,
        };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if given { property } else { NONE },
        };
        self.conn
            .send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
        Ok(())
    }
}

/// Sleeps until one of `fds` is readable, or SHUTDOWN_POLL.
fn wait_readable(fds: &[i32]) -> anyhow::Result<()> {
    let mut polls: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ms = SHUTDOWN_POLL.as_millis() as libc::c_int;
    // SAFETY: `polls` is a valid array of pollfds for the duration of the call.
    let rv = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, ms) };
    if rv < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(anyhow::Error::new(e).context("poll on the X connection"));
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::time::Instant;

    use x11rb::protocol::xproto::SelectionRequestEvent;

    use super::*;
    use crate::x11::testutil::Xvfb;

    /// An X client standing in for an application: it copies and pastes through CLIPBOARD.
    struct App {
        conn: RustConnection,
        window: Window,
        clipboard: Atom,
        utf8: Atom,
    }

    impl App {
        fn new(xvfb: &Xvfb) -> App {
            let (conn, screen) = connect(&xvfb.display).unwrap();
            let root = conn.setup().roots[screen].root;
            let window = conn.generate_id().unwrap();
            conn.create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )
            .unwrap();
            let atom = |name: &[u8]| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
            let (clipboard, utf8) = (atom(b"CLIPBOARD"), atom(b"UTF8_STRING"));
            App {
                conn,
                window,
                clipboard,
                utf8,
            }
        }

        /// Pastes: the CLIPBOARD as `target`, or None when its owner refuses.
        fn paste(&self, target: Atom) -> Option<Vec<u8>> {
            let property = self.utf8; // any atom will do as the property's name
            self.conn
                .convert_selection(self.window, self.clipboard, target, property, CURRENT_TIME)
                .unwrap();
            self.conn.flush().unwrap();
            let end = Instant::now() + Duration::from_secs(2);
            while Instant::now() < end {
                match self.conn.poll_for_event().unwrap() {
                    Some(Event::SelectionNotify(e)) if e.property == NONE => return None,
                    Some(Event::SelectionNotify(e)) => {
                        let reply = self
                            .conn
                            .get_property(true, self.window, e.property, AtomEnum::ANY, 0, 1 << 20)
                            .unwrap()
                            .reply()
                            .unwrap();
                        return Some(reply.value);
                    }
                    _ => std::thread::sleep(Duration::from_millis(2)),
                }
            }
            panic!("no SelectionNotify within 2 s");
        }

        /// Copies `text`, and answers requests for it until tilt has asked once.
        fn copy(&self, text: &[u8]) {
            self.conn
                .set_selection_owner(self.window, self.clipboard, CURRENT_TIME)
                .unwrap();
            self.conn.flush().unwrap();
            let end = Instant::now() + Duration::from_secs(2);
            while Instant::now() < end {
                let Some(Event::SelectionRequest(e)) = self.conn.poll_for_event().unwrap() else {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                };
                let SelectionRequestEvent {
                    requestor,
                    selection,
                    target,
                    property,
                    time,
                    ..
                } = e;
                self.conn
                    .change_property8(PropMode::REPLACE, requestor, property, self.utf8, text)
                    .unwrap();
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time,
                    requestor,
                    selection,
                    target,
                    property,
                };
                self.conn
                    .send_event(false, requestor, EventMask::NO_EVENT, notify)
                    .unwrap();
                self.conn.flush().unwrap();
                return;
            }
            panic!("tilt did not ask for the copied text");
        }
    }

    #[test]
    #[ignore = "needs Xvfb"]
    fn pasted_text_reaches_applications_and_copied_text_reaches_viewers() {
        let xvfb = Xvfb::start("640x480x24", &[]);
        let shutdown = Arc::new(AtomicBool::new(false));
        let (handle, mut copied) =
            spawn_clipboard_thread(&xvfb.display, Arc::clone(&shutdown)).unwrap();
        let app = App::new(&xvfb);

        // A viewer pastes: once set returns, an application's paste gets the text.
        handle.set("héllo\nwörld".into());
        assert_eq!(
            app.paste(app.utf8).as_deref(),
            Some("héllo\nwörld".as_bytes())
        );
        let string = app
            .conn
            .intern_atom(false, b"STRING")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        assert_eq!(
            app.paste(string).as_deref(),
            Some(&b"h\xe9llo\nw\xf6rld"[..])
        );
        let png = app
            .conn
            .intern_atom(false, b"image/png")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        assert_eq!(app.paste(png), None);
        // Taking the selection ourselves is not something copied.
        assert!(!copied.has_changed().unwrap());

        // An application copies: the text is published, once per copy, the same text again too.
        for _ in 0..2 {
            app.copy("copied ✓".as_bytes());
            let end = Instant::now() + Duration::from_secs(2);
            while !copied.has_changed().unwrap() {
                assert!(Instant::now() < end, "the copy was not published");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(copied.borrow_and_update().as_deref(), Some("copied ✓"));
        }

        // And the application's copy replaced ours: pasting gets it from the application.
        handle.set("again".into());
        assert_eq!(app.paste(app.utf8).as_deref(), Some(&b"again"[..]));
        shutdown.store(true, Ordering::Relaxed);
    }
}
