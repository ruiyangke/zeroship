import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { defineConfig, devices } from "@playwright/test";

// Prefer the release binary at the repo root, but ONLY if it is really there.
//
// `dev-server.ts` uses ZEROSHIP_BIN verbatim when set, with no existence check
// and no fallback, so pointing it at a missing file is strictly worse than not
// setting it at all: `pnpm dev` finds a working binary on its own, and setting
// this to a path that does not exist turns that into
// `Error: spawn .../target/release/zeroship ENOENT` before the suite starts.
// That is exactly what happens in a git worktree, where the checkout has no
// target/ of its own. Measured 2026-08-12.
const releaseBin = fileURLToPath(new URL("../../target/release/zeroship", import.meta.url));
const zeroshipBin = process.env.ZEROSHIP_BIN ?? (existsSync(releaseBin) ? releaseBin : undefined);

// Must match vite.config.ts's `webPort`, which sets `strictPort: true` so a
// clash fails the boot instead of quietly moving the app to another port and
// leaving these tests pointed at whatever else is listening.
const webPort = Number(process.env.ISSUE_TRACKER_WEB_PORT ?? 5183);
const baseURL = `http://localhost:${webPort}`;

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  // One worker: every spec drives the same dev database, and the reports and
  // bug-list assertions read totals that a concurrent spec would move under
  // them.
  //
  // THE SUITE PASSES AT --workers=4 AND THAT IS NOT EVIDENCE IT IS SAFE.
  // Measured 2026-08-12: 3 of 3 parallel runs green, and ~40% faster, which is
  // exactly the result that invites raising this. The race it hides is in
  // table-labels.spec.ts, which asserts that no dashboard row prints a raw
  // `prod_...` id. Those rows resolve their product through maps that
  // `useBugLookups()` fetched once on mount; a product created by another
  // worker AFTER that fetch and BEFORE the rows render is absent from the map
  // and renders as an id. The window is milliseconds wide, so it closes on
  // most runs and opens on a loaded machine.
  //
  // reports-labels.spec.ts survives concurrency only by accident: it needs two
  // products with the SAME name, and names carry a pid-derived marker that
  // differs per worker. That is luck, not isolation.
  //
  // Raising this needs per-worker database isolation, not a green run.
  workers: 1,
  // Sweeps the fixture groups the access-control specs leave in the dev
  // database. See e2e/global-teardown.ts -- it deletes through the same
  // guarded procedure a person would, so an in-use group is refused.
  globalTeardown: "./e2e/global-teardown.ts",
  reporter: process.env.CI ? "line" : [["list"], ["html", { open: "never" }]],
  timeout: 60_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL,
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    actionTimeout: 10_000,
    navigationTimeout: 20_000,
  },
  projects: [
    {
      // The browser comes from the version-matched Nix package via
      // PLAYWRIGHT_BROWSERS_PATH (set by `nix develop`). @playwright/test is
      // pinned to the nixpkgs playwright-driver version so the chromium
      // revision matches -- npm-downloaded browsers cannot link their libs on
      // NixOS, the Nix ones can.
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
  webServer: {
    // `pnpm migrate` first, because `pnpm dev` does NOT apply committed
    // migrations. Without it a clean checkout runs these specs against a
    // database with no tables, and every spec fails on rendered UI errors that
    // say nothing about the real cause. Measured: 2 tables instead of 24, and
    // four specs failing with assertion messages about the app.
    //
    // `reuseExistingServer` means this whole command is skipped when something
    // is already listening, so a dev server started by hand still needs its own
    // `pnpm migrate` -- which is what README's "Run locally" says to do.
    command: "pnpm migrate && pnpm dev",
    url: baseURL,
    reuseExistingServer: true,
    timeout: 120_000,
    // Pinning the RELEASE binary when it exists is deliberate: a stale
    // target/release/zeroship reproduces already-fixed runtime bugs exactly,
    // which is a long way to travel before suspecting the binary.
    ...(zeroshipBin ? { env: { ZEROSHIP_BIN: zeroshipBin } } : {}),
  },
});
