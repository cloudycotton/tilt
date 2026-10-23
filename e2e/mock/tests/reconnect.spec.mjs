import {
  COLORS, command, controlling, expect, expectColor, from, open, pollState, sample, sessionOf, setVisibility,
  startMockServer, state, stats, test, waitDrawn,
} from './fixtures.mjs';

test('reconnects after tilt.ws.close() and takes control again', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const first = await controlling(page);
  await page.waitForFunction(() => window.tilt.stats.framesDecoded >= 10);
  // Counted as the socket closes: frames decoded before that (a slow engine has several in its
  // decoder) are the old session's.
  const before = await page.evaluate(() => {
    window.tilt.ws.close();
    return { ...window.tilt.stats };
  });
  await pollState(page, 'message').toBe('Reconnecting…');
  expect((await state(page)).status).toBe('reconnecting');
  // The last frame stays on screen meanwhile.
  expectColor(await sample(page, 192, 192), COLORS[0]);
  await page.waitForFunction((n) => window.tilt.stats.framesDecoded > n + 5, before.framesDecoded, { timeout: 5000 });
  const second = await page.evaluate(() => window.tilt.stats.session);
  expect(second).not.toBe(first);
  await pollState(page, 'status').toBe('live');
  expect((await state(page)).message).toBe('');
  await page.waitForFunction(() => window.tilt.stats.control);
  expect(mock.holder()).toBe(second);
  // ACKs in the new session restart at seq 1 and stay in order.
  expect(mock.violations).toEqual([]);
  expect((await stats(page)).decodeErrors).toBe(0);
});

test('after a reconnect, control stays with a viewer who took it meanwhile', async ({ page, browser, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const a1 = await controlling(page);
  const b = await browser.newPage();
  await open(b, mock, 'token=devtoken');
  const bid = await sessionOf(b);
  // The server is full until B holds control, so this page cannot come back before that.
  mock.options.maxViewers = 1;
  const since = mock.messages.length;
  mock.drop(a1);
  await mock.waitFor((m) => m.type === 'rejected' && m.code === 'busy', { since });
  await b.evaluate(() => window.tilt.takeControl());
  await b.waitForFunction(() => window.tilt.stats.control);
  mock.options.maxViewers = Infinity;
  const a2 = (await mock.waitFor((m) => m.type === 'welcomed' && m.session !== a1 && m.session !== bid,
    { since, timeout: 10_000 })).session;
  await page.waitForTimeout(1000);
  expect(from(mock, a2, 'control')).toEqual([]);
  expect(mock.holder()).toBe(bid);
  expect(await b.evaluate(() => window.tilt.stats.control)).toBe(true);
  // Once B lets go, this page (which held control when it dropped) takes it back.
  await command(b, { type: 'tilt:control', take: false });
  await page.waitForFunction(() => window.tilt.stats.control);
  expect(mock.holder()).toBe(a2);
  await b.close();
});

test('after a reconnect, takes control back at once from its own lost session', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const a1 = await controlling(page);
  const since = mock.messages.length;
  // A dead network: this page's socket closes, but the server still sees a1, holding control.
  mock.cut(a1);
  const welcomed = await mock.waitFor((m) => m.type === 'welcomed' && m.session !== a1, { since });
  const take = await mock.waitFor((m) => m.session === welcomed.session && m.type === 'control', { since, timeout: 2000 });
  expect(take.take).toBe(true);
  // Well before the server's idle close of a1 would have freed control.
  expect(take.at - welcomed.at).toBeLessThan(1000);
  await page.waitForFunction(() => window.tilt.stats.control);
  expect(mock.holder()).toBe(welcomed.session);
  expect(mock.messages.slice(since).filter((m) => m.session === a1 && m.type === 'released'))
    .toEqual([expect.objectContaining({ reason: `taken by ${welcomed.session}` })]);
});

test('a hidden page does not reconnect; shown again, it reconnects and takes control back', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const a1 = await controlling(page);
  let since = mock.messages.length;
  await setVisibility(page, 'hidden');
  await mock.waitFor((m) => m.session === a1 && m.type === 'video' && m.on === false, { since });
  since = mock.messages.length;
  // The server's idle close of a page whose timers the browser throttles.
  mock.drop(a1, 1000);
  await page.waitForTimeout(3000);
  expect(mock.messages.slice(since).filter((m) => m.type === 'hello')).toEqual([]);
  expect(await state(page)).toMatchObject({ status: 'reconnecting', message: 'Reconnecting…' });
  await setVisibility(page, 'visible');
  await page.waitForFunction(() => window.tilt.stats.control, null, { timeout: 5000 });
  expect(mock.holder()).toBe(await sessionOf(page));
});

test('a session that dies right after its welcome is retried with backoff', async ({ page, mock }) => {
  await open(page, mock);
  const first = await sessionOf(page);
  await waitDrawn(page, 1);
  // Every new session is closed before its first frame, as by a proxy that cuts the first large
  // message.
  mock.closeOnWelcome(1011);
  const hellos = () => mock.messages.filter((m) => m.type === 'hello').length;
  const before = hellos();
  mock.drop(first, 1011);
  await page.waitForTimeout(6000);
  // 250 ms doubling up to 5 s, jittered: 4 or 5 attempts in 6 s (restarting at 250 ms made 22).
  const attempts = hellos() - before;
  expect(attempts).toBeGreaterThanOrEqual(3);
  expect(attempts).toBeLessThanOrEqual(5);
  // A session that decodes frames shows the link works: its loss is retried at the shortest delay.
  mock.closeOnWelcome(null);
  const ok = (await mock.waitFor((m) => m.type === 'welcomed', { since: mock.messages.length, timeout: 10_000 })).session;
  await page.waitForFunction((id) => window.tilt.stats.session === id, ok);
  await waitDrawn(page, (await stats(page)).framesDrawn + 5);
  const since = mock.messages.length;
  const dropped = Date.now();
  mock.drop(ok);
  await mock.waitFor((m) => m.type === 'welcomed', { since });
  expect(Date.now() - dropped).toBeLessThan(2000);
});

test('reconnects after the server drops the connection', async ({ page, mock }) => {
  await open(page, mock);
  await waitDrawn(page, 10);
  const first = await sessionOf(page);
  mock.terminate(first);
  await page.waitForFunction((id) => window.tilt.stats.session !== id && window.tilt.stats.session !== '', first, { timeout: 5000 });
  const drawn = (await stats(page)).framesDrawn;
  await waitDrawn(page, drawn + 5);
  expect(mock.violations).toEqual([]);
});

test('declares a silent link dead after 5 s and reconnects', async ({ page, mock }) => {
  await open(page, mock);
  await waitDrawn(page, 10);
  const first = await sessionOf(page);
  const frozenAt = Date.now();
  mock.freeze(first);
  await mock.waitFor((m) => m.type === 'welcomed' && m.session !== first, { timeout: 15_000 });
  const elapsed = Date.now() - frozenAt;
  expect(elapsed).toBeGreaterThan(4500);
  expect(elapsed).toBeLessThan(9000);
  // The client closed the dead socket itself.
  expect(mock.session(first).closed).toBe(true);
  const drawn = (await stats(page)).framesDrawn;
  await waitDrawn(page, drawn + 5);
});

test('keeps retrying while the server is down', async ({ page }) => {
  const mock = await startMockServer({ quiet: true });
  await open(page, mock);
  await waitDrawn(page, 5);
  const { port } = mock;
  await mock.close();
  await pollState(page, 'message').toBe('Reconnecting…');
  await page.waitForTimeout(2000);
  const again = await startMockServer({ quiet: true, port });
  try {
    const drawn = (await stats(page)).framesDrawn;
    await waitDrawn(page, drawn + 5, 15_000);
    await pollState(page, 'status').toBe('live');
    expect(again.messages.filter((m) => m.type === 'hello')).toHaveLength(1);
  } finally {
    await again.close();
  }
});

for (const [name, reconnect] of [
  ['tilt.reconnect()', (page) => page.evaluate(() => window.tilt.reconnect())],
  ['the tilt:reconnect command', (page) => command(page, { type: 'tilt:reconnect' })],
]) test(`${name} opens a fresh session`, async ({ page, mock }) => {
  await open(page, mock);
  await waitDrawn(page, 5);
  const first = await sessionOf(page);
  await reconnect(page);
  await page.waitForFunction((id) => window.tilt.stats.session !== '' && window.tilt.stats.session !== id, first);
  const drawn = (await stats(page)).framesDrawn;
  await waitDrawn(page, drawn + 5);
  await expect.poll(() => mock.session(first).closed).toBe(true);
  expect(mock.violations).toEqual([]);
});

test('welcomed but before its first frame, the page is still connecting, not reconnecting', async ({ page, mock }) => {
  // A decoder that never outputs: the session is welcomed, but no frame is ever drawn.
  await page.addInitScript(() => {
    window.VideoDecoder = class {
      constructor() { this.state = 'unconfigured'; this.decodeQueueSize = 0; }
      configure() { this.state = 'configured'; }
      decode() {}
      flush() { return new Promise(() => {}); }
      close() { this.state = 'closed'; }
      static isConfigSupported(config) { return Promise.resolve({ supported: true, config }); }
    };
  });
  await open(page, mock);
  await page.waitForFunction(() => window.tilt.stats.session !== '');
  expect(await state(page)).toMatchObject({ status: 'connecting', message: 'Connecting…' });
  expect(await page.evaluate(() => window.tilt.stats.framesDrawn)).toBe(0);
});
