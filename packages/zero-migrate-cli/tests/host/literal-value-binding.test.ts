// Authored literal values travel as binds, and NUL is where the dialects part.
//
// The identifier half of this question is `identifier-quoting.test.ts`. This is
// the value half, and it is a different code path: identifiers are quoted into
// the statement text, values are bound as parameters. A slip here corrupts DATA
// rather than mangling a name.
//
// The fragment payload is the same shape as the identifier one - a quote, a
// statement terminator, a comment opener - and the BYSTANDER TABLE is again the
// assertion rather than the round-trip. A value that came back intact would still
// come back intact if the statement had ALSO executed the payload.
//
// NUL IS A REAL PORTABILITY DIFFERENCE, not a bug in any of the three:
//
//   PostgreSQL   REFUSED - `text` cannot hold a NUL byte, and the engine surfaces
//                the error rather than storing a truncated value
//   MySQL        stored exactly
//   SQLite       stored exactly
//
// So the same migration succeeds on MySQL and SQLite and fails on PostgreSQL.
// That is worth pinning because of how it is usually met: a team develops against
// SQLite, the value arrives from imported data, and the deploy is the first thing
// that sees PostgreSQL. Each dialect is behaving correctly; the portability gap is
// the finding.
//
// The PostgreSQL arm asserts REFUSAL, deliberately. If PostgreSQL ever started
// accepting a NUL it would be storing something other than what was authored, and
// this arm is what would notice.
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
const OWNER_APP = "app_literal_binding";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

/** Built from char codes so the source file carries no control characters. */
const NUL = String.fromCharCode(0);
const NEWLINE = String.fromCharCode(10);
const TAB = String.fromCharCode(9);

const FRAGMENT = "'); DROP TABLE bystander; --";
const NUL_VALUE = `before${NUL}after`;

/** Values every dialect must store byte-for-byte. */
const PORTABLE: ReadonlyArray<readonly [string, string]> = [
  ["single quote", "O'Brien"],
  ["sql fragment", FRAGMENT],
  ["backslashes", "a\\b\\'c"],
  ["newline and tab", `line1${NEWLINE}line2${TAB}end`],
  ["long", "x".repeat(500)],
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

function insertingMigration(value: string): MigrationModule {
  return {
    default: {
      up() {
        table("items").create({
          columns: { id: t.int().notNull(), body: t.text() },
          primaryKey: ["id"],
        });
        table("items").insert({ rows: { id: 1, body: value } });
      },
    },
  } as MigrationModule;
}

test("PostgreSQL binds literal values, and refuses a NUL byte rather than truncating", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const withSchema = async <T>(run: (schema: string) => Promise<T>): Promise<T> => {
    const schema = uniqueNamespace("lit_pg");
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      await client.query(`CREATE TABLE "${schema}".bystander (id int)`);
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

  const applyValue = (value: string, schema: string) =>
    apply({
      migration: insertingMigration(value),
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver,
      registry: {},
      policy: [charter(schema)],
      approved: true,
      appliedBy: "literal-value-binding",
      nameFallback: "insert_it",
    });

  try {
    for (const [label, value] of PORTABLE) {
      await withSchema(async (schema) => {
        await applyValue(value, schema);
        const { rows } = await client.query(`SELECT body FROM "${schema}".items WHERE id = 1`);
        assert.equal(rows[0]?.body, value, `${label}: the value must round-trip byte for byte`);

        const { rows: tables } = await client.query(
          `SELECT table_name FROM information_schema.tables WHERE table_schema = $1`,
          [schema],
        );
        assert.ok(
          tables.some((row) => row.table_name === "bystander"),
          `${label}: the bystander must survive - if it did not, the value was interpolated`,
        );
      });
    }

    // PostgreSQL text cannot hold a NUL. Refusing is the correct answer, and the
    // one that must not quietly become "stored, truncated at the NUL".
    await withSchema(async (schema) => {
      await assert.rejects(
        applyValue(NUL_VALUE, schema),
        /invalid byte sequence|0x00|NUL/i,
        "a NUL byte must be refused rather than silently truncated",
      );
    });
  } finally {
    await client.end().catch(() => {});
  }
});

test("MySQL binds literal values and stores a NUL byte exactly", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL literal binding skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;

  for (const [label, value] of [...PORTABLE, ["NUL byte", NUL_VALUE] as const]) {
    const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
    const database = uniqueNamespace("lit_my");
    try {
      await admin.query(`CREATE DATABASE \`${database}\``);
      await admin.query(`CREATE TABLE \`${database}\`.bystander (id int) ENGINE=InnoDB`);
      await apply({
        migration: insertingMigration(value),
        ownerApp: OWNER_APP,
        projectSchema: database,
        driver: { kind: "mysql", url: MYSQL_URL },
        registry: {},
        policy: [charter(database)],
        approved: true,
        appliedBy: "literal-value-binding",
        nameFallback: "insert_it",
      });

      const [rows] = await admin.query(`SELECT body FROM \`${database}\`.items WHERE id = 1`);
      assert.equal(
        (rows as Array<{ body: string }>)[0]?.body,
        value,
        `${label}: the value must round-trip byte for byte`,
      );

      const [tables] = await admin.query(
        `SELECT TABLE_NAME AS t FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?`,
        [database],
      );
      assert.ok(
        (tables as Array<{ t: string }>).some((row) => row.t === "bystander"),
        `${label}: the bystander must survive`,
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

// IF THIS TEST FAILS WITH `+ 'before'  - 'before\x00after'`, IT IS NOT FLAKY AND
// IT IS NOT THIS FILE. Check `node --version` first.
//
//   Node v22.23.1   node:sqlite stores "before\0after" as "before"   TRUNCATED
//   Node v24.18.1   stores it exactly                                 EXACT
//
// Reproduced with raw `node:sqlite` DatabaseSync on an in-memory database - no
// engine, no addon, no driver - so it is a Node behaviour difference, not a
// zero-migrate defect. The assertion below is correct and is catching real data
// truncation on the platform `flake.nix` pins (`pkgs.nodejs_22`) and
// CONTRIBUTING documents.
//
// Consequence: run this suite OUTSIDE the devShell on a newer Node and it passes,
// which is how it stayed hidden. A green host suite says nothing about NUL-byte
// handling unless you know which Node produced it.
//
// Do not weaken this assertion to make the suite green. Silencing it converts a
// loud correct signal into the silent truncation it exists to detect. The open
// decision - raise the Node pin, bind NUL-bearing strings as BLOB, refuse them
// fail-closed, or document the limitation - is in `docs/review-log` F558.
test("SQLite binds literal values and stores a NUL byte exactly", async () => {
  for (const [label, value] of [...PORTABLE, ["NUL byte", NUL_VALUE] as const]) {
    const work = mkdtempSync(join(HERE, "lit-sq-"));
    const dbPath = join(work, "app.db");
    try {
      const seed = new DatabaseSync(dbPath);
      seed.exec("CREATE TABLE bystander (id INTEGER)");
      seed.close();

      await apply({
        migration: insertingMigration(value),
        ownerApp: OWNER_APP,
        projectSchema: "main",
        driver: { kind: "sqlite", appPath: dbPath, journalPath: join(work, "mig.db") },
        registry: {},
        policy: [charter("main")],
        approved: true,
        appliedBy: "literal-value-binding",
        nameFallback: "insert_it",
      });

      const db = new DatabaseSync(dbPath);
      try {
        const row = db.prepare("SELECT body FROM items WHERE id = 1").get() as
          | Record<string, unknown>
          | undefined;
        assert.equal(row?.body, value, `${label}: the value must round-trip byte for byte`);

        const tables = db
          .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
          .all()
          .map((entry: Record<string, unknown>) => entry.name as string);
        assert.ok(tables.includes("bystander"), `${label}: the bystander must survive`);
      } finally {
        db.close();
      }
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});
