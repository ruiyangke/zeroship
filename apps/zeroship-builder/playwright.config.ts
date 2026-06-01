import { defineConfig, devices } from "@playwright/test";

const skipWebServer = process.env.PLAYWRIGHT_NO_WEBSERVER === "1";

// ─── e2e config ──────────────────────────────────────────────────
//
// `chat-openai.spec.ts` runs against the worktree's
// dev server on :5173. The zeroship vite-plugin spins an in-process
// V8 runtime that handles `/__zeroship/v1/chat` directly.
//
// `reuseExistingServer` means the runner uses your already-running
// `npm run dev` if one exists on :5173, otherwise it starts one.
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
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
      // On NixOS the npm-downloaded Playwright chromium can't dynamically
      // link (no glib/nss/etc on the loader path). Run inside `nix develop`,
      // which sets PLAYWRIGHT_BROWSERS_PATH to the version-matched Nix
      // browsers (npm @playwright/test is pinned to the nixpkgs
      // playwright-driver version, 1.58.2, so the chromium revision matches).
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
  ...(skipWebServer
    ? {}
    : {
        webServer: {
          command: "npm run dev -- --port 5173",
          url: "http://localhost:5173",
          timeout: 60_000,
          reuseExistingServer: !process.env.CI,
          stdout: "ignore",
          stderr: "pipe",
        },
      }),
});
