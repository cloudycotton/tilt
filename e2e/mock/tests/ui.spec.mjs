import { solidCursor } from '../server.mjs';
import {
  canvasRect, collect, command, controlling, expect, from, inputLog, isApple, keyEvents, keyLog, norm, open,
  pollState, state, test, waitDrawn,
} from './fixtures.mjs';

/**
 * Loads the client in a frame of a host page on the mock's origin (a secure context, as WebCodecs
 * in the frame needs) and collects what it posts. Returns the frame and a reader of the messages.
 */
async function framed(page, mock, hash) {
  const hostUrl = new URL('host.html', mock.url).href;
  await page.route(hostUrl, (route) => route.fulfill({ contentType: 'text/html', body: `
    <iframe id="f" src="${mock.url}#${hash}" style="width:640px;height:360px;border:0"></iframe>
    <script>
      window.msgs = [];
      addEventListener('message', (e) => msgs.push(e.data));
      window.send = (m) => document.getElementById('f').contentWindow.postMessage(m, '*');
    </script>` }));
  await page.goto(hostUrl);
  await expect.poll(() => page.frames().length).toBe(2);
  const frame = page.frames().find((f) => f !== page.mainFrame());
  await frame.waitForFunction(() => window.tilt !== undefined);
  const msgs = (type) => page.evaluate((t) => window.msgs.filter((m) => m.type === t), type);
  return { frame, msgs };
}

test('reports an unsupported browser without WebCodecs', async ({ page, mock }) => {
  await page.addInitScript(() => {
    delete window.VideoDecoder;
    delete window.EncodedVideoChunk;
  });
  await page.goto(`${mock.url}#token=devtoken`);
  await page.waitForFunction(() => window.tilt !== undefined);
  expect(await state(page)).toMatchObject({
    status: 'error',
    code: 'unsupported',
    message: 'This browser lacks WebCodecs H.264 decoding (needs Chrome/Edge 94+, Safari/iOS 16.4+, Android WebView 94+).',
  });
  await page.waitForTimeout(300);
  expect(mock.messages.filter((m) => m.type === 'open')).toHaveLength(0);
});

test('mentions the secure-context requirement on insecure origins', async ({ page, mock }) => {
  await page.addInitScript(() => {
    delete window.VideoDecoder;
    Object.defineProperty(window, 'isSecureContext', { value: false });
  });
  await page.goto(`${mock.url}#token=devtoken`);
  await page.waitForFunction(() => window.tilt !== undefined);
  const s = await state(page);
  expect(s.code).toBe('unsupported');
  expect(s.message).toContain('not a secure context');
  expect(s.message).toContain('https://');
});

test('stats=1 posts client and server numbers to the host page, until turned off', async ({ page, mock }) => {
  const { frame, msgs } = await framed(page, mock, 'token=devtoken&stats=1');
  await frame.waitForFunction(() => window.tilt.stats.framesDrawn > 30 && window.tilt.stats.rttMs !== null);
  // Once a second: wait for one sent after the numbers above were in.
  const since = (await msgs('tilt:stats')).length;
  await expect.poll(async () => (await msgs('tilt:stats')).length, { timeout: 5000 }).toBeGreaterThan(since);
  const { client, server } = (await msgs('tilt:stats')).at(-1);
  expect(client.fpsDecoded).toEqual(expect.any(Number));
  expect(client.fpsDrawn).toEqual(expect.any(Number));
  expect(client.fpsDrawn).toBeGreaterThan(0);
  expect(client.rttMs).toEqual(expect.any(Number));
  expect([client.width, client.height]).toEqual([1280, 720]);
  expect(client.codec).toMatch(/^avc1\./);
  expect(client.queue).toEqual(expect.any(Number));
  expect(client.decodeErrors).toBe(0);
  expect(server).not.toBeNull();
  expect(server.fps).toEqual(expect.any(Number));
  expect(server.viewers).toBe(1);
  expect(server.t).toBeUndefined();
  // The host page turns them off and on.
  await page.evaluate(() => window.send({ type: 'tilt:stats', on: false }));
  await page.waitForTimeout(300); // one already on its way
  const off = (await msgs('tilt:stats')).length;
  await page.waitForTimeout(2200);
  expect((await msgs('tilt:stats')).length).toBe(off);
  await page.evaluate(() => window.send({ type: 'tilt:stats', on: true }));
  await expect.poll(async () => (await msgs('tilt:stats')).length, { timeout: 3000 }).toBeGreaterThan(off);
});

test('without stats=1 the host page gets no statistics', async ({ page, mock }) => {
  const { frame, msgs } = await framed(page, mock, 'token=devtoken');
  await frame.waitForFunction(() => window.tilt.stats.framesDrawn > 0);
  await page.waitForTimeout(2200);
  expect(await msgs('tilt:stats')).toEqual([]);
  expect((await msgs('tilt:state')).at(-1).status).toBe('live');
});

test.describe('in an 800x600 window', () => {
  test.use({ viewport: { width: 800, height: 600 } });

  test('fits, switches to 1:1 and keeps input mapping right', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    const id = await controlling(page);
    expect(await canvasRect(page)).toEqual({ left: 0, top: 75, width: 800, height: 450 });
    const dpr = await page.evaluate(() => window.devicePixelRatio);
    await command(page, { type: 'tilt:scale', mode: '1' });
    await expect.poll(async () => (await canvasRect(page)).width).toBeCloseTo(1280 / dpr, 3);
    const r = await canvasRect(page);
    expect(r.width).toBeCloseTo(1280 / dpr, 3);
    expect(r.height).toBeCloseTo(720 / dpr, 3);
    if (1280 / dpr > 800) {
      // Desktop 1:1 scrolls natively.
      await page.evaluate(() => document.getElementById('viewport').scrollTo(200, 100));
      const scrolled = await canvasRect(page);
      expect(scrolled.left).toBeCloseTo(-200, 3);
      const since = mock.messages.length;
      await page.mouse.click(400, 300);
      const [down] = await collect(mock, id, 'BUTTON', since, 2);
      expect([down.x, down.y]).toEqual(norm(scrolled, 400, 300));
      // Content point (600, 400) of the 1280x720 screen, within the protocol's W-1 scaling.
      expect(Math.abs(down.px - 600)).toBeLessThanOrEqual(1);
      expect(Math.abs(down.py - 400)).toBeLessThanOrEqual(1);
    }
    await command(page, { type: 'tilt:scale', mode: 'fit' });
    await expect.poll(() => canvasRect(page)).toEqual({ left: 0, top: 75, width: 800, height: 450 });
    // Anything else is ignored.
    await command(page, { type: 'tilt:scale', mode: '2' });
    await page.waitForTimeout(100);
    expect(await canvasRect(page)).toEqual({ left: 0, top: 75, width: 800, height: 450 });
  });
});

test('clicks map onto the remote screen after every window resize', async ({ page, mock }) => {
  // The client keeps the canvas rect between pointer events until the layout changes.
  await page.setViewportSize({ width: 800, height: 600 });
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  for (const [w, h] of [[800, 600], [1000, 400], [500, 700], [1280, 720]]) {
    await page.setViewportSize({ width: w, height: h });
    // Fitted 16:9, centred: the canvas follows the viewport a frame later.
    const fit = Math.min(w / 1280, h / 720);
    await expect.poll(async () => (await canvasRect(page)).width).toBeCloseTo(1280 * fit, 1);
    const r = await canvasRect(page);
    // Whole pixels: WebKit rounds synthetic mouse positions.
    const [x, y] = [Math.round(r.left + r.width * 0.25), Math.round(r.top + r.height * 0.75)];
    await page.mouse.move(x - 5, y - 5);
    const since = mock.messages.length;
    await page.mouse.click(x, y);
    const [down] = await collect(mock, id, 'BUTTON', since, 2);
    expect([down.x, down.y], `${w}x${h}`).toEqual(norm(r, x, y));
  }
});

test.describe('with no token file yet', () => {
  test.use({ mockOptions: { tokenFile: '/nonexistent/tilt-token' } });

  test('reports the server not ready while connecting, with no endless animations', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken');
    // Retrying a server that is not ready yet: the "Connecting…" state.
    await pollState(page, 'detail').toContain('not ready');
    expect(await state(page)).toMatchObject({ status: 'connecting', message: 'Connecting…' });
    // An endless CSS animation makes the browser composite every display frame; the page must
    // stay quiet between video frames.
    expect(await page.evaluate(() => document.getAnimations()
      .filter((a) => a.effect && a.effect.getComputedTiming().iterations === Infinity).length)).toBe(0);
  });
});

test('scale=1 starts in 1:1 mode', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&scale=1');
  await waitDrawn(page, 1);
  const dpr = await page.evaluate(() => window.devicePixelRatio);
  expect((await canvasRect(page)).width).toBeCloseTo(1280 / dpr, 3);
  await command(page, { type: 'tilt:scale', mode: 'fit' });
  await expect.poll(async () => (await canvasRect(page)).width).toBeCloseTo(1280, 3);
});

test('tilt:type sends the text as TEXT, only while in control', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  let since = mock.messages.length;
  await command(page, { type: 'tilt:type', text: 'echo "hi"\nls -la' });
  const [msg] = await collect(mock, id, 'TEXT', since, 1);
  expect(msg.text).toBe('echo "hi"\nls -la');
  // Without control nothing goes out.
  await command(page, { type: 'tilt:control', take: false });
  await pollState(page, 'control').toBe(false);
  since = mock.messages.length;
  await command(page, { type: 'tilt:type', text: 'nope' });
  await page.waitForTimeout(300);
  expect(from(mock, id, 'TEXT', since)).toEqual([]);
});

test('long text goes out in TEXT messages of at most 4096 UTF-8 bytes, split between characters', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  // 9801 bytes; the 2-byte 'é' straddles the first 4096-byte boundary.
  const text = `${'x'.repeat(4095)}é${'€'.repeat(1500)}${'😀'.repeat(300)}\nend`;
  const since = mock.messages.length;
  await command(page, { type: 'tilt:type', text });
  const msgs = await collect(mock, id, 'TEXT', since, 3);
  expect(msgs.map((m) => Buffer.byteLength(m.text))).toEqual([4095, 4094, 1612]);
  expect(msgs.map((m) => m.text).join('')).toBe(text);
  expect(mock.violations).toEqual([]);
});

test('host keys: Esc, arrows with repeat, one-shot and locked Ctrl', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  const key = (keysym, down) => command(page, { type: 'tilt:key', keysym, down });
  const ctrl = async () => (await state(page)).modifiers.ctrl;

  let since = mock.messages.length;
  await key(0xff1b, true);
  await key(0xff1b, false);
  expect(keyLog(await collect(mock, id, 'KEY', since, 2))).toEqual(['down ff1b', 'up ff1b']);

  // One-shot Ctrl: held for the next key only.
  since = mock.messages.length;
  await command(page, { type: 'tilt:modifier', name: 'ctrl' });
  expect(await ctrl()).toBe(1);
  await page.keyboard.press('c');
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ffe3', 'down 63', 'up 63', 'up ffe3']);
  expect(await ctrl()).toBe(0);

  // Locked Ctrl: stays down until toggled again.
  since = mock.messages.length;
  await command(page, { type: 'tilt:modifier', name: 'ctrl' });
  await command(page, { type: 'tilt:modifier', name: 'ctrl' });
  expect(await ctrl()).toBe(2);
  await page.keyboard.press('x');
  await key(0xff09, true);
  await key(0xff09, false);
  await command(page, { type: 'tilt:modifier', name: 'ctrl' });
  expect(await ctrl()).toBe(0);
  expect(keyLog(await collect(mock, id, 'KEY', since, 6)))
    .toEqual(['down ffe3', 'down 78', 'up 78', 'down ff09', 'up ff09', 'up ffe3']);

  // Holding an arrow repeats it (repeated KEY down), then releases once.
  since = mock.messages.length;
  await key(0xff51, true);
  await page.waitForTimeout(800);
  await key(0xff51, false);
  const keys = keyLog(await collect(mock, id, 'KEY', since, 4));
  expect(keys.at(-1)).toBe('up ff51');
  expect(keys.slice(0, -1).every((k) => k === 'down ff51')).toBe(true);
  expect(keys.length).toBeGreaterThanOrEqual(5);
});

test('a held host key stops when the window loses focus', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  await command(page, { type: 'tilt:key', keysym: 0xff1b, down: true });
  await page.waitForTimeout(700); // repeating since 500 ms
  const since = mock.messages.length;
  await page.evaluate(() => window.dispatchEvent(new Event('blur')));
  await page.waitForTimeout(600);
  await command(page, { type: 'tilt:key', keysym: 0xff1b, down: false });
  await page.waitForTimeout(200);
  const log = inputLog(from(mock, id, ['KEY', 'RELEASE_ALL'], since));
  const at = log.indexOf('RELEASE_ALL');
  expect(at, 'RELEASE_ALL on blur').toBeGreaterThanOrEqual(0);
  // A repeat still on its way when the window lost focus may come first; nothing follows, not
  // even a key up when the host lets go.
  expect(log.slice(0, at).every((k) => k === 'down ff1b'), log.join(', ')).toBe(true);
  expect(log.slice(at)).toEqual(['RELEASE_ALL']);
});

test('host modifiers are let go for typed text', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  const ctrl = async () => (await state(page)).modifiers.ctrl;
  // One-shot Ctrl, then a soft keyboard commits a whole word: text, not Ctrl+c Ctrl+v.
  let since = mock.messages.length;
  await command(page, { type: 'tilt:modifier', name: 'ctrl' });
  await page.evaluate(() => {
    const ta = document.getElementById('kbd');
    ta.value += 'cv';
    ta.dispatchEvent(new InputEvent('input', { inputType: 'insertText', data: 'cv', bubbles: true }));
  });
  expect(inputLog(await collect(mock, id, ['KEY', 'TEXT'], since, 3))).toEqual(['down ffe3', 'up ffe3', 'TEXT cv']);
  expect(await ctrl()).toBe(0);
  // Locked Ctrl and tilt:type: Ctrl goes up for the text and down again after it.
  since = mock.messages.length;
  await command(page, { type: 'tilt:modifier', name: 'ctrl' });
  await command(page, { type: 'tilt:modifier', name: 'ctrl' });
  expect(await ctrl()).toBe(2);
  await command(page, { type: 'tilt:type', text: 'hello' });
  expect(inputLog(await collect(mock, id, ['KEY', 'TEXT'], since, 4))).toEqual(['down ffe3', 'up ffe3', 'TEXT hello', 'down ffe3']);
  expect(await ctrl()).toBe(2);
});

test('the controller sees the remote cursor as a CSS cursor; viewers get a sprite', async ({ browser, mock }) => {
  const a = await browser.newPage();
  const b = await browser.newPage();
  await open(a, mock, 'token=devtoken&control=1');
  await controlling(a);
  await a.mouse.move(400, 300);
  const css = () => a.evaluate(() => document.getElementById('screen').style.cursor);
  await expect.poll(css).toMatch(/^url\("data:image\/png;base64,[A-Za-z0-9+/=]+"\) 0 0, default$/);
  await expect(a.locator('#cursor')).toBeHidden();

  await open(b, mock, 'token=viewtoken');
  await waitDrawn(b, 1);
  const sprite = b.locator('#cursor');
  await expect(sprite).toBeVisible();
  mock.moveCursor(100, 200);
  await expect.poll(() => sprite.evaluate((el) => el.style.transform)).toBe('translate(100px, 200px)');
  await expect(b.locator('#cursor')).toHaveAttribute('src', /^data:image\/png;base64,/);

  // Hidden cursor: CSS 'none' for the controller, no sprite for viewers.
  mock.setCursor(null);
  await expect.poll(css).toBe('none');
  await expect(sprite).toBeHidden();

  // Shapes over 128 px cannot be CSS cursors: the controller gets the sprite too.
  mock.setCursor(solidCursor(160, 160));
  await expect.poll(css).toBe('none');
  await expect(a.locator('#cursor')).toBeVisible();
  await expect(sprite).toBeVisible();
  await a.close();
  await b.close();
});

for (const [name, viewport, hasTouch] of [
  ['at phone size', { width: 375, height: 667 }, false],
  ['on a narrow touch phone', { width: 360, height: 740 }, true],
]) {
  test.describe(name, () => {
    test.use({ viewport, hasTouch });

    test('the screen fits the width, centred, with no page scroll', async ({ page, mock }) => {
      await open(page, mock, 'token=devtoken&control=1');
      await controlling(page);
      const m = await page.evaluate(() => ({
        scrollWidth: document.documentElement.scrollWidth,
        scrollHeight: document.documentElement.scrollHeight,
        bodyScroll: document.body.scrollWidth,
        width: window.innerWidth,
        height: window.innerHeight,
        viewport: document.getElementById('viewport').getBoundingClientRect().toJSON(),
        canvas: document.getElementById('screen').getBoundingClientRect().toJSON(),
      }));
      expect(m.scrollWidth).toBeLessThanOrEqual(m.width);
      expect(m.bodyScroll).toBeLessThanOrEqual(m.width);
      expect(m.scrollHeight).toBeLessThanOrEqual(m.height);
      // The viewport is the whole window; 1280x720 fitted to its width, centred vertically.
      expect([m.viewport.left, m.viewport.top, m.viewport.width, m.viewport.height]).toEqual([0, 0, m.width, m.height]);
      expect(m.canvas.left).toBeCloseTo(0, 3);
      expect(m.canvas.width).toBeCloseTo(m.width, 3);
      expect(m.canvas.height).toBeCloseTo(m.width * 720 / 1280, 3);
      expect(m.canvas.top).toBeCloseTo((m.height - m.width * 720 / 1280) / 2, 3);
    });
  });
}

test('keymap: Cmd+C can stay a Super shortcut on Apple clients', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  test.skip(!(await isApple(page)), 'Apple-only option');
  await page.evaluate(() => document.getElementById('kbd').focus());
  // By default Cmd+C acts as Ctrl+C.
  let since = mock.messages.length;
  await page.keyboard.press('Meta+c');
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ffe3', 'down 63', 'up 63', 'up ffe3']);
  await command(page, { type: 'tilt:keymap', cmdToCtrl: false });
  since = mock.messages.length;
  await page.keyboard.press('Meta+c');
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ffeb', 'down 63', 'up 63', 'up ffeb']);
});

test('keymap: Option can act as Alt on Apple clients', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  test.skip(!(await isApple(page)), 'Apple-only option');
  await command(page, { type: 'tilt:keymap', optionAsAlt: true });
  await page.evaluate(() => document.getElementById('kbd').focus());
  // Option+c on a US Mac layout types ç; as Alt it is Alt+c.
  const since = mock.messages.length;
  await keyEvents(page, [
    ['keydown', { key: 'Alt', code: 'AltLeft', altKey: true }],
    ['keydown', { key: 'ç', code: 'KeyC', altKey: true }], ['keyup', { key: 'ç', code: 'KeyC', altKey: true }],
    ['keyup', { key: 'Alt', code: 'AltLeft' }],
  ]);
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ffe9', 'down 63', 'up 63', 'up ffe9']);
});

test('tilt:fullscreen toggles fullscreen and the state follows', async ({ page, mock }) => {
  await open(page, mock);
  await waitDrawn(page, 1);
  const fullscreen = () => page.evaluate(() => Boolean(document.fullscreenElement || document.webkitFullscreenElement));
  test.skip(!(await page.evaluate(() => Boolean(document.documentElement.requestFullscreen
    || document.documentElement.webkitRequestFullscreen))), 'no Fullscreen API in this engine');
  // Entering fullscreen needs a user gesture: a click (which, as a viewer, sends nothing).
  await page.mouse.click(640, 360);
  await command(page, { type: 'tilt:fullscreen' });
  await expect.poll(fullscreen).toBe(true);
  await pollState(page, 'fullscreen').toBe(true);
  await command(page, { type: 'tilt:fullscreen' });
  await expect.poll(fullscreen).toBe(false);
  await pollState(page, 'fullscreen').toBe(false);
});

test.describe('a base64 token', () => {
  const TOKEN = 'k3/Zp+Qe==';
  test.use({ mockOptions: { token: TOKEN } });

  for (const [form, value] of [['as is', TOKEN], ['percent-encoded', encodeURIComponent(TOKEN)]]) {
    test(`is read from the URL fragment ${form}, keeping the other options`, async ({ page, mock }) => {
      const { frame, msgs } = await framed(page, mock, `token=${value}&stats=1`);
      await frame.waitForFunction(() => window.tilt.stats.framesDrawn > 0);
      expect(await frame.evaluate(() => window.tilt.stats.role)).toBe('control');
      expect(mock.messages.filter((m) => m.type === 'rejected')).toEqual([]);
      expect(await frame.evaluate(() => location.hash)).toBe('#stats=1');
      // stats=1 still applies.
      await expect.poll(async () => (await msgs('tilt:stats')).length, { timeout: 3000 }).toBeGreaterThan(0);
    });
  }
});

test('a new #token in the URL reconnects with it', async ({ page, mock }) => {
  await open(page, mock, 'token=wrong');
  await pollState(page, 'message').toContain('Access denied');
  expect(await state(page)).toMatchObject({ status: 'error', code: 'auth' });
  await page.evaluate(() => { location.hash = 'token=devtoken'; });
  await waitDrawn(page, 3);
  expect(await page.evaluate(() => window.tilt.stats.role)).toBe('control');
  expect(page.url()).not.toContain('token');
  await pollState(page, 'status').toBe('live');
});

test('tilt:token saves the token for the tab and reconnects with it', async ({ page, mock }) => {
  await open(page, mock, 'token=wrong');
  await pollState(page, 'code').toBe('auth');
  await command(page, { type: 'tilt:token', token: ' devtoken ' });
  await waitDrawn(page, 3);
  await pollState(page, 'status').toBe('live');
  expect(await page.evaluate(() => window.tilt.stats.role)).toBe('control');
  expect(await page.evaluate(() => sessionStorage.getItem('tilt.token'))).toBe('devtoken');
  // A reload keeps it.
  await page.reload();
  await page.waitForFunction(() => window.tilt !== undefined);
  await waitDrawn(page, 1);
  expect(await state(page)).toMatchObject({ status: 'live', role: 'control', message: '', code: '' });
});
