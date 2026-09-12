import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    include: ["tests/*.test.ts"],
    globalSetup: ["./tests/fixture.ts"],
    fileParallelism: false,
    testTimeout: 90_000,
    hookTimeout: 1_200_000,
    teardownTimeout: 30_000,
  },
});
