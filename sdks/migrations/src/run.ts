/**
 * `migrations.run` — orchestrate a backfill end-to-end.
 *
 * Surface: the SDK calls `env.db.migrations.start(spec)` to mint a
 * Migration v8_class wrapper, then drives the loop:
 *
 *   1. `m.fetchBatch(cursor, batchSize)` → `{ rows }`.
 *   2. For each row, call `migrateOne(row, ctx)`. Build an updates
 *      list. Per-row throws push the row's id onto a dead-letter list
 *      (subject to `failureBudget`).
 *   3. `m.commitBatch(updates, deadLetterPks, nextCursor, processed,
 *      false, "", "")`. The native side BEGIN/COMMITs; dry-runs
 *      ROLLBACK.
 *   4. Loop until `rows.length === 0`.
 *   5. Final `m.commitBatch(..., true, terminal, error)` drives the
 *      audit row to its terminal state and releases the advisory
 *      lock. After `isDone=true`, the wrapper's internal `inner` is
 *      cleared so the GC finalizer no longer auto-cancels.
 *
 * On cancellation (another worker / operator), `fetchBatch` throws
 * `migration_cancelled` — the SDK returns the cancelled status
 * without calling the terminal commitBatch (the cancel already
 * settled the audit row).
 */

import type { NativeMigration, NativeMigrations } from "./native.js";
import { getNativeMigrations, toNativeError } from "./native.js";
import type {
  DeadLetterEntry,
  Migration,
  MigrationStatus,
  PlainObject,
  Result,
  RunOptions,
  RunResult,
} from "./types.js";

/**
 * Run a migration to completion (or to a terminal failure). Always
 * returns a Result — never throws.
 *
 * Note: `resume` is the default. Pass `reset: true` to start from
 * scratch (clears persisted state before the run). The two options
 * are mutually exclusive — `reset` wins if both are set.
 */
export async function runMigration<Row extends PlainObject, Update extends PlainObject>(
  migration: Migration<Row, Update>,
  options: RunOptions = {},
  nativeOverride?: NativeMigrations,
): Promise<Result<RunResult>> {
  const native = nativeOverride ?? (await getNativeMigrations());
  const dryRun = options.dryRun === true;
  const reset = options.reset === true;

  let m: NativeMigration;
  try {
    m = await native.start({
      name: migration.name,
      collection: migration.collection,
      dryRun,
      reset,
    });
  } catch (e) {
    return { data: null, error: toNativeError(e) };
  }

  let cursor = 0;
  let processed = 0;
  const deadLetters: DeadLetterEntry[] = [];
  let failures = 0;
  const budget = migration.failureBudget ?? 0;

  /** PK-only view of `deadLetters` — the audit row stores PKs only,
   *  so this is what we ship to native.commitBatch. The richer entry
   *  list with `.error` lives in the JS-side RunResult. */
  function deadLetterPks(): number[] {
    return deadLetters.map((d) => d.id);
  }

  // Helper to send the final commit. `terminal` is the audit status
  // we want persisted; `error` is the message stored in the audit row.
  // Returns a Result rather than a bare RunResult so the caller can
  // surface a post-commit failure (audit row UPDATE rejected) instead
  // of swallowing it and lying about "applied".
  async function finish(
    terminal: MigrationStatus,
    error: string,
  ): Promise<Result<RunResult>> {
    const pks = deadLetterPks();
    try {
      await m.commitBatch({
        updates: [],
        deadLetterPks: pks,
        nextCursor: cursor,
        processedTotal: processed,
        isDone: true,
        terminalStatus: terminal,
        errorMessage: error,
      });
    } catch (e) {
      // Audit row update / advisory-lock release failed AFTER the SDK
      // already drove every per-row update through native. Surface as
      // an error — the migration did not fully apply if its terminal
      // commit didn't reach Postgres.
      return { data: null, error: toNativeError(e) };
    }
    return {
      data: {
        status: terminal,
        processed,
        deadLetters,
        deadLetterPks: pks,
        cursor,
      },
      error: null,
    };
  }

  // Main loop.
  for (;;) {
    let rows: PlainObject[];
    try {
      rows = (await m.fetchBatch(cursor, migration.batchSize)) as PlainObject[];
    } catch (e) {
      const err = toNativeError(e);
      if (err.code === "migration_cancelled") {
        // Don't try to drive the audit row — cancel already
        // terminalised it.
        return {
          data: {
            status: "cancelled",
            processed,
            deadLetters,
            deadLetterPks: deadLetterPks(),
            cursor,
          },
          error: null,
        };
      }
      return { data: null, error: err };
    }

    if (rows.length === 0) {
      const terminal: MigrationStatus = deadLetters.length > 0
        ? "applied_with_dead_letter"
        : "applied";
      return finish(terminal, "");
    }

    const updates: Array<{ id: number; set: PlainObject }> = [];
    let nextCursor = cursor;
    for (const row of rows) {
      const id = typeof row.id === "number" ? row.id : Number(row.id);
      if (Number.isFinite(id) && id > nextCursor) nextCursor = id;

      let out: Update | undefined | null;
      try {
        out = await migration.migrateOne(row as Row, {
          // auditId is no longer surfaced through the entry point —
          // the row count is what the migrate callback needs in
          // practice; if a use case for auditId emerges, expose a
          // `m.auditId` getter on the wrapper.
          auditId: 0,
          cursor,
          processed,
        });
      } catch (rowErr) {
        failures += 1;
        if (failures > budget) {
          const msg = rowErr instanceof Error ? rowErr.message : String(rowErr);
          return finish(
            "failed",
            `migration_failure_budget_exceeded: ${msg}`,
          );
        }
        const err = rowErr instanceof Error ? rowErr : new Error(String(rowErr));
        deadLetters.push({ id, error: err });
        continue;
      }

      if (out === null) {
        // Caller explicitly asked to dead-letter this row. Synthesise
        // an Error so the entry is uniform — consumers can branch on
        // `.error.message === "dead_letter"` if they need to distinguish
        // explicit-opt-out from a thrown handler failure.
        deadLetters.push({ id, error: new Error("dead_letter") });
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
      await m.commitBatch({
        updates,
        deadLetterPks: deadLetterPks(),
        nextCursor,
        processedTotal: processed,
        isDone: false,
      });
    } catch (e) {
      return { data: null, error: toNativeError(e) };
    }

    cursor = nextCursor;
    // If fewer rows came back than asked, the table is drained — but
    // we still want one zero-row fetch so the terminal commit path
    // runs uniformly. Continue the loop.
  }
}
