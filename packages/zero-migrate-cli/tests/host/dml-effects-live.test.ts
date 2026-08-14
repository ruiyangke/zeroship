// What an authored UPDATE and DELETE actually do to the rows.
//
// The fold oracle cannot see any of this. `fold_ops` models schema, and DML
// changes no schema, so every one of this crate's fold roundtrips is silent
// about whether an update touched the right rows or a delete removed the right
// ones. That is by construction rather than an oversight, but it means DML needs
// its own end-to-end check and does not get one for free.
//
// What exists: INSERT is read back on PostgreSQL by `literal-value-binding` and
// `numeric-precision-boundary`, and UPDATE effects are asserted on SQLite by the
// engine's `ir_apply_sqlite.rs`. `apply_dml_validation_pg.rs` is about REFUSALS -
// qualified references, ghost tables - and classifies outcomes rather than rows.
// So on PostgreSQL an authored UPDATE or DELETE had nothing asserting it changed
// what it was supposed to change.
//
// THE `WHERE` IS THE TEST. Asserting "a row changed" would pass for an update
// that rewrote every row in the table, which is the failure that matters: it is
// silent, it is destructive, and a migration is exactly where it would happen.
// So each case leaves BYSTANDER ROWS that must come through untouched, and the
// assertions read the whole table rather than the row under test.
//
// Both dialects run the same authored migration, because a DML lowering that
// went wrong on one engine and not the other is precisely what a portable
// migration tool has to rule out.
//
// GATE: PostgreSQL needs `ZERO_MIGRATE_TEST_PG_URL`; SQLite always runs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const OWNER_APP = "app_dml_effects";

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

/**
 * Four rows in two groups, then an update and a delete that each name ONE group.
 *
 * Rows 1 and 2 are `keep`; rows 3 and 4 are `touch`. The update must move only
 * the `touch` rows to 99, and the delete must remove only row 4. Rows 1 and 2
 * are the bystanders: an update or delete that ignored its predicate would take
 * them with it, and the row-set assertion below is what notices.
 */
const SCHEMA_MIGRATION: MigrationModule = {
  default: {
    schema() {
      table("items").create({
        columns: {
          id: t.int().notNull(),
          grp: t.text(),
          n: t.int(),
        },
        primaryKey: ["id"],
      });
    },
  },
} as MigrationModule;

const DML_MIGRATION: MigrationModule = {
  default: {
    data() {
      table("items").insert({
        rows: [
          { id: 1, grp: "keep", n: 1 },
          { id: 2, grp: "keep", n: 2 },
          { id: 3, grp: "touch", n: 3 },
          { id: 4, grp: "touch", n: 4 },
        ],
      });
      table("items").update({
        set: { n: 99 },
        where: (col) => col("grp").eq("touch"),
      });
      table("items").delete({
        where: (col) => col("id").eq(4),
      });
    },
    inverse() {
      table("items").delete({ where: (col) => col("id").in([1, 2, 3]) });
    },
  },
} as MigrationModule;

/** What the migration above must leave behind, on every dialect. */
const EXPECTED: ReadonlyArray<readonly [number, string, number]> = [
  [1, "keep", 1],
  [2, "keep", 2],
  [3, "touch", 99],
];

test("PostgreSQL: an authored update and delete change only the rows their predicate names", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("dmleff");
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const applyOne = (migration: MigrationModule, nameFallback: string) =>
      apply({
        migration,
        ownerApp: OWNER_APP,
        projectSchema: schema,
        driver,
        registry: { items: OWNER_APP },
        policy: [charter(schema)],
        approved: true,
        appliedBy: "dml-effects-live",
        nameFallback,
      });
    await applyOne(SCHEMA_MIGRATION, "create_items");
    await applyOne(DML_MIGRATION, "dml_effects");

    const { rows } = await client.query(
      `SELECT id, grp, n FROM "${schema}".items ORDER BY id`,
    );
    assert.deepEqual(
      rows.map((row) => [Number(row.id), row.grp, Number(row.n)]),
      EXPECTED.map((row) => [...row]),
      "the whole table, not just the changed row: bystanders prove the predicates were honoured",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

test("SQLite: the same authored migration leaves the same rows", async () => {
  // The cross-dialect arm. A DML lowering that went wrong on one engine and not
  // the other is what a portable migration tool exists to rule out, and running
  // the SAME module is the only way to say the two agree rather than that each
  // does something locally reasonable.
  const work = mkdtempSync(join(HERE, "dmleff-sq-"));
  const dbPath = join(work, "app.db");
  try {
    const applyOne = (migration: MigrationModule, nameFallback: string) =>
      apply({
        migration,
        ownerApp: OWNER_APP,
        projectSchema: "main",
        driver: { kind: "sqlite", appPath: dbPath, journalPath: join(work, "mig.db") },
        registry: { items: OWNER_APP },
        policy: [charter("main")],
        approved: true,
        appliedBy: "dml-effects-live",
        nameFallback,
      });
    await applyOne(SCHEMA_MIGRATION, "create_items");
    await applyOne(DML_MIGRATION, "dml_effects");

    const db = new DatabaseSync(dbPath);
    try {
      const rows = db.prepare("SELECT id, grp, n FROM items ORDER BY id").all() as Array<
        Record<string, unknown>
      >;
      assert.deepEqual(
        rows.map((row) => [Number(row.id), row.grp, Number(row.n)]),
        EXPECTED.map((row) => [...row]),
        "SQLite must land on the same rows as PostgreSQL",
      );
    } finally {
      db.close();
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("PostgreSQL: a predicate matching nothing changes nothing", async (ctx) => {
  // The control for the two above. They assert that rows CHANGED; this asserts
  // the engine is not simply rewriting the table on every DML step regardless of
  // the predicate, which would satisfy them both.
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("dmlnone");
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const applyOne = (migration: MigrationModule, nameFallback: string) =>
      apply({
        migration,
        ownerApp: OWNER_APP,
        projectSchema: schema,
        driver,
        registry: { items: OWNER_APP },
        policy: [charter(schema)],
        approved: true,
        appliedBy: "dml-effects-live",
        nameFallback,
      });
    await applyOne(SCHEMA_MIGRATION, "create_items");
    await applyOne(
      {
        default: {
          data() {
            table("items").insert({
              rows: [
                { id: 1, grp: "keep", n: 1 },
                { id: 2, grp: "keep", n: 2 },
              ],
            });
            table("items").update({
              set: { n: 99 },
              where: (col) => col("grp").eq("absent"),
            });
            table("items").delete({
              where: (col) => col("id").eq(404),
            });
          },
          inverse() {
            table("items").delete({ where: (col) => col("id").in([1, 2]) });
          },
        },
      } as MigrationModule,
      "dml_no_match",
    );

    const { rows } = await client.query(
      `SELECT id, grp, n FROM "${schema}".items ORDER BY id`,
    );
    assert.deepEqual(
      rows.map((row) => [Number(row.id), row.grp, Number(row.n)]),
      [
        [1, "keep", 1],
        [2, "keep", 2],
      ],
      "an update and a delete whose predicates match nothing must leave the table alone",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});
