// A column's `.unique()` can be dropped by name whichever way it was authored.
//
// F649. The same `.unique()` facet produces DIFFERENT CATALOG OBJECTS depending on
// where it is written, which is invisible until you try to remove it:
//
//   create({ columns: { e: t.text().unique() } })   -> a bare UNIQUE INDEX
//   column("e").add({ type: t.text().unique() })    -> a UNIQUE CONSTRAINT
//   create({ uniques: [{ name, columns }] })        -> a UNIQUE CONSTRAINT
//
// All three enforce uniqueness, so no ordinary test separates them. The engine
// emits `CREATE TABLE ...` followed by a separate `CREATE UNIQUE INDEX` at create
// time rather than an inline column `UNIQUE`, which PostgreSQL would have
// materialized as a real constraint named `<table>_<col>_key`.
//
// The consequence is a lifecycle gap: `constraint(name).drop()` succeeds after the
// addColumn route and FAILS AT APPLY after the create route, with
// `constraint "pk_t_e_key" of relation "pk_t" does not exist`. That failure lands
// against a live database rather than at lint, which is the shape this project
// exists to prevent.
//
// The chosen resolution keeps the emitted DDL untouched and makes the DROP
// tolerant: when the named object is not a live constraint but IS a live unique
// index, remove the index. That closes the gap for tables that ALREADY EXIST,
// which a change to create-time DDL could never reach.
//
// THE THIRD TEST IS WHAT KEEPS THIS HONEST. A tolerant drop must not degrade into
// a drop that always "succeeds": a name matching NEITHER a constraint nor an index
// must still be refused, or every typo becomes a silent no-op and the tolerance
// has bought a much worse bug than it fixed.
//
// The uniqueness is checked by INSERTING A DUPLICATE after the drop, not by
// reading the catalog. A `DROP` that ran but removed the wrong object, or removed
// nothing, still leaves a clean catalog query looking plausible.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. The constraint/index distinction is
// PostgreSQL's; MySQL's unique constraint IS an index, and SQLite's inline UNIQUE
// creates an implicit one.

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

const OWNER_APP = "app_uqdrop";
const TABLE = "uf_t";

// PostgreSQL's own name for a column-level unique on this table/column.
const DERIVED = `${TABLE}_e_key`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(body: string): string {
  const work = mkdtempSync(join(HERE, "uqdrop-"));
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

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default { up() { ${body} } };
`,
  );
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

/** Applies `body`, then hands the caller the namespace to inspect. */
async function run(
  body: string,
  check: (
    client: { query: (text: string, values?: unknown[]) => Promise<{ rows: unknown[] }> },
    namespace: string,
    applied: { code: number | null; text: string },
  ) => Promise<void>,
): Promise<void> {
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("uqdrop");
  const work = project(body);
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = apply(work, namespace);
    await check(client as never, namespace, applied);
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

test("a create-time unique facet can be dropped by its derived name", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  await run(
    `table("${TABLE}").create({
       columns: { id: t.int().notNull(), e: t.text().unique() },
       primaryKey: ["id"],
     });
     table("${TABLE}").constraint("${DERIVED}").drop({});`,
    async (client, namespace, applied) => {
      assert.equal(
        applied.code,
        0,
        `the create-time facet must be droppable by name, like the other two ` +
          `authoring routes; ${applied.text}`,
      );
      // The uniqueness must ACTUALLY be gone. A drop that ran but removed nothing
      // leaves this insert failing while the exit code still reads 0.
      await client.query(
        `INSERT INTO "${namespace}"."${TABLE}" (id, e) VALUES (1, 'x'), (2, 'x')`,
      );
      const { rows } = await client.query(
        `SELECT id FROM "${namespace}"."${TABLE}" WHERE e = 'x'`,
      );
      assert.equal(rows.length, 2, "both duplicate rows must survive once the unique is gone");
    },
  );
});

test("CONTROL: the addColumn route still drops by the same name", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  // This route already produced a real constraint, so it must keep working -- the
  // tolerant path must not disturb the case that was never broken.
  await run(
    `table("${TABLE}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
     table("${TABLE}").column("e").add({ type: t.text().unique() });
     table("${TABLE}").constraint("${DERIVED}").drop({});`,
    async (client, namespace, applied) => {
      assert.equal(applied.code, 0, `the constraint route must still drop; ${applied.text}`);
      await client.query(
        `INSERT INTO "${namespace}"."${TABLE}" (id, e) VALUES (1, 'x'), (2, 'x')`,
      );
    },
  );
});

test("CONTROL: a name that is neither constraint nor index is STILL refused", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  // Without this, "tolerant" would mean "never fails", and every mistyped name
  // would become a silent no-op -- a far worse defect than the one being fixed.
  await run(
    `table("${TABLE}").create({
       columns: { id: t.int().notNull(), e: t.text().unique() },
       primaryKey: ["id"],
     });
     table("${TABLE}").constraint("no_such_thing").drop({});`,
    async (_client, _namespace, applied) => {
      assert.equal(
        applied.code,
        1,
        `a name matching NOTHING must still fail; tolerance is for a unique index ` +
          `of that name, not for absence; ${applied.text}`,
      );
    },
  );
});
