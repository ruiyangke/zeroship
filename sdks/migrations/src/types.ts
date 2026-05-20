/**
 * Public types for @zeroship/migrations.
 */

/** A plain JSON-shaped object used for row inputs/outputs. */
export type PlainObject = Record<string, unknown>;

/** Result envelope shared with @zeroship/db. */
export type Result<T> = { data: T; error: null } | { data: null; error: Error };

/**
 * Lifecycle status for a backfill migration, mirroring the
 * `__zeroship_migrations.status` enum in the database (proposal A3/B1).
 */
export type MigrationStatus =
  | "pending"
  | "running"
  | "applied"
  | "applied_with_dead_letter"
  | "failed"
  | "cancelled";

/**
 * A user-defined migration. Produced by `defineMigration` and consumed
 * by `migrations.run`, `.status`, `.cancel`.
 *
 * @template Row Shape of a row as returned by the native fetch primitive
 *               (raw column → JSON value).
 * @template Update Shape of the per-row diff returned by `migrateOne`.
 */
export interface Migration<Row extends PlainObject = PlainObject, Update extends PlainObject = PlainObject> {
  /** Stable migration name. Persisted in `change_kind`. */
  readonly name: string;
  /** Target collection (Postgres table). */
  readonly collection: string;
  /** Rows per batch. SDK loops until the native side returns an empty batch. */
  readonly batchSize: number;
  /**
   * Per-row transform. Receives the row as JSON; the return value
   * controls what happens to the row:
   *
   * - **Patch** — return an object `{ col: value, ... }`. The SDK
   *   queues an UPDATE so this row's columns are written on the next
   *   commit. Field names are taken verbatim — pre-name columns (e.g.
   *   raw `event_type`) and JS-side camelCase keys both work; the SDK
   *   does not re-map.
   * - **Skip** — return `undefined`. The row is left untouched and
   *   counted under `processed` but not under `deadLetters`. Use this
   *   when a row is already in the post-migration shape.
   * - **Dead-letter** — return `null`. The row's primary key (and a
   *   synthetic `Error("dead_letter")`) is appended to
   *   `RunResult.deadLetters`. The audit row's
   *   `dead_letter_pks` column records the PK. Use this when the row
   *   is permanently un-migratable (e.g. corrupt input) but you want
   *   the rest of the batch to keep going.
   *
   * Throws inside this callback are caught by the SDK loop and counted
   * against `failureBudget`. Over budget, the run terminates with
   * status `"failed"`. Under budget, the thrown error is captured into
   * `deadLetters` so the caller can inspect what went wrong.
   */
  readonly migrateOne: (row: Row, ctx: MigrateContext) => Update | undefined | null | Promise<Update | undefined | null>;
  /**
   * Per-row failure budget. If `migrateOne` throws more than this many
   * times, the run terminates as `failed`. Defaults to 0 (any throw fails).
   */
  readonly failureBudget?: number;
}

/** Context passed to `migrateOne`. Carries the active audit-row id. */
export interface MigrateContext {
  readonly auditId: number;
  readonly cursor: number;
  readonly processed: number;
}

/** Snapshot returned by `migrations.status`. */
export interface MigrationStatusSnapshot {
  readonly name: string;
  readonly collection: string;
  readonly exists: boolean;
  readonly status: MigrationStatus | null;
  readonly processed: number;
  readonly cursor: number;
  readonly isDone: boolean;
  readonly deadLetterPks: number[];
  readonly lastError: string | null;
}

/** Options for `migrations.run`. */
export interface RunOptions {
  /** If true, every UPDATE rolls back and audit state is not advanced. */
  dryRun?: boolean;
  /** If true (default), resumes from the persisted cursor. */
  resume?: boolean;
  /** If true, clears persisted state before starting (status='pending', cursor=0). */
  reset?: boolean;
}

/** One dead-letter entry — the row's primary key plus the error
 *  `migrateOne` threw (or returned `null` to opt out). */
export interface DeadLetterEntry {
  readonly id: number;
  readonly error: Error;
}

/** Final result returned by `migrations.run`. */
export interface RunResult {
  readonly status: MigrationStatus;
  readonly processed: number;
  /**
   * Rows the SDK loop set aside instead of patching. Each entry carries
   * the row's primary key plus the error `migrateOne` threw (or a
   * synthetic `Error("dead_letter")` if the callback explicitly
   * returned `null` to opt out of the patch). Mirrors the native
   * `dead_letter_pks` column on the audit row.
   */
  readonly deadLetters: DeadLetterEntry[];
  /**
   * Bare-id view of {@link deadLetters} — handy for callers that only
   * need the keys (e.g. to drive a retry batch). Equal to
   * `deadLetters.map(d => d.id)`.
   */
  readonly deadLetterPks: number[];
  readonly cursor: number;
}
