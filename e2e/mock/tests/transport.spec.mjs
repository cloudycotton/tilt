// The client's WebTransport handling (web/transport.js) through the fake WebTransport in
// fixtures.mjs, which can deliver video streams out of order, fail one, or never connect: what a
// real QUIC path does only at random.
import {
  expect, from, open, pollState, sessionOf, state, stats, test, waitDrawn,
} from './fixtures.mjs';

test.describe('recovery shards', () => {
  test.use({ wtMode: 'recovery' });

  test('decode with any single shard absent and ignore duplicates and late originals', async ({ page, mock }) => {
    await open(page, mock);
    await waitDrawn(page, 60);
    expect((await stats(page)).decodeErrors).toBe(0);
    const id = await sessionOf(page);
    const acks = from(mock, id, 'ACK').map(a => a.seq);
    expect(acks.length).toBeGreaterThanOrEqual(55);
    expect(acks).toEqual(acks.map((_, i) => i + 1));
    expect(from(mock, id, 'idr')).toEqual([]);
    expect(mock.violations).toEqual([]);
  });
});

test.describe('receiver reordering time', () => {
  test.use({ wtMode: 'reorder-delay' });

  test('is included in ACK processing time, not mistaken for network delay', async ({ page, mock }) => {
    await open(page, mock);
    await waitDrawn(page, 30);
    const id = await sessionOf(page);
    const acks = from(mock, id, 'ACK');
    // Frame 10 is on the wire 250 ms longer. Frame 11 arrived promptly but waited for 10.
    expect(acks.find(a => a.seq === 11).decodeMs).toBeGreaterThan(150);
    expect(acks.find(a => a.seq === 11).decodeMs - acks.find(a => a.seq === 10).decodeMs).toBeGreaterThan(100);
    expect(mock.violations).toEqual([]);
  });
});

test.describe('video streams out of order', () => {
  test.use({ wtMode: 'shuffle' });

  test('are decoded in seq order, and the session is set up from /transport', async ({ page, mock }) => {
    await open(page, mock);
    await waitDrawn(page, 60);
    expect((await stats(page)).decodeErrors).toBe(0);
    const wt = await page.evaluate(() => window.fakeWt);
    expect(wt.created).toBe(1);
    expect(wt.url).toBe('https://127.0.0.1:4433/stream?ticket=t1');
    expect(wt.hashes).toEqual([['sha-256', Array(32).fill(0xab)]]);
    // One ACK per seq, in order: the client put the frames back in order before decoding.
    const id = await sessionOf(page);
    const acks = from(mock, id, 'ACK').map((a) => a.seq);
    expect(acks.length).toBeGreaterThanOrEqual(55);
    expect(acks).toEqual(acks.map((_, i) => i + 1));
    expect(mock.violations).toEqual([]);
  });
});

test.describe('a UDP listener on another host', () => {
  test.use({ mockOptions: { udpHost: 'udp-3f2a.example.net', udpPort: 41234 } });

  test('is where the page dials the stream', async ({ page, mock }) => {
    await open(page, mock);
    await waitDrawn(page, 5);
    expect(await page.evaluate(() => window.fakeWt.url)).toBe('https://udp-3f2a.example.net:41234/stream?ticket=t1');
    expect(mock.violations).toEqual([]);
  });
});

test.describe('a video stream that fails', () => {
  test.use({ wtMode: 'lose' });

  test('is acked and skipped, and a keyframe resyncs', async ({ page, mock }) => {
    await open(page, mock);
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
});

test.describe('a session that never connects', () => {
  test.use({ wtMode: 'never' });

  test('reports connecting and tries again after the welcome timeout', async ({ page, mock }) => {
    await open(page, mock);
    expect(await state(page)).toMatchObject({ status: 'connecting', message: 'Connecting…' });
    // The page gives a session 10 s to say welcome (WELCOME_TIMEOUT_MS in app.js).
    await expect.poll(() => page.evaluate(() => window.fakeWt.created), { timeout: 20_000 })
      .toBeGreaterThanOrEqual(2);
    expect((await state(page)).status).toBe('connecting');
  });
});

test('closes from the server reach the page with their codes', async ({ page, mock }) => {
  await open(page, mock, 'token=wrong');
  await pollState(page, 'message').toBe('Access denied: the token is missing or wrong.');
  expect(await state(page)).toMatchObject({ status: 'error', code: 'auth' });
});

test('a browser without WebTransport is told so', async ({ page, mock }) => {
  await page.addInitScript(() => { delete window.WebTransport; });
  await open(page, mock);
  await pollState(page, 'message').toContain('needs WebTransport');
  expect(await state(page)).toMatchObject({ status: 'error', code: 'webtransport' });
});
