// Touch gestures, driven by synthetic touch PointerEvents on the viewer (both engines), plus a
// real touchscreen tap where the browser supports one.
import {
  canvasRect, collect, controlling, expect, from, INPUT_TYPES, norm, open, sessionOf, tapToolbar, test, waitDrawn,
} from './fixtures.mjs';

/**
 * Plays touch pointer events on #viewport. Steps: ['down'|'move'|'up', id, x, y] or ['wait', ms].
 * Moves are split into `n` intermediate events like a real finger.
 *
 * Each event's timeStamp is the scripted time (waits, plus 8 ms per move step), not the time it
 * was dispatched: a busy CI runner can stall a 30 ms wait past the 250 ms tap window, which would
 * turn a tap into a hold. Timers in the page (long press, the tap's button-up) still run on real time.
 */
async function touch(page, steps) {
  await page.evaluate(async (list) => {
    const vp = document.getElementById('viewport');
    const at = new Map();
    let clock = performance.now();
    const fire = (type, id, x, y) => {
      const e = new PointerEvent(`pointer${type}`, {
        pointerId: 100 + id,
        pointerType: 'touch',
        isPrimary: id === 0,
        clientX: x,
        clientY: y,
        button: type === 'move' ? -1 : 0,
        buttons: type === 'up' ? 0 : 1,
        width: 20,
        height: 20,
        bubbles: true,
        cancelable: true,
        composed: true,
      });
      Object.defineProperty(e, 'timeStamp', { value: clock });
      vp.dispatchEvent(e);
      at.set(id, [x, y]);
    };
    for (const [type, id, x, y, n = 1] of list) {
      if (type === 'wait') {
        clock += id;
        await new Promise((r) => setTimeout(r, id));
      } else if (type === 'move') {
        const [x0, y0] = at.get(id);
        for (let i = 1; i <= n; i++) {
          fire('move', id, x0 + ((x - x0) * i) / n, y0 + ((y - y0) * i) / n);
          clock += 8;
          await new Promise((r) => setTimeout(r, 8));
        }
      } else if (type === 'pair') {
        // Both fingers [id 0 -> (x0,y0,x1,y1)] move together in n steps.
        const [ax, ay] = at.get(0);
        const [bx, by] = at.get(1);
        const [tax, tay, tbx, tby] = id;
        const steps = x;
        for (let i = 1; i <= steps; i++) {
          fire('move', 0, ax + ((tax - ax) * i) / steps, ay + ((tay - ay) * i) / steps);
          fire('move', 1, bx + ((tbx - bx) * i) / steps, by + ((tby - by) * i) / steps);
          clock += 8;
          await new Promise((r) => setTimeout(r, 8));
        }
      } else {
        fire(type, id, x, y);
      }
    }
  }, steps);
}

const stageScale = (page) => page.evaluate(() => {
  const m = /scale\(([\d.]+)\)/.exec(document.getElementById('stage').style.transform);
  return m ? Number(m[1]) : 1;
});

test.describe('direct touch', () => {
  test('tap clicks, long-press right-clicks, drag drags', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1&touch=direct');
    const id = await controlling(page);
    const r = await canvasRect(page);

    let since = mock.messages.length;
    await touch(page, [['down', 0, 400, 300], ['wait', 60], ['up', 0, 400, 300]]);
    let msgs = await collect(mock, id, ['BUTTON', 'MOVE'], since, 2);
    expect(msgs.map((m) => [m.type, m.button, m.down, m.x, m.y])).toEqual([
      ['BUTTON', 1, true, ...norm(r, 400, 300)],
      ['BUTTON', 1, false, ...norm(r, 400, 300)],
    ]);

    since = mock.messages.length;
    await touch(page, [['down', 0, 500, 350], ['wait', 650], ['up', 0, 500, 350]]);
    msgs = await collect(mock, id, ['BUTTON', 'MOVE'], since, 2);
    expect(msgs.map((m) => [m.button, m.down, m.x, m.y])).toEqual([
      [3, true, ...norm(r, 500, 350)],
      [3, false, ...norm(r, 500, 350)],
    ]);

    since = mock.messages.length;
    await touch(page, [['down', 0, 300, 300], ['move', 0, 420, 360, 12], ['up', 0, 420, 360]]);
    msgs = await collect(mock, id, ['BUTTON', 'MOVE'], since, 3);
    const buttons = msgs.filter((m) => m.type === 'BUTTON');
    expect(buttons.map((m) => [m.button, m.down, m.x, m.y])).toEqual([
      [1, true, ...norm(r, 300, 300)],
      [1, false, ...norm(r, 420, 360)],
    ]);
    expect(msgs.indexOf(buttons[1]) - msgs.indexOf(buttons[0])).toBeGreaterThan(1);
    expect(mock.violations).toEqual([]);
  });

  test('two fingers scroll, pinch zooms locally, two-finger tap right-clicks', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1&touch=direct');
    const id = await controlling(page);
    const r = await canvasRect(page);

    // Fingers move up 100 px together: 5 notches of 20 px, content follows (+dy, scroll down).
    let since = mock.messages.length;
    await touch(page, [['down', 0, 500, 400], ['down', 1, 600, 400], ['pair', [500, 300, 600, 300], 20], ['up', 0, 500, 300], ['up', 1, 600, 300]]);
    const wheels = await collect(mock, id, 'WHEEL', since, 1, 200);
    expect(wheels.reduce((n, w) => n + w.dy, 0)).toBe(5);
    expect(wheels.every((w) => w.dx === 0)).toBe(true);
    expect(from(mock, id, 'BUTTON', since)).toEqual([]);
    expect(await stageScale(page)).toBe(1);

    // Spreading the fingers is a pinch: local zoom only, nothing sent.
    since = mock.messages.length;
    await touch(page, [['down', 0, 550, 400], ['down', 1, 650, 400], ['pair', [450, 400, 750, 400], 20], ['up', 0, 450, 400], ['up', 1, 750, 400]]);
    await page.waitForTimeout(200);
    expect(from(mock, id, INPUT_TYPES, since).filter((m) => m.type !== 'MOVE')).toEqual([]);
    expect(await stageScale(page)).toBeGreaterThan(2);

    // Two-finger tap: right click at the fingers' midpoint (mapped through the zoom).
    const zoomed = await canvasRect(page);
    since = mock.messages.length;
    await touch(page, [['down', 0, 300, 300], ['down', 1, 340, 300], ['wait', 50], ['up', 0, 300, 300], ['up', 1, 340, 300]]);
    const right = await collect(mock, id, 'BUTTON', since, 2);
    expect(right.map((m) => [m.button, m.down, m.x, m.y])).toEqual([
      [3, true, ...norm(zoomed, 320, 300)],
      [3, false, ...norm(zoomed, 320, 300)],
    ]);
    expect(r.width).toBeLessThan(zoomed.width);
  });
});

test.describe('direct touch beside the video', () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test('touches that start in the letterbox send nothing; on the video they click', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1&touch=direct');
    const id = await controlling(page);
    const r = await canvasRect(page);
    expect(r).toEqual({ left: 0, top: 90, width: 1280, height: 720 });
    let since = mock.messages.length;
    // Above the video: a tap; below it: a long-press, and a drag onto the video; above it: a
    // two-finger tap. Nothing may land on the remote screen's edge (its panels).
    await touch(page, [['down', 0, 640, 40], ['wait', 60], ['up', 0, 640, 40]]);
    await touch(page, [['down', 0, 900, 870], ['wait', 650], ['up', 0, 900, 870]]);
    await touch(page, [['down', 0, 300, 860], ['move', 0, 420, 500, 30], ['up', 0, 420, 500]]);
    await touch(page, [['down', 0, 300, 30], ['down', 1, 340, 30], ['wait', 50], ['up', 0, 300, 30], ['up', 1, 340, 30]]);
    await page.waitForTimeout(300);
    expect(from(mock, id, INPUT_TYPES, since)).toEqual([]);
    since = mock.messages.length;
    await touch(page, [['down', 0, 640, 400], ['wait', 60], ['up', 0, 640, 400]]);
    const msgs = await collect(mock, id, 'BUTTON', since, 2);
    expect(msgs.map((m) => [m.button, m.down, m.x, m.y])).toEqual([
      [1, true, ...norm(r, 640, 400)],
      [1, false, ...norm(r, 640, 400)],
    ]);
  });
});

test.describe('direct touch, two fingers', () => {
  test('a second finger landing just after the first moved makes a pinch, not a drag', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1&touch=direct');
    const id = await controlling(page);
    const r = await canvasRect(page);
    // A slow one-finger drag still drags, live: the press goes out while the finger moves.
    let since = mock.messages.length;
    await touch(page, [['down', 0, 300, 300], ['move', 0, 420, 360, 30], ['up', 0, 420, 360]]);
    const drag = await collect(mock, id, ['BUTTON', 'MOVE'], since, 3);
    const buttons = drag.filter((m) => m.type === 'BUTTON');
    expect(buttons.map((m) => [m.button, m.down, m.x, m.y])).toEqual([
      [1, true, ...norm(r, 300, 300)],
      [1, false, ...norm(r, 420, 360)],
    ]);
    expect(drag.indexOf(buttons[1]) - drag.indexOf(buttons[0])).toBeGreaterThan(1);
    expect(buttons[1].at - buttons[0].at).toBeGreaterThan(50);
    // The first finger moves 15 px, then the second lands and they spread: local zoom only.
    since = mock.messages.length;
    await touch(page, [
      ['down', 0, 600, 400], ['move', 0, 593, 400], ['move', 0, 585, 400], ['down', 1, 700, 400],
      ['pair', [435, 400, 850, 400], 10], ['up', 1, 850, 400], ['up', 0, 435, 400],
    ]);
    await page.waitForTimeout(300);
    expect(from(mock, id, INPUT_TYPES, since)).toEqual([]);
    expect(await stageScale(page)).toBeGreaterThan(1);
  });
});

test.describe('trackpad touch', () => {
  test('moves a virtual cursor, taps click at it, two-finger tap right-clicks', async ({ page, mock }) => {
    // When the page sent each BUTTON: arrival times at the mock bunch up on a loaded machine.
    await page.addInitScript(() => {
      window.buttonsSentAt = [];
      const send = WebSocket.prototype.send;
      WebSocket.prototype.send = function (data) {
        if (data instanceof Uint8Array && data[0] === 0x21) window.buttonsSentAt.push(performance.now());
        return send.call(this, data);
      };
    });
    await open(page, mock, 'token=devtoken&control=1&touch=trackpad');
    const id = await controlling(page);
    const sprite = page.locator('#cursor');

    // The cursor starts at the server's position (screen centre) and moves relative to the finger.
    let since = mock.messages.length;
    await touch(page, [['down', 0, 200, 600], ['move', 0, 260, 630, 6], ['up', 0, 260, 630]]);
    const moves = await collect(mock, id, 'MOVE', since, 3);
    const last = moves.at(-1);
    expect(last.px).toBeGreaterThan(640);
    expect(last.py).toBeGreaterThan(360);
    expect(from(mock, id, 'BUTTON', since)).toEqual([]);
    await expect(sprite).toBeVisible();

    // Tap: left click at the virtual cursor, released after the 250 ms double-tap window.
    await page.waitForTimeout(400);
    since = mock.messages.length;
    await touch(page, [['down', 0, 100, 100], ['wait', 40], ['up', 0, 100, 100]]);
    const click = await collect(mock, id, 'BUTTON', since, 2);
    expect(click.map((m) => [m.button, m.down, m.px, m.py])).toEqual([
      [1, true, last.px, last.py],
      [1, false, last.px, last.py],
    ]);
    const [downAt, upAt] = await page.evaluate(() => window.buttonsSentAt.slice(-2));
    expect(upAt - downAt).toBeGreaterThan(200);

    since = mock.messages.length;
    await touch(page, [['down', 0, 300, 300], ['down', 1, 340, 300], ['wait', 50], ['up', 0, 300, 300], ['up', 1, 340, 300]]);
    const right = await collect(mock, id, 'BUTTON', since, 2);
    expect(right.map((m) => [m.button, m.down, m.px, m.py])).toEqual([
      [3, true, last.px, last.py],
      [3, false, last.px, last.py],
    ]);
  });

  test('tap then touch-and-drag is a left drag', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1&touch=trackpad');
    const id = await controlling(page);
    const since = mock.messages.length;
    await touch(page, [
      // The second touch follows the tap at once: a real wait could stall past the tap's 250 ms
      // button-up timer on a busy runner, which would make this a click and then a plain move.
      ['down', 0, 400, 400], ['wait', 30], ['up', 0, 400, 400],
      ['down', 0, 400, 400], ['move', 0, 480, 400, 8], ['up', 0, 480, 400],
    ]);
    const msgs = await collect(mock, id, ['BUTTON', 'MOVE'], since, 4, 400);
    const buttons = msgs.filter((m) => m.type === 'BUTTON');
    expect(buttons.map((m) => [m.button, m.down])).toEqual([[1, true], [1, false]]);
    const between = msgs.slice(msgs.indexOf(buttons[0]) + 1, msgs.indexOf(buttons[1]));
    expect(between.length).toBeGreaterThan(0);
    expect(between.every((m) => m.type === 'MOVE')).toBe(true);
    expect(buttons[1].px).toBeGreaterThan(buttons[0].px);
  });
});

test('a trackpad tap after the screen shrank under a resting finger stays on the new screen', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1&touch=trackpad');
  const id = await controlling(page);
  // Drive the virtual cursor to the right edge of the 1280-wide screen.
  for (let i = 0; i < 4; i++) await touch(page, [['down', 0, 100, 400], ['move', 0, 700, 400, 12], ['up', 0, 700, 400]]);
  expect(from(mock, id, 'MOVE').at(-1).px).toBe(1279);
  await page.waitForTimeout(400);
  // The remote shrinks to 1024x768 while a finger rests on the screen, so the server's pointer
  // report is not adopted and the virtual cursor keeps its old x.
  await touch(page, [['down', 0, 300, 300]]);
  mock.switchStream('high_1024x768');
  await page.waitForFunction(() => window.tilt.stats.width === 1024);
  mock.moveCursor(1023, 400);
  await touch(page, [['wait', 400], ['up', 0, 300, 300]]); // a hold, not a tap: sends nothing
  await page.waitForTimeout(400);
  const since = mock.messages.length;
  await touch(page, [['down', 0, 300, 300], ['wait', 40], ['up', 0, 300, 300]]);
  const [down] = await collect(mock, id, 'BUTTON', since, 2);
  // Clamped to the right edge, not wrapped around to x 256.
  expect([down.x, down.px]).toEqual([65535, 1023]);
});

test('viewers can pinch and pan locally without sending input', async ({ page, mock }) => {
  await open(page, mock, 'token=viewtoken&touch=direct');
  await waitDrawn(page, 1);
  const id = await sessionOf(page);
  await touch(page, [['down', 0, 550, 400], ['down', 1, 650, 400], ['pair', [450, 400, 750, 400], 20], ['up', 0, 450, 400], ['up', 1, 750, 400]]);
  expect(await stageScale(page)).toBeGreaterThan(2);
  const before = await canvasRect(page);
  await touch(page, [['down', 0, 600, 400], ['move', 0, 500, 350, 10], ['up', 0, 500, 350]]);
  const after = await canvasRect(page);
  expect(after.left).toBeCloseTo(before.left - 100, 3);
  expect(after.top).toBeCloseTo(before.top - 50, 3);
  await touch(page, [['down', 0, 300, 300], ['wait', 40], ['up', 0, 300, 300]]);
  await page.waitForTimeout(300);
  expect(from(mock, id, INPUT_TYPES)).toEqual([]);
});

test.describe('real touchscreen', () => {
  test.use({ hasTouch: true });

  test('a touchscreen tap is a left click in direct mode', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1&touch=direct');
    const id = await controlling(page);
    const r = await canvasRect(page);
    const since = mock.messages.length;
    await page.touchscreen.tap(640, 500);
    const msgs = await collect(mock, id, 'BUTTON', since, 2);
    expect(msgs.map((m) => [m.button, m.down, m.x, m.y])).toEqual([
      [1, true, ...norm(r, 640, 500)],
      [1, false, ...norm(r, 640, 500)],
    ]);
  });
});

test.describe('on a phone', () => {
  test.use({ hasTouch: true, viewport: { width: 390, height: 844 } });

  test('keyboard button, extra keys and the auto-hiding toolbar', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    const id = await controlling(page);
    const active = () => page.evaluate(() => document.activeElement.id);
    const keyboard = page.getByRole('button', { name: 'Keyboard' });
    await expect(keyboard).toBeVisible();
    // Control alone never pops up the soft keyboard; the Keyboard button does.
    expect(await active()).not.toBe('kbd');
    await tapToolbar(page, keyboard);
    await expect.poll(active).toBe('kbd');
    let since = mock.messages.length;
    await page.keyboard.insertText('hi');
    expect((await collect(mock, id, 'TEXT', since, 1))[0].text).toBe('hi');

    // Toolbar and extra keys keep the keyboard open.
    await tapToolbar(page, page.getByRole('button', { name: 'Keys' }));
    await expect(page.locator('#keys')).toBeVisible();
    since = mock.messages.length;
    await page.locator('#keys').getByRole('button', { name: 'Esc' }).tap();
    expect((await collect(mock, id, 'KEY', since, 2)).map((m) => m.keysym)).toEqual([0xff1b, 0xff1b]);
    expect(await active()).toBe('kbd');
    await tapToolbar(page, keyboard);
    await expect.poll(active).not.toBe('kbd');

    // The toolbar slides away after 3 s; a tap on the top edge brings it back.
    await expect(page.locator('#toolbar')).toHaveClass(/away/, { timeout: 6000 });
    await expect(page.locator('#edge')).toBeVisible();
    await page.locator('#edge').tap();
    await expect(page.locator('#toolbar')).not.toHaveClass(/away/);
    await expect(page.locator('#edge')).toBeHidden();
  });

  test('a tap on the top edge brings the toolbar back without pressing the button under it', async ({ page, mock }) => {
    // With no slide the bar is back before the tap ends, as it is part-way through the slide when
    // a real finger lifts.
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await open(page, mock, 'token=devtoken&control=1');
    await controlling(page);
    const active = () => page.evaluate(() => document.activeElement.id);
    await tapToolbar(page, page.getByRole('button', { name: 'Keyboard' }));
    await expect.poll(active).toBe('kbd');
    await expect(page.locator('#toolbar')).toHaveClass(/away/, { timeout: 6000 });
    await page.evaluate(() => {
      window.clicked = [];
      window.addEventListener('click', (e) => window.clicked.push(e.target.closest('[id]').id), true);
    });
    // Low in the 14 px edge: where the buttons are once the bar is back.
    await page.touchscreen.tap(195, 12);
    await expect(page.locator('#toolbar')).not.toHaveClass(/away/);
    // Taps click in order, so a click from the edge tap would come before this one.
    await tapToolbar(page, page.getByRole('button', { name: 'Scale' }));
    await expect.poll(() => page.evaluate(() => window.clicked)).toEqual(['btn-scale']);
    await expect(page.locator('#typer')).toBeHidden();
    // The soft keyboard stays open.
    expect(await active()).toBe('kbd');
  });

  test('the touch mode can be switched and is remembered', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    const id = await controlling(page);
    // 390x844 is a phone: trackpad by default.
    await tapToolbar(page, page.getByRole('button', { name: 'Settings' }));
    const trackpad = page.getByRole('radio', { name: 'Trackpad' });
    const direct = page.getByRole('radio', { name: 'Direct' });
    await expect(trackpad).toHaveAttribute('aria-checked', 'true');
    await direct.tap();
    await expect(direct).toHaveAttribute('aria-checked', 'true');
    await page.reload();
    await controlling(page);
    await tapToolbar(page, page.getByRole('button', { name: 'Settings' }));
    await expect(page.getByRole('radio', { name: 'Direct' })).toHaveAttribute('aria-checked', 'true');
    await page.locator('#viewport').tap({ position: { x: 300, y: 600 } });
    await expect(page.locator('#menu')).toBeHidden();
    // Direct mode: a tap clicks where the finger is.
    const r = await canvasRect(page);
    const since = mock.messages.length;
    await page.touchscreen.tap(195, 420);
    const msgs = await collect(mock, (await sessionOf(page)), 'BUTTON', since, 2);
    expect(msgs.map((m) => [m.button, m.down, m.x, m.y])).toEqual([[1, true, ...norm(r, 195, 420)], [1, false, ...norm(r, 195, 420)]]);
    expect(id).not.toBe(await sessionOf(page));
  });
});
