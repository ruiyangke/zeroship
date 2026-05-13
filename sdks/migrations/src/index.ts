/**
 * @zeroship/migrations — online data-backfill orchestrator.
 *
 * ```ts
 * import { defineMigration, migrations } from "@zeroship/migrations";
 *
 * const backfillRole = defineMigration({
 *   collection: "users",
 *   name: "backfill_role_2026_05",
 *   batchSize: 200,
 *   migrateOne: (doc) => {
 *     if (doc.role === undefined) return { role: "user" };
 *   },
 * });
 *
 * await migrations.run(backfillRole);
 * await migrations.run(backfillRole, { dryRun: true });
 *
 * const { data: status } = await migrations.status(backfillRole);
 * if (status?.status === "running") await migrations.cancel(backfillRole);
 * ```
 *
 * The native side (Rust `zeroship-plugin-db::migrations`) owns the
 * advisory lock, audit row, dry-run rollback, and SQL execution. The
 * SDK owns the JS orchestration loop, per-row error handling, and the
 * dead-letter / failure-budget bookkeeping.
 */

"use server";

export { defineMigration } from "./define.js";
export { runMigration } from "./run.js";
export { statusOf } from "./status.js";
export { cancelMigration, resetMigration } from "./cancel.js";

export type {
  Migration,
  MigrateContext,
  MigrationStatus,
  MigrationStatusSnapshot,
  PlainObject,
  Result,
  RunOptions,
  RunResult,
} from "./types.js";
export type { DefineMigrationInput } from "./define.js";
export type { NativeMigrations } from "./native.js";

import type { Migration, PlainObject, Result, RunOptions, RunResult, MigrationStatusSnapshot } from "./types.js";
import type { NativeMigrations } from "./native.js";
import { runMigration } from "./run.js";
import { statusOf } from "./status.js";
import { cancelMigration, resetMigration } from "./cancel.js";

/**
 * Convenience namespace mirroring the proposal's surface
 * (`migrations.run(...)`, `migrations.status(...)`, etc.).
 */
export const migrations = {
  run: <Row extends PlainObject, Update extends PlainObject>(
    migration: Migration<Row, Update>,
    options?: RunOptions,
    native?: NativeMigrations,
  ): Promise<Result<RunResult>> => runMigration(migration, options, native),

  status: <Row extends PlainObject, Update extends PlainObject>(
    migration: Migration<Row, Update>,
    native?: NativeMigrations,
  ): Promise<Result<MigrationStatusSnapshot>> => statusOf(migration, native),

  cancel: <Row extends PlainObject, Update extends PlainObject>(
    migration: Migration<Row, Update>,
    native?: NativeMigrations,
  ): Promise<Result<{ ok: boolean }>> => cancelMigration(migration, native),

  reset: <Row extends PlainObject, Update extends PlainObject>(
    migration: Migration<Row, Update>,
    native?: NativeMigrations,
  ): Promise<Result<{ ok: boolean }>> => resetMigration(migration, native),
} as const;
