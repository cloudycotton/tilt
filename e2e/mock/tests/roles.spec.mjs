import fs from 'node:fs';
import {
  collect, command, controlling, expect, from, INPUT_TYPES, keyLog, open, pollState, sessionOf, startMockServer, state,
  test, waitDrawn,
} from './fixtures.mjs';

/**
 * Gives the page a host to post to before it loads (a stand-in window.opener, as for a page a host
 * opened) and returns the texts of the `tilt:notice` messages it got so far.
 */
async function hostNotices(page) {
  await page.addInitScript(() => {
    window.hostNotices = [];
    window.opener = { postMessage: (m) => { if (m && m.type === 'tilt:notice') window.hostNotices.push(m.text); } };
  });
  return () => page.evaluate(() => window.hostNotices);
}

/** What the old Control button showed: [enabled, pressed]. */
const controlButton = async (page) => {
  const s = await state(page);
  return [s.role === 'control' && s.status === 'live', s.control];
};

test('the view role cannot send input or take control', async ({ page, mock }) => {
  const notices = await hostNotices(page);
  await open(page, mock, 'token=viewtoken&control=1');
  await page.waitForFunction(() => window.tilt.stats.role === 'view' && window.tilt.stats.framesDrawn > 0);
  const id = await sessionOf(page);
  expect(await controlButton(page)).toEqual([false, false]);
  await page.mouse.move(300, 300);
  await page.mouse.click(300, 300);
  await page.mouse.click(300, 300, { button: 'right' });
  await page.mouse.wheel(0, 200);
  await page.keyboard.press('a');
  await page.keyboard.press('Enter');
  await page.keyboard.insertText('typed');
  await page.evaluate(() => window.dispatchEvent(new Event('blur')));
  const since = mock.messages.length;
  await page.evaluate(() => window.tilt.takeControl());
  await mock.waitFor((m) => m.session === id && m.type === 'control' && m.take === true, { since });
  await expect.poll(notices).toContainEqual(expect.stringContaining('view-only'));
  await page.waitForTimeout(500);
  expect(from(mock, id, INPUT_TYPES)).toEqual([]);
  expect(await page.evaluate(() => window.tilt.stats.control)).toBe(false);
  expect(mock.session(id).closed).toBe(false);
  expect(mock.holder()).toBe(null);
  expect(mock.marker()).toBe(0);
  // The session keeps streaming after the refusal.
  const drawn = await page.evaluate(() => window.tilt.stats.framesDrawn);
  await waitDrawn(page, drawn + 5);
});

test('control moves between sessions and can be released', async ({ browser, mock }) => {
  const a = await browser.newPage();
  const b = await browser.newPage();
  const notices = await hostNotices(a);
  await open(a, mock, 'token=devtoken&control=1');
  const ida = await controlling(a);
  await open(b, mock, 'token=devtoken');
  const idb = await sessionOf(b);
  await expect.poll(() => controlButton(b)).toEqual([true, false]);
  await command(b, { type: 'tilt:control', take: true });
  await b.waitForFunction(() => window.tilt.stats.control);
  await a.waitForFunction(() => !window.tilt.stats.control);
  expect(await controlButton(b)).toEqual([true, true]);
  await expect.poll(notices).toContainEqual(expect.stringContaining('Another viewer took control'));
  expect(mock.holder()).toBe(idb);

  let since = mock.messages.length;
  await a.keyboard.press('a');
  await b.keyboard.press('b');
  expect(keyLog(await collect(mock, idb, 'KEY', since, 2))).toEqual(['down 62', 'up 62']);
  expect(from(mock, ida, INPUT_TYPES, since)).toEqual([]);

  since = mock.messages.length;
  await command(b, { type: 'tilt:control', take: false });
  await mock.waitFor((m) => m.session === idb && m.type === 'control' && m.take === false, { since });
  await b.waitForFunction(() => !window.tilt.stats.control);
  expect(await controlButton(b)).toEqual([true, false]);
  expect(mock.holder()).toBe(null);
  await a.close();
  await b.close();
});

test('a wrong token stops retrying and asks for a token', async ({ page, mock }) => {
  await open(page, mock, 'token=wrong');
  await pollState(page, 'message').toContain('Access denied');
  // The host page is told to ask for a token.
  expect(await state(page)).toMatchObject({ status: 'error', code: 'auth' });
  expect(mock.messages.filter((m) => m.type === 'closed').map((m) => m.code)).toEqual([4001]);
  await page.waitForTimeout(1500);
  expect(mock.messages.filter((m) => m.type === 'hello')).toHaveLength(1);
  await command(page, { type: 'tilt:token', token: ' devtoken ' });
  await waitDrawn(page, 5);
  expect(await state(page)).toMatchObject({ status: 'live', message: '', detail: '', code: '' });
  expect(await page.evaluate(() => sessionStorage.getItem('tilt.token'))).toBe('devtoken');
  expect(await page.evaluate(() => window.tilt.stats.role)).toBe('control');
});

test.describe('an older server with a viewer limit', () => {
  test.use({ mockOptions: { maxViewers: 1 } });

  test('a busy server is retried until a slot frees up', async ({ browser, mock }) => {
    const a = await browser.newPage();
    const b = await browser.newPage();
    await open(a, mock);
    await waitDrawn(a, 1);
    await open(b, mock);
    await pollState(b, 'detail').toContain('maximum number of viewers');
    expect(await state(b)).toMatchObject({ status: 'connecting', message: 'Connecting…' });
    await expect.poll(() => mock.messages.filter((m) => m.type === 'rejected' && m.code === 'busy').length).toBeGreaterThanOrEqual(2);
    await expect.poll(() => mock.messages.filter((m) => m.type === 'closed' && m.code === 4002).length).toBeGreaterThanOrEqual(2);
    await a.close();
    await waitDrawn(b, 5);
    await pollState(b, 'status').toBe('live');
    expect(await state(b)).toMatchObject({ message: '', detail: '' });
    await b.close();
  });
});

test('not_ready is retried until the token file appears', async ({ page }, info) => {
  const tokenFile = info.outputPath('token');
  const mock = await startMockServer({ quiet: true, tokenFile });
  try {
    await open(page, mock);
    await pollState(page, 'detail').toContain('not ready');
    await expect.poll(() => mock.messages.filter((m) => m.type === 'rejected' && m.code === 'not_ready').length).toBeGreaterThanOrEqual(2);
    fs.writeFileSync(tokenFile, 'devtoken\n');
    await waitDrawn(page, 5, 15_000);
    await pollState(page, 'status').toBe('live');
  } finally {
    await mock.close();
  }
});

test.describe('no-auth mode', () => {
  test.use({ mockOptions: { noAuth: true } });

  test('connects without a token', async ({ page, mock }) => {
    await page.goto(mock.url);
    await waitDrawn(page, 5);
    expect(mock.messages.find((m) => m.type === 'hello').hasToken).toBe(false);
  });
});
