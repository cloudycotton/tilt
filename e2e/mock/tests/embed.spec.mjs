// The headless page (docs/guide.md, "Embedding"): the screen alone, remote=1 for a viewer that
// controls by default, and the postMessage bridge through which a host page draws every control.
import { collect, command, controlling, expect, from, inputLog, open, pollState, sessionOf, state, test, waitDrawn } from './fixtures.mjs';

/** Serves a host page next to tilt's (a secure context, as WebCodecs in the frame needs) framing it with `hash`. */
async function hostPage(page, mock, hash) {
  const hostUrl = new URL('host.html', mock.url).href;
  await page.route(hostUrl, (route) => route.fulfill({ contentType: 'text/html', body: `
    <iframe id="f" src="${mock.url}#${hash}" style="width:640px;height:360px;border:0"></iframe>
    <script>
      window.msgs = [];
      addEventListener('message', (e) => msgs.push(e.data));
      window.send = (m) => document.getElementById('f').contentWindow.postMessage(m, '*');
    </script>` }));
  await page.goto(hostUrl);
  const frame = page.frame({ url: (u) => u.href.startsWith(mock.url) && u.href !== hostUrl });
  return {
    frame,
    /** Posts each message to the frame, in one go. */
    send: (...list) => page.evaluate((msgs) => msgs.forEach((m) => window.send(m)), list),
    /** The last `tilt:state` the host page was posted. */
    last: () => page.evaluate(() => window.msgs.filter((m) => m.type === 'tilt:state').at(-1) ?? null),
    of: (type) => page.evaluate((t) => window.msgs.filter((m) => m.type === t), type),
  };
}

test('the page holds the screen alone, on a transparent or #bg= background', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken');
  await waitDrawn(page, 1);
  const layout = (p = page) => p.evaluate(() => ({
    body: [...document.body.children].map((e) => e.id || e.tagName.toLowerCase()),
    stage: [...document.getElementById('stage').children].map((e) => e.id),
    bg: getComputedStyle(document.body).backgroundColor,
  }));
  expect(await layout()).toEqual({ body: ['viewport', 'kbd', 'script'], stage: ['screen', 'cursor'], bg: 'rgba(0, 0, 0, 0)' });
  // The keyboard field is there but cannot be seen.
  expect(await page.evaluate(() => getComputedStyle(document.getElementById('kbd')).opacity)).toBe('0');
  expect(await page.evaluate(() => Object.keys(localStorage))).toEqual([]);

  const tinted = await page.context().newPage();
  await open(tinted, mock, 'token=devtoken&bg=102030');
  expect((await layout(tinted)).bg).toBe('rgb(16, 32, 48)');
  await tinted.close();
});

test('without remote=1 the page is view only until asked: a press does not take control', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken');
  await waitDrawn(page, 1);
  const id = await sessionOf(page);
  expect(await state(page)).toMatchObject({ status: 'live', role: 'control', control: false, remote: false });
  await page.locator('#screen').click();
  await page.waitForTimeout(400);
  expect((await state(page)).control).toBe(false);
  expect(mock.holder()).toBe(null);
  expect(from(mock, id, ['BUTTON', 'MOVE'])).toEqual([]);
});

for (const flag of ['remote=1', 'remote=yes']) {
  test(`${flag} takes control on opening, and a press takes it back from another viewer`, async ({ page, mock }) => {
    await open(page, mock, `token=devtoken&${flag}`);
    const id = await controlling(page);
    expect(mock.holder()).toBe(id);
    expect((await state(page)).remote).toBe(true);

    const other = await page.context().newPage();
    await open(other, mock, 'token=devtoken&remote=1');
    const otherId = await controlling(other);
    expect(mock.holder()).toBe(otherId);
    await pollState(page, 'control').toBe(false);
    expect((await state(page)).held).toBe(true);

    // The first press only takes control back.
    await page.locator('#screen').click();
    await pollState(page, 'control').toBe(true);
    expect(mock.holder()).toBe(id);
    await other.close();
  });
}

test('in a frame, it posts its state to the host page and obeys only the host page', async ({ page, mock }) => {
  const host = await hostPage(page, mock, 'token=devtoken');
  await expect.poll(async () => (await host.last())?.status, { timeout: 10_000 }).toBe('live');
  const live = await host.last();
  expect(live).toMatchObject({
    status: 'live', code: '', message: '', control: false, held: false, role: 'control', remote: false,
    screen: { w: 1280, h: 720 }, modifiers: { ctrl: 0, alt: 0, super: 0 }, fullscreen: false,
  });
  expect(live.session).toMatch(/^s/);
  const id = live.session;

  // A message from the frame's own window is not the host page's.
  await host.frame.evaluate(() => window.postMessage({ type: 'tilt:control', take: true }, '*'));
  await page.waitForTimeout(300);
  expect(mock.holder()).toBe(null);
  expect((await host.last()).control).toBe(false);

  await host.send({ type: 'tilt:control', take: true });
  await expect.poll(async () => (await host.last()).control).toBe(true);
  expect(mock.holder()).toBe(id);

  // Typing and extra keys.
  let since = mock.messages.length;
  await host.send({ type: 'tilt:type', text: 'héllo' });
  await host.send({ type: 'tilt:key', keysym: 0xff0d, down: true }, { type: 'tilt:key', keysym: 0xff0d, down: false });
  expect(inputLog(await collect(mock, id, ['TEXT', 'KEY'], since, 3))).toEqual(['TEXT héllo', 'down ff0d', 'up ff0d']);

  // A sticky modifier: off -> next key only -> locked -> off.
  since = mock.messages.length;
  await host.send({ type: 'tilt:modifier', name: 'ctrl' });
  await expect.poll(async () => (await host.last()).modifiers.ctrl).toBe(1);
  await host.send({ type: 'tilt:key', keysym: 0x63, down: true }, { type: 'tilt:key', keysym: 0x63, down: false });
  await expect.poll(async () => (await host.last()).modifiers.ctrl).toBe(0);
  await host.send({ type: 'tilt:modifier', name: 'alt' });
  await host.send({ type: 'tilt:modifier', name: 'alt' });
  await expect.poll(async () => (await host.last()).modifiers.alt).toBe(2);
  await host.send({ type: 'tilt:key', keysym: 0x78, down: true }, { type: 'tilt:key', keysym: 0x78, down: false });
  await page.waitForTimeout(150);
  expect((await host.last()).modifiers.alt).toBe(2);
  await host.send({ type: 'tilt:modifier', name: 'alt' });
  await expect.poll(async () => (await host.last()).modifiers.alt).toBe(0);
  expect(inputLog(await collect(mock, id, 'KEY', since, 8))).toEqual([
    'down ffe3', 'down 63', 'up 63', 'up ffe3',
    'down ffe9', 'down 78', 'up 78', 'up ffe9',
  ]);

  await host.send({ type: 'tilt:control', take: false });
  await expect.poll(async () => (await host.last()).control).toBe(false);
  expect(mock.holder()).toBe(null);
  // Without control, typing sends nothing.
  since = mock.messages.length;
  await host.send({ type: 'tilt:type', text: 'nope' });
  await page.waitForTimeout(300);
  expect(from(mock, id, ['TEXT', 'KEY'], since)).toEqual([]);

  // Remote on: takes control, and a press on the screen takes it back from another viewer, who
  // taking it is a notice for the host page.
  await host.send({ type: 'tilt:remote', on: true });
  await expect.poll(async () => (await host.last())?.control).toBe(true);
  expect((await host.last()).remote).toBe(true);
  const other = await page.context().newPage();
  await open(other, mock, 'token=devtoken&remote=1');
  const otherId = await controlling(other);
  await expect.poll(async () => (await host.last()).control).toBe(false);
  expect(mock.holder()).toBe(otherId);
  expect(await host.of('tilt:notice')).toContainEqual({ type: 'tilt:notice', text: 'Another viewer took control.' });
  await page.frameLocator('#f').locator('#screen').click();
  await expect.poll(async () => (await host.last()).control).toBe(true);
  expect(mock.holder()).toBe(id);
  await other.close();

  await host.send({ type: 'tilt:remote', on: false });
  await expect.poll(async () => (await host.last()).remote).toBe(false);

  // Stats, once a second while asked for.
  await host.send({ type: 'tilt:stats', on: true });
  await expect.poll(async () => (await host.of('tilt:stats')).length, { timeout: 5000 }).toBeGreaterThanOrEqual(2);
  const st = (await host.of('tilt:stats')).at(-1);
  expect(st.client).toMatchObject({ width: 1280, height: 720 });
  expect(st.client.codec).toMatch(/^avc1\./);
  expect('server' in st).toBe(true);
  await host.send({ type: 'tilt:stats', on: false });
  await page.waitForTimeout(200);
  const n = (await host.of('tilt:stats')).length;
  await page.waitForTimeout(1500);
  expect((await host.of('tilt:stats')).length).toBe(n);

  await host.send({ type: 'tilt:background', color: 'rgb(10, 20, 30)' });
  await expect.poll(() => host.frame.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe('rgb(10, 20, 30)');
});

test('in a frame, a wrong token is an auth error until the host page sends a token', async ({ page, mock }) => {
  const host = await hostPage(page, mock, 'token=wrong');
  await expect.poll(async () => (await host.last())?.status, { timeout: 10_000 }).toBe('error');
  expect(await host.last()).toMatchObject({ code: 'auth', message: 'Access denied: the token is missing or wrong.', control: false });
  await host.send({ type: 'tilt:token', token: 'devtoken' });
  await expect.poll(async () => (await host.last()).status, { timeout: 10_000 }).toBe('live');
  expect(await host.last()).toMatchObject({ code: '', message: '', role: 'control', screen: { w: 1280, h: 720 } });
  expect(await host.frame.evaluate(() => sessionStorage.getItem('tilt.token'))).toBe('devtoken');
});

test('commands run directly through window.tilt.command', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken');
  await waitDrawn(page, 1);
  await command(page, { type: 'tilt:control', take: true });
  const id = await controlling(page);
  // A held key repeats, as a keyboard's does, until released.
  let since = mock.messages.length;
  await command(page, { type: 'tilt:key', keysym: 0xff08, down: true });
  await page.waitForTimeout(800);
  await command(page, { type: 'tilt:key', keysym: 0xff08, down: false });
  const held = inputLog(await collect(mock, id, 'KEY', since, 3));
  expect(held.at(-1)).toBe('up ff08');
  expect(held.slice(0, -1).every((k) => k === 'down ff08')).toBe(true);
  expect(held.length).toBeGreaterThanOrEqual(3);
  await command(page, { type: 'tilt:scale', mode: '1' });
  await expect(page.locator('#viewport')).toHaveClass(/scroll/);
  await command(page, { type: 'tilt:scale', mode: 'fit' });
  await expect(page.locator('#viewport')).not.toHaveClass(/scroll/);
  since = mock.messages.length;
  await command(page, { type: 'tilt:reconnect' });
  await expect.poll(async () => (await state(page)).session).not.toBe(id);
  await pollState(page, 'status').toBe('live');
  // Control held when the connection went is taken back.
  await pollState(page, 'control').toBe(true);
  expect(mock.messages.slice(since).some((m) => m.type === 'hello')).toBe(true);
});
