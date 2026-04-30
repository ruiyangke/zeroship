import { existsSync } from "node:fs";
import { defineConfig, devices } from "@playwright/test";

// ─── e2e config ──────────────────────────────────────────────────
//
// Plan 01 tests run against the worktree's own dev server on :5174
// so they don't conflict with the main branch dev server at :5173.
// The zeroship API backend runs on :3002 (ZEROSHIP_DEV_PORT=3002)
// for the same reason.
//
// The chat-mock tests exercise the WorkspaceShell + mock postChat
// stream — no real control plane needed.
//
// Other test files (workspace.spec.ts, etc.) were written against
// the old architecture (real backend at :9090). They are left intact
// but may fail when no backend is running; that is expected.
//
// `reuseExistingServer` means the runner uses your already-running
// `npm run dev` if one exists on :5174, otherwise it starts one.
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
    baseURL: "http://localhost:5174",
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
    // ZEROSHIP_MOCK_CHAT=1 activates the Node.js mock middleware in
    // vite.config.ts that serves /_rpc/postChat directly (no V8 runtime).
    // Port 5174 avoids clashing with the main branch dev server on :5173.
    command: "ZEROSHIP_MOCK_CHAT=1 npm run dev -- --port 5174",
    url: "http://localhost:5174",
    timeout: 60_000,
    reuseExistingServer: !process.env.CI,
    stdout: "ignore",
    stderr: "pipe",
  },
});
