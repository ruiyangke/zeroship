import react from "@vitejs/plugin-react";
import { configDefaults, defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    setupFiles: ["./test/setup.ts"],
    globals: true,
    include: ["test/**/*.test.{ts,tsx}"],
    exclude: [...configDefaults.exclude, "e2e/**"],
  },
});
