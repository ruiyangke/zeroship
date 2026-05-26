/**
 * `migrations.cancel` and `migrations.reset` — operator-facing
 * lifecycle controls.
 *
 * Cancel transitions a `pending` or `running` run to `cancelled`; the
 * next `fetchBatch` call observes the state change and
 * surfaces `migration_cancelled` to the running worker.
 *
 * Reset clears persisted state so an operator can re-run from scratch
 * after a `cancelled` or `failed` run.
 */

import type { NativeMigrations } from "./native";
import { getNativeMigrations, toNativeError } from "./native";
import type { Migration, PlainObject, Result } from "./types";

export async function cancelMigration<Row extends PlainObject, Update extends PlainObject>(
  migration: Migration<Row, Update>,
  nativeOverride?: NativeMigrations,
): Promise<Result<{ ok: boolean }>> {
  const native = nativeOverride ?? (await getNativeMigrations());
  try {
    await native.cancel({ name: migration.name, collection: migration.collection });
    return { data: { ok: true }, error: null };
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
    await native.reset({ name: migration.name, collection: migration.collection });
    return { data: { ok: true }, error: null };
  } catch (e) {
    return { data: null, error: toNativeError(e) };
  }
}
