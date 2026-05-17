"use server";

// Backfill the `archived` field for any todos that pre-date its addition.
//
// Exercises @zeroship/migrations (B1):
//   • defineMigration with collection + batchSize + migrateOne
//   • Stateful — state lives in __zeroship_migrations (A3 audit table)
//   • Resumable on crash via the persisted validate_cursor
//   • Dry-runnable — `await backfillArchived.run({ dryRun: true })`
//
// Expand-migrate-contract pattern this represents:
//   1. EXPAND   ship schema with `archived: t.boolean().default(false)` —
//                new rows get the default automatically; old rows have NULL.
//   2. MIGRATE  run this backfill so every row has `archived: false`.
//   3. CONTRACT (optional) ship schema with `archived: t.boolean().required()`
//                in a later deploy if you want to forbid NULLs.

import { defineMigration } from "@zeroship/migrations";

export const backfillArchived = defineMigration({
  name: "todos.backfill_archived",
  collection: "todos",
  batchSize: 100,
  migrateOne: async (doc) => {
    if (doc.archived === undefined || doc.archived === null) {
      return { archived: false };
    }
    // Already migrated — returning undefined skips the row, keeping
    // the migration idempotent on resume.
    return undefined;
  },
});
