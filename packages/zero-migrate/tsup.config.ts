import { defineConfig, type Options } from "tsup";

const shared = {
  format: ["esm"],
  // `resolve: true` inlines the leaf's declarations. Without it dist/index.d.ts
  // imports `@zeroship/schema`, a dev dependency: the published tarball would
  // ship types that resolve nowhere, and publish-packages.sh's dependency check
  // cannot see a types-only reference.
  dts: { resolve: true },
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  treeshake: true,
  splitting: true,
  // The schema lexicon is a DEV dependency that must be INLINED: it is the one
  // lexicon both packages share, and bundling it is what keeps the published
  // migration package's zero-runtime-dependency promise intact.
  noExternal: [/^@zeroship\/schema$/],
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
  // code-splitting (single file), no `.d.ts` (build artifact only). The db
  // type-builder (`@zeroship/schema`) is bundled in — there is no external db dep.
  {
    ...shared,
    entry: { "embedded-recorder": "src/embedded-recorder.ts" },
    splitting: false,
    dts: false,
    clean: false,
  },
]);
