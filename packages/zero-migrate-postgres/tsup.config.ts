import { defineConfig } from "tsup";

// One entry, and it is types-only in substance: the package's whole job is to augment
// `zero-migrate`'s `VendorAttributeNamespaces` interface via declaration merging. `dts`
// is therefore the load-bearing output — without emitted declarations, installing this
// package would add nothing at all.
export default defineConfig({
  entry: { index: "src/index.ts" },
  format: ["esm"],
  dts: true,
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  clean: true,
});
