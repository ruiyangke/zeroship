// Losing the SQLite journal sidecar fails closed. It never re-runs data migrations.
//
// SQLite keeps the journal in a SEPARATE FILE beside the application database
// (`sqlite-journal-is-a-separate-file.test.ts` pins that). The cost of that design
// is an operator mistake it invites: copy, restore or deploy `app.db` WITHOUT
// `app.migrations.db`, and the engine's record of what already ran is gone while
// the schema and the data are still there.
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
// THE SECOND MIGRATION'S UPDATE IS DELIBERATELY NON-IDEMPOTENT (`n = n + 10`).
// An INSERT of fixed primary keys would have been useless here: a re-run would be
// refused by the primary key, so "the data is unchanged" would hold whether or not
// apply had stopped, and the test would prove nothing. With an increment, a re-run
// is VISIBLE -- and the test verifies that by performing one itself before the
// real assertion, so a reader can see the instrument works.
//
// The diagnostic is worth knowing but is NOT asserted in detail: the operator sees
// SQLite's own `table "…" already exists` rather than a message naming the missing
// journal. That is a poor diagnosis of a real situation, recorded in the review log
// rather than pinned here, since the safety property is what matters and the
// wording is free to improve.
//
// GATE: none. SQLite runs everywhere.

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
  up() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), n: t.int().notNull() },
      primaryKey: ["id"],
    });
    table("${TABLE}").insert({ rows: [{ id: 1, n: 10 }] });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_b.ts"),
    `import { table } from "zero-migrate";
export const name = "b";
export default {
  up() {
    // Non-idempotent on purpose: running it twice is observable.
    table("${TABLE}").update({
      set: { n: (col) => col("n").add(10) },
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
    assert.equal(valueOfN(appPath), 20, "10 inserted, then 10 added by the second migration");
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
