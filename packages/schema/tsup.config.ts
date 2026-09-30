import { defineConfig } from "tsup";

// The leaf holds the schema builder BOTH consumers share: the phantom-brand
// `TypeBuilder`, `FieldDef`/`TypeName`, and the `t.*` factories. It declares no
// dependencies and emits a single ESM entry plus its types.
//
// Neither consumer depends on the emitted package at run time: both declare it
// as a dev dependency and inline it. `@zeroship/db` bundles it into its
// published entry and into the V8 adapter, where no bare specifier resolves;
// `@zeroship/migrate` marks it `noExternal`, so the published migration package
// keeps its zero-runtime-dependency promise.
export default defineConfig({
  format: ["esm"],
  dts: true,
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  treeshake: true,
  splitting: false,
  entry: ["src/index.ts"],
  clean: true,
});
