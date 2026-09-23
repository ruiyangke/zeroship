import { defineConfig } from "vitest/config";
import babel from "@rolldown/plugin-babel";
import { lingui, linguiTransformerBabelPreset } from "@lingui/vite-plugin";
export default defineConfig({
  plugins: [lingui({ configPath: "../../packages/shared/lingui.config.ts" }), babel({ presets: [linguiTransformerBabelPreset()] })],
  test: { include: ["tests/*.test.ts"] },
});
