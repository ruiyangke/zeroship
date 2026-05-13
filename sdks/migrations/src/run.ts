/**
 * `migrations.run` — orchestrate a backfill end-to-end.
 *
 * Loop, in JS:
 *   1. `migrationBegin(name, collection, dryRun, reset)` →
 *      `{ auditId, cursor, processed, deadLetterPks }`. Acquires the
 *      Postgres advisory lock; subsequent runs from other workers fail
 *      with `migration_already_running`.
 *   2. Repeatedly:
 *        a. `migrationFetchBatch(cursor, batchSize)` → `{ rows }`.
 *        b. For each row, call `migrateOne(row, ctx)`. Build an updates
 *           list. Per-row throws push the row's id onto a dead-letter
 *           list (subject to `failureBudget`).
 *        c. `migrationCommitBatch(updates, deadLetterPks, nextCursor,
 *           processed, false, "", "")`. The native side BEGIN/COMMITs;
 *           dry-runs ROLLBACK.
 *      until rows.length === 0.
 *   3. Final `migrationCommitBatch(..., true, terminal, error)` to drive
 *      the audit row to its terminal state and release the lock.
 *
 * All loop state is local to this function — the native side stores
 * only what's persisted in `__zeroship_migrations` for crash recovery.
 */

import type { NativeMigrations } from "./native.js";
import { getNativeMigrations, parseNative, toNativeError } from "./native.js";
import type {
  Migration,
  MigrationStatus,
  PlainObject,
  Result,
  RunOptions,
  RunResult,
} from "./types.js";

interface BeginResponse {
  auditId: number;
  cursor: number;
  processed: number;
  status: MigrationStatus;
  deadLetterPks: number[];
}

interface FetchResponse {
  rows: PlainObject[];
}

/**
 * Run a migration to completion (or to a terminal failure). Always
 * returns a Result — never throws.
 *
 * Note: `resume` is the default. Pass `reset: true` to start from
 * scratch (clears persisted state before the run). The two options are
 * mutually exclusive — `reset` wins if both are set.
 */
export async function runMigration<Row extends PlainObject, Update extends PlainObject>(
  migration: Migration<Row, Update>,
  options: RunOptions = {},
  nativeOverride?: NativeMigrations,
): Promise<Result<RunResult>> {
  const native = nativeOverride ?? (await getNativeMigrations());
  const dryRun = options.dryRun === true;
  const reset = options.reset === true;

  let begin: BeginResponse;
  try {
    const raw = await native.migrationBegin(
      migration.name,
      migration.collection,
      dryRun,
      reset,
    );
    begin = parseNative<BeginResponse>(raw);
  } catch (e) {
    return { data: null, error: toNativeError(e) };
  }

  let cursor = begin.cursor ?? 0;
  let processed = begin.processed ?? 0;
  const deadLetter: number[] = Array.isArray(begin.deadLetterPks)
    ? [...begin.deadLetterPks]
    : [];
  let failures = 0;
  const budget = migration.failureBudget ?? 0;

  // Helper to send the final commit. `terminal` is the audit status
  // we want persisted; `error` is the message stored in the audit row.
  async function finish(
    terminal: MigrationStatus,
    error: string,
  ): Promise<RunResult> {
    try {
      const raw = await native.migrationCommitBatch(
        JSON.stringify([]),
        JSON.stringify(deadLetter),
        cursor,
        processed,
        true,
        terminal,
        error,
      );
      parseNative<unknown>(raw);
    } catch (e) {
      // Suppress — the audit row was already touched and the loop is
      // unwinding. Re-throwing would lose the terminal status the
      // caller needs.
      void toNativeError(e);
    }
    return { status: terminal, processed, deadLetterPks: deadLetter, cursor };
  }

  // Main loop.
  for (;;) {
    let fetched: FetchResponse;
    try {
      const raw = await native.migrationFetchBatch(cursor, migration.batchSize);
      fetched = parseNative<FetchResponse>(raw);
    } catch (e) {
      const err = toNativeError(e);
      if (err.code === "migration_cancelled" || err.message.includes("migration_cancelled")) {
        // Don't try to drive the audit row — cancel already terminalised it.
        return {
          data: { status: "cancelled", processed, deadLetterPks: deadLetter, cursor },
          error: null,
        };
      }
      return { data: null, error: err };
    }

    const rows = fetched.rows ?? [];
    if (rows.length === 0) {
      const terminal: MigrationStatus = deadLetter.length > 0
        ? "applied_with_dead_letter"
        : "applied";
      const result = await finish(terminal, "");
      return { data: result, error: null };
    }

    const updates: Array<{ id: number; set: PlainObject }> = [];
    let nextCursor = cursor;
    for (const row of rows) {
      const id = typeof row.id === "number" ? row.id : Number(row.id);
      if (Number.isFinite(id) && id > nextCursor) nextCursor = id;

      let out: Update | undefined | null;
      try {
        out = await migration.migrateOne(row as Row, {
          auditId: begin.auditId,
          cursor,
          processed,
        });
      } catch (rowErr) {
        failures += 1;
        if (failures > budget) {
          const msg = rowErr instanceof Error ? rowErr.message : String(rowErr);
          const result = await finish(
            "failed",
            `migration_failure_budget_exceeded: ${msg}`,
          );
          return { data: result, error: null };
        }
        deadLetter.push(id);
        continue;
      }

      if (out === null) {
        // Caller asked to dead-letter this row explicitly.
        deadLetter.push(id);
        continue;
      }
      if (out === undefined) {
        // Skip — no change.
        continue;
      }
      updates.push({ id, set: out as PlainObject });
    }

    processed += rows.length;

    try {
      const raw = await native.migrationCommitBatch(
        JSON.stringify(updates),
        JSON.stringify(deadLetter),
        nextCursor,
        processed,
        false,
        "",
        "",
      );
      parseNative<unknown>(raw);
    } catch (e) {
      return { data: null, error: toNativeError(e) };
    }

    cursor = nextCursor;
    // If fewer rows came back than asked, the table is drained — but
    // we still want one zero-row fetch so the terminal commit path
    // runs uniformly. Continue the loop.
  }
}
