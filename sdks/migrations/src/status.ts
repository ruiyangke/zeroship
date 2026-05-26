/**
 * `migrations.status` — read the persisted state of a migration.
 *
 * Calls `env.db.migrations.status({name, collection})` — separate from
 * the Migration v8_class wrapper because status reads must be safe to
 * call from any worker, including ones that don't own the run. The
 * Migration wrapper's `.status()` method delegates to the same
 * underlying SQL but requires the caller to hold the wrapper's
 * `MIG_LOCK`.
 */

import type { NativeMigrations } from "./native";
import { getNativeMigrations, toNativeError } from "./native";
import type {
  Migration,
  MigrationStatus,
  MigrationStatusSnapshot,
  PlainObject,
  Result,
} from "./types";

export async function statusOf<Row extends PlainObject, Update extends PlainObject>(
  migration: Migration<Row, Update>,
  nativeOverride?: NativeMigrations,
): Promise<Result<MigrationStatusSnapshot>> {
  const native = nativeOverride ?? (await getNativeMigrations());
  try {
    const parsed = await native.status({ name: migration.name, collection: migration.collection });
    return {
      data: {
        name: migration.name,
        collection: migration.collection,
        exists: !!parsed.exists,
        status: (parsed.status as MigrationStatus | null) ?? null,
        cursor: parsed.cursor ?? 0,
        processed: parsed.processed ?? 0,
        isDone: !!parsed.isDone,
        deadLetterPks: Array.isArray(parsed.deadLetterPks) ? parsed.deadLetterPks : [],
        lastError: parsed.error ?? null,
      },
      error: null,
    };
  } catch (e) {
    return { data: null, error: toNativeError(e) };
  }
}
