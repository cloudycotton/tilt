// The WebTransport fast path (docs/protocol.md section 7), against a running tilt:
// a browser that has WebTransport streams and sends input over UDP, falls back to the WebSocket
// when UDP does not get through, and keeps to the WebSocket when told to. Skipped when the
// server offers no fast path.
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
  test.skip(!(await advert(request)), 'the server offers no WebTransport');
  test.skip(!(await hasWebTransport(page)), 'this browser has no WebTransport');
  await openTilt(page, `token=${CONTROL_TOKEN}&control=1&probe=${MARKER}`);
  expect(await page.evaluate(() => window.tilt.stats.transport)).toBe('udp');
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
  expect(await page.evaluate(() => window.tilt.stats.transport)).toBe('udp');
});

test('falls back to the WebSocket when UDP does not get through, and remembers', async ({ page, request }) => {
  const wt = await advert(request);
  test.skip(!wt, 'the server offers no WebTransport');
  test.skip(!(await hasWebTransport(page)), 'this browser has no WebTransport');
  // As if the UDP port were blocked: the advert names a port nothing listens on.
  await page.route('**/transport', (route) => route.fulfill({ json: { wt: { ...wt, port: 9 } } }));
  const failed = () => page.evaluate(() => JSON.parse(localStorage.getItem('tilt.wtFailed'))?.n ?? 0);
  const started = Date.now();
  await openTilt(page, `token=${CONTROL_TOKEN}`);
  expect(await page.evaluate(() => window.tilt.stats.transport)).toBe('ws');
  expect(Date.now() - started, 'first frame despite the failed attempt (ms)').toBeLessThan(8_000);
  expect(await failed()).toBe(1);
  // The second failure in a row is remembered: later connections go straight to the WebSocket.
  for (const n of [2, 2]) {
    await page.evaluate(() => window.tilt.reconnect());
    await page.waitForFunction(() => window.tilt.ws instanceof WebSocket && window.tilt.ws.readyState === 1);
    expect(await failed()).toBe(n);
  }
});

test('#transport=ws keeps to the WebSocket', async ({ page, request }) => {
  test.skip(!(await advert(request)), 'the server offers no WebTransport');
  await openTilt(page, `token=${CONTROL_TOKEN}&transport=ws`);
  expect(await page.evaluate(() => window.tilt.stats.transport)).toBe('ws');
});
