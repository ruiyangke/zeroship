/**
 * Test-only adapter for the Stage-6 `installSchema` shape.
 *
 * Stage 6 of the @zeroship/db refactor replaced the legacy
 * `_installSchema(schemas, { native, ... })` return-shape (`Db<T>` =
 * collections + `transaction` + `live`) with
 * `installSchema(schemas, env) → { collections, ready }`. The
 * `transaction` / `live` extension methods are now planted on the
 * supplied `env` (the native handle) instead of being on the return
 * value — which is the right shape for production (`env.db.users.find`,
 * `env.db.transaction(...)`) but inconvenient for the tens of test
 * call sites that captured `const db = _installSchema(...)` and then
 * called `db.transaction(...)`.
 *
 * This helper bridges the test surface back: it calls `installSchema`,
 * then assembles a `Db<T>`-shaped object by composing `collections`
 * with the `transaction` / `live` methods the install just planted on
 * the supplied mock env. The composition stays minimal — no new
 * behaviour, no extra runtime cost — so the test assertions still
 * exercise the same code paths as production. Production callers
 * should NOT use this helper; read collections off `env.db` and call
 * `env.db.transaction(...)` directly.
 */
import { installSchema } from "../src/db.js";
import type { Db, InstallSchemaOptions } from "../src/db.js";
import type { NativeDb } from "../src/collection.js";

export function installSchemaForTest<
  const T extends Record<string, unknown>,
>(
  schemas: T,
  opts: { native: NativeDb; naming?: InstallSchemaOptions["naming"] },
): Db<T> {
  // Cast schemas through `any` — the ValidateSchemaShape constraint on
  // `installSchema` is type-only and the test fixtures pass conforming
  // shapes; tightening the generic here would force every test to
  // re-state the constraint.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const result = installSchema(schemas as any, opts.native, opts.naming ? { naming: opts.naming } : undefined);
  // After install, the native handle carries the per-collection wrappers
  // and the `transaction` / `live` extensions as own properties. Return
  // it as the `Db<T>` shape so existing call sites
  // (`db.users.find(...)`, `db.transaction(tx => ...)`) keep working.
  return opts.native as unknown as Db<T>;
}
