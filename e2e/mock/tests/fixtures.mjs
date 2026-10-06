// Shared Playwright fixtures: a fresh mock server per test (`mock`, configured with `mockOptions`)
// and small helpers for driving the client and reading the mock's message log.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { test as base, expect } from '@playwright/test';
import { startMockServer } from '../server.mjs';

export { expect };

export const test = base.extend({
  mockOptions: [{}, { option: true }],
  mock: async ({ mockOptions }, use) => {
    const mock = await startMockServer({ quiet: true, ...mockOptions });
    await use(mock);
    await mock.close();
  },
  // Every page in the test's browser contexts: uncaught errors and console output, for the report.
  pageLog: [async ({ context, browser }, use, info) => {
    const lines = [];
    const watch = (page) => {
      page.on('pageerror', (e) => lines.push(`pageerror: ${e.message}`));
      page.on('console', (m) => lines.push(`console.${m.type()}: ${m.text()}`));
    };
    context.on('page', watch);
    const newPage = browser.newPage.bind(browser);
    browser.newPage = async (...args) => {
      const p = await newPage(...args);
      watch(p);
      return p;
    };
    await use(lines);
    browser.newPage = newPage;
    if (lines.length) await info.attach('page-log', { body: lines.join('\n'), contentType: 'text/plain' });
    if (process.env.PAGE_LOG) for (const l of lines) process.stdout.write(`[${info.project.name}] ${info.title}: ${l}\n`);
  }, { auto: true }],
});

// The marker colours of the fixtures (and of tilt-testpattern), in counter order.
export const COLORS = [[230, 40, 40], [40, 200, 60], [40, 80, 230], [240, 240, 240]];
export const BACKGROUND = [32, 32, 32];

export const INPUT_TYPES = ['MOVE', 'BUTTON', 'WHEEL', 'KEY', 'TEXT', 'RELEASE_ALL'];

/** The codec string the client should derive from a fixture's first SPS. */
export function fixtureCodec(stream) {
  const data = fs.readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'fixtures', `${stream}.h264`));
  for (let i = 0; i + 7 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1 && (data[i + 3] & 0x1f) === 7) {
      return 'avc1.' + [...data.subarray(i + 4, i + 7)].map((b) => b.toString(16).padStart(2, '0')).join('').toUpperCase();
    }
  }
  throw new Error(`no SPS in ${stream}`);
}

/** Loads the client with URL fragment options and waits for its debug hook. */
export async function open(page, mock, hash = 'token=devtoken') {
  await page.goto(`${mock.url}#${hash}`);
  await page.waitForFunction(() => window.tilt !== undefined);
}

export const stats = (page) => page.evaluate(() => ({ ...window.tilt.stats }));

export async function waitDrawn(page, n, timeout = 10_000) {
  await page.waitForFunction((min) => window.tilt.stats.framesDrawn >= min, n, { timeout });
}

/** Waits until this page holds control and has drawn a frame; returns its session id. */
export async function controlling(page) {
  await page.waitForFunction(() => window.tilt.stats.control && window.tilt.stats.framesDrawn > 0);
  return page.evaluate(() => window.tilt.stats.session);
}

export async function sessionOf(page) {
  await page.waitForFunction(() => window.tilt.stats.session !== '');
  return page.evaluate(() => window.tilt.stats.session);
}

/** Messages of the given types from one session, logged at or after index `since`. */
export function from(mock, session, types, since = 0) {
  const want = new Set(Array.isArray(types) ? types : [types]);
  return mock.messages.slice(since).filter((m) => m.session === session && want.has(m.type));
}

/** Waits until `count` messages of `types` arrived, then a further `settle` ms, and returns them. */
export async function collect(mock, session, types, since, count, settle = 150) {
  await expect.poll(() => from(mock, session, types, since).length, { timeout: 5000 }).toBeGreaterThanOrEqual(count);
  if (settle) await new Promise((r) => setTimeout(r, settle));
  return from(mock, session, types, since);
}

export const keyLog = (msgs) => msgs.map((m) => `${m.down ? 'down' : 'up'} ${m.keysym.toString(16)}`);

/** Input messages in short form: 'down 61', 'TEXT abc', 'BUTTON 1 down', 'WHEEL 0,2', 'RELEASE_ALL'. */
export const inputLog = (msgs) => msgs.map((m) => {
  switch (m.type) {
    case 'KEY': return `${m.down ? 'down' : 'up'} ${m.keysym.toString(16)}`;
    case 'TEXT': return `TEXT ${m.text}`;
    case 'BUTTON': return `BUTTON ${m.button} ${m.down ? 'down' : 'up'}`;
    case 'WHEEL': return `WHEEL ${m.dx},${m.dy}`;
    default: return m.type;
  }
});

/**
 * Dispatches synthetic keyboard events on the hidden textarea: [[type, KeyboardEventInit], ...].
 * For layouts and keys Playwright's keyboard cannot produce (macOS Option characters, AZERTY).
 */
export async function keyEvents(page, list) {
  await page.evaluate((events) => {
    const ta = document.getElementById('kbd');
    for (const [type, init] of events) {
      ta.dispatchEvent(new KeyboardEvent(type, { bubbles: true, cancelable: true, composed: true, ...init }));
    }
  }, list);
}

export const isApple = (page) => page.evaluate(() => /Mac|iPhone|iPad|iPod/.test(navigator.platform));

export function expectColor(actual, expected, tolerance = 16) {
  const diff = expected.map((v, i) => Math.abs(v - actual[i]));
  expect(Math.max(...diff), `colour ${actual.slice(0, 3)} vs ${expected}`).toBeLessThanOrEqual(tolerance);
}

export const sample = (page, x, y) => page.evaluate(([px, py]) => window.tilt.sample(px, py), [x, y]);

export async function canvasRect(page) {
  return page.evaluate(() => {
    const r = document.getElementById('screen').getBoundingClientRect();
    return { left: r.left, top: r.top, width: r.width, height: r.height };
  });
}

/** The normalized coordinates the protocol expects for a client point (brief 4.3). */
export function norm(rect, x, y) {
  const c = (v) => Math.min(1, Math.max(0, v));
  return [Math.round(c((x - rect.left) / rect.width) * 65535), Math.round(c((y - rect.top) / rect.height) * 65535)];
}

/** Fakes page visibility (the hidden-tab path) and fires visibilitychange. */
export async function setVisibility(page, state) {
  await page.evaluate((v) => {
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => v });
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => v === 'hidden' });
    document.dispatchEvent(new Event('visibilitychange'));
  }, state);
}

/**
 * Clicks a toolbar button the way a mouse user does: the bar shows when the pointer touches the top
 * edge. It slides away 3 s later, which a loaded test machine can outlast before the click.
 */
export async function clickToolbar(page, button) {
  await expect(async () => {
    await page.mouse.move(2, 1);
    await expect(page.locator('#toolbar')).not.toHaveClass(/away/, { timeout: 2000 });
    await button.click({ timeout: 2000 });
  }).toPass({ timeout: 20_000 });
}

/**
 * Taps a toolbar button on a touch screen. The bar slides away 3 s after the last press, which a
 * loaded test machine can outlast: a tap on the top edge brings it back.
 */
export async function tapToolbar(page, button) {
  await expect(async () => {
    if (await page.locator('#toolbar.away').count()) await page.locator('#edge').tap({ timeout: 2000 });
    await button.tap({ timeout: 2000 });
  }).toPass({ timeout: 20_000 });
}

export { startMockServer };
