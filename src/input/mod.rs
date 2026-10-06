//! Input injection through XTest on the `tilt-input` thread (brief section 5.10).
//!
//! The thread owns its own X connection. Commands from every session arrive on one channel and
//! are injected in order, flushed once per drained batch. Keysyms resolve TigerVNC-style (see
//! `keymap`): a keycode with the keysym at level 1-2 of the active group, else at level 3-4
//! behind a fake ISO_Level3_Shift, else a spare keycode bound on demand. A Shift or level-3 shift
//! the session holds itself is lifted around a key that must not have it, since clients send
//! the character their own layout made with it. Each session's presses are recorded, so
//! ReleaseAll lets go of exactly what that session pressed and never of what an agent sharing
//! the XTest devices holds.
//!
//! Text types as text: the Control, Alt, Meta, Super or Hyper keys its session holds are lifted
//! for its whole length. It types a slice at a time, and waits for a spare keycode without
//! blocking the thread, so a ReleaseAll (sent when its session loses control, disconnects or
//! asks) cancels the rest at once. What tilt changes on the display (autorepeat, spare keycodes,
//! the keys and buttons it holds down) is recorded on the root window, so that the next tilt can
//! undo it if this one dies without restoring.

pub mod keymap;
pub mod keysym;

use std::collections::{HashMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, AutoRepeatMode, ChangeKeyboardControlAux, ConnectionExt as _, CreateWindowAux,
    Keycode, Mapping, PropMode, Window, WindowClass, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::X11Error;

use crate::clock;
use crate::protocol::denormalize;
use keymap::{KeyPlan, Keycodes, Keymap, ModState, Modifiers, SpareKeycodes};

/// The XTest pointer device has 10 buttons.
const MAX_BUTTON: u8 = 10;
/// Wheel notches injected per axis per message.
const MAX_NOTCHES: u16 = 10;
/// How often autorepeat is re-checked while a controller is present: something else (a desktop
/// session applying its keyboard settings) may turn it back on.
const REPEAT_CHECK: Duration = Duration::from_secs(5);
/// The longest wait for a command, so the shutdown flag and X events are seen while idle.
const IDLE_POLL: Duration = Duration::from_millis(250);
/// Commands taken off the channel between two runs of the queue.
const MAX_BATCH: usize = 64;
/// The longest the queue runs between flushes. New commands are taken between slices, so this
/// bounds how long a text keeps typing after its session let go.
const SLICE: Duration = Duration::from_millis(2);
/// A spare keycode is not rebound within this long of its last use. Clients translate a keycode
/// with the mapping they fetch when they get to its MappingNotify, which can be after a quick
/// rebind, and they would then type the new keysym for the old one's events.
const REBIND_GUARD_US: u64 = 100_000;
/// The most text, in bytes, waiting to be typed; a text beyond it is dropped. Losing control
/// cancels a session's text, so this bounds what one controller can queue by sending text faster
/// than it types (a text outside the layout types at under 200 characters a second).
const MAX_QUEUED_TEXT: usize = 1 << 20;
/// The root window property in which tilt records what it changed on the display, and the
/// selection the running tilt owns. The server drops a selection's owner with its connection,
/// so a record that nobody owns was left by a tilt that died before restoring.
const RECORD: &[u8] = b"_TILT_INPUT";
/// The record's fixed part: the autorepeat word, then the keycodes (8 words, see
/// `Keycodes::to_words`) and the buttons (bit n for button n) tilt holds down.
const RECORD_HEADER: usize = 10;
/// The longest record: the fixed part, then a keycode and a keysym per spare keycode.
const RECORD_WORDS: u32 = RECORD_HEADER as u32 + 2 * 256;

/// A command for the input thread. `session` scopes the press bookkeeping, so ReleaseAll only
/// releases what that session pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputCmd {
    /// Coordinates are normalized: 0 is the first pixel, 65535 the last.
    Move {
        session: u64,
        x: u16,
        y: u16,
    },
    Button {
        session: u64,
        button: u8,
        down: bool,
        x: u16,
        y: u16,
    },
    Wheel {
        session: u64,
        dx: i16,
        dy: i16,
        x: u16,
        y: u16,
    },
    Key {
        session: u64,
        keysym: u32,
        down: bool,
    },
    Text {
        session: u64,
        text: String,
    },
    /// Also cancels the rest of a text the session sent before it, and what the session sent
    /// after that text, which may count on it. Its input from before the text still runs first.
    ReleaseAll {
        session: u64,
    },
    /// Server autorepeat is off while any session holds control, and restored after.
    SetControllerPresent(bool),
}

impl InputCmd {
    /// The session whose input this is; None for a change of controller presence.
    fn session(&self) -> Option<u64> {
        match *self {
            InputCmd::Move { session, .. }
            | InputCmd::Button { session, .. }
            | InputCmd::Wheel { session, .. }
            | InputCmd::Key { session, .. }
            | InputCmd::Text { session, .. }
            | InputCmd::ReleaseAll { session } => Some(session),
            InputCmd::SetControllerPresent(_) => None,
        }
    }
}

#[derive(Clone)]
pub struct InputHandle {
    tx: Sender<InputCmd>,
    last_input_us: Arc<AtomicU64>,
}

impl InputHandle {
    /// Queues a command; it is dropped silently once the input thread has exited.
    pub fn send(&self, c: InputCmd) {
        let _ = self.tx.send(c);
    }

    /// Wakes the thread so that it sees the shutdown flag. (Phase 0 stub: the thread still
    /// wakes on its own every 250 ms, so this does nothing yet.)
    #[expect(dead_code, reason = "phase 0 stub: called at shutdown (G3 C2)")]
    pub fn wake(&self) {}

    /// clock::now_us() when injected input was last flushed; 0 before any.
    pub fn last_input_us(&self) -> u64 {
        self.last_input_us.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
impl InputHandle {
    /// A handle with no input thread behind it, for other modules' tests: the receiver gets
    /// whatever is sent.
    pub fn detached() -> (InputHandle, Receiver<InputCmd>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = InputHandle {
            tx,
            last_input_us: Arc::new(AtomicU64::new(0)),
        };
        (handle, rx)
    }
}

/// Connects to `display` and starts `tilt-input`, first undoing what a tilt that died there left
/// changed. Shutdown joins the returned thread, which restores autorepeat and unbinds spare
/// keycodes before it exits, after a panic too.
pub fn spawn_input_thread(
    display: String,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<(InputHandle, JoinHandle<()>)> {
    // Set up before spawning, so a display without XTest fails startup rather than the thread.
    let injector = Injector::connect(&display, shutdown)?;
    let (tx, rx) = crossbeam_channel::unbounded();
    let last_input_us = Arc::new(AtomicU64::new(0));
    let stamp = Arc::clone(&last_input_us);
    let thread = std::thread::Builder::new()
        .name("tilt-input".into())
        .spawn(move || injector.run(&rx, &stamp))
        .context("spawning tilt-input")?;
    Ok((InputHandle { tx, last_input_us }, thread))
}

/// One key a session holds down.
struct HeldKey {
    keysym: u32,
    keycode: Keycode,
    /// The key was down already, held by an agent sharing the XTest keyboard. The server ignored
    /// the press, so the release is the agent's to send.
    borrowed: bool,
}

/// One button a session holds down; `borrowed` as for keys.
struct HeldButton {
    button: u8,
    borrowed: bool,
}

/// What one session holds down, in press order.
#[derive(Default)]
struct Held {
    keys: Vec<HeldKey>,
    buttons: Vec<HeldButton>,
}

impl Held {
    fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }
}

/// Why a command stopped early.
#[derive(Debug)]
enum Failure {
    /// The X connection is gone, which ends the thread.
    Lost(ConnectionError),
    /// The server answered a request with an error; the rest of the command is dropped.
    Rejected(X11Error),
}

impl From<ConnectionError> for Failure {
    fn from(e: ConnectionError) -> Failure {
        Failure::Lost(e)
    }
}

impl From<ReplyError> for Failure {
    fn from(e: ReplyError) -> Failure {
        match e {
            ReplyError::ConnectionError(e) => Failure::Lost(e),
            ReplyError::X11Error(e) => Failure::Rejected(e),
        }
    }
}

type Step<T = ()> = Result<T, Failure>;

/// Logs a rejected request and carries on as if the command were done: only a lost connection
/// ends the thread.
fn settle<T: Default>(step: Step<T>) -> Result<T, ConnectionError> {
    match step {
        Ok(done) => Ok(done),
        Err(Failure::Lost(e)) => Err(e),
        Err(Failure::Rejected(e)) => {
            tracing::warn!("input: the X server rejected a request: {e:?}");
            Ok(T::default())
        }
    }
}

/// How far a queued command got.
#[derive(Default)]
enum Progress {
    #[default]
    Done,
    /// A text ran out of its slice and carries on in the next.
    Yield,
    /// It needs a spare keycode that may be rebound from this clock::now_us() on (see
    /// REBIND_GUARD_US). It runs again then, and what is queued behind it waits.
    WaitUntil(u64),
}

/// How a keysym can be typed now.
enum Resolved {
    Plan(KeyPlan),
    /// From this clock::now_us() on, on a spare keycode still in its rebind guard.
    Later(u64),
    /// Not at all: every spare keycode is held down.
    Untypable,
}

/// Commands not yet injected, in arrival order.
#[derive(Default)]
struct Queue {
    cmds: VecDeque<InputCmd>,
    /// Bytes of the front command's text typed so far.
    typed: usize,
    /// The clock::now_us() before which the front command does not run.
    resume_us: u64,
    /// The keys the front command, a text, lifted before its first character, to press again
    /// after its last; None until it lifts them.
    lifted: Option<Keycodes>,
    /// Bytes of the texts in `cmds`, typed or not.
    text_bytes: usize,
    /// Whether the last text was dropped (see MAX_QUEUED_TEXT), so a flood is logged once.
    dropping: bool,
}

impl Queue {
    /// Queues `cmd`, unless it is a text that would take the queued text over MAX_QUEUED_TEXT.
    fn push(&mut self, cmd: InputCmd) {
        if let InputCmd::Text { ref text, .. } = cmd {
            let dropping = self.text_bytes + text.len() > MAX_QUEUED_TEXT;
            if dropping && !self.dropping {
                tracing::warn!(
                    "input: dropping text while {} bytes of it wait to be typed",
                    self.text_bytes
                );
            }
            self.dropping = dropping;
            if dropping {
                return;
            }
            self.text_bytes += text.len();
        }
        self.cmds.push_back(cmd);
    }

    /// Forgets `cmd`, the front command, which `run_queue` took off and finished.
    fn done(&mut self, cmd: &InputCmd) {
        if let InputCmd::Text { text, .. } = cmd {
            self.text_bytes -= text.len();
        }
        (self.typed, self.resume_us) = (0, 0);
    }

    /// Drops `session`'s first text not yet typed in full, and the session's commands queued
    /// after it.
    fn cancel_text(&mut self, session: u64) {
        let text =
            |cmd: &InputCmd| matches!(*cmd, InputCmd::Text { session: s, .. } if s == session);
        let Some(first) = self.cmds.iter().position(text) else {
            return;
        };
        if first == 0 {
            // What it lifted stays up: its session let go of everything.
            (self.typed, self.resume_us, self.lifted) = (0, 0, None);
        }
        let (mut index, mut dropped) = (0, 0);
        self.cmds.retain(|cmd| {
            index += 1;
            let keep = index <= first || cmd.session() != Some(session);
            if let (false, InputCmd::Text { text, .. }) = (keep, cmd) {
                dropped += text.len();
            }
            keep
        });
        self.text_bytes -= dropped;
    }
}

/// Reads the whole core keyboard mapping.
fn read_keymap(conn: &RustConnection, min: Keycode, max: Keycode) -> Result<Keymap, ReplyError> {
    let count = max.saturating_sub(min).saturating_add(1);
    let reply = conn.get_keyboard_mapping(min, count)?.reply()?;
    Ok(Keymap::new(min, reply.keysyms_per_keycode, reply.keysyms))
}

/// Undoes what a tilt that died before restoring left on the display, as its record says: the
/// keys and buttons it held down are released, the spare keycodes it bound are unbound where
/// they still hold its keysyms, and autorepeat it turned off goes back on (no session can hold
/// control yet). Only its own presses are recorded, not keys an agent held already, and an
/// agent's press of a recorded key since was ignored, the key being down.
fn repair(
    conn: &RustConnection,
    root: Window,
    record: Atom,
    keymap: &mut Keymap,
) -> Result<(), ReplyError> {
    let reply = conn
        .get_property(false, root, record, AtomEnum::CARDINAL, 0, RECORD_WORDS)?
        .reply()?;
    let Some(words) = reply.value32() else {
        return Ok(());
    };
    let words: Vec<u32> = words.collect();
    conn.delete_property(root, record)?;
    let Some((header, bindings)) = words.split_first_chunk::<RECORD_HEADER>() else {
        return Ok(());
    };
    let [repeat, keys @ .., buttons] = *header;
    // Before unbinding, so that clients translate each release as they did its press.
    let keys = Keycodes::from_words(keys);
    let keys = keys.iter().map(|keycode| (KEY_RELEASE_EVENT, keycode));
    let buttons = (1..=MAX_BUTTON).filter(|&button| buttons & 1 << button != 0);
    let mut released = 0;
    for (kind, detail) in keys.chain(buttons.map(|button| (BUTTON_RELEASE_EVENT, button))) {
        conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)?;
        released += 1;
    }
    let mut unbound = 0;
    for &[keycode, recorded] in bindings.as_chunks::<2>().0 {
        let Ok(keycode) = u8::try_from(keycode) else {
            continue;
        };
        if keymap.keysyms(keycode).first() == Some(&recorded) {
            conn.change_keyboard_mapping(1, keycode, 1, &[keysym::NO_SYMBOL])?;
            keymap.set(keycode, &[keysym::NO_SYMBOL]);
            unbound += 1;
        }
    }
    // Recorded as the mode plus one; only ON and DEFAULT are worth restoring.
    let mode = [AutoRepeatMode::ON, AutoRepeatMode::DEFAULT]
        .into_iter()
        .find(|&mode| u32::from(mode) + 1 == repeat);
    let mut restored = false;
    if let Some(mode) = mode {
        let current = conn.get_keyboard_control()?.reply()?.global_auto_repeat;
        if current == AutoRepeatMode::OFF {
            let aux = ChangeKeyboardControlAux::new().auto_repeat_mode(mode);
            conn.change_keyboard_control(&aux)?;
            restored = true;
        }
    }
    if released > 0 || unbound > 0 || restored {
        tracing::info!(
            "input: undid what a tilt that exited uncleanly left: {released} keys and buttons \
             released, {unbound} spare keycodes unbound{}",
            if restored { ", autorepeat back on" } else { "" }
        );
    }
    Ok(())
}

/// The state of the `tilt-input` thread.
struct Injector {
    conn: RustConnection,
    root: Window,
    width: u16,
    height: u16,
    min_keycode: Keycode,
    max_keycode: Keycode,
    keymap: Keymap,
    modmap: Vec<Keycode>,
    mods: Modifiers,
    spares: SpareKeycodes,
    held: HashMap<u64, Held>,
    queue: Queue,
    /// Names the record (see RECORD).
    record_atom: Atom,
    /// What the record says sessions hold down (see `holding`).
    recorded_holding: (Keycodes, u32),
    /// Whether a session holds control, which keeps server autorepeat off.
    controller: bool,
    /// The autorepeat mode tilt turned off, to restore once no controller is present.
    repeat_restore: Option<AutoRepeatMode>,
    next_repeat_check: Instant,
    /// Whether anything was injected since the last flush.
    injected: bool,
    shutdown: Arc<AtomicBool>,
}

impl Injector {
    fn connect(display_name: &str, shutdown: Arc<AtomicBool>) -> anyhow::Result<Injector> {
        let (conn, screen) = crate::x11::conn::connect(display_name)?;
        let setup = conn.setup();
        let (min_keycode, max_keycode) = (setup.min_keycode, setup.max_keycode);
        let screen = setup
            .roots
            .get(screen)
            .context("the X server has no such screen")?;
        let (root, width, height) = (screen.root, screen.width_in_pixels, screen.height_in_pixels);

        let xtest = conn
            .xtest_get_version(2, 2)
            .map_err(anyhow::Error::from)
            .and_then(|cookie| Ok(cookie.reply()?))
            .context("XTest is unavailable")?;
        // Keep injecting while another client has grabbed the server, as a physical device does.
        conn.xtest_grab_control(true)?;

        // RandR resizes the root window; ScreenChangeNotify says when to re-read its size.
        if conn
            .extension_information(randr::X11_EXTENSION_NAME)?
            .is_some()
        {
            conn.randr_query_version(1, 2)?.reply()?;
            conn.randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE)?;
        } else {
            tracing::warn!("input: no RandR, so pointer positions assume the screen never resizes");
        }

        let mut keymap = read_keymap(&conn, min_keycode, max_keycode)?;
        let modmap = conn.get_modifier_mapping()?.reply()?.keycodes;
        let record_atom = conn.intern_atom(false, RECORD)?.reply()?.atom;
        if conn.get_selection_owner(record_atom)?.reply()?.owner == x11rb::NONE {
            repair(&conn, root, record_atom, &mut keymap)?;
        } else {
            tracing::warn!("input: another tilt is injecting on {display_name}, or still exiting");
        }
        // Owned until this connection closes, however tilt exits.
        let owner = conn.generate_id()?;
        let aux = CreateWindowAux::new();
        let class = WindowClass::INPUT_ONLY;
        conn.create_window(0, owner, root, -1, -1, 1, 1, 0, class, 0, &aux)?;
        conn.set_selection_owner(owner, record_atom, x11rb::CURRENT_TIME)?;
        conn.flush()?;
        let mods = Modifiers::new(&keymap, &modmap);
        let spares = SpareKeycodes::new(&keymap);
        tracing::info!(
            "input: XTest {}.{} on {display_name}, {width}x{height}, {} spare keycodes",
            xtest.major_version,
            xtest.minor_version,
            spares.capacity()
        );
        Ok(Injector {
            conn,
            root,
            width,
            height,
            min_keycode,
            max_keycode,
            keymap,
            modmap,
            mods,
            spares,
            held: HashMap::new(),
            queue: Queue::default(),
            record_atom,
            recorded_holding: (Keycodes::default(), 0),
            controller: false,
            repeat_restore: None,
            next_repeat_check: Instant::now(),
            injected: false,
            shutdown,
        })
    }

    fn run(mut self, rx: &Receiver<InputCmd>, last_input_us: &AtomicU64) {
        // Restore after a panic too: keys left down and autorepeat left off would stay so for
        // every X client.
        let served = panic::catch_unwind(AssertUnwindSafe(|| self.serve(rx, last_input_us)));
        let restored = self.restore();
        let served = match served {
            Ok(served) => served,
            Err(panic) => {
                if let Err(e) = restored {
                    tracing::error!("input: could not restore after a panic: {e}");
                }
                panic::resume_unwind(panic);
            }
        };
        if let Err(e) = served.and(restored) {
            tracing::error!("input: lost the X connection: {e}");
        }
    }

    /// Injects commands until shutdown, or until every InputHandle is gone.
    fn serve(
        &mut self,
        rx: &Receiver<InputCmd>,
        last_input_us: &AtomicU64,
    ) -> Result<(), ConnectionError> {
        while !self.shutdown.load(Ordering::Relaxed) {
            let first = match rx.recv_timeout(self.idle_wait()) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            self.poll_events()?;
            for cmd in first.into_iter().chain(rx.try_iter().take(MAX_BATCH - 1)) {
                self.accept(cmd);
            }
            self.run_queue()?;
            // Before the flush, which sends the re-check's ChangeKeyboardControl too.
            if self.controller && Instant::now() >= self.next_repeat_check {
                settle(self.keep_repeat_off())?;
            }
            self.conn.flush()?;
            if std::mem::take(&mut self.injected) {
                last_input_us.store(clock::now_us(), Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn idle_wait(&self) -> Duration {
        let mut wait = IDLE_POLL;
        if self.controller {
            wait = wait.min(
                self.next_repeat_check
                    .saturating_duration_since(Instant::now()),
            );
        }
        if !self.queue.cmds.is_empty() {
            let resume_in = self.queue.resume_us.saturating_sub(clock::now_us());
            wait = wait.min(Duration::from_micros(resume_in));
        }
        wait
    }

    /// Queues `cmd`. A ReleaseAll first cancels its session's text, so a session that lets go
    /// (or loses control, or disconnects) stops typing at once rather than when the text ends.
    fn accept(&mut self, cmd: InputCmd) {
        if let InputCmd::ReleaseAll { session } = cmd {
            self.queue.cancel_text(session);
        }
        self.queue.push(cmd);
    }

    /// Runs queued commands for up to SLICE, until one has to wait for a spare keycode.
    fn run_queue(&mut self) -> Result<(), ConnectionError> {
        let deadline = Instant::now() + SLICE;
        while Instant::now() < deadline && self.queue.resume_us <= clock::now_us() {
            let Some(cmd) = self.queue.cmds.pop_front() else {
                break;
            };
            match settle(self.execute(&cmd, deadline))? {
                Progress::Done => self.queue.done(&cmd),
                Progress::Yield => {
                    self.queue.cmds.push_front(cmd);
                    break;
                }
                Progress::WaitUntil(us) => {
                    self.queue.resume_us = us;
                    self.queue.cmds.push_front(cmd);
                    break;
                }
            }
        }
        // Once per run however many keys and buttons went down or up: a tilt that dies holding
        // them leaves word of them for `repair`.
        if self.holding() != self.recorded_holding {
            settle(self.record())?;
        }
        Ok(())
    }

    /// Drains X events, then re-reads what they say changed, once however many arrived.
    fn poll_events(&mut self) -> Result<(), ConnectionError> {
        let (mut keyboard, mut modifiers, mut screen) = (false, false, false);
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::MappingNotify(e) => {
                    keyboard |= e.request == Mapping::KEYBOARD;
                    modifiers |= e.request == Mapping::MODIFIER;
                }
                Event::RandrScreenChangeNotify(_) => screen = true,
                Event::Error(e) => tracing::warn!("input: the X server rejected a request: {e:?}"),
                _ => {}
            }
        }
        settle(self.refresh(keyboard, modifiers, screen))
    }

    fn refresh(&mut self, keyboard: bool, modifiers: bool, screen: bool) -> Step {
        if keyboard {
            self.keymap = read_keymap(&self.conn, self.min_keycode, self.max_keycode)?;
            // Bindings someone else overwrote are no longer ours to reuse or unbind.
            if self.spares.reconcile(&self.keymap) {
                self.record()?;
            }
        }
        if modifiers {
            self.modmap = self.conn.get_modifier_mapping()?.reply()?.keycodes;
        }
        if keyboard || modifiers {
            self.mods = Modifiers::new(&self.keymap, &self.modmap);
        }
        if screen {
            let geometry = self.conn.get_geometry(self.root)?.reply()?;
            (self.width, self.height) = (geometry.width, geometry.height);
            tracing::debug!("input: the screen is now {}x{}", self.width, self.height);
        }
        Ok(())
    }

    /// Runs `cmd`; a text only until `deadline`.
    fn execute(&mut self, cmd: &InputCmd, deadline: Instant) -> Step<Progress> {
        match *cmd {
            InputCmd::Move { session: _, x, y } => {
                let (x, y) = self.pixel(x, y);
                self.fake(MOTION_NOTIFY_EVENT, 0, x, y)?;
            }
            InputCmd::Button {
                session,
                button,
                down,
                x,
                y,
            } => self.button(session, button, down, x, y)?,
            InputCmd::Wheel {
                session: _,
                dx,
                dy,
                x,
                y,
            } => self.wheel(dx, dy, x, y)?,
            InputCmd::Key {
                session,
                keysym,
                down: true,
            } => return self.key_down(session, keysym),
            InputCmd::Key {
                session,
                keysym,
                down: false,
            } => self.key_up(session, keysym)?,
            InputCmd::Text { session, ref text } => return self.text(session, text, deadline),
            InputCmd::ReleaseAll { session } => self.release_all(session)?,
            InputCmd::SetControllerPresent(present) => self.set_controller_present(present)?,
        }
        Ok(Progress::Done)
    }

    /// The root-window pixel for normalized coordinates.
    fn pixel(&self, x: u16, y: u16) -> (i16, i16) {
        // X coordinates are i16, so no screen is wide enough for this to saturate.
        let to_i16 = |v: u32| i16::try_from(v).unwrap_or(i16::MAX);
        (
            to_i16(denormalize(x, u32::from(self.width))),
            to_i16(denormalize(y, u32::from(self.height))),
        )
    }

    /// Queues one XTest event. Motion (detail 0: absolute) is on our root window; key and button
    /// events take no position.
    fn fake(&mut self, kind: u8, detail: u8, x: i16, y: i16) -> Step {
        let root = if kind == MOTION_NOTIFY_EVENT {
            self.root
        } else {
            x11rb::NONE
        };
        self.conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, root, x, y, 0)?;
        self.injected = true;
        Ok(())
    }

    fn key(&mut self, press: bool, keycode: Keycode) -> Step {
        let kind = if press {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake(kind, keycode, 0, 0)
    }

    fn button_event(&mut self, press: bool, button: u8) -> Step {
        let kind = if press {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(kind, button, 0, 0)
    }

    /// Moves the pointer to (x, y) unless it is already there, so buttons and notches act where
    /// the client saw the pointer, and returns the core state (modifiers and buttons 1-5 down).
    /// The server is asked rather than trusting where tilt last put the pointer, because an
    /// agent may have moved it since, and a motion to the current position would still reach
    /// clients as a MotionNotify.
    fn move_to(&mut self, x: u16, y: u16) -> Step<u16> {
        let (x, y) = self.pixel(x, y);
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        if (pointer.root_x, pointer.root_y) != (x, y) {
            self.fake(MOTION_NOTIFY_EVENT, 0, x, y)?;
        }
        Ok(u16::from(pointer.mask))
    }

    fn button(&mut self, session: u64, button: u8, down: bool, x: u16, y: u16) -> Step {
        if !(1..=MAX_BUTTON).contains(&button) {
            tracing::debug!("input: ignoring button {button}");
            return Ok(());
        }
        let state = self.move_to(x, y)?;
        if !down {
            return match self.take_button(session, button) {
                Some(held) => self.release_button(&held),
                None => Ok(()),
            };
        }
        let pressed = self
            .held
            .get(&session)
            .is_some_and(|held| held.buttons.iter().any(|b| b.button == button));
        if pressed {
            return Ok(());
        }
        // As for keys, a button that is down already is an agent's unless a session holds it.
        // The core state shows buttons 1-5 only, so presses of 6-10 always count as tilt's own.
        let borrowed = button <= 5 && state & 1 << (7 + button) != 0 && !self.holds_button(button);
        let held = HeldButton { button, borrowed };
        self.held.entry(session).or_default().buttons.push(held);
        self.button_event(true, button)
    }

    /// One press and release of 4/5 (up/down) or 6/7 (left/right) per notch: XTest has no
    /// smooth scrolling.
    fn wheel(&mut self, dx: i16, dy: i16, x: u16, y: u16) -> Step {
        self.move_to(x, y)?;
        for (delta, negative, positive) in [(dy, 4, 5), (dx, 6, 7)] {
            let button = if delta < 0 { negative } else { positive };
            for _ in 0..delta.unsigned_abs().min(MAX_NOTCHES) {
                self.button_event(true, button)?;
                self.button_event(false, button)?;
            }
        }
        Ok(())
    }

    fn key_down(&mut self, session: u64, keysym: u32) -> Step<Progress> {
        if !keysym::is_valid(keysym) {
            tracing::debug!("input: ignoring keysym {keysym:#x}");
            return Ok(Progress::Done);
        }
        let pressed = self
            .held
            .get(&session)
            .is_some_and(|held| held.keys.iter().any(|k| k.keysym == keysym));
        if pressed {
            // A modifier stays down. Anything else is the client's autorepeat, sent as release
            // + press: with server autorepeat off, a second bare press would be dropped.
            if keysym::is_modifier(keysym) {
                return Ok(Progress::Done);
            }
            self.key_up(session, keysym)?;
        }
        let (state, down) = self.keyboard(session)?;
        let plan = match self.resolve(keysym, state, &down)? {
            Resolved::Plan(plan) => plan,
            Resolved::Later(us) => return Ok(Progress::WaitUntil(us)),
            Resolved::Untypable => return Ok(Progress::Done),
        };
        // Only a modifier keeps its key when that is down already (see resolve), and the server
        // ignores the press. Unless a session of ours holds it, an agent does, and the release
        // is the agent's to send.
        let borrowed = down.contains(plan.keycode) && !self.holds_key(plan.keycode);
        self.type_key(plan, &down, false)?;
        self.held.entry(session).or_default().keys.push(HeldKey {
            keysym,
            keycode: plan.keycode,
            borrowed,
        });
        Ok(Progress::Done)
    }

    fn key_up(&mut self, session: u64, keysym: u32) -> Step {
        match self.take_key(session, keysym) {
            Some(key) => self.release_key(&key),
            None => {
                tracing::debug!(
                    "input: session {session} released {keysym:#x} without pressing it"
                );
                Ok(())
            }
        }
    }

    /// Releases what `session` holds: keys in reverse press order, then buttons.
    fn release_all(&mut self, session: u64) -> Step {
        // One at a time, so each release sees what is still held, this session's keys included.
        while let Some(key) = self.held.get_mut(&session).and_then(|h| h.keys.pop()) {
            self.release_key(&key)?;
        }
        while let Some(button) = self.held.get_mut(&session).and_then(|h| h.buttons.pop()) {
            self.release_button(&button)?;
        }
        self.held.remove(&session);
        Ok(())
    }

    /// Forgets `session`'s press of `keysym`, returning it.
    fn take_key(&mut self, session: u64, keysym: u32) -> Option<HeldKey> {
        let held = self.held.get_mut(&session)?;
        let i = held.keys.iter().position(|k| k.keysym == keysym)?;
        let key = held.keys.remove(i);
        if held.is_empty() {
            self.held.remove(&session);
        }
        Some(key)
    }

    /// Forgets `session`'s press of `button`, returning it.
    fn take_button(&mut self, session: u64, button: u8) -> Option<HeldButton> {
        let held = self.held.get_mut(&session)?;
        let i = held.buttons.iter().position(|b| b.button == button)?;
        let button = held.buttons.remove(i);
        if held.is_empty() {
            self.held.remove(&session);
        }
        Some(button)
    }

    /// Releases `key` if tilt pressed it and no other press of its keycode remains: sessions can
    /// hold the same key, and it stays down until the last of them lets go.
    fn release_key(&mut self, key: &HeldKey) -> Step {
        self.spares.touch(key.keycode, clock::now_us());
        if key.borrowed || self.holds_key(key.keycode) {
            return Ok(());
        }
        self.key(false, key.keycode)
    }

    fn release_button(&mut self, held: &HeldButton) -> Step {
        if held.borrowed || self.holds_button(held.button) {
            return Ok(());
        }
        self.button_event(false, held.button)
    }

    /// Whether a session holds `keycode` down through a press of tilt's own.
    fn holds_key(&self, keycode: Keycode) -> bool {
        self.held
            .values()
            .any(|h| h.keys.iter().any(|k| k.keycode == keycode && !k.borrowed))
    }

    /// Whether a session holds `button` down through a press of tilt's own.
    fn holds_button(&self, button: u8) -> bool {
        self.held
            .values()
            .any(|h| h.buttons.iter().any(|b| b.button == button && !b.borrowed))
    }

    /// What sessions hold down through presses of tilt's own: the keycodes, and the buttons as
    /// bit n for button n.
    fn holding(&self) -> (Keycodes, u32) {
        let keys = self.held.values().flat_map(|h| &h.keys);
        let buttons = self.held.values().flat_map(|h| &h.buttons);
        (
            keys.filter(|k| !k.borrowed).map(|k| k.keycode).collect(),
            buttons
                .filter(|b| !b.borrowed)
                .fold(0, |bits, b| bits | 1 << b.button),
        )
    }

    /// The keyboard as `session` finds it: the modifiers in effect (an agent's included, and
    /// which of them the session may lift) and the keys down.
    fn keyboard(&self, session: u64) -> Step<(ModState, Keycodes)> {
        // Both requests go out before either reply is awaited: one round trip.
        let pointer = self.conn.query_pointer(self.root)?;
        let keys = self.conn.query_keymap()?;
        let mask = u16::from(pointer.reply()?.mask);
        let down = Keycodes::from_bits(keys.reply()?.keys);
        Ok((self.mods.state(mask, &down, &self.own_keys(session)), down))
    }

    /// The keycodes `session` holds through presses of its own.
    fn own_keys(&self, session: u64) -> Keycodes {
        self.held
            .get(&session)
            .into_iter()
            .flat_map(|held| &held.keys)
            .filter(|key| !key.borrowed)
            .map(|key| key.keycode)
            .collect()
    }

    /// How to type `keysym` from `state`, binding it to a spare keycode if the keymap lacks it
    /// or its key is down. The server ignores a press of a key that is down, so it would type
    /// nothing, and a tap's release would let go of whoever holds it. Modifiers keep their own
    /// keys: only those are in the modifier map.
    fn resolve(&mut self, keysym: u32, state: ModState, down: &Keycodes) -> Step<Resolved> {
        let usable = |keycode| keysym::is_modifier(keysym) || !down.contains(keycode);
        let planned = self.keymap.plan(keysym, state, &self.mods);
        if let Some(plan) = planned.filter(|plan| usable(plan.keycode)) {
            return Ok(Resolved::Plan(plan));
        }
        // Our bindings type alike in every group, the third and fourth too, which `plan` cannot
        // see.
        let bound = self.spares.bindings().iter().map(|b| b.keycode);
        let free = bound.filter(|&keycode| usable(keycode));
        if let Some(plan) = self.keymap.plan_on(free, keysym, state, &self.mods) {
            return Ok(Resolved::Plan(plan));
        }
        self.bind_spare(keysym, state, down)
    }

    /// Binds `keysym` to a spare keycode, and plans it there.
    fn bind_spare(&mut self, keysym: u32, state: ModState, down: &Keycodes) -> Step<Resolved> {
        // Never a keycode that is down, whoever holds it: the server would ignore its press.
        let candidate = self
            .spares
            .candidate(|keycode| down.contains(keycode) || self.holds_key(keycode));
        let Some(allocation) = candidate else {
            tracing::debug!("input: no spare keycode free for keysym {keysym:#x}");
            return Ok(Resolved::Untypable);
        };
        let now = clock::now_us();
        if let Some(used_us) = allocation.evicted_used_us {
            // Let clients translate the evicted keysym's events first (see REBIND_GUARD_US).
            let free_us = used_us.saturating_add(REBIND_GUARD_US);
            if now < free_us {
                return Ok(Resolved::Later(free_us));
            }
        }
        let keysyms = keymap::spare_row(keysym, &self.mods);
        self.spares.bind(allocation.keycode, keysyms[0], now);
        // Recorded before the server has it, so that a tilt dying in between leaves word of it.
        self.record()?;
        // check() is a GetInputFocus round trip, so a rejected binding drops the command.
        self.conn
            .change_keyboard_mapping(1, allocation.keycode, 2, &keysyms)?
            .check()?;
        self.keymap.set(allocation.keycode, &keysyms);
        let bound = std::iter::once(allocation.keycode);
        Ok(
            match self.keymap.plan_on(bound, keysym, state, &self.mods) {
                Some(plan) => Resolved::Plan(plan),
                None => Resolved::Untypable,
            },
        )
    }

    /// Presses `plan.keycode` (and releases it too when `tap`) inside its modifier changes: the
    /// session's own Shift or level-3 shift lifted, fake ones pressed. All of it is undone
    /// straight after, so none outlives the press.
    fn type_key(&mut self, plan: KeyPlan, down: &Keycodes, tap: bool) -> Step {
        let lifted = self.mods.lifted(&plan, down);
        let fakes = [
            self.mods.level3_key.filter(|_| plan.fake_level3),
            self.mods.shift_key.filter(|_| plan.fake_shift),
        ];
        for keycode in lifted.iter() {
            self.key(false, keycode)?;
        }
        for &keycode in fakes.iter().flatten() {
            self.key(true, keycode)?;
        }
        self.key(true, plan.keycode)?;
        if tap {
            self.key(false, plan.keycode)?;
        }
        for &keycode in fakes.iter().rev().flatten() {
            self.key(false, keycode)?;
        }
        for keycode in lifted.iter() {
            self.key(true, keycode)?;
        }
        self.spares.touch(plan.keycode, clock::now_us());
        Ok(())
    }

    /// Types `text` on from where its last slice stopped, until `deadline`, a tap per character.
    /// Characters with no keysym (control characters other than \n, \t, BS, ESC and DEL) are
    /// skipped, and so are those that no spare keycode can take. The Control, Alt, Meta, Super
    /// or Hyper keys `session` holds are lifted before the first character and pressed again
    /// after the last: held, they would make each character a shortcut (Ctrl+W closes a tab).
    /// Once per text rather than per character, so as not to tap them between characters.
    fn text(&mut self, session: u64, text: &str, deadline: Instant) -> Step<Progress> {
        let typed = self.type_text(session, text, deadline);
        // Also when the server rejected a request, which drops the rest of the text.
        if matches!(typed, Ok(Progress::Done) | Err(Failure::Rejected(_))) {
            for keycode in self.queue.lifted.take().unwrap_or_default().iter() {
                self.key(true, keycode)?;
            }
        }
        typed
    }

    /// `text`, but for pressing the lifted keys again.
    fn type_text(&mut self, session: u64, text: &str, deadline: Instant) -> Step<Progress> {
        // Typing leaves the keyboard as it found it, so one look serves the whole slice, and
        // another after the lift.
        let (mut state, mut down) = self.keyboard(session)?;
        let start = self.queue.typed;
        for (i, c) in text.get(start..).unwrap_or_default().char_indices() {
            // At least a character per slice.
            if i > 0 && Instant::now() >= deadline {
                self.queue.typed = start + i;
                return Ok(Progress::Yield);
            }
            let Some(keysym) = keysym::char_to_keysym(c) else {
                continue;
            };
            if self.queue.lifted.is_none() {
                // Only what the session pressed itself, as for Shift (see keymap::plan_at).
                let lifted = self.mods.shortcut_keys(&self.own_keys(session)).and(&down);
                for keycode in lifted.iter() {
                    self.key(false, keycode)?;
                }
                self.queue.lifted = Some(lifted);
                if !lifted.is_empty() {
                    (state, down) = self.keyboard(session)?;
                }
            }
            match self.resolve(keysym, state, &down)? {
                Resolved::Plan(plan) => self.type_key(plan, &down, true)?,
                Resolved::Later(us) => {
                    self.queue.typed = start + i;
                    return Ok(Progress::WaitUntil(us));
                }
                Resolved::Untypable => {}
            }
        }
        Ok(Progress::Done)
    }

    fn set_controller_present(&mut self, present: bool) -> Step {
        self.controller = present;
        if present {
            return self.keep_repeat_off();
        }
        if let Some(mode) = self.repeat_restore.take() {
            let restore = ChangeKeyboardControlAux::new().auto_repeat_mode(mode);
            self.conn.change_keyboard_control(&restore)?;
            self.record()?;
        }
        Ok(())
    }

    /// Turns server autorepeat off, remembering the mode it replaces. With it on, a key whose
    /// release the network delays keeps repeating on the server until the release arrives;
    /// clients send their own repeats instead.
    fn keep_repeat_off(&mut self) -> Step {
        self.next_repeat_check = Instant::now() + REPEAT_CHECK;
        let mode = self
            .conn
            .get_keyboard_control()?
            .reply()?
            .global_auto_repeat;
        if mode != AutoRepeatMode::OFF {
            self.repeat_restore = Some(mode);
            // Recorded first, so that a tilt dying before it restores the mode leaves word of it.
            self.record()?;
            let off = ChangeKeyboardControlAux::new().auto_repeat_mode(AutoRepeatMode::OFF);
            self.conn.change_keyboard_control(&off)?;
        }
        Ok(())
    }

    /// Rewrites the record of what tilt changed on the display (see RECORD and `repair`): the
    /// autorepeat mode to restore plus one (0 for none), what sessions hold down (see
    /// `holding`), then each spare binding's keycode and first keysym.
    fn record(&mut self) -> Step {
        self.recorded_holding = self.holding();
        let (keys, buttons) = self.recorded_holding;
        let repeat = self.repeat_restore.map_or(0, |mode| u32::from(mode) + 1);
        let bindings = self.spares.bindings().iter();
        let words: Vec<u32> = std::iter::once(repeat)
            .chain(keys.to_words())
            .chain([buttons])
            .chain(bindings.flat_map(|b| [u32::from(b.keycode), b.keysym]))
            .collect();
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            self.record_atom,
            AtomEnum::CARDINAL,
            &words,
        )?;
        Ok(())
    }

    /// Leaves the server as tilt found it: releases what sessions still hold, restores autorepeat
    /// and unbinds the spare keycodes that are still ours. Queued input is dropped.
    fn restore(&mut self) -> Result<(), ConnectionError> {
        // Forget bindings someone else took over before unbinding the rest.
        self.poll_events()?;
        let sessions: Vec<u64> = self.held.keys().copied().collect();
        for session in sessions {
            settle(self.release_all(session))?;
        }
        settle(self.set_controller_present(false))?;
        for binding in self.spares.bindings() {
            self.conn
                .change_keyboard_mapping(1, binding.keycode, 1, &[keysym::NO_SYMBOL])?;
        }
        // Nothing is left for the next tilt to undo.
        self.conn.delete_property(self.root, self.record_atom)?;
        // The process may exit right after the join, so wait until the server has applied it all.
        settle(self.sync())
    }

    fn sync(&self) -> Step {
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detached_handle_queues_commands_and_outlives_its_receiver() {
        let (input, rx) = InputHandle::detached();
        input.send(InputCmd::ReleaseAll { session: 7 });
        assert_eq!(rx.try_recv(), Ok(InputCmd::ReleaseAll { session: 7 }));
        assert_eq!(input.last_input_us(), 0);
        drop(rx);
        // With nothing receiving, commands are dropped rather than reported.
        input.send(InputCmd::SetControllerPresent(true));
    }

    #[test]
    fn cancelling_a_text_drops_its_rest_and_what_its_session_sent_after_it() {
        let text = |session| InputCmd::Text {
            session,
            text: "abc".to_owned(),
        };
        let key = |session| InputCmd::Key {
            session,
            keysym: 0x61,
            down: true,
        };
        let present = InputCmd::SetControllerPresent(false);
        let mut queue = Queue::default();
        for cmd in [text(1), key(2), present.clone(), key(1), text(1)] {
            queue.push(cmd);
        }
        let control_l: Keycodes = [37].into_iter().collect();
        (queue.typed, queue.resume_us, queue.lifted) = (2, 99, Some(control_l));

        // A session without a text: nothing changes.
        queue.cancel_text(2);
        assert_eq!(queue.cmds.len(), 5);
        assert_eq!((queue.typed, queue.resume_us, queue.text_bytes), (2, 99, 6));
        assert_eq!(queue.lifted, Some(control_l));
        // The text being typed goes, with its progress and what its session sent after it. What
        // it lifted is not pressed again.
        queue.cancel_text(1);
        assert_eq!(queue.cmds, [key(2), present]);
        assert_eq!((queue.typed, queue.resume_us, queue.text_bytes), (0, 0, 0));
        assert_eq!(queue.lifted, None);

        // A key waiting at the front for a spare keycode keeps its turn, and what the session
        // sent before its text stays.
        let mut queue = Queue::default();
        for cmd in [key(1), key(2), text(2), key(1), key(2), text(2)] {
            queue.push(cmd);
        }
        queue.resume_us = 99;
        queue.cancel_text(2);
        assert_eq!(queue.cmds, [key(1), key(2), key(1)]);
        assert_eq!((queue.resume_us, queue.text_bytes), (99, 0));
    }

    #[test]
    fn text_beyond_the_queued_text_limit_is_dropped() {
        let text = |session, bytes| InputCmd::Text {
            session,
            text: "x".repeat(bytes),
        };
        let mut queue = Queue::default();
        queue.push(text(1, MAX_QUEUED_TEXT - 10));
        queue.push(text(1, 11));
        queue.push(text(2, 11));
        assert_eq!(queue.cmds.len(), 1);
        // Only text counts.
        queue.push(InputCmd::ReleaseAll { session: 2 });
        queue.push(text(2, 10));
        assert_eq!(queue.cmds.len(), 3);
        assert_eq!(queue.text_bytes, MAX_QUEUED_TEXT);

        // Text typed in full, or cancelled, makes room.
        let typed = queue.cmds.pop_front().unwrap();
        queue.done(&typed);
        assert_eq!(queue.text_bytes, 10);
        queue.push(text(1, MAX_QUEUED_TEXT - 10));
        assert_eq!(queue.cmds.len(), 3);
        queue.cancel_text(2);
        queue.push(text(3, 10));
        assert_eq!(queue.cmds.len(), 3);
        assert_eq!(queue.text_bytes, MAX_QUEUED_TEXT);
    }
}

/// Against a private Xvfb per test, with `xev` as a real Xlib client. They need the tilt-dev
/// image: `cargo test input -- --include-ignored`.
#[cfg(test)]
mod xvfb_tests {
    use std::io::{BufRead as _, BufReader};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::AtomicU32;
    use std::thread::sleep;

    use x11rb::protocol::xproto::{ChangeWindowAttributesAux, EventMask, InputFocus, KeyButMask};

    use super::keysym::{
        char_to_keysym, is_modifier, keysym_to_char, XK_CONTROL_L, XK_CONTROL_R,
        XK_ISO_LEVEL3_SHIFT, XK_SHIFT_L, XK_TAB,
    };
    use super::*;
    use crate::control::ControlState;

    const SHIFT: u16 = 1;
    const LOCK: u16 = 1 << 1;
    const CONTROL: u16 = 1 << 2;
    const MOD5: u16 = 1 << 7;
    const BUTTON1: u16 = 1 << 8;
    const BUTTON3: u16 = 1 << 10;
    const XK_CAPS_LOCK: u32 = 0xffe5;
    const XK_ALT_L: u32 = 0xffe9;
    const XK_ISO_LEFT_TAB: u32 = 0xfe20;
    const XK_EURO_SIGN: u32 = 0x20ac;
    const XK_ISO_NEXT_GROUP: u32 = 0xfe08;
    const WAIT: Duration = Duration::from_secs(5);

    static NEXT_DISPLAY: AtomicU32 = AtomicU32::new(70);

    /// A private Xvfb, killed on drop.
    struct Xvfb {
        child: Child,
        display: String,
    }

    impl Xvfb {
        fn start(width: u16, height: u16) -> Xvfb {
            loop {
                let n = NEXT_DISPLAY.fetch_add(1, Ordering::Relaxed);
                assert!(n < 400, "no free X display number");
                if Path::new(&format!("/tmp/.X{n}-lock")).exists() {
                    continue;
                }
                let display = format!(":{n}");
                let size = format!("{width}x{height}x24");
                let mut child = Command::new("Xvfb")
                    .args([display.as_str(), "-screen", "0", size.as_str()])
                    .args(["-nolisten", "tcp", "-noreset"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("Xvfb must be installed");
                let deadline = Instant::now() + WAIT;
                while Instant::now() < deadline && child.try_wait().unwrap().is_none() {
                    if x11rb::connect(Some(&display)).is_ok() {
                        return Xvfb { child, display };
                    }
                    sleep(Duration::from_millis(20));
                }
                // Lost a race for the display number, or it never came up: try the next one.
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for Xvfb {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// The input thread under test.
    struct Input {
        handle: InputHandle,
        shutdown: Arc<AtomicBool>,
        thread: JoinHandle<()>,
    }

    impl Input {
        fn start(xvfb: &Xvfb) -> Input {
            let shutdown = Arc::new(AtomicBool::new(false));
            let (handle, thread) =
                spawn_input_thread(xvfb.display.clone(), Arc::clone(&shutdown)).unwrap();
            Input {
                handle,
                shutdown,
                thread,
            }
        }

        fn send(&self, cmd: InputCmd) {
            self.handle.send(cmd);
        }

        fn key(&self, session: u64, keysym: u32, down: bool) {
            self.send(InputCmd::Key {
                session,
                keysym,
                down,
            });
        }

        /// A KEY down, then up.
        fn press(&self, session: u64, keysym: u32) {
            self.key(session, keysym, true);
            self.key(session, keysym, false);
        }

        fn text(&self, session: u64, text: &str) {
            self.send(InputCmd::Text {
                session,
                text: text.to_owned(),
            });
        }

        fn click(&self, session: u64, button: u8, x: u16, y: u16) {
            for down in [true, false] {
                self.send(InputCmd::Button {
                    session,
                    button,
                    down,
                    x,
                    y,
                });
            }
        }

        /// Shuts the thread down the way main does and waits for its cleanup.
        fn stop(self) {
            self.shutdown.store(true, Ordering::Relaxed);
            self.thread.join().unwrap();
        }
    }

    /// What the test window received. `keysym` is the key translated with the mapping current
    /// when the event was read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Got {
        Key {
            down: bool,
            keycode: u8,
            state: u16,
            keysym: u32,
        },
        Button {
            down: bool,
            button: u8,
            x: i16,
            y: i16,
        },
        Motion {
            x: i16,
            y: i16,
        },
    }

    /// The test's own client: a focused window covering the screen, which receives the input.
    struct Client {
        conn: RustConnection,
        display: String,
        root: Window,
        window: Window,
        min_keycode: u8,
        max_keycode: u8,
        per_keycode: usize,
        keysyms: Vec<u32>,
        barriers: u16,
    }

    impl Client {
        fn new(xvfb: &Xvfb) -> Client {
            let (conn, screen) = x11rb::connect(Some(&xvfb.display)).unwrap();
            let setup = conn.setup();
            let (min_keycode, max_keycode) = (setup.min_keycode, setup.max_keycode);
            let screen = &setup.roots[screen];
            let (root, width, height) =
                (screen.root, screen.width_in_pixels, screen.height_in_pixels);
            let window = conn.generate_id().unwrap();
            let events = EventMask::KEY_PRESS
                | EventMask::KEY_RELEASE
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::POINTER_MOTION;
            let aux = CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(events);
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                width,
                height,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &aux,
            )
            .unwrap();
            conn.map_window(window).unwrap();
            conn.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
                .unwrap();
            conn.get_input_focus().unwrap().reply().unwrap();
            let mut client = Client {
                conn,
                display: xvfb.display.clone(),
                root,
                window,
                min_keycode,
                max_keycode,
                per_keycode: 0,
                keysyms: Vec::new(),
                barriers: 0,
            };
            client.refresh_keymap();
            client
        }

        fn refresh_keymap(&mut self) {
            let count = self.max_keycode - self.min_keycode + 1;
            let reply = self
                .conn
                .get_keyboard_mapping(self.min_keycode, count)
                .unwrap()
                .reply()
                .unwrap();
            self.per_keycode = usize::from(reply.keysyms_per_keycode);
            self.keysyms = reply.keysyms;
        }

        fn row(&self, keycode: u8) -> &[u32] {
            let start = usize::from(keycode - self.min_keycode) * self.per_keycode;
            &self.keysyms[start..start + self.per_keycode]
        }

        /// The whole core mapping, as the server has it now.
        fn mapping(&mut self) -> (usize, Vec<u32>) {
            self.refresh_keymap();
            (self.per_keycode, self.keysyms.clone())
        }

        /// Keycodes without keysyms, as the server has them now.
        fn spares(&mut self) -> Vec<u8> {
            self.refresh_keymap();
            (self.min_keycode..=self.max_keycode)
                .filter(|&k| self.row(k).iter().all(|&s| s == 0))
                .collect()
        }

        /// The keysym an XKB client gets for `keycode` in `state`, read off the core mapping:
        /// Mod5 (ISO_Level3_Shift here) selects levels 3-4 on keys that have them, Shift the
        /// second keysym of a pair, and Caps Lock the uppercase one of a letter pair.
        fn lookup(&self, keycode: u8, state: u16) -> u32 {
            let row = self.row(keycode);
            let at = |i: usize| row.get(i).copied().unwrap_or(0);
            let base = if state & MOD5 != 0 && (at(4) != 0 || at(5) != 0) {
                4
            } else {
                0
            };
            let (lower, upper) = (at(base), at(base + 1));
            if upper == 0 || upper == lower {
                return lower;
            }
            let letters = matches!(
                (keysym_to_char(lower), keysym_to_char(upper)),
                (Some(l), Some(u)) if l.is_lowercase() && u.is_uppercase()
            );
            let caps = state & LOCK != 0 && letters;
            if (state & SHIFT != 0) != caps {
                upper
            } else {
                lower
            }
        }

        fn key_event(&self, down: bool, keycode: u8, state: KeyButMask) -> Got {
            let state = u16::from(state);
            Got::Key {
                down,
                keycode,
                state,
                keysym: self.lookup(keycode, state),
            }
        }

        /// The next input event, or None after WAIT without one. A keyboard MappingNotify
        /// refreshes the mapping first, as Xlib does, so keys translate the way clients see them.
        fn next(&mut self) -> Option<Got> {
            let deadline = Instant::now() + WAIT;
            loop {
                let Some(event) = self.conn.poll_for_event().unwrap() else {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    sleep(Duration::from_micros(200));
                    continue;
                };
                let got = match event {
                    Event::MappingNotify(e) if e.request == Mapping::KEYBOARD => {
                        self.refresh_keymap();
                        continue;
                    }
                    Event::KeyPress(e) => self.key_event(true, e.detail, e.state),
                    Event::KeyRelease(e) => self.key_event(false, e.detail, e.state),
                    Event::ButtonPress(e) => Got::Button {
                        down: true,
                        button: e.detail,
                        x: e.root_x,
                        y: e.root_y,
                    },
                    Event::ButtonRelease(e) => Got::Button {
                        down: false,
                        button: e.detail,
                        x: e.root_x,
                        y: e.root_y,
                    },
                    Event::MotionNotify(e) => Got::Motion {
                        x: e.root_x,
                        y: e.root_y,
                    },
                    Event::Error(e) => panic!("X error on the test connection: {e:?}"),
                    _ => continue,
                };
                return Some(got);
            }
        }

        /// Moves the pointer through `input` and returns everything received before that motion
        /// arrived, by which time every earlier command has been injected.
        fn barrier(&mut self, input: &Input) -> Vec<Got> {
            self.barriers = self.barriers % 50 + 1;
            let (x, y) = (1000 * self.barriers, 1000);
            input.send(InputCmd::Move { session: 0, x, y });
            let (width, height) = self.size();
            let target = Got::Motion {
                x: px(x, width),
                y: px(y, height),
            };
            let mut before = Vec::new();
            loop {
                match self.next() {
                    Some(got) if got == target => return before,
                    Some(got) => before.push(got),
                    None => panic!("the barrier motion never arrived; got {before:?}"),
                }
            }
        }

        fn size(&self) -> (u16, u16) {
            let geometry = self.conn.get_geometry(self.root).unwrap().reply().unwrap();
            (geometry.width, geometry.height)
        }

        /// Keycodes the server has down.
        fn keys_down(&self) -> Vec<u8> {
            let keys = self.conn.query_keymap().unwrap().reply().unwrap().keys;
            (0..=255u8)
                .filter(|&k| keys[usize::from(k / 8)] & (1 << (k % 8)) != 0)
                .collect()
        }

        /// The core state: modifiers and buttons down.
        fn state(&self) -> u16 {
            let pointer = self.conn.query_pointer(self.root).unwrap().reply().unwrap();
            u16::from(pointer.mask)
        }

        fn autorepeat(&self) -> AutoRepeatMode {
            let control = self.conn.get_keyboard_control().unwrap().reply().unwrap();
            control.global_auto_repeat
        }

        fn set_autorepeat(&self, mode: AutoRepeatMode) {
            let aux = ChangeKeyboardControlAux::new().auto_repeat_mode(mode);
            self.conn.change_keyboard_control(&aux).unwrap();
            self.conn.get_input_focus().unwrap().reply().unwrap();
        }

        /// Whether any client selected `mask` on the window.
        fn selected(&self, mask: EventMask) -> bool {
            let attributes = self
                .conn
                .get_window_attributes(self.window)
                .unwrap()
                .reply()
                .unwrap();
            u32::from(attributes.all_event_masks) & u32::from(mask) != 0
        }

        /// The effective XKB group, 0 for the first.
        fn group(&self) -> u16 {
            self.state() >> 13 & 3
        }

        /// The owner of tilt's selection, and the words of its record (see RECORD).
        fn tilt_record(&self) -> (Window, Option<Vec<u32>>) {
            let atom = self
                .conn
                .intern_atom(false, RECORD)
                .unwrap()
                .reply()
                .unwrap()
                .atom;
            let owner = self
                .conn
                .get_selection_owner(atom)
                .unwrap()
                .reply()
                .unwrap();
            let record = self
                .conn
                .get_property(false, self.root, atom, AtomEnum::CARDINAL, 0, RECORD_WORDS)
                .unwrap()
                .reply()
                .unwrap();
            (owner.owner, record.value32().map(|words| words.collect()))
        }

        /// Injects one event through the test's own XTest connection, as an agent does, and
        /// waits until the server has it.
        fn fake(&self, kind: u8, detail: u8) {
            self.conn
                .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)
                .unwrap();
            self.conn.get_input_focus().unwrap().reply().unwrap();
        }

        /// Stops taking key events on the test window; buttons and motion still come.
        fn ignore_keys(&self) {
            let events =
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
            let aux = ChangeWindowAttributesAux::new().event_mask(events);
            self.conn
                .change_window_attributes(self.window, &aux)
                .unwrap();
            self.conn.get_input_focus().unwrap().reply().unwrap();
        }

        /// Taps `keycode` through the test's own XTest and consumes the events.
        fn tap(&mut self, keycode: u8) {
            self.fake(KEY_PRESS_EVENT, keycode);
            self.fake(KEY_RELEASE_EVENT, keycode);
            loop {
                match self.next() {
                    Some(Got::Key {
                        down: false,
                        keycode: k,
                        ..
                    }) if k == keycode => return,
                    Some(_) => {}
                    None => panic!("the tapped key never arrived"),
                }
            }
        }
    }

    /// What `xev` printed that the tests act on.
    enum Seen {
        /// A key press, and the keysym Xlib translated it to.
        Press {
            keycode: u8,
            keysym: u32,
        },
        MappingNotify,
    }

    /// `xev` on the test window: Xlib's own XKB translation of each key press.
    struct Xev {
        child: Child,
        seen: Receiver<Seen>,
    }

    impl Xev {
        /// Starts xev and waits until its Xlib follows keymap changes. Xlib loads its XKB keymap
        /// when it first translates a key event and only then subscribes to keymap changes, so
        /// a keycode bound around that moment can reach a fresh client as NoSymbol. That startup
        /// race is every Xlib client's, and not what these tests are about.
        fn start(client: &mut Client) -> Xev {
            let window = format!("{:#x}", client.window);
            let mut child = Command::new("stdbuf")
                .args(["-oL", "xev", "-display", client.display.as_str()])
                .args(["-id", window.as_str()])
                .args(["-event", "keyboard"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("xev and stdbuf must be installed");
            let stdout = child.stdout.take().unwrap();
            let (tx, seen) = crossbeam_channel::unbounded();
            std::thread::spawn(move || {
                let mut press = false;
                // Not lines(): xev prints XLookupString's bytes in the locale's encoding, which
                // need not be UTF-8.
                for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
                    let line = String::from_utf8_lossy(&line);
                    if line.starts_with("MappingNotify event") {
                        let _ = tx.send(Seen::MappingNotify);
                    } else if line.starts_with("KeyPress event") {
                        press = true;
                    } else if line.starts_with("KeyRelease event") {
                        press = false;
                    } else if let Some(rest) = line.split(", keycode ").nth(1) {
                        // "state 0x0, keycode 38 (keysym 0x61, a), same_screen YES,"
                        if let (true, Some((keycode, rest))) =
                            (press, rest.split_once(" (keysym 0x"))
                        {
                            let keysym = rest.split(',').next().unwrap_or_default();
                            let _ = tx.send(Seen::Press {
                                keycode: keycode.parse().unwrap(),
                                keysym: u32::from_str_radix(keysym, 16).unwrap(),
                            });
                        }
                    }
                }
            });
            // xev listens once its KeymapStateMask shows up on the window.
            let deadline = Instant::now() + WAIT;
            while !client.selected(EventMask::KEYMAP_STATE) {
                assert!(Instant::now() < deadline, "xev never selected input");
                sleep(Duration::from_millis(10));
            }
            let xev = Xev { child, seen };

            // A key event for xev to translate, on a keycode with no keysyms...
            let spare = *client.spares().first().expect("no spare keycode");
            client.tap(spare);
            assert!(xev.typed(1) == [0], "xev did not report the warm-up key");
            // ...then a keymap change that changes nothing, until xev reports one.
            let deadline = Instant::now() + WAIT;
            loop {
                client
                    .conn
                    .change_keyboard_mapping(1, spare, 1, &[0])
                    .unwrap();
                client.conn.flush().unwrap();
                if let Ok(Seen::MappingNotify) = xev.seen.recv_timeout(Duration::from_millis(20)) {
                    return xev;
                }
                assert!(Instant::now() < deadline, "xev never saw a keymap change");
            }
        }

        /// The keycodes and keysyms of the next `n` key presses.
        fn presses(&self, n: usize) -> Vec<(u8, u32)> {
            let mut presses = Vec::new();
            while presses.len() < n {
                match self.seen.recv_timeout(WAIT) {
                    Ok(Seen::Press { keycode, keysym }) => presses.push((keycode, keysym)),
                    Ok(Seen::MappingNotify) => {}
                    Err(_) => panic!("xev reported only {} of {n} key presses", presses.len()),
                }
            }
            presses
        }

        /// The non-modifier keysyms among the next `n` key presses.
        fn typed(&self, n: usize) -> Vec<u32> {
            let keysyms = self.presses(n).into_iter().map(|(_, keysym)| keysym);
            keysyms.filter(|&k| !is_modifier(k)).collect()
        }
    }

    impl Drop for Xev {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Loads a keyboard layout, as `setxkbmap` with `args` does.
    fn setxkbmap(xvfb: &Xvfb, args: &[&str]) {
        let status = Command::new("setxkbmap")
            .args(["-display", xvfb.display.as_str()])
            .args(args)
            .status()
            .expect("setxkbmap must be installed");
        assert!(status.success(), "setxkbmap {args:?}");
    }

    /// The brief's mapping, computed independently: round(v * (extent - 1) / 65535).
    fn px(v: u16, extent: u16) -> i16 {
        (f64::from(v) * f64::from(extent - 1) / 65535.0).round() as i16
    }

    fn keysyms(text: &str) -> Vec<u32> {
        text.chars().filter_map(char_to_keysym).collect()
    }

    /// The keysyms of the non-modifier key presses in `events`.
    fn typed(events: &[Got]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|got| match *got {
                Got::Key {
                    down: true, keysym, ..
                } if !is_modifier(keysym) => Some(keysym),
                _ => None,
            })
            .collect()
    }

    fn key_presses(events: &[Got]) -> usize {
        events
            .iter()
            .filter(|got| matches!(got, Got::Key { down: true, .. }))
            .count()
    }

    /// Key ('k') and button ('b') events, without positions or state.
    fn transitions(events: &[Got]) -> Vec<(char, bool, u8)> {
        events
            .iter()
            .filter_map(|got| match *got {
                Got::Key { down, keycode, .. } => Some(('k', down, keycode)),
                Got::Button { down, button, .. } => Some(('b', down, button)),
                Got::Motion { .. } => None,
            })
            .collect()
    }

    /// The transitions of typing `keycode` the way `type_key` does: the keys in `lift` released
    /// and those in `fake` pressed around its press, and its release after all that for a KEY
    /// down and up, or inside it for a tap.
    fn around(lift: &[u8], fake: &[u8], keycode: u8, tap: bool) -> Vec<(char, bool, u8)> {
        let mut t: Vec<_> = lift.iter().map(|&k| ('k', false, k)).collect();
        t.extend(fake.iter().map(|&k| ('k', true, k)));
        t.push(('k', true, keycode));
        if tap {
            t.push(('k', false, keycode));
        }
        t.extend(fake.iter().rev().map(|&k| ('k', false, k)));
        t.extend(lift.iter().map(|&k| ('k', true, k)));
        if !tap {
            t.push(('k', false, keycode));
        }
        t
    }

    /// The position of each button event in `events`.
    fn buttons_at(events: &[Got]) -> Vec<(i16, i16)> {
        events
            .iter()
            .filter_map(|got| match *got {
                Got::Button { x, y, .. } => Some((x, y)),
                _ => None,
            })
            .collect()
    }

    #[test]
    #[ignore = "needs Xvfb and xev: run in the tilt-dev image"]
    fn text_types_every_character_and_leaves_nothing_down() {
        let xvfb = Xvfb::start(1280, 720);
        let mut client = Client::new(&xvfb);
        let before = client.mapping();
        let spares = client.spares();
        let input = Input::start(&xvfb);
        let xev = Xev::start(&mut client);

        let text = "Hello, World! üé€你😀 |¦\r\n\t";
        input.text(1, text);
        let events = client.barrier(&input);
        let expected = keysyms(text);
        assert_eq!(typed(&events), expected);
        assert_eq!(
            xev.typed(key_presses(&events)),
            expected,
            "as Xlib translated them"
        );

        let press = |keysym: u32| {
            events
                .iter()
                .find_map(|got| match *got {
                    Got::Key {
                        down: true,
                        keycode,
                        state,
                        keysym: k,
                    } if k == keysym => Some((keycode, state & 0xff)),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(press(0x48).1, SHIFT, "H comes with a fake Shift");
        assert_eq!(press(0x65).1, 0, "e comes without");
        assert_eq!(
            press(0xa6),
            (94, SHIFT | MOD5),
            "brokenbar is level 4 of keycode 94"
        );
        assert_eq!(
            client.spares().len(),
            spares.len() - 5,
            "ü é € 你 😀 need spares"
        );
        assert_eq!(client.keys_down(), Vec::<u8>::new(), "keys left down");
        assert_eq!(client.state() & 0xff, 0, "modifiers left on");

        input.stop();
        assert!(
            client.mapping() == before,
            "spare keycodes were not all unbound"
        );
    }

    #[test]
    #[ignore = "needs Xvfb and xev: run in the tilt-dev image"]
    fn caps_lock_does_not_change_the_case_text_asks_for() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        let xev = Xev::start(&mut client);

        input.key(1, XK_CAPS_LOCK, true);
        input.key(1, XK_CAPS_LOCK, false);
        // Layout letters, then letters, a symbol and a CJK character on spare keycodes.
        let text = "aAbéÉüÜ€你";
        input.text(1, text);
        let events = client.barrier(&input);
        assert_eq!(client.state() & 0xff, LOCK);
        assert_eq!(typed(&events), keysyms(text));
        assert_eq!(
            xev.typed(key_presses(&events)),
            keysyms(text),
            "as Xlib translated them"
        );

        input.key(1, XK_CAPS_LOCK, true);
        input.key(1, XK_CAPS_LOCK, false);
        client.barrier(&input);
        assert_eq!(client.state() & 0xff, 0);
        assert_eq!(client.keys_down(), Vec::<u8>::new());
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn clicks_and_moves_land_on_the_exact_pixel() {
        let (width, height) = (1001, 667);
        let xvfb = Xvfb::start(width, height);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        let points = [
            (0, 0),
            (65535, 0),
            (0, 65535),
            (65535, 65535),
            (32768, 32768),
            (12345, 54321),
            (1, 65534),
        ];
        for (x, y) in points {
            let (ex, ey) = (px(x, width), px(y, height));
            input.click(1, 1, x, y);
            let click = [
                Got::Motion { x: ex, y: ey },
                Got::Button {
                    down: true,
                    button: 1,
                    x: ex,
                    y: ey,
                },
                Got::Button {
                    down: false,
                    button: 1,
                    x: ex,
                    y: ey,
                },
            ];
            assert_eq!(client.barrier(&input), click, "click at ({x}, {y})");
            input.send(InputCmd::Move { session: 1, x, y });
            let moved = [Got::Motion { x: ex, y: ey }];
            assert_eq!(client.barrier(&input), moved, "move to ({x}, {y})");
        }
        assert_eq!((px(65535, width), px(65535, height)), (1000, 666));

        // Where the pointer already is, a click adds no motion.
        let (ex, ey) = (px(40000, width), px(20000, height));
        input.send(InputCmd::Move {
            session: 1,
            x: 40000,
            y: 20000,
        });
        input.click(1, 3, 40000, 20000);
        let events = client.barrier(&input);
        assert_eq!(events[0], Got::Motion { x: ex, y: ey });
        assert_eq!(transitions(&events[1..]), [('b', true, 3), ('b', false, 3)]);
        assert_eq!(buttons_at(&events), [(ex, ey); 2]);
        assert_eq!(events.len(), 3);

        // Buttons outside 1..=10 are ignored.
        input.click(1, 0, 100, 100);
        input.click(1, 11, 100, 100);
        assert_eq!(client.barrier(&input), []);
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn wheel_notches_click_buttons_4_to_7() {
        let (width, height) = (1280, 720);
        let xvfb = Xvfb::start(width, height);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        let (x, y) = (30000, 40000);
        let wheels = [
            (0, 3),
            (0, -2),
            (1, 0),
            (-4, 0),
            (2, -1),
            (0, i16::MAX),
            (i16::MIN, 0),
        ];
        for (dx, dy) in wheels {
            input.send(InputCmd::Wheel {
                session: 1,
                dx,
                dy,
                x,
                y,
            });
        }
        let events = client.barrier(&input);
        // Vertical first, then horizontal; at most 10 notches per axis.
        let notches = [
            (5, 3),
            (4, 2),
            (7, 1),
            (6, 4),
            (4, 1),
            (7, 2),
            (5, 10),
            (6, 10),
        ];
        let expected: Vec<(char, bool, u8)> = notches
            .iter()
            .flat_map(|&(button, n)| [('b', true, button), ('b', false, button)].repeat(n))
            .collect();
        assert_eq!(transitions(&events), expected);
        let at = (px(x, width), px(y, height));
        assert!(buttons_at(&events).iter().all(|&p| p == at));
        let motions = events.iter().filter(|g| matches!(g, Got::Motion { .. }));
        assert_eq!(
            motions.count(),
            1,
            "only the first wheel had to move the pointer"
        );
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn release_all_lets_go_of_that_sessions_input_only() {
        let xvfb = Xvfb::start(1280, 720);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        // A controller is present, so keys held here do not autorepeat on the server.
        input.send(InputCmd::SetControllerPresent(true));
        // us layout keycodes: a 38, b 56, d 40, Shift_L 50.
        let (a, b, d, shift) = (0x61, 0x62, 0x64, XK_SHIFT_L);
        for keysym in [a, b, d, shift] {
            input.key(1, keysym, true);
        }
        let button = |session, button| InputCmd::Button {
            session,
            button,
            down: true,
            x: 100,
            y: 100,
        };
        input.send(button(1, 1));
        input.key(2, shift, true);
        input.send(button(2, 3));
        client.barrier(&input);
        assert_eq!(client.keys_down(), [38, 40, 50, 56]);
        assert_eq!(client.state() & (BUTTON1 | BUTTON3), BUTTON1 | BUTTON3);

        // Reverse press order, keys before buttons. Shift stays down because session 2 holds it
        // too: a modifier keeps its own key, so sessions share it.
        input.send(InputCmd::ReleaseAll { session: 1 });
        let released = [
            ('k', false, 40),
            ('k', false, 56),
            ('k', false, 38),
            ('b', false, 1),
        ];
        assert_eq!(transitions(&client.barrier(&input)), released);
        assert_eq!(client.keys_down(), [50]);
        assert_eq!(client.state() & (BUTTON1 | BUTTON3), BUTTON3);

        input.send(InputCmd::ReleaseAll { session: 1 });
        assert_eq!(transitions(&client.barrier(&input)), []);

        input.send(InputCmd::ReleaseAll { session: 2 });
        let released = [('k', false, 50), ('b', false, 3)];
        assert_eq!(transitions(&client.barrier(&input)), released);
        assert_eq!(client.keys_down(), Vec::<u8>::new());
        assert_eq!(client.state() & (BUTTON1 | BUTTON3), 0);
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn a_repeated_key_down_is_release_and_press_except_for_modifiers() {
        let xvfb = Xvfb::start(1280, 720);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        input.send(InputCmd::SetControllerPresent(true));

        for _ in 0..3 {
            input.key(1, 0x61, true);
        }
        input.key(1, 0x61, false);
        let a = [('k', true, 38), ('k', false, 38)].repeat(3);
        assert_eq!(transitions(&client.barrier(&input)), a);

        // A shifted key gets its fake Shift around each press only.
        input.key(1, 0x41, true);
        input.key(1, 0x41, true);
        input.key(1, 0x41, false);
        let events = client.barrier(&input);
        let shifted_a_then_release = [
            ('k', true, 50),
            ('k', true, 38),
            ('k', false, 50),
            ('k', false, 38),
        ];
        assert_eq!(transitions(&events), shifted_a_then_release.repeat(2));
        assert_eq!(typed(&events), [0x41, 0x41]);

        // A modifier does not repeat, and releasing what was never pressed does nothing.
        for _ in 0..3 {
            input.key(1, XK_SHIFT_L, true);
        }
        input.key(1, XK_SHIFT_L, false);
        input.key(1, 0x62, false);
        let shift = [('k', true, 50), ('k', false, 50)];
        assert_eq!(transitions(&client.barrier(&input)), shift);
        assert_eq!(client.keys_down(), Vec::<u8>::new());
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image (takes about 5 s)"]
    fn server_autorepeat_is_off_while_a_controller_is_present() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        assert_eq!(client.autorepeat(), AutoRepeatMode::ON);
        input.send(InputCmd::SetControllerPresent(true));
        client.barrier(&input);
        assert_eq!(client.autorepeat(), AutoRepeatMode::OFF);

        // Something turns it back on; the periodic re-check turns it off again.
        client.set_autorepeat(AutoRepeatMode::ON);
        let since = Instant::now();
        while client.autorepeat() != AutoRepeatMode::OFF {
            let limit = REPEAT_CHECK + Duration::from_secs(2);
            assert!(since.elapsed() < limit, "autorepeat stayed on");
            sleep(Duration::from_millis(20));
        }
        eprintln!(
            "autorepeat re-check: turned off again {:?} after it was turned on",
            since.elapsed()
        );

        input.send(InputCmd::SetControllerPresent(false));
        client.barrier(&input);
        assert_eq!(client.autorepeat(), AutoRepeatMode::ON);

        // Shutdown restores it too.
        input.send(InputCmd::SetControllerPresent(true));
        client.barrier(&input);
        assert_eq!(client.autorepeat(), AutoRepeatMode::OFF);
        input.stop();
        assert_eq!(client.autorepeat(), AutoRepeatMode::ON);
    }

    #[test]
    #[ignore = "needs Xvfb and xrandr: run in the tilt-dev image"]
    fn pointer_mapping_follows_a_randr_resize() {
        let xvfb = Xvfb::start(1280, 720);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        input.click(1, 1, 65535, 65535);
        assert_eq!(buttons_at(&client.barrier(&input)), [(1279, 719); 2]);

        let xrandr = |args: &[&str]| {
            let status = Command::new("xrandr")
                .args(["-display", xvfb.display.as_str()])
                .args(args)
                .status()
                .expect("xrandr must be installed");
            assert!(status.success(), "xrandr {args:?}");
        };
        let mode = "tilt-800x600";
        xrandr(&[
            "--newmode",
            mode,
            "0",
            "800",
            "0",
            "0",
            "0",
            "600",
            "0",
            "0",
            "0",
        ]);
        xrandr(&["--addmode", "screen", mode]);
        xrandr(&["--output", "screen", "--mode", mode]);
        assert_eq!(client.size(), (800, 600));

        for (x, y) in [(65535, 65535), (32768, 32768), (0, 0)] {
            input.click(1, 1, x, y);
            let at = (px(x, 800), px(y, 600));
            assert_eq!(
                buttons_at(&client.barrier(&input)),
                [at; 2],
                "click at ({x}, {y})"
            );
        }
        assert_eq!((px(65535, 800), px(65535, 600)), (799, 599));
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb and xev: run in the tilt-dev image"]
    fn spare_keycodes_are_reused_least_recently_used_but_never_while_held() {
        let xvfb = Xvfb::start(1280, 720);
        let mut client = Client::new(&xvfb);
        let before = client.mapping();
        let lowest = client.spares()[0];
        let spares = client.spares().len() as u32;
        let input = Input::start(&xvfb);
        let xev = Xev::start(&mut client);
        input.send(InputCmd::SetControllerPresent(true));

        // Session 2 holds 你 on a spare keycode while session 1 types twice as many distinct
        // characters as there are spares.
        let ni = 0x0100_4f60;
        input.key(2, ni, true);
        let text: String = (0x4e00..0x4e00 + 2 * spares)
            .filter_map(char::from_u32)
            .collect();
        input.text(1, &text);
        let events = client.barrier(&input);
        let expected = [vec![ni], keysyms(&text)].concat();
        assert_eq!(typed(&events), expected);
        assert_eq!(
            xev.typed(key_presses(&events)),
            expected,
            "as Xlib translated them"
        );

        let held = events
            .iter()
            .find_map(|got| match *got {
                Got::Key {
                    down: true,
                    keycode,
                    keysym,
                    ..
                } if keysym == ni => Some(keycode),
                _ => None,
            })
            .unwrap();
        assert_eq!(client.keys_down(), [held]);
        assert_eq!(
            client.spares(),
            [lowest],
            "every spare keycode is in use but the lowest, which is left to agents"
        );
        assert_eq!(client.row(held)[0], ni, "the held keycode was rebound");

        input.key(2, ni, false);
        input.stop();
        assert!(
            client.mapping() == before,
            "spare keycodes were not all unbound"
        );
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn typing_speed() {
        let xvfb = Xvfb::start(1280, 720);
        let mut client = Client::new(&xvfb);
        let spares = client.spares().len() as u32;
        let input = Input::start(&xvfb);
        let ascii =
            "The quick brown fox jumps over the lazy dog! 0123456789 THE END ~@#$%^&*()_+{}|:<>?\n";
        let distinct = 3 * spares;
        let cases = [
            ("ASCII, a third of it shifted".to_owned(), ascii.repeat(12)),
            (
                "a Latin-1 character on a spare keycode".to_owned(),
                "ü".repeat(500),
            ),
            ("an emoji on a spare keycode".to_owned(), "😀".repeat(500)),
            (
                format!(
                    "{distinct} distinct CJK characters over the {} spare keycodes tilt binds",
                    spares - 1
                ),
                (0x4e00..0x4e00 + distinct)
                    .filter_map(char::from_u32)
                    .collect(),
            ),
        ];
        for (name, text) in cases {
            // Let the previous barrier's stamp land first.
            sleep(Duration::from_millis(50));
            let chars = text.chars().count() as f64;
            let sent = clock::now_us();
            let start = Instant::now();
            input.text(1, &text);
            // Read while tilt types, as an application does: one more than REBIND_GUARD_US behind
            // would translate a rebound spare keycode with its new keysym.
            let events = client.barrier(&input);
            let delivered = start.elapsed();
            // The stamp lands just after the flush that carried the barrier's motion.
            sleep(Duration::from_millis(20));
            let injected = Duration::from_micros(input.handle.last_input_us().saturating_sub(sent));
            assert_eq!(typed(&events), keysyms(&text), "{name}");
            eprintln!(
                "typing speed, {name}: {chars} chars injected in {injected:?} ({:.1} us/char), \
                 all events at the client after {delivered:?} ({:.1} us/char)",
                injected.as_secs_f64() * 1e6 / chars,
                delivered.as_secs_f64() * 1e6 / chars,
            );
        }
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb and xev: run in the tilt-dev image"]
    fn the_sessions_own_shift_is_lifted_for_what_its_client_typed_without_it() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        let xev = Xev::start(&mut client);
        input.send(InputCmd::SetControllerPresent(true));

        // German and French clients send / = 1 . for keys their users type with Shift held;
        // ? @ and Tab come with Shift on a US server too, and Shift+Tab stays ISO_Left_Tab.
        input.key(1, XK_SHIFT_L, true);
        for keysym in [0x2f, 0x3d, 0x31, 0x2e, 0x3f, 0x40, XK_TAB] {
            input.press(1, keysym);
        }
        input.text(1, "a/1A");
        input.key(1, XK_SHIFT_L, false);
        let events = client.barrier(&input);

        // us keycodes: Shift_L 50, / 61, = 21, 1 10, . 60, 2 11, Tab 23, a 38.
        let lifted = |keycode, tap| around(&[50], &[], keycode, tap);
        let plain = |keycode, tap| around(&[], &[], keycode, tap);
        let expected = [
            vec![('k', true, 50)],
            lifted(61, false),
            lifted(21, false),
            lifted(10, false),
            lifted(60, false),
            plain(61, false),
            plain(11, false),
            plain(23, false),
            lifted(38, true),
            lifted(61, true),
            lifted(10, true),
            plain(38, true),
            vec![('k', false, 50)],
        ]
        .concat();
        assert_eq!(transitions(&events), expected);
        let want = [
            0x2f,
            0x3d,
            0x31,
            0x2e,
            0x3f,
            0x40,
            XK_ISO_LEFT_TAB,
            0x61,
            0x2f,
            0x31,
            0x41,
        ];
        assert_eq!(typed(&events), want);
        assert_eq!(
            xev.typed(key_presses(&events)),
            want,
            "as Xlib translated them"
        );
        assert_eq!(client.keys_down(), Vec::<u8>::new());
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb, setxkbmap and xev: run in the tilt-dev image"]
    fn on_a_german_server_the_sessions_shift_and_altgr_are_lifted_where_they_must_be() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        setxkbmap(&xvfb, &["de"]);
        let input = Input::start(&xvfb);
        let xev = Xev::start(&mut client);
        input.send(InputCmd::SetControllerPresent(true));

        // What a US client sends with Shift held. On de, { @ | are AltGr+7, AltGr+q and AltGr+<
        // without Shift, + and # need no Shift, and & ? need it.
        input.key(1, XK_SHIFT_L, true);
        for keysym in keysyms("{@|&?+#") {
            input.press(1, keysym);
        }
        input.key(1, XK_SHIFT_L, false);
        // What a client sends with its AltGr held, where it has no third level on q and e.
        input.key(1, XK_ISO_LEVEL3_SHIFT, true);
        for keysym in keysyms("q@e") {
            input.press(1, keysym);
        }
        input.key(1, XK_ISO_LEVEL3_SHIFT, false);
        let events = client.barrier(&input);

        // de keycodes: Shift_L 50, ISO_Level3_Shift 92 (on Mod5), 7 16, q 24, < 94, 6 15, ß 20,
        // + 35, # 51, e 26.
        let expected = [
            vec![('k', true, 50)],
            around(&[50], &[92], 16, false),
            around(&[50], &[92], 24, false),
            around(&[50], &[92], 94, false),
            around(&[], &[], 15, false),
            around(&[], &[], 20, false),
            around(&[50], &[], 35, false),
            around(&[50], &[], 51, false),
            vec![('k', false, 50), ('k', true, 92)],
            around(&[92], &[], 24, false),
            around(&[], &[], 24, false),
            around(&[92], &[], 26, false),
            vec![('k', false, 92)],
        ]
        .concat();
        assert_eq!(transitions(&events), expected);
        assert_eq!(
            xev.typed(key_presses(&events)),
            keysyms("{@|&?+#q@e"),
            "as Xlib translated them"
        );
        assert_eq!(client.keys_down(), Vec::<u8>::new());
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn text_types_with_the_sessions_shortcut_modifiers_lifted_for_its_whole_length() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        input.send(InputCmd::SetControllerPresent(true));
        // The keycode and modifiers of each character's press.
        let presses = |events: &[Got]| -> Vec<(u8, u16)> {
            events
                .iter()
                .filter_map(|got| match *got {
                    Got::Key {
                        down: true,
                        keycode,
                        state,
                        keysym,
                    } if !is_modifier(keysym) => Some((keycode, state & 0xff)),
                    _ => None,
                })
                .collect()
        };

        // us keycodes: Control_L 37, Alt_L 64, Shift_L 50, Control_R 105, a 38, b 56, / 61.
        // Control and Alt go up before the text and down again after it. With them up, the
        // session's own Shift is lifted around each character that must not have it, as in any
        // text.
        let held = [XK_CONTROL_L, XK_ALT_L, XK_SHIFT_L];
        for keysym in held {
            input.key(1, keysym, true);
        }
        input.text(1, "a/");
        for keysym in held.into_iter().rev() {
            input.key(1, keysym, false);
        }
        let events = client.barrier(&input);
        let expected = [
            vec![('k', true, 37), ('k', true, 64), ('k', true, 50)],
            vec![('k', false, 37), ('k', false, 64)],
            around(&[50], &[], 38, true),
            around(&[50], &[], 61, true),
            vec![('k', true, 37), ('k', true, 64)],
            vec![('k', false, 50), ('k', false, 64), ('k', false, 37)],
        ]
        .concat();
        assert_eq!(transitions(&events), expected);
        assert_eq!(presses(&events), [(38, 0), (61, 0)]);

        // An agent's Control is not the session's to lift: the text types with it.
        client.fake(KEY_PRESS_EVENT, 105);
        client.barrier(&input);
        input.key(1, XK_CONTROL_L, true);
        input.text(1, "b");
        input.key(1, XK_CONTROL_L, false);
        let events = client.barrier(&input);
        let around_b = [
            ('k', true, 37),
            ('k', false, 37),
            ('k', true, 56),
            ('k', false, 56),
            ('k', true, 37),
            ('k', false, 37),
        ];
        assert_eq!(transitions(&events), around_b);
        assert_eq!(presses(&events), [(56, CONTROL)]);
        assert_eq!(client.keys_down(), [105]);
        client.fake(KEY_RELEASE_EVENT, 105);
        client.barrier(&input);

        // A text cancelled midway leaves what it lifted up, and the next text lifts afresh.
        input.key(1, XK_CONTROL_L, true);
        let text: String = (0x4e00..0x4e00 + 200).filter_map(char::from_u32).collect();
        input.text(1, &text);
        let mut started = Vec::new();
        while presses(&started).is_empty() {
            started.push(client.next().expect("the text never started"));
        }
        input.send(InputCmd::ReleaseAll { session: 1 });
        input.text(1, "b");
        let rest = client.barrier(&input);
        assert_eq!(
            transitions(&started)[..2],
            [('k', true, 37), ('k', false, 37)]
        );
        assert!(
            !transitions(&rest).contains(&('k', true, 37)),
            "Control went down again: {rest:?}"
        );
        assert_eq!(presses(&rest).last(), Some(&(56, 0)));
        assert_eq!(client.keys_down(), Vec::<u8>::new());
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn what_an_agent_holds_is_neither_lifted_nor_released() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        input.send(InputCmd::SetControllerPresent(true));
        client.barrier(&input);
        // An agent (the test's own XTest connection) holds Shift and button 1.
        client.fake(KEY_PRESS_EVENT, 50);
        client.fake(BUTTON_PRESS_EVENT, 1);
        let held = [('k', true, 50), ('b', true, 1)];
        assert_eq!(transitions(&client.barrier(&input)), held);

        // Its Shift is not the session's to lift, so / comes out as ?. The server ignores the
        // session's presses of the key and button the agent holds, and tilt sends no releases
        // for them, not even for RELEASE_ALL.
        input.press(1, 0x2f);
        input.press(1, XK_SHIFT_L);
        input.click(1, 1, 100, 100);
        input.key(1, XK_SHIFT_L, true);
        input.send(InputCmd::Button {
            session: 1,
            button: 1,
            down: true,
            x: 100,
            y: 100,
        });
        input.send(InputCmd::ReleaseAll { session: 1 });
        let events = client.barrier(&input);
        assert_eq!(transitions(&events), [('k', true, 61), ('k', false, 61)]);
        assert_eq!(typed(&events), [0x3f]);
        assert_eq!(client.keys_down(), [50]);
        assert_eq!(client.state() & (SHIFT | BUTTON1), SHIFT | BUTTON1);

        // Once the agent lets go, the session's presses are its own again.
        client.fake(KEY_RELEASE_EVENT, 50);
        client.fake(BUTTON_RELEASE_EVENT, 1);
        input.press(1, XK_SHIFT_L);
        input.click(1, 1, 100, 100);
        let released_then_own = [
            ('k', false, 50),
            ('b', false, 1),
            ('k', true, 50),
            ('k', false, 50),
            ('b', true, 1),
            ('b', false, 1),
        ];
        assert_eq!(transitions(&client.barrier(&input)), released_then_own);
        assert_eq!(client.keys_down(), Vec::<u8>::new());
        assert_eq!(client.state() & (SHIFT | BUTTON1), 0);
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn a_key_that_is_down_is_typed_on_another_keycode() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        input.send(InputCmd::SetControllerPresent(true));
        client.barrier(&input);
        let e = 26;
        assert_eq!(client.row(e)[0], 0x65);
        client.fake(KEY_PRESS_EVENT, e);
        assert_eq!(transitions(&client.barrier(&input)), [('k', true, e)]);

        // On the key the agent holds, the press would type nothing and the release would let
        // go of the agent's e.
        input.text(1, "e");
        input.press(1, 0x65);
        let events = client.barrier(&input);
        let spare = transitions(&events).first().map_or(e, |t| t.2);
        assert_ne!(spare, e, "typed on the agent's key: {events:?}");
        let tap = [('k', true, spare), ('k', false, spare)];
        assert_eq!(transitions(&events), [tap, tap].concat());
        assert_eq!(typed(&events), [0x65, 0x65]);
        assert_eq!(client.keys_down(), [e]);

        // Likewise while the session holds e itself: it stays down until the session lets go.
        client.fake(KEY_RELEASE_EVENT, e);
        input.key(1, 0x65, true);
        input.text(1, "e");
        input.key(1, 0x65, false);
        let mut own = vec![('k', false, e), ('k', true, e)];
        own.extend(tap);
        own.push(('k', false, e));
        assert_eq!(transitions(&client.barrier(&input)), own);
        assert_eq!(client.keys_down(), Vec::<u8>::new());
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb, setxkbmap and xev: run in the tilt-dev image"]
    fn the_active_group_decides_which_key_types_a_keysym() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        let xev = Xev::start(&mut client);
        // The server rewrites an event's state in place for each client it delivers the event
        // to, and gives one without XKB, like this test's, the core state, which has no group.
        // xev, served after it on the same window, would get that state too, so only xev takes
        // the keys here.
        client.ignore_keys();
        // Two groups, Scroll Lock (78) switching between them, loaded while tilt runs.
        setxkbmap(&xvfb, &["-layout", "us,ru", "-option", "grp:sclk_toggle"]);
        let next_group = |client: &Client| {
            client.fake(KEY_PRESS_EVENT, 78);
            client.fake(KEY_RELEASE_EVENT, 78);
            assert_eq!(xev.presses(1), [(78, XK_ISO_NEXT_GROUP)]);
            client.group()
        };
        assert_eq!(next_group(&client), 1, "the second group is locked");

        // In group 2, a's and b's keys (38 and 56) type Cyrillic, so a and b take spare
        // keycodes, and legacy Cyrillic_ef is typed on a's key.
        let ef = 0x6c6;
        input.text(1, "ab");
        input.press(1, ef);
        client.barrier(&input);
        let (keycodes, keysyms): (Vec<u8>, Vec<u32>) = xev.presses(3).into_iter().unzip();
        assert!(
            keycodes[..2].iter().all(|k| ![38, 56].contains(k)),
            "{keycodes:?}"
        );
        assert_eq!(keycodes[2], 38);
        assert_eq!(keysyms, [0x61, 0x62, ef], "as Xlib translated them");

        assert_eq!(next_group(&client), 0);
        input.text(1, "ab");
        client.barrier(&input);
        assert_eq!(xev.presses(2), [(38, 0x61), (56, 0x62)]);

        // In us,de, us has two levels on e and de four, so de's EuroSign takes the place of
        // group 1's level 3: AltGr+e would type e in group 1, and a spare keycode is used.
        setxkbmap(&xvfb, &["-layout", "us,de"]);
        input.press(1, XK_EURO_SIGN);
        client.barrier(&input);
        let presses = xev.presses(1);
        assert!(
            matches!(presses[..], [(keycode, XK_EURO_SIGN)] if keycode != 26),
            "as Xlib translated it: {presses:?}"
        );
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb and xdotool: run in the tilt-dev image"]
    fn xdotool_still_types_after_tilt_took_every_spare_keycode_it_may() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let spares = client.spares();
        let lowest = spares[0];
        // Without an empty keycode, xdotool takes these, whose first keysym is NoSymbol, and
        // wipes one each time.
        let fallbacks: Vec<Vec<u32>> = (204..=207).map(|k| client.row(k).to_vec()).collect();
        let input = Input::start(&xvfb);
        let text: String = (0x4e00..0x4e00 + 2 * spares.len() as u32)
            .filter_map(char::from_u32)
            .collect();
        input.text(1, &text);
        client.barrier(&input);
        assert_eq!(
            client.spares(),
            [lowest],
            "the lowest spare is left to agents"
        );

        let xdotool = Command::new("xdotool")
            .args(["type", "ñ"])
            .env("DISPLAY", &xvfb.display)
            .env("LC_ALL", "C.UTF-8")
            .output()
            .expect("xdotool must be installed");
        let stderr = String::from_utf8_lossy(&xdotool.stderr);
        assert!(xdotool.status.success(), "xdotool type failed: {stderr}");
        let keycode = loop {
            match client.next() {
                Some(Got::Key {
                    down: true,
                    keycode,
                    ..
                }) => break keycode,
                Some(_) => {}
                None => panic!("xdotool's key never arrived"),
            }
        };
        assert_eq!(keycode, lowest, "xdotool types on the lowest empty keycode");
        client.refresh_keymap();
        let after: Vec<Vec<u32>> = (204..=207).map(|k| client.row(k).to_vec()).collect();
        assert_eq!(after, fallbacks);
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn the_next_tilt_undoes_what_one_that_died_left_changed() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let before = client.mapping();
        assert_eq!(client.autorepeat(), AutoRepeatMode::ON);
        // An agent holds Control_R and button 1 throughout.
        client.fake(KEY_PRESS_EVENT, 105);
        client.fake(BUTTON_PRESS_EVENT, 1);

        // A tilt that dies without restoring anything, as on SIGKILL: a controller present (so
        // autorepeat off), two spare keycodes bound, and its session holding Shift, button 3 and
        // the agent's Control_R and button 1, after a press and release of Alt.
        let mut dead = Injector::connect(&xvfb.display, Arc::new(AtomicBool::new(false))).unwrap();
        let session = 1;
        let key = |keysym, down| InputCmd::Key {
            session,
            keysym,
            down,
        };
        let press = |button| InputCmd::Button {
            session,
            button,
            down: true,
            x: 100,
            y: 100,
        };
        let deadline = Instant::now() + WAIT;
        for cmd in [
            InputCmd::SetControllerPresent(true),
            key(XK_SHIFT_L, true),
            InputCmd::Text {
                session,
                text: "ü你".to_owned(),
            },
            key(XK_ALT_L, true),
            key(XK_ALT_L, false),
            key(XK_CONTROL_R, true),
            press(1),
            press(3),
        ] {
            // Each in a run of its own, as input arrives, so that the record follows each.
            dead.accept(cmd);
            while !dead.queue.cmds.is_empty() {
                assert!(Instant::now() < deadline, "the input never ran");
                dead.run_queue().unwrap();
            }
        }
        dead.sync().unwrap();
        // Alt is up again, and an agent may take it.
        client.fake(KEY_PRESS_EVENT, 64);
        // Its connection closes, as when its process dies.
        drop(dead);
        while client.tilt_record().0 != x11rb::NONE {
            assert!(
                Instant::now() < deadline,
                "the dead tilt still owns its selection"
            );
            sleep(Duration::from_millis(10));
        }
        let record = client
            .tilt_record()
            .1
            .expect("the dead tilt left no record");
        let (header, bindings) = record.split_first_chunk::<RECORD_HEADER>().unwrap();
        let [repeat, keys @ .., buttons] = *header;
        // Autorepeat was ON (recorded plus one). Of the keys and buttons down, Shift and button 3
        // are its own presses. Then each binding's keycode and first keysym.
        assert_eq!(repeat, u32::from(AutoRepeatMode::ON) + 1);
        assert_eq!(Keycodes::from_words(keys).iter().collect::<Vec<_>>(), [50]);
        assert_eq!(buttons, 1 << 3);
        client.refresh_keymap();
        assert_eq!(bindings.len(), 4, "{record:?}");
        for binding in bindings.chunks(2) {
            let keycode = u8::try_from(binding[0]).unwrap();
            assert_eq!(client.row(keycode)[0], binding[1]);
        }
        assert_eq!(client.autorepeat(), AutoRepeatMode::OFF);
        assert_eq!(client.keys_down(), [50, 64, 105]);
        assert_eq!(client.state() & (BUTTON1 | BUTTON3), BUTTON1 | BUTTON3);

        let input = Input::start(&xvfb);
        client.barrier(&input);
        assert_eq!(
            client.autorepeat(),
            AutoRepeatMode::ON,
            "autorepeat stayed off"
        );
        assert!(client.mapping() == before, "spare keycodes stayed bound");
        assert_eq!(
            client.tilt_record().1,
            None,
            "the record outlived the repair"
        );
        // What it held is up again; what the agent holds is not.
        assert_eq!(client.keys_down(), [64, 105]);
        assert_eq!(client.state() & (BUTTON1 | BUTTON3), BUTTON1);
        input.stop();
        client.fake(KEY_RELEASE_EVENT, 64);
        client.fake(KEY_RELEASE_EVENT, 105);
        client.fake(BUTTON_RELEASE_EVENT, 1);
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn a_panic_on_the_input_thread_still_restores_the_display() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let before = client.mapping();
        let mut injector =
            Injector::connect(&xvfb.display, Arc::new(AtomicBool::new(false))).unwrap();
        // As in the_next_tilt_undoes_what_one_that_died_left_changed: autorepeat off, Shift
        // down, a spare keycode bound.
        for cmd in [
            InputCmd::SetControllerPresent(true),
            InputCmd::Key {
                session: 1,
                keysym: XK_SHIFT_L,
                down: true,
            },
            InputCmd::Text {
                session: 1,
                text: "ü".to_owned(),
            },
        ] {
            injector.accept(cmd);
        }
        while !injector.queue.cmds.is_empty() {
            injector.run_queue().unwrap();
        }
        injector.sync().unwrap();
        assert_eq!(client.keys_down(), [50]);
        assert_eq!(client.autorepeat(), AutoRepeatMode::OFF);
        assert!(client.mapping() != before);

        // A text the queue never counted, so finishing it underflows the count, which panics in a
        // debug build: a stand-in for any bug that panics the thread (its message is expected).
        injector.queue.cmds.push_back(InputCmd::Text {
            session: 1,
            text: "a".to_owned(),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let thread = std::thread::spawn(move || injector.run(&rx, &AtomicU64::new(0)));
        let deadline = Instant::now() + WAIT;
        while !thread.is_finished() {
            assert!(Instant::now() < deadline, "the input thread did not panic");
            sleep(Duration::from_millis(10));
        }
        assert!(thread.join().is_err(), "the panic was not passed on");
        drop(tx);
        assert_eq!(client.keys_down(), Vec::<u8>::new(), "Shift stayed down");
        assert_eq!(
            client.autorepeat(),
            AutoRepeatMode::ON,
            "autorepeat stayed off"
        );
        assert!(client.mapping() == before, "the spare keycode stayed bound");
        assert_eq!(
            client.tilt_record().1,
            None,
            "the record outlived the restore"
        );
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn text_stops_as_soon_as_its_session_lets_go() {
        let xvfb = Xvfb::start(640, 480);
        let mut client = Client::new(&xvfb);
        let input = Input::start(&xvfb);
        let control = ControlState::new(input.handle.clone());
        // Each way's text, and how many of its characters may still come after the session lets
        // go. A thousand characters outside the layout each bind a spare keycode, over 5 s of
        // typing, and come in bursts of a spare keycode each. 40,000 digits on the layout take
        // about half a second, and come a slice (a few hundred) at a time.
        let cjk = |n: u32| -> String {
            let first = 0x4e00 + 1000 * n;
            (first..first + 1000).filter_map(char::from_u32).collect()
        };
        let digits = "0123456789".repeat(4000);
        let ways = [
            ("another session takes control", cjk(0), 20),
            ("the session sends RELEASE_ALL", cjk(1), 20),
            ("the session disconnects", cjk(2), 20),
            ("the session sends RELEASE_ALL in digits", digits, 2000),
        ];
        for (i, (how, text, most_after)) in ways.into_iter().enumerate() {
            let (typist, other) = (10 * i as u64 + 1, 10 * i as u64 + 2);
            control.take(typist);
            let text = InputCmd::Text {
                session: typist,
                text,
            };
            assert!(control.input_if_holder(typist, text));
            // Well into it: for the CJK, where every character waits to rebind a spare keycode.
            let mut typed_before = 0;
            while typed_before < 40 {
                match client.next() {
                    Some(Got::Key { down: true, .. }) => typed_before += 1,
                    Some(_) => {}
                    None => panic!("{how}: the text never started"),
                }
            }

            let let_go = Instant::now();
            let next = match i {
                0 => {
                    control.take(other);
                    other
                }
                1 | 3 => {
                    let release = InputCmd::ReleaseAll { session: typist };
                    assert!(control.input_if_holder(typist, release));
                    typist
                }
                _ => {
                    // As session.rs tears a session down.
                    input.send(InputCmd::ReleaseAll { session: typist });
                    control.unregister(typist);
                    control.take(other);
                    other
                }
            };
            for down in [true, false] {
                let a = InputCmd::Key {
                    session: next,
                    keysym: 0x61,
                    down,
                };
                assert!(control.input_if_holder(next, a));
            }
            let mut typed_after = 0;
            let latency = loop {
                match client.next() {
                    Some(Got::Key {
                        down: true,
                        keycode: 38,
                        ..
                    }) => break let_go.elapsed(),
                    Some(Got::Key { down: true, .. }) => typed_after += 1,
                    Some(_) => {}
                    None => panic!("{how}: a never arrived"),
                }
            };
            let rest = client.barrier(&input);
            eprintln!(
                "text cancellation, {how}: the next key arrived {latency:?} after, with \
                 {typed_after} more characters of the text before it"
            );
            assert_eq!(key_presses(&rest), 0, "{how}: the text went on: {rest:?}");
            assert!(
                typed_after < most_after,
                "{how}: the text went on for {typed_after} characters"
            );
            // Rather than for a spare keycode's rebind guard, or the rest of the text.
            assert!(
                latency < Duration::from_millis(50),
                "{how}: the next key waited {latency:?}"
            );
            control.release(next);
        }
        client.barrier(&input);
        assert_eq!(client.keys_down(), Vec::<u8>::new());
        input.stop();
    }

    #[test]
    #[ignore = "needs Xvfb: run in the tilt-dev image"]
    fn the_thread_sleeps_only_until_a_waiting_command_may_run() {
        let xvfb = Xvfb::start(640, 480);
        let mut injector =
            Injector::connect(&xvfb.display, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(injector.idle_wait(), IDLE_POLL, "with nothing queued");
        let text = InputCmd::Text {
            session: 1,
            text: "你".to_owned(),
        };
        injector.accept(text);
        assert_eq!(
            injector.idle_wait(),
            Duration::ZERO,
            "with a command to run"
        );

        // Waiting for a spare keycode's rebind guard: until it ends, rather than IDLE_POLL
        // (which would slow text outside the layout to a spare keycode per IDLE_POLL), and
        // without spinning.
        let resume_us = clock::now_us() + 200_000;
        injector.queue.resume_us = resume_us;
        let wait = injector.idle_wait();
        let least = Duration::from_micros(resume_us.saturating_sub(clock::now_us()));
        assert!(
            least <= wait && wait <= Duration::from_millis(200),
            "waited {wait:?}"
        );
    }
}
