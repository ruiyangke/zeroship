import { defineConfig } from "tsup";

// One entry, and it is types-only in substance: the package's whole job is to augment
// `@zeroship/migrate`'s `VendorAttributeNamespaces` interface via declaration merging.
// TypeScript emits declarations separately in the build script. A declaration bundler
// removes the type-only import that marks this file as an external-module augmentation,
// turning it into an ambient replacement for `@zeroship/migrate`.
export default defineConfig({
  entry: { index: "src/index.ts" },
  format: ["esm"],
  dts: false,
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  clean: true,
});
