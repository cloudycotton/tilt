import { expect, from, open, sessionOf, test } from './fixtures.mjs';

test.describe('with --resize', () => {
  test.use({ mockOptions: { resize: true } });

  test('a control viewer sends its window size, then each new size once it settles', async ({ page, mock }) => {
    await page.setViewportSize({ width: 600, height: 900 });
    await open(page, mock);
    await mock.waitFor((m) => m.type === 'welcomed');
    const id = await sessionOf(page);
    await mock.waitFor((m) => m.session === id && m.type === 'resize');
    await page.setViewportSize({ width: 700, height: 800 });
    await page.setViewportSize({ width: 1100, height: 500 });
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
