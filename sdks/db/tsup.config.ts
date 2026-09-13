import { defineConfig } from "tsup";

// Build public and internal declarations together so branded SDK types share
// their declaration site. JavaScript entries remain independently bundled;
// installer tests exercise public builders against the internal facade.
export default defineConfig({
  format: ["esm"],
  dts: true,
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  treeshake: true,
  splitting: false,
  external: ["@zeroship/types", "zeroship"],
  entry: ["src/index.ts", "src/internal.ts"],
  clean: true,
});
