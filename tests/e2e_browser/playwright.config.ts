import { defineConfig, devices } from "@playwright/test";

// Browser-level E2E against a live multi-process stack (control + worker +
// gateway + ephemeral PostgreSQL) that global-setup.ts builds, starts and tears
// down. There is no `webServer`: the stack is not a single dev server, and the
// specs address each deployed app by `<slug>.localhost:<gatePort>` Host
// routing.
//
// baseURL is not set: the gateway port is chosen at bring-up and recorded in
// .stack.json, and helpers.ts builds per-app URLs from that descriptor.
export default defineConfig({
  testDir: "./specs",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  workers: 1,
  reporter: [["list"]],
  timeout: 60_000,
  expect: { timeout: 15_000 },
  globalSetup: "./global-setup.ts",
  use: {
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    actionTimeout: 10_000,
    navigationTimeout: 30_000,
  },
  projects: [
    {
      // The browser comes from the development shell's
      // PLAYWRIGHT_BROWSERS_PATH, and global setup launches it first. Why
      // @playwright/test has to match it: xtask/tests/playwright_browsers.rs.
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
});
