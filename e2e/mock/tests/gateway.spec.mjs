// The client behind a gateway that serves tilt under a path prefix with a real round trip, as
// a remote VM's proxied URL does (e2e/proxy.mjs).
import { startProxy } from '../../proxy.mjs';
import { controlling, collect, expect, keyLog, test } from './fixtures.mjs';

const RTT_MS = 60;

test.describe('behind a path-prefix gateway', () => {
  let proxy;
  test.beforeEach(async ({ mock }) => {
    proxy = await startProxy({ upstream: new URL(mock.url).host, prefix: '/vm/desk/', delayMs: RTT_MS / 2 });
  });
  test.afterEach(async () => {
    await proxy.close();
  });

  for (const [what, path] of [['with', '/vm/desk/'], ['without', '/vm/desk']]) {
    test(`streams and takes input, opened ${what} the trailing slash`, async ({ page, mock }) => {
      const url = new URL(path, proxy.url);
      await page.goto(`${url}#token=devtoken&control=1`);
      // The page is a directory under the prefix, and the token left the address bar.
      await expect.poll(() => page.evaluate(() => location.pathname)).toBe('/vm/desk/');
      const id = await controlling(page);
      expect(page.url()).not.toContain('token');
      // Its stream goes through the gateway too.
      expect(await page.evaluate(() => window.tilt.ws.url)).toBe(`ws://${url.host}/vm/desk/stream`);
      const since = mock.messages.length;
      await page.evaluate(() => document.getElementById('kbd').focus());
      await page.keyboard.press('a');
      expect(keyLog(await collect(mock, id, 'KEY', since, 2))).toEqual(['down 61', 'up 61']);
      // PING/PONG measures the gateway's round trip.
      await expect.poll(() => page.evaluate(() => window.tilt.stats.rttMs), { timeout: 5000 })
        .toBeGreaterThanOrEqual(RTT_MS);
    });
  }

  test('reconnects through the gateway', async ({ page }) => {
    await page.goto(`${proxy.url}#token=devtoken`);
    await page.waitForFunction(() => window.tilt?.stats.framesDrawn > 0);
    const first = await page.evaluate(() => window.tilt.stats.session);
    await page.evaluate(() => window.tilt.ws.close());
    await page.waitForFunction((s) => window.tilt.stats.session !== s && window.tilt.stats.framesDrawn > 0, first);
  });
});
