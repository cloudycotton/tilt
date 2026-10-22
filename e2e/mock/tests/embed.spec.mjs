// Embedded use (#embed=1, docs/guide.md "Embedding"): the screen alone, a press that takes control,
// and the postMessage bridge to the host page.
import { expect, open, sessionOf, test, waitDrawn } from './fixtures.mjs';

test('embedded, the page shows the screen alone and a press takes control', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&embed=1');
  await waitDrawn(page, 1);
  const id = await sessionOf(page);
  for (const sel of ['#toolbar', '#edge', '#menu', '#keys', '#toast']) await expect(page.locator(sel)).toBeHidden();
  await expect(page.locator('#overlay')).toBeHidden();
  expect(await page.evaluate(() => window.tilt.stats.control)).toBe(false);
  await page.locator('#screen').click();
  await page.waitForFunction(() => window.tilt.stats.control);
  expect(mock.holder()).toBe(id);
});

test('in a frame, it reports its state to the host page and takes its commands', async ({ page, mock }) => {
  // A host page next to tilt's (a secure context, as WebCodecs in the frame needs).
  const hostUrl = new URL('host.html', mock.url).href;
  await page.route(hostUrl, (route) => route.fulfill({ contentType: 'text/html', body: `
    <iframe id="f" src="${mock.url}#token=devtoken&embed=1" style="width:640px;height:360px;border:0"></iframe>
    <script>
      window.msgs = [];
      addEventListener('message', (e) => msgs.push(e.data));
      window.send = (m) => document.getElementById('f').contentWindow.postMessage(m, '*');
    </script>` }));
  await page.goto(hostUrl);
  const last = () => page.evaluate(() => window.msgs.filter((m) => m.type === 'tilt:state').at(-1) ?? null);
  await expect.poll(async () => (await last())?.status, { timeout: 10_000 }).toBe('live');
  const live = await last();
  expect(live).toMatchObject({ control: false, role: 'control', screen: { w: 1280, h: 720 } });
  expect(live.session).toMatch(/^s/);
  const frame = page.frames().find((f) => f !== page.mainFrame());

  await page.evaluate(() => window.send({ type: 'tilt:control', take: true }));
  await expect.poll(async () => (await last()).control).toBe(true);
  expect(mock.holder()).toBe(live.session);

  // Only the parent is obeyed.
  await frame.evaluate(() => window.postMessage({ type: 'tilt:control', take: false }, '*'));
  await page.waitForTimeout(300);
  expect(mock.holder()).toBe(live.session);

  await page.evaluate(() => window.send({ type: 'tilt:theme', theme: 'light' }));
  await expect.poll(() => frame.evaluate(() => document.documentElement.dataset.theme)).toBe('light');
  await page.evaluate(() => window.send({ type: 'tilt:theme', theme: 'system' }));
  await expect.poll(() => frame.evaluate(() => document.documentElement.dataset.theme ?? null)).toBe(null);

  await page.evaluate(() => window.send({ type: 'tilt:control', take: false }));
  await expect.poll(async () => (await last()).control).toBe(false);
  expect(mock.holder()).toBe(null);
});
