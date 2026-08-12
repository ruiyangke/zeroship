// A support-matrix "No" for an integrity constraint must mean REFUSED, never
// silently dropped.
//
// `docs/support-matrix.md` is generated from `model/support.rs`, and a committed
// test keeps the markdown in sync with that table. So the markdown cannot drift
// from the table - but NOTHING checks the table against a database. A row could
// read `No` while the engine quietly emitted a `CREATE TABLE` with the constraint
// omitted, and every existing gate would stay green.
//
// For an integrity constraint that outcome is the dangerous one, and it is
// dangerous precisely because it is invisible. A refused migration stops the
// deploy and someone reads the message. A dropped UNIQUE or CHECK produces a table
// that looks right, accepts the duplicate or the negative quantity months later,
// and nothing ever points back at the migration that failed to carry it.
//
// Three rows, all `No`, all integrity constraints:
//
//   Table-level unique constraint | Yes | Yes | No[^5]
//   Table-level check constraint  | Yes | No[^3] | No[^3]
//
// THE POSTGRESQL ARM IS THE CONTROL AND IS NOT OPTIONAL. "Refused on SQLite and
// MySQL" also holds for a build where `uniques:` and `checks:` were broken
// outright, or where the authored shape never reached the engine at all - and this
// file would then be reporting a working dialect gate over a dead feature. The PG
// arm authors the SAME two shapes, applies them, and then proves the constraints
// are LIVE by making the database reject a duplicate and a negative.
//
// GATE: PG arm needs `ZERO_MIGRATE_TEST_PG_URL`, MySQL arm needs
// `ZERO_MIGRATE_MYSQL_URL`. The SQLite arms are an in-process file and always run.

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
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_unsupported_constraints";

type NamedMigration = MigrationModule & { readonly name: string };

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

/** `items(id, sku)` with a table-level UNIQUE on `sku`. */
const uniqueShape = (): void => {
  table("items").create({
    columns: { id: t.int().notNull(), sku: t.string({ length: 32 }).notNull() },
    primaryKey: ["id"],
    uniques: [{ name: "items_sku_key", columns: ["sku"] }],
  });
};

/** `parts(id, qty)` with a table-level CHECK that `qty >= 0`. */
const checkShape = (): void => {
  table("parts").create({
    columns: { id: t.int().notNull(), qty: t.int().notNull() },
    primaryKey: ["id"],
    checks: [{ name: "parts_qty_check", expr: (col) => col("qty").ge(0) }],
  });
};

function applyShape(
  name: string,
  up: () => void,
  projectSchema: string,
  driver: DriverConfig,
): Promise<unknown> {
  return apply({
    migration: { name, default: { up } } as NamedMigration,
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: {},
    policy: [charter(projectSchema)],
    approved: true,
    appliedBy: "unsupported-constraints-refuse",
    nameFallback: name,
  });
}

function withSqliteFile(prefix: string, body: (driver: DriverConfig, dbPath: string) => Promise<void>) {
  const work = mkdtempSync(join(HERE, `${prefix}-`));
  const dbPath = join(work, "app.db");
  return body(
    { kind: "sqlite", appPath: dbPath, journalPath: join(work, "mig.db") },
    dbPath,
  ).finally(() => rmSync(work, { recursive: true, force: true }));
}

/** The names SQLite holds in `sqlite_master`, or `[]` when the file has no tables. */
function sqliteTables(dbPath: string): string[] {
  let db: DatabaseSync;
  try {
    db = new DatabaseSync(dbPath);
  } catch {
    return [];
  }
  try {
    return db
      .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
      .all()
      .map((row: Record<string, unknown>) => row.name as string);
  } finally {
    db.close();
  }
}

test("SQLite refuses a table-level UNIQUE rather than emitting a table without it", async () => {
  await withSqliteFile("unsup-uniq", async (driver, dbPath) => {
    await assert.rejects(
      applyShape("sqlite_table_unique", uniqueShape, "main", driver),
      /unsupported shape|UNSUPPORTED/,
      "the matrix says No, so the deploy must stop rather than drop the constraint",
    );
    // The half that matters. A refusal that had already emitted the table would
    // leave a live `items` with no uniqueness on `sku`, which is the silent
    // outcome this file exists to rule out.
    assert.ok(
      !sqliteTables(dbPath).includes("items"),
      "the refused migration must not have created the table at all",
    );
  });
});

test("SQLite refuses a table-level CHECK rather than emitting a table without it", async () => {
  await withSqliteFile("unsup-chk", async (driver, dbPath) => {
    await assert.rejects(
      applyShape("sqlite_table_check", checkShape, "main", driver),
      /does not render expressions for this op|UNSUPPORTED/,
      "the matrix says No, so the deploy must stop rather than drop the constraint",
    );
    assert.ok(
      !sqliteTables(dbPath).includes("parts"),
      "the refused migration must not have created the table at all",
    );
  });
});

test("MySQL refuses a table-level CHECK rather than emitting a table without it", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL unsupported-constraint arm skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace("unsup_chk_my");
  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    await assert.rejects(
      applyShape("mysql_table_check", checkShape, database, { kind: "mysql", url: MYSQL_URL }),
      /table-level CHECK expression rendering is PostgreSQL-only/,
      "MySQL must refuse and name the reason",
    );
    const [tables] = await admin.query(
      "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
      [database],
    );
    assert.deepEqual(
      (tables as Array<{ TABLE_NAME: string }>).map((row) => row.TABLE_NAME),
      [],
      "the refused migration must not have created the table at all",
    );
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${database}_migrations\``,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("PostgreSQL control: both shapes apply and the constraints are really enforced", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("unsup_ctl");
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    await applyShape("pg_table_unique", uniqueShape, schema, driver);
    await applyShape("pg_table_check", checkShape, schema, driver);

    // Not "the migration succeeded" - that would still hold if the constraint had
    // been dropped on the way. The DATABASE has to reject the violations.
    await client.query(`INSERT INTO "${schema}".items (id, sku) VALUES (1, 'A')`);
    await assert.rejects(
      client.query(`INSERT INTO "${schema}".items (id, sku) VALUES (2, 'A')`),
      /duplicate key value violates unique constraint/,
      "the authored UNIQUE must be live on PostgreSQL",
    );
    await assert.rejects(
      client.query(`INSERT INTO "${schema}".parts (id, qty) VALUES (1, -5)`),
      /violates check constraint/,
      "the authored CHECK must be live on PostgreSQL",
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
