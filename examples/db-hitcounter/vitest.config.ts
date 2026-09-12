import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    include: ["tests/*.test.ts"],
    fileParallelism: false,
    testTimeout: 180_000,
    hookTimeout: 1_200_000,
    teardownTimeout: 30_000,
  },
});
