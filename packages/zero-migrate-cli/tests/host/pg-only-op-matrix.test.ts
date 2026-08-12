// The PostgreSQL-only OP rows of the support matrix, checked against real
// databases.
//
// Completes the audit begun in `unsupported-constraints-refuse.test.ts`,
// `index-facet-matrix.test.ts` and `trigger-facet-matrix.test.ts`. The rows here
// are whole operations rather than facets of one:
//
//   Comment              | Yes | No[^16] | No[^16]
//   Standalone sequence  | Yes | No[^22] | No[^22]
//   Materialized view    | Yes | No[^20] | No[^20]
//
// THE CHARTER GATE FIRES BEFORE THE DIALECT GATE, and that is the trap this file
// exists to document as much as to test. A materialized view is also a
// charter-gated vendor primitive, so under an ordinary charter ALL THREE DIALECTS
// refuse it with the identical `VENDOR_OP_DENIED ... requires the
// allowMaterializedView capability` - including PostgreSQL, which declares it
// supported. Read quickly, that looks like "the dialect gate works". It is not the
// dialect gate at all, and it says nothing about the matrix row.
//
// So the materialized-view arms grant `code.materialized_view` first. Only with
// the vendor gate satisfied does the dialect decision become the thing being
// measured - and then SQLite and MySQL each refuse in their own words while
// PostgreSQL applies.
//
// Every PostgreSQL arm asserts the object EXISTS in the catalog afterwards, not
// that apply resolved. A refusal suite whose control only checks for the absence
// of an exception cannot tell a working feature from one that silently did
// nothing.
//
// GATE: PG arms need `ZERO_MIGRATE_TEST_PG_URL`, MySQL arms need
// `ZERO_MIGRATE_MYSQL_URL`. SQLite arms always run.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { table, t, sequence, view } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_pg_only_ops";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** `code.materialized_view` is a GLOBAL knob - scoping it is rejected at load
 *  with `ScopeIllegalForGlobalKnob`, so it takes `scope = "all"`. */
function charter(scopeName: string, materializedView = false): string {
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
${
  materializedView
    ? `
[[grant]]
key = "code.materialized_view"
value = true
scope = "all"
`
    : ""
}`;
}

const SHAPES = {
  comment: () => {
    table("items").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    table("items").comment("a table comment");
  },
  standalone_sequence: () => {
    sequence("item_ids").create({ as: t.bigInt() });
  },
  materialized_view: () => {
    table("items").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    view("items_mv").create({ as: (q) => q.from("items").select(["id"]), materialized: true });
  },
  /** The control shape: a plain structured view, declared supported everywhere. */
  plain_view: () => {
    table("items").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    view("items_v").create({ as: (q) => q.from("items").select(["id"]) });
  },
} as const;

type ShapeName = keyof typeof SHAPES;

function applyShape(
  shape: ShapeName,
  projectSchema: string,
  driver: DriverConfig,
): Promise<unknown> {
  return apply({
    migration: { default: { up: SHAPES[shape] } } as MigrationModule,
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: {},
    policy: [charter(projectSchema, shape === "materialized_view")],
    approved: true,
    appliedBy: "pg-only-op-matrix",
    nameFallback: `op_${shape}`,
  });
}

function withSqliteFile<T>(prefix: string, run: (driver: DriverConfig) => Promise<T>) {
  const work = mkdtempSync(join(HERE, `${prefix}-`));
  return run({
    kind: "sqlite",
    appPath: join(work, "app.db"),
    journalPath: join(work, "mig.db"),
  }).finally(() => rmSync(work, { recursive: true, force: true }));
}

test("SQLite refuses the PostgreSQL-only ops, each in its own words", async () => {
  for (const [shape, reason] of [
    ["comment", /COMMENT ON is PostgreSQL-only/],
    ["standalone_sequence", /standalone sequence objects are PostgreSQL-only/],
    ["materialized_view", /SQLite has no materialized views/],
  ] as ReadonlyArray<readonly [ShapeName, RegExp]>) {
    await withSqliteFile(`pgop-sq-${shape}`, async (driver) => {
      await assert.rejects(
        applyShape(shape, "main", driver),
        reason,
        // Matching the SPECIFIC reason, not just "it threw". The materialized-view
        // arm is the one that needs it: under a charter without the grant, that
        // shape refuses on every dialect with a vendor denial, which would satisfy
        // a looser assertion while measuring nothing about SQLite.
        `SQLite must refuse ${shape} for the dialect reason the matrix records`,
      );
    });
  }
});

test("MySQL refuses the PostgreSQL-only ops, each in its own words", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL PG-only op matrix skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  try {
    for (const [shape, reason] of [
      ["comment", /COMMENT ON is PostgreSQL-only/],
      ["standalone_sequence", /standalone sequence objects are PostgreSQL-only/],
      ["materialized_view", /materialized views are PostgreSQL-only/],
    ] as ReadonlyArray<readonly [ShapeName, RegExp]>) {
      const database = uniqueNamespace(`pgop_my_${shape}`);
      await admin.query(`CREATE DATABASE \`${database}\``);
      try {
        await assert.rejects(
          applyShape(shape, database, { kind: "mysql", url: MYSQL_URL }),
          reason,
          `MySQL must refuse ${shape} for the dialect reason the matrix records`,
        );
        const [tables] = await admin.query(
          "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
          [database],
        );
        assert.deepEqual(
          (tables as Array<{ TABLE_NAME: string }>).map((row) => row.TABLE_NAME),
          [],
          `the refused ${shape} migration must leave nothing behind`,
        );
      } finally {
        await admin
          .query(
            `DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${database}_migrations\``,
          )
          .catch(() => {});
      }
    }
  } finally {
    await admin.end().catch(() => {});
  }
});

test("PostgreSQL control: every one of those ops applies and the object is in the catalog", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const probes: ReadonlyArray<readonly [ShapeName, string, () => Promise<number>]> = [
    ["comment", "comment", async () => 0],
    ["standalone_sequence", "sequence", async () => 0],
    ["materialized_view", "matview", async () => 0],
    ["plain_view", "view", async () => 0],
  ];

  for (const [shape] of probes) {
    const schema = uniqueNamespace(`pgop_ctl_${shape}`);
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      await applyShape(shape, schema, driver);

      // The object has to be THERE. "apply resolved" would also hold for an
      // engine that emitted nothing at all.
      const found = await (async (): Promise<number> => {
        switch (shape) {
          case "comment": {
            const r = await client.query(
              `SELECT obj_description(c.oid, 'pg_class') AS note
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE n.nspname = $1 AND c.relname = 'items'`,
              [schema],
            );
            assert.equal(r.rows[0]?.note, "a table comment", "the comment must be on the table");
            return 1;
          }
          case "standalone_sequence": {
            const r = await client.query(
              `SELECT sequencename FROM pg_sequences WHERE schemaname = $1`,
              [schema],
            );
            return r.rows.length;
          }
          case "materialized_view": {
            const r = await client.query(`SELECT matviewname FROM pg_matviews WHERE schemaname = $1`, [
              schema,
            ]);
            return r.rows.length;
          }
          default: {
            const r = await client.query(`SELECT viewname FROM pg_views WHERE schemaname = $1`, [
              schema,
            ]);
            return r.rows.length;
          }
        }
      })();
      assert.equal(found, 1, `PostgreSQL must really carry the ${shape} object`);
    } finally {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
           DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
        )
        .catch(() => {});
    }
  }
  await client.end().catch(() => {});
});
