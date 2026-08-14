// A string INLINED INTO DDL cannot break out of its literal.
//
// `literal-value-binding.test.ts` covers authored values that travel as BINDS. This
// is the other half: a column DEFAULT is not bound, it is rendered into the
// `CREATE TABLE` text itself, by a different function
// (`inline_string_literal`) whose dialect legs do not even share a strategy:
//
//   PostgreSQL / SQLite   `sql_string_literal` -- an ordinary quoted literal with
//                         embedded quotes doubled
//   MySQL                 `_utf8mb4 X'<hex>'` -- hex-encoded, so no quoting rule
//                         applies at all
//
// Two strategies, one requirement. A slip in either writes attacker-chosen text
// into a DDL statement the engine then executes with migration privileges, which is
// the highest-value place in this system to get escaping wrong.
//
// WHAT ACTUALLY DETECTS A SLIP HERE, stated precisely because the obvious answer
// is wrong. A column default sits INSIDE the `CREATE TABLE (...)` parentheses, so a
// payload that closes its literal does not go on to terminate the statement -- it
// leaves an unclosed paren. Checked directly: rendering this payload by naive
// concatenation gives SQLite `near "brien": syntax error`, and the second statement
// never runs. The same is true of the tidier `x'; DROP TABLE …; --` shape.
//
// So the load-bearing assertions are the two ordinary ones:
//
//   exit 0            a broken literal fails LOUDLY in this position, so applying
//                     at all is most of the proof
//   exact round-trip  an escaping scheme that stripped or truncated the quotes
//                     would still apply cleanly while silently corrupting every
//                     default containing an apostrophe
//
// The bystander table is kept as a cheap guard rather than the primary detector: it
// costs one extra `create` and it is the assertion that WOULD fire if this literal
// were ever rendered somewhere a statement can be terminated. Claiming more for it
// than that would misdescribe what this file measures.
//
// MySQL cannot take a DEFAULT on a `TEXT` column at all, so its arm uses
// `t.string({ length })` to land a `VARCHAR`. That is not a workaround for this
// test -- it is the engine's own advice, and refusing `t.text().default(…)` there
// with a message naming the three ways out is correct behaviour met from a
// direction that was not testing for it.
//
// GATES: SQLite always runs; the others need `ZERO_MIGRATE_TEST_PG_URL` and
// `ZERO_MIGRATE_MYSQL_URL`.

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

const OWNER_APP = "app_ddl_quote";
const TABLE = "ddlq_t";
const BYSTANDER = "ddlq_bystander";

/** Tries to close the literal and start a second statement, then comment out the
  *  rest. Carries a bare apostrophe too -- the case a real schema hits by accident
  *  long before anyone tries the rest. */
const PAYLOAD = `o'brien'; DROP TABLE ${BYSTANDER}; --`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** `bounded` renders VARCHAR, which is what MySQL requires before it will accept a
 *  DEFAULT at all. */
function project(bounded: boolean): string {
  const work = mkdtempSync(join(HERE, "ddlquote-"));
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
  const column = bounded ? `t.string({ length: 128 })` : `t.text()`;
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("${BYSTANDER}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), label: ${column}.default(${JSON.stringify(PAYLOAD)}) },
      primaryKey: ["id"],
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

test("SQLite keeps a quote-bearing DDL default inside its literal", () => {
  const work = project(false);
  try {
    const appPath = join(work, "app.db");
    const applied = apply(work, `sqlite:${appPath}`, null);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    const write = new DatabaseSync(appPath);
    write.prepare(`INSERT INTO ${TABLE} (id) VALUES (1)`).run();
    const stored = (
      write.prepare(`SELECT label FROM ${TABLE} WHERE id = 1`).get() as { label: string }
    ).label;
    const tables = (
      write
        .prepare(`SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'ddlq%' ORDER BY 1`)
        .all() as Array<{ name: string }>
    ).map((row) => row.name);
    write.close();

    assert.ok(
      tables.includes(BYSTANDER),
      "no second statement may have run (a guard, not the primary detector here)",
    );
    assert.equal(
      stored,
      PAYLOAD,
      "the default must be stored verbatim -- applying cleanly is most of the proof " +
        "in this position, and this is what catches an escaping scheme that stripped " +
        "or truncated the quotes instead",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("PostgreSQL keeps a quote-bearing DDL default inside its literal", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("ddlq_pg");
  const work = project(false);
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = apply(work, pgUrl(), namespace);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    await client.query(`INSERT INTO "${namespace}"."${TABLE}" (id) VALUES (1)`);
    const stored = (
      await client.query(`SELECT label FROM "${namespace}"."${TABLE}" WHERE id = 1`)
    ).rows[0].label;
    const tables = (
      await client.query(
        `SELECT table_name FROM information_schema.tables WHERE table_schema = $1 ORDER BY 1`,
        [namespace],
      )
    ).rows.map((row: { table_name: string }) => row.table_name);

    assert.ok(tables.includes(BYSTANDER), "the payload's DROP must not have executed");
    assert.equal(stored, PAYLOAD, "the default must be stored verbatim");
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

test("MySQL hex-encodes a quote-bearing DDL default rather than quoting it", async (ctx) => {
  const mysqlUrl = process.env.ZERO_MIGRATE_MYSQL_URL;
  if (!mysqlUrl) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const admin = await driver.createConnection({ uri: String(mysqlUrl) });
  const base = String(mysqlUrl).replace(/\/[^/]*$/, "");
  const namespace = uniqueNamespace("ddlq_my");
  // VARCHAR, because MySQL refuses a DEFAULT on TEXT outright.
  const work = project(true);
  try {
    await admin.query(`CREATE DATABASE \`${namespace}\``);
    const applied = apply(work, `${base}/${namespace}`, namespace);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    await admin.query(`INSERT INTO \`${namespace}\`.\`${TABLE}\` (id) VALUES (1)`);
    const [rows] = await admin.query(
      `SELECT label FROM \`${namespace}\`.\`${TABLE}\` WHERE id = 1`,
    );
    const [tables] = await admin.query(
      `SELECT TABLE_NAME AS n FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY 1`,
      [namespace],
    );
    const names = (tables as Array<{ n: string }>).map((row) => row.n);

    assert.ok(names.includes(BYSTANDER), "the payload's DROP must not have executed");
    assert.equal(
      (rows as Array<{ label: string }>)[0].label,
      PAYLOAD,
      "the default must survive the hex round-trip verbatim",
    );
  } finally {
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("MySQL refuses a DEFAULT on TEXT, naming the ways out", async (ctx) => {
  const mysqlUrl = process.env.ZERO_MIGRATE_MYSQL_URL;
  if (!mysqlUrl) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const admin = await driver.createConnection({ uri: String(mysqlUrl) });
  const base = String(mysqlUrl).replace(/\/[^/]*$/, "");
  const namespace = uniqueNamespace("ddlq_myt");
  // The UNBOUNDED column the arm above deliberately avoids. Pinned because it is
  // why that arm differs, so a reader does not mistake `t.string` for arbitrary.
  const work = project(false);
  try {
    await admin.query(`CREATE DATABASE \`${namespace}\``);
    const applied = apply(work, `${base}/${namespace}`, namespace);
    assert.equal(applied.code, 1, `a TEXT default must be refused; ${applied.text}`);
    assert.match(
      applied.text,
      /t\.string\(\{ length \}\)/,
      `and the refusal must name the bounded-column way out; got: ${applied.text}`,
    );
  } finally {
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
