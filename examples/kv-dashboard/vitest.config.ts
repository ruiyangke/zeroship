import { defineConfig } from "vitest/config";

export default defineConfig(({ mode }) => ({
  test: {
    fileParallelism: false,
    projects: [
      { extends: true, test: { name: "unit", environment: "node", include: ["tests/*.unit.test.ts"] } },
      { extends: true, test: {
        name: "dashboard",
        environment: "node",
        include: ["tests/rpc.test.ts", "tests/browser.test.ts"],
        globalSetup: mode === "existing" ? [] : ["tests/fixture/setup.ts"],
        provide: { dashboardExisting: mode === "existing" },
        testTimeout: 90_000,
        hookTimeout: 30_000,
        teardownTimeout: 30_000,
      } },
    ],
  },
}));
