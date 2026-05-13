/**
 * `migrations.cancel` and `migrations.reset` — operator-facing
 * lifecycle controls.
 *
 * Cancel transitions a `pending` or `running` run to `cancelled`; the
 * next `migrationFetchBatch` call observes the state change and
 * surfaces `migration_cancelled` to the running worker.
 *
 * Reset clears persisted state so an operator can re-run from scratch
 * after a `cancelled` or `failed` run.
 */

import type { NativeMigrations } from "./native.js";
import { getNativeMigrations, parseNative, toNativeError } from "./native.js";
import type { Migration, PlainObject, Result } from "./types.js";

interface OkResponse {
  ok: boolean;
}

export async function cancelMigration<Row extends PlainObject, Update extends PlainObject>(
  migration: Migration<Row, Update>,
  nativeOverride?: NativeMigrations,
): Promise<Result<{ ok: boolean }>> {
  const native = nativeOverride ?? (await getNativeMigrations());
  try {
    const raw = await native.migrationCancel(migration.name, migration.collection);
    const parsed = parseNative<OkResponse>(raw);
    return { data: { ok: !!parsed.ok }, error: null };
  } catch (e) {
    return { data: null, error: toNativeError(e) };
  }
}

export async function resetMigration<Row extends PlainObject, Update extends PlainObject>(
  migration: Migration<Row, Update>,
  nativeOverride?: NativeMigrations,
): Promise<Result<{ ok: boolean }>> {
  const native = nativeOverride ?? (await getNativeMigrations());
  try {
    const raw = await native.migrationReset(migration.name, migration.collection);
    const parsed = parseNative<OkResponse>(raw);
    return { data: { ok: !!parsed.ok }, error: null };
  } catch (e) {
    return { data: null, error: toNativeError(e) };
  }
}
