// Shared by the specs. They drive the web client only through its documented surface: the
// window.tilt hook (brief 6.6), canvas#screen, the hidden textarea and the Control button.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const CONTROL_TOKEN = process.env.TILT_TOKEN || 'devtoken';
export const VIEW_TOKEN = process.env.TILT_VIEW_TOKEN || 'viewtoken';
/** The Xvfb screen tilt serves, as WxH. */
export const EXPECT_SCREEN = (process.env.EXPECT_SCREEN || '1920x1080').split('x').map(Number);

/** tilt-testpattern's marker colours in counter order (tools/e2e/src/lib.rs). */
export const COLORS = [[230, 40, 40], [40, 200, 60], [40, 80, 230], [240, 240, 240]];
/** The marker covers 64..320 on both axes; probes and clicks use its centre. */
export const MARKER = [192, 192];
/** Same rule as tilt-probe: further than this from every marker colour is "no marker". */
const MAX_MATCH_DISTANCE = 60;
const FIRST_FRAME_TIMEOUT_MS = 15_000;

const RESULTS = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../results');

/** Runs in the page before the client: marker classification for in-page polling. */
function installPageHelpers({ colors, marker, maxDistance }) {
  const classify = (rgb) => {
    let best = -1;
    let bestDistance = maxDistance;
    colors.forEach((c, i) => {
      const d = Math.hypot(rgb[0] - c[0], rgb[1] - c[1], rgb[2] - c[2]);
      if (d <= bestDistance) {
        best = i;
        bestDistance = d;
      }
    });
    return best;
  };
  window.e2e = { classify, marker: () => classify(window.tilt.sample(marker[0], marker[1])) };
}

/** Opens the client with the given fragment options and waits for its first drawn frame. */
export async function openTilt(page, hash) {
  await page.addInitScript(installPageHelpers, { colors: COLORS, marker: MARKER, maxDistance: MAX_MATCH_DISTANCE });
  await page.goto(`/#${hash}`);
  try {
    await page.waitForFunction(() => window.tilt?.stats.framesDrawn > 0, null, { timeout: FIRST_FRAME_TIMEOUT_MS });
  } catch {
    const state = await page.evaluate(() => ({
      stats: window.tilt?.stats ?? 'no window.tilt',
      page: document.body.innerText.replace(/\s+/g, ' ').trim().slice(0, 300),
    }));
    throw new Error(`no frame drawn within ${FIRST_FRAME_TIMEOUT_MS / 1000} s: ${JSON.stringify(state)}`);
  }
}

/** The marker's colour index on the canvas, or -1 if it does not look like the marker. */
export const markerColor = (page) => page.evaluate(() => window.e2e.marker());

export async function waitForControl(page) {
  await page.waitForFunction(() => window.tilt.stats.control === true, null, { timeout: 10_000 });
}

/** Viewport coordinates `{x, y}` of screen pixel (x, y), and whether the canvas is on top there. */
export function screenToClient(page, [x, y]) {
  return page.evaluate(([x, y]) => {
    const canvas = document.getElementById('screen');
    const r = canvas.getBoundingClientRect();
    const cx = r.left + ((x + 0.5) * r.width) / canvas.width;
    const cy = r.top + ((y + 0.5) * r.height) / canvas.height;
    return { x: cx, y: cy, onCanvas: document.elementFromPoint(cx, cy) === canvas };
  }, [x, y]);
}

/**
 * Waits until the marker shows colour `next` and returns when that change was first drawn:
 * the time of the first probe event (needs `probe=` in the URL) at or after `since` that shows
 * colour `next`, on the performance.now() clock. Null on timeout. Matching the colour matters
 * because the client records every change: the refinement frames that follow the previous
 * change still nudge the old colour by a level or two after `since`.
 */
export async function waitForMarker(page, next, since, timeout) {
  return page
    .waitForFunction(
      ([next, since]) => {
        if (window.e2e.marker() !== next) return false;
        const ev = window.tilt.probeEvents.find((e) => e.at >= since && window.e2e.classify(e.rgb) === next);
        return ev ? ev.at : false;
      },
      [next, since],
      { timeout },
    )
    .then((handle) => handle.jsonValue(), () => null);
}

/** Nearest-rank percentile, as tilt-probe computes it. */
export function percentile(sorted, q) {
  if (!sorted.length) return null;
  return sorted[Math.max(1, Math.ceil(q * sorted.length)) - 1];
}

export const round1 = (v) => (v === null ? null : Math.round(v * 10) / 10);

/** Writes e2e/results/<name>-<project>.json for scripts/e2e.sh and later comparison. */
export function writeResult(testInfo, name, data) {
  fs.mkdirSync(RESULTS, { recursive: true });
  const body = JSON.stringify({ project: testInfo.project.name, ...data }, null, 2);
  fs.writeFileSync(path.join(RESULTS, `${name}-${testInfo.project.name}.json`), `${body}\n`);
}
