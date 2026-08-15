import { defineConfig, devices } from "@playwright/test";

const baseURL = process.env.ZEROSHIP_AUTH_UI_BASE_URL;
const jsonOutput = process.env.ZEROSHIP_AUTH_UI_RESULTS_JSON;

if (!baseURL) {
  throw new Error("ZEROSHIP_AUTH_UI_BASE_URL is required; use tests/e2e_auth_ui.sh");
}
if (!jsonOutput) {
  throw new Error("ZEROSHIP_AUTH_UI_RESULTS_JSON is required; use tests/e2e_auth_ui.sh");
}

export default defineConfig({
  testDir: "./specs",
  fullyParallel: false,
  forbidOnly: true,
  retries: 0,
  workers: 1,
  reporter: [["list"], ["json", { outputFile: jsonOutput }]],
  timeout: 60_000,
  expect: { timeout: 15_000 },
  globalSetup: "./global-setup.ts",
  globalTeardown: "./global-teardown.ts",
  use: {
    baseURL,
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    actionTimeout: 10_000,
    navigationTimeout: 30_000,
  },
  projects: [
    {
      // The Nix shell provides the version-matched Chromium through
      // PLAYWRIGHT_BROWSERS_PATH. Do not set executablePath or download one.
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
});
