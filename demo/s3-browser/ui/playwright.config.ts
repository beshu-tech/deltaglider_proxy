import { defineConfig, devices } from '@playwright/test';

const baseURL = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:19077';

export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  // No retries, CI included: a retry that passes hid flaky tests. A flake
  // fails the run; the trace of the failed attempt is kept.
  retries: 0,
  workers: 1,
  timeout: 30_000,
  // CI uploads playwright-report/ on failure.
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : 'list',
  use: {
    ...devices['Desktop Chrome'],
    baseURL,
    trace: 'retain-on-failure',
  },
});
