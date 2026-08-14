// A stand-alone `addConstraint` must be rendered in the TARGET dialect's syntax.
//
// `renderer.rs` declares `Capability::AlterTableAddConstraint` TRUE for MySQL and
// PostgreSQL (and false for SQLite, which refuses the op in lowering instead), and
// MySQL genuinely supports `ALTER TABLE t ADD CONSTRAINT n UNIQUE (c)`. But
// `lower_add_constraint` reuses the differ's render seam, whose `quote_ident` is
// documented as "Quote a Postgres identifier" — so on MySQL the emitted ALTER
// carried double quotes and the server rejected it:
//
//   CREATE TABLE `db`.`t` (...)                                  <- correct
//   ALTER TABLE "db"."t" ADD CONSTRAINT "t_uq" UNIQUE (val)      <- PG quoting
//   -> You have an error in your SQL syntax ... near '"db"."t" ADD CONSTRAINT ...'
//
// That is a supported operation failing as a RAW SERVER PARSER ERROR partway
// through a deploy. The contrast that makes it a defect rather than an unsupported
// op is `setColumnNotNull`, which IS unsupported on MySQL and says so with a typed
// refusal before any SQL is sent.
//
// EACH ARM ASSERTS THE CONSTRAINT EXISTS AFTERWARDS, not just a zero exit. A
// renderer that emitted syntactically valid SQL naming the wrong object would exit
// 0 and leave the table unconstrained, which is the failure this file most needs to
// exclude.
//
// The DROP arm is here because the original sweep could not separate it: that probe
// added a unique first, so it died on the same statement. `declarative.rs` already
// branches `DROP FOREIGN KEY` (MySQL) against `DROP CONSTRAINT` (PG), so the drop
// path may always have been fine — this says so either way rather than leaving it
// inferred.
//
// GATES: `ZERO_MIGRATE_TEST_PG_URL`, `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
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
const OWNER_APP = "app_constraint_quoting";
const TABLE = "cq_rows";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(namespace: string, step: string): string {
  const work = mkdtempSync(join(HERE, "cquote-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(namespace)}] }

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
    join(work, "migrations", "20260101000000_base.ts"),
    `import { table, t } from "zero-migrate";
export const name = "base";
export default {
  schema() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), val: t.int().notNull() },
      primaryKey: ["id"],
      uniques: [{ name: "${TABLE}_inline_uq", columns: ["val"] }],
    });
  },
};
`,
  );
  writeFileSync(join(work, "migrations", "20260102000000_step.ts"), step);
  return work;
}

const ADD_UNIQUE = `import { table } from "zero-migrate";
export const name = "add_uq";
export default { schema() { table("${TABLE}").unique("${TABLE}_added_uq").add({ columns: ["id", "val"] }); } };
`;

/** Drops the constraint the BASE migration created inline, so this arm stands on
 *  its own instead of depending on the add arm succeeding first. */
const DROP_CONSTRAINT = `import { table } from "zero-migrate";
export const name = "drop_ct";
export default { schema() { table("${TABLE}").constraint("${TABLE}_inline_uq").drop(); } };
`;

function apply(work: string, url: string, namespace: string) {
  return new Promise<{ code: number | null; text: string }>((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", url,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        "--schema", namespace,
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

interface Target {
  readonly name: string;
  setUp(ns: string): Promise<void>;
  url(ns: string): string;
  /** Constraint names currently on the table. */
  constraints(ns: string): Promise<string[]>;
  tearDown(ns: string): Promise<void>;
  close(): Promise<void>;
}

async function postgres(): Promise<Target> {
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  return {
    name: "PostgreSQL",
    async setUp(ns) {
      await client.query(`CREATE SCHEMA "${ns}"`);
    },
    url: () => pgUrl(),
    async constraints(ns) {
      const { rows } = await client.query(
        `SELECT constraint_name FROM information_schema.table_constraints
          WHERE table_schema = $1 AND table_name = $2 ORDER BY constraint_name`,
        [ns, TABLE],
      );
      return rows.map((row) => row.constraint_name as string);
    },
    async tearDown(ns) {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${ns}" CASCADE; DROP SCHEMA IF EXISTS "${ns}_migrations" CASCADE`,
        )
        .catch(() => {});
    },
    async close() {
      await client.end().catch(() => {});
    },
  };
}

async function mysql(): Promise<Target> {
  const driver = (await import("mysql2/promise")).default;
  const connection = await driver.createConnection({ uri: String(MYSQL_URL) });
  const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
  return {
    name: "MySQL",
    async setUp(ns) {
      await connection.query(`CREATE DATABASE \`${ns}\``);
    },
    url: (ns) => `${base}/${ns}`,
    async constraints(ns) {
      const [rows] = await connection.query(
        `SELECT CONSTRAINT_NAME AS n FROM information_schema.TABLE_CONSTRAINTS
          WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY 1`,
        [ns, TABLE],
      );
      return (rows as Array<{ n: string }>).map((row) => row.n);
    },
    async tearDown(ns) {
      await connection.query(`DROP DATABASE IF EXISTS \`${ns}\``).catch(() => {});
      await connection.query(`DROP DATABASE IF EXISTS \`${ns}_migrations\``).catch(() => {});
    },
    async close() {
      await connection.end().catch(() => {});
    },
  };
}

async function runAdd(target: Target): Promise<void> {
  const ns = uniqueNamespace("cq");
  const work = project(ns, ADD_UNIQUE);
  try {
    await target.setUp(ns);
    const result = await apply(work, target.url(ns), ns);

    assert.equal(
      result.code,
      0,
      `${target.name}: a supported addConstraint must apply, not fail at the server; ${result.text}`,
    );
    // A raw parser error is the specific regression, so name it.
    assert.doesNotMatch(
      result.text,
      /error in your SQL syntax/i,
      `${target.name}: the server must never be handed syntax it cannot parse: ${result.text}`,
    );
    // Exit 0 is not enough: valid SQL naming the wrong object would also exit 0.
    assert.ok(
      (await target.constraints(ns)).includes(`${TABLE}_added_uq`),
      `${target.name}: the constraint must actually exist afterwards`,
    );
  } finally {
    await target.tearDown(ns);
    rmSync(work, { recursive: true, force: true });
  }
}

async function runDrop(target: Target): Promise<void> {
  const ns = uniqueNamespace("cqd");
  const work = project(ns, DROP_CONSTRAINT);
  try {
    await target.setUp(ns);
    const result = await apply(work, target.url(ns), ns);

    assert.equal(result.code, 0, `${target.name}: the drop must apply; ${result.text}`);
    assert.doesNotMatch(
      result.text,
      /error in your SQL syntax/i,
      `${target.name}: the drop must not hand the server unparseable syntax: ${result.text}`,
    );
    assert.ok(
      !(await target.constraints(ns)).includes(`${TABLE}_inline_uq`),
      `${target.name}: and the constraint must actually be gone`,
    );
  } finally {
    await target.tearDown(ns);
    rmSync(work, { recursive: true, force: true });
  }
}

test("PostgreSQL adds a stand-alone unique constraint", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const target = await postgres();
  try {
    await runAdd(target);
  } finally {
    await target.close();
  }
});

test("MySQL adds a stand-alone unique constraint", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const target = await mysql();
  try {
    await runAdd(target);
  } finally {
    await target.close();
  }
});

test("PostgreSQL drops a named constraint", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const target = await postgres();
  try {
    await runDrop(target);
  } finally {
    await target.close();
  }
});

test("MySQL drops a named constraint", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const target = await mysql();
  try {
    await runDrop(target);
  } finally {
    await target.close();
  }
});
