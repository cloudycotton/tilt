// The stream over WebTransport (docs/protocol.md) against a running tilt: video arrives on its
// streams and input goes out on the control stream. Skipped in browsers without WebTransport.
import { expect, test } from '@playwright/test';
import { CONTROL_TOKEN, MARKER, markerColor, openTilt, waitForControl, waitForMarker } from './helpers.mjs';

const TRIAL_TIMEOUT_MS = 2_000;

/** The server's WebTransport advert, or null. */
async function advert(request) {
  const r = await request.get('./transport');
  return r.ok() ? (await r.json()).wt ?? null : null;
}

/** Whether the browser offers WebTransport to tilt's origin (it needs a secure context). */
async function hasWebTransport(page) {
  await page.goto('./healthz');
  return page.evaluate(() => typeof WebTransport === 'function');
}

test.afterEach(async ({ page }) => {
  const s = await page.evaluate(() => ({ ...window.tilt?.stats })).catch(() => ({}));
  expect(s.decodeErrors ?? 0, 'tilt.stats.decodeErrors').toBe(0);
});

test('streams and takes input over WebTransport', async ({ page, request }) => {
  test.skip(!(await hasWebTransport(page)), 'this browser has no WebTransport');
  expect(await advert(request), 'GET /transport').toBeTruthy();
  await openTilt(page, `token=${CONTROL_TOKEN}&remote=1&probe=${MARKER}`);
  await waitForControl(page);
  if (!(await page.evaluate(() => document.activeElement?.tagName === 'TEXTAREA'))) {
    await page.locator('textarea').first().focus();
  }
  // Keys go out on the control stream, and the frames showing them come back on video streams.
  let color = await markerColor(page);
  expect(color, 'marker colour (is tilt-testpattern running?)').toBeGreaterThanOrEqual(0);
  for (let i = 0; i < 5; i++) {
    await page.keyboard.down('a');
    const sentAt = await page.evaluate(() => window.tilt.lastInputAt);
    await page.keyboard.up('a');
    const next = (color + 1) % 4;
    expect(await waitForMarker(page, next, sentAt, TRIAL_TIMEOUT_MS), `marker ${next} drawn`).not.toBeNull();
    color = next;
  }
  // PINGs are answered over it too.
  await page.waitForFunction(() => window.tilt.stats.rttMs !== null, null, { timeout: 5_000 });
});
