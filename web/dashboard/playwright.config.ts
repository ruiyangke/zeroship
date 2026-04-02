import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  timeout: 30000,
  expect: { timeout: 5000 },
  use: {
    baseURL: "http://localhost:5173",
    headless: true,
  },
  // Servers must be started manually:
  //   1. cd /tmp/appbase-e2e && APPBASE_MASTER_KEY=e2e-test-key appbase serve server.js --port=3335
  //   2. cd web/dashboard && npx vite --port 5173
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
});
