// iOS hardware-keyboard dead keys, replayed from what WKWebView and Mobile Safari deliver on iOS
// 26.5 (recorded with XCUITest on the simulators). Option+E then E types é. iOS sends no keydown for
// the dead key itself, only a composition, releases Option at some point in it, and then either
// commits é from the composition or cancels it and sends é as a plain keydown whose keyup can be
// missing. Sometimes there is no composition at all: Option goes up with nothing typed, and é comes
// as a plain keydown, so on iOS a lone Option tap (Alt) waits for the next key.
import { canvasRect, collect, controlling, expect, from, inputLog, isApple, open, test } from './fixtures.mjs';

const ALT = { key: 'Alt', code: 'AltLeft', keyCode: 18, altKey: true };
const ALT_UP = { key: 'Alt', code: 'AltLeft', keyCode: 18 };
const SHIFT = { key: 'Shift', code: 'ShiftLeft', keyCode: 16, shiftKey: true };
const SHIFT_UP = { key: 'Shift', code: 'ShiftLeft', keyCode: 16 };
const E_ACUTE = { key: 'é', code: 'KeyE', keyCode: 69 };
const E_ACUTE_CAP = { key: 'É', code: 'KeyE', keyCode: 69, shiftKey: true };
// The client's LONE_OPTION_MS, and a margin: a lone Alt tap held back would have come by then.
const LONE_OPTION_WAIT_MS = 2000;

/** Dispatches [kind, init] steps on the hidden textarea: key events, composition events, edits. */
async function replay(page, steps) {
  await page.evaluate((list) => {
    const ta = document.getElementById('kbd');
    const base = ta.value;
    for (const [kind, init] of list) {
      if (kind === 'keydown' || kind === 'keyup') {
        ta.dispatchEvent(new KeyboardEvent(kind, { bubbles: true, cancelable: true, composed: true, ...init }));
      } else if (kind.startsWith('composition')) {
        ta.dispatchEvent(new CompositionEvent(kind, { bubbles: true, data: init.data }));
      } else {
        // An edit: the textarea's text after the padding, and the input event iOS sends for it.
        ta.value = base + init.text;
        ta.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: kind, data: init.data ?? null, isComposing: true }));
      }
    }
  }, steps);
}

const compose = [
  ['compositionstart', { data: '' }],
  ['compositionupdate', { data: '´' }],
  ['insertCompositionText', { text: '´', data: '´' }],
];
const commit = [
  ['keydown', { ...E_ACUTE, isComposing: true }],
  ['deleteCompositionText', { text: '' }],
  ['insertFromComposition', { text: 'é', data: 'é' }],
  ['compositionend', { data: 'é' }],
  ['keyup', E_ACUTE],
];
const cancel = [
  ['deleteCompositionText', { text: '' }],
  ['compositionend', { data: '' }],
];

test.describe('iOS', () => {
  // An iPhone: Apple, with touch points (iPadOS reports a Mac platform and is told apart by those).
  test.beforeEach(async ({ page }) => {
    await page.addInitScript(() => {
      Object.defineProperty(Navigator.prototype, 'platform', { get: () => 'iPhone', configurable: true });
      Object.defineProperty(Navigator.prototype, 'maxTouchPoints', { get: () => 5, configurable: true });
    });
  });

  for (const [what, steps, expected] of [
    ['composed: Option up during the composition, é committed from it', [
      ['keydown', ALT], ...compose, ['keyup', { ...ALT_UP, isComposing: true }], ...commit,
    ], ['TEXT é']],
    // The composition can start 150 ms after Option's keydown: a quick Option+E releases Option first.
    ['composed: Option up before the composition starts', [
      ['keydown', ALT], ['keyup', ALT_UP], ...compose, ...commit,
    ], ['TEXT é']],
    ['cancelled: Option up during the composition, é as a keydown with no keyup', [
      ['keydown', ALT], ...compose, ['keyup', { ...ALT_UP, isComposing: true }], ...cancel, ['keydown', E_ACUTE],
    ], ['down e9', 'up e9']],
    ['cancelled: Option up after the composition, é as a keydown and keyup', [
      ['keydown', ALT], ...compose, ...cancel, ['keyup', ALT_UP], ['keydown', E_ACUTE], ['keyup', E_ACUTE],
    ], ['down e9', 'up e9']],
    ['cancelled: Option up before the composition starts', [
      ['keydown', ALT], ['keyup', ALT_UP], ...compose, ...cancel, ['keydown', E_ACUTE],
    ], ['down e9', 'up e9']],
    ['no composition: Option up with nothing typed, é as a keydown and keyup', [
      ['keydown', ALT], ['keyup', ALT_UP], ['keydown', E_ACUTE], ['keyup', E_ACUTE],
    ], ['down e9', 'up e9']],
    ['no composition: Option up with nothing typed, é as a keydown with no keyup', [
      ['keydown', ALT], ['keyup', ALT_UP], ['keydown', E_ACUTE],
    ], ['down e9', 'up e9']],
    ['no composition, Shift+E: Shift first, then É', [
      ['keydown', ALT], ['keyup', ALT_UP], ['keydown', SHIFT], ['keydown', E_ACUTE_CAP], ['keyup', E_ACUTE_CAP], ['keyup', SHIFT_UP],
    ], ['down ffe1', 'down c9', 'up c9', 'up ffe1']],
  ]) {
    test(`dead key Option+E, E (${what}) types the letter once, with no Alt and nothing left held`, async ({ page, mock }) => {
      await open(page, mock, 'token=devtoken&control=1');
      const id = await controlling(page);
      const since = mock.messages.length;
      await replay(page, steps);
      const msgs = await collect(mock, id, ['KEY', 'TEXT'], since, expected.length, LONE_OPTION_WAIT_MS);
      expect(inputLog(msgs)).toEqual(expected);
    });
  }

  test('only the key right after a cancelled composition is typed at once; the next is held', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    const id = await controlling(page);
    const since = mock.messages.length;
    await replay(page, [...compose, ...cancel, ['keydown', E_ACUTE], ['keyup', E_ACUTE]]);
    await page.keyboard.down('a');
    await expect.poll(() => inputLog(from(mock, id, ['KEY', 'TEXT'], since))).toEqual(['down e9', 'up e9', 'down 61']);
    await page.keyboard.up('a');
    await expect.poll(() => inputLog(from(mock, id, ['KEY', 'TEXT'], since))).toEqual(['down e9', 'up e9', 'down 61', 'up 61']);
  });

  test('a key after a composition that ended with text is held until its keyup', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    const id = await controlling(page);
    const since = mock.messages.length;
    await replay(page, [
      ['compositionstart', { data: '' }],
      ['insertCompositionText', { text: 'に', data: 'に' }],
      ['compositionend', { data: 'に' }],
    ]);
    await page.keyboard.down('a');
    await expect.poll(() => inputLog(from(mock, id, ['KEY', 'TEXT'], since))).toEqual(['TEXT に', 'down 61']);
    await page.keyboard.up('a');
    await expect.poll(() => inputLog(from(mock, id, ['KEY', 'TEXT'], since))).toEqual(['TEXT に', 'down 61', 'up 61']);
  });

  test('a lone Option tap is Alt: sent before the next key, or on its own after a pause', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    const id = await controlling(page);
    let since = mock.messages.length;
    await replay(page, [['keydown', ALT], ['keyup', ALT_UP], ['keydown', { key: 'f', code: 'KeyF', keyCode: 70 }]]);
    await expect.poll(() => inputLog(from(mock, id, ['KEY', 'TEXT'], since))).toEqual(['down ffe9', 'up ffe9', 'down 66']);
    await replay(page, [['keyup', { key: 'f', code: 'KeyF', keyCode: 70 }]]);
    since = mock.messages.length;
    await replay(page, [['keydown', ALT], ['keyup', ALT_UP]]);
    await expect.poll(() => inputLog(from(mock, id, ['KEY', 'TEXT'], since)), { timeout: 5000 }).toEqual(['down ffe9', 'up ffe9']);
  });

  test('a click right after a lone Option tap comes after the Alt tap', async ({ page, mock }) => {
    await open(page, mock, 'token=devtoken&control=1');
    const id = await controlling(page);
    const r = await canvasRect(page);
    const since = mock.messages.length;
    await replay(page, [['keydown', ALT], ['keyup', ALT_UP]]);
    await page.mouse.click(r.left + r.width / 2, r.top + r.height / 2);
    await expect.poll(() => inputLog(from(mock, id, ['KEY', 'BUTTON'], since))).toEqual(['down ffe9', 'up ffe9', 'BUTTON 1 down', 'BUTTON 1 up']);
  });
});

// A Mac keyboard on a desktop browser (no touch points) keeps the plain behaviour.
test('desktop: a lone Option tap is Alt at once', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  test.skip(!(await isApple(page)), 'Option is held back on Apple clients only');
  const since = mock.messages.length;
  await replay(page, [['keydown', ALT], ['keyup', ALT_UP]]);
  // Well before the iOS client's LONE_OPTION_MS.
  await expect.poll(() => inputLog(from(mock, id, ['KEY', 'TEXT'], since)), { timeout: 1000 }).toEqual(['down ffe9', 'up ffe9']);
});

test('desktop: a key after a cancelled composition is held until its keyup', async ({ page, mock }) => {
  await open(page, mock, 'token=devtoken&control=1');
  const id = await controlling(page);
  const since = mock.messages.length;
  await replay(page, [...compose, ...cancel]);
  await page.keyboard.down('a');
  await expect.poll(() => inputLog(from(mock, id, ['KEY', 'TEXT'], since))).toEqual(['down 61']);
  await page.keyboard.up('a');
  await expect.poll(() => inputLog(from(mock, id, ['KEY', 'TEXT'], since))).toEqual(['down 61', 'up 61']);
});
