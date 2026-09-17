import { defineConfig } from "tsup";

// The leaf holds the schema builder BOTH consumers share: the phantom-brand
// `TypeBuilder`, `FieldDef`/`TypeName`, and the `t.*` factories. It declares no
// dependencies and emits a single ESM entry plus its types.
//
// `@zeroship/db` consumes the emitted package as an ordinary dependency;
// `@zeroship/migrate` marks it `noExternal` and bundles it, so the published
// migration package keeps its zero-runtime-dependency promise.
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
