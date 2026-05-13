/**
 * `migrations.status` — read the persisted state of a migration.
 *
 * Wraps the native `migrationStatus(name, collection)` primitive and
 * normalises its JSON envelope into a `MigrationStatusSnapshot`.
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
