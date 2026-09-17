import { defineConfig } from "tsup";

// The DB adapter is embedded in the worker as `zeroship:db/adapter` and evaluated
// inside V8, where NO bare npm specifier resolves. Everything it needs must be
// inlined except the `zeroship` platform module, which the runtime provides.
//
// `@zeroship/schema` must be INLINED here. The adapter runs inside V8, where no
// bare npm specifier resolves, so a surviving `import { TypeBuilder, t, decimal }
// from "@zeroship/schema"` fails every module that touches a collection with
// "Cannot resolve import '@zeroship/schema' from 'zeroship:db/adapter'".
//
// It is a devDependency of this package and is bundled into `dist/index.js` as
// well, which makes tsup inline it by default; `noExternal` states that intent
// explicitly so it cannot regress if the declaration ever moves back to
// `dependencies`. tsup's CLI has no flag for it, which is why this file exists.
//
// It lives HERE, in packages/db, rather than beside the adapter: tsup bundles the
// config and executes it from the config's own directory, and `tsup` only
// resolves from this package. Paths below are therefore relative to packages/db,
// which is also the CWD the root build uses (`pnpm --dir packages/db exec tsup`).
export default defineConfig({
  entry: ["../../crates/zeroship-data-v8/js/adapter.ts"],
  format: ["esm"],
  platform: "neutral",
  target: "es2022",
  outDir: "../../crates/zeroship-data-v8/dist",
  clean: true,
  external: ["zeroship"],
  noExternal: [/^@zeroship\/schema$/],
});