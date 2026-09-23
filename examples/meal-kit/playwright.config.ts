import { defineConfig } from "@playwright/test";
import { existsSync } from "node:fs";
import { delimiter, join } from "node:path";
import { readyOrigin, testOrigin } from "./tests/fixture/settings";
const executablePath =
  process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ??
  (process.env.PATH ?? "")
    .split(delimiter)
    .map((p) => join(p, "chromium"))
    .find(existsSync);
export default defineConfig({
  testDir: "tests/browser",
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  use: {
    baseURL: testOrigin,
    launchOptions: { executablePath },
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
  },
  webServer: {
    command: "node tests/fixture/browser-server.mjs",
    // The fixture's gate, not either app's port: it opens once both runtimes
    // answer a procedure. A Vite port binds long before the runtime behind it
    // serves, so gating on one starts the suite against a 503.
    url: readyOrigin,
    // Both runtimes log here. A spec that fails on `internal error` is
    // diagnosed from the runtime's own line, not from the RPC envelope.
    stdout: "pipe",
    reuseExistingServer: false,
    gracefulShutdown: { signal: "SIGTERM", timeout: 10_000 },
    timeout: 600_000,
  },
});
