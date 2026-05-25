/**
 * Test-only adapter for the Stage-7 `installSchema` shape.
 *
 * Stage 7 of the refactor moved `installSchema` into `@zeroship/bootstrap`
 * (the framework-internal coordination package). Tests in `@zeroship/db`
 * call it for setup convenience — they're testing Collection/Query
 * user-facing behaviour and need a Db<T>-shaped object to run their
 * assertions against.
 *
 * The reverse direction (db → bootstrap as a dev-only dep) is fine —
 * the production graph (bootstrap → db) stays acyclic; the
 * devDependency edge only exists at test time.
 */
import { installSchema, type InstallSchemaOptions } from "@zeroship/bootstrap/install-schema";
import type { Db } from "../src/db-types.js";
import type { NativeDb } from "../src/native.js";

export function installSchemaForTest<
  const T extends Record<string, unknown>,
>(
  schemas: T,
  opts: { native: NativeDb; naming?: InstallSchemaOptions["naming"] },
): Db<T> {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  installSchema(schemas as any, opts.native, opts.naming ? { naming: opts.naming } : undefined);
  // After install, the native handle carries the per-collection wrappers
  // and the `transaction` / `live` extensions as own properties. Return
  // it as the `Db<T>` shape so existing call sites
  // (`db.users.find(...)`, `db.transaction(tx => ...)`) keep working.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  return opts.native as unknown as Db<T>;
}
