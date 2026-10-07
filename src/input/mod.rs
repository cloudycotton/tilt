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
mod xvfb_tests;
