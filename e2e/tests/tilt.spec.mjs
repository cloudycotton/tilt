// Browser checks of brief section 8 against tilt serving tilt-testpattern (static, not animated).
import { expect, test } from '@playwright/test';
import {
  CONTROL_TOKEN,
  EXPECT_SCREEN,
  MARKER,
  VIEW_TOKEN,
  markerColor,
  openTilt,
  percentile,
  round1,
  screenToClient,
  waitForControl,
  waitForMarker,
  writeResult,
} from './helpers.mjs';

const LATENCY_TRIALS = 20;
const LATENCY_P95_MAX_MS = 150;
const TRIAL_TIMEOUT_MS = 2_000;
const TRIAL_PAUSE_MS = 100;
const RESUME_TIMEOUT_MS = 5_000;

// Brief 9: no decode errors in any run. Nor decoder stalls: a decoder that outputs every frame as
// it comes never needs the client's flush watchdog.
test.afterEach(async ({ page }) => {
  const s = await page.evaluate(() => ({ ...window.tilt?.stats })).catch(() => ({}));
  expect(s.decodeErrors ?? 0, 'tilt.stats.decodeErrors').toBe(0);
  expect(s.decoderStalls ?? 0, 'tilt.stats.decoderStalls').toBe(0);
});

test('stream renders at the screen size', async ({ page }) => {
  await openTilt(page, `token=${CONTROL_TOKEN}&stats=1`);
  // A new viewer gets a keyframe plus the refinement tail, even from a static screen; the tail
  // ends as soon as the encoder has nothing left to refine, after a few frames.
  await page.waitForFunction(() => window.tilt.stats.framesDecoded >= 2, null, { timeout: 10_000 });
  const s = await page.evaluate(() => {
    const canvas = document.getElementById('screen');
    return { ...window.tilt.stats, canvas: [canvas.width, canvas.height] };
  });
  expect(s.canvas, 'canvas size').toEqual(EXPECT_SCREEN);
  expect([s.width, s.height], 'tilt.stats width x height').toEqual(EXPECT_SCREEN);
  expect(await markerColor(page), 'marker colour (is tilt-testpattern running?)').toBeGreaterThanOrEqual(0);
});

test('key press to drawn marker latency', async ({ page }, testInfo) => {
  await openTilt(page, `token=${CONTROL_TOKEN}&control=1&probe=${MARKER}`);
  await waitForControl(page);
  // The client focuses its hidden textarea when control arrives; make sure nothing took it.
  if (!(await page.evaluate(() => document.activeElement?.tagName === 'TEXTAREA'))) {
    await page.locator('textarea').first().focus();
  }
  let color = await markerColor(page);
  expect(color, 'marker colour (is tilt-testpattern running?)').toBeGreaterThanOrEqual(0);

  const samples = [];
  let lastSentAt = 0;
  for (let i = 0; i < LATENCY_TRIALS && color >= 0; i++) {
    const next = (color + 1) % 4;
    // lastInputAt is read between down and up, so it is when the KEY down went out.
    await page.keyboard.down('a');
    const sentAt = await page.evaluate(() => window.tilt.lastInputAt);
    await page.keyboard.up('a');
    expect(sentAt, 'tilt.lastInputAt advances when a key is sent').toBeGreaterThan(lastSentAt);
    lastSentAt = sentAt;
    const drawnAt = await waitForMarker(page, next, sentAt, TRIAL_TIMEOUT_MS);
    if (drawnAt === null) {
      // Resynchronise; a marker that is no longer recognisable ends the run.
      color = await markerColor(page);
    } else {
      samples.push(drawnAt - sentAt);
      color = next;
    }
    await page.waitForTimeout(TRIAL_PAUSE_MS);
  }

  const sorted = [...samples].sort((a, b) => a - b);
  const result = {
    trials: LATENCY_TRIALS,
    n: samples.length,
    failed: LATENCY_TRIALS - samples.length,
    p50: round1(percentile(sorted, 0.5)),
    p95: round1(percentile(sorted, 0.95)),
    max: round1(percentile(sorted, 1)),
    samples_ms: samples.map(round1),
  };
  writeResult(testInfo, 'latency', result);
  console.log(`${testInfo.project.name}: key->drawn p50 ${result.p50} ms, p95 ${result.p95} ms, n ${result.n}`);
  expect(result.failed, 'trials without a marker change').toBe(0);
  expect(result.p95, 'key->drawn p95 (ms)').toBeLessThan(LATENCY_P95_MAX_MS);
});

test('click on the marker advances it', async ({ page }, testInfo) => {
  await openTilt(page, `token=${CONTROL_TOKEN}&control=1&probe=${MARKER}`);
  await waitForControl(page);
  const before = await markerColor(page);
  expect(before, 'marker colour (is tilt-testpattern running?)').toBeGreaterThanOrEqual(0);
  const at = await screenToClient(page, MARKER);
  expect(at.onCanvas, 'nothing covers the canvas at the marker').toBe(true);
  await page.mouse.move(at.x, at.y);
  await page.mouse.down();
  const sentAt = await page.evaluate(() => window.tilt.lastInputAt);
  await page.mouse.up();
  expect(sentAt, 'tilt.lastInputAt after the button went out').toBeGreaterThan(0);
  const drawnAt = await waitForMarker(page, (before + 1) % 4, sentAt, TRIAL_TIMEOUT_MS);
  writeResult(testInfo, 'click', { advanced: drawnAt !== null, ms: drawnAt === null ? null : round1(drawnAt - sentAt) });
  expect(drawnAt, 'time the marker advanced after the click').not.toBeNull();
});

test('view-only session cannot take control or send input', async ({ page }) => {
  await openTilt(page, `token=${VIEW_TOKEN}`);
  await page.waitForFunction(() => window.tilt.stats.role === 'view', null, { timeout: 10_000 });
  const control = page.locator('#btn-control').or(page.getByRole('button', { name: /control/i })).first();
  await expect(control, 'Control button').toBeDisabled();
  const before = await markerColor(page);
  expect(before, 'marker colour (is tilt-testpattern running?)').toBeGreaterThanOrEqual(0);

  // The client must not send input for a viewer; the server must drop it if sent anyway.
  await page.keyboard.press('a');
  await page.keyboard.press('Space');
  const at = await screenToClient(page, MARKER);
  await page.mouse.click(at.x, at.y);
  await page.evaluate(
    ([px, py]) => {
      const { ws, stats } = window.tilt;
      const norm = (v, size) => Math.round((v * 65535) / (size - 1));
      const key = (down) => new Uint8Array([0x30, down, 0x20, 0, 0, 0]);
      const button = (down) => {
        const b = new DataView(new ArrayBuffer(7));
        b.setUint8(0, 0x21);
        b.setUint8(1, 1);
        b.setUint8(2, down);
        b.setUint16(3, norm(px, stats.width), true);
        b.setUint16(5, norm(py, stats.height), true);
        return b.buffer;
      };
      window.e2e.viewWs = ws;
      for (const msg of [key(1), key(0), button(1), button(0)]) ws.send(msg);
      window.tilt.takeControl();
    },
    MARKER,
  );
  await page.waitForTimeout(1_000);

  expect(await markerColor(page), 'marker colour 1 s after the input').toBe(before);
  const after = await page.evaluate(() => ({
    control: window.tilt.stats.control,
    sameSocket: window.tilt.ws === window.e2e.viewWs && window.tilt.ws.readyState === WebSocket.OPEN,
  }));
  expect(after.control, 'tilt.stats.control').toBe(false);
  // A refused take answers {"t":"error","code":"forbidden"} and keeps the session open.
  expect(after.sameSocket, 'the view session survives its refused take').toBe(true);
});

test('stream resumes after the socket drops', async ({ page }, testInfo) => {
  await openTilt(page, `token=${CONTROL_TOKEN}`);
  const started = Date.now();
  await page.evaluate(() => {
    const dropped = window.tilt.ws;
    const state = { dropped, framesAtClose: null };
    window.e2e.reconnect = state;
    // Frames still in flight on the old socket count until it is closed; only later ones show
    // that a new session streams.
    dropped.addEventListener('close', () => {
      state.framesAtClose = window.tilt.stats.framesDecoded;
    });
    dropped.close();
  });
  const resumed = await page
    .waitForFunction(
      () => {
        const { dropped, framesAtClose } = window.e2e.reconnect;
        const ws = window.tilt.ws;
        return (
          framesAtClose !== null &&
          ws !== null &&
          ws !== dropped &&
          ws.readyState === WebSocket.OPEN &&
          window.tilt.stats.framesDecoded > framesAtClose
        );
      },
      null,
      { timeout: RESUME_TIMEOUT_MS },
    )
    .then(() => true, () => false);
  const ms = Date.now() - started;
  writeResult(testInfo, 'reconnect', { resumed, ms: resumed ? ms : null });
  expect(resumed, `frames decoded on a new socket within ${RESUME_TIMEOUT_MS / 1000} s`).toBe(true);
});
