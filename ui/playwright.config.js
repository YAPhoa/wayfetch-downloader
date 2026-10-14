// Browser tests against a real `wayfetchd` (see tests/e2e/harness.js).
// Build the daemon first: `cargo build --locked -p wayfetchd`.

import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: 'tests/e2e',
  timeout: 180_000,
  expect: { timeout: 30_000 },
  workers: 1,
  reporter: 'list',
  use: {
    browserName: 'chromium',
    headless: true,
    // The WAN is unreachable: every request beyond loopback goes to a
    // proxy that does not exist, so the UI must work from the daemon alone.
    proxy: { server: 'http://127.0.0.1:9', bypass: '127.0.0.1' },
    launchOptions: { args: ['--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1'] },
  },
});
