// One authored `.column().rename()`, three genuinely different outcomes.
//
// The docs are explicit about this and say it in four places:
//
//   dialects.md          "MySQL 8 does not currently support column rename"
//   dialects.md          SQLite's "column rename is one rebuild"
//   writing-migrations   "Column rename works on PostgreSQL and SQLite, not MySQL"
//   writing-migrations   "This isolation rule is PostgreSQL-only"
//
// Nothing verified it. Every rename fixture in this suite is PostgreSQL-only, so
// the documented split rested entirely on prose. If SQLite's rename began
// refusing, or MySQL's began applying, the docs would be wrong and no test would
// fail.
//
// It is worth pinning because of who meets it. An author developing against
// SQLite gets a rename that completes in ONE deploy with no window and no
// `resolve` step. The same authored line, deployed to PostgreSQL, opens a
// two-deploy coexistence window that must be resolved before anything else may
// touch the table - and deployed to MySQL, is refused outright. The migration
// that worked locally is the one that stops the pipeline.
//
// Each arm asserts the DATA as well as the shape: a rename that dropped the
// values while producing the right column name would satisfy a column-list check
// and be the worst of the three outcomes.
//
// GATE: PostgreSQL needs `ZERO_MIGRATE_TEST_PG_URL`, MySQL needs
// `ZERO_MIGRATE_MYSQL_URL`, SQLite always runs. Lint is offline.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);
const OWNER_APP = "app_rename_split";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** The same three migrations for every dialect: create, seed, then rename. */
function project(scope: string): string {
  const work = mkdtempSync(join(HERE, "renamesplit-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(scope)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(scope)}] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ users: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_create_users.ts"),
    `import { table, t } from "zero-migrate";
export const name = "create_users";
export default {
  schema() {
    table("users").create({
      columns: { id: t.int().notNull(), display_name: t.text() },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260101000001_seed_users.ts"),
    `import { table } from "zero-migrate";
export const name = "seed_users";
export default {
  data() {
    table("users").insert({ rows: { id: 1, display_name: "ada" } });
  },
  inverse() {
    table("users").delete({ where: (col) => col("id").eq(1) });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_rename.ts"),
    `import { table, t } from "zero-migrate";
export const name = "rename_display_name";
export default {
  schema() {
    table("users").column("display_name").rename({ to: "full_name", type: t.text() });
  },
};
`,
  );
  return work;
}

function run(work: string, argv: string[]): { code: number | null; out: string; err: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...argv,
      "--dir", join(work, "migrations"),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
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
    out: (result.stdout ?? "").trim(),
    err: (result.stderr ?? "").replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("lint accepts the rename for PostgreSQL and SQLite and refuses it for MySQL", () => {
  const work = project("main");
  try {
    for (const dialect of ["postgres", "sqlite"] as const) {
      const result = run(work, ["lint", "--dialect", dialect]);
      assert.equal(result.code, 0, `${dialect} must accept a column rename; ${result.err}`);
    }
    const mysql = run(work, ["lint", "--dialect", "mysql"]);
    assert.equal(mysql.code, 1, "MySQL must refuse a column rename at lint time");
    assert.match(
      `${mysql.out}${mysql.err}`,
      /renameColumn/,
      `the refusal must name the operation; got: ${mysql.err || mysql.out}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("SQLite completes the rename in one deploy, values intact", () => {
  // One rebuild, per `dialects.md`. No window, no resolve step - which is why an
  // author who only ever deploys to SQLite never meets the PostgreSQL workflow.
  const work = project("main");
  const dbPath = join(work, "app.db");
  try {
    const applied = run(work, [
      "apply", "--approve", "--database-url", `sqlite:${dbPath}`, "--schema", "main",
    ]);
    assert.equal(applied.code, 0, `SQLite must apply the rename; ${applied.err}`);

    const db = new DatabaseSync(dbPath);
    try {
      const columns = db
        .prepare("SELECT name FROM pragma_table_info('users')")
        .all()
        .map((row: Record<string, unknown>) => row.name as string)
        .sort();
      assert.deepEqual(
        columns,
        ["full_name", "id"],
        "the old column is gone immediately: this is a cutover, not a window",
      );
      const rows = db.prepare("SELECT id, full_name FROM users").all() as Array<
        Record<string, unknown>
      >;
      assert.deepEqual(
        rows.map((row) => [Number(row.id), row.full_name]),
        [[1, "ada"]],
        "the rebuild must carry the values across",
      );
    } finally {
      db.close();
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("PostgreSQL opens a window instead: both columns, values in both", async (ctx) => {
  // The contrast that makes the SQLite arm mean something. Same authored line,
  // and the old column is still there afterwards.
  const client = await connectLivePg(ctx);
  if (!client) return;
  const schema = uniqueNamespace("renamesplit");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const applied = run(work, [
      "apply", "--approve", "--database-url", pgUrl(), "--schema", schema,
    ]);
    assert.equal(applied.code, 0, `PostgreSQL must apply the expand phase; ${applied.err}`);

    const { rows: columns } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'users' ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      columns.map((row) => row.column_name),
      ["display_name", "full_name", "id"],
      "PostgreSQL keeps both columns until the rename is resolved",
    );
    const { rows } = await client.query(
      `SELECT display_name, full_name FROM "${schema}".users WHERE id = 1`,
    );
    assert.equal(rows[0]?.display_name, "ada", "the source keeps its value");
    assert.equal(rows[0]?.full_name, "ada", "and the destination starts out carrying it");
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("MySQL refuses the rename at apply, leaving the table as it was", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL rename refusal skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const database = uniqueNamespace("renamesplitmy");
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const work = project(database);
  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    const applied = run(work, [
      "apply", "--approve", "--database-url", MYSQL_URL, "--schema", database,
    ]);
    assert.equal(applied.code, 1, "MySQL must refuse the rename at apply too, not only at lint");
    assert.match(
      applied.err,
      /renameColumn/,
      `the refusal must name the operation; got: ${applied.err}`,
    );

    // Refused, not half-applied: the first migration's table is intact and the
    // rename left no trace. A partial apply here is the outcome that would hurt.
    const [columns] = await admin.query(
      `SELECT COLUMN_NAME AS c FROM information_schema.COLUMNS
        WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'users' ORDER BY c`,
      [database],
    );
    assert.deepEqual(
      (columns as Array<{ c: string }>).map((row) => row.c),
      ["display_name", "id"],
      "the table keeps its original shape - no shadow column, no rename",
    );
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${database}_migrations\``,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
