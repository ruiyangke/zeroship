import { existsSync } from "node:fs";
import { defineConfig, devices } from "@playwright/test";

// ─── e2e config ──────────────────────────────────────────────────
//
// Runs against the real dev server at :5173 + the real zeroship
// runtime + RPC handler at :3001. No mocks. Tests that need to
// create + delete apps hit the real control plane via the same
// RPC stubs the app uses.
//
// `reuseExistingServer` means the runner uses your already-running
// `npm run dev` if one exists, otherwise it starts one for the
// suite.
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false, // serial — tests share the real backend
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  globalTeardown: "./e2e/global-teardown.ts",
  reporter: process.env.CI ? "line" : [["list"], ["html", { open: "never" }]],
  timeout: 30_000,
  expect: { timeout: 5_000 },
  use: {
    baseURL: "http://localhost:5173",
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    actionTimeout: 5_000,
    navigationTimeout: 15_000,
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        // On NixOS the bundled Playwright chromium can't dynamically link
        // (no glib/nss/etc on the system loader path). Default to the
        // system chromium if one exists; override with
        // PLAYWRIGHT_CHROMIUM_PATH if you want the bundled one.
        launchOptions: {
          executablePath:
            process.env.PLAYWRIGHT_CHROMIUM_PATH ??
            (existsSync("/run/current-system/sw/bin/chromium")
              ? "/run/current-system/sw/bin/chromium"
              : undefined),
        },
      },
    },
  ],
  webServer: {
    command: "npm run dev",
    url: "http://localhost:5173",
    timeout: 60_000,
    reuseExistingServer: true,
    stdout: "ignore",
    stderr: "pipe",
  },
});
