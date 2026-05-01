import { existsSync } from "node:fs";
import { defineConfig, devices } from "@playwright/test";

// ─── e2e config ──────────────────────────────────────────────────
//
// Plan 01.5 tests run against the worktree's dev server on :5173.
// The zeroship vite-plugin spins an in-process V8 runtime that
// handles `/_zs/v1/chat` directly — no Node.js mock middleware.
//
// Other test files (workspace.spec.ts, etc.) were written against
// the old architecture (real backend at :9090). They are left intact
// but may fail when no backend is running; that is expected.
//
// `reuseExistingServer` means the runner uses your already-running
// `npm run dev` if one exists on :5173, otherwise it starts one.
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
    command: "npm run dev -- --port 5173",
    url: "http://localhost:5173",
    timeout: 60_000,
    reuseExistingServer: !process.env.CI,
    stdout: "ignore",
    stderr: "pipe",
  },
});
