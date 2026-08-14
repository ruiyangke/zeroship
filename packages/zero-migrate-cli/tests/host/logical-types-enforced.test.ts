// Enums and domains keep their MEANING on every target, not just their shape.
//
// PostgreSQL has native `CREATE TYPE ... AS ENUM` and `CREATE DOMAIN`. MySQL has an
// ENUM column type and no domains at all; SQLite has neither. So on two of three
// targets these logical types must be reproduced out of whatever the dialect does
// have — and a reproduction that keeps the COLUMN but drops the CONSTRAINT would
// apply cleanly, store anything, and tell nobody.
//
// That is not hypothetical. `t.vector` degrades to a bare `BLOB` on SQLite with no
// vector behaviour whatsoever and applies without complaint (recorded in F611), so
// "the migration succeeded" is known not to imply "the type still means something"
// in this engine.
//
// THE ASSERTION IS THEREFORE A REJECTED WRITE, not a column's presence or its
// declared type. An enum column must refuse a value outside its value list; a
// domain column must refuse a value its CHECK forbids. Both are checked on all
// three targets, because the whole risk lives in the two that lack the native
// feature.
//
// Coverage before this file: one host test mentioned `enum` and one mentioned
// `domain`, neither asserting cross-dialect enforcement.
//
// GATES: `ZERO_MIGRATE_TEST_PG_URL`, `ZERO_MIGRATE_MYSQL_URL`. SQLite always runs.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
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

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_logical_types";
const TABLE = "logical_rows";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

const ENUM_MIGRATION = `import { enumType, table, t } from "zero-migrate";
export const name = "base";
export default {
  schema() {
    const state = enumType("logical_state").create({ values: ["invited", "active"] });
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), state: t.enum(state).notNull() },
      primaryKey: ["id"],
    });
  },
};
`;

const DOMAIN_MIGRATION = `import { domain, table, t } from "zero-migrate";
export const name = "base";
export default {
  schema() {
    domain("logical_cents").create({ as: t.bigInt(), check: (value) => value.ge(0), notNull: true });
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), bal: t.domain("logical_cents") },
      primaryKey: ["id"],
    });
  },
};
`;

function project(migration: string): string {
  const work = mkdtempSync(join(HERE, "logicaltypes-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(join(work, "migrations", "20260101000000_base.ts"), migration);
  return work;
}

function apply(
  work: string,
  databaseUrl: string,
  namespace: string | null,
): Promise<{ code: number | null; text: string }> {
  const schemaArgs = namespace ? ["--schema", namespace] : [];
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", databaseUrl,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        ...schemaArgs,
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: work,
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({
        code,
        text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim(),
      }),
    );
  });
}

/** `column`/`badValue` are spliced into an INSERT as literal SQL: both are fixed
 *  strings owned by this file, never author or user input. */
interface Case {
  readonly what: string;
  readonly migration: string;
  readonly column: string;
  readonly badValue: string;
}

const CASES: readonly Case[] = [
  { what: "enum", migration: ENUM_MIGRATION, column: "state", badValue: "'BOGUS'" },
  { what: "domain", migration: DOMAIN_MIGRATION, column: "bal", badValue: "-5" },
];

test("PostgreSQL enforces enum and domain constraints", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  for (const testCase of CASES) {
    const namespace = uniqueNamespace("logical_pg");
    const work = project(testCase.migration);
    const client = new pg.Client({ connectionString: pgUrl() });
    await client.connect();
    try {
      await client.query(`CREATE SCHEMA "${namespace}"`);
      const applied = await apply(work, pgUrl(), namespace);
      assert.equal(applied.code, 0, `${testCase.what} must apply; ${applied.text}`);

      let rejected = false;
      try {
        await client.query(
          `INSERT INTO "${namespace}"."${TABLE}" (id, ${testCase.column}) VALUES (1, ${testCase.badValue})`,
        );
      } catch {
        rejected = true;
      }
      assert.ok(rejected, `PostgreSQL must reject ${testCase.badValue} in the ${testCase.what}`);
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
});

test("MySQL enforces enum and domain constraints without native domains", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
  for (const testCase of CASES) {
    const namespace = uniqueNamespace("logical_my");
    const work = project(testCase.migration);
    const connection = await driver.createConnection({ uri: String(MYSQL_URL) });
    try {
      await connection.query(`CREATE DATABASE \`${namespace}\``);
      const applied = await apply(work, `${base}/${namespace}`, namespace);
      assert.equal(applied.code, 0, `${testCase.what} must apply; ${applied.text}`);

      let rejected = false;
      try {
        await connection.query(
          `INSERT INTO \`${namespace}\`.\`${TABLE}\` (id, ${testCase.column}) VALUES (1, ${testCase.badValue})`,
        );
      } catch {
        rejected = true;
      }
      assert.ok(rejected, `MySQL must reject ${testCase.badValue} in the ${testCase.what}`);
    } finally {
      await connection.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
      await connection.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
      await connection.end().catch(() => {});
      rmSync(work, { recursive: true, force: true });
    }
  }
});

test("SQLite enforces enum and domain constraints with neither native feature", async () => {
  for (const testCase of CASES) {
    const work = project(testCase.migration);
    try {
      const applied = await apply(work, `sqlite:${join(work, "app.db")}`, null);
      assert.equal(applied.code, 0, `${testCase.what} must apply; ${applied.text}`);

      const db = new DatabaseSync(join(work, "app.db"));
      let rejected = false;
      try {
        db.prepare(
          `INSERT INTO ${TABLE} (id, ${testCase.column}) VALUES (1, ${testCase.badValue})`,
        ).run();
      } catch {
        rejected = true;
      }
      db.close();

      assert.ok(
        rejected,
        `SQLite must reject ${testCase.badValue} in the ${testCase.what} — it has no ` +
          `native enum or domain, so this is where a shape-only reproduction would ` +
          `apply cleanly and silently accept anything`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});
