// The stream: tilt's protocol over WebTransport (QUIC, UDP), behind the small socket interface
// app.js and the tests use: readyState, send(), close(), and open, message and close events (as
// on* handlers or listeners). Wire format in docs/protocol.md.
//
// It finds the server's UDP host and port, certificate and ticket at GET transport, next to the
// page.
// Anything that keeps the session from coming up ends it with a close event (1006), and the
// page reconnects as after any lost connection.

const VIDEO = 0x01;
const VIDEO_HEADER = 18;
const SHARD = 0x05;
const SHARD_HEADER = 10;
const MAX_RECOVERY_FRAME = 128 * 1024;
const KIND_TEXT = 0;
const KIND_BINARY = 1;
const KIND_CLOSE = 2;
const ADVERT_TIMEOUT_MS = 2000;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

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
  /** `pageUrl` resolves `transport`, so a path prefix in front of tilt is kept. */
  constructor(pageUrl) {
    super();
    this.readyState = 0;
    this.onopen = this.onmessage = this.onclose = this.onskip = null;
    this.wt = null;
    this.writer = null;
    // Video waits until the first control message (welcome) is out: the page acks only then.
    this.heard = false;
    this.expect = 1; // the next VIDEO seq to deliver
    this.early = new Map(); // seq -> { buf, receivedAt }, before ordered delivery
    this.shards = new Map(); // at most the server's 32 in-flight frames
    this.recovered = 0;
    this.protected = 0;
    this.lost = 0; // video streams that failed before giving their seq
    this.start(pageUrl);
  }

  async start(pageUrl) {
    let advert = null;
    try {
      const r = await fetch(new URL('transport', pageUrl), {
        cache: 'no-store',
        signal: AbortSignal.timeout(ADVERT_TIMEOUT_MS),
      });
      if (r.ok) advert = (await r.json()).wt ?? null;
    } catch { /* unreachable, or not JSON */ }
    if (this.readyState === 3) return;
    if (!advert || !advert.port || !Array.isArray(advert.hashes)) {
      this.ended(1006);
      return;
    }
    // A sandbox's public UDP listener can be on another host than the page (--udp-public).
    const host = hostForUrl(typeof advert.host === 'string' && advert.host ? advert.host : new URL(pageUrl).hostname);
    const url = `https://${host}:${advert.port}/stream?ticket=${encodeURIComponent(advert.ticket || '')}`;
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
      this.ended(1006);
    }
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
    this.readyState = 3;
    this.early.clear();
    this.shards.clear();
    if (this.wt) {
      try { this.wt.close(); } catch { /* closed already */ }
    }
  }

  /** The session ended (or never came up): one close event. */
  ended(code) {
    if (this.readyState === 3) return;
    this.shut();
    this.emit(new CloseEvent('close', { code }));
  }

  emit(event) {
    const handler = this[`on${event.type}`];
    if (handler) handler.call(this, event);
    this.dispatchEvent(event);
  }

  deliver(data, receivedAt = performance.now()) {
    if (this.readyState === 3) return;
    this.emit(Object.assign(new MessageEvent('message', { data }), { receivedAt }));
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
    let kind;
    try {
      buf = await new Response(stream.pipeThrough(new TransformStream({
        transform(chunk, controller) {
          if (kind === undefined && chunk.length) kind = chunk[0];
          controller.enqueue(chunk);
        },
      }))).arrayBuffer();
    } catch {
      // A reset shard is not a missing VIDEO seq: other shards may already have repaired
      // it. Reconnect rather than skipping an unrelated future frame or waiting forever.
      if (kind === SHARD || this.protected) {
        this.ended(1006);
        return;
      }
      buf = null;
    }
    if (this.readyState === 3) return;
    if (buf && new Uint8Array(buf)[0] === SHARD) {
      this.readShard(buf);
      return;
    }
    if (!buf || buf.byteLength < VIDEO_HEADER || new Uint8Array(buf, 0, 1)[0] !== VIDEO) {
      this.lost++;
    } else {
      this.acceptVideo(buf);
    }
    this.drainVideo();
  }

  acceptVideo(buf) {
    const seq = new DataView(buf).getUint32(2, true);
    if (ahead(this.expect, seq) >= 0x80000000 || this.early.has(seq)) return;
    this.early.set(seq, { buf, receivedAt: performance.now() });
    this.shards.delete(seq);
  }

  /** Any four of five shards reconstruct a VIDEO, without waiting for retransmission. */
  readShard(buf) {
    if (buf.byteLength < SHARD_HEADER) return this.ended(1006);
    const d = new DataView(buf);
    const seq = d.getUint32(1, true);
    const len = d.getUint32(5, true);
    const index = d.getUint8(9);
    const size = Math.ceil(len / 4);
    if (!seq || len < VIDEO_HEADER || len > MAX_RECOVERY_FRAME || index > 4 ||
        buf.byteLength !== SHARD_HEADER + size) return this.ended(1006);
    if (ahead(this.expect, seq) >= 0x80000000 || this.early.has(seq)) return;
    let group = this.shards.get(seq);
    if (!group) {
      // Do not silently drop a frame at the limit: that would leave cumulative ACKs stuck.
      if (this.shards.size >= 32) return this.ended(1006);
      group = { len, parts: new Map() };
      this.shards.set(seq, group);
      this.protected++;
    }
    if (group.len !== len) return this.ended(1006);
    if (group.parts.has(index)) return;
    group.parts.set(index, new Uint8Array(buf, SHARD_HEADER));
    if (group.parts.size < 4) return;
    const parts = group.parts;
    let repaired = false;
    for (let i = 0; i < 4; i++) {
      if (parts.has(i)) continue;
      const missing = new Uint8Array(parts.get(4));
      for (const [j, bytes] of parts) {
        if (j === 4) continue;
        for (let k = 0; k < size; k++) missing[k] ^= bytes[k];
      }
      parts.set(i, missing);
      repaired = true;
    }
    const video = new Uint8Array(len);
    for (let i = 0; i < 4; i++) video.set(parts.get(i).subarray(0, Math.min(size, len - i * size)), i * size);
    // The reconstructed header must agree with its envelope before it can advance ACKs.
    if (video[0] !== VIDEO || new DataView(video.buffer).getUint32(2, true) !== seq) return this.ended(1006);
    if (repaired) this.recovered++;
    this.acceptVideo(video.buffer);
    this.drainVideo();
  }

  /** Delivers VIDEO in seq order. A frame that never came is skipped (and acked by the page). */
  drainVideo() {
    if (!this.heard) return;
    for (;;) {
      const frame = this.early.get(this.expect);
      if (frame) {
        this.early.delete(this.expect);
        this.expect = nextSeq(this.expect);
        this.deliver(frame.buf, frame.receivedAt);
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
      for (const seq of this.shards.keys()) {
        if (ahead(this.expect, seq) >= 0x80000000) this.shards.delete(seq);
      }
      if (this.onskip) this.onskip(last);
    }
  }
}
