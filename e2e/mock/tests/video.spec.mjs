import {
  BACKGROUND, COLORS, canvasRect, controlling, expect, expectColor, fixtureCodec, from, open, sample, sessionOf,
  setVisibility, stats, test, waitDrawn,
} from './fixtures.mjs';

test('decodes and draws the stream at its native size', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&stats=1');
  await waitDrawn(page, 30);
  const s = await stats(page);
  const canvas = await page.evaluate(() => [document.getElementById('screen').width, document.getElementById('screen').height]);
  expect(s.decodeErrors).toBe(0);
  expect(canvas).toEqual([1280, 720]);
  expect([s.width, s.height]).toEqual([1280, 720]);
  expect(s.codec).toBe(fixtureCodec('high_1280x720'));
  expect(s.framing).toBe('annexb');
  expect(s.keyframes).toBeGreaterThanOrEqual(1);
  expect(s.framesDecoded).toBeGreaterThanOrEqual(s.framesDrawn);
  expect(s.lastSeq).toBeGreaterThanOrEqual(s.lastDrawnSeq);
  expect(s.bytes).toBeGreaterThan(18_000);
  expect(s.role).toBe('control');
  expectColor(await sample(page, 192, 192), COLORS[0]);
  expectColor(await sample(page, 900, 100), BACKGROUND);
  expect(await sample(page, 5000, 5)).toEqual([0, 0, 0, 0]);
  await expect(page.locator('#overlay')).toBeHidden();
  await expect(page.locator('#stats')).toContainText(fixtureCodec('high_1280x720'));
  // Fit mode: 1280x720 in the 1280x720 test viewport fills it.
  expect(await canvasRect(page)).toEqual({ left: 0, top: 0, width: 1280, height: 720 });
  // The token left the address bar for sessionStorage.
  expect(page.url()).not.toContain('token');
  expect(mock.violations).toEqual([]);
});

test('acks every frame once, in order, within the credit window', async ({ page, mock }) => {
  await open(page, mock);
  await waitDrawn(page, 60);
  const id = await sessionOf(page);
  const acks = from(mock, id, 'ACK');
  expect(acks.length).toBeGreaterThanOrEqual(55);
  // ACK(n) is cumulative and the client sends one per seq, so the seqs run 1, 2, 3, ...
  expect(acks.map((a) => a.seq)).toEqual(acks.map((_, i) => i + 1));
  // decode_ms is receipt-to-output, so it fits between the mock sending the frame and the ACK
  // arriving, give or take the browser's coarse clock (WebKit: 1 ms) and the rounding. Not an
  // absolute bound: software decoding on a busy CI runner takes 100 ms and more.
  const sentAt = new Map(mock.sent.filter((f) => f.session === id).map((f) => [f.seq, f.at]));
  for (const a of acks) expect(a.decodeMs).toBeLessThanOrEqual(a.at - sentAt.get(a.seq) + 3);
  const sess = mock.session(id);
  expect(sess.maxInflight).toBeLessThanOrEqual(mock.options.window);
  expect(mock.violations).toEqual([]);
});

test.describe('with 4 KiB messages', () => {
  test.use({ mockOptions: { maxMsgBytes: 4096 } });

  test('reassembles fragmented frames', async ({ page, mock }) => {
    await open(page, mock);
    await waitDrawn(page, 40);
    const id = await sessionOf(page);
    const sent = mock.sent.filter((f) => f.session === id);
    // Each IDR is about 18-23 KB, so it arrives in 5-6 fragments.
    expect(sent[0].key).toBe(true);
    expect(sent[0].fragments).toBeGreaterThanOrEqual(5);
    const s = await stats(page);
    expect(s.decodeErrors).toBe(0);
    expect(s.keyframes).toBeGreaterThanOrEqual(1);
    expectColor(await sample(page, 192, 192), COLORS[0]);
    const acks = from(mock, id, 'ACK').map((a) => a.seq);
    expect(acks).toEqual(acks.map((_, i) => i + 1));
    expect(mock.violations).toEqual([]);
  });
});

test.describe('Baseline profile', () => {
  test.use({ mockOptions: { stream: 'baseline_1280x720' } });

  test('decodes a CAVLC Baseline stream', async ({ page, mock }) => {
    await open(page, mock);
    await waitDrawn(page, 30);
    const s = await stats(page);
    expect(s.codec).toBe(fixtureCodec('baseline_1280x720'));
    expect(s.codec.startsWith('avc1.42')).toBe(true);
    expect(s.decodeErrors).toBe(0);
    expectColor(await sample(page, 192, 192), COLORS[0]);
  });
});

test('follows a screen size change', async ({ page, mock }) => {
  await open(page, mock);
  await waitDrawn(page, 10);
  mock.switchStream('high_1024x768');
  await page.waitForFunction(() => window.tilt.stats.width === 1024 && window.tilt.stats.height === 768);
  const drawn = (await stats(page)).framesDrawn;
  await waitDrawn(page, drawn + 15);
  const s = await stats(page);
  expect(s.decodeErrors).toBe(0);
  expect(s.codec).toBe(fixtureCodec('high_1024x768'));
  expect(await page.evaluate(() => [document.getElementById('screen').width, document.getElementById('screen').height])).toEqual([1024, 768]);
  // Fit keeps the 4:3 aspect inside the 16:9 viewport: 960x720, centred.
  expect(await canvasRect(page)).toEqual({ left: 160, top: 0, width: 960, height: 720 });
  expectColor(await sample(page, 192, 192), COLORS[0]);
  expect(mock.violations).toEqual([]);
});

test('a screen size the server cannot encode shows a notice until a keyframe at a size that works', async ({ page, mock }) => {
  await page.addInitScript(() => {
    const Real = VideoDecoder;
    // Late outputs: frames sent before the error are drawn after it.
    window.VideoDecoder = class extends Real {
      constructor(init) {
        super({ output: (frame) => setTimeout(() => init.output(frame), 200), error: init.error });
      }
    };
  });
  await open(page, mock);
  await waitDrawn(page, 10);
  const id = await sessionOf(page);
  const msg = 'cannot stream this screen: frame size 8192x4608 is too large';
  mock.unsupportedSize(msg);
  const last = mock.session(id).seq;
  const overlay = page.locator('#overlay');
  await expect(page.locator('#overlay-msg')).toHaveText('The remote screen cannot be streamed at its current size.');
  await expect(page.locator('#overlay-detail')).toHaveText(msg);
  // The frames still in the decoder are drawn, and the notice stays: it is not a toast.
  await page.waitForFunction((seq) => window.tilt.stats.lastDrawnSeq >= seq, last);
  await page.waitForTimeout(300);
  await expect(overlay).toBeVisible();
  await expect(page.locator('#toast')).toBeHidden();
  // Hiding and showing the page turns video off and on: still no picture, so the notice stays.
  await setVisibility(page, 'hidden');
  await setVisibility(page, 'visible');
  await mock.waitFor((m) => m.session === id && m.type === 'video' && m.on === true);
  await page.waitForTimeout(500);
  await expect(overlay).toBeVisible();
  mock.switchStream('high_1024x768');
  await page.waitForFunction(() => window.tilt.stats.width === 1024);
  await expect(overlay).toBeHidden();
  expect(await sessionOf(page)).toBe(id);
  expect(mock.violations).toEqual([]);
});

for (const kind of ['p_garbage', 'empty_slice']) {
  test(`requests an IDR after a corrupt frame (${kind}) and recovers`, async ({ page, mock }) => {
    await open(page, mock);
    await waitDrawn(page, 20);
    const id = await sessionOf(page);
    const since = mock.messages.length;
    mock.corruptNext(kind, id);
    const idr = await mock.waitFor((m) => m.session === id && m.type === 'idr', { since });
    const bad = mock.sent.find((f) => f.session === id && f.corrupt === kind);
    expect(bad).toBeTruthy();
    expect(idr.at).toBeGreaterThan(bad.at);
    await page.waitForFunction(() => window.tilt.stats.decodeErrors >= 1);
    await expect.poll(() => mock.sent.some((f) => f.session === id && f.key && f.at > idr.at)).toBe(true);
    const keySeq = mock.sent.find((f) => f.session === id && f.key && f.at > idr.at).seq;
    await page.waitForFunction((seq) => window.tilt.stats.lastDrawnSeq > seq + 10, keySeq);
    const errors = (await stats(page)).decodeErrors;
    await page.waitForTimeout(1000);
    const s = await stats(page);
    expect(s.decodeErrors).toBe(errors);
    expect(errors).toBe(1);
    expectColor(await sample(page, 192, 192), COLORS[0]);
    // The corrupt frame and anything skipped while waiting for the IDR were still acked, in order.
    const acks = from(mock, id, 'ACK').map((a) => a.seq);
    expect(acks).toEqual(acks.map((_, i) => i + 1));
    expect(mock.violations).toEqual([]);
  });
}

test('gives up on a decoder that fails every frame, after trying a software decoder', async ({ page, mock }) => {
  await page.addInitScript(() => {
    const Real = VideoDecoder;
    window.__configs = [];
    window.__failing = false;
    // A broken hardware path: once failing, every chunk errors.
    window.VideoDecoder = class extends Real {
      constructor(init) {
        super(init);
        this.fail = init.error;
      }

      configure(config) {
        window.__configs.push(config.hardwareAcceleration);
        return super.configure(config);
      }

      decode(chunk) {
        if (!window.__failing) return super.decode(chunk);
        queueMicrotask(() => this.fail(new DOMException('Decoding error.', 'EncodingError')));
        return undefined;
      }
    };
  });
  await open(page, mock);
  await waitDrawn(page, 20);
  const id = await sessionOf(page);
  // Chrome on Android has no software H.264 decoder; the client asks before switching.
  const software = await page.evaluate(async () => (await VideoDecoder.isConfigSupported({
    codec: window.tilt.stats.codec, codedWidth: 1280, codedHeight: 720, hardwareAcceleration: 'prefer-software',
  })).supported);
  const since = mock.messages.length;
  await page.evaluate(() => { window.__failing = true; });
  await expect(page.locator('#overlay-msg')).toHaveText('This browser cannot decode the desktop stream.', { timeout: 15_000 });
  const s = await stats(page);
  expect(s.decodeErrors).toBe(4);
  expect((await page.evaluate(() => window.__configs)).includes('prefer-software')).toBe(software);
  // IDRs were asked for at growing intervals, then the stream was turned off.
  expect(from(mock, id, 'idr', since).length).toBeLessThanOrEqual(3);
  expect(from(mock, id, 'video', since).map((m) => m.on)).toEqual([false]);
  // Retry starts over with the default decoder.
  await page.evaluate(() => { window.__failing = false; });
  await page.getByRole('button', { name: 'Retry' }).click();
  await waitDrawn(page, s.framesDrawn + 10);
  await expect(page.locator('#overlay')).toBeHidden();
  expect(await page.evaluate(() => window.__configs.at(-1))).toBe('no-preference');
});

test('turns video off for a stream it cannot decode, also after reconnecting', async ({ page, mock }) => {
  await page.addInitScript(() => {
    VideoDecoder.prototype.configure = function configure() {
      throw new DOMException('Unsupported codec.', 'NotSupportedError');
    };
  });
  await open(page, mock);
  const message = page.locator('#overlay-msg');
  await expect(message).toHaveText('This browser cannot decode the desktop stream.');
  const id = await sessionOf(page);
  await mock.waitFor((m) => m.session === id && m.type === 'video' && m.on === false);
  await page.waitForTimeout(300); // frames already on the way
  const seq = mock.session(id).seq;
  // Showing the page again does not turn it back on.
  await setVisibility(page, 'hidden');
  await setVisibility(page, 'visible');
  await page.waitForTimeout(700);
  expect(mock.session(id).seq).toBe(seq);
  expect(from(mock, id, 'video').map((m) => m.on)).toEqual([false]);
  // An automatic reconnect keeps the verdict: video goes off at once and the message stays up.
  const since = mock.messages.length;
  mock.drop(id);
  const next = (await mock.waitFor((m) => m.type === 'welcomed' && m.session !== id, { since })).session;
  await mock.waitFor((m) => m.session === next && m.type === 'video' && m.on === false, { since });
  await expect(message).toHaveText('This browser cannot decode the desktop stream.');
  await expect(page.getByRole('button', { name: 'Retry' })).toBeVisible();
});

test.describe('a stream that goes idle', () => {
  test.use({ mockOptions: { loop: false } });

  test('every frame is output and acked, with no decoder stall', async ({ page, mock }) => {
    await open(page, mock);
    const id = await sessionOf(page);
    await expect.poll(() => mock.session(id).seq, { timeout: 10_000 }).toBe(90);
    await page.waitForTimeout(2500);
    const s = await stats(page);
    expect(mock.session(id).lastAck).toBe(90);
    expect(s.lastDrawnSeq).toBe(90);
    expect(s.decoderStalls).toBe(0);
    expect(s.decodeErrors).toBe(0);
  });

  test('a decoder that holds back its newest frame is flushed', async ({ page, mock }) => {
    await page.addInitScript(() => {
      const Real = VideoDecoder;
      // Like Android's MediaCodec outside low-latency mode: an output waits for the next input.
      window.VideoDecoder = class extends Real {
        constructor(init) {
          let held = null;
          super({
            output: (frame) => {
              const prev = held;
              held = frame;
              if (prev) init.output(prev);
            },
            error: init.error,
          });
          this.release = () => {
            const frame = held;
            held = null;
            if (frame) init.output(frame);
          };
        }

        flush() {
          return super.flush().then(() => this.release());
        }
      };
    });
    await open(page, mock, 'token=devtoken&control=1');
    const id = await controlling(page);
    for (let burst = 1; burst <= 2; burst++) {
      const since = mock.messages.length;
      if (burst === 2) await page.keyboard.press('a'); // new content: the next marker's segment
      await expect.poll(() => mock.session(id).seq, { timeout: 10_000 }).toBe(90 * burst);
      // The held frame comes out (and is acked) once the watchdog sees the decoder idle.
      await expect.poll(() => mock.session(id).lastAck, { timeout: 5000 }).toBe(90 * burst);
      expect((await stats(page)).decoderStalls).toBe(burst);
      expect(from(mock, id, 'idr', since).length).toBeLessThanOrEqual(1);
    }
    const s = await stats(page);
    expect(s.lastDrawnSeq).toBe(180);
    expect(s.decodeErrors).toBe(0);
  });
});

test('replaces a decoder that stops producing output', async ({ page, mock }) => {
  await page.addInitScript(() => {
    const Real = VideoDecoder;
    window.__decoders = [];
    window.VideoDecoder = class extends Real {
      constructor(init) {
        const self = { silent: false };
        super({
          output: (frame) => {
            if (self.silent) frame.close(); // lost inside a broken decoder
            else init.output(frame);
          },
          error: init.error,
        });
        window.__decoders.push(self);
      }
    };
  });
  await open(page, mock);
  await waitDrawn(page, 20);
  const id = await sessionOf(page);
  const since = mock.messages.length;
  await page.evaluate(() => { window.__decoders.at(-1).silent = true; });
  await mock.waitFor((m) => m.session === id && m.type === 'idr', { since });
  await waitDrawn(page, (await stats(page)).framesDrawn + 10);
  const s = await stats(page);
  expect(s.decoderStalls).toBe(1);
  expect(s.decodeErrors).toBe(1);
  expect(await page.evaluate(() => window.__decoders.length)).toBe(2);
  const acks = from(mock, id, 'ACK').map((a) => a.seq);
  expect(acks).toEqual(acks.map((_, i) => i + 1));
  expect(mock.violations).toEqual([]);
});

test('decodes with avcC framing when asked (#framing=avcc)', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&framing=avcc');
  await waitDrawn(page, 30);
  const s = await stats(page);
  expect(s.framing).toBe('avcc');
  expect(s.decodeErrors).toBe(0);
  expectColor(await sample(page, 192, 192), COLORS[0]);
});

test('falls back to avcC when Annex B is reported unsupported', async ({ page, mock }) => {
  await page.addInitScript(() => {
    const real = VideoDecoder.isConfigSupported.bind(VideoDecoder);
    VideoDecoder.isConfigSupported = (c) => (c.description ? real(c) : Promise.resolve({ supported: false, config: c }));
  });
  await open(page, mock);
  await page.waitForFunction(() => window.tilt.stats.framing === 'avcc');
  const drawn = (await stats(page)).framesDrawn;
  await waitDrawn(page, drawn + 20);
  expect((await stats(page)).decodeErrors).toBe(0);
  expectColor(await sample(page, 192, 192), COLORS[0]);
  expect(from(mock, await sessionOf(page), 'idr').length).toBeGreaterThanOrEqual(1);
});

test('falls back to avcC when configure() rejects Annex B', async ({ page, mock }) => {
  await page.addInitScript(() => {
    const real = VideoDecoder.prototype.configure;
    VideoDecoder.prototype.configure = function configure(c) {
      if (!c.description) throw new TypeError('description required');
      return real.call(this, c);
    };
  });
  await open(page, mock);
  await waitDrawn(page, 20);
  const s = await stats(page);
  expect(s.framing).toBe('avcc');
  expect(s.decodeErrors).toBe(0);
  expectColor(await sample(page, 192, 192), COLORS[0]);
});

test('records probe colour changes after input', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1&probe=192,192');
  await page.waitForFunction(() => window.tilt.stats.control && window.tilt.probeEvents.length >= 1);
  expectColor((await page.evaluate(() => window.tilt.probeEvents[0])).rgb, COLORS[0]);
  for (let i = 1; i <= 4; i++) {
    // The way tools/e2e measures latency: the first event after the key that shows the next colour.
    await page.keyboard.down('a');
    const sentAt = await page.evaluate(() => window.tilt.lastInputAt);
    await page.keyboard.up('a');
    const ev = await page.waitForFunction(([want, since]) => window.tilt.probeEvents.find((e) => e.at >= since
      && Math.max(...e.rgb.map((v, k) => Math.abs(v - want[k]))) <= 16), [COLORS[i % 4], sentAt]).then((h) => h.jsonValue());
    expect(ev.at).toBeGreaterThan(sentAt);
    expect(ev.seq).toBeGreaterThan(0);
  }
  // Consecutive events always differ: unchanged frames are not recorded.
  const events = await page.evaluate(() => window.tilt.probeEvents);
  for (let k = 1; k < events.length; k++) expect(events[k].rgb).not.toEqual(events[k - 1].rgb);
});

test('pauses video while hidden and resumes with a keyframe', async ({ page, mock }) => {
  await open(page, mock);
  await waitDrawn(page, 10);
  const id = await sessionOf(page);
  let since = mock.messages.length;
  await setVisibility(page, 'hidden');
  await mock.waitFor((m) => m.session === id && m.type === 'video' && m.on === false, { since });
  const seq = mock.session(id).seq;
  await page.waitForTimeout(500);
  expect(mock.session(id).seq).toBe(seq);
  since = mock.messages.length;
  await setVisibility(page, 'visible');
  const resume = await mock.waitFor((m) => m.session === id && m.type === 'video' && m.on === true, { since });
  await expect.poll(() => mock.sent.some((f) => f.session === id && f.key && f.at > resume.at)).toBe(true);
  const drawn = (await stats(page)).framesDrawn;
  await waitDrawn(page, drawn + 10);
  expect((await stats(page)).decodeErrors).toBe(0);
  expect(mock.violations).toEqual([]);
});
