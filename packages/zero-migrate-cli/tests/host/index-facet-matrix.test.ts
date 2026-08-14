// Every index-facet row of the support matrix, checked against a real database.
//
// `docs/support-matrix.md` publishes nine index facets across three dialects. The
// markdown is generated from `model/support.rs` and a committed test keeps the two
// in sync - but nothing checks that table against a database, so a `No` that was
// really a silent drop, or a `Yes` that never reached the emitter, would leave
// every existing gate green. `unsupported-constraints-refuse.test.ts` closed that
// hole for the integrity-constraint rows; this file closes it for the index rows.
//
// The matrix rows under test:
//
//   Expression index              | Yes | No[^7]  | Yes
//   Partial index                 | Yes | No[^8]  | Yes
//   Included index columns        | Yes | No[^9]  | No[^9]
//   Index storage parameters      | Yes | No[^10] | No[^10]
//   Index on `ONLY`               | Yes | No[^11] | No[^11]
//   Unique index NULLS NOT DISTINCT | Yes | No[^12] | No[^12]
//   Index operator class          | Yes | No[^13] | No[^13]
//   Index collation               | Yes | No[^14] | No[^14]
//   Non-btree index method        | Yes | No[^15] | No[^15]
//
// THE TWO SQLITE `Yes` ROWS ARE THE CONTROL, and they are why this file is
// data-driven rather than a list of refusals. Sixteen refusals prove nothing on
// their own: they hold just as well for a build where `table().index().add()` was
// broken, or where none of these options reached the engine at all. The `Yes` rows
// apply and then assert the facet is VISIBLE IN THE EMITTED SQL - the expression in
// the key, the predicate in a `WHERE` - which is what says the options are wired
// and the refusals are a dialect decision rather than a dead feature.
//
// GATE: MySQL arm needs `ZERO_MIGRATE_MYSQL_URL`; the SQLite arms are an
// in-process file and always run.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_index_facet_matrix";

type IndexOptions = Parameters<ReturnType<ReturnType<typeof table>["index"]>["add"]>[0];

/** One facet, and the index options that carry it. */
const FACETS: ReadonlyArray<readonly [string, IndexOptions]> = [
  ["expression", { on: [{ expr: (col) => col("rank").add(1) }] }],
  ["partial", { on: [{ column: "email" }], where: (col) => col("rank").gt(0) }],
  ["include", { on: [{ column: "email" }], include: ["rank"] }],
  ["storage_params", { on: [{ column: "email" }], with: { fillfactor: 90 } }],
  ["only", { on: [{ column: "email" }], only: true }],
  ["nulls_not_distinct", { on: [{ column: "email" }], unique: true, nullsNotDistinct: true }],
  ["opclass", { on: [{ column: "email", opclass: "text_pattern_ops" }] }],
  ["collation", { on: [{ column: "email", collation: "C" }] }],
  ["non_btree", { on: [{ column: "email" }], using: "gin" }],
] as const;

/** Declared `No` on SQLite; `expression` and `partial` are declared `Yes`. */
const SQLITE_UNSUPPORTED = FACETS.filter(
  ([name]) => name !== "expression" && name !== "partial",
);

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

/** One migration creating `accounts` and one index over it carrying `options`.
 *  Table and index land together, so a refusal leaves nothing behind at all. */
function facetMigration(facet: string, options: IndexOptions): MigrationModule {
  return {
    default: {
      schema() {
        table("accounts").create({
          columns: {
            id: t.int().notNull(),
            email: t.string({ length: 64 }).notNull(),
            rank: t.int().notNull(),
          },
          primaryKey: ["id"],
        });
        table("accounts").index(`accounts_${facet}_idx`).add(options);
      },
    },
  } as MigrationModule;
}

function applyFacet(
  facet: string,
  options: IndexOptions,
  projectSchema: string,
  driver: DriverConfig,
): Promise<unknown> {
  return apply({
    migration: facetMigration(facet, options),
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: {},
    policy: [charter(projectSchema)],
    approved: true,
    appliedBy: "index-facet-matrix",
    nameFallback: `facet_${facet}`,
  });
}

function withSqliteFile<T>(prefix: string, body: (driver: DriverConfig, dbPath: string) => Promise<T>) {
  const work = mkdtempSync(join(HERE, `${prefix}-`));
  const dbPath = join(work, "app.db");
  return body(
    { kind: "sqlite", appPath: dbPath, journalPath: join(work, "mig.db") },
    dbPath,
  ).finally(() => rmSync(work, { recursive: true, force: true }));
}

/** Every object name SQLite holds, so an arm can assert a refusal left nothing. */
function sqliteObjects(dbPath: string): string[] {
  let db: DatabaseSync;
  try {
    db = new DatabaseSync(dbPath);
  } catch {
    return [];
  }
  try {
    return db
      .prepare("SELECT name FROM sqlite_master")
      .all()
      .map((row: Record<string, unknown>) => row.name as string);
  } finally {
    db.close();
  }
}

test("SQLite refuses every index facet the matrix declares unsupported, leaving nothing behind", async () => {
  for (const [facet, options] of SQLITE_UNSUPPORTED) {
    await withSqliteFile(`facet-sq-${facet}`, async (driver, dbPath) => {
      await assert.rejects(
        applyFacet(facet, options, "main", driver),
        /unsupported shape|UNSUPPORTED/,
        `SQLite must refuse the ${facet} facet the matrix declares No`,
      );
      // The table and the index are authored in ONE migration, so a refusal that
      // had already emitted the table would show up here. That is the silent
      // outcome - an index missing its facet, or a table with no index at all.
      assert.deepEqual(
        sqliteObjects(dbPath).filter((name) => name.startsWith("accounts")),
        [],
        `the refused ${facet} migration must leave no accounts object behind`,
      );
    });
  }
});

test("SQLite control: the two facets the matrix declares supported apply and reach the SQL", async () => {
  // Without this, the sixteen refusals above and below also hold for a build
  // where none of these index options reached the engine at all.
  const emitted = async (facet: string, options: IndexOptions): Promise<string> =>
    withSqliteFile(`facet-sq-ok-${facet}`, async (driver, dbPath) => {
      await applyFacet(facet, options, "main", driver);
      const db = new DatabaseSync(dbPath);
      try {
        const row = db
          .prepare("SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?")
          .get(`accounts_${facet}_idx`) as Record<string, unknown> | undefined;
        return String(row?.sql ?? "");
      } finally {
        db.close();
      }
    });

  const expression = await emitted("expression", FACETS[0][1]);
  assert.match(
    expression,
    /\(\s*"?rank"?\s*\+\s*1\s*\)/,
    `the expression key must reach the emitted SQL, got ${expression}`,
  );

  const partial = await emitted("partial", FACETS[1][1]);
  assert.match(
    partial,
    /WHERE\s*\(\s*"?rank"?\s*>\s*0\s*\)/i,
    `the partial predicate must reach the emitted SQL, got ${partial}`,
  );
});

test("MySQL refuses every index facet the matrix declares unsupported, leaving nothing behind", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL index-facet matrix skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace("facet_my");
  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    // MySQL declares ALL NINE unsupported, including expression and partial, which
    // SQLite supports. Every refusal leaves the database empty, so they share one.
    for (const [facet, options] of FACETS) {
      await assert.rejects(
        applyFacet(facet, options, database, { kind: "mysql", url: MYSQL_URL }),
        /unsupported shape|UNSUPPORTED/,
        `MySQL must refuse the ${facet} facet the matrix declares No`,
      );
    }
    const [tables] = await admin.query(
      "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
      [database],
    );
    assert.deepEqual(
      (tables as Array<{ TABLE_NAME: string }>).map((row) => row.TABLE_NAME),
      [],
      "no refused migration may leave a table behind",
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
