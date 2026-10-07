import { solidCursor } from '../server.mjs';
import {
  canvasRect, clickMenu, clickToolbar, collect, controlling, expect, from, inputLog, isApple, keyEvents, keyLog, norm, open,
  tapMenu, test, waitDrawn,
} from './fixtures.mjs';

test('shows the unsupported panel without WebCodecs', async ({ page, mock }) => {
  await page.addInitScript(() => {
    delete window.VideoDecoder;
    delete window.EncodedVideoChunk;
  });
  await page.goto(`${mock.url}#token=devtoken`);
  await expect(page.locator('#unsupported')).toBeVisible();
  await expect(page.locator('#unsupported-msg')).toHaveText(
    'This browser lacks WebCodecs H.264 decoding (needs Chrome/Edge 94+, Safari/iOS 16.4+, Android WebView 94+).');
  await page.waitForTimeout(300);
  expect(mock.messages.filter((m) => m.type === 'open')).toHaveLength(0);
});

test('mentions the secure-context requirement on insecure origins', async ({ page, mock }) => {
  await page.addInitScript(() => {
    delete window.VideoDecoder;
    Object.defineProperty(window, 'isSecureContext', { value: false });
  });
  await page.goto(`${mock.url}#token=devtoken`);
  await expect(page.locator('#unsupported-msg')).toContainText('not a secure context');
  await expect(page.locator('#unsupported-msg')).toContainText('https://');
});

test('stats overlay shows client and server numbers and remembers the toggle', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&stats=1');
  await page.waitForFunction(() => window.tilt.stats.framesDrawn > 30 && window.tilt.stats.rttMs !== null);
  const panel = page.locator('#stats');
  await expect(panel).toBeVisible();
  await expect(panel).toContainText(/fps +\d+(\.\d)? decoded +\d+(\.\d)? drawn/);
  await expect(panel).toContainText(/ping \d+ ms/);
  await expect(panel).toContainText('1280x720');
  await expect(panel).toContainText(/server +\d+ fps/);
  await expect(panel).toContainText(/viewers 1/);
  await expect(panel).toContainText(/queue \d+/);
  // The switch lives in the settings menu.
  const stats = page.getByRole('checkbox', { name: 'Stream statistics' });
  await clickToolbar(page, page.getByRole('button', { name: 'Settings' }));
  await expect(stats).toBeChecked();
  await stats.uncheck();
  await expect(panel).toBeHidden();
  // Without stats= in the URL the stored choice applies; the token comes from sessionStorage.
  await page.goto(mock.url);
  await waitDrawn(page, 1);
  await expect(panel).toBeHidden();
  await clickToolbar(page, page.getByRole('button', { name: 'Settings' }));
  await stats.check();
  await expect(panel).toBeVisible();
  await page.reload();
  await waitDrawn(page, 1);
  await expect(panel).toBeVisible();
});

test.describe('in an 800x600 window', () => {
  test.use({ viewport: { width: 800, height: 600 } });

  test('fits, switches to 1:1 and keeps input mapping right', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    const id = await controlling(page);
    expect(await canvasRect(page)).toEqual({ left: 0, top: 75, width: 800, height: 450 });
    const dpr = await page.evaluate(() => window.devicePixelRatio);
    const scale = page.getByRole('button', { name: 'Scale' });
    await expect(scale).toHaveText('Fit');
    await clickMenu(page, scale);
    await expect(scale).toHaveText('1:1');
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
    await clickMenu(page, scale);
    await expect(scale).toHaveText('Fit');
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

  test('runs no endless animations, connecting or connected', async ({ page, mock }) => {
    // An endless CSS animation makes the browser composite every display frame, at 120 Hz on
    // some screens, for as long as it runs; the page must stay quiet between video frames.
    const endless = () => page.evaluate(() => document.getAnimations()
      .filter((a) => a.effect && a.effect.getComputedTiming().iterations === Infinity).length);
    await open(page, mock, 'token=devtoken');
    // Retrying a server that is not ready yet: the "Connecting…" state.
    await expect(page.locator('#overlay')).toBeVisible();
    await expect(page.locator('#overlay-detail')).toContainText('not ready');
    expect(await endless()).toBe(0);
  });
});

test('runs no endless animations while streaming', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  await controlling(page);
  await expect(page.locator('#overlay')).toBeHidden();
  expect(await page.evaluate(() => document.getAnimations()
    .filter((a) => a.effect && a.effect.getComputedTiming().iterations === Infinity).length)).toBe(0);
});

test('scale=1 starts in 1:1 mode', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&scale=1');
  await waitDrawn(page, 1);
  const dpr = await page.evaluate(() => window.devicePixelRatio);
  expect((await canvasRect(page)).width).toBeCloseTo(1280 / dpr, 3);
  await expect(page.getByRole('button', { name: 'Scale' })).toHaveText('1:1');
});

test('Type text sends the dialog contents as TEXT', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  await clickMenu(page, page.getByRole('button', { name: 'Type text' }));
  await expect(page.locator('#typer')).toBeVisible();
  await page.fill('#typer-text', 'echo "hi"\nls -la');
  const since = mock.messages.length;
  await page.click('#typer-send');
  const [msg] = await collect(mock, id, 'TEXT', since, 1);
  expect(msg.text).toBe('echo "hi"\nls -la');
  await expect(page.locator('#typer')).toBeHidden();
  // Focus returns to the remote keyboard.
  expect(await page.evaluate(() => document.activeElement.id)).toBe('kbd');
});

test('long text goes out in TEXT messages of at most 4096 UTF-8 bytes, split between characters', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  await clickMenu(page, page.getByRole('button', { name: 'Type text' }));
  // 9801 bytes; the 2-byte 'é' straddles the first 4096-byte boundary.
  const text = `${'x'.repeat(4095)}é${'€'.repeat(1500)}${'😀'.repeat(300)}\nend`;
  await page.fill('#typer-text', text);
  const since = mock.messages.length;
  await page.click('#typer-send');
  const msgs = await collect(mock, id, 'TEXT', since, 3);
  expect(msgs.map((m) => Buffer.byteLength(m.text))).toEqual([4095, 4094, 1612]);
  expect(msgs.map((m) => m.text).join('')).toBe(text);
  expect(mock.violations).toEqual([]);
});

test('extra keys: Esc, arrows with repeat, one-shot and locked Ctrl', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  await clickMenu(page, page.getByRole('button', { name: 'Keys' }));
  const strip = page.locator('#keys');
  await expect(strip).toBeVisible();

  let since = mock.messages.length;
  await strip.getByRole('button', { name: 'Esc' }).click();
  expect(keyLog(await collect(mock, id, 'KEY', since, 2))).toEqual(['down ff1b', 'up ff1b']);

  // One-shot Ctrl: held for the next key only.
  const ctrl = strip.getByRole('button', { name: 'Ctrl' });
  since = mock.messages.length;
  await ctrl.click();
  await expect(ctrl).toHaveAttribute('data-state', '1');
  await page.keyboard.press('c');
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ffe3', 'down 63', 'up 63', 'up ffe3']);
  await expect(ctrl).toHaveAttribute('data-state', '0');

  // Locked Ctrl: stays down until tapped again.
  since = mock.messages.length;
  await ctrl.click();
  await ctrl.click();
  await expect(ctrl).toHaveAttribute('data-state', '2');
  await page.keyboard.press('x');
  await strip.getByRole('button', { name: 'Tab' }).click();
  await ctrl.click();
  await expect(ctrl).toHaveAttribute('data-state', '0');
  expect(keyLog(await collect(mock, id, 'KEY', since, 6)))
    .toEqual(['down ffe3', 'down 78', 'up 78', 'down ff09', 'up ff09', 'up ffe3']);

  // Holding an arrow repeats it (repeated KEY down), then releases once.
  const left = strip.getByRole('button', { name: 'Left arrow' });
  const box = await left.boundingBox();
  since = mock.messages.length;
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(800);
  await page.mouse.up();
  const keys = keyLog(await collect(mock, id, 'KEY', since, 4));
  expect(keys.at(-1)).toBe('up ff51');
  expect(keys.slice(0, -1).every((k) => k === 'down ff51')).toBe(true);
  expect(keys.length).toBeGreaterThanOrEqual(5);
});

test('a held extra key stops when the window loses focus', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  await clickMenu(page, page.getByRole('button', { name: 'Keys' }));
  const box = await page.locator('#keys').getByRole('button', { name: 'Esc' }).boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(700); // repeating since 500 ms
  const since = mock.messages.length;
  await page.evaluate(() => window.dispatchEvent(new Event('blur')));
  await page.waitForTimeout(600);
  await page.mouse.up();
  await page.waitForTimeout(200);
  const log = inputLog(from(mock, id, ['KEY', 'RELEASE_ALL'], since));
  const at = log.indexOf('RELEASE_ALL');
  expect(at, 'RELEASE_ALL on blur').toBeGreaterThanOrEqual(0);
  // A repeat still on its way when the window lost focus may come first; nothing follows, not
  // even a key up when the button is let go.
  expect(log.slice(0, at).every((k) => k === 'down ff1b'), log.join(', ')).toBe(true);
  expect(log.slice(at)).toEqual(['RELEASE_ALL']);
});

test('extra-key modifiers are let go for typed text', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  await clickMenu(page, page.getByRole('button', { name: 'Keys' }));
  const ctrl = page.locator('#keys').getByRole('button', { name: 'Ctrl' });
  // One-shot Ctrl, then a soft keyboard commits a whole word: text, not Ctrl+c Ctrl+v.
  let since = mock.messages.length;
  await ctrl.click();
  await page.evaluate(() => {
    const ta = document.getElementById('kbd');
    ta.value += 'cv';
    ta.dispatchEvent(new InputEvent('input', { inputType: 'insertText', data: 'cv', bubbles: true }));
  });
  expect(inputLog(await collect(mock, id, ['KEY', 'TEXT'], since, 3))).toEqual(['down ffe3', 'up ffe3', 'TEXT cv']);
  await expect(ctrl).toHaveAttribute('data-state', '0');
  // Locked Ctrl and Type text: Ctrl goes up for the text and down again after it.
  since = mock.messages.length;
  await ctrl.click();
  await ctrl.click();
  await expect(ctrl).toHaveAttribute('data-state', '2');
  await clickMenu(page, page.getByRole('button', { name: 'Type text' }));
  await page.fill('#typer-text', 'hello');
  await page.click('#typer-send');
  expect(inputLog(await collect(mock, id, ['KEY', 'TEXT'], since, 4))).toEqual(['down ffe3', 'up ffe3', 'TEXT hello', 'down ffe3']);
  await expect(ctrl).toHaveAttribute('data-state', '2');
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

test.describe('at phone size', () => {
  test.use({ viewport: { width: 375, height: 667 } });

  test('has no horizontal page scroll and keeps the toolbar on screen', async ({ page, mock }) => {
    await open(page, mock);
    await waitDrawn(page, 1);
    const m = await page.evaluate(() => ({
      scrollWidth: document.documentElement.scrollWidth,
      bodyScroll: document.body.scrollWidth,
      width: window.innerWidth,
      toolbar: document.getElementById('toolbar').getBoundingClientRect().toJSON(),
      canvas: document.getElementById('screen').getBoundingClientRect().toJSON(),
    }));
    expect(m.scrollWidth).toBeLessThanOrEqual(m.width);
    expect(m.bodyScroll).toBeLessThanOrEqual(m.width);
    expect(m.toolbar.left).toBeGreaterThanOrEqual(0);
    expect(m.toolbar.right).toBeLessThanOrEqual(m.width);
    // 1280x720 fitted to 375 wide.
    expect(m.canvas.width).toBeCloseTo(375, 3);
    expect(m.canvas.height).toBeCloseTo(210.9375, 3);
  });
});

test.describe('on a narrow touch phone', () => {
  test.use({ viewport: { width: 360, height: 740 }, hasTouch: true });

  test('fits every toolbar button and extra key on screen, above which the video sits', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    await controlling(page);
    await tapMenu(page, page.getByRole('button', { name: 'Keys' }));
    await expect(page.locator('#keys')).toBeVisible();
    // The remote screen gives way to the strip (a frame after the strip appears).
    await expect.poll(() => page.evaluate(() => document.getElementById('viewport').getBoundingClientRect().bottom
      <= document.getElementById('keys').getBoundingClientRect().top)).toBe(true);
    const m = await page.evaluate(() => {
      const shown = (sel) => [...document.querySelectorAll(sel)].filter((b) => b.getClientRects().length > 0)
        .map((b) => ({ name: b.getAttribute('aria-label') || b.textContent.trim(), ...b.getBoundingClientRect().toJSON() }));
      const bar = document.getElementById('toolbar');
      return {
        width: window.innerWidth,
        pageScroll: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth),
        barOverflow: bar.scrollWidth - bar.clientWidth,
        buttons: shown('#toolbar button'),
        keys: shown('#keys button'),
        canvas: document.getElementById('screen').getBoundingClientRect().toJSON(),
      };
    });
    expect(m.pageScroll).toBeLessThanOrEqual(m.width);
    expect(m.barOverflow).toBe(0);
    expect(m.buttons.map((b) => b.name)).toEqual(expect.arrayContaining(['Control', 'Keyboard', 'Settings']));
    for (const b of m.buttons) {
      expect(b.left, b.name).toBeGreaterThanOrEqual(0);
      expect(b.right, b.name).toBeLessThanOrEqual(m.width);
      expect(b.width, b.name).toBeGreaterThanOrEqual(36);
      expect(b.height, b.name).toBeGreaterThanOrEqual(44);
    }
    expect(m.keys).toHaveLength(9);
    for (const k of m.keys) {
      expect(k.left, k.name).toBeGreaterThanOrEqual(0);
      expect(k.right, k.name).toBeLessThanOrEqual(m.width);
      expect(k.width, k.name).toBeGreaterThanOrEqual(44);
      expect(k.height, k.name).toBeGreaterThanOrEqual(44);
    }
    expect(m.canvas.width).toBeCloseTo(360, 3);
  });
});

test('settings: Cmd+C can stay a Super shortcut on Apple clients', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  test.skip(!(await page.evaluate(() => /Mac|iPhone|iPad/.test(navigator.platform))), 'Apple-only option');
  await clickToolbar(page, page.getByRole('button', { name: 'Settings' }));
  const option = page.getByRole('checkbox', { name: /act as Ctrl/ });
  await expect(option).toBeChecked();
  // Desktop pointers get no touch-mode choice.
  await expect(page.locator('#touch-row')).toBeHidden();
  await option.uncheck();
  expect(await page.evaluate(() => localStorage.getItem('tilt.cmdToCtrl'))).toBe('0');
  await page.mouse.click(640, 600);
  await expect(page.locator('#menu')).toBeHidden();
  await page.evaluate(() => document.getElementById('kbd').focus());
  const since = mock.messages.length;
  await page.keyboard.press('Meta+c');
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ffeb', 'down 63', 'up 63', 'up ffeb']);
});

test('settings: Option can act as Alt on Apple clients', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  test.skip(!(await isApple(page)), 'Apple-only option');
  await clickToolbar(page, page.getByRole('button', { name: 'Settings' }));
  const option = page.getByRole('checkbox', { name: /Option acts as Alt/ });
  await expect(option).not.toBeChecked();
  await option.check();
  expect(await page.evaluate(() => localStorage.getItem('tilt.optionAsAlt'))).toBe('1');
  await page.mouse.click(640, 600);
  await expect(page.locator('#menu')).toBeHidden();
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

test('fullscreen toggles from the toolbar', async ({ page, mock }) => {
  await open(page, mock);
  await waitDrawn(page, 1);
  const button = page.getByRole('button', { name: 'Fullscreen' });
  test.skip(!(await button.isVisible()), 'no Fullscreen API in this engine');
  await button.click();
  await expect.poll(() => page.evaluate(() => Boolean(document.fullscreenElement || document.webkitFullscreenElement))).toBe(true);
  await expect(button).toHaveAttribute('aria-pressed', 'true');
  await button.click();
  await expect.poll(() => page.evaluate(() => Boolean(document.fullscreenElement || document.webkitFullscreenElement))).toBe(false);
  await expect(button).toHaveAttribute('aria-pressed', 'false');
});

test.describe('a base64 token', () => {
  const TOKEN = 'k3/Zp+Qe==';
  test.use({ mockOptions: { token: TOKEN } });

  for (const [form, value] of [['as is', TOKEN], ['percent-encoded', encodeURIComponent(TOKEN)]]) {
    test(`is read from the URL fragment ${form}, keeping the other options`, async ({ page, mock }) => {
      await open(page, mock, `token=${value}&stats=1`);
      await waitDrawn(page, 1);
      expect(await page.evaluate(() => window.tilt.stats.role)).toBe('control');
      expect(mock.messages.filter((m) => m.type === 'rejected')).toEqual([]);
      expect(await page.evaluate(() => location.hash)).toBe('#stats=1');
      await expect(page.locator('#stats')).toBeVisible();
    });
  }
});

test('a new #token in the URL reconnects with it', async ({ page, mock }) => {
  await open(page, mock, 'token=wrong');
  await expect(page.locator('#overlay-msg')).toContainText('Access denied');
  await page.evaluate(() => { location.hash = 'token=devtoken'; });
  await waitDrawn(page, 3);
  expect(await page.evaluate(() => window.tilt.stats.role)).toBe('control');
  expect(page.url()).not.toContain('token');
});
