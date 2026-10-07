// Records the README demo: a viewer controlling the desktop scripts/demo.sh sets up (an xterm
// and a browser window under xfwm4), through tilt, in Chrome.
//   node e2e/demo/record.mjs <tilt url with #token=...> <output.webm>
import fs from 'node:fs';
import path from 'node:path';
import { chromium } from '@playwright/test';

const [url, out] = process.argv.slice(2);
const size = { width: 1280, height: 720 };
const dir = fs.mkdtempSync(path.join(path.dirname(path.resolve(out)), 'video-'));

const browser = await chromium.launch({ channel: 'chrome' });
const context = await browser.newContext({ viewport: size, recordVideo: { dir, size } });
const page = await context.newPage();
const pause = (ms) => page.waitForTimeout(ms);

await page.goto(url);
await page.waitForFunction(() => window.tilt?.stats.control && window.tilt.stats.framesDrawn > 0);
await pause(1200);

// The terminal: type, and watch it answer at once.
await page.mouse.click(300, 300);
for (const line of ['echo "hello from a remote VM"', 'uname -srm', 'for i in $(seq 40); do echo "frame $i  $(date +%T.%N)"; sleep 0.04; done']) {
  await page.keyboard.type(line, { delay: 35 });
  await page.keyboard.press('Enter');
  await pause(500);
}
await pause(1500);

// Drag the terminal by its title bar.
await page.mouse.move(250, 54);
await page.mouse.down();
for (let i = 1; i <= 40; i++) {
  await page.mouse.move(250 + i * 6, 54 + i * 3);
  await pause(16);
}
await page.mouse.up();
await pause(600);

// Scroll the browser window.
await page.mouse.move(950, 430);
for (let i = 0; i < 24; i++) {
  await page.mouse.wheel(0, 60);
  await pause(40);
}
for (let i = 0; i < 24; i++) {
  await page.mouse.wheel(0, -60);
  await pause(40);
}
await pause(1200);

await context.close();
await browser.close();
const [video] = fs.readdirSync(dir);
fs.renameSync(path.join(dir, video), out);
fs.rmdirSync(dir);
