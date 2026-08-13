// A backfill whose predicate selects only PART of the table, across many batches.
//
// `backfill-cursor-ordering.test.ts` covers the cursor: every row visited exactly
// once, whatever the key values sort like. But its cohort is `col("id").gt(0)`,
// which matches every row in the table - so it says nothing about what happens
// when the predicate genuinely excludes rows.
//
// That distinction is where a paging backfill goes wrong. The cursor advances
// through a filtered set while the batch window is drawn over the table, and the
// two can disagree at a boundary: a batch whose rows are all excluded can leave
// the cursor parked, or advance it past rows that should have been included. The
// symptom is not an error. It is a backfill that reports success having skipped
// some of its cohort, which is F452's failure mode in a different disguise - and
// F452 is the worst defect this session found.
//
// SO THE COHORT IS INTERLEAVED, not contiguous. Twenty rows alternating between
// two groups, with a batch size that divides neither group evenly, so the
// selected rows straddle batch boundaries rather than filling batches. A
// contiguous cohort would let a broken pager land on the right answer.
//
// TWO ASSERTIONS, and the second is the one with teeth:
//
//   1. every selected row was transformed;
//   2. every UNSELECTED row is untouched.
//
// (2) is what catches a backfill that ignored its predicate and rewrote the
// table, which is silent and destructive and exactly what a migration would do
// at scale. (1) alone would pass for it.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_backfill_selective";

/** Twenty rows, alternating groups, so the cohort straddles every batch. */
const ROWS = Array.from({ length: 20 }, (_, index) => ({
  id: index + 1,
  grp: index % 2 === 0 ? "touch" : "keep",
  val: 0,
}));
const SELECTED = ROWS.filter((row) => row.grp === "touch").map((row) => row.id);

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function charter(scopeName: string): string {
  const scope = `{ include = [${JSON.stringify(scopeName)}] }`;
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

test("a backfill transforms every selected row and leaves every other row alone", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("bfsel");
  const meta = `${schema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const seed = {
    name: "seed",
    default: {
      up() {
        table("items").create({
          columns: {
            id: t.int().notNull(),
            grp: t.text().notNull(),
            val: t.int().notNull(),
          },
          primaryKey: ["id"],
        });
        table("items").insert({ rows: ROWS });
      },
    },
  } as MigrationModule & { name: string };

  const fill = {
    name: "fill",
    default: {
      up() {
        table("items").backfill({
          set: { val: (col) => col("val").add(1) },
          // The selective predicate. Ten of twenty rows, every other one.
          where: (col) => col("grp").eq("touch"),
          cursorColumns: ["id"],
          cursorStability: { mode: "externalInvariant", name: "items_id_immutable" },
          // Divides neither the table nor the cohort evenly, so selected rows
          // land at batch boundaries rather than filling batches.
          batchSize: 3,
          name: "fill_touch",
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
      priorNameFallbacks: priors.map((prior) => prior.name),
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver,
      registry: priors.length ? { items: OWNER_APP } : {},
      policy: [charter(schema)],
      approved: true,
      appliedBy: "backfill-selective-cohort",
      nameFallback: migration.name,
    });

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    await applyOne(seed, []);
    await applyOne(fill, [seed]);

    const { rows } = await client.query(
      `SELECT id, grp, val FROM "${schema}".items ORDER BY id`,
    );
    assert.equal(rows.length, ROWS.length, "no row may be added or lost");

    // (1) Every selected row transformed, named individually so a failure says
    // WHICH rows the pager skipped rather than only that some count is wrong.
    const missed = rows
      .filter((row) => row.grp === "touch" && Number(row.val) !== 1)
      .map((row) => Number(row.id));
    assert.deepEqual(
      missed,
      [],
      `every selected row must be transformed exactly once; cohort was ${SELECTED.join(",")}`,
    );

    // (2) And nothing else was. This is the arm with teeth: a backfill that
    // ignored its predicate satisfies (1) completely.
    const collateral = rows
      .filter((row) => row.grp === "keep" && Number(row.val) !== 0)
      .map((row) => Number(row.id));
    assert.deepEqual(
      collateral,
      [],
      "a row outside the cohort must not be touched",
    );

    // Non-vacuity: the journal must show the backfill actually ran over the
    // cohort. Without this the two assertions above are equally satisfied by a
    // backfill that did nothing at all and a seed that never set `val`.
    const { rows: progress } = await client.query(
      `SELECT rows_done FROM "${meta}".schema_backfills`,
    );
    assert.equal(progress.length, 1, "exactly one backfill was recorded");
    assert.equal(
      Number(progress[0].rows_done),
      SELECTED.length,
      "the recorded progress must be the size of the cohort, not of the table",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

test("a backfill whose predicate matches nothing completes and records nothing", async (ctx) => {
  // The control for `rows_done` above. That assertion says the recorded progress
  // is the cohort size rather than the table size; it would hold just as well for
  // a build that recorded a constant. Here the cohort is empty, so the same field
  // must read zero - and the backfill must still COMPLETE rather than stall
  // waiting for a batch that never arrives.
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("bfnone");
  const meta = `${schema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const seed = {
    name: "seed",
    default: {
      up() {
        table("items").create({
          columns: {
            id: t.int().notNull(),
            grp: t.text().notNull(),
            val: t.int().notNull(),
          },
          primaryKey: ["id"],
        });
        table("items").insert({ rows: ROWS });
      },
    },
  } as MigrationModule & { name: string };

  const fill = {
    name: "fill",
    default: {
      up() {
        table("items").backfill({
          set: { val: (col) => col("val").add(1) },
          where: (col) => col("grp").eq("absent"),
          cursorColumns: ["id"],
          cursorStability: { mode: "externalInvariant", name: "items_id_immutable" },
          batchSize: 3,
          name: "fill_absent",
        });
      },
    },
  } as MigrationModule & { name: string };

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    for (const [migration, priors] of [
      [seed, []],
      [fill, [seed]],
    ] as Array<[MigrationModule & { name: string }, (MigrationModule & { name: string })[]]>) {
      await apply({
        migration,
        priorMigrations: priors,
        priorNameFallbacks: priors.map((prior) => prior.name),
        ownerApp: OWNER_APP,
        projectSchema: schema,
        driver,
        registry: priors.length ? { items: OWNER_APP } : {},
        policy: [charter(schema)],
        approved: true,
        appliedBy: "backfill-selective-cohort",
        nameFallback: migration.name,
      });
    }

    const { rows } = await client.query(
      `SELECT count(*)::int AS n FROM "${schema}".items WHERE val <> 0`,
    );
    assert.equal(rows[0].n, 0, "an empty cohort must transform no row at all");

    const { rows: progress } = await client.query(
      `SELECT rows_done FROM "${meta}".schema_backfills`,
    );
    assert.equal(progress.length, 1, "the backfill is still recorded");
    assert.equal(
      Number(progress[0].rows_done),
      0,
      "and its progress is zero, so the cohort-size assertion above is not a constant",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});
