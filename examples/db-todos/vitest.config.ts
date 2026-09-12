import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  test: {
    fileParallelism: false,
    teardownTimeout: 30_000,
    projects: [
      { extends: true, test: {
        name: "unit", environment: "jsdom", setupFiles: ["./test/setup.ts"], globals: true, include: ["test/**/*.test.{ts,tsx}"],
      } },
      { extends: true, test: {
        name: "acceptance", environment: "node", include: ["tests/*.test.ts"], globalSetup: ["./tests/fixture/setup.ts"],
        testTimeout: 180_000, hookTimeout: 1_200_000,
      } },
    ],
  },
});
