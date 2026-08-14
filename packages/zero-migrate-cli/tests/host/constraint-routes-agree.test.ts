// Uniques, foreign keys and exclusions mean the same thing inline as via the add op.
//
// `CreateTableArgs` exposes five constraint/index collections that can each also be
// authored as a separate op after the table exists. F646 measured `checks`; F647
// measured `indexes` and found a REAL defect there -- `nullsNotDistinct` was
// unreachable inline because `IrIndex` was missing `rename_all`, so its only
// multi-word field was snake_case on the wire while the DSL sent camelCase. This
// file closes the remaining three, completing that matrix.
//
// They agree. The serde class behind F647 was already swept (`IrConstraintKind`
// carries `rename_all_fields`, so its many multi-word fields -- `on_delete`,
// `on_update`, `initially_deferred`, `references_table` -- are camelCase on the
// wire), so what is left to measure is SEMANTIC divergence: the two routes reach
// the renderer by different paths, inline inside `CREATE TABLE` and the add op as
// an `ALTER`.
//
// THE FK OPTIONS ARE ASSERTED BY BEHAVIOUR, not only by catalog text:
//
//   onDelete: cascade      deleting the parent must REMOVE the child row
//   initiallyDeferred      a child may be inserted BEFORE its parent inside one
//                          transaction, so long as the commit is consistent
//
// The second is the one that matters most here. `DEFERRABLE INITIALLY DEFERRED`
// changes WHEN the constraint is checked, and a route that dropped it would still
// produce a working foreign key -- every ordinary test would pass. The only way to
// see it is to depend on the deferral: insert the child first. Under a
// non-deferred constraint that INSERT fails immediately.
//
// THE ASSERTION IS EQUALITY BETWEEN ROUTES for the catalog text, with the
// constraint name normalized out. Pinning a literal per route would still pass if
// both drifted together into something the DSL never promised.
//
// The exclusion arm asserts enforcement rather than the `EXCLUDE USING ...` string
// for the same reason: a constraint that exists but does not fire is the failure
// worth catching. `btree` is used because `gist` over integers needs `btree_gist`,
// which is a property of the server rather than of this engine.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. Exclusion constraints are PostgreSQL-only, and
// deferrable FKs are not a SQLite/MySQL shape either.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_conroutes";
const TABLES = [
  "cr_parent",
  "fk_inline", "fk_addop",
  "uq_inline", "uq_addop",
  "ex_inline", "ex_addop",
];

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

// Constraint names differ per route ON PURPOSE: a UNIQUE or EXCLUDE constraint
// creates a BACKING INDEX, and index names are schema-scoped rather than
// table-scoped, so reusing one name across two tables collides in the catalog.
// (Foreign keys and checks create no relation, which is why F646 could reuse a
// name across three tables.)
const SOURCE = `import { table, t } from "zero-migrate";
export const name = "a";
const FK = {
  columns: ["pid"],
  references: { table: "cr_parent", columns: ["id"] },
  onDelete: "cascade",
  onUpdate: "setNull",
  deferrable: true,
  initiallyDeferred: true,
};
const EX = { using: "btree", elements: [{ target: "room", operator: "=" }] };
const fkCols = { id: t.int().notNull(), pid: t.int() };
const uqCols = { id: t.int().notNull(), a: t.int().notNull(), b: t.int().notNull() };
const exCols = { id: t.int().notNull(), room: t.int().notNull() };
export default {
  schema() {
    table("cr_parent").create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });

    table("fk_inline").create({
      columns: fkCols, primaryKey: ["id"],
      foreignKeys: [{ name: "fk_i", ...FK }],
    });
    table("fk_addop").create({ columns: fkCols, primaryKey: ["id"] });
    table("fk_addop").foreignKey("fk_a").add(FK);

    table("uq_inline").create({
      columns: uqCols, primaryKey: ["id"],
      uniques: [{ name: "uq_i", columns: ["a", "b"] }],
    });
    table("uq_addop").create({ columns: uqCols, primaryKey: ["id"] });
    table("uq_addop").unique("uq_a").add({ columns: ["a", "b"] });

    table("ex_inline").create({
      columns: exCols, primaryKey: ["id"],
      exclusions: [{ name: "ex_i", ...EX }],
    });
    table("ex_addop").create({ columns: exCols, primaryKey: ["id"] });
    table("ex_addop").exclusion("ex_a").add(EX);
  },
};
`;

function project(): string {
  const work = mkdtempSync(join(HERE, "conroutes-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"
`,
  );
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify(Object.fromEntries(TABLES.map((name) => [name, OWNER_APP]))),
  );
  writeFileSync(join(work, "migrations", "20260101000000_a.ts"), SOURCE);
  return work;
}

function apply(work: string, namespace: string): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", namespace,
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

/** Runs the shared migration once and hands the caller a live connection. */
async function withApplied(
  run: (client: {
    query: (text: string, values?: unknown[]) => Promise<{ rows: unknown[] }>;
  }, namespace: string) => Promise<void>,
): Promise<void> {
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("conroutes");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = apply(work, namespace);
    assert.equal(applied.code, 0, `both routes must apply; ${applied.text}`);
    await run(client as never, namespace);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
}

test("both routes render the identical constraint definition", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  await withApplied(async (client, namespace) => {
    const { rows } = await client.query(
      `SELECT rel.relname, c.contype::text AS contype,
              pg_get_constraintdef(c.oid) AS def,
              c.condeferrable, c.condeferred
         FROM pg_constraint c
         JOIN pg_class rel ON rel.oid = c.conrelid
         JOIN pg_namespace n ON n.oid = rel.relnamespace
        WHERE n.nspname = $1 AND c.contype IN ('f', 'u', 'x')
        ORDER BY rel.relname`,
      [namespace],
    );
    type Row = {
      relname: string; contype: string; def: string;
      condeferrable: boolean; condeferred: boolean;
    };
    const byKind = new Map<string, string[]>();
    for (const row of rows as Row[]) {
      const kind = row.relname.slice(0, 2); // fk / uq / ex
      const normalized = JSON.stringify({
        contype: row.contype,
        // The schema name and the constraint name differ by construction.
        def: row.def.replace(new RegExp(namespace, "g"), "S"),
        deferrable: row.condeferrable,
        deferred: row.condeferred,
      });
      byKind.set(kind, [...(byKind.get(kind) ?? []), normalized]);
    }

    assert.deepEqual(
      [...byKind.keys()].sort(),
      ["ex", "fk", "uq"],
      "all three constraint kinds must exist on both routes",
    );
    for (const [kind, defs] of byKind) {
      assert.equal(defs.length, 2, `${kind}: both routes must produce a constraint`);
      assert.equal(
        new Set(defs).size,
        1,
        `${kind}: inline renders inside CREATE TABLE and the add op as an ALTER, ` +
          `so a divergence here is a real difference in what the author gets; ` +
          `got ${JSON.stringify(defs)}`,
      );
    }
  });
});

test("onDelete cascade reaches the database on both routes", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  await withApplied(async (client, namespace) => {
    for (const [index, table] of ["fk_inline", "fk_addop"].entries()) {
      const parent = 100 + index;
      await client.query(`INSERT INTO "${namespace}"."cr_parent" (id) VALUES ($1)`, [parent]);
      await client.query(
        `INSERT INTO "${namespace}"."${table}" (id, pid) VALUES ($1, $2)`,
        [1, parent],
      );
      await client.query(`DELETE FROM "${namespace}"."cr_parent" WHERE id = $1`, [parent]);
      const { rows } = await client.query(`SELECT id FROM "${namespace}"."${table}"`);
      assert.equal(
        rows.length,
        0,
        `${table}: ON DELETE CASCADE must remove the child row. A route that ` +
          `dropped the action would leave a working FK and an orphan-blocking ` +
          `error instead, which no catalog-text check distinguishes from success`,
      );
    }
  });
});

test("initiallyDeferred reaches the database on both routes", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  await withApplied(async (client, namespace) => {
    for (const [index, table] of ["fk_inline", "fk_addop"].entries()) {
      const parent = 200 + index;
      // The child is inserted BEFORE its parent. Only a DEFERRED constraint
      // tolerates this; a non-deferred one rejects the INSERT immediately, so
      // this is the only arm that can see the option at all.
      await client.query("BEGIN");
      await client.query(
        `INSERT INTO "${namespace}"."${table}" (id, pid) VALUES ($1, $2)`,
        [9, parent],
      );
      await client.query(`INSERT INTO "${namespace}"."cr_parent" (id) VALUES ($1)`, [parent]);
      await client.query("COMMIT");

      const { rows } = await client.query(
        `SELECT id FROM "${namespace}"."${table}" WHERE id = 9`,
      );
      assert.equal(
        rows.length,
        1,
        `${table}: DEFERRABLE INITIALLY DEFERRED must hold the check until COMMIT`,
      );
      await client.query(`DELETE FROM "${namespace}"."${table}" WHERE id = 9`);
    }
  });
});

test("the exclusion constraint fires on both routes", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  await withApplied(async (client, namespace) => {
    for (const table of ["ex_inline", "ex_addop"]) {
      await client.query(
        `INSERT INTO "${namespace}"."${table}" (id, room) VALUES (1, 7)`,
      );
      await assert.rejects(
        () =>
          client.query(`INSERT INTO "${namespace}"."${table}" (id, room) VALUES (2, 7)`),
        /exclusion constraint/,
        `${table}: a constraint that exists but does not fire is the failure worth ` +
          `catching, and catalog text alone cannot tell the two apart`,
      );
      // The same table must still accept a NON-conflicting row, or "rejects
      // everything" would satisfy the assertion above.
      await client.query(
        `INSERT INTO "${namespace}"."${table}" (id, room) VALUES (3, 8)`,
      );
    }
  });
});
