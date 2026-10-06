// Web client tests against the mock server (server.mjs). Each test starts its own mock on a free
// port, so tests run in parallel. http://127.0.0.1 is a secure context, so Chrome exposes WebCodecs.
import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  timeout: 60_000,
  expect: { timeout: 5_000 },
  fullyParallel: true,
  workers: 4,
  reporter: [['list']],
  use: {
    trace: 'retain-on-failure',
  },
  projects: [
    { name: 'chrome', use: { ...devices['Desktop Chrome'], channel: 'chrome' } },
    { name: 'webkit', use: { ...devices['Desktop Safari'] } },
  ],
});
