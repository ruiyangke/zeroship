// `whenUnsupported: "collapse"` must degrade ONLY where it has to.
//
// Native partitioning is PostgreSQL-only. Declaring `partitionBy` without an opt-out
// is refused on MySQL and SQLite, and the refusal names the escape hatch:
// `partitionBy.whenUnsupported: "collapse"`. That hatch is the interesting part,
// because an opt-in degradation has a failure mode a refusal does not: it can
// degrade MORE than it was asked to.
//
// Specifically, if `collapse` were applied unconditionally rather than per-target,
// a migration written for three databases would quietly stop partitioning on
// PostgreSQL too. The table would still be created, every row would still be
// stored, and the only lost thing would be the feature the author asked for. That is
// the same silent-loss shape as `t.vector` degrading to a BLOB (F611), except an
// author opted into it for OTHER targets and would have no reason to re-check the
// one that works.
//
// So the assertion on PostgreSQL is `relkind = 'p'` — the catalog's own word for
// "this is a partitioned table" — not that the table exists. Existence is what a
// wrongly-collapsed table would also satisfy.
//
// The other half of the hatch is that it demands TOTAL bounds: without a default
// partition the engine refuses with `PARTITION_BOUNDS_NOT_TOTAL`, because
// collapsing a partition set into one table is only meaningful if every row had a
// home to begin with. That refusal is asserted too, since it is what stops
// `collapse` from being a blanket "ignore my partitioning" switch.
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
const OWNER_APP = "app_partition_collapse";
const TABLE = "pcollapse_rows";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** `withDefaultPartition: false` is the bounds-not-total case the engine refuses. */
function project(withDefaultPartition: boolean): string {
  const work = mkdtempSync(join(HERE, "pcollapse-"));
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
    JSON.stringify({ [TABLE]: OWNER_APP, [`${TABLE}_all`]: OWNER_APP }),
  );
  const defaultChild = withDefaultPartition
    ? `    table("${TABLE}").partition("${TABLE}_all").create({ default: true });\n`
    : "";
  writeFileSync(
    join(work, "migrations", "20260101000000_base.ts"),
    `import { table, t } from "zero-migrate";
export const name = "base";
export default {
  up() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), occurred_at: t.timestamp().notNull() },
      primaryKey: ["id", "occurred_at"],
      partitionBy: { range: ["occurred_at"], whenUnsupported: "collapse" },
    });
${defaultChild}  },
};
`,
  );
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

test("collapse leaves PostgreSQL genuinely partitioned", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("pcollapse_pg");
  const work = project(true);
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = await apply(work, pgUrl(), namespace);
    assert.equal(applied.code, 0, `the collapse-annotated migration must apply; ${applied.text}`);

    const { rows } = await client.query(
      `SELECT c.relkind FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1 AND c.relname = $2`,
      [namespace, TABLE],
    );
    // 'p' is the catalog's word for a partitioned table; 'r' is an ordinary one.
    assert.equal(
      rows[0]?.relkind,
      "p",
      "opting into collapse FOR OTHER TARGETS must not collapse PostgreSQL — the " +
        "table would still exist and still store every row, so existence proves nothing",
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

test("collapse produces a single table on MySQL", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const connection = await driver.createConnection({ uri: String(MYSQL_URL) });
  const namespace = uniqueNamespace("pcollapse_my");
  const work = project(true);
  try {
    await connection.query(`CREATE DATABASE \`${namespace}\``);
    const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
    const applied = await apply(work, `${base}/${namespace}`, namespace);
    assert.equal(applied.code, 0, `collapse must let MySQL apply; ${applied.text}`);

    const [rows] = await connection.query(
      `SELECT TABLE_NAME AS n FROM information_schema.TABLES
        WHERE TABLE_SCHEMA = ? ORDER BY 1`,
      [namespace],
    );
    // The declared partition child must NOT become a stray second table.
    assert.deepEqual(
      (rows as Array<{ n: string }>).map((row) => row.n),
      [TABLE],
      "collapse must yield exactly the one table, not the parent plus an orphaned child",
    );
  } finally {
    await connection.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
    await connection.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
    await connection.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("collapse produces a single table on SQLite", async () => {
  const work = project(true);
  try {
    const applied = await apply(work, `sqlite:${join(work, "app.db")}`, null);
    assert.equal(applied.code, 0, `collapse must let SQLite apply; ${applied.text}`);

    const db = new DatabaseSync(join(work, "app.db"), { readOnly: true });
    const tables = (
      db
        .prepare(
          `SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY 1`,
        )
        .all() as Array<{ name: string }>
    ).map((row) => row.name);
    db.close();

    assert.deepEqual(
      tables,
      [TABLE],
      "collapse must yield exactly the one table on SQLite too",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

/** Without a default partition the bounds are not total, and collapsing a set that
 *  never covered every row would invent a home for rows that had none. The engine
 *  refuses — which is what keeps `collapse` from being a blanket "ignore my
 *  partitioning" switch. */
test("collapse still requires the partition bounds to be total", async () => {
  const work = project(false);
  try {
    const applied = await apply(work, `sqlite:${join(work, "app.db")}`, null);
    assert.equal(applied.code, 1, `partial bounds must be refused; ${applied.text}`);
    assert.match(
      applied.text,
      /PARTITION_BOUNDS_NOT_TOTAL|default: true/,
      `and the refusal must name the missing default partition: ${applied.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
