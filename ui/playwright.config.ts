import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './e2e',
  testMatch: '*.spec.ts',
  outputDir: '.playwright/results',
  workers: 1,
  retries: 0,
  use: {
    baseURL: 'http://127.0.0.1:19001',
    trace: 'retain-on-failure',
    launchOptions: {
      executablePath: process.env.SQRZL_E2E_CHROMIUM_EXECUTABLE,
    },
  },
  projects: [
    {
      name: 'desktop',
      use: { browserName: 'chromium', viewport: { width: 1440, height: 1000 } },
    },
    {
      name: 'mobile',
      use: {
        browserName: 'chromium',
        viewport: { width: 390, height: 844 },
        isMobile: true,
        hasTouch: true,
      },
    },
  ],
  webServer: [
    {
      command: 'node e2e/server.mjs open',
      url: 'http://127.0.0.1:19001/healthz',
      reuseExistingServer: false,
      gracefulShutdown: { signal: 'SIGTERM', timeout: 5000 },
    },
    {
      command: 'node e2e/server.mjs authenticated',
      url: 'http://127.0.0.1:19003/healthz',
      reuseExistingServer: false,
      gracefulShutdown: { signal: 'SIGTERM', timeout: 5000 },
    },
  ],
});
