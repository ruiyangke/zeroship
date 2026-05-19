/**
 * `migrations.status` — read the persisted state of a migration.
 *
 * Calls the standalone `env.db.migrationStatus(name, collection)`
 * primitive — separate from the Migration v8_class wrapper because
 * status reads must be safe to call from any worker, including ones
 * that don't own the run. The Migration wrapper's `.status()` method
 * delegates to the same underlying SQL but requires the caller to
 * hold the wrapper's `MIG_LOCK`.
 */

import type { NativeMigrations } from "./native.js";
import { getNativeMigrations, parseNative, toNativeError } from "./native.js";
import type {
  Migration,
  MigrationStatus,
  MigrationStatusSnapshot,
  PlainObject,
  Result,
} from "./types.js";

interface NativeStatus {
  exists: boolean;
  status: MigrationStatus | null;
  cursor: number;
  processed: number;
  deadLetterPks: number[];
  isDone: boolean;
  error: string | null;
}

export async function statusOf<Row extends PlainObject, Update extends PlainObject>(
  migration: Migration<Row, Update>,
  nativeOverride?: NativeMigrations,
): Promise<Result<MigrationStatusSnapshot>> {
  const native = nativeOverride ?? (await getNativeMigrations());
  try {
    const raw = await native.migrationStatus(migration.name, migration.collection);
    const parsed = parseNative<NativeStatus>(raw);
    return {
      data: {
        name: migration.name,
        collection: migration.collection,
        exists: !!parsed.exists,
        status: parsed.status ?? null,
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
