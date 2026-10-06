// Runs on the host against a running tilt (docker compose up, or scripts/e2e.sh).
//   TILT_URL       default http://localhost:6090 (localhost is a secure context, which WebCodecs needs)
//   TILT_TOKEN, TILT_VIEW_TOKEN, EXPECT_SCREEN: see tests/helpers.mjs
import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  // One tilt, one desktop and one marker: tests must not overlap.
  workers: 1,
  fullyParallel: false,
  retries: 0,
  timeout: 60_000,
  outputDir: './results/artifacts',
  reporter: [['list'], ['json', { outputFile: './results/playwright.json' }]],
  use: {
    baseURL: process.env.TILT_URL || 'http://localhost:6090',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [
    { name: 'chrome', use: { ...devices['Desktop Chrome'], channel: 'chrome' } },
    { name: 'webkit', use: { ...devices['Desktop Safari'] } },
  ],
});
