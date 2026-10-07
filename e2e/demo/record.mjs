// Plays the README demo on the viewer's display with xdotool: real pointer moves along eased
// curves, real key presses, so the recorded cursor glides and the remote desktop answers live.
//   DISPLAY=<viewer display> node e2e/demo/record.mjs <left> <top>
// <left> <top> is where the remote screen's top-left pixel is on the viewer's display.
import { spawnSync } from 'node:child_process';

const [ox, oy] = process.argv.slice(2).map(Number);
const FPS = 60;
let at = [ox + 640, oy + 420];

const xdo = (...args) => {
  const r = spawnSync('xdotool', args.map(String), { stdio: 'inherit' });
  if (r.status !== 0) throw new Error(`xdotool ${args.join(' ')} failed`);
};
const pause = (ms) => xdo('sleep', ms / 1000);
const ease = (t) => (t < 0.5 ? 4 * t * t * t : 1 - (-2 * t + 2) ** 3 / 2);

/** Glides to remote (x, y) on a gentle arc, easing in and out. */
function glide(x, y, ms = 700, { bow = 0.12 } = {}) {
  const [x0, y0] = at;
  const [x1, y1] = [ox + x, oy + y];
  // A control point off the straight line bends the path, as a hand does.
  const cx = (x0 + x1) / 2 - (y1 - y0) * bow;
  const cy = (y0 + y1) / 2 + (x1 - x0) * bow;
  const steps = Math.max(2, Math.round((ms / 1000) * FPS));
  const args = [];
  for (let i = 1; i <= steps; i++) {
    const t = ease(i / steps);
    const px = (1 - t) ** 2 * x0 + 2 * (1 - t) * t * cx + t * t * x1;
    const py = (1 - t) ** 2 * y0 + 2 * (1 - t) * t * cy + t * t * y1;
    args.push('mousemove', Math.round(px), Math.round(py), 'sleep', (1 / FPS).toFixed(4));
  }
  xdo(...args);
  at = [x1, y1];
}
const click = () => xdo('click', 1);
const type = (text, delay = 55) => xdo('type', '--delay', delay, '--', text);
const enter = () => xdo('key', 'Return');
const wheel = (dir, n, ms) => {
  const args = [];
  for (let i = 0; i < n; i++) args.push('click', dir > 0 ? 5 : 4, 'sleep', (ms / 1000).toFixed(3));
  xdo(...args);
};

// The remote desktop (scripts/demo.sh): a terminal at the top left, Chrome on the right.
pause(900);

// 1. Terminal: click in, type, and watch it answer at once.
glide(330, 300, 900);
click();
pause(300);
type('echo "hello from a Linux VM"');
enter();
pause(450);
type('uname -sm');
enter();
pause(450);
type('top -bn1 | head -24');
enter();
pause(1600);

// 2. Drag the terminal by its title bar.
glide(300, 88, 600, { bow: -0.1 });
xdo('mousedown', 1);
pause(120);
glide(420, 200, 1100, { bow: 0.15 });
pause(80);
xdo('mouseup', 1);
pause(500);

// 3. Over to Chrome: bring it forward and scroll the page.
glide(1000, 560, 900);
click();
pause(250);
wheel(1, 14, 70);
pause(500);
wheel(-1, 14, 70);
pause(400);

// 4. Settle on the clock.
glide(980, 380, 800);
pause(1400);
