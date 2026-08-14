// Where an unmatched `onConflict` target is caught, and what it costs.
//
// An `insert({ onConflict })` names the columns whose conflict triggers the
// upsert. If no unique index covers exactly those columns, the statement cannot
// mean anything - and the two dialects discover that at different LAYERS:
//
//   MySQL       refused during lowering, before any database change
//   PostgreSQL  refused by the SERVER while applying the data migration
//
// The layer is the whole finding. Schema and data are separate migrations, so
// the table is already committed on both dialects before the upsert is attempted.
// PostgreSQL commits the seed operation, then reaches the server and refuses the
// upsert, leaving a partially applied data migration. MySQL refuses during
// lowering, before either DML operation reaches the database.
//
// The engine already owns the check - `ensure_exact_unique_conflict_target`
// proves the target against a live unique index, rejecting supersets, prefix
// indexes and functional entries - but only on the MySQL path. PostgreSQL's
// backend takes the target as `_conflict_target` and ignores it, leaving the
// server to object.
//
// WHY THIS IS PINNED AND NOT FIXED. Moving the refusal earlier on PostgreSQL runs
// into the boundary F415 and F429 both hit: validation is offline, and the table
// an upsert targets may not exist in the validation environment, so there is no
// live catalog to prove the index against yet. A preflight inside the DML
// transaction would catch it sooner, but the schema migration is intentionally
// already committed. Closing it properly means deciding where that gate lives,
// which is the same open question those findings left.
//
// The arms below record today's answer on both dialects so the asymmetry is
// visible, and so the day PostgreSQL learns to refuse earlier, the arm that
// changes says exactly what improved.
//
// GATE: PG arm needs `ZERO_MIGRATE_TEST_PG_URL`; MySQL arm needs
// `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_on_conflict_boundary";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

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
`;
}

/** `codes(code PK, label)`. Only `code` is covered by a unique index. */
function schemaMigration(): MigrationModule {
  return {
    default: {
      schema() {
        table("codes").create({
          columns: { code: t.int().notNull(), label: t.string({ length: 32 }).notNull() },
          primaryKey: ["code"],
        });
      },
    },
  } as MigrationModule;
}

/** Seed one row, then upsert against `target`. */
function upsertMigration(target: readonly string[]): MigrationModule {
  return {
    default: {
      data() {
        table("codes").insert({ rows: [{ code: 200, label: "ok" }] });
        table("codes").insert({
          rows: [{ code: 200, label: "dup" }],
          onConflict: { columns: [...target], doUpdate: { label: "updated" } },
        });
      },
      inverse() {
        table("codes").delete({ where: (col) => col("code").eq(200) });
      },
    },
  } as MigrationModule;
}

test("PostgreSQL: a matched target upserts; an unmatched one fails in the data migration", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const run = async (target: readonly string[], schema: string) => {
    const applyOne = (migration: MigrationModule, nameFallback: string) =>
      apply({
        migration,
        ownerApp: OWNER_APP,
        projectSchema: schema,
        driver,
        registry: { codes: OWNER_APP },
        policy: [charter(schema)],
        approved: true,
        appliedBy: "on-conflict-target-boundary",
        nameFallback,
      });
    await applyOne(schemaMigration(), "create_codes");
    await applyOne(upsertMigration(target), "upsert");
  };

  const withSchema = async <T>(run: (schema: string) => Promise<T>): Promise<T> => {
    const schema = uniqueNamespace("on_conflict_pg");
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      return await run(schema);
    } finally {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
           DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
        )
        .catch(() => {});
    }
  };

  try {
    // The control: a target covered by the primary key really upserts.
    await withSchema(async (schema) => {
      await run(["code"], schema);
      const { rows } = await client.query(`SELECT code, label FROM "${schema}".codes`);
      assert.deepEqual(
        rows,
        [{ code: 200, label: "updated" }],
        "a matched conflict target must perform the upsert",
      );
    });

    // The boundary. `label` carries no unique index, so the statement cannot be
    // inferred - and PostgreSQL is what says so, at execution time.
    await withSchema(async (schema) => {
      await assert.rejects(
        run(["label"], schema),
        /no unique or exclusion constraint matching the ON CONFLICT specification/,
        "PostgreSQL refuses an uninferable target - from the server, not the engine",
      );

      // The schema migration committed before the data migration ran. PostgreSQL
      // also committed the first DML operation before the server refused the
      // upsert, so both the table and its seed row remain.
      const { rows } = await client.query(
        `SELECT table_name FROM information_schema.tables WHERE table_schema = $1`,
        [schema],
      );
      assert.deepEqual(
        rows.map((row) => row.table_name),
        ["codes"],
        "the separately applied schema migration must remain after the data migration fails",
      );
      const contents = await client.query(`SELECT code, label FROM "${schema}".codes`);
      assert.deepEqual(
        contents.rows,
        [{ code: 200, label: "ok" }],
        "PostgreSQL reaches the server only after committing the preceding seed operation",
      );
    });
  } finally {
    await client.end().catch(() => {});
  }
});

test("MySQL: the same unmatched target is refused before any data change", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL onConflict boundary skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace("on_conflict_my");

  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    const applyOne = (migration: MigrationModule, nameFallback: string) =>
      apply({
        migration,
        ownerApp: OWNER_APP,
        projectSchema: database,
        driver: { kind: "mysql", url: MYSQL_URL },
        registry: { codes: OWNER_APP },
        policy: [charter(database)],
        approved: true,
        appliedBy: "on-conflict-target-boundary",
        nameFallback,
      });
    await applyOne(schemaMigration(), "create_codes");
    await assert.rejects(
      applyOne(upsertMigration(["label"]), "upsert"),
      /onConflict/,
      "MySQL must refuse the unmatched target",
    );

    // The schema migration is already applied, but lowering must refuse before
    // either insert in the data migration reaches the database.
    const [tables] = await admin.query(
      "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
      [database],
    );
    assert.deepEqual(
      (tables as Array<{ TABLE_NAME: string }>).map((row) => row.TABLE_NAME),
      ["codes"],
      "the separately applied schema migration must remain",
    );
    const [contents] = await admin.query(`SELECT code, label FROM \`${database}\`.codes`);
    assert.deepEqual(contents, [], "lowering refusal must happen before the seed insert");
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${database}_migrations\``,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});
