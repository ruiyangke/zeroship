import { defineConfig, type Options } from "tsup";

const shared = {
  format: ["esm"],
  dts: true,
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  treeshake: true,
  splitting: false,
  external: ["@zeroship/types", "zeroship"],
} satisfies Options;

export default defineConfig({
  ...shared,
  entry: ["src/index.ts"],
  clean: true,
});
