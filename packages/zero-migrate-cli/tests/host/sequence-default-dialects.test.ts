// The support matrix's "Sequence-backed default" row, checked against real
// databases - and kept distinct from the "Standalone sequence" row beside it.
//
//   Sequence-backed default | Yes | No[^2] | No[^2]
//   Standalone sequence     | Yes | No[^22] | No[^22]
//
// `pg-only-op-matrix.test.ts` covers the second. The first was uncovered at the
// host level: `nextval()` is exported from the DSL and appears in engine tests,
// but no host test authored a column whose default draws from a sequence.
//
// THE TWO ROWS ARE EASY TO CONFLATE, and conflating them is how this file nearly
// went wrong. A migration that creates a sequence AND uses it as a default is
// refused on MySQL/SQLite at op 0 - the sequence creation - and never reaches the
// default at all. Read quickly that looks like the default row being enforced. It
// is the other row, one op earlier. Same shape as the charter-gate trap
// documented in `pg-only-op-matrix.test.ts`.
//
// So the refusal arms author the default WITHOUT a sequence-create op, which is
// the only way the default is what gets judged. The last test then requires the
// two refusals to carry DIFFERENT text, so the rows cannot silently collapse into
// one.
//
// For a default the silent outcome is the dangerous one: a dropped `nextval`
// leaves a column that looks right and never populates, and the failure surfaces
// at the first insert rather than at the migration.
//
// GATE: PostgreSQL needs `ZERO_MIGRATE_TEST_PG_URL`, MySQL needs
// `ZERO_MIGRATE_MYSQL_URL`, SQLite always runs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { table, t, sequence, nextval } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const OWNER_APP = "app_sequence_default";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
/** Verbatim footnote [^2] of the generated matrix. */
const DEFAULT_REFUSAL = /nextval sequence defaults are PostgreSQL-only/;
const STANDALONE_REFUSAL = /standalone sequence objects are PostgreSQL-only/;

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

/** Sequence plus a column defaulting from it - the whole feature. */
const WITH_SEQUENCE: MigrationModule = {
  default: {
    schema() {
      sequence("item_ids").create({ as: t.bigInt() });
      table("items").create({
        columns: { id: t.bigInt().notNull().default(nextval("item_ids")), label: t.text() },
        primaryKey: ["id"],
      });
    },
  },
} as MigrationModule;

/** The default ALONE, so the default is what gets judged rather than op 0. */
const DEFAULT_ONLY: MigrationModule = {
  default: {
    schema() {
      table("items").create({
        columns: { id: t.bigInt().notNull().default(nextval("item_ids")), label: t.text() },
        primaryKey: ["id"],
      });
    },
  },
} as MigrationModule;

/** Just the sequence, for the message-distinction arm. */
const SEQUENCE_ONLY: MigrationModule = {
  default: {
    schema() {
      sequence("item_ids").create({ as: t.bigInt() });
    },
  },
} as MigrationModule;

function deploy(
  migration: MigrationModule,
  projectSchema: string,
  driver: DriverConfig,
): Promise<unknown> {
  return apply({
    migration,
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: {},
    policy: [charter(projectSchema)],
    approved: true,
    appliedBy: "sequence-default-dialects",
    nameFallback: "seq_default",
  });
}

test("PostgreSQL carries a nextval default into the catalog", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const schema = uniqueNamespace("seqdef");
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    await deploy(WITH_SEQUENCE, schema, { kind: "postgres", url: pgUrl() });

    // Read the DEFAULT from the catalog, not the plan: a matrix `Yes` that
    // lowered without the default would leave a column that never populates.
    const { rows } = await client.query(
      `SELECT column_default FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'items' AND column_name = 'id'`,
      [schema],
    );
    assert.match(
      String(rows[0]?.column_default ?? ""),
      /nextval\(/,
      `the id column must default from the sequence; got ${rows[0]?.column_default}`,
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

test("SQLite refuses a nextval default, naming the default rather than a sequence op", async () => {
  const work = mkdtempSync(join(HERE, "seqdef-sq-"));
  try {
    await assert.rejects(
      deploy(DEFAULT_ONLY, "main", {
        kind: "sqlite",
        appPath: join(work, "app.db"),
        journalPath: join(work, "mig.db"),
      }),
      DEFAULT_REFUSAL,
      "SQLite must refuse the default itself",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("MySQL refuses a nextval default, naming the default rather than a sequence op", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL nextval default skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const database = uniqueNamespace("seqdefmy");
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    await assert.rejects(
      deploy(DEFAULT_ONLY, database, { kind: "mysql", url: MYSQL_URL }),
      DEFAULT_REFUSAL,
      "MySQL must refuse the default itself",
    );

    // Refused, not half-applied.
    const [tables] = await admin.query(
      `SELECT TABLE_NAME AS t FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?`,
      [database],
    );
    assert.deepEqual(
      (tables as Array<{ t: string }>).map((row) => row.t),
      [],
      "a refused migration must leave no table behind",
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

test("the two matrix rows refuse with different text, so neither stands in for the other", async () => {
  // The arm this file exists for. A migration carrying BOTH is refused at op 0 -
  // the sequence creation - and never reaches the default, so it cannot tell the
  // rows apart. Only authoring each alone can, and the two messages must differ
  // or one row is silently covering for the other.
  const work = mkdtempSync(join(HERE, "seqdef-dist-"));
  const driver = (name: string): DriverConfig => ({
    kind: "sqlite",
    appPath: join(work, `${name}.db`),
    journalPath: join(work, `${name}-mig.db`),
  });
  try {
    await assert.rejects(
      deploy(SEQUENCE_ONLY, "main", driver("standalone")),
      STANDALONE_REFUSAL,
      "the standalone sequence row has its own message",
    );
    await assert.rejects(
      deploy(DEFAULT_ONLY, "main", driver("default")),
      DEFAULT_REFUSAL,
      "and the default row has a different one",
    );
    // Belt and braces: neither message may match the other's pattern.
    await assert.rejects(
      deploy(SEQUENCE_ONLY, "main", driver("standalone2")),
      (error: Error) => !DEFAULT_REFUSAL.test(error.message),
      "the standalone refusal must not also read as the default refusal",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
