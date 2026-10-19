// The WebTransport (QUIC/UDP) fast path to /stream, behind the part of the WebSocket interface
// app.js and the tests use: readyState, send(), close(), and open, message and close events (as
// on* handlers or listeners). Wire format in docs/protocol.md section 7.
//
// It finds the server's UDP port and certificate at GET transport. If that says no, or the
// session cannot be set up and answered within TRY_MS (UDP blocked, a gateway in the way), it
// calls onfallback() and the page uses the WebSocket instead. Two failures to connect in a row
// are remembered for a day, so such pages do not pay the wait on every reconnect, while one
// slow handshake on a lossy link does not give up on UDP.

const VIDEO = 0x01;
const VIDEO_HEADER = 18;
const KIND_TEXT = 0;
const KIND_BINARY = 1;
const KIND_CLOSE = 2;
const ADVERT_TIMEOUT_MS = 2000;
// From opening the session to the server's first message (welcome, or a refusal).
const TRY_MS = 1500;
// {"n": failures in a row, "at": ms of the last}.
const FAILED_KEY = 'tilt.wtFailed';
const GIVE_UP_AFTER = 2;
const FAILED_FOR_MS = 24 * 3600 * 1000;

const encoder = new TextEncoder();
const decoder = new TextDecoder();
// This server said it has no fast path: not asked again until the page reloads.
let noAdvert = false;

function load(key) {
  try { return localStorage.getItem(key); } catch { return null; }
}

function store(key, value) {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch { /* unavailable */ }
}

/**
 * Whether to try WebTransport: the browser has it, the server may offer it, and it did not fail
 * to connect lately. `mode` is the page's #transport option: 'ws' never, 'udp' despite a
 * remembered failure.
 */
export function wtCandidate(mode) {
  if (typeof WebTransport !== 'function' || mode === 'ws' || noAdvert) return false;
  if (mode === 'udp') return true;
  const f = failures();
  return !(f.n >= GIVE_UP_AFTER && Date.now() - f.at < FAILED_FOR_MS);
}

function failures() {
  try {
    const f = JSON.parse(load(FAILED_KEY));
    if (Number.isFinite(f.n) && Number.isFinite(f.at)) return f;
  } catch { /* none, or not ours */ }
  return { n: 0, at: 0 };
}

const nextSeq = (s) => (s >= 0xffffffff ? 1 : s + 1);
// Distance from a to b going forward through the u32 seq space (which skips 0).
const ahead = (a, b) => (b - a + 0x100000000) % 0x100000000;

function hexBytes(hex) {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
  return out;
}

function hostForUrl(host) {
  // Chrome dials `localhost` over QUIC on ::1 only, which fails where IPv6 is off (some
  // microVMs) or tilt listens on IPv4; 127.0.0.1 reaches both a 0.0.0.0 and a [::] listener.
  if (host === 'localhost') return '127.0.0.1';
  return host.includes(':') && !host.startsWith('[') ? `[${host}]` : host;
}

export class WtSocket extends EventTarget {
  /** `pageUrl` resolves `transport`, as `stream` is resolved for the WebSocket. */
  constructor(pageUrl) {
    super();
    this.readyState = 0;
    this.onopen = this.onmessage = this.onclose = this.onfallback = this.onskip = null;
    this.wt = null;
    this.writer = null;
    // Video waits until the first control message (welcome) is out: the page acks only then.
    this.heard = false;
    this.expect = 1; // the next VIDEO seq to deliver
    this.early = new Map(); // seq -> VIDEO message that came before `expect`'s
    this.lost = 0; // video streams that failed before giving their seq
    this.timer = 0;
    this.start(pageUrl);
  }

  async start(pageUrl) {
    let advert = null;
    // Only an answer counts: an unreachable server (restarting, say) may offer it again.
    let answered = false;
    try {
      const ctl = new AbortController();
      const t = setTimeout(() => ctl.abort(), ADVERT_TIMEOUT_MS);
      const r = await fetch(new URL('transport', pageUrl), { cache: 'no-store', signal: ctl.signal });
      answered = r.ok || r.status === 404;
      if (r.ok) advert = (await r.json()).wt ?? null;
      clearTimeout(t);
    } catch { /* unreachable, or not JSON */ }
    if (this.readyState === 3) return;
    if (!advert || !advert.port || !Array.isArray(advert.hashes)) {
      if (answered) noAdvert = true;
      this.fallback(false);
      return;
    }
    const host = hostForUrl(advert.host || new URL(pageUrl).hostname);
    const url = `https://${host}:${advert.port}/stream?ticket=${encodeURIComponent(advert.ticket || '')}`;
    this.timer = setTimeout(() => this.fallback(true), TRY_MS);
    try {
      this.wt = new WebTransport(url, {
        serverCertificateHashes: advert.hashes.map((h) => ({ algorithm: 'sha-256', value: hexBytes(h) })),
        congestionControl: 'low-latency',
      });
      this.wt.closed.then(() => this.ended(1006), () => this.ended(1006));
      await this.wt.ready;
      const bidi = await this.wt.createBidirectionalStream();
      if (this.readyState === 3) return;
      this.writer = bidi.writable.getWriter();
      this.readyState = 1;
      this.readControl(bidi.readable);
      this.readVideo();
      this.emit(new Event('open'));
    } catch {
      this.fallback(true);
    }
  }

  /** Gives up before the server answered: the page should use the WebSocket. */
  fallback(remember) {
    if (this.readyState === 3 || this.heard) return;
    if (remember) store(FAILED_KEY, JSON.stringify({ n: failures().n + 1, at: Date.now() }));
    this.shut();
    if (this.onfallback) this.onfallback();
  }

  send(data) {
    if (this.readyState !== 1) return;
    let kind = KIND_BINARY;
    let bytes;
    if (typeof data === 'string') {
      kind = KIND_TEXT;
      bytes = encoder.encode(data);
    } else if (data instanceof ArrayBuffer) {
      bytes = new Uint8Array(data);
    } else {
      bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    }
    const out = new Uint8Array(5 + bytes.length);
    new DataView(out.buffer).setUint32(0, bytes.length + 1, true);
    out[4] = kind;
    out.set(bytes, 5);
    this.writer.write(out).catch(() => this.ended(1006));
  }

  /** Like WebSocket.close(): a close event follows, unless the session had already ended. */
  close() {
    if (this.readyState === 3) return;
    this.shut();
    setTimeout(() => this.emit(new CloseEvent('close', { code: 1005 })));
  }

  shut() {
    clearTimeout(this.timer);
    this.readyState = 3;
    if (this.wt) {
      try { this.wt.close(); } catch { /* closed already */ }
    }
  }

  /** The session ended: a close event once, after the server's first message. */
  ended(code) {
    if (this.readyState === 3) return;
    if (!this.heard) {
      this.fallback(true);
      return;
    }
    this.shut();
    this.emit(new CloseEvent('close', { code }));
  }

  emit(event) {
    const handler = this[`on${event.type}`];
    if (handler) handler.call(this, event);
    this.dispatchEvent(event);
  }

  deliver(data) {
    if (this.readyState === 3) return;
    this.emit(new MessageEvent('message', { data }));
  }

  async readControl(readable) {
    const reader = readable.getReader();
    let buf = new Uint8Array(0);
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        const joined = new Uint8Array(buf.length + value.length);
        joined.set(buf);
        joined.set(value, buf.length);
        buf = joined;
        let at = 0;
        while (buf.length - at >= 4) {
          const len = new DataView(buf.buffer, buf.byteOffset + at, 4).getUint32(0, true);
          if (len < 1 || buf.length - at < 4 + len) break;
          this.onFrame(buf[at + 4], buf.slice(at + 5, at + 4 + len));
          if (this.readyState === 3) return;
          at += 4 + len;
        }
        buf = buf.slice(at);
      }
    } catch { /* the session went away */ }
    this.ended(1006);
  }

  onFrame(kind, payload) {
    if (kind === KIND_CLOSE) {
      const code = payload.length >= 2 ? payload[0] | (payload[1] << 8) : 1005;
      this.heard = true;
      this.ended(code);
      return;
    }
    if (kind === KIND_TEXT) this.deliver(decoder.decode(payload));
    else if (kind === KIND_BINARY) this.deliver(payload.buffer);
    else return;
    if (!this.heard) {
      this.heard = true;
      clearTimeout(this.timer);
      store(FAILED_KEY, null);
      this.drainVideo();
    }
  }

  async readVideo() {
    const streams = this.wt.incomingUnidirectionalStreams.getReader();
    try {
      for (;;) {
        const { value, done } = await streams.read();
        if (done) return;
        this.readFrame(value);
      }
    } catch { /* the session went away */ }
  }

  async readFrame(stream) {
    let buf;
    try {
      buf = await new Response(stream).arrayBuffer();
    } catch {
      buf = null;
    }
    if (this.readyState === 3) return;
    if (!buf || buf.byteLength < VIDEO_HEADER || new Uint8Array(buf, 0, 1)[0] !== VIDEO) {
      this.lost++;
    } else {
      const seq = new DataView(buf).getUint32(2, true);
      // Older than what was delivered: a duplicate, never expected from the server.
      if (ahead(this.expect, seq) < 0x80000000) this.early.set(seq, buf);
    }
    this.drainVideo();
  }

  /** Delivers VIDEO in seq order. A frame that never came is skipped (and acked by the page). */
  drainVideo() {
    if (!this.heard) return;
    for (;;) {
      const buf = this.early.get(this.expect);
      if (buf) {
        this.early.delete(this.expect);
        this.expect = nextSeq(this.expect);
        this.deliver(buf);
        if (this.readyState === 3) return;
        continue;
      }
      if (!this.lost || !this.early.size) return;
      // A stream failed before its seq was known: assume it held the gap up to the oldest
      // frame waiting, skip that, and let the page ack it and ask for a keyframe.
      this.lost--;
      let oldest = null;
      for (const s of this.early.keys()) {
        if (oldest === null || ahead(this.expect, s) < ahead(this.expect, oldest)) oldest = s;
      }
      const last = oldest === 1 ? 0xffffffff : oldest - 1;
      this.expect = oldest;
      if (this.onskip) this.onskip(last);
    }
  }
}
