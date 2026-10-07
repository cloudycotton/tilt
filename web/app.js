// tilt web client: connection, decoder, renderer, UI, stats and debug hooks (brief 6).
import { createInput, message } from './input.js';

const VIDEO = 0x01;
const CURSOR_SHAPE = 0x02;
const CURSOR_POS = 0x03;
const PONG = 0x04;
const ACK = 0x10;
const PING = 0x11;
const FLAG_KEY = 1;
const FLAG_MORE = 2;
const VIDEO_HEADER = 18;

const CLIENT = 'tilt-web/0.1';
const BACKOFF_MS = [250, 500, 1000, 2000, 5000];
const PING_MS = 1000;
const DEAD_MS = 5000;
const WELCOME_TIMEOUT_MS = 10000;
// After a reconnect, how long this page waits for control to be free before giving up taking it
// back. A server that names no holder may be leaving it with this page's own lost session, which
// it idle-closes within 30 s.
const RETAKE_MS = 35000;
const IDR_INTERVAL_MS = 500;
// Frames in the decoder this long with no output: flush it, and replace it if the flush hangs.
const STALL_MS = 1000;
// Decode failures in a row (no output between them) before asking for a software decoder, and
// before giving up on the stream.
const SOFTWARE_AFTER_FAILURES = 2;
const GIVE_UP_AFTER_FAILURES = 4;
const TOOLBAR_HIDE_MS = 3000;
const TOAST_MS = 3500;
const PROBE_MAX_EVENTS = 10000;
const CURSOR_CACHE = 64;
// Chrome and Firefox ignore CSS cursor images larger than 128x128.
const CSS_CURSOR_MAX = 128;
// The cursor sprite never shrinks below this many CSS px per cursor pixel.
const MIN_CURSOR_SCALE = 0.6;
// Pinch zoom stops at this many CSS px per video pixel.
const MAX_ZOOM_SCALE = 4;

const UNSUPPORTED = 'This browser lacks WebCodecs H.264 decoding (needs Chrome/Edge 94+, Safari/iOS 16.4+, Android WebView 94+).';
const INSECURE = 'This page is also not a secure context: Chrome and Android WebView only offer WebCodecs over https:// or on localhost.';

const DEFAULT_CURSOR = {
  url: 'data:image/svg+xml,' + encodeURIComponent(
    "<svg xmlns='http://www.w3.org/2000/svg' width='13' height='20' viewBox='0 0 13 20'>" +
    "<path d='M1 1v15.5l3.8-3.6 2.7 6.1 2.6-1.1-2.7-6h5.3z' fill='#fff' stroke='#000' stroke-linejoin='round'/></svg>"),
  w: 13,
  h: 20,
  xhot: 1,
  yhot: 1,
};

const $ = (id) => document.getElementById(id);
const ui = {
  viewport: $('viewport'),
  stage: $('stage'),
  canvas: $('screen'),
  sprite: $('cursor'),
  kbd: $('kbd'),
  toolbar: $('toolbar'),
  edge: $('edge'),
  dot: $('dot'),
  control: $('btn-control'),
  keyboard: $('btn-keyboard'),
  keys: $('btn-keys'),
  type: $('btn-type'),
  scale: $('btn-scale'),
  fullscreen: $('btn-fullscreen'),
  menuBtn: $('btn-menu'),
  strip: $('keys'),
  menu: $('menu'),
  touchRow: $('touch-row'),
  cmdRow: $('cmd-row'),
  cmdToggle: $('opt-cmd'),
  altRow: $('alt-row'),
  altToggle: $('opt-alt'),
  info: $('menu-info'),
  statsToggle: $('opt-stats'),
  typer: $('typer'),
  typerForm: $('typer-form'),
  typerText: $('typer-text'),
  typerSend: $('typer-send'),
  typerCancel: $('typer-cancel'),
  stats: $('stats'),
  overlay: $('overlay'),
  overlayMsg: $('overlay-msg'),
  overlayDetail: $('overlay-detail'),
  tokenForm: $('token-form'),
  tokenInput: $('token-input'),
  retry: $('overlay-retry'),
  unsupported: $('unsupported'),
  unsupportedMsg: $('unsupported-msg'),
  toast: $('toast'),
};

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const hex = (bytes) => Array.from(bytes, (v) => v.toString(16).padStart(2, '0')).join('').toUpperCase();

// ---- options and storage (storage access throws in sandboxed frames and some private modes)

function load(area, key) {
  try { return window[area].getItem(key); } catch { return null; }
}

function save(area, key, value) {
  try { window[area].setItem(key, value); } catch { /* unavailable */ }
}

function decodeParam(s) {
  try { return decodeURIComponent(s); } catch { return s; }
}

/**
 * The URL fragment's parameters as [name, value, raw part]. Not URLSearchParams: its form
 * decoding turns '+' into a space, and tokens may be base64.
 */
function hashParams() {
  return location.hash.slice(1).split('&').filter(Boolean).map((part) => {
    const i = part.indexOf('=');
    return [decodeParam(i < 0 ? part : part.slice(0, i)), i < 0 ? '' : decodeParam(part.slice(i + 1)), part];
  });
}

/** The first value of `name`, or null. */
function hashParam(params, name) {
  const p = params.find(([n]) => n === name);
  return p ? p[1] : null;
}

/** Moves `token` from the URL fragment into sessionStorage, keeping it out of the address bar. */
function takeHashToken() {
  const params = hashParams();
  const t = hashParam(params, 'token');
  if (t === null) return null;
  save('sessionStorage', 'tilt.token', t);
  const rest = params.filter(([n]) => n !== 'token').map((p) => p[2]).join('&');
  history.replaceState(history.state, '', location.pathname + location.search + (rest ? '#' + rest : ''));
  return t;
}

function readOptions() {
  const params = hashParams();
  const get = (name) => hashParam(params, name);
  const touch = get('touch');
  const probe = (get('probe') || '').split(',').map(Number);
  return {
    token: takeHashToken() ?? load('sessionStorage', 'tilt.token'),
    control: get('control') === '1',
    stats: get('stats') !== null ? get('stats') === '1' : load('localStorage', 'tilt.stats') === '1',
    touch: touch === 'trackpad' || touch === 'direct' ? touch
      : load('localStorage', 'tilt.touch') || (Math.min(screen.width, screen.height) < 600 ? 'trackpad' : 'direct'),
    probe: probe.length === 2 && probe.every((v) => Number.isInteger(v) && v >= 0) ? probe : null,
    scale: get('scale') === '1' ? '1' : 'fit',
    framing: get('framing') === 'avcc' ? 'avcc' : 'annexb',
  };
}

const opts = readOptions();
let token = opts.token;
let touchMode = opts.touch === 'direct' ? 'direct' : 'trackpad';
let cmdToCtrl = load('localStorage', 'tilt.cmdToCtrl') !== '0';
let optionAsAlt = load('localStorage', 'tilt.optionAsAlt') === '1';
const APPLE = /Mac|iPhone|iPad|iPod/.test(navigator.platform);
// Phones and tablets; a laptop with a touchscreen keeps the desktop UI.
const touchDevice = matchMedia('(pointer: coarse)').matches;
const finePointer = matchMedia('(pointer: fine)');

// ---- state

const stats = {
  framesDecoded: 0,
  framesDrawn: 0,
  keyframes: 0,
  bytes: 0,
  decodeErrors: 0,
  lastSeq: 0,
  lastDrawnSeq: 0,
  // Beyond the brief's list: watchdog flushes of a decoder that held frames without output.
  decoderStalls: 0,
  rttMs: null,
  codec: '',
  width: 0,
  height: 0,
  role: '',
  control: false,
  // Beyond the brief's list: the server's session id and the H.264 framing in use (annexb | avcc).
  session: '',
  framing: opts.framing,
};
const probeEvents = [];

const conn = {
  ws: null,
  attempt: 0,
  timer: 0,
  openedAt: 0,
  lastRecvAt: 0,
  pingSentAt: 0,
  welcomed: false,
  everWelcomed: false,
  held: null, // someone holds control (from this session's `control` messages); null before the first
  holder: null, // the session id holding control, from servers that name it
  take: null, // an automatic take of control waiting for its moment: { force, until } (maybeTake)
  resume: false, // this page held control when its last session ended
  lost: '', // the id of this page's last session that held control when its connection went
  server: '',
  fatal: '',
  askToken: false,
  notice: '',
};
let wantControl = opts.control;
let serverStats = null;
let videoOn = true; // what this session's server was last told; sessions start with video on

const video = {
  decoder: null,
  configKey: '',
  framing: opts.framing,
  undecodable: '', // config key this browser failed to decode in both framings
  undecodableDetail: '',
  probed: new Set(),
  hw: 'no-preference', // hardwareAcceleration; 'prefer-software' once the default decoder kept failing
  config: null, // the current decoder's configuration
  failures: 0, // decode failures since the last output
  lastOutputAt: 0,
  flush: null, // a watchdog flush in progress: { at, seq, decoded }
  needKey: true,
  frag: null,
  pending: new Map(), // seq -> receive time, in seq order, until acked
  frame: null, // a decoded frame waiting for the next refresh
  raf: 0, // the next refresh is booked: frames until then wait for it
  lastIdrAt: -Infinity,
  idrTimer: 0,
  sizeNotice: false, // the server cannot encode the screen at its size (until its next KEY frame)
};

const cursor = { shapes: new Map(), shape: null, pos: null };
let lastPointer = finePointer.matches ? 'mouse' : 'touch';
let spriteRaf = 0;
let cssCursor = '';

// vw, vh: the viewport's client size, re-read by layout() (the ResizeObserver calls it) so that
// per-event pans and zooms do not force a layout.
const view = { mode: opts.scale, s: 1, z: 1, tx: 0, ty: 0, scroll: false, vw: 0, vh: 0 };

function context2d() {
  try {
    const c = ui.canvas.getContext('2d', { alpha: false, desynchronized: true });
    if (c) return c;
  } catch { /* retry without the low-latency hint */ }
  return ui.canvas.getContext('2d', { alpha: false });
}
const ctx = context2d();

// ---- sending

function wsSend(data) {
  const ws = conn.ws;
  if (!ws || ws.readyState !== WebSocket.OPEN || !conn.welcomed) return false;
  ws.send(data);
  return true;
}

const sendJson = (obj) => wsSend(JSON.stringify(obj));

// ---- connection

function streamUrl() {
  const url = new URL('stream', location.href);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.hash = '';
  return url.href;
}

function connect() {
  clearTimeout(conn.timer);
  conn.timer = 0;
  detach();
  let ws;
  try {
    ws = new WebSocket(streamUrl());
  } catch (e) {
    conn.fatal = `Cannot open the stream: ${e.message}`;
    showFatal();
    return;
  }
  ws.binaryType = 'arraybuffer';
  conn.ws = ws;
  conn.openedAt = performance.now();
  setDot('wait', 'Connecting');
  ws.onopen = () => {
    if (ws !== conn.ws) return;
    const hello = { t: 'hello', v: 1 };
    if (token) hello.token = token;
    hello.client = CLIENT;
    ws.send(JSON.stringify(hello));
    conn.lastRecvAt = performance.now();
  };
  ws.onmessage = (ev) => {
    if (ws === conn.ws) onMessage(ev.data);
  };
  ws.onclose = (ev) => {
    if (ws === conn.ws) onClosed(ev.code);
  };
}

/** Drops the current socket without running its close handling. */
function detach() {
  const ws = conn.ws;
  conn.ws = null;
  if (!ws) return;
  ws.onopen = ws.onmessage = ws.onclose = null;
  try { ws.close(); } catch { /* already closing */ }
}

function onClosed(code) {
  conn.ws = null;
  endSession();
  if (!conn.fatal && code === 4001) {
    conn.fatal = 'Access denied: the token is missing or wrong.';
    conn.askToken = true;
  }
  if (!conn.fatal && code === 4003) conn.fatal = 'This page and the server speak different protocol versions. Reload the page.';
  if (conn.fatal) showFatal();
  else scheduleReconnect();
}

function scheduleReconnect() {
  clearTimeout(conn.timer);
  conn.timer = 0;
  showOverlay(conn.everWelcomed ? 'Reconnecting…' : 'Connecting…', conn.notice);
  // A hidden page gets about one timer wake-up a minute (Chrome's intensive throttling): too few
  // PINGs to keep a session from idling out, so each one would just cycle. The visibilitychange
  // handler connects as soon as the page is shown.
  if (document.visibilityState === 'hidden') return;
  const max = BACKOFF_MS[BACKOFF_MS.length - 1];
  const base = BACKOFF_MS[Math.min(conn.attempt, BACKOFF_MS.length - 1)];
  conn.attempt++;
  // Jitter keeps viewers that dropped together from reconnecting in lockstep.
  const delay = Math.min(max, base * (0.75 + Math.random() * 0.5));
  conn.timer = setTimeout(() => {
    conn.timer = 0;
    if (document.visibilityState !== 'hidden') connect();
  }, delay);
}

function reconnect() {
  conn.fatal = '';
  conn.askToken = false;
  conn.notice = '';
  conn.attempt = 0;
  video.undecodable = '';
  video.hw = 'no-preference';
  video.failures = 0;
  setFraming(opts.framing);
  endSession();
  if (!ui.overlay.hidden) showOverlay(conn.everWelcomed ? 'Reconnecting…' : 'Connecting…');
  connect();
}

function endSession() {
  // Control that went away with the connection (not given away) may be taken back after reconnecting.
  if (stats.control || conn.take) conn.resume = true;
  // The server keeps that session, holding control, until it notices the connection is gone.
  if (stats.control) conn.lost = stats.session;
  conn.take = null;
  conn.welcomed = false;
  conn.pingSentAt = 0;
  conn.held = null;
  conn.holder = null;
  input.setEnabled(false);
  stats.control = false;
  // A fresh session restarts seq at 1: frames still inside the old decoder must never ack into it.
  closeDecoder(false);
  video.frag = null;
  clearTimeout(video.idrTimer);
  video.idrTimer = 0;
  video.lastIdrAt = -Infinity;
  cursor.shapes.clear();
  serverStats = null;
  setDot(conn.fatal ? 'bad' : 'wait', conn.fatal ? 'Disconnected' : 'Connecting');
  updateControls();
  applyCursor();
}

function onMessage(data) {
  conn.lastRecvAt = performance.now();
  if (typeof data === 'string') {
    onJson(data);
    return;
  }
  if (!data.byteLength) return;
  switch (new Uint8Array(data, 0, 1)[0]) {
    case VIDEO: onVideo(data); break;
    case CURSOR_SHAPE: onCursorShape(data); break;
    case CURSOR_POS: onCursorPos(data); break;
    case PONG: onPong(data); break;
    default: break;
  }
}

function onJson(text) {
  let m;
  try { m = JSON.parse(text); } catch { return; }
  if (!m || typeof m !== 'object') return;
  switch (m.t) {
    case 'welcome': onWelcome(m); break;
    case 'control': onControl(m); break;
    case 'stats': serverStats = m; break;
    case 'error': onServerError(m); break;
    default: break; // 'screen' needs nothing: the KEY frame that follows carries the new size
  }
}

// The backoff is not reset here but on the first decoded frame: a session that dies right after
// its welcome must not be retried at the shortest delay forever.
function onWelcome(m) {
  const first = !conn.everWelcomed;
  conn.welcomed = true;
  conn.everWelcomed = true;
  conn.notice = '';
  stats.session = String(m.session ?? '');
  conn.server = String(m.server ?? '');
  stats.role = m.role === 'control' ? 'control' : 'view';
  setDot('ok', 'Connected');
  updateControls();
  showToolbar();
  videoOn = true;
  syncVideo();
  // Video stays off for a stream this browser cannot decode, so no frame will end "Reconnecting…".
  if (video.undecodable) showUndecodable();
  // Brief 6.1: control=1 takes control after the first welcome. A reconnect takes it back only if
  // this page held it when the connection went, and only once nobody holds it: another viewer may
  // have taken it in the meantime.
  if (wantControl && stats.role === 'control' && (first || conn.resume)) {
    conn.take = { force: first, until: first ? Infinity : performance.now() + RETAKE_MS };
  }
  conn.resume = false;
  maybeTake();
}

function onControl(m) {
  const you = m.you === true;
  if (stats.control && !you && wantControl) {
    wantControl = false;
    toast('Another viewer took control.');
  }
  stats.control = you;
  conn.held = m.held === true;
  conn.holder = conn.held && typeof m.holder === 'string' ? m.holder : null;
  input.setEnabled(you);
  if (you && finePointer.matches && !touchDevice) input.focusKeyboard();
  updateControls();
  applyCursor();
  maybeTake();
}

/**
 * Sends a pending automatic take once it is due: at once for control=1's first take, otherwise
 * when this session's `control` status says nobody holds control, or that this page's own lost
 * session does (the server closes it only once it notices). Never from another viewer, and never
 * while the page is hidden. A server that names no holder leaves us unable to tell the two
 * apart, so control held there is waited out for RETAKE_MS.
 */
function maybeTake() {
  const t = conn.take;
  if (!t || !conn.welcomed) return;
  if (!wantControl || stats.role !== 'control' || stats.control) {
    conn.take = null;
    return;
  }
  if (document.visibilityState === 'hidden') return;
  if (t.force || conn.held === false || (conn.holder !== null && conn.holder === conn.lost)) {
    conn.take = null;
    sendJson({ t: 'control', take: true });
  }
}

/** Video flows while the page is visible and can decode it. */
function syncVideo() {
  const want = document.visibilityState !== 'hidden' && !video.undecodable;
  if (want !== videoOn && sendJson({ t: 'video', on: want })) videoOn = want;
}

function onServerError(m) {
  const code = String(m.code ?? '');
  const msg = typeof m.msg === 'string' ? m.msg : '';
  switch (code) {
    case 'auth':
      conn.fatal = 'Access denied: the token is missing or wrong.';
      conn.askToken = true;
      break;
    case 'version':
      conn.fatal = `This page and the server speak different protocol versions${msg ? ` (${msg})` : ''}. Reload the page.`;
      break;
    case 'busy':
      conn.notice = 'The server already has its maximum number of viewers.';
      break;
    case 'not_ready':
      conn.notice = 'The server is not ready yet: it has no access token.';
      break;
    case 'hello_timeout':
      conn.notice = 'The server stopped waiting for this page.';
      break;
    case 'forbidden':
      wantControl = false;
      toast('This is a view-only session: it cannot take control.');
      break;
    case 'unsupported_size':
      // The session stays open without video. The server sends this once per size; a KEY frame
      // follows only once the screen has a size that works.
      video.sizeNotice = true;
      showOverlay('The remote screen cannot be streamed at its current size.', msg);
      break;
    default:
      toast(msg || `Server error: ${code}`);
  }
}

function sendPing(now) {
  const [b, d] = message(PING, 5);
  d.setUint32(1, Math.floor(now) >>> 0, true);
  if (wsSend(b) && !conn.pingSentAt) conn.pingSentAt = now;
}

function onPong(buf) {
  if (buf.byteLength < 5) return;
  const t = new DataView(buf).getUint32(1, true);
  stats.rttMs = ((Math.floor(performance.now()) >>> 0) - t) >>> 0;
  conn.pingSentAt = 0;
}

function linkLost(notice) {
  detach();
  endSession();
  conn.notice = notice;
  scheduleReconnect();
}

// Once a second: handshake timeout, liveness, PING, the pending take, the decoder watchdog and
// the stats overlay.
function tick() {
  const now = performance.now();
  if (conn.ws && !conn.welcomed && now - conn.openedAt > WELCOME_TIMEOUT_MS) {
    linkLost('The server did not answer.');
  } else if (conn.ws && conn.welcomed) {
    if (conn.pingSentAt && now - conn.lastRecvAt > DEAD_MS) linkLost('The connection stopped responding.');
    else sendPing(now);
  }
  // A take that still finds control held when its time is up gives up; one held back only because
  // the page is hidden (control free) waits for the page to be shown.
  if (conn.take && conn.held && now > conn.take.until) {
    conn.take = null;
    wantControl = false;
    toast('Another viewer has control.');
  }
  watchDecoder(now);
  updateStats(now);
}

// ---- video

function onVideo(buf) {
  if (buf.byteLength < VIDEO_HEADER) return;
  const dv = new DataView(buf);
  const flags = dv.getUint8(1);
  const seq = dv.getUint32(2, true);
  const width = dv.getUint16(14, true);
  const height = dv.getUint16(16, true);
  const part = new Uint8Array(buf, VIDEO_HEADER);
  stats.bytes += buf.byteLength;
  let f = video.frag;
  if (f && f.seq !== seq) {
    // Fragments of a seq are contiguous on the socket; never wedge on a server that breaks that.
    video.frag = null;
    skipFrame(f.seq);
    f = null;
  }
  if (flags & FLAG_MORE) {
    if (!f) f = video.frag = { seq, parts: [], len: 0 };
    f.parts.push(part);
    f.len += part.length;
    return;
  }
  let data = part;
  if (f) {
    video.frag = null;
    data = new Uint8Array(f.len + part.length);
    let o = 0;
    for (const p of [...f.parts, part]) {
      data.set(p, o);
      o += p.length;
    }
  }
  onAccessUnit(seq, (flags & FLAG_KEY) !== 0, width, height, data);
}

/** Acks a frame that will not be decoded (decode_ms 0) and asks for a keyframe. */
function skipFrame(seq) {
  video.pending.set(seq, performance.now());
  ackThrough(seq);
  video.needKey = true;
  requestIdr();
}

function onAccessUnit(seq, key, width, height, data) {
  stats.lastSeq = seq;
  if (key) {
    stats.keyframes++;
    video.sizeNotice = false;
    if (!configure(data, width, height)) {
      skipFrame(seq);
      return;
    }
    video.needKey = false;
  } else if (video.needKey || !video.decoder || video.decoder.state !== 'configured') {
    skipFrame(seq);
    return;
  }
  video.pending.set(seq, performance.now());
  try {
    video.decoder.decode(new EncodedVideoChunk({
      type: key ? 'key' : 'delta',
      timestamp: seq * 1000,
      data: video.framing === 'avcc' ? toAvcc(data) : data,
    }));
  } catch (e) {
    decoderFailed(e);
  }
}

/** Makes sure a decoder matches this keyframe; false when it cannot be decoded. */
function configure(au, width, height) {
  const nals = splitNals(au);
  const sps = nals.find((n) => (n[0] & 0x1f) === 7);
  const pps = nals.find((n) => (n[0] & 0x1f) === 8);
  if (!sps || sps.length < 4 || (video.framing === 'avcc' && !pps)) return false;
  const codec = 'avc1.' + hex(sps.subarray(1, 4));
  const description = video.framing === 'avcc' ? avcDescription(sps, pps) : null;
  const key = `${codec} ${width}x${height} ${video.framing} ${video.hw}${description ? ' ' + hex(description) : ''}`;
  if (key === video.configKey && video.decoder && video.decoder.state === 'configured') return true;
  if (key === video.undecodable) {
    showUndecodable();
    return false;
  }
  closeDecoder(true);
  const config = { codec, codedWidth: width, codedHeight: height, optimizeForLatency: true, hardwareAcceleration: video.hw };
  if (description) config.description = description;
  const decoder = new VideoDecoder({
    output: (frame) => onDecoded(decoder, frame),
    error: (e) => {
      if (decoder === video.decoder) decoderFailed(e);
    },
  });
  try {
    decoder.configure(config);
  } catch (e) {
    try { decoder.close(); } catch { /* never opened */ }
    if (video.framing === 'annexb') {
      setFraming('avcc');
      return configure(au, width, height);
    }
    cannotDecode(key, `${codec} ${width}x${height}`, e);
    return false;
  }
  video.decoder = decoder;
  video.configKey = key;
  video.config = config;
  if (video.undecodable) {
    // A keyframe of another stream (one still in flight when video was turned off) decodes.
    video.undecodable = '';
    syncVideo();
  }
  stats.codec = codec;
  if (video.framing === 'annexb' && !video.probed.has(key)) {
    video.probed.add(key);
    VideoDecoder.isConfigSupported(config).then((r) => {
      if (r.supported || video.framing !== 'annexb') return;
      // Brief 6.2: this engine rejects Annex B without a description; resync on avcC framing.
      setFraming('avcc');
      closeDecoder(true);
      requestIdr();
    }, () => {});
  }
  return true;
}

function setFraming(framing) {
  video.framing = framing;
  stats.framing = framing;
}

function closeDecoder(ack) {
  const d = video.decoder;
  video.decoder = null;
  video.configKey = '';
  video.config = null;
  video.flush = null;
  video.needKey = true;
  if (d && d.state !== 'closed') {
    try { d.close(); } catch { /* closed by the browser */ }
  }
  if (ack) ackThrough(Infinity);
  else video.pending.clear();
}

function decoderFailed(e) {
  stats.decodeErrors++;
  const key = video.configKey;
  const config = video.config;
  const name = (e && e.name) || 'Error';
  closeDecoder(true);
  if (name === 'NotSupportedError') {
    if (video.framing === 'avcc') {
      cannotDecode(key, stats.codec, e);
      return;
    }
    setFraming('avcc');
  } else if (name !== 'QuotaExceededError') {
    // (QuotaExceededError is the browser reclaiming an idle decoder, not a failure to decode.)
    // Rebuilding the same config gets the same decoder back, so a decoder that fails on every
    // frame would loop on IDRs forever: count the failures, try software, then stop.
    video.failures++;
    if (video.failures >= GIVE_UP_AFTER_FAILURES) {
      cannotDecode(key, stats.codec, e);
      return;
    }
    if (video.failures === SOFTWARE_AFTER_FAILURES && config) preferSoftware(config);
  }
  toast(`Video decoder error (${name}); resyncing.`);
  requestIdr();
}

/**
 * Switches to a software decoder after repeated failures, if the browser has one for this stream
 * (Chrome on Android has none for H.264). Chrome only picks software when asked: for the same
 * config it selects the same, failing, hardware decoder again.
 */
function preferSoftware(config) {
  VideoDecoder.isConfigSupported({ ...config, hardwareAcceleration: 'prefer-software' }).then((r) => {
    if (!r.supported || video.failures < SOFTWARE_AFTER_FAILURES || video.undecodable) return;
    video.hw = 'prefer-software';
    // A keyframe that came in meanwhile went to a hardware decoder again: replace it.
    if (video.decoder) {
      closeDecoder(true);
      requestIdr();
    }
  }, () => {});
}

function cannotDecode(key, what, e) {
  video.undecodable = key;
  video.undecodableDetail = `${what}: ${(e && e.message) || e}`;
  showUndecodable();
  // Nothing can be shown: stop the stream rather than ack it at full rate (Retry turns it back on).
  syncVideo();
}

function showUndecodable() {
  showOverlay('This browser cannot decode the desktop stream.', video.undecodableDetail, { retry: true });
}

function onDecoded(decoder, frame) {
  if (decoder !== video.decoder) {
    frame.close();
    return;
  }
  stats.framesDecoded++;
  video.failures = 0;
  video.lastOutputAt = performance.now();
  // The session works: the next drop starts the reconnect backoff afresh.
  conn.attempt = 0;
  ackThrough(Math.round(frame.timestamp / 1000));
  // The first frame after a quiet refresh is drawn at once, half a refresh sooner on average
  // than at the next animation frame (the low-latency canvas shows it at the next refresh
  // either way); frames that follow before that refresh wait for it, newest first. So the
  // canvas is drawn at most once per refresh: drawing each frame as it came made a busy main
  // thread on a loaded machine fall behind the decoder.
  if (video.raf) {
    if (video.frame) video.frame.close();
    video.frame = frame;
    return;
  }
  draw(frame);
  video.raf = requestAnimationFrame(nextRefresh);
}

/** Draws the frame that waited for this refresh, if any, and books the next one for it. */
function nextRefresh() {
  video.raf = 0;
  const frame = video.frame;
  if (!frame) return;
  video.frame = null;
  draw(frame);
  video.raf = requestAnimationFrame(nextRefresh);
}

/**
 * Frames went into the decoder and none came out for STALL_MS. Some decoders hold the newest
 * frames until more input arrives (Android MediaCodec outside its low-latency mode, which Chrome
 * does not request), and a static desktop sends none: the picture would lag one change behind and
 * the server would stall on the missing ACKs. flush() pushes them out. A flush that outputs
 * nothing, or hangs, means the decoder is broken.
 */
function watchDecoder(now) {
  const d = video.decoder;
  if (!d || d.state !== 'configured') return;
  if (video.flush) {
    if (now - video.flush.at > STALL_MS) decoderFailed(new DOMException('The decoder stopped responding.', 'EncodingError'));
    return;
  }
  const oldest = video.pending.values().next().value;
  if (oldest === undefined || now - oldest < STALL_MS || now - video.lastOutputAt < STALL_MS || d.decodeQueueSize > 0) return;
  stats.decoderStalls++;
  // WebCodecs: the chunk after a flush() must be a keyframe. The next delta asks for one.
  video.needKey = true;
  const flush = { at: now, seq: [...video.pending.keys()].pop(), decoded: stats.framesDecoded };
  video.flush = flush;
  d.flush().then(() => {
    if (video.flush !== flush) return;
    video.flush = null;
    if (stats.framesDecoded === flush.decoded) decoderFailed(new DOMException('The decoder produced no output.', 'EncodingError'));
    else ackThrough(flush.seq); // frames the decoder dropped will not come out later
  }, () => {
    if (video.flush === flush) video.flush = null;
  });
}

/** ACKs every pending seq up to `seq`, oldest first: one ACK per seq, cumulative by construction. */
function ackThrough(seq) {
  const now = performance.now();
  for (const [s, at] of video.pending) {
    if (s > seq) break;
    video.pending.delete(s);
    const [b, d] = message(ACK, 7);
    d.setUint32(1, s, true);
    d.setUint16(5, Math.min(65535, Math.round(now - at)), true);
    wsSend(b);
  }
}

function requestIdr() {
  if (video.idrTimer || video.undecodable) return;
  // Along a streak of decoder failures the spacing doubles: 0.5, 1, 2 s.
  const spacing = IDR_INTERVAL_MS * 2 ** Math.max(0, video.failures - 1);
  const wait = video.lastIdrAt + spacing - performance.now();
  if (wait > 0) {
    video.idrTimer = setTimeout(() => {
      video.idrTimer = 0;
      if (video.needKey) requestIdr();
    }, wait);
    return;
  }
  if (sendJson({ t: 'idr' })) video.lastIdrAt = performance.now();
}

function draw(frame) {
  const w = frame.displayWidth;
  const h = frame.displayHeight;
  if (ui.canvas.width !== w || ui.canvas.height !== h) {
    ui.canvas.width = w;
    ui.canvas.height = h;
    stats.width = w;
    stats.height = h;
    layout();
  }
  ctx.drawImage(frame, 0, 0);
  stats.lastDrawnSeq = Math.round(frame.timestamp / 1000);
  frame.close();
  stats.framesDrawn++;
  if (opts.probe) probe(stats.lastDrawnSeq);
  // A fresh frame in a live session ends "Connecting…", "Reconnecting…" and decoder trouble alike;
  // frames the decoder still held when the screen size failed do not.
  if (!ui.overlay.hidden && conn.welcomed && !video.sizeNotice) hideOverlay();
}

// Pixel reads (probe, sample) copy through a 1x1 CPU canvas: reading the display canvas itself
// would pull it off the GPU.
let pixelCtx = null;
function readPixel(x, y) {
  if (!pixelCtx) {
    const c = document.createElement('canvas');
    c.width = 1;
    c.height = 1;
    pixelCtx = c.getContext('2d', { willReadFrequently: true });
    pixelCtx.imageSmoothingEnabled = false;
  }
  pixelCtx.drawImage(ui.canvas, x, y, 1, 1, 0, 0, 1, 1);
  return pixelCtx.getImageData(0, 0, 1, 1).data;
}

function probe(seq) {
  const [x, y] = opts.probe;
  if (x >= ui.canvas.width || y >= ui.canvas.height) return;
  const d = readPixel(x, y);
  const rgb = [d[0], d[1], d[2]];
  const last = probeEvents[probeEvents.length - 1];
  // Every change counts, refinement nudges included; the e2e tools classify the colours.
  if (last && rgb.every((v, i) => v === last.rgb[i])) return;
  probeEvents.push({ rgb, at: performance.now(), seq });
  if (probeEvents.length > PROBE_MAX_EVENTS) probeEvents.splice(0, probeEvents.length - PROBE_MAX_EVENTS);
}

function sample(x, y) {
  if (!(x >= 0 && y >= 0 && x < ui.canvas.width && y < ui.canvas.height)) return [0, 0, 0, 0];
  return Array.from(readPixel(Math.floor(x), Math.floor(y)));
}

// ---- H.264 framing: Annex B in, avcC only as the fallback (brief 6.2)

/** NAL units of an Annex B access unit, without start codes or trailing zero bytes. */
function splitNals(b) {
  const out = [];
  let start = -1;
  const push = (end) => {
    while (end > start && b[end - 1] === 0) end--;
    if (end > start) out.push(b.subarray(start, end));
  };
  for (let i = 0; i + 2 < b.length; i++) {
    if (b[i] === 0 && b[i + 1] === 0 && b[i + 2] === 1) {
      if (start >= 0) push(i);
      i += 2;
      start = i + 1;
    }
  }
  if (start >= 0) push(b.length);
  return out;
}

/** AVCDecoderConfigurationRecord (ISO 14496-15) for one SPS and PPS, 4-byte NAL lengths. */
function avcDescription(sps, pps) {
  const ext = [100, 110, 122, 144].includes(sps[1]) ? highProfileExt(sps) : [];
  const d = new Uint8Array(11 + sps.length + pps.length + ext.length);
  d.set([1, sps[1], sps[2], sps[3], 0xff, 0xe1, sps.length >> 8, sps.length & 0xff]);
  d.set(sps, 8);
  let o = 8 + sps.length;
  d.set([1, pps.length >> 8, pps.length & 0xff], o);
  d.set(pps, o + 3);
  o += 3 + pps.length;
  d.set(ext, o);
  return d;
}

// High-profile avcC tail: chroma format and bit depths read from the SPS (exp-Golomb, after
// removing emulation-prevention bytes), and no SPS extensions.
function highProfileExt(sps) {
  const rbsp = [];
  for (let i = 4, zeros = 0; i < sps.length && rbsp.length < 32; i++) {
    if (zeros >= 2 && sps[i] === 3) {
      zeros = 0;
      continue;
    }
    zeros = sps[i] === 0 ? zeros + 1 : 0;
    rbsp.push(sps[i]);
  }
  let bit = 0;
  const u1 = () => ((rbsp[bit >> 3] ?? 0) >> (7 - (bit++ & 7))) & 1;
  const ue = () => {
    let n = 0;
    while (n < 31 && !u1()) n++;
    let v = 0;
    for (let k = 0; k < n; k++) v = v * 2 + u1();
    return 2 ** n - 1 + v;
  };
  ue(); // seq_parameter_set_id
  const chroma = ue();
  if (chroma === 3) u1(); // separate_colour_plane_flag
  const lumaDepth = ue();
  const chromaDepth = ue();
  return [0xfc | (chroma & 3), 0xf8 | (lumaDepth & 7), 0xf8 | (chromaDepth & 7), 0];
}

/** Annex B access unit -> length-prefixed NAL units; parameter sets live in the description. */
function toAvcc(au) {
  const nals = splitNals(au).filter((n) => {
    const t = n[0] & 0x1f;
    return t !== 7 && t !== 8 && t !== 9;
  });
  const out = new Uint8Array(nals.reduce((n, nal) => n + 4 + nal.length, 0));
  const dv = new DataView(out.buffer);
  let o = 0;
  for (const nal of nals) {
    dv.setUint32(o, nal.length);
    out.set(nal, o + 4);
    o += 4 + nal.length;
  }
  return out;
}

// ---- cursor

function onCursorShape(buf) {
  if (buf.byteLength < 13) return;
  const dv = new DataView(buf);
  const serial = dv.getUint32(1, true);
  const w = dv.getUint16(5, true);
  const h = dv.getUint16(7, true);
  let shape = cursor.shapes.get(serial);
  if (!shape) {
    if (!w || !h) {
      shape = { hidden: true };
    } else {
      if (buf.byteLength < 13 + w * h * 4) return;
      const c = document.createElement('canvas');
      c.width = w;
      c.height = h;
      c.getContext('2d').putImageData(new ImageData(new Uint8ClampedArray(buf, 13, w * h * 4), w, h), 0, 0);
      shape = { url: c.toDataURL(), w, h, xhot: Math.min(dv.getUint16(9, true), w - 1), yhot: Math.min(dv.getUint16(11, true), h - 1) };
    }
    cursor.shapes.set(serial, shape);
    if (cursor.shapes.size > CURSOR_CACHE) cursor.shapes.delete(cursor.shapes.keys().next().value);
  }
  cursor.shape = shape;
  applyCursor();
}

function onCursorPos(buf) {
  if (buf.byteLength < 5) return;
  const dv = new DataView(buf);
  const x = dv.getInt16(1, true);
  const y = dv.getInt16(3, true);
  cursor.pos = [x, y];
  input.serverCursor(x, y);
  scheduleSprite();
}

// A controlling mouse user gets the real cursor shape as the CSS cursor; everyone else (viewers,
// touch, oversized shapes) sees a sprite at the remote position.
function cssCursorMode() {
  const s = cursor.shape;
  return stats.control && lastPointer === 'mouse' && !(s && !s.hidden && (s.w > CSS_CURSOR_MAX || s.h > CSS_CURSOR_MAX));
}

function applyCursor() {
  const s = cursor.shape;
  let css = '';
  if (cssCursorMode()) css = !s ? 'default' : s.hidden ? 'none' : `url("${s.url}") ${s.xhot} ${s.yhot}, default`;
  else if (stats.control && lastPointer === 'mouse') css = 'none';
  if (css !== cssCursor) {
    cssCursor = css;
    ui.canvas.style.cursor = css;
  }
  scheduleSprite();
}

function scheduleSprite() {
  if (!spriteRaf) spriteRaf = requestAnimationFrame(updateSprite);
}

function updateSprite() {
  spriteRaf = 0;
  const pos = input.virtualCursor() || cursor.pos;
  const shape = cursor.shape || DEFAULT_CURSOR;
  const show = Boolean(pos && ui.canvas.width && !shape.hidden && !cssCursorMode());
  ui.sprite.hidden = !show;
  if (!show) return;
  if (ui.sprite.getAttribute('src') !== shape.url) ui.sprite.src = shape.url;
  const k = Math.max(view.s * view.z, MIN_CURSOR_SCALE) / view.z;
  ui.sprite.style.width = `${shape.w * k}px`;
  ui.sprite.style.height = `${shape.h * k}px`;
  ui.sprite.style.transform = `translate(${pos[0] * view.s - shape.xhot * k}px, ${pos[1] * view.s - shape.yhot * k}px)`;
}

// ---- layout: fit or 1:1, plus local pinch zoom as a transform on the stage

function viewportSize() {
  return [view.vw, view.vh];
}

function zoomLimits() {
  const [vw, vh] = viewportSize();
  const fit = Math.min(vw / ui.canvas.width, vh / ui.canvas.height);
  const min = view.mode === 'fit' ? 1 : Math.min(1, fit / view.s);
  return [min, Math.max(min, MAX_ZOOM_SCALE / view.s)];
}

function layout() {
  view.vw = ui.viewport.clientWidth;
  view.vh = ui.viewport.clientHeight;
  input.layoutChanged();
  const W = ui.canvas.width;
  const H = ui.canvas.height;
  const [vw, vh] = viewportSize();
  if (!W || !H || !vw || !vh) return;
  view.s = view.mode === 'fit' ? Math.min(vw / W, vh / H) : 1 / (window.devicePixelRatio || 1);
  // Desktop 1:1 scrolls natively; touch devices pan with gestures instead.
  view.scroll = view.mode === '1' && !touchDevice;
  ui.viewport.classList.toggle('scroll', view.scroll);
  ui.stage.style.width = `${W * view.s}px`;
  ui.stage.style.height = `${H * view.s}px`;
  const [zmin, zmax] = zoomLimits();
  view.z = view.scroll ? 1 : clamp(view.z, zmin, zmax);
  clampPan();
  applyView();
}

function clampPan() {
  if (view.scroll) {
    view.tx = 0;
    view.ty = 0;
    return;
  }
  const [vw, vh] = viewportSize();
  const w = ui.canvas.width * view.s * view.z;
  const h = ui.canvas.height * view.s * view.z;
  view.tx = w <= vw ? (vw - w) / 2 : clamp(view.tx, vw - w, 0);
  view.ty = h <= vh ? (vh - h) / 2 : clamp(view.ty, vh - h, 0);
}

function applyView() {
  ui.stage.style.transform = view.scroll ? '' : `translate(${view.tx}px, ${view.ty}px) scale(${view.z})`;
  input.layoutChanged();
  scheduleSprite();
}

const viewApi = {
  scale: () => view.s * view.z,
  zoomAt(f, clientX, clientY) {
    if (view.scroll || !ui.canvas.width) return;
    const r = ui.viewport.getBoundingClientRect();
    const x = clientX - r.left;
    const y = clientY - r.top;
    const [zmin, zmax] = zoomLimits();
    const z = clamp(view.z * f, zmin, zmax);
    view.tx = x - ((x - view.tx) * z) / view.z;
    view.ty = y - ((y - view.ty) * z) / view.z;
    view.z = z;
    clampPan();
    applyView();
  },
  panBy(dx, dy) {
    if (view.scroll || !ui.canvas.width) return;
    view.tx += dx;
    view.ty += dy;
    clampPan();
    applyView();
  },
  /** Pans the zoomed view so video point (x, y) stays clear of the edges. */
  reveal(x, y) {
    if (view.scroll || !ui.canvas.width) return;
    const [vw, vh] = viewportSize();
    const m = Math.min(48, vw / 4, vh / 4);
    const sx = view.tx + x * view.s * view.z;
    const sy = view.ty + y * view.s * view.z;
    if (sx < m) view.tx += m - sx;
    else if (sx > vw - m) view.tx -= sx - (vw - m);
    if (sy < m) view.ty += m - sy;
    else if (sy > vh - m) view.ty -= sy - (vh - m);
    clampPan();
    applyView();
  },
};

function setScaleMode(mode) {
  view.mode = mode;
  view.z = 1;
  view.tx = 0;
  view.ty = 0;
  ui.scale.textContent = mode === 'fit' ? 'Fit' : '1:1';
  ui.scale.title = mode === 'fit' ? 'Scaled to fit; switch to 1:1 pixels' : '1:1 pixels; switch to fit';
  layout();
}

function watchDpr() {
  matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`).addEventListener('change', () => {
    layout();
    watchDpr();
  }, { once: true });
}

// ---- input

const input = createInput({
  viewport: ui.viewport,
  canvas: ui.canvas,
  textarea: ui.kbd,
  send: wsSend,
  videoSize: () => [ui.canvas.width, ui.canvas.height],
  touchMode: () => touchMode,
  cmdToCtrl: () => cmdToCtrl,
  optionAsAlt: () => optionAsAlt,
  view: viewApi,
  onChange() {
    renderSticky();
    scheduleSprite();
  },
  onCursor: scheduleSprite,
  onPointerType(type) {
    lastPointer = type === 'mouse' ? 'mouse' : 'touch';
    applyCursor();
  },
});

function takeControl() {
  wantControl = true;
  conn.take = null;
  sendJson({ t: 'control', take: true });
}

// ---- UI

function setDot(state, label) {
  ui.dot.className = state;
  ui.dot.title = label;
  ui.dot.setAttribute('aria-label', label);
}

function updateControls() {
  const canControl = conn.welcomed && stats.role === 'control';
  ui.control.disabled = !canControl;
  ui.control.setAttribute('aria-pressed', String(stats.control));
  ui.control.title = !conn.welcomed ? 'Not connected'
    : stats.role !== 'control' ? 'View-only session'
      : stats.control ? 'Release control'
        : conn.held ? 'Take control from the current controller' : 'Take control';
  const typing = stats.control;
  for (const b of [ui.keyboard, ui.keys, ui.type, ui.typerSend, ...ui.strip.querySelectorAll('button')]) b.disabled = !typing;
  if (!typing) {
    ui.strip.hidden = true;
    ui.keys.setAttribute('aria-pressed', 'false');
  }
  ui.info.textContent = [stats.session && `session ${stats.session}`, stats.role && `${stats.role} role`, conn.server].filter(Boolean).join(' · ');
  armToolbarHide();
}

function renderSticky() {
  const s = input.sticky();
  for (const b of ui.strip.querySelectorAll('[data-mod]')) {
    const level = s[b.dataset.mod];
    b.dataset.state = String(level);
    b.setAttribute('aria-pressed', String(level > 0));
  }
}

function showOverlay(title, detail = '', { retry = false, askToken = false } = {}) {
  ui.overlayMsg.textContent = title;
  ui.overlayDetail.textContent = detail;
  ui.overlayDetail.hidden = !detail;
  ui.retry.hidden = !retry;
  ui.tokenForm.hidden = !askToken;
  ui.overlay.classList.toggle('solid', !ui.canvas.width);
  ui.overlay.hidden = false;
  showToolbar();
}

function hideOverlay() {
  ui.overlay.hidden = true;
  armToolbarHide();
}

function showFatal() {
  clearTimeout(conn.timer);
  setDot('bad', 'Disconnected');
  showOverlay(conn.fatal, '', { retry: true, askToken: conn.askToken });
}

let toastTimer = 0;
function toast(text) {
  ui.toast.textContent = text;
  ui.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { ui.toast.hidden = true; }, TOAST_MS);
}

// Touch devices always hide the toolbar after a pause; desktops only while controlling, when it
// would cover part of the remote screen.
let hideTimer = 0;
function showToolbar() {
  ui.toolbar.classList.remove('away');
  ui.edge.hidden = true;
  armToolbarHide();
}

function armToolbarHide() {
  clearTimeout(hideTimer);
  if (!(touchDevice || stats.control)) {
    ui.toolbar.classList.remove('away');
    ui.edge.hidden = true;
    return;
  }
  hideTimer = setTimeout(() => {
    // Hover and focus stick after taps on mobile browsers, so they only count with a mouse.
    const busy = !conn.welcomed || !ui.overlay.hidden || !ui.menu.hidden || !ui.typer.hidden
      || (!touchDevice && (ui.toolbar.matches(':hover') || ui.toolbar.contains(document.activeElement)));
    if (busy) {
      armToolbarHide();
      return;
    }
    ui.toolbar.classList.add('away');
    // Touch users get a tap target at the top edge; mice reveal the bar by touching the edge.
    ui.edge.hidden = !touchDevice;
  }, TOOLBAR_HIDE_MS);
}

function setMenu(open) {
  if (open) {
    // Hang the menu under its button; the toolbar is centred, so the screen edge can be far off.
    const r = ui.menuBtn.getBoundingClientRect();
    ui.menu.style.right = `${Math.max(8, window.innerWidth - r.right)}px`;
  }
  ui.menu.hidden = !open;
  ui.menuBtn.setAttribute('aria-expanded', String(open));
  if (!open) armToolbarHide();
}

function setStats(on) {
  ui.stats.hidden = !on;
  ui.statsToggle.checked = on;
  if (on) updateStats(performance.now());
}

function setTouchMode(mode) {
  touchMode = mode;
  for (const b of ui.menu.querySelectorAll('[data-touch]')) b.setAttribute('aria-checked', String(b.dataset.touch === mode));
  scheduleSprite();
}

function openTyper() {
  ui.typer.hidden = false;
  ui.typerText.value = '';
  ui.typerText.focus();
}

function closeTyper() {
  ui.typer.hidden = true;
  if (stats.control && finePointer.matches && !touchDevice) input.focusKeyboard();
  armToolbarHide();
}

const fsRoot = document.documentElement;
const requestFs = fsRoot.requestFullscreen || fsRoot.webkitRequestFullscreen;
const exitFs = document.exitFullscreen || document.webkitExitFullscreen;
const fullscreenElement = () => document.fullscreenElement || document.webkitFullscreenElement || null;

function toggleFullscreen() {
  try {
    const p = fullscreenElement() ? exitFs.call(document) : requestFs.call(fsRoot);
    if (p && p.catch) p.catch(() => toast('Fullscreen is not available here.'));
  } catch {
    toast('Fullscreen is not available here.');
  }
}

function onFullscreenChange() {
  const on = Boolean(fullscreenElement());
  ui.fullscreen.setAttribute('aria-pressed', String(on));
  // Keyboard Lock (Chromium, fullscreen only) lets Esc and browser shortcuts reach the remote.
  const kl = navigator.keyboard;
  if (on && kl && kl.lock) kl.lock().catch(() => {});
  else if (!on && kl && kl.unlock) kl.unlock();
}

let rates = { at: performance.now(), decoded: 0, drawn: 0, bytes: 0 };
function updateStats(now) {
  const dt = (now - rates.at) / 1000;
  if (dt >= 0.5) {
    rates = {
      at: now,
      decoded: stats.framesDecoded,
      drawn: stats.framesDrawn,
      bytes: stats.bytes,
      fpsDecoded: (stats.framesDecoded - rates.decoded) / dt,
      fpsDrawn: (stats.framesDrawn - rates.drawn) / dt,
      kbps: ((stats.bytes - rates.bytes) * 8) / 1000 / dt,
    };
  }
  if (ui.stats.hidden) return;
  const n = (v, digits = 1) => (typeof v === 'number' && Number.isFinite(v) ? v.toFixed(Number.isInteger(v) ? 0 : digits) : '-');
  const s = serverStats || {};
  ui.stats.textContent = [
    `fps    ${n(rates.fpsDecoded)} decoded  ${n(rates.fpsDrawn)} drawn`,
    `recv   ${n(rates.kbps, 0)} kbps   ping ${stats.rttMs ?? '-'} ms`,
    `video  ${stats.width}x${stats.height} ${stats.codec || '-'} ${video.framing}${video.hw === 'prefer-software' ? ' software' : ''}  queue ${video.decoder ? video.decoder.decodeQueueSize : 0}`,
    `you    ${stats.role ? `${stats.role} role` : '-'}${stats.control ? ', in control' : ''}  errors ${stats.decodeErrors}  stalls ${stats.decoderStalls}`,
    `server ${n(s.fps)} fps  ${n(s.kbps, 0)} kbps  target ${n(s.bitrate_kbps, 0)} kbps`,
    `       rtt ${n(s.rtt_ms)} (min ${n(s.min_rtt_ms)})  queue ${n(s.queue_ms)}  enc ${n(s.enc_ms)} ms`,
    `       inflight ${n(s.inflight)}  skipped ${n(s.skipped)}  viewers ${n(s.viewers)}`,
  ].join('\n');
}

function wireUi() {
  // Toolbar presses must not take focus from the hidden textarea: that would stop desktop typing
  // and close the soft keyboard. Cancelling mousedown keeps the click in every engine; cancelling
  // pointerdown would lose it in WebKit.
  for (const el of [ui.toolbar, ui.strip, ui.menu]) {
    el.addEventListener('mousedown', (e) => {
      if (!(e.target instanceof HTMLInputElement)) e.preventDefault();
    });
  }
  ui.toolbar.addEventListener('pointerdown', armToolbarHide);
  ui.edge.addEventListener('pointerdown', (e) => {
    e.preventDefault();
    showToolbar();
  });
  // The bar slides in under the finger, so the touch must not end in a click: it would press the
  // button now under it (in Chrome, and on iOS even after a drag). Cancelling pointerdown does not
  // stop that click.
  ui.edge.addEventListener('touchstart', (e) => e.preventDefault(), { passive: false });
  window.addEventListener('pointermove', (e) => {
    if (e.pointerType === 'mouse' && e.clientY <= 4) showToolbar();
  });

  ui.control.addEventListener('click', () => {
    if (!stats.control) {
      takeControl();
      return;
    }
    wantControl = false;
    sendJson({ t: 'control', take: false });
  });
  ui.keyboard.hidden = !touchDevice;
  let keyboardWasOpen = false;
  ui.keyboard.addEventListener('pointerdown', () => { keyboardWasOpen = input.keyboardFocused(); });
  ui.keyboard.addEventListener('click', () => {
    // iOS opens the soft keyboard only for a focus() made inside this handler.
    if (keyboardWasOpen) input.blurKeyboard();
    else input.focusKeyboard();
    keyboardWasOpen = false;
  });
  ui.keys.addEventListener('click', () => {
    ui.strip.hidden = !ui.strip.hidden;
    ui.keys.setAttribute('aria-pressed', String(!ui.strip.hidden));
  });
  ui.type.addEventListener('click', openTyper);
  ui.scale.addEventListener('click', () => setScaleMode(view.mode === 'fit' ? '1' : 'fit'));
  ui.fullscreen.hidden = !requestFs;
  ui.fullscreen.addEventListener('click', toggleFullscreen);
  document.addEventListener('fullscreenchange', onFullscreenChange);
  document.addEventListener('webkitfullscreenchange', onFullscreenChange);
  ui.statsToggle.addEventListener('change', () => {
    setStats(ui.statsToggle.checked);
    save('localStorage', 'tilt.stats', ui.statsToggle.checked ? '1' : '0');
  });
  ui.menuBtn.addEventListener('click', () => setMenu(ui.menu.hidden));
  document.addEventListener('pointerdown', (e) => {
    if (!ui.menu.hidden && !ui.menu.contains(e.target) && !ui.menuBtn.contains(e.target)) setMenu(false);
  }, true);

  // Extra keys act on pointerdown and never take focus, so the soft keyboard stays open.
  ui.strip.addEventListener('touchstart', (e) => e.preventDefault(), { passive: false });
  for (const b of ui.strip.querySelectorAll('button')) {
    if (b.dataset.mod) {
      b.addEventListener('pointerdown', (e) => {
        e.preventDefault();
        if (!b.disabled) input.toggleSticky(b.dataset.mod);
      });
      continue;
    }
    const ks = parseInt(b.dataset.ks, 16);
    b.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      if (!b.disabled) input.holdKey(ks);
    });
    for (const type of ['pointerup', 'pointercancel', 'pointerleave']) b.addEventListener(type, () => input.releaseKey(ks));
  }

  ui.touchRow.hidden = !touchDevice;
  for (const b of ui.menu.querySelectorAll('[data-touch]')) {
    b.addEventListener('click', () => {
      setTouchMode(b.dataset.touch);
      save('localStorage', 'tilt.touch', touchMode);
    });
  }
  ui.cmdRow.hidden = !APPLE;
  ui.cmdToggle.checked = cmdToCtrl;
  ui.cmdToggle.addEventListener('change', () => {
    cmdToCtrl = ui.cmdToggle.checked;
    save('localStorage', 'tilt.cmdToCtrl', cmdToCtrl ? '1' : '0');
  });
  ui.altRow.hidden = !APPLE;
  ui.altToggle.checked = optionAsAlt;
  ui.altToggle.addEventListener('change', () => {
    optionAsAlt = ui.altToggle.checked;
    save('localStorage', 'tilt.optionAsAlt', optionAsAlt ? '1' : '0');
  });

  ui.typerForm.addEventListener('submit', (e) => {
    e.preventDefault();
    if (!stats.control) return;
    if (ui.typerText.value) input.sendText(ui.typerText.value);
    closeTyper();
  });
  ui.typerCancel.addEventListener('click', closeTyper);
  ui.typerText.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeTyper();
    else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) ui.typerForm.requestSubmit();
  });

  ui.retry.addEventListener('click', reconnect);
  ui.tokenForm.addEventListener('submit', (e) => {
    e.preventDefault();
    token = ui.tokenInput.value.trim();
    save('sessionStorage', 'tilt.token', token);
    ui.tokenInput.value = '';
    reconnect();
  });

  window.addEventListener('hashchange', () => {
    const t = takeHashToken();
    if (t !== null && t !== token) {
      token = t;
      reconnect();
    }
  });

  document.addEventListener('visibilitychange', () => {
    syncVideo();
    if (document.visibilityState === 'hidden') return;
    // Hidden pages do not reconnect, and background timers are throttled: connect now.
    if (!conn.ws && !conn.fatal) {
      conn.attempt = 0;
      connect();
    } else {
      maybeTake();
    }
  });

  new ResizeObserver(layout).observe(ui.viewport);
  // The viewport ends above the strip (CSS --strip), whose height depends on how its rows wrap.
  // Applied a frame later: resizing the viewport inside this callback would trip the
  // ResizeObserver loop limit.
  new ResizeObserver(() => requestAnimationFrame(() => {
    const h = ui.strip.hidden ? 0 : ui.strip.offsetHeight + 16;
    document.documentElement.style.setProperty('--strip', `${h}px`);
  })).observe(ui.strip);
  watchDpr();
  const vv = window.visualViewport;
  if (vv) {
    // The on-screen keyboard shrinks the visual viewport (CSS --kb). So does a page the user
    // zoomed, which is no reason to shrink the remote screen.
    const onViewport = () => {
      const kb = vv.scale > 1.01 ? 0 : Math.max(0, window.innerHeight - vv.height - vv.offsetTop);
      document.documentElement.style.setProperty('--kb', `${Math.round(kb)}px`);
    };
    vv.addEventListener('resize', onViewport);
    vv.addEventListener('scroll', onViewport);
  }
}

window.tilt = {
  get ws() { return conn.ws; },
  stats,
  sample,
  probeEvents,
  get lastInputAt() { return input.lastInputAt; },
  takeControl,
  reconnect,
};

wireUi();
setScaleMode(view.mode);
setTouchMode(touchMode);
setStats(opts.stats);
updateControls();

if (typeof VideoDecoder === 'undefined' || typeof EncodedVideoChunk === 'undefined') {
  ui.unsupportedMsg.textContent = window.isSecureContext ? UNSUPPORTED : `${UNSUPPORTED} ${INSECURE}`;
  ui.unsupported.hidden = false;
  ui.overlay.hidden = true;
  setDot('bad', 'Unsupported browser');
} else {
  connect();
  setInterval(tick, PING_MS);
}
