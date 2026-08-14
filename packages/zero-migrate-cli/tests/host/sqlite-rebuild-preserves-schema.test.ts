// What survives SQLite's table rebuild.
//
// SQLite cannot `ALTER TABLE … DROP COLUMN` in the general case, so the engine
// reaches most column alterations through a REBUILD: create a new table, copy the
// rows, swap. Every schema feature the old table carried has to be re-emitted on
// the new one, and anything forgotten is lost SILENTLY — the migration succeeds,
// the rows are all there, and an invariant is simply gone.
//
// That is the failure this file exists for. `backfill-cursor-ordering` covers row
// CORRECTNESS through data steps; nothing covered SCHEMA SURVIVAL through the
// rebuild, which is the same class of silent loss one layer down.
//
// The table carries one of everything a rebuild must re-emit: a primary key, a
// NOT NULL, a DEFAULT, a UNIQUE index and a plain index. The migration then drops
// an UNRELATED column, so the rebuild is incidental to the author's intent —
// exactly the case where a quiet loss would go unnoticed.
//
// ENFORCEMENT, NOT PRESENCE. The unique index is checked by inserting a duplicate
// and requiring the write to fail. An index row in `sqlite_master` proves the
// object was re-created; only a rejected duplicate proves the INVARIANT came back
// with it, and those are different failures.
//
// GATE: none. SQLite needs no server.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
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

const OWNER_APP = "app_sqlite_rebuild";
const TABLE = "rebuild_rows";

/** A named unique INDEX, not a table-level `uniques` entry: SQLite refuses the
 *  latter outright, which is why the docs steer portable migrations to the index
 *  form. Using the supported spelling keeps this test about the REBUILD. */
function project(): string {
  const work = mkdtempSync(join(HERE, "sqliterebuild-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

# Required because the index add emits an existence-guard probe, and the probe's
# schema scope is built only from schema.cross_schema includes (see F605).
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
    join(work, "migrations", "20260101000000_base.ts"),
    `import { table, t } from "zero-migrate";
export const name = "base";
export default {
  up() {
    table("${TABLE}").create({
      columns: {
        id: t.int().notNull(),
        email: t.string({ length: 100 }).notNull(),
        status: t.string({ length: 20 }).notNull().default("new"),
        doomed: t.int(),
      },
      primaryKey: ["id"],
      indexes: [
        { name: "${TABLE}_email_uq", on: ["email"], unique: true },
        { name: "${TABLE}_status_ix", on: ["status"] },
      ],
    });
    table("${TABLE}").insert({ rows: [{ id: 1, email: "a@x", status: "new", doomed: 9 }] });
  },
};
`,
  );
  // Dropping an unrelated column is what forces the rebuild.
  writeFileSync(
    join(work, "migrations", "20260102000000_drop.ts"),
    `import { table } from "zero-migrate";
export const name = "drop_doomed";
export default { up() { table("${TABLE}").column("doomed").drop(); } };
`,
  );
  return work;
}

function apply(work: string): Promise<{ code: number | null; text: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", `sqlite:${join(work, "app.db")}`,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
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

test("a SQLite rebuild carries every schema feature across", async () => {
  const work = project();
  const dbPath = join(work, "app.db");
  try {
    const applied = await apply(work);
    assert.equal(applied.code, 0, `the rebuild must succeed; ${applied.text}`);

    const db = new DatabaseSync(dbPath, { readOnly: true });
    const ddl = (
      db.prepare(`SELECT sql FROM sqlite_master WHERE type='table' AND name = ?`).get(TABLE) as {
        sql: string;
      }
    ).sql;
    const indexes = (
      db
        .prepare(`SELECT name FROM sqlite_master WHERE type='index' AND tbl_name = ? ORDER BY name`)
        .all(TABLE) as Array<{ name: string }>
    ).map((row) => row.name);
    const rows = db.prepare(`SELECT count(*) AS n FROM ${TABLE}`).get() as { n: number };
    db.close();

    // The rows, first: a rebuild that drops data fails here before any schema check.
    assert.equal(Number(rows.n), 1, "the rebuild must keep the table's rows");
    // The op the author actually asked for.
    assert.doesNotMatch(ddl, /doomed/i, "the dropped column must be gone");

    // Everything the author did NOT ask to change must come back.
    assert.match(ddl, /PRIMARY KEY/i, `the primary key must survive: ${ddl}`);
    assert.match(ddl, /"email"[^,]*NOT NULL/i, `the NOT NULL must survive: ${ddl}`);
    assert.match(ddl, /"status"[^,]*DEFAULT/i, `the DEFAULT must survive: ${ddl}`);
    assert.ok(
      indexes.includes(`${TABLE}_status_ix`),
      `the plain index must survive; saw ${JSON.stringify(indexes)}`,
    );
    assert.ok(
      indexes.includes(`${TABLE}_email_uq`),
      `the unique index must survive; saw ${JSON.stringify(indexes)}`,
    );

    // ENFORCEMENT. The index existing is not the invariant holding.
    const writable = new DatabaseSync(dbPath);
    let rejected = false;
    try {
      writable.prepare(`INSERT INTO ${TABLE} (id, email, status) VALUES (2, 'a@x', 'new')`).run();
    } catch {
      rejected = true;
    }
    writable.close();
    assert.ok(
      rejected,
      "the rebuilt unique index must still REJECT a duplicate — an index row in " +
        "sqlite_master proves the object came back, not that the constraint did",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
