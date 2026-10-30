import { expect, from, open, sessionOf, test } from './fixtures.mjs';

/**
 * Runs in the page: while `settle.hold` is on, the client's 300 ms resize-settle timers wait for
 * `settle.release()` instead of the clock. `settle.seen` lists the widths of resize events since.
 */
function holdSettleTimer() {
  const { setTimeout: set, clearTimeout: clear } = window;
  const held = new Map();
  const settle = {
    hold: false,
    seen: [],
    release() {
      settle.hold = false;
      const due = [...held.values()];
      held.clear();
      for (const fn of due) fn();
    },
  };
  window.settle = settle;
  addEventListener('resize', () => { if (settle.hold) settle.seen.push(innerWidth); });
  window.setTimeout = (fn, ms, ...args) => {
    if (!settle.hold || ms !== 300) return set(fn, ms, ...args);
    const id = set(() => {}, 2 ** 31 - 1);
    held.set(id, () => fn(...args));
    return id;
  };
  window.clearTimeout = (id) => {
    held.delete(id);
    clear(id);
  };
}

test.describe('with --resize', () => {
  test.use({ mockOptions: { resize: true } });

  test('a control viewer sends its window size, then each new size once it settles', async ({ page, mock }) => {
    await page.setViewportSize({ width: 600, height: 900 });
    await page.addInitScript(holdSettleTimer);
    await open(page, mock);
    await mock.waitFor((m) => m.type === 'welcomed');
    const id = await sessionOf(page);
    await mock.waitFor((m) => m.session === id && m.type === 'resize');
    // Playwright's WebKit waits 500 ms after each viewport change on Ubuntu 22.04 and Debian 12,
    // longer than a size must hold: the settle timer is held meanwhile, so 700x800 is a size
    // that did not hold however long the resizes take.
    await page.evaluate(() => { window.settle.hold = true; });
    for (const [width, height] of [[700, 800], [1100, 500]]) {
      await page.setViewportSize({ width, height });
      // Chrome folds quick changes into one resize event: each size must have reached the page.
      await page.waitForFunction((w) => window.settle.seen.at(-1) === w, width);
    }
    await page.evaluate(() => window.settle.release());
    await mock.waitFor((m) => m.session === id && m.type === 'resize' && m.w === 1100);
    await page.waitForTimeout(500);
    const sizes = from(mock, id, ['resize']).map((m) => [m.w, m.h]);
    expect(sizes).toEqual([[600, 900], [1100, 500]]);
    expect(mock.violations).toEqual([]);
  });

  test('a view-only viewer never sends one', async ({ page, mock }) => {
    await page.setViewportSize({ width: 600, height: 900 });
    await open(page, mock, 'token=viewtoken');
    await mock.waitFor((m) => m.type === 'welcomed');
    await page.setViewportSize({ width: 700, height: 800 });
    await page.waitForTimeout(500);
    expect(from(mock, await sessionOf(page), ['resize'])).toEqual([]);
  });
});

test('without --resize nothing is sent', async ({ page, mock }) => {
  await open(page, mock);
  await mock.waitFor((m) => m.type === 'welcomed');
  await page.setViewportSize({ width: 700, height: 800 });
  await page.waitForTimeout(500);
  expect(from(mock, await sessionOf(page), ['resize'])).toEqual([]);
});
