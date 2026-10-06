import fs from 'node:fs';
import {
  collect, controlling, expect, from, INPUT_TYPES, keyLog, open, sessionOf, startMockServer, test, waitDrawn,
} from './fixtures.mjs';

const controlButton = (page) => page.getByRole('button', { name: 'Control', exact: true });

test('the view role cannot send input or take control', async ({ page, mock }) => {
  await open(page, mock, 'token=viewtoken&control=1');
  await page.waitForFunction(() => window.tilt.stats.role === 'view' && window.tilt.stats.framesDrawn > 0);
  const id = await sessionOf(page);
  await expect(controlButton(page)).toBeDisabled();
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
  await expect(page.locator('#toast')).toContainText('view-only');
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
  await open(a, mock, 'token=devtoken&control=1');
  const ida = await controlling(a);
  await open(b, mock, 'token=devtoken');
  const idb = await sessionOf(b);
  await expect(controlButton(b)).toBeEnabled();
  await expect(controlButton(b)).toHaveAttribute('aria-pressed', 'false');
  await controlButton(b).click();
  await b.waitForFunction(() => window.tilt.stats.control);
  await a.waitForFunction(() => !window.tilt.stats.control);
  await expect(controlButton(b)).toHaveAttribute('aria-pressed', 'true');
  await expect(a.locator('#toast')).toContainText('Another viewer took control');
  expect(mock.holder()).toBe(idb);

  let since = mock.messages.length;
  await a.keyboard.press('a');
  await b.keyboard.press('b');
  expect(keyLog(await collect(mock, idb, 'KEY', since, 2))).toEqual(['down 62', 'up 62']);
  expect(from(mock, ida, INPUT_TYPES, since)).toEqual([]);

  since = mock.messages.length;
  await controlButton(b).click();
  await mock.waitFor((m) => m.session === idb && m.type === 'control' && m.take === false, { since });
  await b.waitForFunction(() => !window.tilt.stats.control);
  await expect(controlButton(b)).toHaveAttribute('aria-pressed', 'false');
  expect(mock.holder()).toBe(null);
  await a.close();
  await b.close();
});

test('a wrong token stops retrying and asks for a token', async ({ page, mock }) => {
  await open(page, mock, 'token=wrong');
  await expect(page.locator('#overlay-msg')).toContainText('Access denied');
  await expect(page.locator('#token-form')).toBeVisible();
  expect(mock.messages.filter((m) => m.type === 'closed').map((m) => m.code)).toEqual([4001]);
  await page.waitForTimeout(1500);
  expect(mock.messages.filter((m) => m.type === 'hello')).toHaveLength(1);
  await page.fill('#token-input', 'devtoken');
  await page.press('#token-input', 'Enter');
  await waitDrawn(page, 5);
  await expect(page.locator('#overlay')).toBeHidden();
  expect(await page.evaluate(() => window.tilt.stats.role)).toBe('control');
});

test.describe('one viewer at most', () => {
  test.use({ mockOptions: { maxViewers: 1 } });

  test('a busy server is retried until a slot frees up', async ({ browser, mock }) => {
    const a = await browser.newPage();
    const b = await browser.newPage();
    await open(a, mock);
    await waitDrawn(a, 1);
    await open(b, mock);
    await expect(b.locator('#overlay-detail')).toContainText('maximum number of viewers');
    await expect.poll(() => mock.messages.filter((m) => m.type === 'rejected' && m.code === 'busy').length).toBeGreaterThanOrEqual(2);
    await expect.poll(() => mock.messages.filter((m) => m.type === 'closed' && m.code === 4002).length).toBeGreaterThanOrEqual(2);
    await a.close();
    await waitDrawn(b, 5);
    await expect(b.locator('#overlay')).toBeHidden();
    await b.close();
  });
});

test('not_ready is retried until the token file appears', async ({ page }, info) => {
  const tokenFile = info.outputPath('token');
  const mock = await startMockServer({ quiet: true, tokenFile });
  try {
    await open(page, mock);
    await expect(page.locator('#overlay-detail')).toContainText('not ready');
    await expect.poll(() => mock.messages.filter((m) => m.type === 'rejected' && m.code === 'not_ready').length).toBeGreaterThanOrEqual(2);
    fs.writeFileSync(tokenFile, 'devtoken\n');
    await waitDrawn(page, 5, 15_000);
    await expect(page.locator('#overlay')).toBeHidden();
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
