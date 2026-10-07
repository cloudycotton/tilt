// Films the README demo: the desktop scripts/demo.sh sets up (a terminal and a browser window
// under xfwm4), controlled through tilt, in Chrome. Chrome shows showcase.html, which frames the
// real tilt client, on its own X display; ffmpeg grabs that display at 60 fps.
//   DISPLAY=<stage display> node e2e/demo/record.mjs <tilt url with #token=...> <output.mkv>
// CHROME names the Chrome binary (the default is Playwright's "chrome" channel); WebCodecs
// needs a build with H.264, which Playwright's own Chromium lacks.
import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from '@playwright/test';

const [url, out] = process.argv.slice(2);
const stage = { width: 1280, height: 800 };
const desktop = { width: 1280, height: 720 };
const showcase = path.join(path.dirname(fileURLToPath(import.meta.url)), 'showcase.html');

const browser = await chromium.launch({
  headless: false,
  ...(process.env.CHROME ? { executablePath: process.env.CHROME } : { channel: 'chrome' }),
  args: ['--kiosk', '--window-position=0,0', `--window-size=${stage.width},${stage.height}`,
    '--hide-scrollbars', '--disable-gpu', '--force-device-scale-factor=1'],
});
const context = await browser.newContext({ viewport: null });
const page = await context.newPage();
const pause = (ms) => page.waitForTimeout(ms);

await page.goto(`file://${showcase}?src=${encodeURIComponent(url)}`);
const frame = await (await page.waitForSelector('#tilt')).contentFrame();
await frame.waitForFunction(() => window.tilt?.stats.control && window.tilt.stats.framesDrawn > 0);
const box = await (await page.$('#tilt')).boundingBox();
const k = box.width / desktop.width;
// Desktop pixels to page pixels.
const at = (x, y) => [box.x + x * k, box.y + y * k];

// The pointer glides with an ease-in-out, one step per frame, like a hand on a mouse.
let pointer = [desktop.width / 2, desktop.height / 2];
const ease = (t) => (t < 0.5 ? 4 * t * t * t : 1 - (-2 * t + 2) ** 3 / 2);
async function glide(x, y, ms = 600) {
  const [x0, y0] = pointer;
  const steps = Math.max(1, Math.round(ms / 16));
  for (let i = 1; i <= steps; i++) {
    const t = ease(i / steps);
    await page.mouse.move(...at(x0 + (x - x0) * t, y0 + (y - y0) * t));
    await pause(16);
  }
  pointer = [x, y];
}
async function scroll(dy, ms) {
  const steps = Math.round(ms / 16);
  for (let i = 0; i < steps; i++) {
    // A flick: fast at first, then slowing down.
    await page.mouse.wheel(0, Math.round((dy * 2 * (1 - i / steps)) / steps) || Math.sign(dy));
    await pause(16);
  }
}

await page.mouse.move(...at(...pointer));
await pause(400);

const ffmpeg = spawn('ffmpeg', ['-loglevel', 'error', '-y', '-f', 'x11grab', '-draw_mouse', '0',
  '-framerate', '60', '-video_size', `${stage.width}x${stage.height}`, '-i', process.env.DISPLAY,
  '-c:v', 'libx264rgb', '-preset', 'ultrafast', '-crf', '0', out], { stdio: ['pipe', 'inherit', 'inherit'] });
const done = new Promise((resolve) => ffmpeg.on('exit', resolve));
await pause(900);

// The terminal: type, and watch it answer at once.
await glide(330, 330, 700);
await page.mouse.down();
await page.mouse.up();
for (const line of ['uname -sm', 'wave']) {
  await page.keyboard.type(line, { delay: 70 });
  await pause(150);
  await page.keyboard.press('Enter');
  await pause(450);
}
await pause(1700);

// Drag the terminal by its title bar.
await glide(300, 52, 650);
await page.mouse.down();
await pause(120);
await glide(390, 112, 1100);
await page.mouse.up();
await pause(500);

// Over to the browser window: hover a card, then scroll the page.
await glide(950, 300, 800);
await pause(500);
await scroll(1100, 900);
await pause(500);
await scroll(-1100, 900);
await glide(960, 380, 500);
await pause(1200);

ffmpeg.stdin.end('q');
await done;
await browser.close();
