// A string literal inside a view body cannot break out of the view's statement.
//
// Third position in the same family, and each one needed checking separately
// because the RENDERED SHAPE differs and so does what detects a slip:
//
//   column DEFAULT   inside `CREATE TABLE (...)`   naive render -> syntax error
//   COMMENT ON       statement level               naive render -> executes
//   view body        depends on the dialect        see below
//
// A view predicate is rendered by the SelectAst printer, not by the column-default
// path, so it is a third code route to the same requirement. And the two dialects
// disagree about the shape:
//
//   SQLite   CREATE VIEW "v" AS SELECT … WHERE ("name" = '…')   -- parenthesised
//   PG       CREATE VIEW v AS SELECT … WHERE name = '…'         -- bare, at the tail
//
// So PostgreSQL's form is injectable and SQLite's is not. Measured, not assumed --
// rendering this payload naively against live PostgreSQL:
//
//   … CREATE VIEW vq_v AS SELECT id, name FROM vq_t WHERE name = ''; DROP TABLE vq_bystander; --'
//     -> executed without error
//     -> tables after: ["vq_t", "vq_v"]
//
// The bystander is gone. So the bystander assertion is load-bearing on the
// PostgreSQL arm, exactly as in the COMMENT case.
//
// THE ROUND-TRIP HERE IS FUNCTIONAL RATHER THAN TEXTUAL. Reading the stored view
// definition back would only show that some text was stored. Instead a row is
// inserted whose `name` IS the payload, and the view must RETURN it: that passes
// only if the predicate's literal means exactly the authored string. An escaper
// that doubled a quote too many, or dropped one, yields a view that matches
// nothing -- silently, since an empty view is not an error.
//
// GATES: SQLite always runs; the PostgreSQL arm needs `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
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

const OWNER_APP = "app_view_quote";
const TABLE = "vq_t";
const VIEW = "vq_v";
const BYSTANDER = "vq_bystander";

/** No leading word: `IS 'o'brien'` would fail to parse under a naive render and
 *  prove nothing about the case that stays silent. */
const PAYLOAD = `'; DROP TABLE ${BYSTANDER}; --`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "viewquote-"));
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
    JSON.stringify({ [TABLE]: OWNER_APP, [BYSTANDER]: OWNER_APP }),
  );
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, view, t } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("${BYSTANDER}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), name: t.text() },
      primaryKey: ["id"],
    });
    view("${VIEW}").create({
      as: (q) =>
        q
          .from("${TABLE}")
          .select(["id", "name"])
          .where((col) => col("name").eq(${JSON.stringify(PAYLOAD)})),
    });
  },
};
`,
  );
  return work;
}

function apply(
  work: string,
  databaseUrl: string,
  namespace: string | null,
): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", databaseUrl,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      ...(namespace ? ["--schema", namespace] : []),
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

test("SQLite keeps a view predicate's literal intact, and the view still matches", () => {
  const work = project();
  try {
    const appPath = join(work, "app.db");
    const applied = apply(work, `sqlite:${appPath}`, null);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    const db = new DatabaseSync(appPath);
    db.prepare(`INSERT INTO ${TABLE} (id, name) VALUES (1, ?)`).run(PAYLOAD);
    // Functional round-trip: the predicate must mean exactly the authored string.
    const matched = db.prepare(`SELECT COUNT(*) AS n FROM ${VIEW}`).get() as { n: number };
    const names = (
      db
        .prepare(`SELECT name FROM sqlite_master WHERE name LIKE 'vq%' ORDER BY 1`)
        .all() as Array<{ name: string }>
    ).map((row) => row.name);
    db.close();

    assert.ok(names.includes(BYSTANDER), "no second statement may have run");
    assert.equal(
      Number(matched.n),
      1,
      "the view must match the row whose value IS the payload -- an escaper that " +
        "added or dropped a quote yields a view matching nothing, which is silent",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("PostgreSQL keeps a view predicate's literal intact, where it could terminate", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("vq_pg");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = apply(work, pgUrl(), namespace);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    // Load-bearing here: PostgreSQL renders the predicate bare at the statement
    // tail, so a naive render of this payload executes cleanly (verified) and
    // nothing else would reveal the compromise.
    const tables = (
      await client.query(
        `SELECT table_name FROM information_schema.tables WHERE table_schema = $1 ORDER BY 1`,
        [namespace],
      )
    ).rows.map((row: { table_name: string }) => row.table_name);
    assert.ok(
      tables.includes(BYSTANDER),
      `the payload's DROP must not have executed; tables were ${JSON.stringify(tables)}`,
    );

    await client.query(`INSERT INTO "${namespace}"."${TABLE}" (id, name) VALUES (1, $1)`, [
      PAYLOAD,
    ]);
    const matched = (
      await client.query(`SELECT COUNT(*)::int AS n FROM "${namespace}"."${VIEW}"`)
    ).rows[0].n;
    assert.equal(
      Number(matched),
      1,
      "the view must match the row whose value IS the payload",
    );
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
});
