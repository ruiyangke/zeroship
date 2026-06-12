import { defineConfig, devices } from "@playwright/test";

// Browser-level E2E against an EXTERNAL stack (control + worker + gateway +
// ephemeral PG), brought up by global-setup → scripts/up.sh and torn down by
// global-teardown → scripts/down.sh. There is intentionally NO `webServer`:
// the stack is a multi-process Rust deployment, not a single dev server, and
// the specs address it by `<slug>.localhost:<gatePort>` Host routing.
//
// baseURL is NOT hardcoded — the gateway port is dynamic (chosen by up.sh and
// recorded in .stack.json); helpers.ts builds per-app URLs from the descriptor.
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
  globalTeardown: "./global-teardown.ts",
  use: {
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    actionTimeout: 10_000,
    navigationTimeout: 30_000,
  },
  projects: [
    {
      // Browser comes from the version-matched Nix package via
      // PLAYWRIGHT_BROWSERS_PATH — run inside the nix env that sets it. The
      // npm @playwright/test is pinned to 1.58.2 so the chromium revision
      // matches the nix playwright-driver; npm-downloaded browsers can't link
      // their libs on NixOS, the Nix ones can. NO executablePath on purpose.
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
});
