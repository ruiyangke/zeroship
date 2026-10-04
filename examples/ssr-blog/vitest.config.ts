import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    fileParallelism: false,
    teardownTimeout: 30_000,
    projects: [
      { extends: true, test: { name: "unit", environment: "node", include: ["test/**/*.test.ts"] } },
      { extends: true, test: {
        name: "browser", environment: "node", include: ["tests/*.test.ts"], globalSetup: ["tests/fixture/setup.ts"],
        testTimeout: 90_000, hookTimeout: 30_000,
      } },
    ],
  },
});
