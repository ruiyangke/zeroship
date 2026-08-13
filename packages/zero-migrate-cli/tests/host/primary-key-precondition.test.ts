// `primaryKey().replace()` and `.drop()` state the key they expect to find, and
// the engine refuses if the live key is anything else.
//
// That precondition is the whole safety of the operation. A primary-key
// replacement drops the existing key; if the author's belief about which key that
// is turns out to be wrong, the migration removes a constraint nobody meant to
// remove, on a table that keeps working until something relies on the uniqueness
// that is no longer there.
//
// `e2e-id-lifecycle.test.ts` covers the happy path - replace, add and drop with
// matching `expectedColumns`. It never supplies a MISMATCHED one, so the guard
// itself had no end-to-end coverage on either dialect.
//
// ORDER IS SIGNIFICANT, and that arm is the one worth having. `(tenant_id, id)`
// and `(id, tenant_id)` are different keys - different index, different uniqueness
// under partial data, different prefix for lookups - and an implementation
// comparing them as sets would accept a table whose key is not the one declared.
// Both backends compare ordered; only a test that reverses the order can tell.
//
// WHERE THE REFUSAL COMES FROM, measured rather than assumed: both dialects are
// stopped by the FOLD, offline, before either backend opens a transaction - the
// message is `fold: primary key precondition failed`. The backends carry live
// preconditions too (PostgreSQL `validate_current_primary_key`, MySQL the same
// decision inlined), but on this path the fold gets there first.
//
// I first read the two as refusing at DIFFERENT layers, because MySQL surfaced the
// fold message while I had matched PostgreSQL against the backend wording. Running
// PostgreSQL alone showed it emits the fold message too. Asserting the same string
// on both is what makes that shared, and a later divergence visible.
//
// GATE: PG arms need `ZERO_MIGRATE_TEST_PG_URL`, MySQL arms need
// `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_pk_precondition";
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

/** The live table's key is `(tenant_id, id)`, in that order. */
const created = {
  name: "create_items",
  default: {
    up() {
      table("items").create({
        columns: {
          tenant_id: t.int().notNull(),
          id: t.int().notNull(),
          label: t.string({ length: 32 }).notNull(),
        },
        primaryKey: ["tenant_id", "id"],
        // The replacement target needs a pre-existing UNIQUE candidate: the fold
        // refuses to promote columns that are not already provably unique, which
        // is itself what stops a replacement from silently weakening the key.
        uniques: [{ name: "items_id_key", columns: ["id"] }],
      });
    },
  },
} as MigrationModule & { name: string };

/** Replace the key, declaring `expected` as the one currently in place. */
function replaceWith(expected: readonly string[]): MigrationModule & { name: string } {
  return {
    name: "replace_pk",
    default: {
      up() {
        table("items").primaryKey().replace({
          expectedColumns: [...expected],
          columns: ["id"],
        });
      },
    },
  } as MigrationModule & { name: string };
}

async function livePgKey(client: import("pg").Client, schema: string): Promise<string[]> {
  const { rows } = await client.query(
    `SELECT a.attname
       FROM pg_index i
       JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey)
      WHERE i.indrelid = ($1 || '.items')::regclass AND i.indisprimary
      ORDER BY array_position(i.indkey, a.attnum)`,
    [`"${schema}"`],
  );
  return rows.map((row) => row.attname as string);
}

test("PostgreSQL refuses a primary-key replacement whose expected key is wrong", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const withTable = async <T>(run: (schema: string) => Promise<T>): Promise<T> => {
    const schema = uniqueNamespace("pk_pre_pg");
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      await apply({
        migration: created,
        ownerApp: OWNER_APP,
        projectSchema: schema,
        driver,
        registry: {},
        policy: [charter(schema)],
        approved: true,
        appliedBy: "primary-key-precondition",
        nameFallback: created.name,
      });
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

  const replace = (expected: readonly string[], schema: string) =>
    apply({
      migration: replaceWith(expected),
      priorMigrations: [created],
      priorNameFallbacks: [created.name],
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver,
      registry: { items: OWNER_APP },
      policy: [charter(schema)],
      approved: true,
      appliedBy: "primary-key-precondition",
      nameFallback: "replace_pk",
    });

  try {
    // The control: the true key, in the true order, really does replace.
    await withTable(async (schema) => {
      await replace(["tenant_id", "id"], schema);
      assert.deepEqual(
        await livePgKey(client, schema),
        ["id"],
        "a matching precondition must let the replacement through",
      );
    });

    // A key the table does not have.
    await withTable(async (schema) => {
      await assert.rejects(
        replace(["label"], schema),
        /primary key precondition failed/,
        "a wrong expected key must be refused",
      );
      assert.deepEqual(
        await livePgKey(client, schema),
        ["tenant_id", "id"],
        "and the live key must be untouched",
      );
    });

    // The right columns in the WRONG ORDER. A set comparison would accept this.
    await withTable(async (schema) => {
      await assert.rejects(
        replace(["id", "tenant_id"], schema),
        /primary key precondition failed/,
        "the same columns in a different order are a different key and must be refused",
      );
      assert.deepEqual(
        await livePgKey(client, schema),
        ["tenant_id", "id"],
        "and the live key must be untouched",
      );
    });
  } finally {
    await client.end().catch(() => {});
  }
});

test("MySQL refuses the same mismatches, including column order", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL primary-key precondition skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;

  const withTable = async <T>(
    run: (
      admin: Awaited<ReturnType<typeof mysql.createConnection>>,
      database: string,
    ) => Promise<T>,
  ): Promise<T> => {
    const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
    const database = uniqueNamespace("pk_pre_my");
    try {
      await admin.query(`CREATE DATABASE \`${database}\``);
      await apply({
        migration: created,
        ownerApp: OWNER_APP,
        projectSchema: database,
        driver: { kind: "mysql", url: MYSQL_URL },
        registry: {},
        policy: [charter(database)],
        approved: true,
        appliedBy: "primary-key-precondition",
        nameFallback: created.name,
      });
      return await run(admin, database);
    } finally {
      await admin
        .query(
          `DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${database}_migrations\``,
        )
        .catch(() => {});
      await admin.end().catch(() => {});
    }
  };

  const liveKey = async (
    admin: Awaited<ReturnType<typeof mysql.createConnection>>,
    database: string,
  ): Promise<string[]> => {
    const [rows] = await admin.query(
      `SELECT COLUMN_NAME FROM information_schema.STATISTICS
        WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'items' AND INDEX_NAME = 'PRIMARY'
        ORDER BY SEQ_IN_INDEX`,
      [database],
    );
    return (rows as Array<{ COLUMN_NAME: string }>).map((row) => row.COLUMN_NAME);
  };

  const replace = (expected: readonly string[], database: string) =>
    apply({
      migration: replaceWith(expected),
      priorMigrations: [created],
      priorNameFallbacks: [created.name],
      ownerApp: OWNER_APP,
      projectSchema: database,
      driver: { kind: "mysql", url: MYSQL_URL },
      registry: { items: OWNER_APP },
      policy: [charter(database)],
      approved: true,
      appliedBy: "primary-key-precondition",
      nameFallback: "replace_pk",
    });

  // A key the table does not have.
  await withTable(async (admin, database) => {
    await assert.rejects(
      replace(["label"], database),
      /primary key precondition failed/,
      "a wrong expected key must be refused on MySQL too",
    );
    assert.deepEqual(
      await liveKey(admin, database),
      ["tenant_id", "id"],
      "and the live key must be untouched",
    );
  });

  // Right columns, wrong order.
  await withTable(async (admin, database) => {
    await assert.rejects(
      replace(["id", "tenant_id"], database),
      /primary key precondition failed/,
      "MySQL must compare the key as ordered, not as a set",
    );
    assert.deepEqual(
      await liveKey(admin, database),
      ["tenant_id", "id"],
      "and the live key must be untouched",
    );
  });
});
