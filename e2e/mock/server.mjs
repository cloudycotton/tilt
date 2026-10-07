// Mock tilt server for testing the web client without Xvfb or the Rust binary. It implements the
// server side of protocol v1 (brief 4) on node:http + ws and streams real OpenH264 output
// (fixtures/*.h264, made by fixtures/gen). Every decoded client message is logged as a JSON line.
//
// The fixtures mimic tilt-testpattern: 4 segments, one per marker colour, each starting with an
// IDR. KEY down, TEXT and BUTTON 1 down from the controller advance the marker like the real
// testpattern, and the stream jumps to that segment's IDR.
//
//   node server.mjs [--port 6090] [--host 127.0.0.1] [--token devtoken] [--view-token viewtoken]
//                   [--no-auth] [--token-file PATH] [--stream high_1280x720] [--fps 30]
//                   [--max-msg-bytes 262144] [--max-viewers 4] [--window 4] [--no-loop]
//                   [--log FILE] [--quiet]
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.join(HERE, 'fixtures');
const SERVER = 'tilt-mock/0.1';

// Exactly the Rust server's static routes (src/assets.rs).
const ASSETS = {
  '/': ['index.html', 'text/html; charset=utf-8'],
  '/app.js': ['app.js', 'text/javascript; charset=utf-8'],
  '/input.js': ['input.js', 'text/javascript; charset=utf-8'],
  '/keysyms.js': ['keysyms.js', 'text/javascript; charset=utf-8'],
  '/icon.svg': ['icon.svg', 'image/svg+xml'],
};

const VIDEO = 0x01;
const CURSOR_SHAPE = 0x02;
const CURSOR_POS = 0x03;
const PONG = 0x04;
const ACK = 0x10;
const PING = 0x11;
const MOVE = 0x20;
const BUTTON = 0x21;
const WHEEL = 0x22;
const KEY = 0x30;
const TEXT = 0x31;
const RELEASE_ALL = 0x32;
const FLAG_KEY = 1;
const FLAG_MORE = 2;
const VIDEO_HEADER = 18;
const CLOSE = { auth: 4001, busy: 4002, version: 4003, not_ready: 4004, hello_timeout: 4005 };
const IDR_INTERVAL_MS = 500;
const MARKER_COLORS = 4;

// Exact binary message sizes; TEXT is variable, up to MAX_TEXT_BYTES of UTF-8 (src/protocol.rs).
const SIZES = { [ACK]: 7, [PING]: 5, [MOVE]: 5, [BUTTON]: 7, [WHEEL]: 9, [KEY]: 6, [RELEASE_ALL]: 1 };
const MAX_TEXT_BYTES = 4096;
const INPUT = new Set([MOVE, BUTTON, WHEEL, KEY, TEXT, RELEASE_ALL]);

export const DEFAULTS = {
  port: 0,
  host: '127.0.0.1',
  token: 'devtoken',
  viewToken: 'viewtoken',
  noAuth: false,
  tokenFile: null, // read on every hello, like TILT_TOKEN_FILE; missing file -> not_ready
  maxViewers: 4,
  namesHolder: true, // false: `control` messages without `holder`, as from servers before it
  maxMsgBytes: 262144,
  fps: 30,
  window: 4, // frames in flight before the stream waits for ACKs
  stream: 'high_1280x720',
  loop: true, // false: a segment plays once and the stream goes idle, like a static desktop
  helloTimeoutMs: 5000,
  authDelayMs: 500,
  webDir: path.resolve(HERE, '../../web'),
  log: null, // JSON-lines file; stdout when null and !quiet
  quiet: false,
};

const streams = new Map();

/** A fixture stream: Annex B access units plus the index written by fixtures/gen. */
export function loadStream(name) {
  let s = streams.get(name);
  if (s) return s;
  const m = /_(\d+)x(\d+)$/.exec(name);
  if (!m) throw new Error(`stream name must end in _WxH: ${name}`);
  const data = fs.readFileSync(path.join(FIXTURES, `${name}.h264`));
  const frames = fs.readFileSync(path.join(FIXTURES, `${name}.idx`), 'utf8').trim().split('\n').map((line) => {
    const [, off, size, type, seg] = line.split(' ').map(Number);
    return { key: type === 1, seg, data: data.subarray(off, off + size) };
  });
  const segStart = [];
  const segEnd = [];
  frames.forEach((f, i) => {
    if (segStart[f.seg] === undefined) segStart[f.seg] = i;
    segEnd[f.seg] = i + 1;
  });
  if (segStart.length !== MARKER_COLORS || segStart.some((i) => !frames[i].key)) throw new Error(`bad fixture index: ${name}`);
  s = { name, w: Number(m[1]), h: Number(m[2]), frames, segStart, segEnd };
  streams.set(name, s);
  return s;
}

// A 12x19 arrow (X black, . white, space transparent), hotspot 0,0.
const ARROW = [
  'X           ', 'XX          ', 'X.X         ', 'X..X        ', 'X...X       ', 'X....X      ',
  'X.....X     ', 'X......X    ', 'X.......X   ', 'X........X  ', 'X.........X ', 'X......XXXXX',
  'X...X..X    ', 'X..XX..X    ', 'X.X  X..X   ', 'XX   X..X   ', 'X     X..X  ', '      X..X  ',
  '       XX   ',
];

export function arrowCursor() {
  const w = ARROW[0].length;
  const h = ARROW.length;
  const rgba = Buffer.alloc(w * h * 4);
  ARROW.forEach((row, y) => [...row].forEach((c, x) => {
    if (c === ' ') return;
    const v = c === 'X' ? 0 : 255;
    rgba.set([v, v, v, 255], (y * w + x) * 4);
  }));
  return { w, h, xhot: 0, yhot: 0, rgba };
}

/** A solid w x h cursor, e.g. to test shapes over the 128 px CSS cursor limit. */
export function solidCursor(w, h, [r, g, b, a] = [255, 0, 255, 255], xhot = 0, yhot = 0) {
  const rgba = Buffer.alloc(w * h * 4);
  for (let i = 0; i < rgba.length; i += 4) rgba.set([r, g, b, a], i);
  return { w, h, xhot, yhot, rgba };
}

// Deterministic bytes for corrupt frames.
function noise(n, seed = 0x9e3779b9) {
  const out = Buffer.alloc(n);
  let x = seed >>> 0;
  for (let i = 0; i < n; i++) {
    x ^= x << 13;
    x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5;
    x >>>= 0;
    out[i] = x & 0xff;
  }
  return out;
}

// Both reliably raise EncodingError in Chrome (hardware and software decode) and WebKit.
const CORRUPTIONS = {
  empty_slice: () => Buffer.from([0, 0, 0, 1, 0x61]),
  p_garbage: (au) => Buffer.concat([au.subarray(0, 8), noise(600)]),
};

export async function startMockServer(options = {}) {
  const opts = { ...DEFAULTS, ...options };
  const t0 = performance.now();
  const messages = []; // every decoded client message, plus session lifecycle events
  const sent = []; // every VIDEO access unit sent
  const violations = []; // client behaviour that breaks the protocol
  const waiters = new Set();
  const sessions = new Map();
  const logFile = opts.log ? fs.createWriteStream(opts.log, { flags: 'a' }) : null;
  const state = {
    stream: loadStream(opts.stream),
    marker: 0,
    holder: null,
    cursor: { serial: 1, ...arrowCursor() },
    cursorPos: null,
    nextId: 1,
    unsupported: null, // the unsupported_size message while the encoder rejects the screen's size
    closeOnWelcome: null, // close code for sessions to end right after their welcome
  };
  state.cursorPos = [state.stream.w >> 1, state.stream.h >> 1];

  const now = () => performance.now() - t0;

  function record(sess, entry) {
    const rec = { at: Math.round(now() * 1000) / 1000, session: sess ? sess.id : null, ...entry };
    messages.push(rec);
    const line = JSON.stringify(rec);
    if (logFile) logFile.write(line + '\n');
    else if (!opts.quiet) process.stdout.write(line + '\n');
    for (const w of waiters) {
      if (w.pred(rec)) {
        waiters.delete(w);
        clearTimeout(w.timer);
        w.resolve(rec);
      }
    }
    return rec;
  }

  function violation(sess, what) {
    violations.push({ at: now(), session: sess ? sess.id : null, what });
    record(sess, { type: 'violation', what });
  }

  // ---- sending

  function sendRaw(sess, data) {
    if (sess.ws.readyState === sess.ws.OPEN) sess.ws.send(data);
  }

  const sendJson = (sess, obj) => sendRaw(sess, JSON.stringify(obj));

  function cursorShapeMsg() {
    const c = state.cursor;
    const b = Buffer.alloc(13 + c.rgba.length);
    b[0] = CURSOR_SHAPE;
    b.writeUInt32LE(c.serial, 1);
    b.writeUInt16LE(c.w, 5);
    b.writeUInt16LE(c.h, 7);
    b.writeUInt16LE(c.xhot, 9);
    b.writeUInt16LE(c.yhot, 11);
    c.rgba.copy(b, 13);
    return b;
  }

  function cursorPosMsg() {
    const b = Buffer.alloc(5);
    b[0] = CURSOR_POS;
    b.writeInt16LE(state.cursorPos[0], 1);
    b.writeInt16LE(state.cursorPos[1], 3);
    return b;
  }

  const live = () => [...sessions.values()].filter((s) => s.welcomed && !s.closed);

  function broadcast(data, except = null) {
    for (const s of live()) if (s !== except && !s.frozen) sendRaw(s, data);
  }

  function controlMsg(sess) {
    const h = state.holder;
    return { t: 'control', you: sess === h, held: h !== null, holder: h && opts.namesHolder ? h.id : undefined };
  }

  function broadcastControl() {
    for (const s of live()) sendJson(s, controlMsg(s));
  }

  // ---- video

  function nextFrame(sess) {
    const st = state.stream;
    if (sess.marker !== state.marker || sess.forceIdr || sess.pos < 0) {
      sess.marker = state.marker;
      sess.forceIdr = false;
      sess.pos = st.segStart[state.marker % MARKER_COLORS];
      return st.frames[sess.pos];
    }
    const seg = state.marker % MARKER_COLORS;
    let i = sess.pos + 1;
    if (i >= st.segEnd[seg]) {
      if (!opts.loop) return null;
      i = st.segStart[seg];
    }
    sess.pos = i;
    return st.frames[i];
  }

  function pump(sess) {
    if (!sess.videoOn || sess.frozen || sess.closed || state.unsupported) return;
    // Brief 5.4: with frames in flight, send only while under the credit window.
    if (sess.inflight.size && sess.inflight.size >= opts.window) {
      sess.blocked++;
      return;
    }
    const frame = nextFrame(sess);
    if (frame) sendFrame(sess, frame);
  }

  function sendFrame(sess, frame) {
    const st = state.stream;
    let au = frame.data;
    let corrupt = null;
    if (!frame.key && sess.corrupt) {
      corrupt = sess.corrupt;
      sess.corrupt = null;
      au = CORRUPTIONS[corrupt](au);
    }
    const seq = ++sess.seq;
    const header = Buffer.alloc(VIDEO_HEADER);
    header[0] = VIDEO;
    header.writeUInt32LE(seq, 2);
    header.writeBigUInt64LE(BigInt(Math.round(performance.now() * 1000)), 6);
    header.writeUInt16LE(st.w, 14);
    header.writeUInt16LE(st.h, 16);
    const room = Math.max(1, opts.maxMsgBytes - VIDEO_HEADER);
    const fragments = Math.max(1, Math.ceil(au.length / room));
    // Taken before sending: on a busy machine this process can be preempted right after the
    // write, long enough for the client to decode and ACK the frame first.
    const at = now();
    for (let i = 0; i < fragments; i++) {
      header[1] = (frame.key ? FLAG_KEY : 0) | (i < fragments - 1 ? FLAG_MORE : 0);
      sendRaw(sess, Buffer.concat([header, au.subarray(i * room, (i + 1) * room)]));
    }
    sess.inflight.set(seq, at);
    sess.maxInflight = Math.max(sess.maxInflight, sess.inflight.size);
    sess.second.frames++;
    sess.second.bytes += au.length + VIDEO_HEADER * fragments;
    if (frame.key) sess.lastIdrAt = at;
    sent.push({ session: sess.id, seq, key: frame.key, bytes: au.length, fragments, marker: frame.seg, at, corrupt });
  }

  function onAck(sess, seq, decodeMs) {
    // One ACK per seq, cumulative: each must be the next seq the client has not acked yet.
    if (seq !== sess.lastAck + 1) violation(sess, `ACK ${seq} after ${sess.lastAck}`);
    if (seq > sess.seq) violation(sess, `ACK ${seq} for an unsent seq (last sent ${sess.seq})`);
    sess.lastAck = Math.max(sess.lastAck, seq);
    const t = now();
    for (const [s, at] of sess.inflight) {
      if (s > seq) break;
      sess.inflight.delete(s);
      const rtt = Math.max(1, t - at - decodeMs);
      sess.srtt = sess.srtt === null ? rtt : sess.srtt + (rtt - sess.srtt) / 8;
      sess.minRtt = Math.min(sess.minRtt, rtt);
    }
  }

  function requestIdr(sess) {
    if (sess.idrTimer) return;
    const wait = sess.lastIdrAt + IDR_INTERVAL_MS - now();
    if (wait <= 0) {
      sess.forceIdr = true;
      return;
    }
    // Deferred, not dropped (brief 4.4).
    sess.idrTimer = setTimeout(() => {
      sess.idrTimer = null;
      sess.forceIdr = true;
    }, wait);
  }

  function sendStats(sess) {
    if (!sess.videoOn || sess.frozen || sess.closed) return;
    const s = sess.second;
    sendJson(sess, {
      t: 'stats',
      fps: s.frames,
      kbps: Math.round((s.bytes * 8) / 100) / 10,
      bitrate_kbps: 8000,
      rtt_ms: sess.srtt === null ? 0 : Math.round(sess.srtt * 10) / 10,
      min_rtt_ms: Number.isFinite(sess.minRtt) ? Math.round(sess.minRtt * 10) / 10 : 0,
      queue_ms: sess.srtt === null ? 0 : Math.round((sess.srtt - sess.minRtt) * 10) / 10,
      enc_ms: 0,
      inflight: sess.inflight.size,
      skipped: sess.blocked,
      viewers: live().length,
    });
    sess.second = { frames: 0, bytes: 0 };
  }

  // ---- control and input

  function setHolder(sess) {
    if (state.holder === sess) return;
    const prev = state.holder;
    state.holder = sess;
    if (prev) record(prev, { type: 'released', reason: sess ? `taken by ${sess.id}` : 'released' });
    broadcastControl();
  }

  function advanceMarker(n = 1) {
    state.marker += n;
  }

  function onInput(sess, type, b) {
    const accepted = sess === state.holder;
    const { w, h } = state.stream;
    const px = (v, size) => Math.round((v * (size - 1)) / 65535);
    let rec;
    switch (type) {
      case MOVE: {
        const x = b.readUInt16LE(1);
        const y = b.readUInt16LE(3);
        rec = { type: 'MOVE', x, y, px: px(x, w), py: px(y, h) };
        break;
      }
      case BUTTON: {
        const x = b.readUInt16LE(3);
        const y = b.readUInt16LE(5);
        rec = { type: 'BUTTON', button: b[1], down: b[2] !== 0, x, y, px: px(x, w), py: px(y, h) };
        break;
      }
      case WHEEL: {
        const x = b.readUInt16LE(5);
        const y = b.readUInt16LE(7);
        rec = { type: 'WHEEL', dx: b.readInt16LE(1), dy: b.readInt16LE(3), x, y, px: px(x, w), py: px(y, h) };
        break;
      }
      case KEY:
        rec = { type: 'KEY', down: b[1] !== 0, keysym: b.readUInt32LE(2) };
        break;
      case TEXT:
        // The real server types the first MAX_TEXT_BYTES and drops the rest.
        if (b.length - 1 > MAX_TEXT_BYTES) violation(sess, `TEXT of ${b.length - 1} bytes, over ${MAX_TEXT_BYTES}`);
        rec = { type: 'TEXT', text: b.subarray(1).toString('utf8') };
        break;
      default:
        rec = { type: 'RELEASE_ALL' };
    }
    record(sess, { ...rec, accepted });
    if (!accepted) return;
    if (rec.px !== undefined) {
      state.cursorPos = [rec.px, rec.py];
      broadcast(cursorPosMsg());
    }
    if ((rec.type === 'KEY' && rec.down) || (rec.type === 'BUTTON' && rec.button === 1 && rec.down)) advanceMarker();
    if (rec.type === 'TEXT') advanceMarker([...rec.text].length);
  }

  function onBinary(sess, b) {
    const type = b[0];
    const size = SIZES[type];
    if (b.length === 0 || (size === undefined && type !== TEXT)) {
      violation(sess, `unknown binary type 0x${(type ?? 0).toString(16)}`);
      return;
    }
    if (size !== undefined && b.length !== size) {
      violation(sess, `type 0x${type.toString(16)} is ${b.length} bytes, expected ${size}`);
      return;
    }
    if (type === ACK) {
      const seq = b.readUInt32LE(1);
      const decodeMs = b.readUInt16LE(5);
      record(sess, { type: 'ACK', seq, decodeMs });
      onAck(sess, seq, decodeMs);
    } else if (type === PING) {
      const t = b.readUInt32LE(1);
      record(sess, { type: 'PING', t });
      if (!sess.frozen) {
        const pong = Buffer.alloc(5);
        pong[0] = PONG;
        pong.writeUInt32LE(t, 1);
        sendRaw(sess, pong);
      }
    } else if (INPUT.has(type)) {
      onInput(sess, type, b);
    }
  }

  function onText(sess, text) {
    let m;
    try {
      m = JSON.parse(text);
    } catch {
      violation(sess, 'text message is not JSON');
      return;
    }
    switch (m && m.t) {
      case 'control':
        record(sess, { type: 'control', take: m.take });
        if (typeof m.take !== 'boolean') violation(sess, 'control.take is not a boolean');
        else if (sess.role !== 'control') sendJson(sess, { t: 'error', code: 'forbidden', msg: 'view-only session' });
        else if (m.take) setHolder(sess);
        else if (state.holder === sess) setHolder(null);
        break;
      case 'idr':
        record(sess, { type: 'idr' });
        requestIdr(sess);
        break;
      case 'video':
        record(sess, { type: 'video', on: m.on });
        if (m.on === false) sess.videoOn = false;
        else if (m.on === true && !sess.videoOn) {
          sess.videoOn = true;
          sess.forceIdr = true;
        } else if (m.on !== true) violation(sess, 'video.on is not a boolean');
        break;
      default:
        record(sess, { type: 'unknown', t: m && m.t });
        violation(sess, `unknown JSON message ${JSON.stringify(m && m.t)}`);
    }
  }

  // ---- handshake

  function fail(sess, code, msg) {
    sendJson(sess, { t: 'error', code, msg });
    record(sess, { type: 'rejected', code });
    sess.ws.close(CLOSE[code], code);
  }

  function readTokenFile() {
    try {
      return fs.readFileSync(opts.tokenFile, 'utf8').trim();
    } catch {
      return null;
    }
  }

  async function onHello(sess, data, isBinary) {
    sess.helloSeen = true;
    clearTimeout(sess.helloTimer);
    let m = null;
    if (!isBinary) {
      try { m = JSON.parse(data.toString('utf8')); } catch { /* reported below */ }
    }
    if (!m || m.t !== 'hello') {
      violation(sess, 'first message is not hello');
      sess.ws.close(1002, 'expected hello');
      return;
    }
    record(sess, { type: 'hello', v: m.v, client: m.client, hasToken: typeof m.token === 'string' });
    if (m.v !== 1) {
      fail(sess, 'version', `server speaks v1, client sent v${m.v}`);
      return;
    }
    let controlToken = opts.token;
    if (opts.tokenFile) {
      controlToken = readTokenFile();
      if (!controlToken) {
        fail(sess, 'not_ready', 'token file missing');
        return;
      }
    }
    let role = 'control';
    if (!opts.noAuth) {
      if (m.token === controlToken) role = 'control';
      else if (opts.viewToken && m.token === opts.viewToken) role = 'view';
      else {
        await new Promise((r) => setTimeout(r, opts.authDelayMs));
        if (!sess.closed) fail(sess, 'auth', 'bad token');
        return;
      }
    }
    if (live().length >= opts.maxViewers) {
      fail(sess, 'busy', `${opts.maxViewers} viewers already connected`);
      return;
    }
    if (sess.closed) return;
    sess.role = role;
    sess.welcomed = true;
    const st = state.stream;
    sendJson(sess, { t: 'welcome', v: 1, session: sess.id, role, screen: { w: st.w, h: st.h }, server: SERVER });
    sendJson(sess, controlMsg(sess));
    sendRaw(sess, cursorShapeMsg());
    sendRaw(sess, cursorPosMsg());
    record(sess, { type: 'welcomed', role });
    if (state.closeOnWelcome !== null) {
      sess.ws.close(state.closeOnWelcome, 'dropped by mock');
      return;
    }
    if (state.unsupported) sendJson(sess, { t: 'error', code: 'unsupported_size', msg: state.unsupported });
    pump(sess);
    sess.pumpTimer = setInterval(() => pump(sess), 1000 / opts.fps);
    sess.statsTimer = setInterval(() => sendStats(sess), 1000);
  }

  function end(sess, code) {
    sess.closed = true;
    sess.closeCode = code;
    clearTimeout(sess.helloTimer);
    clearTimeout(sess.idrTimer);
    clearInterval(sess.pumpTimer);
    clearInterval(sess.statsTimer);
    record(sess, { type: 'closed', code });
    if (state.holder === sess) setHolder(null);
  }

  function onConnection(ws) {
    const sess = {
      id: `s${state.nextId++}`,
      ws,
      role: null,
      welcomed: false,
      helloSeen: false,
      closed: false,
      closeCode: null,
      videoOn: true,
      frozen: false,
      seq: 0,
      lastAck: 0,
      pos: -1,
      marker: -1,
      forceIdr: true,
      lastIdrAt: -Infinity,
      idrTimer: null,
      inflight: new Map(),
      maxInflight: 0,
      blocked: 0,
      srtt: null,
      minRtt: Infinity,
      corrupt: null,
      cut: false,
      second: { frames: 0, bytes: 0 },
      helloTimer: null,
      pumpTimer: null,
      statsTimer: null,
    };
    sessions.set(sess.id, sess);
    record(sess, { type: 'open' });
    sess.helloTimer = setTimeout(() => fail(sess, 'hello_timeout', `no hello within ${opts.helloTimeoutMs} ms`), opts.helloTimeoutMs);
    ws.on('message', (data, isBinary) => {
      if (sess.closed) return;
      if (!sess.helloSeen) {
        onHello(sess, data, isBinary);
        return;
      }
      if (!sess.welcomed) return;
      if (isBinary) onBinary(sess, data);
      else onText(sess, data.toString('utf8'));
    });
    ws.on('close', (code) => {
      // A cut session lives on, as on a server that has not noticed: drop() ends it.
      if (sess.cut) record(sess, { type: 'cut' });
      else end(sess, code);
    });
    ws.on('error', () => {});
  }

  // ---- HTTP

  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://mock');
    if (req.method !== 'GET' && req.method !== 'HEAD') {
      res.writeHead(405).end();
      return;
    }
    if (url.pathname === '/healthz') {
      res.writeHead(200, { 'Content-Type': 'text/plain; charset=utf-8', 'Cache-Control': 'no-cache' }).end('ok');
      return;
    }
    const asset = ASSETS[url.pathname];
    if (!asset) {
      res.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' }).end('not found');
      return;
    }
    // Read per request so edits to web/ show up without restarting the mock.
    fs.readFile(path.join(opts.webDir, asset[0]), (err, body) => {
      if (err) {
        res.writeHead(500).end();
        return;
      }
      res.writeHead(200, {
        'Content-Type': asset[1],
        'Content-Length': body.length,
        'Cache-Control': 'no-cache',
        'X-Content-Type-Options': 'nosniff',
      });
      res.end(req.method === 'HEAD' ? undefined : body);
    });
  });

  const wss = new WebSocketServer({ noServer: true, maxPayload: 1 << 20, perMessageDeflate: false });
  server.on('upgrade', (req, socket, head) => {
    if (new URL(req.url, 'http://mock').pathname !== '/stream') {
      socket.destroy();
      return;
    }
    wss.handleUpgrade(req, socket, head, onConnection);
  });

  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(opts.port, opts.host, resolve);
  });
  const { port } = server.address();

  const pick = (id) => (id ? [sessions.get(id)].filter(Boolean) : live());

  return {
    url: `http://${opts.host}:${port}/`,
    port,
    options: opts,
    messages,
    sent,
    violations,
    /** Session state by id ('s1', 's2', ...). */
    session: (id) => sessions.get(id),
    sessions: () => [...sessions.values()],
    holder: () => (state.holder ? state.holder.id : null),
    marker: () => state.marker,
    /** Resolves with the first message (from index `since`) matching `pred`. */
    waitFor(pred, { since = 0, timeout = 5000 } = {}) {
      const hit = messages.slice(since).find(pred);
      if (hit) return Promise.resolve(hit);
      return new Promise((resolve, reject) => {
        const w = { pred, resolve };
        w.timer = setTimeout(() => {
          waiters.delete(w);
          reject(new Error(`mock: no matching message within ${timeout} ms`));
        }, timeout);
        waiters.add(w);
      });
    },
    /** Replaces the next P-frame sent to the session (default: every live session). */
    corruptNext(kind = 'p_garbage', id = null) {
      if (!CORRUPTIONS[kind]) throw new Error(`unknown corruption ${kind}`);
      for (const s of pick(id)) s.corrupt = kind;
    },
    /** Changes the screen: {t:'screen'} to everyone, then a KEY frame at the new size. */
    switchStream(name) {
      state.unsupported = null;
      state.stream = loadStream(name);
      state.cursorPos = [Math.min(state.cursorPos[0], state.stream.w - 1), Math.min(state.cursorPos[1], state.stream.h - 1)];
      for (const s of live()) {
        sendJson(s, { t: 'screen', w: state.stream.w, h: state.stream.h });
        s.forceIdr = true;
      }
    },
    /**
     * Gives the screen a size the encoder rejects, as the real server does over 36864 macroblocks:
     * every session, also each new one, gets one unsupported_size error and no video until
     * switchStream() changes the size.
     */
    unsupportedSize(msg = "cannot stream this screen: frame size 8192x4608 is over OpenH264's 36864 macroblocks (about 4096x2304)") {
      state.unsupported = msg;
      for (const s of live()) sendJson(s, { t: 'error', code: 'unsupported_size', msg });
    },
    /** New cursor shape for everyone; null hides the cursor. */
    setCursor(shape) {
      state.cursor = { serial: state.cursor.serial + 1, ...(shape || { w: 0, h: 0, xhot: 0, yhot: 0, rgba: Buffer.alloc(0) }) };
      broadcast(cursorShapeMsg());
    },
    moveCursor(x, y) {
      state.cursorPos = [x, y];
      broadcast(cursorPosMsg());
    },
    /** Stops all traffic to a session (no video, PONG or stats) without closing it. */
    freeze(id, on = true) {
      for (const s of pick(id)) s.frozen = on;
    },
    /** Closes each new session right after its welcome, before any video; null stops that. */
    closeOnWelcome(code) {
      state.closeOnWelcome = code;
    },
    /** Closes sessions from the server side; terminate() drops the TCP connection instead. */
    drop(id = null, code = 1001) {
      for (const s of pick(id)) {
        if (s.cut) end(s, code);
        else s.ws.close(code, 'dropped by mock');
      }
    },
    /**
     * Breaks the connection the way a dead network does: the client sees its socket close, but
     * the session stays live (holding control, if it did) until drop() ends it, as the real
     * server's idle close would.
     */
    cut(id = null) {
      for (const s of pick(id)) {
        s.cut = true;
        s.ws.terminate();
      }
    },
    terminate(id = null) {
      for (const s of pick(id)) s.ws.terminate();
    },
    async close() {
      for (const w of waiters) {
        clearTimeout(w.timer);
        w.resolve(null);
      }
      waiters.clear();
      for (const s of sessions.values()) {
        clearTimeout(s.helloTimer);
        clearTimeout(s.idrTimer);
        clearInterval(s.pumpTimer);
        clearInterval(s.statsTimer);
        s.ws.terminate();
      }
      await new Promise((r) => wss.close(r));
      server.closeAllConnections();
      await new Promise((r) => server.close(r));
      if (logFile) await new Promise((r) => logFile.end(r));
    },
  };
}

function parseArgs(argv) {
  const o = {};
  const flags = { '--no-auth': ['noAuth', true], '--no-loop': ['loop', false], '--quiet': ['quiet', true] };
  const values = {
    '--port': ['port', Number], '--host': ['host', String], '--token': ['token', String],
    '--view-token': ['viewToken', String], '--token-file': ['tokenFile', String], '--stream': ['stream', String],
    '--fps': ['fps', Number], '--max-msg-bytes': ['maxMsgBytes', Number], '--max-viewers': ['maxViewers', Number],
    '--window': ['window', Number], '--log': ['log', String],
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (flags[a]) o[flags[a][0]] = flags[a][1];
    else if (values[a] && i + 1 < argv.length) o[values[a][0]] = values[a][1](argv[++i]);
    else throw new Error(`unknown argument ${a}`);
  }
  return o;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const cli = { port: 6090, ...parseArgs(process.argv.slice(2)) };
  const mock = await startMockServer(cli);
  process.stderr.write(`tilt mock server on ${mock.url} (stream ${cli.stream || DEFAULTS.stream})\n`);
  const stop = async () => {
    await mock.close();
    process.exit(0);
  };
  process.on('SIGINT', stop);
  process.on('SIGTERM', stop);
}
