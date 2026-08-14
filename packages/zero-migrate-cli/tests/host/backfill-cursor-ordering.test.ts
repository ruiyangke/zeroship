// A backfill applies its transform to each row exactly once, whatever the cursor
// values are.
//
// This file exists for a defect that corrupted data silently on the default path.
// A batch records its next checkpoint with
//
//     (SELECT _bf_key_0::text FROM _bf_window ORDER BY _bf_key_0 DESC LIMIT 1)
//
// and PostgreSQL names a cast expression after its underlying column, so the
// OUTPUT column of that select is ALSO `_bf_key_0`. `ORDER BY` resolves an
// output-column name in preference to an input column, so it sorted by the TEXT
// cast rather than by the bigint. Each batch therefore checkpointed at the
// TEXT-maximum of its window.
//
// For a window holding 9 and 10 the text-maximum is '9', so the saved cursor moved
// BACKWARDS from 10 to 9, the next batch re-selected row 10, and a non-idempotent
// transform ran on it twice. The migration reported success and the journal
// recorded a completed backfill.
//
// THE TEST IS WRITTEN AGAINST THE RULE, NOT THE SYMPTOM. Asserting "row 10 is
// applied once" would pass again the moment anything perturbed the ordering, while
// leaving the defect intact for every other value pair. What actually
// characterises it is a disagreement between TEXT and NUMERIC order within one
// window, so each case pairs an inverted cohort with a non-inverted one of the
// same shape. The non-inverted arms passed before the fix too - they are the
// control that says the harness measures ordering rather than backfills in
// general.
//
// The transform is `val = val + 1` from a seeded 0, so the stored value IS the
// number of times the row was visited. A double-apply cannot hide as an
// idempotent-looking result.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_backfill_cursor_ordering";

/** Cohorts whose TEXT order disagrees with their NUMERIC order, and controls that
 *  agree. `'10' < '9'` while `10 > 9`, and likewise at every decade boundary. */
const COHORTS: ReadonlyArray<readonly [string, readonly number[], boolean]> = [
  ["9,10", [9, 10], true],
  ["99,100", [99, 100], true],
  ["999,1000", [999, 1000], true],
  ["2,10", [2, 10], true],
  ["1..12", [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], true],
  // Controls: no text/numeric disagreement anywhere in the cohort.
  ["19,20", [19, 20], false],
  ["89,90", [89, 90], false],
  ["8,9", [8, 9], false],
  ["1,2", [1, 2], false],
];

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function charter(schema: string): string {
  const scope = `{ include = [${JSON.stringify(schema)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

test("a backfill visits every row exactly once, whatever its cursor values sort like", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  try {
    for (const [label, ids, textInverted] of COHORTS) {
      const projectSchema = uniqueNamespace("bf_order");
      const meta = `${projectSchema}_migrations`;

      // `val` counts visits: seeded 0, incremented by 1 per application.
      const seed = {
        name: "seed",
        default: {
          up() {
            table("nums").create({
              columns: { id: t.int().notNull(), val: t.int().notNull() },
              primaryKey: ["id"],
            });
            table("nums").insert({ rows: ids.map((id) => ({ id, val: 0 })) });
          },
        },
      } as MigrationModule & { name: string };

      const bump = {
        name: "bump",
        default: {
          data() {
            table("nums").backfill({
              set: { val: (col) => col("val").add(1) },
              where: (col) => col("id").gt(0),
              cursorColumns: ["id"],
              cursorStability: { mode: "externalInvariant", name: "nums_id_immutable" },
              // Two rows per batch, so an inverted pair shares a window.
              batchSize: 2,
              name: "bump_all",
            });
          },
          inverse() {
            table("nums").backfill({
              set: { val: (col) => col("val").sub(1) },
              where: (col) => col("id").gt(0),
              cursorColumns: ["id"],
              cursorStability: { mode: "externalInvariant", name: "nums_id_immutable" },
              batchSize: 2,
              name: "undo_bump_all",
            });
          },
        },
      } as MigrationModule & { name: string };

      const applyOne = (
        migration: MigrationModule & { name: string },
        priors: (MigrationModule & { name: string })[],
      ) =>
        apply({
          migration,
          priorMigrations: priors,
          priorNameFallbacks: priors.map((p) => p.name),
          ownerApp: OWNER_APP,
          projectSchema,
          driver,
          registry: priors.length ? { nums: OWNER_APP } : {},
          policy: [charter(projectSchema)],
          approved: true,
          appliedBy: "backfill-cursor-ordering",
          nameFallback: migration.name,
        });

      try {
        await admin.query(`CREATE SCHEMA "${projectSchema}"`);
        await applyOne(seed, []);
        await applyOne(bump, [seed]);

        const { rows } = await admin.query(
          `SELECT id, val FROM "${projectSchema}".nums ORDER BY id`,
        );
        const visitedTwice = rows
          .filter((row) => row.val !== 1)
          .map((row) => `id ${row.id} visited ${row.val}x`);

        assert.deepEqual(
          visitedTwice,
          [],
          `${label}${textInverted ? " (text-inverted)" : " (control)"}: every row must be visited exactly once`,
        );

        // Non-vacuity: the cohort really was backfilled, so an "all clean" result
        // cannot come from a backfill that did nothing.
        assert.equal(rows.length, ids.length, `${label}: the cohort must be intact`);

        // And the engine's own count must agree with the table.
        const { rows: progress } = await admin.query(
          `SELECT rows_done FROM "${meta}".schema_backfills`,
        );
        assert.equal(
          Number(progress[0]?.rows_done),
          ids.length,
          `${label}: rows_done must equal the cohort size, not exceed it`,
        );
      } finally {
        await admin
          .query(
            `DROP SCHEMA IF EXISTS "${projectSchema}" CASCADE;
             DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
          )
          .catch(() => {});
      }
    }
  } finally {
    await admin.end().catch(() => {});
  }
});

// The same rule on a COMPOSITE cursor.
//
// `returned_cursor` is generated once per cursor component with the same shape, so
// the missing alias collided on EVERY component of a multi-column cursor, not just
// on a single-column one. A composite cursor also exercises machinery the
// single-column arms never reach: a multi-term `ORDER BY _bf_key_0 DESC,
// _bf_key_1 DESC`, and the lexicographic tuple comparison that resumes from a
// tuple rather than a scalar.
//
// The inversion is placed in each position in turn - second component, first
// component, then both - because a fix that aliased only the leading key would
// pass a test that only ever inverted the leading one.
test("a composite cursor visits every row exactly once under the same inversions", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const CASES: ReadonlyArray<readonly [string, ReadonlyArray<readonly [number, number]>, number]> = [
    ["second component inverted", [[1, 9], [1, 10]], 2],
    ["first component inverted", [[9, 1], [10, 1]], 2],
    ["both inverted", [[9, 9], [9, 10], [10, 9], [10, 10]], 2],
    ["both inverted, one batch", [[9, 9], [9, 10], [10, 9], [10, 10]], 4],
    // Controls: no text/numeric disagreement in either position.
    ["control, no inversion", [[1, 1], [1, 2]], 2],
    ["control, twenties", [[19, 19], [19, 20], [20, 19], [20, 20]], 2],
  ];

  try {
    for (const [label, pairs, batchSize] of CASES) {
      const projectSchema = uniqueNamespace("bf_order_composite");
      const meta = `${projectSchema}_migrations`;

      const seed = {
        name: "seed",
        default: {
          up() {
            table("nums").create({
              columns: {
                tenant: t.int().notNull(),
                id: t.int().notNull(),
                val: t.int().notNull(),
              },
              primaryKey: ["tenant", "id"],
            });
            table("nums").insert({
              rows: pairs.map(([tenant, id]) => ({ tenant, id, val: 0 })),
            });
          },
        },
      } as MigrationModule & { name: string };

      const bump = {
        name: "bump",
        default: {
          data() {
            table("nums").backfill({
              set: { val: (col) => col("val").add(1) },
              where: (col) => col("id").gt(0),
              cursorColumns: ["tenant", "id"],
              cursorStability: { mode: "externalInvariant", name: "nums_key_immutable" },
              batchSize,
              name: "bump_all",
            });
          },
          inverse() {
            table("nums").backfill({
              set: { val: (col) => col("val").sub(1) },
              where: (col) => col("id").gt(0),
              cursorColumns: ["tenant", "id"],
              cursorStability: { mode: "externalInvariant", name: "nums_key_immutable" },
              batchSize,
              name: "undo_bump_all",
            });
          },
        },
      } as MigrationModule & { name: string };

      const applyOne = (
        migration: MigrationModule & { name: string },
        priors: (MigrationModule & { name: string })[],
      ) =>
        apply({
          migration,
          priorMigrations: priors,
          priorNameFallbacks: priors.map((p) => p.name),
          ownerApp: OWNER_APP,
          projectSchema,
          driver,
          registry: priors.length ? { nums: OWNER_APP } : {},
          policy: [charter(projectSchema)],
          approved: true,
          appliedBy: "backfill-cursor-ordering",
          nameFallback: migration.name,
        });

      try {
        await admin.query(`CREATE SCHEMA "${projectSchema}"`);
        await applyOne(seed, []);
        await applyOne(bump, [seed]);

        const { rows } = await admin.query(
          `SELECT tenant, id, val FROM "${projectSchema}".nums ORDER BY tenant, id`,
        );
        assert.deepEqual(
          rows.filter((row) => row.val !== 1).map((row) => `(${row.tenant},${row.id}) x${row.val}`),
          [],
          `${label}: every row must be visited exactly once`,
        );
        assert.equal(rows.length, pairs.length, `${label}: the cohort must be intact`);

        const { rows: progress } = await admin.query(
          `SELECT rows_done FROM "${meta}".schema_backfills`,
        );
        assert.equal(
          Number(progress[0]?.rows_done),
          pairs.length,
          `${label}: rows_done must equal the cohort size`,
        );
      } finally {
        await admin
          .query(
            `DROP SCHEMA IF EXISTS "${projectSchema}" CASCADE;
             DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
          )
          .catch(() => {});
      }
    }
  } finally {
    await admin.end().catch(() => {});
  }
});
