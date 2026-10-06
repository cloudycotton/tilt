import {
  canvasRect, collect, controlling, expect, from, INPUT_TYPES, inputLog, isApple, keyEvents, keyLog, norm, open,
  setVisibility, test,
} from './fixtures.mjs';

async function ready(page, mock, extra = '') {
  await open(page, mock, `token=devtoken&control=1${extra}`);
  return controlling(page);
}

// Playwright's macOS WebKit delivers middle-button events at bogus coordinates (a click at 1200,300
// arrived at 142,1416 on <html>), so WebKit gets the same press as synthetic pointer events.
async function middleClick(page, x, y, project) {
  if (project !== 'webkit') {
    await page.mouse.click(x, y, { button: 'middle' });
    return;
  }
  await page.evaluate(([cx, cy]) => {
    const target = document.elementFromPoint(cx, cy);
    const init = { pointerId: 1, pointerType: 'mouse', isPrimary: true, clientX: cx, clientY: cy, button: 1, bubbles: true, cancelable: true, composed: true };
    target.dispatchEvent(new PointerEvent('pointerdown', { ...init, buttons: 4 }));
    target.dispatchEvent(new PointerEvent('pointerup', { ...init, buttons: 0 }));
  }, [x, y]);
}

test('mouse moves, buttons and wheel are normalized', async ({ page, mock }, info) => {
  const id = await ready(page, mock);
  const r = await canvasRect(page);
  expect(r).toEqual({ left: 0, top: 0, width: 1280, height: 720 });

  let since = mock.messages.length;
  await page.mouse.move(320, 360);
  const [move] = await collect(mock, id, 'MOVE', since, 1);
  expect([move.x, move.y]).toEqual(norm(r, 320, 360));
  expect([move.x, move.y]).toEqual([16384, 32768]);
  expect([move.px, move.py]).toEqual([320, 360]);

  // Left drag: press, coalesced moves, release; every message carries normalized coordinates.
  since = mock.messages.length;
  await page.mouse.down();
  await page.mouse.move(640, 400, { steps: 5 });
  await page.mouse.up();
  let msgs = await collect(mock, id, ['MOVE', 'BUTTON'], since, 3);
  const buttons = msgs.filter((m) => m.type === 'BUTTON');
  expect(buttons.map((b) => [b.button, b.down, b.x, b.y])).toEqual([
    [1, true, ...norm(r, 320, 360)],
    [1, false, ...norm(r, 640, 400)],
  ]);
  const moves = msgs.slice(msgs.indexOf(buttons[0]) + 1, msgs.indexOf(buttons[1]));
  expect(moves.length).toBeGreaterThanOrEqual(1);
  expect(moves.every((m) => m.type === 'MOVE' && m.accepted)).toBe(true);
  expect([moves.at(-1).x, moves.at(-1).y]).toEqual(norm(r, 640, 400));

  // Right and middle buttons map to X buttons 3 and 2; the context menu is suppressed.
  since = mock.messages.length;
  await page.mouse.click(100, 600, { button: 'right' });
  await middleClick(page, 1200, 80, info.project.name);
  msgs = await collect(mock, id, 'BUTTON', since, 4);
  expect(msgs.map((b) => [b.button, b.down, b.x, b.y])).toEqual([
    [3, true, ...norm(r, 100, 600)],
    [3, false, ...norm(r, 100, 600)],
    [2, true, ...norm(r, 1200, 80)],
    [2, false, ...norm(r, 1200, 80)],
  ]);

  // Wheel: 50 px per notch, remainder kept, reset when the direction flips; +dy is down.
  await page.mouse.move(500, 500);
  since = mock.messages.length;
  await page.mouse.wheel(0, 120);
  let wheels = await collect(mock, id, 'WHEEL', since, 1);
  expect(wheels.map((w) => [w.dx, w.dy])).toEqual([[0, 2]]);
  expect([wheels[0].x, wheels[0].y]).toEqual(norm(r, 500, 500));
  since = mock.messages.length;
  await page.mouse.wheel(0, 30);
  wheels = await collect(mock, id, 'WHEEL', since, 1);
  expect(wheels.map((w) => [w.dx, w.dy])).toEqual([[0, 1]]);
  since = mock.messages.length;
  await page.mouse.wheel(0, -100);
  await page.mouse.wheel(120, 0);
  wheels = await collect(mock, id, 'WHEEL', since, 2);
  expect(wheels.map((w) => [w.dx, w.dy])).toEqual([[0, -2], [2, 0]]);
  expect(mock.violations).toEqual([]);
});

test.describe('letterboxed', () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test('clicks in the letterbox are ignored; drags clamp to the edge', async ({ page, mock }) => {
    const id = await ready(page, mock);
    const r = await canvasRect(page);
    expect(r).toEqual({ left: 0, top: 90, width: 1280, height: 720 });
    let since = mock.messages.length;
    await page.mouse.click(100, 860);
    await page.waitForTimeout(300);
    expect(from(mock, id, ['MOVE', 'BUTTON'], since)).toEqual([]);
    since = mock.messages.length;
    await page.mouse.move(600, 700);
    await page.mouse.down();
    await page.mouse.move(600, 880, { steps: 3 });
    await page.mouse.up();
    const buttons = await collect(mock, id, 'BUTTON', since, 2);
    expect(buttons.map((b) => [b.down, b.x, b.y])).toEqual([[true, ...norm(r, 600, 700)], [false, norm(r, 600, 880)[0], 65535]]);
  });
});

test('keys carry X keysyms', async ({ page, mock }) => {
  const id = await ready(page, mock);
  expect(await page.evaluate(() => document.activeElement.id)).toBe('kbd');
  const cases = [
    [() => page.keyboard.press('a'), ['down 61', 'up 61']],
    [() => page.keyboard.press('Shift+A'), ['down ffe1', 'down 41', 'up 41', 'up ffe1']],
    [() => page.keyboard.press('Enter'), ['down ff0d', 'up ff0d']],
    [() => page.keyboard.press('ArrowLeft'), ['down ff51', 'up ff51']],
    [() => page.keyboard.press('ArrowUp'), ['down ff52', 'up ff52']],
    [() => page.keyboard.press('ArrowRight'), ['down ff53', 'up ff53']],
    [() => page.keyboard.press('ArrowDown'), ['down ff54', 'up ff54']],
    [() => page.keyboard.press('F5'), ['down ffc2', 'up ffc2']],
    [() => page.keyboard.press('Control+c'), ['down ffe3', 'down 63', 'up 63', 'up ffe3']],
    [() => page.keyboard.press('Escape'), ['down ff1b', 'up ff1b']],
    [() => page.keyboard.press('Backspace'), ['down ff08', 'up ff08']],
    [() => page.keyboard.press('Tab'), ['down ff09', 'up ff09']],
    [() => page.keyboard.press('Digit7'), ['down 37', 'up 37']],
    [() => page.keyboard.press('Shift+Digit7'), ['down ffe1', 'down 26', 'up 26', 'up ffe1']],
    // A second keydown without keyup is autorepeat: the same keysym goes down again.
    [async () => {
      await page.keyboard.down('b');
      await page.keyboard.down('b');
      await page.keyboard.up('b');
    }, ['down 62', 'down 62', 'up 62']],
  ];
  for (const [action, expected] of cases) {
    const since = mock.messages.length;
    await action();
    const keys = await collect(mock, id, 'KEY', since, expected.length);
    expect(keyLog(keys)).toEqual(expected);
    expect(keys.every((k) => k.accepted)).toBe(true);
  }
  // Keys never leak into the hidden textarea's diff path as TEXT.
  expect(from(mock, id, 'TEXT')).toEqual([]);
  expect(mock.violations).toEqual([]);
});

test('Cmd+C is sent as Ctrl+C on Apple clients', async ({ page, mock }) => {
  const id = await ready(page, mock);
  test.skip(!(await page.evaluate(() => /Mac|iPhone|iPad/.test(navigator.platform))), 'Apple-only mapping');
  let since = mock.messages.length;
  await page.keyboard.press('Meta+c');
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ffe3', 'down 63', 'up 63', 'up ffe3']);
  // Other Cmd shortcuts stay Super shortcuts; a lone Cmd tap is a Super tap.
  since = mock.messages.length;
  await page.keyboard.press('Meta+l');
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ffeb', 'down 6c', 'up 6c', 'up ffeb']);
  since = mock.messages.length;
  await page.keyboard.press('Meta');
  expect(keyLog(await collect(mock, id, 'KEY', since, 2))).toEqual(['down ffeb', 'up ffeb']);
});

test('Cmd+click is Ctrl+click and Cmd+wheel a plain wheel, with no Super tap after', async ({ page, mock }) => {
  const id = await ready(page, mock);
  test.skip(!(await isApple(page)), 'Apple-only mapping');
  let since = mock.messages.length;
  await page.keyboard.down('Meta');
  await page.mouse.click(400, 300);
  await page.keyboard.up('Meta');
  expect(inputLog(await collect(mock, id, ['KEY', 'BUTTON', 'WHEEL'], since, 4)))
    .toEqual(['down ffe3', 'BUTTON 1 down', 'BUTTON 1 up', 'up ffe3']);
  since = mock.messages.length;
  await page.keyboard.down('Meta');
  await page.mouse.wheel(0, 120);
  await page.keyboard.up('Meta');
  expect(inputLog(await collect(mock, id, ['KEY', 'BUTTON', 'WHEEL'], since, 1, 300))).toEqual(['WHEEL 0,2']);
});

// A key pressed while a modifier is held: [type, KeyboardEventInit] pairs for keyEvents().
const OPTION = { key: 'Alt', code: 'AltLeft', altKey: true };
const CMD = { key: 'Meta', code: 'MetaLeft', metaKey: true };
function chord(mod, key, code) {
  const flags = { altKey: mod.altKey, metaKey: mod.metaKey };
  return [
    ['keydown', mod], ['keydown', { key, code, ...flags }], ['keyup', { key, code, ...flags }],
    ['keyup', { key: mod.key, code: mod.code }],
  ];
}

test('Cmd acts as Ctrl by the letter the layout types', async ({ page, mock }) => {
  const id = await ready(page, mock);
  test.skip(!(await isApple(page)), 'Apple-only mapping');
  const ctrl = (ks) => ['down ffe3', `down ${ks}`, `up ${ks}`, 'up ffe3'];
  const sup = (ks) => ['down ffeb', `down ${ks}`, `up ${ks}`, 'up ffeb'];
  for (const [layout, key, code, expected] of [
    ['QWERTZ', 'z', 'KeyY', ctrl('7a')], // undo, on the US Y key
    ['QWERTZ', 'y', 'KeyZ', sup('79')],
    ['AZERTY', 'a', 'KeyQ', ctrl('61')],
    ['Dvorak', 'v', 'Period', ctrl('76')],
    ['Dvorak', 'k', 'KeyV', sup('6b')],
    ['Russian', 'с', 'KeyC', ctrl('63')], // no Latin letter: the US key decides, as for the keysym
  ]) {
    const since = mock.messages.length;
    await keyEvents(page, chord(CMD, key, code));
    expect(keyLog(await collect(mock, id, 'KEY', since, 4)), `${layout} Cmd+${key}`).toEqual(expected);
  }
});

test('Option types the characters it composes on Apple clients; other Option chords are Alt', async ({ page, mock }) => {
  const id = await ready(page, mock);
  test.skip(!(await isApple(page)), 'Apple-only mapping');
  for (const [what, events, expected] of [
    // German layout: Option+L is @ and Option+5 is [. US layout: Option+c is ç.
    ['German Option+L', chord(OPTION, '@', 'KeyL'), ['down 40', 'up 40']],
    ['German Option+5', chord(OPTION, '[', 'Digit5'), ['down 5b', 'up 5b']],
    ['US Option+c', chord(OPTION, 'ç', 'KeyC'), ['down e7', 'up e7']],
    ['Option+Left', chord(OPTION, 'ArrowLeft', 'ArrowLeft'), ['down ffe9', 'down ff51', 'up ff51', 'up ffe9']],
    ['lone Option', [['keydown', OPTION], ['keyup', { key: 'Alt', code: 'AltLeft' }]], ['down ffe9', 'up ffe9']],
  ]) {
    const since = mock.messages.length;
    await keyEvents(page, events);
    expect(keyLog(await collect(mock, id, 'KEY', since, expected.length)), what).toEqual(expected);
  }
  // Option+click is Alt+click (moving a window on X).
  const since = mock.messages.length;
  await keyEvents(page, [['keydown', OPTION]]);
  await page.mouse.click(400, 300);
  await keyEvents(page, [['keyup', { key: 'Alt', code: 'AltLeft' }]]);
  expect(inputLog(await collect(mock, id, ['KEY', 'BUTTON'], since, 4)))
    .toEqual(['down ffe9', 'BUTTON 1 down', 'BUTTON 1 up', 'up ffe9']);
});

test('a key released after focus left the viewer is released on the remote', async ({ page, mock }) => {
  const id = await ready(page, mock);
  const since = mock.messages.length;
  await page.keyboard.down('Shift');
  await page.evaluate(() => document.getElementById('btn-type').click());
  await expect(page.locator('#typer-text')).toBeFocused();
  await page.keyboard.up('Shift');
  await page.keyboard.type('abc');
  await page.evaluate(() => document.getElementById('typer-form').requestSubmit());
  expect(inputLog(await collect(mock, id, ['KEY', 'TEXT'], since, 3))).toEqual(['down ffe1', 'up ffe1', 'TEXT abc']);
});

test('Japanese keys: Convert is Henkan_Mode, Eisu is Eisu_toggle', async ({ page, mock }) => {
  const id = await ready(page, mock);
  const since = mock.messages.length;
  await keyEvents(page, [
    ['keydown', { key: 'Convert', code: 'Convert' }], ['keyup', { key: 'Convert', code: 'Convert' }],
    ['keydown', { key: 'Eisu', code: 'Lang2' }], ['keyup', { key: 'Eisu', code: 'Lang2' }],
  ]);
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ff23', 'up ff23', 'down ff30', 'up ff30']);
});

test.describe('on Windows', () => {
  test.beforeEach(async ({ page }) => {
    await page.addInitScript(() => Object.defineProperty(Navigator.prototype, 'platform', { get: () => 'Win32' }));
  });

  test('AltGr held past the repeat delay never presses Ctrl', async ({ page, mock }) => {
    const id = await ready(page, mock);
    // Windows sends a fake ControlLeft with the same timestamp before each AltRight press,
    // autorepeats included.
    const altGr = (repeat) => page.evaluate((rep) => {
      const ta = document.getElementById('kbd');
      const g = { ctrlKey: true, altKey: true, modifierAltGraph: true, repeat: rep, bubbles: true, cancelable: true };
      const ts = performance.now();
      for (const init of [{ key: 'Control', code: 'ControlLeft', ...g }, { key: 'AltGraph', code: 'AltRight', ...g }]) {
        const e = new KeyboardEvent('keydown', init);
        Object.defineProperty(e, 'timeStamp', { value: ts });
        ta.dispatchEvent(e);
      }
    }, repeat);
    const g = { ctrlKey: true, altKey: true, modifierAltGraph: true };
    let since = mock.messages.length;
    await altGr(false);
    await page.waitForTimeout(600);
    await altGr(true);
    await keyEvents(page, [
      ['keydown', { key: '{', code: 'Digit7', ...g }], ['keyup', { key: '{', code: 'Digit7', ...g }],
      ['keyup', { key: 'Control', code: 'ControlLeft', altKey: true }], ['keyup', { key: 'AltGraph', code: 'AltRight' }],
    ]);
    expect(keyLog(await collect(mock, id, 'KEY', since, 5))).toEqual(['down fe03', 'down fe03', 'down 7b', 'up 7b', 'up fe03']);
    // A real left Ctrl, held long enough to autorepeat, still works.
    since = mock.messages.length;
    await keyEvents(page, [['keydown', { key: 'Control', code: 'ControlLeft', ctrlKey: true }]]);
    await page.waitForTimeout(600);
    await keyEvents(page, [
      ['keydown', { key: 'Control', code: 'ControlLeft', ctrlKey: true, repeat: true }],
      ['keydown', { key: 'c', code: 'KeyC', ctrlKey: true }], ['keyup', { key: 'c', code: 'KeyC', ctrlKey: true }],
      ['keyup', { key: 'Control', code: 'ControlLeft' }],
    ]);
    expect(keyLog(await collect(mock, id, 'KEY', since, 5))).toEqual(['down ffe3', 'down ffe3', 'down 63', 'up 63', 'up ffe3']);
  });
});

test.describe('on iPadOS', () => {
  // A Mac platform with touch points.
  test.beforeEach(async ({ page }) => {
    await page.addInitScript(() => {
      Object.defineProperty(Navigator.prototype, 'platform', { get: () => 'MacIntel', configurable: true });
      Object.defineProperty(Navigator.prototype, 'maxTouchPoints', { get: () => 5, configurable: true });
    });
  });

  test('a key pressed under Ctrl whose keyup never comes goes up with Ctrl', async ({ page, mock }) => {
    const id = await ready(page, mock);
    const since = mock.messages.length;
    // Ctrl+Z as the iPad simulator sometimes delivers it: no keyup for the Z.
    await keyEvents(page, [
      ['keydown', { key: 'Control', code: 'ControlLeft', ctrlKey: true }],
      ['keydown', { key: 'z', code: 'KeyZ', ctrlKey: true }],
      ['keyup', { key: 'Control', code: 'ControlLeft' }],
    ]);
    const ctrlZ = ['down ffe3', 'down 7a', 'up 7a', 'up ffe3'];
    expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(ctrlZ);
    // Should the keyup come after all, it sends nothing.
    await keyEvents(page, [['keyup', { key: 'z', code: 'KeyZ' }]]);
    expect(keyLog(await collect(mock, id, 'KEY', since, 4, 300))).toEqual(ctrlZ);
  });
});

test('a key still held when Ctrl goes up stays down until its own keyup', async ({ page, mock }) => {
  const id = await ready(page, mock);
  const since = mock.messages.length;
  await page.keyboard.down('Control');
  await page.keyboard.down('z');
  await page.keyboard.up('Control');
  expect(keyLog(await collect(mock, id, 'KEY', since, 3))).toEqual(['down ffe3', 'down 7a', 'up ffe3']);
  await page.keyboard.up('z');
  expect(keyLog(await collect(mock, id, 'KEY', since, 4))).toEqual(['down ffe3', 'down 7a', 'up ffe3', 'up 7a']);
});

test('RELEASE_ALL is sent when the window loses focus or the page is hidden', async ({ page, mock }) => {
  const id = await ready(page, mock);
  let since = mock.messages.length;
  await page.keyboard.down('Shift');
  await page.mouse.move(400, 400);
  await page.mouse.down();
  await collect(mock, id, ['KEY', 'BUTTON'], since, 2, 0);
  since = mock.messages.length;
  await page.evaluate(() => window.dispatchEvent(new Event('blur')));
  const [ra] = await collect(mock, id, 'RELEASE_ALL', since, 1);
  expect(ra.accepted).toBe(true);
  // Everything was forgotten locally: the physical releases send nothing further.
  await page.keyboard.up('Shift');
  await page.mouse.up();
  await page.waitForTimeout(300);
  expect(from(mock, id, ['KEY', 'BUTTON'], since)).toEqual([]);

  await page.keyboard.down('a');
  await collect(mock, id, 'KEY', since, 1, 0);
  since = mock.messages.length;
  await setVisibility(page, 'hidden');
  await collect(mock, id, 'RELEASE_ALL', since, 1, 0);
  await mock.waitFor((m) => m.session === id && m.type === 'video' && m.on === false, { since });
});

test('with nothing down, losing focus or hiding sends no RELEASE_ALL, which would stop a paste', async ({ page, mock }) => {
  const id = await ready(page, mock);
  const since = mock.messages.length;
  const types = ['KEY', 'BUTTON', 'TEXT', 'RELEASE_ALL'];
  await page.keyboard.press('a');
  await page.mouse.click(400, 400);
  await page.keyboard.insertText('pasted');
  await collect(mock, id, types, since, 5, 0);
  await page.evaluate(() => window.dispatchEvent(new Event('blur')));
  await setVisibility(page, 'hidden');
  await mock.waitFor((m) => m.session === id && m.type === 'video' && m.on === false, { since });
  await page.waitForTimeout(300);
  expect(inputLog(from(mock, id, types, since))).toEqual(['down 61', 'up 61', 'BUTTON 1 down', 'BUTTON 1 up', 'TEXT pasted']);
});

test('insertText arrives as one TEXT message', async ({ page, mock }) => {
  const id = await ready(page, mock);
  const since = mock.messages.length;
  const text = 'héllo wörld 日本 \u{1D11E}';
  await page.keyboard.insertText(text);
  const [msg] = await collect(mock, id, 'TEXT', since, 1);
  expect(msg.text).toBe(text);
  expect(from(mock, id, 'KEY', since)).toEqual([]);
});

/**
 * Edits the hidden textarea the way a soft keyboard does. Steps: [inputType, 'append', text],
 * [inputType, 'chop', [n, text]] (the last n characters become text) or [inputType, 'set', value].
 */
async function edit(page, steps) {
  await page.evaluate((list) => {
    const ta = document.getElementById('kbd');
    for (const [inputType, op, arg] of list) {
      ta.value = op === 'append' ? ta.value + arg : op === 'chop' ? ta.value.slice(0, -arg[0]) + arg[1] : arg;
      ta.dispatchEvent(new InputEvent('input', { inputType, bubbles: true }));
    }
  }, steps);
}

test('textarea edits become BackSpace taps plus TEXT', async ({ page, mock }) => {
  const id = await ready(page, mock);
  const since = mock.messages.length;
  // Typing "teh", then what a soft keyboard's autocorrect does: replace the word before the caret.
  await edit(page, [['insertText', 'append', 'teh'], ['insertReplacementText', 'chop', [3, 'the']]]);
  expect(inputLog(await collect(mock, id, ['KEY', 'TEXT'], since, 6)))
    .toEqual(['TEXT teh', 'down ff08', 'up ff08', 'down ff08', 'up ff08', 'TEXT he']);
});

test('a suggestion that takes the padding for part of the word deletes only the word', async ({ page, mock }) => {
  const id = await ready(page, mock);
  let since = mock.messages.length;
  // "hel", then a suggestion replacing everything before the caret with "hello".
  await edit(page, [['insertText', 'append', 'hel'], ['insertReplacementText', 'set', 'hello']]);
  expect(inputLog(await collect(mock, id, ['KEY', 'TEXT'], since, 8)))
    .toEqual(['TEXT hel', 'down ff08', 'up ff08', 'down ff08', 'up ff08', 'down ff08', 'up ff08', 'TEXT hello']);
  // A backspace with nothing typed still deletes on the remote: that is what the padding is for.
  since = mock.messages.length;
  await edit(page, [['deleteContentBackward', 'chop', [1, '']]]);
  expect(inputLog(await collect(mock, id, ['KEY', 'TEXT'], since, 2))).toEqual(['down ff08', 'up ff08']);
});

test('IME composition is sent once, at compositionend', async ({ page, mock }) => {
  const id = await ready(page, mock);
  const since = mock.messages.length;
  await page.evaluate(async () => {
    const ta = document.getElementById('kbd');
    const base = ta.value;
    const frame = () => new Promise((r) => setTimeout(r, 20));
    ta.dispatchEvent(new CompositionEvent('compositionstart', { data: '' }));
    for (const step of ['に', 'にほ', 'にほん']) {
      ta.value = base + step;
      ta.dispatchEvent(new InputEvent('input', { inputType: 'insertCompositionText', data: step, isComposing: true, bubbles: true }));
      await frame();
    }
    ta.value = base + '日本';
    ta.dispatchEvent(new InputEvent('input', { inputType: 'insertCompositionText', data: '日本', isComposing: true, bubbles: true }));
    ta.dispatchEvent(new CompositionEvent('compositionend', { data: '日本' }));
  });
  const msgs = await collect(mock, id, ['KEY', 'TEXT'], since, 1, 300);
  expect(msgs.map((m) => m.type + (m.text ? ` ${m.text}` : ''))).toEqual(['TEXT 日本']);
});

test('real IME composition through Chrome DevTools', async ({ page, mock }, info) => {
  test.skip(info.project.name !== 'chrome', 'needs CDP Input.imeSetComposition');
  const id = await ready(page, mock);
  const cdp = await page.context().newCDPSession(page);
  const since = mock.messages.length;
  await cdp.send('Input.imeSetComposition', { text: 'に', selectionStart: 1, selectionEnd: 1 });
  await cdp.send('Input.imeSetComposition', { text: 'にほ', selectionStart: 2, selectionEnd: 2 });
  await page.waitForTimeout(100);
  expect(from(mock, id, ['KEY', 'TEXT'], since)).toEqual([]);
  await cdp.send('Input.insertText', { text: '日本' });
  const msgs = await collect(mock, id, ['KEY', 'TEXT'], since, 1, 300);
  expect(msgs.map((m) => m.type + (m.text ? ` ${m.text}` : ''))).toEqual(['TEXT 日本']);
});

test('no input is sent before control is taken', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken');
  await page.waitForFunction(() => window.tilt.stats.framesDrawn > 0);
  const id = await page.evaluate(() => window.tilt.stats.session);
  await page.mouse.click(300, 300);
  await page.keyboard.press('a');
  await page.mouse.wheel(0, 200);
  await page.waitForTimeout(300);
  expect(from(mock, id, INPUT_TYPES)).toEqual([]);
  // Taking control turns input on.
  await page.evaluate(() => window.tilt.takeControl());
  await page.waitForFunction(() => window.tilt.stats.control);
  const since = mock.messages.length;
  await page.keyboard.press('a');
  expect(keyLog(await collect(mock, id, 'KEY', since, 2))).toEqual(['down 61', 'up 61']);
  expect(mock.holder()).toBe(id);
});
