// A backfill cursor must be a column whose value nothing else can move.
//
// The whole resumable-backfill design rests on the cursor being stable: progress
// is a saved cursor value, and a resume selects rows after it. A column whose
// value can change out from under that - a GENERATED column recomputed when its
// sources change, or MySQL's `ON UPDATE CURRENT_TIMESTAMP` - breaks it in the way
// F452 made concrete: rows silently revisited or silently skipped, with the
// migration reporting success.
//
// Both backends refuse such a column, and this file measures both. It exists
// because the two implementations look different enough that a reader comparing
// them can conclude one is missing the rule: PostgreSQL extracts
// `reject_generated_cursor_column`, MySQL inlines the same decision inside its
// cursor validation. A function-name comparison across the backends reports an
// asymmetry that is not there - which is why this asserts BEHAVIOUR on both.
//
// MySQL's rule is the wider one. It also refuses `ON UPDATE CURRENT_TIMESTAMP`,
// which has no PostgreSQL equivalent as a column clause (the analogous risk there
// is a trigger, covered by `reject_trigger_interactions`).
//
// The plain-cursor arms are the control. Without them, "generated is refused"
// also holds for a build that refuses every cursor, and the file would report a
// working guard over a dead feature.
//
// GATE: PG arms need `ZERO_MIGRATE_TEST_PG_URL`, MySQL arms need
// `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_cursor_column_kind";
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

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

/** A backfill cursored on `k`, whichever kind of column the fixture made it. */
const bump = {
  default: {
    up() {
      table("rows_t").backfill({
        set: { val: (col) => col("val").add(1) },
        where: (col) => col("id").gt(0),
        cursorColumns: ["k"],
        cursorStability: { mode: "externalInvariant", name: "rows_k_immutable" },
        batchSize: 2,
        name: "bump_all",
      });
    },
  },
} as MigrationModule;

const PLAIN_DDL = `id int NOT NULL, val int NOT NULL, k int NOT NULL UNIQUE, PRIMARY KEY (id)`;
const PLAIN_SEED = `INSERT INTO @T (id, val, k) VALUES (1,0,10),(2,0,20),(3,0,30)`;
const GENERATED_DDL =
  `id int NOT NULL, val int NOT NULL, ` +
  `k int GENERATED ALWAYS AS (id * 10) STORED UNIQUE, PRIMARY KEY (id)`;
const GENERATED_SEED = `INSERT INTO @T (id, val) VALUES (1,0),(2,0),(3,0)`;

test("PostgreSQL refuses a generated cursor column and accepts a plain one", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const run = async (ddl: string, seed: string): Promise<void> => {
    const schema = uniqueNamespace("cursor_kind_pg");
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      await client.query(`CREATE TABLE "${schema}".rows_t (${ddl})`);
      await client.query(seed.replaceAll("@T", `"${schema}".rows_t`));
      await apply({
        migration: bump,
        ownerApp: OWNER_APP,
        projectSchema: schema,
        driver,
        registry: { rows_t: OWNER_APP },
        policy: [charter(schema)],
        approved: true,
        appliedBy: "backfill-cursor-column-kind",
        nameFallback: "bump",
      });
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
    // The control: an ordinary unique column really is an acceptable cursor.
    await run(PLAIN_DDL, PLAIN_SEED);

    await assert.rejects(
      run(GENERATED_DDL, GENERATED_SEED),
      /cursor component/,
      "a generated column must be refused as a cursor - its value moves when its sources do",
    );
  } finally {
    await client.end().catch(() => {});
  }
});

test("MySQL refuses generated and ON UPDATE cursor columns, and accepts a plain one", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL cursor-kind arms skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;

  const run = async (ddl: string, seed: string): Promise<void> => {
    const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
    const database = uniqueNamespace("cursor_kind_my");
    try {
      await admin.query(`CREATE DATABASE \`${database}\``);
      await admin.query(`CREATE TABLE \`${database}\`.rows_t (${ddl}) ENGINE=InnoDB`);
      await admin.query(seed.replaceAll("@T", `\`${database}\`.rows_t`));
      await apply({
        migration: bump,
        ownerApp: OWNER_APP,
        projectSchema: database,
        driver: { kind: "mysql", url: MYSQL_URL },
        registry: { rows_t: OWNER_APP },
        policy: [charter(database)],
        approved: true,
        appliedBy: "backfill-cursor-column-kind",
        nameFallback: "bump",
      });
    } finally {
      await admin
        .query(
          `DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${database}_migrations\``,
        )
        .catch(() => {});
      await admin.end().catch(() => {});
    }
  };

  // The control.
  await run(PLAIN_DDL, PLAIN_SEED);

  await assert.rejects(
    run(GENERATED_DDL, GENERATED_SEED),
    /cursor component/,
    "a generated column must be refused as a cursor on MySQL too",
  );

  // MySQL's wider half. The seed writes DISTINCT explicit timestamps: letting the
  // default supply them makes all three rows share one value and fail the UNIQUE,
  // which would abort the fixture before the guard was ever consulted.
  await assert.rejects(
    run(
      `id int NOT NULL, val int NOT NULL, ` +
        `k timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, ` +
        `PRIMARY KEY (id), UNIQUE KEY rows_k_uk (k)`,
      `INSERT INTO @T (id, val, k) VALUES ` +
        `(1,0,'2026-01-01 00:00:01'),(2,0,'2026-01-01 00:00:02'),(3,0,'2026-01-01 00:00:03')`,
    ),
    /cursor component/,
    "an ON UPDATE CURRENT_TIMESTAMP column must be refused: the server moves it on every write",
  );
});
