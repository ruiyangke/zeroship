import { defineConfig, type Options } from "tsup";

const shared = {
  format: ["esm"],
  dts: true,
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  treeshake: true,
  splitting: true,
} satisfies Options;

export default defineConfig([
  {
    ...shared,
    // The public DSL entry (`.`) + the framework-internal pure-JS recorder
    // (`./internal/recorder`) exposed to the `zero-migrate-cli` host
    // package via a documented subpath export. This package carries ZERO native
    // code and ZERO runtime deps; the host/addon/drivers live in the separate
    // `zero-migrate-cli` package.
    entry: {
      index: "src/index.ts",
      "internal/recorder": "src/internal/recorder.ts",
    },
    clean: true,
  },
  // The recorder artifact (DSL redesign S0.5): ONE self-contained ESM file
  // exposing the FULL recorder surface — the internal recorder seam
  // (`__begin`/`__drain`), the producer census (`opProducers`), the value-position
  // `cCase` helper, the internal `__pgDomain`/`__pgSequence` handles, AND the whole
  // public vendor surface — in one module. The SDK's recorder-internal tests import
  // it (`tests/{ops,sequences-exclusion,column-facets-lockstep}.test.ts`). No
  // code-splitting (single file), no `.d.ts` (build artifact only). The inlined db
  // type-builder (`./db-types.ts`) is bundled in — there is no external db dep.
  {
    ...shared,
    entry: { "embedded-recorder": "src/embedded-recorder.ts" },
    splitting: false,
    dts: false,
    clean: false,
  },
]);
