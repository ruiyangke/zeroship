// Authored identifiers must satisfy the portable identifier contract before any
// dialect sees SQL.
//
// Each payload carries a statement terminator and comment opener. Every backend
// must reject it during guarded lowering, leave the pre-existing bystander table
// intact, and create no authored table.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { table, t } from "@zeroship/migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "@zeroship/migrate/internal/recorder";

import { connectLivePg, mysqlUrl, pgUrl } from "./live-db.js";

// The host suite builds and resolves its addon in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const OWNER_APP = "app_identifier_validation";
const MYSQL_URL = mysqlUrl();

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
      schema() {
        table("items").create({
          columns: { id: t.int().required(), [column]: t.int() },
          primaryKey: ["id"],
        });
      },
    },
  } as MigrationModule;
}

test("PostgreSQL rejects a non-portable authored identifier before mutation", async () => {
  const client = await connectLivePg();
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  try {
    for (const [label, payload] of PAYLOADS) {
      const schema = uniqueNamespace("ident_pg");
      try {
        await client.query(`CREATE SCHEMA "${schema}"`);
        await client.query(`CREATE TABLE "${schema}".bystander (id int)`);

        await assert.rejects(
          () =>
            apply({
              migration: migrationNaming(payload),
              ownerApp: OWNER_APP,
              projectSchema: schema,
              driver,
              registry: {},
              policy: [charter(schema)],
              approved: true,
              appliedBy: "identifier-validation",
              nameFallback: "name_it",
            }),
          /invalid identifier:.*ASCII alphanumeric \+ underscore/i,
          `${label}: guarded lowering must reject the identifier`,
        );

        const { rows: tables } = await client.query(
          `SELECT table_name FROM information_schema.tables WHERE table_schema = $1`,
          [schema],
        );
        assert.ok(
          tables.some((row) => row.table_name === "bystander"),
          `${label}: the bystander table must survive`,
        );
        assert.ok(
          !tables.some((row) => row.table_name === "items"),
          `${label}: no table landed`,
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

test("MySQL rejects a non-portable authored identifier before mutation", async () => {
  const mysql = (await import("mysql2/promise")).default;

  for (const [label, payload] of PAYLOADS) {
    const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
    const database = uniqueNamespace("ident_my");
    try {
      await admin.query(`CREATE DATABASE \`${database}\``);
      await admin.query(`CREATE TABLE \`${database}\`.bystander (id int) ENGINE=InnoDB`);

      await assert.rejects(
        () =>
          apply({
            migration: migrationNaming(payload),
            ownerApp: OWNER_APP,
            projectSchema: database,
            driver: { kind: "mysql", url: MYSQL_URL },
            registry: {},
            policy: [charter(database)],
            approved: true,
            appliedBy: "identifier-validation",
            nameFallback: "name_it",
          }),
        /invalid identifier:.*ASCII alphanumeric \+ underscore/i,
        `${label}: guarded lowering must reject the identifier`,
      );

      const [tables] = await admin.query(
        `SELECT TABLE_NAME AS t FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?`,
        [database],
      );
      assert.ok(
        (tables as Array<{ t: string }>).some((row) => row.t === "bystander"),
        `${label}: the bystander table must survive`,
      );
      assert.ok(
        !(tables as Array<{ t: string }>).some((row) => row.t === "items"),
        `${label}: no table landed`,
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

test("SQLite rejects a non-portable authored identifier before mutation", async () => {
  for (const [label, payload] of PAYLOADS) {
    const work = mkdtempSync(join(HERE, "ident-sq-"));
    const dbPath = join(work, "app.db");
    try {
      const seed = new DatabaseSync(dbPath);
      seed.exec("CREATE TABLE bystander (id INTEGER)");
      seed.close();

      await assert.rejects(
        () =>
          apply({
            migration: migrationNaming(payload),
            ownerApp: OWNER_APP,
            projectSchema: "main",
            driver: { kind: "sqlite", appPath: dbPath, journalPath: join(work, "mig.db") },
            registry: {},
            policy: [charter("main")],
            approved: true,
            appliedBy: "identifier-validation",
            nameFallback: "name_it",
          }),
        /invalid identifier:.*ASCII alphanumeric \+ underscore/i,
        `${label}: guarded lowering must reject the identifier`,
      );

      const db = new DatabaseSync(dbPath);
      try {
        const tables = db
          .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
          .all()
          .map((row: Record<string, unknown>) => row.name as string);
        assert.ok(
          tables.includes("bystander"),
          `${label}: the bystander table must survive`,
        );
        assert.ok(!tables.includes("items"), `${label}: no table landed`);
      } finally {
        db.close();
      }
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});
