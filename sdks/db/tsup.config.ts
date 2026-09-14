import { defineConfig } from "tsup";

// The package emits only its documented creator-facing entry. Runtime facade
// assembly belongs to the zeroship-data-v8 crate and is built separately.
export default defineConfig({
  format: ["esm"],
  dts: true,
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  treeshake: true,
  splitting: false,
  external: ["@zeroship/types", "zeroship"],
  entry: ["src/index.ts"],
  clean: true,
});
