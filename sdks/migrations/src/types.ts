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
   * Per-row transform. Receives the row as JSON; returns the column
   * updates to apply (`{ col: value, ... }`), `undefined` to skip,
   * or `null` to dead-letter the row.
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

/** Final result returned by `migrations.run`. */
export interface RunResult {
  readonly status: MigrationStatus;
  readonly processed: number;
  readonly deadLetterPks: number[];
  readonly cursor: number;
}
