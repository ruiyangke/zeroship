// Schema-aware augmentation for `env.db`.
//
// `@zeroship/types` declares the base `zeroship` module with a deliberately
// loose `env` (plugin namespaces are attached at runtime). This file is the
// schema-aware half: it rewrites `env.db` to `Db<YourSchema>` so handlers get
// collection-typed access (`env.db.users.find(...)`) with no cast.
//
// Opt in from your app's tsconfig — no import statement in your code:
//
//   "compilerOptions": {
//     "types": ["@zeroship/types", "@zeroship/db/env"],
//     "paths": { "zeroship-schema": ["./src/schema.ts"] }
//   }
//
// The `zeroship-schema` path alias points at the module whose default export
// is your schema (either the schema object itself, or `{ schema }` — both are
// unwrapped below). With that in place:
//
//   import { env } from "zeroship";
//   const db = env.db;            // Db<typeof yourSchema> — no `as`
//
// This is shipped as a hand-authored ambient declaration (not a bundled tsup
// entry) on purpose: bundlers can't reliably carry an ambient
// `declare module` augmentation, and it must stay a standalone file the
// consumer registers via `types`.
import type { Db, SchemaInput } from "./dist/index.js";

type ZeroshipSchemaDefault = typeof import("zeroship-schema").default;
type ZeroshipSchemaShape = ZeroshipSchemaDefault extends { schema: infer S }
  ? S
  : ZeroshipSchemaDefault;
type ZeroshipEnvDb = ZeroshipSchemaShape extends Record<string, SchemaInput>
  ? Db<ZeroshipSchemaShape>
  : ZeroshipDb;

declare module "zeroship" {
  interface Env {
    db: ZeroshipEnvDb;
  }
}

export {};
