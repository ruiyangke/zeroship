import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["suites/**/*.test.ts"],
    // Node, not jsdom: the browser is a real chromium driven by Playwright.
    environment: "node",
    // One demo at a time. Each suite boots a vite dev server, a V8 runtime and
    // a chromium; running several at once starves the box, and a starved run
    // fails on timeouts that look like app bugs.
    fileParallelism: false,
    pool: "forks",
    maxWorkers: 1,
    // Dev-server boot dominates: vite cold-starts, the plugin spawns
    // `zeroship serve`, gen-types may run. Measured ~10-25s per demo.
    testTimeout: 60_000,
    hookTimeout: 180_000,
    teardownTimeout: 60_000,
    // Never retry. A retry turns a flaky demo into a green one, and "does this
    // demo work" is precisely the question we refuse to blur.
    retry: 0,
    reporters: ["default", new URL("./src/reporter.ts", import.meta.url).pathname],
  },
});
