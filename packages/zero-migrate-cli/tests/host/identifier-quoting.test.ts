// Authored identifiers are quoted, not interpolated - on every dialect.
//
// A column name comes from a migration file, and a migration file is code the
// team writes. So this is not an untrusted-input boundary in the usual sense. It
// is worth testing anyway, for two reasons: names arrive from generators and from
// existing databases during adoption, and a quoting slip does not fail loudly -
// it either mangles the name or executes the rest of the string.
//
// Each payload carries a statement terminator and a comment opener, so an
// interpolating implementation would run `DROP TABLE bystander` and swallow the
// remainder of the statement. The BYSTANDER TABLE IS THE ASSERTION: it exists
// before the migration runs, and it has to still be there afterwards. Checking
// only that the column was created would pass even if the payload had also
// executed.
//
// Both escape characters are exercised on all three dialects rather than each on
// its own: `"` is the delimiter PostgreSQL and SQLite escape by doubling, and
// backtick is MySQL's. Sending both everywhere means a dialect that reached for
// the wrong escape - or a shared code path that hardcoded one - shows up on the
// dialect it does not belong to.
//
// GATE: PG needs `ZERO_MIGRATE_TEST_PG_URL`, MySQL needs `ZERO_MIGRATE_MYSQL_URL`,
// SQLite always runs.

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
const OWNER_APP = "app_identifier_quoting";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

/** Payloads that would execute if an identifier were interpolated. */
const PAYLOADS: ReadonlyArray<readonly [string, string]> = [
  ["double-quote delimiter", 'a"; DROP TABLE bystander; --'],
  ["backtick delimiter", "a`; DROP TABLE bystander; --"],
];

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

function migrationNaming(column: string): MigrationModule {
  return {
    default: {
      up() {
        table("items").create({
          columns: { id: t.int().notNull(), [column]: t.int() },
          primaryKey: ["id"],
        });
      },
    },
  } as MigrationModule;
}

test("PostgreSQL quotes an authored identifier rather than interpolating it", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  try {
    for (const [label, payload] of PAYLOADS) {
      const schema = uniqueNamespace("ident_pg");
      try {
        await client.query(`CREATE SCHEMA "${schema}"`);
        await client.query(`CREATE TABLE "${schema}".bystander (id int)`);

        await apply({
          migration: migrationNaming(payload),
          ownerApp: OWNER_APP,
          projectSchema: schema,
          driver,
          registry: {},
          policy: [charter(schema)],
          approved: true,
          appliedBy: "identifier-quoting",
          nameFallback: "name_it",
        });

        const { rows: columns } = await client.query(
          `SELECT column_name FROM information_schema.columns
            WHERE table_schema = $1 AND table_name = 'items'`,
          [schema],
        );
        assert.ok(
          columns.some((row) => row.column_name === payload),
          `${label}: the column must carry the payload as its literal name`,
        );

        const { rows: tables } = await client.query(
          `SELECT table_name FROM information_schema.tables WHERE table_schema = $1`,
          [schema],
        );
        assert.ok(
          tables.some((row) => row.table_name === "bystander"),
          `${label}: the bystander table must survive - if it did not, the payload ran`,
        );
      } finally {
        await client
          .query(
            `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
             DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
          )
          .catch(() => {});
      }
    }
  } finally {
    await client.end().catch(() => {});
  }
});

test("MySQL quotes an authored identifier rather than interpolating it", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL identifier quoting skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;

  for (const [label, payload] of PAYLOADS) {
    const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
    const database = uniqueNamespace("ident_my");
    try {
      await admin.query(`CREATE DATABASE \`${database}\``);
      await admin.query(`CREATE TABLE \`${database}\`.bystander (id int) ENGINE=InnoDB`);

      await apply({
        migration: migrationNaming(payload),
        ownerApp: OWNER_APP,
        projectSchema: database,
        driver: { kind: "mysql", url: MYSQL_URL },
        registry: {},
        policy: [charter(database)],
        approved: true,
        appliedBy: "identifier-quoting",
        nameFallback: "name_it",
      });

      const [columns] = await admin.query(
        `SELECT COLUMN_NAME AS c FROM information_schema.COLUMNS
          WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'items'`,
        [database],
      );
      assert.ok(
        (columns as Array<{ c: string }>).some((row) => row.c === payload),
        `${label}: the column must carry the payload as its literal name`,
      );

      const [tables] = await admin.query(
        `SELECT TABLE_NAME AS t FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?`,
        [database],
      );
      assert.ok(
        (tables as Array<{ t: string }>).some((row) => row.t === "bystander"),
        `${label}: the bystander table must survive - if it did not, the payload ran`,
      );
    } finally {
      await admin
        .query(
          `DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${database}_migrations\``,
        )
        .catch(() => {});
      await admin.end().catch(() => {});
    }
  }
});

test("SQLite quotes an authored identifier rather than interpolating it", async () => {
  for (const [label, payload] of PAYLOADS) {
    const work = mkdtempSync(join(HERE, "ident-sq-"));
    const dbPath = join(work, "app.db");
    try {
      const seed = new DatabaseSync(dbPath);
      seed.exec("CREATE TABLE bystander (id INTEGER)");
      seed.close();

      await apply({
        migration: migrationNaming(payload),
        ownerApp: OWNER_APP,
        projectSchema: "main",
        driver: { kind: "sqlite", appPath: dbPath, journalPath: join(work, "mig.db") },
        registry: {},
        policy: [charter("main")],
        approved: true,
        appliedBy: "identifier-quoting",
        nameFallback: "name_it",
      });

      const db = new DatabaseSync(dbPath);
      try {
        const columns = db
          .prepare("SELECT name FROM pragma_table_info('items')")
          .all()
          .map((row: Record<string, unknown>) => row.name as string);
        assert.ok(
          columns.includes(payload),
          `${label}: the column must carry the payload as its literal name`,
        );

        const tables = db
          .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
          .all()
          .map((row: Record<string, unknown>) => row.name as string);
        assert.ok(
          tables.includes("bystander"),
          `${label}: the bystander table must survive - if it did not, the payload ran`,
        );
      } finally {
        db.close();
      }
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});
