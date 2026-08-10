import { fileURLToPath } from "node:url";

import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? "line" : [["list"], ["html", { open: "never" }]],
  timeout: 60_000,
  expect: { timeout: 10_000 },
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
      // Browser comes from the version-matched Nix package via
      // PLAYWRIGHT_BROWSERS_PATH — run inside `nix develop` (which sets it).
      // npm @playwright/test is pinned to the nixpkgs playwright-driver
      // version (1.58.2) so the chromium revision matches; the npm-downloaded
      // browsers can't link their libs on NixOS, the Nix ones can.
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
  webServer: {
    command: "pnpm dev",
    url: "http://localhost:5173",
    reuseExistingServer: true,
    timeout: 120_000,
    env: {
      // Pinned to the RELEASE binary on purpose: `pnpm dev` otherwise resolves
      // `node_modules/.bin/zeroship`, and a stale target/release/zeroship
      // reproduces already-fixed runtime bugs exactly, which is a long way to
      // travel before suspecting the binary.
      //
      // Resolved RELATIVE to this file. It was an absolute path into one
      // developer's home directory, so on any other checkout the dev server
      // spawned a binary that does not exist -- `dev-server.ts` uses
      // ZEROSHIP_BIN directly when set, with no existence check and no
      // fallback. The sibling at tests/e2e-browser/src/dev-server.ts already
      // resolves it this way; examples/ssr-blog omits it entirely and relies
      // on the bin-dir fallback, which is the other valid answer.
      //
      // `process.env` still wins, so an operator can point it elsewhere.
      ZEROSHIP_BIN:
        process.env.ZEROSHIP_BIN
        ?? fileURLToPath(new URL("../../target/release/zeroship", import.meta.url)),
    },
  },
});
