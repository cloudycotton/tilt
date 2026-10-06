import {
  COLORS, controlling, expect, expectColor, from, open, sample, sessionOf, setVisibility, startMockServer, stats, test,
  waitDrawn,
} from './fixtures.mjs';

test('reconnects after tilt.ws.close() and takes control again', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const first = await controlling(page);
  await page.waitForFunction(() => window.tilt.stats.framesDecoded >= 10);
  const before = await stats(page);
  await page.evaluate(() => window.tilt.ws.close());
  await expect(page.locator('#overlay-msg')).toHaveText('Reconnecting…');
  // The last frame stays on screen under the overlay.
  expectColor(await sample(page, 192, 192), COLORS[0]);
  await page.waitForFunction((n) => window.tilt.stats.framesDecoded > n + 5, before.framesDecoded, { timeout: 5000 });
  const second = await page.evaluate(() => window.tilt.stats.session);
  expect(second).not.toBe(first);
  await expect(page.locator('#overlay')).toBeHidden();
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
  mock.options.maxViewers = 4;
  const a2 = (await mock.waitFor((m) => m.type === 'welcomed' && m.session !== a1 && m.session !== bid,
    { since, timeout: 10_000 })).session;
  await page.waitForTimeout(1000);
  expect(from(mock, a2, 'control')).toEqual([]);
  expect(mock.holder()).toBe(bid);
  expect(await b.evaluate(() => window.tilt.stats.control)).toBe(true);
  // Once B lets go, this page (which held control when it dropped) takes it back.
  await b.evaluate(() => document.getElementById('btn-control').click());
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

test.describe('with a server that does not name the holder', () => {
  test.use({ mockOptions: { namesHolder: false } });

  test('after a reconnect, waits for its own lost session to let go of control', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    const a1 = await controlling(page);
    let since = mock.messages.length;
    mock.cut(a1);
    const a2 = (await mock.waitFor((m) => m.type === 'welcomed' && m.session !== a1, { since })).session;
    await page.waitForTimeout(1000);
    expect(from(mock, a2, 'control')).toEqual([]);
    expect(mock.holder()).toBe(a1);
    // The server idle-closes a1 and control is free, but the page is hidden: it waits until shown.
    await setVisibility(page, 'hidden');
    since = mock.messages.length;
    mock.drop(a1, 1000);
    await mock.waitFor((m) => m.session === a1 && m.type === 'closed', { since });
    await page.waitForTimeout(500);
    expect(from(mock, a2, 'control')).toEqual([]);
    expect(mock.holder()).toBe(null);
    await setVisibility(page, 'visible');
    await page.waitForFunction(() => window.tilt.stats.control);
    expect(mock.holder()).toBe(a2);
  });
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
  await expect(page.locator('#overlay-msg')).toHaveText('Reconnecting…');
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
  await expect(page.locator('#overlay-msg')).toHaveText('Reconnecting…');
  await page.waitForTimeout(2000);
  const again = await startMockServer({ quiet: true, port });
  try {
    const drawn = (await stats(page)).framesDrawn;
    await waitDrawn(page, drawn + 5, 15_000);
    await expect(page.locator('#overlay')).toBeHidden();
    expect(again.messages.filter((m) => m.type === 'hello')).toHaveLength(1);
  } finally {
    await again.close();
  }
});

test('tilt.reconnect() opens a fresh session', async ({ page, mock }) => {
  await open(page, mock);
  await waitDrawn(page, 5);
  const first = await sessionOf(page);
  await page.evaluate(() => window.tilt.reconnect());
  await page.waitForFunction((id) => window.tilt.stats.session !== '' && window.tilt.stats.session !== id, first);
  const drawn = (await stats(page)).framesDrawn;
  await waitDrawn(page, drawn + 5);
  await expect.poll(() => mock.session(first).closed).toBe(true);
  expect(mock.violations).toEqual([]);
});
