// Losing the journal fails closed on every target. It never re-runs data migrations.
//
// The journal lives beside the application's own objects: a separate FILE on SQLite
// (`sqlite-journal-is-a-separate-file.test.ts` pins that), a sibling SCHEMA on
// PostgreSQL and MySQL. Every one of those can be lost independently of the data --
// copy `app.db` without `app.migrations.db`, restore one schema and not its
// `_migrations` twin, or simply drop the latter believing it to be scratch. The
// engine's record of what already ran is then gone while the schema and the data
// are still there.
//
// ALL THREE DIALECTS ARE COVERED because the danger is identical and the mechanism
// is not: one is a filesystem mistake and two are catalog mistakes, so a change
// that fixed or broke one would not obviously touch the others.
//
// The dangerous outcome would be silent re-application. Every migration looks
// pending, so a data migration -- a backfill, an UPDATE, an INSERT -- would run a
// SECOND time against rows that already have it. Nothing about that is a database
// error; it is simply wrong data, discovered much later if at all.
//
// What actually happens is safe: apply stops at the first migration whose DDL
// collides with the schema already present, and nothing after it runs. This file
// pins that, because it is a property of ordering and transactionality rather than
// anything specific to journals, and either could be changed without anyone
// thinking about this scenario.
//
// THE FINAL MIGRATION'S UPDATE IS DELIBERATELY NON-IDEMPOTENT (`n = n + 10`).
// An INSERT of fixed primary keys would have been useless here: a re-run would be
// refused by the primary key, so "the data is unchanged" would hold whether or not
// apply had stopped, and the test would prove nothing. With an increment, a re-run
// is VISIBLE -- and the test verifies that by performing one itself before the
// real assertion, so a reader can see the instrument works.
//
// The diagnostic is worth knowing but is NOT asserted in detail: the operator sees
// the engine's own `… already exists` rather than a message naming the missing
// journal, on all three. That is a poor diagnosis of a real situation, recorded in
// the review log rather than pinned here, since the safety property is what matters
// and the wording is free to improve.
//
// GATES: SQLite always runs; the other arms need `ZERO_MIGRATE_TEST_PG_URL` and
// `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_lost_journal";
const TABLE = "lost_t";

function project(): string {
  const work = mkdtempSync(join(HERE, "lostjr-"));
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
export default {
  schema() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), n: t.int().notNull() },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260101000001_seed.ts"),
    `import { table } from "zero-migrate";
export const name = "seed";
export default {
  data() {
    table("${TABLE}").insert({ rows: [{ id: 1, n: 10 }] });
  },
  inverse() {
    table("${TABLE}").delete({ where: (col) => col("id").eq(1) });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_b.ts"),
    `import { table } from "zero-migrate";
export const name = "b";
export default {
  data() {
    // Non-idempotent on purpose: running it twice is observable.
    table("${TABLE}").update({
      set: { n: (col) => col("n").add(10) },
      where: (col) => col("id").gt(0),
    });
  },
  inverse() {
    table("${TABLE}").update({
      set: { n: (col) => col("n").sub(10) },
      where: (col) => col("id").gt(0),
    });
  },
};
`,
  );
  return work;
}

function apply(work: string, appPath: string): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", `sqlite:${appPath}`,
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
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

function valueOfN(appPath: string): number {
  const db = new DatabaseSync(appPath, { readOnly: true });
  const row = db.prepare(`SELECT n FROM ${TABLE} WHERE id = 1`).get() as { n: number };
  db.close();
  return Number(row.n);
}

/** Apply the same increment by hand, purely to show the assertion below can see
 *  one. Reversed immediately. */
function proveAnIncrementIsVisible(appPath: string): void {
  const before = valueOfN(appPath);
  const db = new DatabaseSync(appPath);
  db.prepare(`UPDATE ${TABLE} SET n = n + 10 WHERE id > 0`).run();
  db.close();
  const after = valueOfN(appPath);
  assert.equal(
    after,
    before + 10,
    "a repeated update must be observable, or the assertion that it did NOT " +
      "repeat cannot detect anything",
  );
  const undo = new DatabaseSync(appPath);
  undo.prepare(`UPDATE ${TABLE} SET n = n - 10 WHERE id > 0`).run();
  undo.close();
  assert.equal(valueOfN(appPath), before, "the instrument check must leave no trace");
}

test("an app database restored without its journal refuses, and re-runs nothing", () => {
  const work = project();
  try {
    const appPath = join(work, "app.db");
    const journalPath = join(work, "app.migrations.db");

    const first = apply(work, appPath);
    assert.equal(first.code, 0, `the first apply must succeed; ${first.text}`);
    assert.equal(valueOfN(appPath), 20, "10 inserted, then 10 added by the update migration");
    assert.ok(existsSync(journalPath), "the journal sidecar must exist to be removed");

    proveAnIncrementIsVisible(appPath);

    // The operator mistake this design invites: the application database is
    // restored, copied or shipped without its sidecar.
    rmSync(journalPath, { force: true });

    const second = apply(work, appPath);
    assert.equal(
      second.code,
      1,
      `a lost journal must fail closed rather than re-applying; ${second.text}`,
    );
    assert.equal(
      valueOfN(appPath),
      20,
      "the data migration must NOT have run a second time -- every migration looks " +
        "pending with the journal gone, and a silent re-run is wrong data rather " +
        "than an error anyone would notice",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

/** The same project, applied through a named schema/database rather than a file. */
function applyNamespaced(
  work: string,
  databaseUrl: string,
  namespace: string,
): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", databaseUrl,
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

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** The increment is arithmetic, so the instrument proof in the SQLite arm above
 *  carries over: a second run of `n = n + 10` on one row is visible as 30 rather
 *  than 20 wherever it happens. What differs per dialect is whether apply STOPS,
 *  which is what these arms measure. */
test("a PostgreSQL schema whose journal schema was dropped refuses, and re-runs nothing", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const { pgUrl } = await import("./live-db.js");
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("lostjr_pg");
  const work = project();
  const readN = async (): Promise<number> =>
    Number(
      (await client.query(`SELECT n FROM "${namespace}"."${TABLE}" WHERE id = 1`)).rows[0].n,
    );
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const first = applyNamespaced(work, pgUrl(), namespace);
    assert.equal(first.code, 0, `the first apply must succeed; ${first.text}`);
    assert.equal(await readN(), 20, "10 inserted, then 10 added");

    // The catalog equivalent of deleting the sidecar.
    await client.query(`DROP SCHEMA "${namespace}_migrations" CASCADE`);

    const second = applyNamespaced(work, pgUrl(), namespace);
    assert.equal(second.code, 1, `a dropped journal schema must fail closed; ${second.text}`);
    assert.equal(
      await readN(),
      20,
      "the data migration must not have run a second time",
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

test("a MySQL database whose journal database was dropped refuses, and re-runs nothing", async (ctx) => {
  const mysqlUrl = process.env.ZERO_MIGRATE_MYSQL_URL;
  if (!mysqlUrl) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const admin = await driver.createConnection({ uri: String(mysqlUrl) });
  const base = String(mysqlUrl).replace(/\/[^/]*$/, "");
  const namespace = uniqueNamespace("lostjr_my");
  const work = project();
  const readN = async (): Promise<number> => {
    const [rows] = await admin.query(
      `SELECT n FROM \`${namespace}\`.\`${TABLE}\` WHERE id = 1`,
    );
    return Number((rows as Array<{ n: number }>)[0].n);
  };
  try {
    await admin.query(`CREATE DATABASE \`${namespace}\``);
    const first = applyNamespaced(work, `${base}/${namespace}`, namespace);
    assert.equal(first.code, 0, `the first apply must succeed; ${first.text}`);
    assert.equal(await readN(), 20, "10 inserted, then 10 added");

    await admin.query(`DROP DATABASE \`${namespace}_migrations\``);

    const second = applyNamespaced(work, `${base}/${namespace}`, namespace);
    assert.equal(second.code, 1, `a dropped journal database must fail closed; ${second.text}`);
    assert.equal(
      await readN(),
      20,
      "the data migration must not have run a second time",
    );
  } finally {
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
    await admin.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
