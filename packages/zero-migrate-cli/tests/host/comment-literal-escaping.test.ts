// A comment string cannot break out of `COMMENT ON … IS '…'`.
//
// This is the sibling of `ddl-string-literal-escaping.test.ts`, and the difference
// between them is the whole reason it exists.
//
// A column DEFAULT is rendered INSIDE the `CREATE TABLE (...)` parentheses, so a
// payload that closes its literal leaves an unclosed paren and the statement dies
// with a syntax error. A COMMENT is rendered at STATEMENT LEVEL, where closing the
// literal ends a complete, valid statement and whatever follows is simply the next
// one.
//
// That is measured, not assumed. Rendering this file's payload naively against
// PostgreSQL:
//
//   SET search_path TO "…"; COMMENT ON TABLE cmt_t IS ''; DROP TABLE cmt_bystander; --'
//     -> executed without error
//     -> tables after: ["cmt_t"]
//
// The bystander is gone. So in THIS position the bystander table is the
// load-bearing assertion rather than a cheap guard: a slip here is silent, valid
// SQL that does exactly what the payload says, executed with migration privileges.
//
// The payload is `'; DROP TABLE … ; --` rather than something with a leading word,
// because a leading word would make the first statement `IS 'o'brien'` and fail to
// parse -- turning a silent compromise into a loud error and weakening the test.
//
// Comments are PostgreSQL-only in this engine; `pg-only-op-matrix.test.ts` covers
// the refusal on the other two targets, so this file does not repeat it.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

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

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const OWNER_APP = "app_comment_quote";
const TABLE = "cmt_t";
const BYSTANDER = "cmt_bystander";

/** Closes the literal, ends the statement, drops a table, comments out the tail.
 *  Verified to execute cleanly under a naive render — see the header. */
const PAYLOAD = `'; DROP TABLE ${BYSTANDER}; --`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "cmtquote-"));
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
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("${BYSTANDER}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    table("${TABLE}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    table("${TABLE}").comment(${JSON.stringify(PAYLOAD)});
    table("${TABLE}").column("id").comment(${JSON.stringify(PAYLOAD)});
  },
};
`,
  );
  return work;
}

test("a comment carrying a statement terminator cannot execute it", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("cmtq");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = spawnSync(
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
    const text = `${applied.stdout ?? ""}\n${applied.stderr ?? ""}`
      .replace(/^WARNING.*$/gm, "")
      .trim();
    assert.equal(applied.status, 0, `the migration must apply; ${text}`);

    // THE assertion. Unlike a default, this payload in this position is valid SQL
    // when rendered naively, so nothing else would reveal the compromise.
    const tables = (
      await client.query(
        `SELECT table_name FROM information_schema.tables WHERE table_schema = $1 ORDER BY 1`,
        [namespace],
      )
    ).rows.map((row: { table_name: string }) => row.table_name);
    assert.ok(
      tables.includes(BYSTANDER),
      `the comment's DROP must not have executed; tables were ${JSON.stringify(tables)}`,
    );

    // And the comment must still be the comment -- an escaper that dropped the
    // quote entirely would also leave the bystander standing.
    const tableComment = (
      await client.query(`SELECT obj_description(($1 || '.' || $2)::regclass) AS c`, [
        namespace,
        TABLE,
      ])
    ).rows[0].c;
    const columnComment = (
      await client.query(`SELECT col_description(($1 || '.' || $2)::regclass, 1) AS c`, [
        namespace,
        TABLE,
      ])
    ).rows[0].c;

    assert.equal(tableComment, PAYLOAD, "the table comment must be stored verbatim");
    assert.equal(columnComment, PAYLOAD, "the column comment must be stored verbatim");
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
