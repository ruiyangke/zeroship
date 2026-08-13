// The support matrix's exclusion-constraint row, checked against real databases.
//
//   Exclusion constraint | Yes | No[^6] | No[^6]
//
// `unsupported-constraints-refuse.test.ts` audits the UNIQUE and CHECK rows;
// this row was left out of it and had no host coverage anywhere. The two
// existing host files mentioning "exclusion" are about something else - one
// quotes a PostgreSQL `ON CONFLICT` error, the other is about lock exclusion.
//
// For an integrity constraint the silent outcome is the dangerous one, which is
// the whole argument of the constraints file: a refused migration stops the
// deploy and someone reads the message, while a dropped constraint produces a
// table that looks right and accepts the overlapping row months later.
//
// So the PostgreSQL arm reads `pg_constraint` and requires `contype = 'x'`. A
// build that quietly rendered the exclusion as an ordinary unique index would
// satisfy "the migration applied" and even "a constraint named
// bookings_no_overlap exists" - and would enforce something strictly weaker.
//
// A NOTE FOR WHOEVER EXTENDS THIS. The natural way to write an exclusion uses
// `using: "gist"`, and that fails on PostgreSQL itself: `text` has no default
// operator class for gist without the `btree_gist` extension. The refusal is
// PostgreSQL's, not the engine's, and reading it as a matrix result would put a
// `No` against the column that declares `Yes`. `using: "btree"` needs no
// extension, so the control here measures the engine rather than the server's
// contrib packaging.
//
// GATE: PostgreSQL needs `ZERO_MIGRATE_TEST_PG_URL`, MySQL needs
// `ZERO_MIGRATE_MYSQL_URL`, SQLite always runs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
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
const OWNER_APP = "app_exclusion_dialects";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
/** Verbatim footnote [^6] of the generated matrix. */
const REFUSAL = /exclusion constraints are PostgreSQL-only/;

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

/**
 * The exclusion is the only unusual thing here, so no earlier gate can answer
 * for it - the isolation discipline `sequence-default-dialects.test.ts` records.
 */
const MIGRATION: MigrationModule = {
  default: {
    up() {
      table("bookings").create({
        columns: { id: t.int().notNull(), room: t.text(), during: t.text() },
        primaryKey: ["id"],
        exclusions: [
          {
            name: "bookings_no_overlap",
            using: "btree",
            elements: [{ target: "room", operator: "=" }],
          },
        ],
      });
    },
  },
} as MigrationModule;

function deploy(projectSchema: string, driver: DriverConfig): Promise<unknown> {
  return apply({
    migration: MIGRATION,
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: {},
    policy: [charter(projectSchema)],
    approved: true,
    appliedBy: "exclusion-constraint-dialects",
    nameFallback: "exclusion",
  });
}

test("PostgreSQL creates a real EXCLUDE constraint, not a weaker stand-in", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const schema = uniqueNamespace("excdial");
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    await deploy(schema, { kind: "postgres", url: pgUrl() });

    // contype = 'x' is the load-bearing part. Asserting only that a constraint
    // named `bookings_no_overlap` exists would pass for a unique index, which
    // enforces something strictly weaker and would never be noticed.
    const { rows } = await client.query(
      `SELECT conname, contype, pg_get_constraintdef(oid) AS def
         FROM pg_constraint
        WHERE connamespace = $1::regnamespace AND contype = 'x'`,
      [schema],
    );
    assert.equal(rows.length, 1, `exactly one exclusion constraint; got ${rows.length}`);
    assert.equal(rows[0].conname, "bookings_no_overlap", "and it carries the authored name");
    assert.match(
      String(rows[0].def),
      /^EXCLUDE USING btree \(room WITH =\)$/,
      `the definition must be the authored exclusion; got ${rows[0].def}`,
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

test("SQLite refuses an exclusion constraint by name", async () => {
  const work = mkdtempSync(join(HERE, "excdial-sq-"));
  try {
    await assert.rejects(
      deploy("main", {
        kind: "sqlite",
        appPath: join(work, "app.db"),
        journalPath: join(work, "mig.db"),
      }),
      REFUSAL,
      "SQLite must refuse rather than drop the constraint",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("MySQL refuses an exclusion constraint by name and leaves no table", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL exclusion constraint skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const database = uniqueNamespace("excdialmy");
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    await assert.rejects(
      deploy(database, { kind: "mysql", url: MYSQL_URL }),
      REFUSAL,
      "MySQL must refuse rather than drop the constraint",
    );

    // Refused, not half-applied: a table created without its exclusion is the
    // outcome this row exists to prevent.
    const [tables] = await admin.query(
      `SELECT TABLE_NAME AS t FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?`,
      [database],
    );
    assert.deepEqual(
      (tables as Array<{ t: string }>).map((row) => row.t),
      [],
      "a refused migration must not leave the table behind without its constraint",
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
