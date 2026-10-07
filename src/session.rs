//! One /stream connection: handshake, reader and writer tasks, and its encoder worker
//! (brief section 5.5).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::auth::{AuthError, Role, AUTH_FAIL_DELAY};
use crate::clock::{self, LogEvery};
use crate::control::ControlStatus;
use crate::input::InputCmd;
use crate::protocol::{self, ClientMsg, ClientText, ErrorCode, ScreenSize, ServerText};
use crate::server::AppState;
use crate::worker::{self, Inbox};
use crate::x11::cursor::CursorState;

/// Shown to clients as `s<id>`.
pub type SessionId = u64;

/// A message for the session's writer task.
#[derive(Debug)]
pub enum OutMsg {
    Binary(Bytes),
    Text(String),
}

impl OutMsg {
    fn into_message(self) -> Message {
        match self {
            OutMsg::Binary(b) => Message::Binary(b),
            OutMsg::Text(t) => Message::Text(t.into()),
        }
    }
}

/// A row of the /api/status `sessions` list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionStatus {
    pub id: String,
    pub role: Role,
    pub control: bool,
    pub fps: f32,
    pub kbps: f32,
    pub rtt_ms: f32,
}

/// Live sessions: admission against `--max-viewers`, ids, and the numbers behind /api/status.
pub struct Sessions {
    max: usize,
    /// Shared with the cursor thread, which polls faster while anyone watches.
    viewers: Arc<AtomicUsize>,
    next_id: AtomicU64,
    live: Mutex<HashMap<SessionId, Live>>,
    /// Set once at shutdown; every session closes with 1001. (Addition to the skeleton.)
    closing: watch::Sender<bool>,
    /// Mirrors the number of live sessions, for `drained`.
    live_count: watch::Sender<usize>,
}

struct Live {
    role: Role,
    fps: f32,
    kbps: f32,
    rtt_ms: f32,
    liveness: Arc<Liveness>,
    /// Told to close to make room; no longer counts against `--max-viewers`.
    evicted: bool,
}

/// When a session last heard from its client, and the flag that closes it to make room for a
/// new one (see `Sessions::register`).
#[derive(Debug)]
pub struct Liveness {
    /// clock::now_us() at the last message from the client.
    heard_us: AtomicU64,
    evict: watch::Sender<bool>,
}

impl Liveness {
    fn heard(&self) {
        self.heard_us.store(clock::now_us(), Ordering::Relaxed);
    }
}

/// Clients PING every second; one silent this long has most likely lost its network.
const EVICT_SILENT_US: u64 = 5_000_000;

/// Where this server's session ids start. Random, so that a page reconnecting to a restarted
/// server does not take a new session holding control for its own lost one (see
/// `ControlStatus::holder`), but for a one-in-a-million chance. (Addition to brief 5.5, whose
/// ids start at 1.)
fn first_session_id() -> u64 {
    // Fails only without any entropy source; ids then start at 1.
    1 + u64::from(getrandom::u32().unwrap_or(0) % 1_000_000)
}

impl Sessions {
    pub fn new(max_viewers: usize) -> Sessions {
        Sessions {
            max: max_viewers,
            viewers: Arc::new(AtomicUsize::new(0)),
            next_id: AtomicU64::new(first_session_id()),
            live: Mutex::new(HashMap::new()),
            closing: watch::channel(false).0,
            live_count: watch::channel(0).0,
        }
    }

    /// The live session count, shared with the cursor thread.
    pub fn viewer_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.viewers)
    }

    pub fn count(&self) -> usize {
        self.viewers.load(Ordering::Relaxed)
    }

    /// Admits a session and counts it as a viewer; None when the server is shutting down, or
    /// when `--max-viewers` are live and none of them has been silent for 5 s. If one has, the
    /// longest-silent one is told to close (through its Liveness) to make room: a phone that
    /// changes networks leaves a dead session behind, which would otherwise hold its place
    /// for up to 30 s (addition to brief 4.1).
    pub fn register(&self, role: Role) -> Option<(SessionId, Arc<Liveness>)> {
        self.admit(role, clock::now_us())
    }

    fn admit(&self, role: Role, now_us: u64) -> Option<(SessionId, Arc<Liveness>)> {
        let mut live = self.lock();
        if self.is_closing() {
            return None;
        }
        if live.values().filter(|s| !s.evicted).count() >= self.max {
            let heard = |s: &Live| s.liveness.heard_us.load(Ordering::Relaxed);
            let (&silent, s) = live
                .iter_mut()
                .filter(|(_, s)| !s.evicted)
                .min_by_key(|(_, s)| heard(s))
                .filter(|(_, s)| now_us.saturating_sub(heard(s)) > EVICT_SILENT_US)?;
            s.evicted = true;
            s.liveness.evict.send_replace(true);
            info!(
                session = silent,
                "closing: silent for over 5 s, and a new client needs its place"
            );
        }
        let sid = self.next_id.fetch_add(1, Ordering::Relaxed);
        let liveness = Arc::new(Liveness {
            heard_us: AtomicU64::new(now_us),
            evict: watch::channel(false).0,
        });
        live.insert(
            sid,
            Live {
                role,
                fps: 0.0,
                kbps: 0.0,
                rtt_ms: 0.0,
                liveness: Arc::clone(&liveness),
                evicted: false,
            },
        );
        self.counted(live.len());
        Some((sid, liveness))
    }

    pub fn unregister(&self, sid: SessionId) {
        let mut live = self.lock();
        if live.remove(&sid).is_some() {
            self.counted(live.len());
        }
    }

    /// The worker's per-second numbers.
    pub fn update_stats(&self, sid: SessionId, fps: f32, kbps: f32, rtt_ms: f32) {
        if let Some(s) = self.lock().get_mut(&sid) {
            (s.fps, s.kbps, s.rtt_ms) = (fps, kbps, rtt_ms);
        }
    }

    /// Rows for /api/status; `holder` is the session holding control.
    pub fn snapshot(&self, holder: Option<SessionId>) -> Vec<SessionStatus> {
        let live = self.lock();
        let mut ids: Vec<SessionId> = live.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .map(|id| {
                let s = &live[&id];
                SessionStatus {
                    id: format!("s{id}"),
                    role: s.role,
                    control: holder == Some(id),
                    fps: s.fps,
                    kbps: s.kbps,
                    rtt_ms: s.rtt_ms,
                }
            })
            .collect()
    }

    /// Tells every session to close (1001) and refuses new ones.
    pub fn close_all(&self) {
        self.closing.send_replace(true);
    }

    pub fn is_closing(&self) -> bool {
        *self.closing.borrow()
    }

    /// Resolves once no session is live (after `close_all`, once each has torn down).
    pub async fn drained(&self) {
        let mut rx = self.live_count.subscribe();
        let _ = rx.wait_for(|&n| n == 0).await;
    }

    fn closing(&self) -> watch::Receiver<bool> {
        self.closing.subscribe()
    }

    fn counted(&self, n: usize) {
        self.viewers.store(n, Ordering::Relaxed);
        self.live_count.send_replace(n);
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_CLOSE: Duration = Duration::from_secs(30);
const HELD_RELEASE: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MALFORMED: u32 = 100;
const OUT_CAPACITY: usize = 64;
const REPLY_CAPACITY: usize = 32;
/// How long teardown waits for the encoder thread before leaving it to finish on its own.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

type Sink = SplitSink<WebSocket, Message>;
type Stream = SplitStream<WebSocket>;

fn close_frame(code: u16, reason: &'static str) -> CloseFrame {
    CloseFrame {
        code,
        reason: reason.into(),
    }
}

/// Runs one upgraded /stream connection until it closes.
pub async fn run(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();
    let Some((sid, role, liveness)) = handshake(&mut sink, &mut stream, &state).await else {
        return;
    };
    info!(session = sid, ?role, "connected");

    let inbox = Inbox::new();
    let (out_tx, out_rx) = mpsc::channel(OUT_CAPACITY);
    let (reply_tx, reply_rx) = mpsc::channel(REPLY_CAPACITY);
    state.hub.subscribe(sid, inbox.waker());
    let control_rx = state.control.register(sid);
    let worker = worker::spawn_worker(sid, Arc::clone(&state), Arc::clone(&inbox), out_tx);

    let (outcome, why) = match &worker {
        Ok(_) => {
            let (w, h) = state.hub.screen_size();
            let welcome = ServerText::Welcome {
                v: protocol::VERSION,
                session: format!("s{sid}"),
                role,
                screen: ScreenSize { w, h },
                server: protocol::SERVER_ID,
            };
            let mut reader = Reader {
                sid,
                role,
                state: &state,
                inbox: &inbox,
                liveness: &liveness,
                replies: &reply_tx,
                malformed: 0,
                held_keys: HashSet::new(),
                held_buttons: 0,
                text_cut: LogEvery::new(Duration::from_secs(10)),
            };
            let writer = Writer {
                sink: &mut sink,
                inbox: &inbox,
                cursor_serial: None,
                cursor_pos: None,
            };
            tokio::select! {
                r = reader.run(&mut stream) => r,
                r = writer.run(
                    welcome.to_json(),
                    out_rx,
                    reply_rx,
                    state.cursor.clone(),
                    control_rx,
                    state.sessions.closing(),
                ) => r,
            }
        }
        Err(e) => {
            error!(session = sid, "cannot start the encoder thread: {e:#}");
            (
                Some(close_frame(protocol::CLOSE_INTERNAL, "encoder unavailable")),
                "no encoder thread",
            )
        }
    };
    // Our close frame if we are the one closing; either way `close` flushes it, or the reply
    // to the client's close that the WebSocket queued when it read one.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        if let Some(frame) = outcome {
            sink.send(Message::Close(Some(frame))).await?;
        }
        sink.close().await
    })
    .await;
    drop((sink, stream));

    // Teardown, in the brief's order. The writer's end of `out` is gone, so a worker blocked
    // in blocking_send wakes up with an error and exits.
    state.hub.unsubscribe(sid);
    state.input.send(InputCmd::ReleaseAll { session: sid });
    state.control.unregister(sid);
    inbox.shutdown();
    if let Ok(handle) = worker {
        let join = tokio::task::spawn_blocking(move || handle.join());
        match tokio::time::timeout(WORKER_JOIN_TIMEOUT, join).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => error!(session = sid, "encoder thread panicked"),
            Ok(Err(e)) => warn!(session = sid, "joining the encoder thread: {e}"),
            Err(_) => warn!(
                session = sid,
                "encoder thread still busy; not waiting for it"
            ),
        }
    }
    state.sessions.unregister(sid);
    if state.sessions.count() == 0 {
        // The last encoder is gone: give its memory back while nobody watches.
        crate::release_memory();
    }
    info!(session = sid, "disconnected: {why}");
}

/// Steps 1-5 of brief 4.1. On success the session is registered and counted.
async fn handshake(
    sink: &mut Sink,
    stream: &mut Stream,
    state: &AppState,
) -> Option<(SessionId, Role, Arc<Liveness>)> {
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, first_message(stream)).await {
        Err(_) => {
            reject(sink, ErrorCode::HelloTimeout, "no hello within 5 s").await;
            return None;
        }
        Ok(None) => return None,
        Ok(Some(m)) => m,
    };
    let hello = match &hello {
        Message::Text(t) => serde_json::from_str::<ClientText>(t.as_str()).ok(),
        _ => None,
    };
    let Some(ClientText::Hello { v, token, client }) = hello else {
        tokio::time::sleep(AUTH_FAIL_DELAY).await;
        reject(sink, ErrorCode::Auth, "the first message must be hello").await;
        return None;
    };
    if v != protocol::VERSION {
        reject(
            sink,
            ErrorCode::Version,
            "this server speaks protocol version 1",
        )
        .await;
        return None;
    }
    let role = match state.tokens.authenticate(token.as_deref()).await {
        Ok(role) => role,
        Err(e) => {
            // NotReady is delayed as well: it also means no static token matched.
            tokio::time::sleep(AUTH_FAIL_DELAY).await;
            let (code, msg) = match e {
                AuthError::Denied => (ErrorCode::Auth, "invalid token"),
                AuthError::NotReady => (ErrorCode::NotReady, "the token file is missing or empty"),
            };
            debug!(
                client = client.as_deref().unwrap_or("?"),
                "handshake refused: {msg}"
            );
            reject(sink, code, msg).await;
            return None;
        }
    };
    if state.sessions.is_closing() {
        let _ = send_closing(
            sink,
            close_frame(protocol::CLOSE_GOING_AWAY, "shutting down"),
        )
        .await;
        return None;
    }
    let Some((sid, liveness)) = state.sessions.register(role) else {
        reject(sink, ErrorCode::Busy, "too many viewers").await;
        return None;
    };
    debug!(
        session = sid,
        client = client.as_deref().unwrap_or("?"),
        "hello"
    );
    Some((sid, role, liveness))
}

/// The first data message, skipping WebSocket pings; None when the peer goes away first.
async fn first_message(stream: &mut Stream) -> Option<Message> {
    loop {
        match stream.next().await? {
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(m) => return Some(m),
        }
    }
}

/// Sends `{"t":"error",..}` and closes with the code that goes with it.
async fn reject(sink: &mut Sink, code: ErrorCode, msg: &'static str) {
    info!(code = ?code, "handshake refused: {msg}");
    let error = ServerText::Error {
        code,
        msg: Some(msg.into()),
    };
    let close = close_frame(code.close_code().unwrap_or(protocol::CLOSE_POLICY), msg);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        sink.send(Message::Text(error.to_json().into())).await?;
        sink.send(Message::Close(Some(close))).await
    })
    .await;
}

async fn send_closing(sink: &mut Sink, frame: CloseFrame) -> Result<(), ()> {
    match tokio::time::timeout(
        Duration::from_secs(1),
        sink.send(Message::Close(Some(frame))),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// The reader: client messages to the worker, the input thread and control.
struct Reader<'a> {
    sid: SessionId,
    role: Role,
    state: &'a AppState,
    inbox: &'a Inbox,
    liveness: &'a Liveness,
    replies: &'a mpsc::Sender<OutMsg>,
    malformed: u32,
    /// What this session pressed through us, for the 3 s release.
    held_keys: HashSet<u32>,
    /// Bit n: button n is down.
    held_buttons: u32,
    /// Rate limit for the warning about cut TEXT.
    text_cut: LogEvery,
}

impl Reader<'_> {
    /// Until the peer goes away (None) or the session should close with the returned frame.
    async fn run(&mut self, stream: &mut Stream) -> (Option<CloseFrame>, &'static str) {
        let mut last_heard = Instant::now();
        let mut evict = self.liveness.evict.subscribe();
        loop {
            let idle_at = last_heard + IDLE_CLOSE;
            let wake_at = if self.holding() {
                idle_at.min(last_heard + HELD_RELEASE)
            } else {
                idle_at
            };
            let msg = tokio::select! {
                m = stream.next() => m,
                () = until_set(&mut evict) => {
                    return (
                        Some(close_frame(protocol::CLOSE_NORMAL, "silent; another client took the place")),
                        "silent for 5 s while the server was full",
                    );
                }
                _ = tokio::time::sleep_until(wake_at.into()) => {
                    if Instant::now() >= idle_at {
                        return (Some(close_frame(protocol::CLOSE_NORMAL, "idle")), "idle for 30 s");
                    }
                    // Nothing from a client holding keys or buttons for 3 s: it may be gone
                    // for good, so let go of them; the session stays open.
                    debug!(session = self.sid, "client silent for 3 s with input held; releasing");
                    self.state.input.send(InputCmd::ReleaseAll { session: self.sid });
                    self.held_keys.clear();
                    self.held_buttons = 0;
                    continue;
                }
            };
            last_heard = Instant::now();
            self.liveness.heard();
            match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {
                    return (None, "closed by the client")
                }
                Some(Ok(Message::Binary(b))) => self.binary(&b, last_heard),
                Some(Ok(Message::Text(t))) => self.text(t.as_str()),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            }
            if self.malformed > MAX_MALFORMED {
                return (
                    Some(close_frame(
                        protocol::CLOSE_POLICY,
                        "too many malformed messages",
                    )),
                    "too many malformed messages",
                );
            }
        }
    }

    fn holding(&self) -> bool {
        !self.held_keys.is_empty() || self.held_buttons != 0
    }

    fn binary(&mut self, data: &[u8], at: Instant) {
        let msg = match ClientMsg::parse(data) {
            Ok(m) => m,
            Err(e) => return self.malformed(&e),
        };
        let sid = self.sid;
        let cmd = match msg {
            ClientMsg::Ack { seq, decode_ms } => {
                if !self.inbox.ack(seq, decode_ms, at) {
                    self.malformed(&format_args!("ACK {seq} for a frame never sent"));
                }
                return;
            }
            ClientMsg::Ping { t } => {
                // Dropped if the writer is that far behind; any other message keeps the link alive.
                let _ = self.replies.try_send(OutMsg::Binary(protocol::pong_msg(t)));
                return;
            }
            // Input from view-role sessions and from non-holders is silently dropped.
            _ if self.role != Role::Control => return,
            ClientMsg::Move { x, y } => InputCmd::Move { session: sid, x, y },
            ClientMsg::Button { button, down, x, y } => InputCmd::Button {
                session: sid,
                button,
                down,
                x,
                y,
            },
            ClientMsg::Wheel { dx, dy, x, y } => InputCmd::Wheel {
                session: sid,
                dx,
                dy,
                x,
                y,
            },
            ClientMsg::Key { down, keysym } => InputCmd::Key {
                session: sid,
                keysym,
                down,
            },
            ClientMsg::Text { text, dropped } => {
                if dropped > 0 {
                    if let Some(more) = self.text_cut.ready(Instant::now()) {
                        warn!(
                            session = sid,
                            "TEXT over {} bytes: typing the first {}, dropping {dropped} \
                             ({more} more cut since the last warning)",
                            protocol::MAX_TEXT_BYTES,
                            text.len()
                        );
                    }
                }
                InputCmd::Text { session: sid, text }
            }
            ClientMsg::ReleaseAll => InputCmd::ReleaseAll { session: sid },
        };
        let held = match &cmd {
            InputCmd::Key { keysym, down, .. } => Held::Key(*keysym, *down),
            InputCmd::Button { button, down, .. } => Held::Button(*button, *down),
            InputCmd::ReleaseAll { .. } => Held::None,
            _ => Held::Same,
        };
        if self.state.control.input_if_holder(sid, cmd) {
            match held {
                Held::Key(k, true) => {
                    self.held_keys.insert(k);
                }
                Held::Key(k, false) => {
                    self.held_keys.remove(&k);
                }
                Held::Button(b, down) if b < 32 => {
                    if down {
                        self.held_buttons |= 1 << b;
                    } else {
                        self.held_buttons &= !(1 << b);
                    }
                }
                Held::None => {
                    self.held_keys.clear();
                    self.held_buttons = 0;
                }
                Held::Button(..) | Held::Same => {}
            }
        }
    }

    fn text(&mut self, text: &str) {
        let msg = match serde_json::from_str::<ClientText>(text) {
            Ok(m) => m,
            Err(e) => return self.malformed(&e),
        };
        match msg {
            ClientText::Control { .. } if self.role != Role::Control => {
                let error = ServerText::Error {
                    code: ErrorCode::Forbidden,
                    msg: Some("this session is view-only".into()),
                };
                let _ = self.replies.try_send(OutMsg::Text(error.to_json()));
            }
            ClientText::Control { take: true } => self.state.control.take(self.sid),
            ClientText::Control { take: false } => self.state.control.release(self.sid),
            ClientText::Idr => self.inbox.request_idr(),
            ClientText::Video { on } => self.inbox.video(on),
            ClientText::Hello { .. } => debug!(session = self.sid, "ignoring a repeated hello"),
        }
    }

    fn malformed(&mut self, e: &dyn std::fmt::Display) {
        self.malformed += 1;
        debug!(
            session = self.sid,
            count = self.malformed,
            "malformed message: {e}"
        );
    }
}

/// How a forwarded input command changes what the session holds.
enum Held {
    Key(u32, bool),
    Button(u8, bool),
    /// RELEASE_ALL.
    None,
    Same,
}

/// Resolves once the flag is set; never, if its sender is gone.
async fn until_set(rx: &mut watch::Receiver<bool>) {
    // The watch::Ref guard is not Send: drop it here, before any other await.
    let set = rx.wait_for(|&c| c).await.is_ok();
    if !set {
        std::future::pending::<()>().await;
    }
}

/// The writer: everything for the socket goes through here, one message at a time.
struct Writer<'a> {
    sink: &'a mut Sink,
    /// Told when VIDEO is written, which flow control counts as progress.
    inbox: &'a Inbox,
    cursor_serial: Option<u32>,
    cursor_pos: Option<(i32, i32)>,
}

impl Writer<'_> {
    async fn run(
        mut self,
        welcome: String,
        mut out: mpsc::Receiver<OutMsg>,
        mut replies: mpsc::Receiver<OutMsg>,
        mut cursor: watch::Receiver<CursorState>,
        mut control: watch::Receiver<ControlStatus>,
        mut closing: watch::Receiver<bool>,
    ) -> (Option<CloseFrame>, &'static str) {
        let dead = (None, "write failed");
        // Brief 4.1 step 6: welcome, control, cursor if known, then video.
        if self.send(Message::Text(welcome.into())).await.is_err() {
            return dead;
        }
        let status = *control.borrow_and_update();
        if self.send_control(status).await.is_err() {
            return dead;
        }
        let state = cursor.borrow_and_update().clone();
        if (state.shape.is_some() || (state.x, state.y) != (0, 0))
            && self.send_cursor(&state).await.is_err()
        {
            return dead;
        }
        loop {
            let sent = tokio::select! {
                biased;
                () = until_set(&mut closing) => {
                    return (Some(close_frame(protocol::CLOSE_GOING_AWAY, "server shutting down")), "server shutdown");
                }
                Some(m) = replies.recv() => self.send(m.into_message()).await,
                Ok(()) = control.changed() => {
                    let status = *control.borrow_and_update();
                    self.send_control(status).await
                }
                m = out.recv() => match m {
                    Some(m) => self.send_out(m, &mut out).await,
                    None => {
                        return (Some(close_frame(protocol::CLOSE_INTERNAL, "encoder stopped")), "encoder thread exited");
                    }
                },
                Ok(()) = cursor.changed() => {
                    let state = cursor.borrow_and_update().clone();
                    self.send_cursor(&state).await
                }
            };
            if sent.is_err() {
                return dead;
            }
        }
    }

    /// Sends one worker message. The fragments of an access unit go out back to back: the
    /// worker queues them consecutively, so the rest are taken straight from `out`.
    async fn send_out(
        &mut self,
        first: OutMsg,
        out: &mut mpsc::Receiver<OutMsg>,
    ) -> Result<(), ()> {
        let mut msg = first;
        loop {
            let video =
                matches!(&msg, OutMsg::Binary(b) if b.first() == Some(&protocol::MSG_VIDEO));
            let more = matches!(&msg, OutMsg::Binary(b) if protocol::video_has_more(b));
            self.send(msg.into_message()).await?;
            if video {
                self.inbox.written();
            }
            if !more {
                return Ok(());
            }
            msg = out.recv().await.ok_or(())?;
        }
    }

    async fn send_control(&mut self, s: ControlStatus) -> Result<(), ()> {
        self.send(Message::Text(s.message().to_json().into())).await
    }

    /// CURSOR_SHAPE when the serial changed, CURSOR_POS when the position did.
    async fn send_cursor(&mut self, state: &CursorState) -> Result<(), ()> {
        if let Some(shape) = &state.shape {
            if self.cursor_serial != Some(shape.serial) {
                self.cursor_serial = Some(shape.serial);
                self.send(Message::Binary(protocol::cursor_shape_msg(shape)))
                    .await?;
            }
        }
        let pos = (state.x, state.y);
        if self.cursor_pos != Some(pos) {
            self.cursor_pos = Some(pos);
            self.send(Message::Binary(protocol::cursor_pos_msg(pos.0, pos.1)))
                .await?;
        }
        Ok(())
    }

    /// Feed and flush one message; a peer that does not drain it within 10 s is dropped.
    async fn send(&mut self, msg: Message) -> Result<(), ()> {
        match tokio::time::timeout(WRITE_TIMEOUT, self.sink.send(msg)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                debug!("websocket write failed: {e}");
                Err(())
            }
            Err(_) => {
                warn!("websocket write blocked for 10 s; dropping the session");
                Err(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_ids_and_status_rows() {
        let s = Sessions::new(2);
        let a = s.register(Role::Control).unwrap().0;
        let b = s.register(Role::View).unwrap().0;
        assert_ne!(a, b);
        assert_eq!(s.count(), 2);
        assert_eq!(s.viewer_counter().load(Ordering::Relaxed), 2);
        assert!(s.register(Role::View).is_none(), "max viewers");

        s.update_stats(b, 59.5, 2400.0, 12.5);
        s.update_stats(77, 1.0, 1.0, 1.0);
        let rows = s.snapshot(Some(a));
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].id.as_str(), rows[0].role, rows[0].control),
            (format!("s{a}").as_str(), Role::Control, true)
        );
        assert_eq!(
            rows[1],
            SessionStatus {
                id: format!("s{b}"),
                role: Role::View,
                control: false,
                fps: 59.5,
                kbps: 2400.0,
                rtt_ms: 12.5
            }
        );

        s.unregister(a);
        s.unregister(a);
        assert_eq!(s.count(), 1);
        let c = s.register(Role::View).unwrap().0;
        assert!(c > b, "ids are never reused");
        // Nor are they across restarts (but for a one-in-a-million chance).
        let again = Sessions::new(2).register(Role::Control).unwrap().0;
        assert_ne!(again, a, "a restarted server numbers its sessions afresh");
        s.close_all();
        s.unregister(b);
        assert!(s.is_closing());
        assert!(
            s.register(Role::Control).is_none(),
            "closing refuses new sessions"
        );
        s.unregister(c);
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn a_full_server_closes_the_longest_silent_session_to_admit_a_new_one() {
        const SEC: u64 = 1_000_000;
        let s = Sessions::new(2);
        let t0 = 100 * SEC;
        let (a, a_live) = s.admit(Role::Control, t0).unwrap();
        let (b, b_live) = s.admit(Role::View, t0).unwrap();
        let evicted = |l: &Liveness| *l.evict.borrow();
        b_live.heard_us.store(t0 + 2 * SEC, Ordering::Relaxed);
        // Nobody silent for over 5 s: busy.
        assert!(s.admit(Role::View, t0 + 5 * SEC).is_none());
        assert!(!evicted(&a_live) && !evicted(&b_live));
        // `a` is: it is told to close, and the newcomer gets its place at once.
        let (c, c_live) = s.admit(Role::View, t0 + 5 * SEC + 1).unwrap();
        assert!(evicted(&a_live) && !evicted(&b_live));
        assert_eq!(s.count(), 3, "a counts as a viewer until it has gone");
        // `a` is not picked twice; `b` is not silent long enough yet, `c` just came.
        assert!(s.admit(Role::View, t0 + 6 * SEC).is_none());
        s.unregister(a);
        // Then `b` is the longest silent.
        let (d, _) = s.admit(Role::View, t0 + 8 * SEC).unwrap();
        assert!(evicted(&b_live) && !evicted(&c_live));
        let ids: Vec<String> = s.snapshot(None).into_iter().map(|r| r.id).collect();
        assert_eq!(ids, [b, c, d].map(|id| format!("s{id}")));
        s.close_all();
        assert!(s.admit(Role::View, t0 + 60 * SEC).is_none(), "closing");
    }

    #[tokio::test]
    async fn drained_waits_for_the_last_session() {
        let s = Arc::new(Sessions::new(4));
        let a = s.register(Role::View).unwrap().0;
        let waiter = tokio::spawn({
            let s = Arc::clone(&s);
            async move { s.drained().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        s.unregister(a);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("drained resolves")
            .unwrap();
    }
}
