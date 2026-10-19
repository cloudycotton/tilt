// The client's WebTransport path (web/transport.js) against the mock, through a fake WebTransport
// that carries the session over the mock's WebSocket in the WebTransport framing
// (docs/protocol.md section 7): control messages on one bidirectional stream, each VIDEO message
// on a unidirectional stream of its own. The fake can deliver video streams out of order, fail
// one, or never connect, which a real QUIC path does only at random.
import { expect, from, open, sessionOf, stats, test, waitDrawn } from './fixtures.mjs';

const HASH = 'ab'.repeat(32);
const ADVERT = { wt: { port: 4433, hashes: [HASH], ticket: 't1' } };

/** Runs in the page: window.WebTransport bridged to `wsUrl`. */
function installFakeWebTransport({ wsUrl, mode }) {
  const log = { created: 0, url: null, hashes: null, lost: 0 };
  window.fakeWt = log;
  const frame = (kind, bytes) => {
    const out = new Uint8Array(5 + bytes.length);
    new DataView(out.buffer).setUint32(0, bytes.length + 1, true);
    out[4] = kind;
    out.set(bytes, 5);
    return out;
  };
  const once = (bytes) => new ReadableStream({ start(c) { c.enqueue(bytes); c.close(); } });
  window.WebTransport = class {
    constructor(url, options) {
      log.created++;
      log.url = url;
      log.hashes = options.serverCertificateHashes.map((h) => [h.algorithm, [...h.value]]);
      let resolveReady;
      let resolveClosed;
      this.ready = new Promise((r) => { resolveReady = r; });
      this.closed = new Promise((r) => { resolveClosed = r; });
      let control;
      let uni;
      this.controlReadable = new ReadableStream({ start(c) { control = c; } });
      this.incomingUnidirectionalStreams = new ReadableStream({ start(c) { uni = c; } });
      // As if UDP were blocked: no handshake ever completes.
      if (mode === 'never') return;
      const ws = new WebSocket(wsUrl);
      ws.binaryType = 'arraybuffer';
      this.ws = ws;
      let videos = 0;
      let held = null;
      const flushHeld = () => {
        if (held) uni.enqueue(held);
        held = null;
      };
      ws.onopen = () => resolveReady();
      ws.onmessage = (ev) => {
        if (typeof ev.data === 'string') {
          control.enqueue(frame(0, new TextEncoder().encode(ev.data)));
          return;
        }
        const b = new Uint8Array(ev.data);
        if (b[0] !== 0x01) {
          control.enqueue(frame(1, b));
          return;
        }
        videos++;
        const key = (b[1] & 1) !== 0;
        if (mode === 'lose' && videos === 10 && !key) {
          log.lost++;
          uni.enqueue(new ReadableStream({ start(c) { c.error(new Error('stream reset')); } }));
          return;
        }
        if (mode === 'shuffle') {
          // Every other frame waits for the next one and arrives after it.
          if (held) {
            uni.enqueue(once(b));
            flushHeld();
          } else {
            held = once(b);
            setTimeout(flushHeld, 50);
          }
          return;
        }
        uni.enqueue(once(b));
      };
      ws.onclose = (ev) => {
        try {
          control.enqueue(frame(2, new Uint8Array([ev.code & 0xff, ev.code >> 8])));
          control.close();
        } catch { /* closed */ }
        resolveClosed({ closeCode: ev.code });
      };
    }

    async createBidirectionalStream() {
      const ws = this.ws;
      const writable = new WritableStream({
        write(chunk) {
          // transport.js writes one whole frame per write.
          const kind = chunk[4];
          const payload = chunk.slice(5);
          if (kind === 0) ws.send(new TextDecoder().decode(payload));
          else if (kind === 1) ws.send(payload);
        },
      });
      return { readable: this.controlReadable, writable };
    }

    close() {
      if (this.ws) this.ws.close();
    }
  };
}

async function openOverWt(page, mock, mode, hash = 'token=devtoken&stats=1') {
  await page.route('**/transport', (route) => route.fulfill({ json: ADVERT }));
  const wsUrl = mock.url.replace(/^http/, 'ws').replace(/\/?$/, '/stream');
  await page.addInitScript(installFakeWebTransport, { wsUrl, mode });
  await open(page, mock, hash);
}

test('streams over WebTransport, in seq order, whatever order the video streams come in', async ({ page, mock }) => {
  await openOverWt(page, mock, 'shuffle');
  await waitDrawn(page, 60);
  const s = await stats(page);
  expect(s.transport).toBe('udp');
  expect(s.decodeErrors).toBe(0);
  const wt = await page.evaluate(() => window.fakeWt);
  expect(wt.created).toBe(1);
  expect(wt.url).toBe(`https://127.0.0.1:4433/stream?ticket=t1`);
  expect(wt.hashes).toEqual([['sha-256', Array(32).fill(0xab)]]);
  // One ACK per seq, in order: the client put the frames back in order before decoding.
  const id = await sessionOf(page);
  const acks = from(mock, id, 'ACK').map((a) => a.seq);
  expect(acks.length).toBeGreaterThanOrEqual(55);
  expect(acks).toEqual(acks.map((_, i) => i + 1));
  await expect(page.locator('#stats')).toContainText('udp');
  expect(mock.violations).toEqual([]);
});

test('a video stream that fails is acked and skipped, and a keyframe resyncs', async ({ page, mock }) => {
  await openOverWt(page, mock, 'lose');
  await waitDrawn(page, 5);
  const id = await sessionOf(page);
  const idr = await mock.waitFor((m) => m.session === id && m.type === 'idr');
  await expect.poll(() => mock.sent.some((f) => f.session === id && f.key && f.at > idr.at)).toBe(true);
  const keySeq = mock.sent.find((f) => f.session === id && f.key && f.at > idr.at).seq;
  await page.waitForFunction((seq) => window.tilt.stats.lastDrawnSeq > seq + 10, keySeq);
  expect(await page.evaluate(() => window.fakeWt.lost)).toBe(1);
  expect((await stats(page)).decodeErrors).toBe(0);
  // The lost frame was acked like the rest: no seq is ever left unacked.
  const acks = from(mock, id, 'ACK').map((a) => a.seq);
  expect(acks).toEqual(acks.map((_, i) => i + 1));
  expect(mock.violations).toEqual([]);
});

test('falls back to the WebSocket when WebTransport does not connect; two failures in a row are remembered', async ({ page, mock }) => {
  const failed = () => page.evaluate(() => JSON.parse(localStorage.getItem('tilt.wtFailed'))?.n ?? 0);
  await openOverWt(page, mock, 'never');
  await waitDrawn(page, 5);
  expect((await stats(page)).transport).toBe('ws');
  expect(await failed()).toBe(1);
  // One failure may be a slow handshake: the next connection tries again.
  await page.evaluate(() => window.tilt.reconnect());
  await page.waitForFunction(() => window.fakeWt.created === 2);
  await page.waitForFunction(() => window.tilt.ws instanceof WebSocket && window.tilt.ws.readyState === 1);
  expect(await failed()).toBe(2);
  // Two in a row: straight to the WebSocket from now on.
  await page.evaluate(() => window.tilt.reconnect());
  await page.waitForFunction(() => window.tilt.ws instanceof WebSocket && window.tilt.ws.readyState === 1);
  await waitDrawn(page, 10);
  expect(await page.evaluate(() => window.fakeWt.created)).toBe(2);
  // #transport=udp tries it again despite them.
  await page.evaluate(() => { location.hash = 'transport=udp'; location.reload(); });
  await page.waitForFunction(() => window.fakeWt && window.fakeWt.created === 1 && window.tilt.stats.framesDrawn > 0);
  expect((await stats(page)).transport).toBe('ws');
});

test('a session that works clears the failures', async ({ page, mock }) => {
  await page.addInitScript(() => localStorage.setItem('tilt.wtFailed', JSON.stringify({ n: 1, at: Date.now() })));
  await openOverWt(page, mock, 'plain');
  await waitDrawn(page, 5);
  expect((await stats(page)).transport).toBe('udp');
  expect(await page.evaluate(() => localStorage.getItem('tilt.wtFailed'))).toBeNull();
});

test('a server without the fast path is not asked twice', async ({ page, mock }) => {
  const wsUrl = mock.url.replace(/^http/, 'ws').replace(/\/?$/, '/stream');
  await page.addInitScript(installFakeWebTransport, { wsUrl, mode: 'shuffle' });
  let asked = 0;
  await page.route('**/transport', (route) => {
    asked++;
    return route.continue();
  });
  await open(page, mock);
  await waitDrawn(page, 5);
  expect((await stats(page)).transport).toBe('ws');
  await page.evaluate(() => window.tilt.reconnect());
  await waitDrawn(page, 10);
  expect(asked).toBe(1);
  expect(await page.evaluate(() => window.fakeWt.created)).toBe(0);
});

test('closes from the server arrive as WebSocket close codes', async ({ page, mock }) => {
  await openOverWt(page, mock, 'plain', 'token=wrong');
  await expect(page.locator('#overlay-msg')).toHaveText('Access denied: the token is missing or wrong.');
  expect((await stats(page)).transport).toBe('udp');
});
