/**
 * Ambient declaration binding `env.db` to the user's schema.
 *
 * The convention: the app's entry module exports
 * `export default { schema: {...} }`. When the user's tsconfig has
 *
 *   { "compilerOptions": { "paths": { "zeroship-schema": ["./src/index.ts"] } } }
 *
 * TypeScript resolves `import("zeroship-schema").default.schema` to
 * the literal schema object, and the augmentation below narrows
 * `env.db` from the loose native `ZeroshipDb` handle to a typed
 * `Db<typeof schema>` — so `env.db.users.find(...)` typechecks against
 * the declared users collection.
 *
 * When the user hasn't configured the `zeroship-schema` path,
 * `typeof import("zeroship-schema").default` errors out at the augmentation
 * site (the file is part of `@zeroship/types`'s shipped declarations);
 * the typedef chain below short-circuits via `[S] extends [never]` so
 * `env.db` quietly stays as the bare `ZeroshipDb` native handle in that
 * case. No fallback `declare module "zeroship-schema"` block is needed —
 * declaring one would conflict with the user's real schema file under
 * the `paths` alias and re-emit a duplicate-default-export error.
 */

import type { Db } from "@zeroship/db";

// Pull the user's default export shape via the `zeroship-schema` alias.
// Lifting these typedefs out of the augmentation block keeps the
// conditional readable AND, more importantly, lets TS resolve
// `typeof import(...)` in the file's own context — inline conditionals
// inside `declare module "zeroship"` were dropping the resolution to
// `unknown` in practice.
type ZeroshipSchemaDefault = typeof import("zeroship-schema").default;
type ZeroshipSchemaShape = ZeroshipSchemaDefault extends { schema: infer S }
  ? S
  : never;
type ZeroshipEnvDb = [ZeroshipSchemaShape] extends [never]
  ? ZeroshipDb
  : ZeroshipSchemaShape extends Record<string, unknown>
    ? Db<ZeroshipSchemaShape>
    : ZeroshipDb;

declare module "zeroship" {
  interface Env {
    db: ZeroshipEnvDb;
    auth?: ZeroshipAuth;
  }
}
