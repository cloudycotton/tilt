//! Per-session encoder thread `tilt-enc-<sid>` (brief section 5.3).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::clock;
use crate::config::Config;
use crate::flow::{self, FlowControl};
use crate::hub::Frame;
use crate::protocol::{self, ErrorCode, ServerText, Stats, VideoHeader, FLAG_KEY};
use crate::server::AppState;
use crate::session::{OutMsg, SessionId};
use crate::video::encoder::{H264Encoder, UnsupportedSize};

/// What the session's reader, writer and the hub tell the worker. However fast a client sends,
/// it cannot grow: ACKs are cumulative, so the newest stands for all before it, and the rest
/// are flags. The worker takes it all at once after each wake-up. (Replaces the brief's
/// event channel, which queued every ACK while the worker was blocked writing.)
pub struct Inbox {
    pending: Mutex<Pending>,
    /// Capacity 1: a pending wake-up already covers any later ones.
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
    /// VIDEO seq of the last access unit queued for the client; 0 before any.
    sent_seq: AtomicU32,
    /// clock::now_us() when the writer last finished writing a VIDEO message; 0 before any.
    written_us: AtomicU64,
}

#[derive(Debug, Default)]
struct Pending {
    /// The newest cumulative ACK: seq, decode_ms, and when the reader received it.
    ack: Option<(u32, u16, Instant)>,
    /// A keyframe was asked for, or video turned back on (which wants one).
    idr: bool,
    /// Video was turned off at some point.
    paused: bool,
    /// The newest video on/off state.
    video: Option<bool>,
    shutdown: bool,
}

impl Inbox {
    pub fn new() -> Arc<Inbox> {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        Arc::new(Inbox {
            pending: Mutex::new(Pending::default()),
            wake_tx,
            wake_rx,
            sent_seq: AtomicU32::new(0),
            written_us: AtomicU64::new(0),
        })
    }

    /// Wakes the worker; the hub signals it when a frame the worker waits for is published.
    pub fn waker(&self) -> Sender<()> {
        self.wake_tx.clone()
    }

    /// A cumulative ACK the reader received at `at`. False, and dropped, for a seq that was
    /// never sent: acking ahead would keep credit for a client that reads nothing.
    pub fn ack(&self, seq: u32, decode_ms: u16, at: Instant) -> bool {
        if !flow::seq_le(seq, self.sent_seq.load(Ordering::Acquire)) {
            return false;
        }
        self.update(|p| {
            // Keep the first of duplicates: it carries the RTT.
            if p.ack
                .is_none_or(|(newest, _, _)| !flow::seq_le(seq, newest))
            {
                p.ack = Some((seq, decode_ms, at));
            }
        });
        true
    }

    /// The client wants a keyframe; deferred, never dropped, by the 500 ms IDR limit.
    pub fn request_idr(&self) {
        self.update(|p| p.idr = true);
    }

    /// Video paused (false) or resumed (true, which forces an IDR).
    pub fn video(&self, on: bool) {
        self.update(|p| {
            p.video = Some(on);
            if on {
                p.idr = true;
            } else {
                p.paused = true;
            }
        });
    }

    pub fn shutdown(&self) {
        self.update(|p| p.shutdown = true);
    }

    /// The writer finished writing a VIDEO message to the socket.
    pub fn written(&self) {
        self.written_us
            .store(clock::now_us().max(1), Ordering::Relaxed);
    }

    fn last_write(&self) -> Option<Instant> {
        let us = self.written_us.load(Ordering::Relaxed);
        (us != 0).then(|| clock::instant_at(us))
    }

    fn update(&self, f: impl FnOnce(&mut Pending)) {
        f(&mut self.lock());
        let _ = self.wake_tx.try_send(());
    }

    fn take(&self) -> Pending {
        std::mem::take(&mut *self.lock())
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// At most one IDR per this interval, except the first.
const IDR_MIN_GAP: Duration = Duration::from_millis(500);
/// The refinement tail starts once the screen has been still this long after the last new
/// frame (or a frame interval, if that is longer). Starting it one frame interval after the
/// last new frame, as the brief's sketch does, lets a tail frame take the slot of a capture
/// that is only a millisecond late, so a 60 Hz animation lost about a frame in four.
const TAIL_SETTLE: Duration = Duration::from_millis(100);
const FLOW_TICK: Duration = Duration::from_millis(100);
const STATS_EVERY: Duration = Duration::from_secs(1);
/// Longest sleep with nothing scheduled.
const IDLE_WAIT: Duration = Duration::from_millis(250);
/// After the encoder fails (a frame, or creating one), the next attempt waits a frame interval,
/// doubling with every further failure in a row up to this.
const ENCODER_RETRY_MAX: Duration = Duration::from_secs(2);

/// Starts the encoder thread for `sid`. It writes VIDEO, `screen`, `stats` and `error`
/// messages to `out` (bounded, with blocking_send) and exits on `inbox.shutdown()` or once
/// the writer has gone.
pub fn spawn_worker(
    sid: SessionId,
    state: Arc<AppState>,
    inbox: Arc<Inbox>,
    out: mpsc::Sender<OutMsg>,
) -> anyhow::Result<JoinHandle<()>> {
    let handle = std::thread::Builder::new()
        .name(format!("tilt-enc-{sid}"))
        .spawn(move || {
            let mut worker = Worker::new(sid, state, inbox, out);
            worker.run();
            worker.state.hub.set_wants(sid, false);
            debug!(session = sid, "encoder thread done");
        })?;
    Ok(handle)
}

/// The session's writer is gone; the worker has nothing left to do.
#[derive(Debug)]
struct Closed;

/// What the loop should do with the newest frame now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Nothing due: wait for a newer frame, or until the instant given (tail frame, deferred IDR).
    Idle(Option<Instant>),
    /// A frame is due but there is no credit; wait for ACKs.
    Blocked,
    /// A frame is due but the per-session frame rate cap says not before this instant.
    Wait(Instant),
    Encode {
        idr: bool,
    },
}

/// The encode decisions of the brief's loop sketch, apart from the encoder and the network.
#[derive(Debug)]
struct Schedule {
    fi: Duration,
    tail_frames: u32,
    /// Capture seq of the last frame encoded; 0 before any (capture counts from 1).
    last_capture_seq: u64,
    /// Refinement frames still to encode while the screen stays still.
    tail_left: u32,
    force_idr: bool,
    last_idr: Option<Instant>,
    last_encode: Option<Instant>,
    /// When a new capture (not a tail frame) was last encoded: the tail starts TAIL_SETTLE
    /// after it.
    last_new: Option<Instant>,
    /// When the next new capture may be encoded: the frame rate cap, kept on a grid
    /// (clock::next_slot) so that late timer wake-ups do not lower the frame rate.
    new_slot: Option<Instant>,
}

impl Schedule {
    fn new(fi: Duration, tail_frames: u32) -> Schedule {
        Schedule {
            fi,
            tail_frames,
            last_capture_seq: 0,
            tail_left: 0,
            force_idr: true,
            last_idr: None,
            last_encode: None,
            last_new: None,
            new_slot: None,
        }
    }

    fn decide(&self, latest_seq: u64, can_send: bool, now: Instant) -> Step {
        let have_new = latest_seq != self.last_capture_seq;
        let idr_allowed = self.idr_allowed(now);
        let paced = self.last_encode.map_or(now, |t| t + self.fi);
        let tail_at = self.last_new.map_or(paced, |t| paced.max(t + TAIL_SETTLE));
        let tail_due = self.tail_left > 0 && now >= tail_at;
        if !(have_new || (self.force_idr && idr_allowed) || tail_due) {
            let tail_next = (self.tail_left > 0).then_some(tail_at);
            let idr_next = self
                .last_idr
                .filter(|_| self.force_idr)
                .map(|t| t + IDR_MIN_GAP);
            let next = match (tail_next, idr_next) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            return Step::Idle(next);
        }
        if !can_send {
            return Step::Blocked;
        }
        // The frame rate cap. New content keeps to its own grid: a refinement frame that just
        // went out must not hold back the answer to input, so the two may be closer together
        // than a frame interval.
        let due = if have_new {
            self.new_slot.unwrap_or(now)
        } else {
            paced
        };
        if now < due {
            return Step::Wait(due);
        }
        Step::Encode {
            idr: self.force_idr && idr_allowed,
        }
    }

    fn idr_allowed(&self, now: Instant) -> bool {
        self.last_idr.is_none_or(|t| now >= t + IDR_MIN_GAP)
    }

    /// An access unit for capture frame `seq`, decided on at `now`, went out at `sent`. The
    /// tail counts from `sent`: from `now`, an encode slower than TAIL_SETTLE (a loaded
    /// machine) started the tail at once, and its frames went out without capture being asked
    /// whether the screen had moved on, freezing an animation for the length of the tail.
    fn encoded(&mut self, seq: u64, keyframe: bool, now: Instant, sent: Instant) {
        if keyframe {
            self.force_idr = false;
            self.last_idr = Some(now);
        }
        if seq != self.last_capture_seq {
            self.tail_left = self.tail_frames;
            self.last_new = Some(sent);
            self.new_slot = Some(clock::next_slot(self.new_slot, now, self.fi));
        } else {
            self.tail_left = self.tail_left.saturating_sub(1);
        }
        self.last_capture_seq = seq;
        self.last_encode = Some(now);
    }

    /// The encoder has nothing left to refine: a re-encode of the still screen came out as the
    /// same all-skip frame as the one before, at the lowest quantizer, which every further
    /// re-encode would repeat. Ends the tail.
    fn converged(&mut self) {
        self.tail_left = 0;
    }

    /// Rate control skipped the frame: try again a frame interval later, same frame.
    fn rc_skipped(&mut self, now: Instant) {
        self.last_encode = Some(now);
        self.new_slot = Some(now + self.fi);
    }
}

/// A run of encoder failures (EncodeFrame, or creating an encoder) with no access unit in
/// between: when to try again, so that a frame OpenH264 cannot encode does not cost a new
/// encoder and a keyframe attempt at the full frame rate, what to log, so that a run is one
/// warning rather than one per frame, and which capture failed, so that the next attempt can
/// be on a newer one.
#[derive(Debug, Default)]
struct EncoderTrouble {
    /// Failures in the current run; 0 when there is none.
    failures: u32,
    /// When the current run started.
    since: Option<Instant>,
    /// No attempt before this.
    retry_at: Option<Instant>,
    /// Capture seq of the frame the last failure was on.
    failed_seq: u64,
}

impl EncoderTrouble {
    /// Records a failure on capture `seq` and schedules the next attempt `first_delay` after
    /// the first failure of a run, twice as long after each further one, at most
    /// ENCODER_RETRY_MAX. True when the failure starts a run, which is when it should be logged.
    fn failed(&mut self, now: Instant, first_delay: Duration, seq: u64) -> bool {
        let starts_run = self.failures == 0;
        if starts_run {
            self.since = Some(now);
        }
        self.failed_seq = seq;
        self.failures = self.failures.saturating_add(1);
        let doublings = (self.failures - 1).min(16);
        let delay = first_delay
            .saturating_mul(1 << doublings)
            .min(ENCODER_RETRY_MAX);
        self.retry_at = Some(now + delay);
        starts_run
    }

    /// When an attempt now would come too early, the time it may be made.
    fn wait(&self, now: Instant) -> Option<Instant> {
        self.retry_at.filter(|&at| now < at)
    }

    /// Whether to ask capture for a frame while backing off, `latest_seq` being the newest one:
    /// only while that is still the frame that failed. A newer one may well encode (the content
    /// OpenH264 could not fit may be gone), and with it in hand an animated screen need not be
    /// grabbed at the full frame rate for nothing until the next attempt.
    fn wants_newer(&self, latest_seq: u64) -> bool {
        latest_seq == self.failed_seq
    }

    /// An access unit went out. Ends the run, returning its failures and how long it lasted.
    fn recovered(&mut self, now: Instant) -> Option<(u32, Duration)> {
        let run = (self.failures > 0).then(|| {
            let since = self.since.unwrap_or(now);
            (self.failures, now.saturating_duration_since(since))
        });
        *self = EncoderTrouble::default();
        run
    }
}

struct Worker {
    sid: SessionId,
    state: Arc<AppState>,
    cfg: Arc<Config>,
    inbox: Arc<Inbox>,
    out: mpsc::Sender<OutMsg>,
    started: Instant,

    sched: Schedule,
    flow: FlowControl,
    enc: Option<H264Encoder>,
    bitrate: u32,
    /// --max-bitrate-kbps in bps: every encoder signals a level that covers it.
    max_bitrate: u32,
    /// The (even-cropped) size the client knows about, from welcome or the last `screen`.
    announced: (u32, u32),
    video_on: bool,
    /// VIDEO seq of the last access unit sent.
    seq: u32,
    next_tick: Option<Instant>,
    next_flow_tick: Instant,
    next_stats: Instant,
    trouble: EncoderTrouble,
    /// A frame was due but credit was out, since the last encode.
    was_blocked: bool,
    /// Size and quantizer of the last access unit, to tell when the tail has converged.
    last_au: Option<(usize, u8)>,
    /// Waiting for capture to replace this stale capture seq, until the instant (see
    /// `fresh_enough`).
    fresh_wait: Option<(u64, Instant)>,
    /// A screen size the encoder cannot take: no attempts while captures have it.
    unsupported: Option<(u32, u32)>,

    // Stats: since `window_start`, except `skipped`, which counts for the whole session.
    window_start: Instant,
    frames: u32,
    bytes: u64,
    encode_us: u64,
    qp_sum: u64,
    skipped: u64,
}

impl Worker {
    fn new(
        sid: SessionId,
        state: Arc<AppState>,
        inbox: Arc<Inbox>,
        out: mpsc::Sender<OutMsg>,
    ) -> Worker {
        let now = Instant::now();
        let cfg = Arc::clone(&state.cfg);
        let flow_config = cfg.flow_config();
        let max_bitrate = flow_config.max_bps;
        let flow = FlowControl::new(flow_config, now);
        let bitrate = flow.stats().bitrate_bps;
        let (w, h) = state.hub.screen_size();
        Worker {
            sid,
            cfg: Arc::clone(&cfg),
            state,
            inbox,
            out,
            started: now,
            sched: Schedule::new(cfg.frame_interval(), cfg.tail_frames),
            flow,
            enc: None,
            bitrate,
            max_bitrate,
            announced: (w & !1, h & !1),
            video_on: true,
            seq: 0,
            next_tick: None,
            next_flow_tick: now + FLOW_TICK,
            next_stats: now + STATS_EVERY,
            trouble: EncoderTrouble::default(),
            was_blocked: false,
            last_au: None,
            fresh_wait: None,
            unsupported: None,
            window_start: now,
            frames: 0,
            bytes: 0,
            encode_us: 0,
            qp_sum: 0,
            skipped: 0,
        }
    }

    fn run(&mut self) {
        loop {
            let now = Instant::now();
            let mut deadline = (now + IDLE_WAIT)
                .min(self.next_flow_tick)
                .min(self.next_stats);
            if let Some(t) = self.next_tick {
                deadline = deadline.min(t);
            }
            let _ = self.inbox.wake_rx.recv_deadline(deadline);
            let pending = self.inbox.take();
            if pending.shutdown || self.out.is_closed() {
                return;
            }
            self.apply(pending);
            if self.step().is_err() {
                return;
            }
        }
    }

    fn apply(&mut self, p: Pending) {
        // The pause first: an ACK taken with it may be one a frozen client sent as it thawed,
        // for a frame from before the pause, and must teach nothing (pipeline X1).
        if p.paused {
            self.flow.pause(Instant::now());
        }
        if let Some((seq, decode_ms, at)) = p.ack {
            self.flow.on_ack(seq, decode_ms, at);
        }
        if let Some(on) = p.video {
            self.video_on = on;
        }
        if p.idr {
            self.sched.force_idr = true;
        }
    }

    fn step(&mut self) -> Result<(), Closed> {
        let now = Instant::now();
        self.next_tick = None;
        if self.flow.stalled(now, self.inbox.last_write()) {
            // No keyframe: TCP lost nothing, the client decodes in order, and it asks for one
            // itself when it needs one. A forced IDR would only queue behind the frame that is
            // still on its way over a slow link (addition to brief 5.3).
            warn!(
                session = self.sid,
                "no ACK and no video written for 5 s or more; resetting flow control"
            );
            self.flow.reset(now);
        }
        if now >= self.next_flow_tick {
            self.next_flow_tick = now + FLOW_TICK;
            self.retarget(now);
        }
        if now >= self.next_stats {
            self.next_stats = now + STATS_EVERY;
            self.send_stats(now)?;
        }
        if !self.video_on {
            self.state.hub.set_wants(self.sid, false);
            return Ok(());
        }
        let Some(frame) = self.state.hub.latest() else {
            self.ask_newer(0, now);
            return Ok(());
        };
        let size = (frame.image.width, frame.image.height);
        match self.unsupported {
            Some(s) if s == size => {
                // Capture drops `latest` on a resize, which brings the next step here.
                self.state.hub.set_wants(self.sid, false);
                return Ok(());
            }
            Some(_) => self.unsupported = None,
            None => {}
        }
        match self.sched.decide(frame.seq, self.flow.can_send(now), now) {
            Step::Idle(next) => {
                self.next_tick = next;
                self.ask_newer(frame.seq, now);
            }
            Step::Blocked => {
                self.flow.note_blocked(now);
                self.was_blocked = true;
                self.state.hub.set_wants(self.sid, false);
            }
            Step::Wait(at) => self.next_tick = Some(at),
            Step::Encode { idr } => {
                if self.fresh_enough(&frame, idr, now) {
                    self.encode(&frame, idr, now)?;
                }
            }
        }
        Ok(())
    }

    /// Waits for a capture other than `seen`, or decides again at once if one came since.
    fn ask_newer(&mut self, seen: u64, now: Instant) {
        if self.state.hub.want_newer(self.sid, seen) {
            self.next_tick = Some(now);
        }
    }

    /// Whether `frame` is fresh enough to encode now. Back from waiting for credit, and for a
    /// keyframe (a new viewer, a resume), a stale `latest` (FrameHub::screen_changed) can be
    /// as old as the wait, seconds after a pause; then this asks capture for a fresh grab and
    /// waits for it, two frame intervals at most. Elsewhere `latest` is at most a frame
    /// interval behind, and a grab would cost more than it gains. (Addition to brief 5.3,
    /// which encodes the stale frame.)
    fn fresh_enough(&mut self, frame: &Frame, idr: bool, now: Instant) -> bool {
        let came_back = std::mem::take(&mut self.was_blocked);
        match self.fresh_wait {
            Some((seq, until)) if frame.seq == seq && now < until => {
                self.next_tick = Some(until);
                false
            }
            Some(_) => {
                self.fresh_wait = None;
                true
            }
            None if (idr || came_back) && self.state.hub.latest_is_stale() => {
                let until = now + 2 * self.sched.fi;
                self.fresh_wait = Some((frame.seq, until));
                self.next_tick = Some(until);
                self.ask_newer(frame.seq, now);
                false
            }
            None => true,
        }
    }

    fn encode(&mut self, frame: &Frame, idr: bool, now: Instant) -> Result<(), Closed> {
        let (w, h) = (frame.image.width, frame.image.height);
        if w == 0 || h == 0 {
            return Ok(());
        }
        if let Some(at) = self.trouble.wait(now) {
            // Backing off after a failure; the newest frame is tried at `at`. Without asking
            // for a newer one, that would be the frame that failed for as long as the screen
            // shows anything at all, as capture grabs only on demand.
            let wants = self.trouble.wants_newer(frame.seq);
            self.state.hub.set_wants(self.sid, wants);
            self.next_tick = Some(at);
            return Ok(());
        }
        if !self.ensure_encoder(w, h, frame.seq, now)? {
            return Ok(());
        }
        let Some(enc) = self.enc.as_mut() else {
            return Ok(());
        };
        let ts_ms = now.saturating_duration_since(self.started).as_millis() as u64;
        match enc.encode(&frame.image, ts_ms, idr) {
            Ok(Some(f)) if !f.data.is_empty() => {
                if let Some((failures, lasted)) = self.trouble.recovered(now) {
                    info!(
                        session = self.sid,
                        failures, "encoder working again after {lasted:.1?} of failures"
                    );
                }
                if self.sched.last_capture_seq != 0 && frame.seq > self.sched.last_capture_seq {
                    // Captures that went to other viewers while this one waited.
                    self.skipped += frame.seq - self.sched.last_capture_seq - 1;
                }
                self.seq = self.seq.wrapping_add(1).max(1);
                // Before the first fragment goes out, so its ACK is never ahead of it.
                self.inbox.sent_seq.store(self.seq, Ordering::Release);
                let header = VideoHeader {
                    flags: if f.keyframe { FLAG_KEY } else { 0 },
                    seq: self.seq,
                    capture_us: frame.capture_us,
                    width: w as u16,
                    height: h as u16,
                };
                for msg in protocol::video_messages(header, &f.data, self.cfg.max_msg_bytes) {
                    self.send(OutMsg::Binary(msg))?;
                }
                let sent = Instant::now();
                self.flow.on_sent(self.seq, f.data.len(), sent);
                let tail = frame.seq == self.sched.last_capture_seq;
                self.sched.encoded(frame.seq, f.keyframe, now, sent);
                let au = (f.data.len(), f.qp);
                if tail && f.qp <= self.cfg.qp_min && self.last_au == Some(au) {
                    self.sched.converged();
                }
                self.last_au = Some(au);
                // Decide again straight away: that asks capture for the next frame (or notes
                // that credit ran out) instead of waiting for the next event or tick.
                self.next_tick = Some(sent);
                self.frames += 1;
                self.bytes += f.data.len() as u64;
                self.encode_us += u64::from(f.encode_us);
                self.qp_sum += u64::from(f.qp);
                debug!(
                    session = self.sid,
                    seq = self.seq,
                    capture = frame.seq,
                    bytes = f.data.len(),
                    key = f.keyframe,
                    enc_us = f.encode_us,
                    // From the start of the grab to the access unit being queued.
                    age_us = crate::clock::now_us().saturating_sub(frame.capture_us),
                    "frame"
                );
            }
            Ok(_) => {
                // Rate control skipped it (only with --rc-frame-skip).
                self.skipped += 1;
                self.sched.rc_skipped(now);
                self.next_tick = Some(now + self.sched.fi);
            }
            Err(e) => {
                // OpenH264 drops its state after a failed frame (encoder.rs), so start afresh
                // with a keyframe, after a backoff.
                self.enc = None;
                self.sched.force_idr = true;
                if self.trouble.failed(now, self.sched.fi, frame.seq) {
                    warn!(
                        session = self.sid,
                        "encode failed: {e:#}; recreating the encoder, with backoff while it \
                         keeps failing"
                    );
                } else {
                    debug!(
                        session = self.sid,
                        failures = self.trouble.failures,
                        "encode failed again: {e:#}"
                    );
                }
                self.state.hub.set_wants(self.sid, true);
                self.next_tick = self.trouble.retry_at;
            }
        }
        Ok(())
    }

    /// Makes sure an encoder for `w`x`h` (the size of capture `seq`) exists, announcing a size
    /// change first. Ok(false) when it cannot be created right now (retried later).
    fn ensure_encoder(&mut self, w: u32, h: u32, seq: u64, now: Instant) -> Result<bool, Closed> {
        if self
            .enc
            .as_ref()
            .is_some_and(|e| (e.settings().width, e.settings().height) == (w, h))
        {
            return Ok(true);
        }
        self.enc = None;
        let profile = self.cfg.resolved_profile(self.state.cpus);
        let settings = self.cfg.encoder_settings(w, h, self.bitrate, profile);
        match H264Encoder::with_max_bitrate(&settings, self.max_bitrate) {
            Ok(enc) => {
                if self.announced != (w, h) {
                    // Brief 4.4: `screen` precedes the KEY frame at the new size. The hub has
                    // the full root size; use it unless it already moved on again.
                    let (sw, sh) = self.state.hub.screen_size();
                    let (sw, sh) = if (sw & !1, sh & !1) == (w, h) {
                        (sw, sh)
                    } else {
                        (w, h)
                    };
                    self.send(OutMsg::Text(ServerText::Screen { w: sw, h: sh }.to_json()))?;
                    self.announced = (w, h);
                }
                debug!(
                    session = self.sid,
                    width = w,
                    height = h,
                    bitrate = self.bitrate,
                    "encoder ready"
                );
                self.enc = Some(enc);
                self.last_au = None;
                self.sched.force_idr = true;
                Ok(true)
            }
            Err(e) if e.downcast_ref::<UnsupportedSize>().is_some() => {
                // Retrying cannot help until the screen size changes; say why there is no
                // picture (the session stays open).
                warn!(session = self.sid, "{e:#}; no video at this screen size");
                self.unsupported = Some((w, h));
                let error = ServerText::Error {
                    code: ErrorCode::UnsupportedSize,
                    msg: Some(format!("cannot stream this screen: {e:#}")),
                };
                self.send(OutMsg::Text(error.to_json()))?;
                self.state.hub.set_wants(self.sid, false);
                Ok(false)
            }
            Err(e) => {
                if self.trouble.failed(now, self.sched.fi, seq) {
                    warn!(
                        session = self.sid,
                        "cannot create a {w}x{h} encoder: {e:#}; retrying with backoff"
                    );
                } else {
                    debug!(
                        session = self.sid,
                        failures = self.trouble.failures,
                        "cannot create a {w}x{h} encoder: {e:#}"
                    );
                }
                self.state.hub.set_wants(self.sid, true);
                self.next_tick = self.trouble.retry_at;
                Ok(false)
            }
        }
    }

    /// Runs the bitrate controller and hands a new target to the encoder.
    fn retarget(&mut self, now: Instant) {
        let Some(bps) = self.flow.tick(now) else {
            return;
        };
        debug!(session = self.sid, from = self.bitrate, to = bps, "bitrate");
        self.bitrate = bps;
        if let Some(enc) = self.enc.as_mut() {
            if let Err(e) = enc.set_bitrate(bps) {
                warn!(session = self.sid, "set_bitrate({bps}): {e:#}");
            }
        }
    }

    fn send_stats(&mut self, now: Instant) -> Result<(), Closed> {
        let secs = now
            .saturating_duration_since(self.window_start)
            .as_secs_f32()
            .max(0.001);
        let flow = self.flow.stats();
        let round = |v: f32| (v * 10.0).round() / 10.0;
        let fps = round(self.frames as f32 / secs);
        let kbps = round(self.bytes as f32 * 8.0 / 1000.0 / secs);
        self.state
            .sessions
            .update_stats(self.sid, fps, kbps, round(flow.rtt_ms));
        if self.video_on {
            let stats = Stats {
                fps,
                kbps,
                bitrate_kbps: flow.bitrate_bps / 1000,
                rtt_ms: round(flow.rtt_ms),
                min_rtt_ms: round(flow.min_rtt_ms),
                queue_ms: round(flow.queue_ms),
                enc_ms: if self.frames > 0 {
                    round(self.encode_us as f32 / self.frames as f32 / 1000.0)
                } else {
                    0.0
                },
                qp: if self.frames > 0 {
                    round(self.qp_sum as f32 / self.frames as f32)
                } else {
                    0.0
                },
                inflight: flow.inflight_frames,
                skipped: self.skipped,
                viewers: self.state.sessions.count() as u32,
                gov_fps: self.cfg.max_fps,
            };
            self.send(OutMsg::Text(ServerText::Stats(stats).to_json()))?;
        }
        self.window_start = now;
        self.frames = 0;
        self.bytes = 0;
        self.encode_us = 0;
        self.qp_sum = 0;
        Ok(())
    }

    fn send(&self, msg: OutMsg) -> Result<(), Closed> {
        self.out.blocking_send(msg).map_err(|_| Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::convert::FramePool;

    const MS: Duration = Duration::from_millis(1);

    fn sched() -> (Schedule, Instant) {
        (
            Schedule::new(Duration::from_micros(16_667), 3),
            Instant::now(),
        )
    }

    #[test]
    fn first_frame_is_a_keyframe_then_the_tail_refines_it() {
        let (mut s, t0) = sched();
        assert_eq!(s.decide(1, true, t0), Step::Encode { idr: true });
        s.encoded(1, true, t0, t0);
        // Same frame: once the screen has been still for TAIL_SETTLE, the tail re-encodes it
        // at the frame rate, three times, then goes quiet.
        assert_eq!(
            s.decide(1, true, t0 + 5 * MS),
            Step::Idle(Some(t0 + TAIL_SETTLE))
        );
        assert_eq!(
            s.decide(1, true, t0 + s.fi),
            Step::Idle(Some(t0 + TAIL_SETTLE))
        );
        let mut t = t0 + TAIL_SETTLE - s.fi;
        for left in (0..3).rev() {
            t += s.fi;
            assert_eq!(s.decide(1, true, t), Step::Encode { idr: false });
            s.encoded(1, false, t, t);
            assert_eq!(s.tail_left, left);
            if left > 0 {
                assert_eq!(s.decide(1, true, t + MS), Step::Idle(Some(t + s.fi)));
            }
        }
        assert_eq!(s.decide(1, true, t + s.fi), Step::Idle(None));
        // New content restarts the tail.
        assert_eq!(s.decide(2, true, t + s.fi), Step::Encode { idr: false });
        s.encoded(2, false, t + s.fi, t + s.fi);
        assert_eq!(s.tail_left, 3);
    }

    #[test]
    fn a_late_capture_is_not_overtaken_by_a_tail_frame() {
        let (mut s, t0) = sched();
        s.encoded(1, true, t0, t0);
        // A 60 Hz animation whose next capture lands 2 ms after its slot: no tail frame went
        // out in the slot, so the capture is encoded at once, not a frame interval later.
        assert_eq!(
            s.decide(1, true, t0 + s.fi),
            Step::Idle(Some(t0 + TAIL_SETTLE))
        );
        assert_eq!(
            s.decide(2, true, t0 + s.fi + 2 * MS),
            Step::Encode { idr: false }
        );
        s.encoded(2, false, t0 + s.fi + 2 * MS, t0 + s.fi + 2 * MS);
        assert_eq!(
            s.decide(2, true, t0 + 2 * s.fi + 2 * MS),
            Step::Idle(Some(t0 + s.fi + 2 * MS + TAIL_SETTLE))
        );
    }

    #[test]
    fn new_content_is_not_held_back_by_a_tail_frame() {
        let (mut s, t0) = sched();
        s.encoded(1, true, t0, t0);
        let tail = t0 + TAIL_SETTLE;
        assert_eq!(s.decide(1, true, tail), Step::Encode { idr: false });
        s.encoded(1, false, tail, tail);
        // A key press lands 3 ms after the refinement frame: its capture goes out at once.
        assert_eq!(
            s.decide(2, true, tail + 3 * MS),
            Step::Encode { idr: false }
        );
        s.encoded(2, false, tail + 3 * MS, tail + 3 * MS);
        // After a pause the next frame may follow half an interval later (clock::next_slot),
        // then the grid resumes.
        assert_eq!(
            s.decide(3, true, tail + 5 * MS),
            Step::Wait(tail + 3 * MS + s.fi / 2)
        );
    }

    #[test]
    fn the_frame_rate_cap_keeps_to_a_grid() {
        let (mut s, t0) = sched();
        s.tail_frames = 0;
        s.encoded(1, true, t0, t0);
        // The worker woke 3 ms after the slot: the frame goes out then...
        let late = t0 + s.fi + 3 * MS;
        assert_eq!(s.decide(2, true, late), Step::Encode { idr: false });
        s.encoded(2, false, late, late);
        // ...and the next slot is still t0 + 2 fi, not `late` + fi.
        assert_eq!(s.decide(3, true, late + 5 * MS), Step::Wait(t0 + 2 * s.fi));
        assert_eq!(
            s.decide(3, true, t0 + 2 * s.fi),
            Step::Encode { idr: false }
        );
    }

    #[test]
    fn a_deferred_idr_wakes_the_worker_before_a_later_tail_frame() {
        let (mut s, t0) = sched();
        s.encoded(1, true, t0, t0);
        s.encoded(2, false, t0 + 450 * MS, t0 + 450 * MS);
        s.force_idr = true;
        // The tail would start at 550 ms; the IDR is allowed from 500 ms.
        assert_eq!(
            s.decide(2, true, t0 + 460 * MS),
            Step::Idle(Some(t0 + IDR_MIN_GAP))
        );
        assert_eq!(
            s.decide(2, true, t0 + IDR_MIN_GAP),
            Step::Encode { idr: true }
        );
    }

    #[test]
    fn frame_rate_cap_and_credit() {
        let (mut s, t0) = sched();
        s.encoded(1, true, t0, t0);
        // A new frame 5 ms later waits for the frame interval; without credit it is blocked.
        assert_eq!(s.decide(2, true, t0 + 5 * MS), Step::Wait(t0 + s.fi));
        assert_eq!(s.decide(2, false, t0 + 5 * MS), Step::Blocked);
        assert_eq!(s.decide(2, false, t0 + s.fi), Step::Blocked);
        assert_eq!(s.decide(2, true, t0 + s.fi), Step::Encode { idr: false });
        // Nothing due means idle even without credit.
        let (mut s, t0) = sched();
        s.tail_frames = 0;
        s.encoded(1, true, t0, t0);
        assert_eq!(s.decide(1, false, t0 + 100 * MS), Step::Idle(None));
    }

    #[test]
    fn idr_requests_are_rate_limited_and_deferred_not_dropped() {
        let (mut s, t0) = sched();
        s.tail_frames = 0;
        s.encoded(1, true, t0, t0);
        assert!(!s.force_idr && s.last_idr == Some(t0));

        // Requested 100 ms after the last IDR: deferred to t0 + 500 ms.
        s.force_idr = true;
        assert_eq!(
            s.decide(1, true, t0 + 100 * MS),
            Step::Idle(Some(t0 + IDR_MIN_GAP))
        );
        // New content meanwhile goes out as a delta; the request stays pending.
        assert_eq!(
            s.decide(2, true, t0 + 200 * MS),
            Step::Encode { idr: false }
        );
        s.encoded(2, false, t0 + 200 * MS, t0 + 200 * MS);
        assert!(s.force_idr);
        assert_eq!(
            s.decide(2, true, t0 + 300 * MS),
            Step::Idle(Some(t0 + IDR_MIN_GAP))
        );
        // Once allowed, the IDR goes out even though the screen is still.
        assert_eq!(
            s.decide(2, true, t0 + IDR_MIN_GAP),
            Step::Encode { idr: true }
        );
        s.encoded(2, true, t0 + IDR_MIN_GAP, t0 + IDR_MIN_GAP);
        assert!(!s.force_idr);
        assert_eq!(
            s.decide(2, true, t0 + IDR_MIN_GAP + 100 * MS),
            Step::Idle(None)
        );
    }

    #[test]
    fn rc_skips_retry_the_same_frame() {
        let (mut s, t0) = sched();
        s.rc_skipped(t0);
        assert_eq!(s.decide(1, true, t0 + 5 * MS), Step::Wait(t0 + s.fi));
        assert_eq!(s.decide(1, true, t0 + s.fi), Step::Encode { idr: true });
    }

    #[test]
    fn encoder_failures_back_off_and_log_once_per_run() {
        let t0 = Instant::now();
        let fi = Duration::from_micros(16_667);
        let mut t = EncoderTrouble::default();
        assert_eq!(t.wait(t0), None);
        assert_eq!(t.recovered(t0), None);
        // Only the failure that starts a run is logged; retries wait 1, 2, 4... intervals.
        assert!(t.failed(t0, fi, 7));
        assert_eq!(t.wait(t0), Some(t0 + fi));
        assert_eq!(t.wait(t0 + fi), None);
        // Meanwhile capture is asked for a frame newer than the one that failed, and once
        // there is one, for nothing more.
        assert!(t.wants_newer(7));
        assert!(!t.wants_newer(8));
        let mut now = t0 + fi;
        for k in 1..8 {
            assert!(!t.failed(now, fi, 8));
            let delay = (fi * (1 << k)).min(ENCODER_RETRY_MAX);
            assert_eq!(t.wait(now), Some(now + delay));
            now += delay;
        }
        // However long the run, at most ENCODER_RETRY_MAX apart.
        for _ in 0..100 {
            assert!(!t.failed(now, fi, 8));
        }
        assert!(t.wants_newer(8));
        assert_eq!(t.wait(now), Some(now + ENCODER_RETRY_MAX));
        // An access unit ends the run; the next failure starts, and logs, a new one.
        let end = now + ENCODER_RETRY_MAX;
        assert_eq!(t.recovered(end), Some((108, end - t0)));
        assert_eq!(t.wait(end), None);
        assert_eq!(t.recovered(end), None);
        assert!(t.failed(end, fi, 9));
        assert_eq!(t.wait(end), Some(end + fi));
    }

    #[test]
    fn the_inbox_keeps_the_newest_ack_and_refuses_acks_for_frames_never_sent() {
        // Protocol P4 / pipeline P8: however many ACKs a client sends, one is kept.
        let inbox = Inbox::new();
        let t0 = Instant::now();
        assert!(!inbox.ack(1, 0, t0), "nothing sent yet");
        inbox.sent_seq.store(5, Ordering::Release);
        assert!(!inbox.ack(6, 0, t0));
        assert!(
            !inbox.ack(0x7fff_ffff, 0, t0),
            "would keep credit for a client that never reads"
        );
        for _ in 0..100_000 {
            assert!(inbox.ack(3, 1, t0));
        }
        assert!(inbox.ack(5, 2, t0 + MS));
        assert!(inbox.ack(5, 3, t0 + 2 * MS));
        assert!(inbox.ack(4, 4, t0 + 3 * MS));
        assert_eq!(inbox.wake_rx.len(), 1, "one wake-up");
        let p = inbox.take();
        assert_eq!(
            p.ack,
            Some((5, 2, t0 + MS)),
            "the newest, as first received"
        );
        assert!(!p.idr && !p.paused && p.video.is_none() && !p.shutdown);
        assert!(inbox.take().ack.is_none());
        // Seqs wrap (skipping 0).
        inbox.sent_seq.store(2, Ordering::Release);
        assert!(inbox.ack(u32::MAX, 0, t0) && inbox.ack(1, 0, t0) && !inbox.ack(3, 0, t0));
        assert_eq!(inbox.take().ack.map(|a| a.0), Some(1));
        // Paused and back before the worker looked: the pause still reaches flow control, and
        // the resume asks for a keyframe.
        inbox.video(false);
        inbox.video(true);
        let p = inbox.take();
        assert!(p.paused && p.idr && p.video == Some(true));
    }

    /// A worker for session 1 on a hub with no capture thread: the tests publish its frames.
    fn worker(args: &[&str]) -> (Worker, mpsc::Receiver<OutMsg>, FramePool) {
        let state = crate::server::test_state(&[&["--no-auth"], args].concat());
        let inbox = Inbox::new();
        state.hub.subscribe(1, inbox.waker());
        let (out, rx) = mpsc::channel(1024);
        (Worker::new(1, state, inbox, out), rx, FramePool::new())
    }

    fn publish(w: &Worker, pool: &FramePool, seq: u64, (width, height): (u32, u32)) {
        w.state.hub.publish(Frame {
            seq,
            capture_us: seq * 1000,
            image: pool.get(width, height),
        });
    }

    /// The VIDEO headers and JSON messages queued since the last call.
    fn sent(rx: &mut mpsc::Receiver<OutMsg>) -> (Vec<VideoHeader>, Vec<String>) {
        let (mut video, mut text) = (Vec::new(), Vec::new());
        while let Ok(m) = rx.try_recv() {
            match m {
                OutMsg::Binary(b) => video.push(VideoHeader::parse(&b).expect("VIDEO").0),
                OutMsg::Text(t) => text.push(t),
            }
        }
        (video, text)
    }

    /// Steps the worker, sleeping until each tick it asks for, until it sends video or has
    /// nothing to do.
    fn step_until_sent(w: &mut Worker, rx: &mut mpsc::Receiver<OutMsg>) -> Vec<VideoHeader> {
        for _ in 0..20 {
            w.step().unwrap();
            let video = sent(rx).0;
            let Some(tick) = w.next_tick.filter(|_| video.is_empty()) else {
                return video;
            };
            std::thread::sleep(tick.saturating_duration_since(Instant::now()));
        }
        panic!("the worker neither sent nor rested");
    }

    const SIZE: (u32, u32) = (64, 48);

    #[test]
    fn a_keyframe_waits_briefly_for_a_fresh_grab_when_latest_is_stale() {
        // Pipeline P5: the screen changed while nobody watched, so `latest` is old; a new
        // session (or a resume) should not start from it.
        let (mut w, mut rx, pool) = worker(&[]);
        publish(&w, &pool, 1, SIZE);
        w.state.hub.screen_changed();
        w.step().unwrap();
        assert!(
            sent(&mut rx).0.is_empty(),
            "the stale frame is not encoded at once"
        );
        assert!(w.state.hub.demand(), "capture is asked for a fresh grab");
        publish(&w, &pool, 2, SIZE);
        let video = step_until_sent(&mut w, &mut rx);
        assert_eq!(video.len(), 1);
        assert_eq!(
            (video[0].capture_us, video[0].flags & FLAG_KEY),
            (2000, FLAG_KEY)
        );

        // With no grab within two frame intervals, the stale frame is better than nothing.
        let (mut w, mut rx, pool) = worker(&[]);
        publish(&w, &pool, 1, SIZE);
        w.state.hub.screen_changed();
        let t0 = Instant::now();
        let video = step_until_sent(&mut w, &mut rx);
        assert_eq!(video.len(), 1);
        assert!(t0.elapsed() >= 2 * w.sched.fi);
    }

    #[test]
    fn credit_coming_back_waits_briefly_for_a_fresh_grab_when_latest_is_stale() {
        // Pipeline P2: out of credit, the worker stops asking for frames while the screen
        // moves on. When an ACK brings credit back it should send the screen as it is, not
        // as it was when credit ran out.
        let (mut w, mut rx, pool) = worker(&["--tail-frames", "0"]);
        let mut seq = 0;
        loop {
            seq += 1;
            publish(&w, &pool, seq, SIZE);
            if step_until_sent(&mut w, &mut rx).is_empty() {
                break;
            }
        }
        assert!(w.was_blocked && !w.state.hub.demand());
        w.state.hub.screen_changed();
        // Past the frame rate cap, then the ACK.
        std::thread::sleep(2 * w.sched.fi);
        assert!(w.inbox.ack(w.seq, 1, Instant::now()));
        let pending = w.inbox.take();
        w.apply(pending);
        w.step().unwrap();
        assert!(
            sent(&mut rx).0.is_empty(),
            "the stale frame is not encoded at once"
        );
        assert!(w.state.hub.demand(), "capture is asked for a fresh grab");
        publish(&w, &pool, seq + 1, SIZE);
        let video = step_until_sent(&mut w, &mut rx);
        assert_eq!(video[0].capture_us, (seq + 1) * 1000);
    }

    #[test]
    fn a_stall_resets_flow_control_without_forcing_a_keyframe() {
        // Pipeline P4: TCP lost nothing, so a keyframe would only queue behind the frame that
        // is still on its way.
        let (mut w, mut rx, pool) = worker(&["--tail-frames", "0"]);
        publish(&w, &pool, 1, SIZE);
        assert_eq!(
            step_until_sent(&mut w, &mut rx)[0].flags & FLAG_KEY,
            FLAG_KEY
        );
        // As if that frame went 6 s ago, with nothing written or acked since, and a keyframe
        // would be allowed.
        let long_ago = Instant::now().checked_sub(Duration::from_secs(6)).unwrap();
        w.flow = FlowControl::new(w.cfg.flow_config(), long_ago);
        w.flow.on_sent(w.seq, 1_000, long_ago);
        w.sched.last_idr = Some(long_ago);
        assert!(w.flow.stalled(Instant::now(), w.inbox.last_write()));
        publish(&w, &pool, 2, SIZE);
        let video = step_until_sent(&mut w, &mut rx);
        assert_eq!(w.flow.stats().inflight_frames, 1, "reset, then frame 2");
        assert_eq!(video[0].flags & FLAG_KEY, 0, "no keyframe forced");
    }

    #[test]
    fn pausing_video_forgets_the_frames_in_flight() {
        // Pipeline P1/X1 at the worker: the pause reaches flow control.
        let (mut w, mut rx, pool) = worker(&["--tail-frames", "0"]);
        publish(&w, &pool, 1, SIZE);
        assert_eq!(step_until_sent(&mut w, &mut rx).len(), 1);
        assert_eq!(w.flow.stats().inflight_frames, 1);
        w.inbox.video(false);
        let pending = w.inbox.take();
        w.apply(pending);
        assert_eq!(w.flow.stats().inflight_frames, 0);
        w.step().unwrap();
        assert!(!w.state.hub.demand());
    }

    #[test]
    fn a_pause_taken_together_with_the_acks_after_it_teaches_nothing() {
        // Pipeline X1 when the worker is busy (encoding, or blocked writing) while a client
        // pauses, freezes, thaws, acks what it held and resumes: it takes all of that at once.
        let (mut w, mut rx, pool) = worker(&["--tail-frames", "0"]);
        publish(&w, &pool, 1, SIZE);
        assert_eq!(step_until_sent(&mut w, &mut rx).len(), 1);
        assert!(w.inbox.ack(w.seq, 1, Instant::now()));
        let pending = w.inbox.take();
        w.apply(pending);
        publish(&w, &pool, 2, SIZE);
        assert_eq!(step_until_sent(&mut w, &mut rx).len(), 1);
        let before = w.flow.stats();
        assert_eq!(before.inflight_frames, 1);
        w.inbox.video(false);
        assert!(w
            .inbox
            .ack(w.seq, 1, Instant::now() + Duration::from_secs(3)));
        w.inbox.video(true);
        let pending = w.inbox.take();
        w.apply(pending);
        let after = w.flow.stats();
        assert_eq!(after.inflight_frames, 0);
        assert_eq!(
            (after.rtt_ms, after.min_rtt_ms),
            (before.rtt_ms, before.min_rtt_ms),
            "a 3 s sample spanning the pause"
        );
        assert!(w.video_on && w.sched.force_idr);
    }

    #[test]
    fn a_slow_encode_does_not_start_the_tail_at_once() {
        // Seen in the link-emulator runs on a loaded machine: frames taking over TAIL_SETTLE to
        // encode started the tail straight away, and capture, never asked, stopped grabbing an
        // animated screen for the tail's 30 frames.
        let (mut w, mut rx, pool) = worker(&[]);
        publish(&w, &pool, 1, SIZE);
        assert_eq!(step_until_sent(&mut w, &mut rx).len(), 1);
        publish(&w, &pool, 2, SIZE);
        let frame = w.state.hub.latest().unwrap();
        // As if encoding frame 2 took 200 ms.
        let started = Instant::now().checked_sub(200 * MS).unwrap();
        w.encode(&frame, false, started).unwrap();
        assert_eq!(sent(&mut rx).0.len(), 1);
        w.step().unwrap();
        assert!(sent(&mut rx).0.is_empty(), "no refinement frame yet");
        assert!(w.state.hub.demand(), "capture is asked for the next frame");
    }

    /// Runs the worker as `run` does, acking every frame at once, until it rests with nothing
    /// scheduled or `limit` passes; returns the VIDEO it sent.
    fn run_acking(
        w: &mut Worker,
        rx: &mut mpsc::Receiver<OutMsg>,
        limit: Duration,
    ) -> Vec<VideoHeader> {
        let (end, mut all) = (Instant::now() + limit, Vec::new());
        while Instant::now() < end {
            w.step().unwrap();
            for h in sent(rx).0 {
                assert!(w.inbox.ack(h.seq, 1, Instant::now()));
                all.push(h);
            }
            let pending = w.inbox.take();
            w.apply(pending);
            let Some(tick) = w.next_tick else {
                return all;
            };
            std::thread::sleep(tick.saturating_duration_since(Instant::now()).min(50 * MS));
        }
        all
    }

    #[test]
    fn the_tail_ends_once_the_encoder_has_converged() {
        let (mut w, mut rx, pool) = worker(&["--tail-frames", "30"]);
        publish(&w, &pool, 1, SIZE);
        let frames = run_acking(&mut w, &mut rx, Duration::from_secs(3));
        // A keyframe, then re-encodes of the still screen until two come out alike at the
        // lowest quantizer: a handful, not the 30 allowed.
        assert!(frames[0].flags & FLAG_KEY != 0);
        assert!(
            (2..10).contains(&frames.len()),
            "{} frames: {frames:?}",
            frames.len()
        );
        assert_eq!(w.sched.tail_left, 0);
        assert_eq!(w.next_tick, None, "resting");
        // New content brings a new tail, which converges again.
        publish(&w, &pool, 2, SIZE);
        let more = run_acking(&mut w, &mut rx, Duration::from_secs(3));
        assert!((2..10).contains(&more.len()), "{more:?}");
        assert!(more.iter().all(|h| h.flags & FLAG_KEY == 0));
    }

    #[test]
    fn a_screen_size_the_encoder_cannot_take_is_reported_once_and_not_retried() {
        // Pipeline P7.
        let (mut w, mut rx, pool) = worker(&[]);
        publish(&w, &pool, 1, (8, 8));
        w.step().unwrap();
        let (video, texts) = sent(&mut rx);
        assert!(video.is_empty());
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(
            texts[0].contains(r#""code":"unsupported_size""#),
            "{}",
            texts[0]
        );
        assert!(!w.state.hub.demand());
        for _ in 0..5 {
            w.step().unwrap();
        }
        assert_eq!(sent(&mut rx), (vec![], vec![]));
        assert!(!w.state.hub.demand() && w.enc.is_none());
        // A resize: capture drops `latest` and, asked, grabs at the new size.
        w.state.hub.invalidate();
        w.step().unwrap();
        assert!(w.state.hub.demand());
        publish(&w, &pool, 2, SIZE);
        let video = step_until_sent(&mut w, &mut rx);
        assert_eq!((video[0].width, video[0].height), (64, 48));
    }

    #[test]
    fn the_worker_exits_on_shutdown_or_once_the_writer_has_gone() {
        for shutdown in [true, false] {
            let state = crate::server::test_state(&["--no-auth"]);
            let inbox = Inbox::new();
            let (out, rx) = mpsc::channel(8);
            let handle = spawn_worker(1, state, Arc::clone(&inbox), out).unwrap();
            if shutdown {
                inbox.shutdown();
            } else {
                drop(rx);
                inbox.request_idr();
            }
            let t0 = Instant::now();
            while !handle.is_finished() {
                // Not at the next stats message, a second later, when a send would fail.
                assert!(t0.elapsed() < 500 * MS, "shutdown {shutdown}");
                std::thread::sleep(MS);
            }
        }
    }
}
