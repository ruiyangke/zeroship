import { defineConfig, devices } from "@playwright/test";
import { APP_ORIGIN, GATE_ORIGIN } from "./tests/fixture/settings.ts";

export default defineConfig({
  testDir: "./e2e",
  // One worker: the suite drives a streaming reply against a local stub and
  // the box also hosts the dev server this config starts.
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? "line" : [["list"], ["html", { open: "never" }]],
  timeout: 60_000,
  expect: { timeout: 15_000 },
  use: {
    baseURL: APP_ORIGIN,
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    actionTimeout: 5_000,
    navigationTimeout: 15_000,
  },
  projects: [
    {
      // Browser comes from the development shell's PLAYWRIGHT_BROWSERS_PATH:
      // run inside `nix develop`. Why @playwright/test has to match it:
      // xtask/tests/playwright_browsers.rs.
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
  webServer: {
    // Starts the OpenAI-compatible stub and the app's dev server together,
    // configuring the app through its own `ZS_VAR_` environment surface.
    command: "node tests/fixture/browser-server.ts",
    // The fixture's gate, not the Vite port: it opens only once the runtime
    // behind Vite answers an RPC, so the suite never starts against a 503.
    url: GATE_ORIGIN,
    stdout: "pipe",
    reuseExistingServer: false,
    gracefulShutdown: { signal: "SIGTERM", timeout: 10_000 },
    timeout: 120_000,
  },
});
