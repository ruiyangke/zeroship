import { defineConfig } from "tsup";

// PR #267 fix: BOTH entries in ONE tsup invocation (one `entry: [...]` array
// in a single config object), not two separate `defineConfig` array items.
//
// Root cause this fixes: tsup's DTS bundler (rollup-plugin-dts) bundles each
// entry point's declaration file independently when they run as separate
// invocations. `index.ts` and `internal.ts` both re-export `TypeBuilder` /
// `SchemaBuilder` from the SAME `./types.ts` source, and those classes carry
// a module-private `unique symbol` brand (`TYPE_BUILDER_BRAND`,
// `SCHEMA_BUILDER_BRAND` - see src/types.ts). Two independent DTS bundles
// each re-declare that private symbol locally. The runtime value is
// identical (`Symbol.for(...)`, global registry) but TypeScript's `unique
// symbol` typing is keyed by DECLARATION SITE, not by name or value - so
// `dist/index.d.ts`'s TYPE_BUILDER_BRAND and `dist/internal.d.ts`'s
// TYPE_BUILDER_BRAND were two nominally-DISTINCT types, even though every
// consumer (this package's own tests, `@zeroship/bootstrap` importing from
// `@zeroship/db/internal`) expects `t.string()` (built via the main entry)
// to structurally match a `TypeBuilder` typed through the internal entry.
//
// A single tsup invocation with both entries in one `entry` array lets
// rollup-plugin-dts detect the shared module and hoist it into ONE chunk
// (`dist/live-<hash>.d.ts`) that both `index.d.ts` and `internal.d.ts`
// import from - so the brand is declared exactly once. Verified: before this
// change, `grep -c "declare const TYPE_BUILDER_BRAND" dist/*.d.ts` reported
// 1 in each of index.d.ts and internal.d.ts (2 total, two distinct nominal
// types); after, it reports 0 in each and 1 in the shared chunk (1 total,
// one type). Confirmed test-only: `dist/index.js` and `dist/internal.js`
// are byte-identical to the two-invocation build (JS bundling still runs
// per-entry via `splitting: false`, unaffected by this change) - this is a
// pure `.d.ts` fix, not a runtime/shipping change.
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
