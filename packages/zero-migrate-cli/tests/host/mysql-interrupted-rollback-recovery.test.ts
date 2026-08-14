// An interrupted MySQL ROLLBACK, and whether the printed repair works.
//
// MySQL keeps two interruption markers. `mysql-partial-ddl-recovery.test.ts`
// (F504) verified the APPLY one against a real server: the engine fails closed on
// retry and the `DELETE` it prints genuinely unwedges the deploy. The ROLLBACK one
// — `schema_migrations_rollback_inflight` — had recorder-driven unit tests inside
// `mysql/mod.rs` asserting statement ORDER, and no live coverage at all.
//
// Order is not the property that matters here. A recorder cannot show whether a
// real server leaves the marker after a genuine mid-`down` failure, whether the
// next unwind is refused, or whether the repair the refusal prints actually works
// — which is the only arm that distinguishes "recoverable" from "wedged forever".
//
// THE OBSTACLE. F564 established that a plan lowering to more than one journaled
// step is refused for rollback outright, so "the second statement of the down
// fails" cannot arise. The reachable interruption is a SINGLE `down` statement
// that fails at the server, so the obstacle is a foreign key created out of band:
// MySQL refuses `DROP TABLE parent` while another table references it.
//
// FOUR ARMS, following F504:
//   1. the failed unwind leaves the marker, the journal unchanged, the table alive;
//   2. a retry is REFUSED rather than replaying a `down` whose statements may have
//      auto-committed;
//   3. that refusal names the exact `DELETE` and the version;
//   4. running it lets the rollback complete, and the journal ends coherent.
//
// Arm 4 is the control. Every assertion before it holds equally for an engine that
// is simply stuck, and "refuses forever" is not the guarantee.
//
// GATE: `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import "./addon.js";

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_unwind";

const MIGRATION = `import { table, t } from "zero-migrate";
export const name = "create_parent";
export default {
  schema() {
    table("parent").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`;

function uniqueDatabase(): string {
  return `unwind_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(database: string): string {
  const work = mkdtempSync(join(HERE, "unwind-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(database)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(database)}] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ parent: OWNER_APP }));
  writeFileSync(join(work, "migrations", "20260101000000_m.ts"), MIGRATION);
  return work;
}

function cli(work: string, database: string, argv: string[]) {
  const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...argv,
      "--dir", join(work, "migrations"),
      "--database-url", `${base}/${database}`,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", database,
      "--owner-app", OWNER_APP,
    ],
    { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
  );
  let out = "";
  let err = "";
  child.stdout.on("data", (chunk) => (out += chunk));
  child.stderr.on("data", (chunk) => (err += chunk));
  return new Promise<{ code: number | null; text: string }>((done) =>
    child.on("close", (code) =>
      done({ code, text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

const UNWIND = ["rollback", "--steps", "1", "--approve", "--backup-acknowledged"];

test("an interrupted MySQL unwind is refused on retry, and the printed repair works", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; interrupted-rollback coverage skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const connection = await mysql.createConnection({ uri: MYSQL_URL });

  const database = uniqueDatabase();
  const meta = `${database}_migrations`;
  const work = project(database);

  const tables = async (): Promise<string[]> => {
    const [rows] = await connection.query(
      `SELECT TABLE_NAME AS n FROM information_schema.TABLES
        WHERE TABLE_SCHEMA = ? ORDER BY 1`,
      [database],
    );
    return (rows as Array<{ n: string }>).map((row) => row.n);
  };
  const markers = async (): Promise<string[]> => {
    const [rows] = await connection.query(
      `SELECT version FROM \`${meta}\`.schema_migrations_rollback_inflight`,
    );
    return (rows as Array<{ version: string }>).map((row) => row.version);
  };
  const journal = async (): Promise<string[]> => {
    const [rows] = await connection.query(
      `SELECT name, event_kind FROM \`${meta}\`.schema_migrations ORDER BY event_seq`,
    );
    return (rows as Array<{ name: string; event_kind: string }>).map(
      (row) => `${row.name}/${row.event_kind}`,
    );
  };

  try {
    await connection.query(`CREATE DATABASE \`${database}\``);
    const applied = await cli(work, database, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    // The obstacle: a dependent created OUT OF BAND, so the single `down`
    // statement fails at the server rather than in the fold.
    await connection.query(
      `CREATE TABLE \`${database}\`.dependent (
         id int PRIMARY KEY, pid int,
         CONSTRAINT dep_fk FOREIGN KEY (pid) REFERENCES \`${database}\`.parent(id))`,
    );

    // ARM 1 — the unwind fails, and leaves a marker behind.
    const interrupted = await cli(work, database, UNWIND);
    assert.equal(interrupted.code, 1, `the unwind must fail; ${interrupted.text}`);
    assert.match(
      interrupted.text,
      /foreign key constraint/i,
      `it must fail on the dependency, not somewhere earlier: ${interrupted.text}`,
    );
    const armed = await markers();
    assert.equal(armed.length, 1, "the interrupted unwind must leave exactly one marker");
    assert.deepEqual(
      await journal(),
      ["create_table_parent/applied"],
      "no rolled_back event may be journaled for an unwind that did not finish",
    );
    assert.ok((await tables()).includes("parent"), "the table the drop failed on must survive");

    // ARM 2 and 3 — with the obstacle gone the `down` would now succeed, and the
    // engine still refuses, naming the repair. Removing the obstacle first is what
    // makes this a refusal rather than the same failure repeating.
    await connection.query(`DROP TABLE \`${database}\`.dependent`);
    const refused = await cli(work, database, UNWIND);
    assert.equal(refused.code, 1, `the retry must be refused; ${refused.text}`);
    assert.match(
      refused.text,
      /rollback marker from an interrupted unwind/,
      `the refusal must name the cause: ${refused.text}`,
    );
    assert.match(
      refused.text,
      new RegExp("DELETE FROM `" + meta + "`\\.schema_migrations_rollback_inflight"),
      `the refusal must print the exact repair, qualified with the meta schema: ${refused.text}`,
    );
    assert.match(
      refused.text,
      new RegExp(armed[0]),
      `and it must name the stuck version: ${refused.text}`,
    );
    // The message also separates the two markers, which is the distinction an
    // operator most needs: the apply-side one means an `up` may be half-applied,
    // this one means a `down` may be half-reverted, and the repairs differ.
    assert.match(
      refused.text,
      /different marker from the apply-side/,
      `the refusal must distinguish itself from the apply-side marker: ${refused.text}`,
    );

    // ARM 4, the control — the repair genuinely unwedges it.
    await connection.query(
      `DELETE FROM \`${meta}\`.schema_migrations_rollback_inflight WHERE version = ?`,
      [armed[0]],
    );
    const completed = await cli(work, database, UNWIND);
    assert.equal(completed.code, 0, `the unwind must complete after the repair; ${completed.text}`);
    assert.equal((await markers()).length, 0, "the marker must be gone");
    assert.ok(!(await tables()).includes("parent"), "the table must finally be dropped");
    assert.deepEqual(
      await journal(),
      ["create_table_parent/applied", "create_table_parent/rolled_back"],
      "and the journal must end coherent, appending the rollback rather than editing history",
    );
  } finally {
    await connection.query(`DROP DATABASE IF EXISTS \`${database}\``).catch(() => {});
    await connection.query(`DROP DATABASE IF EXISTS \`${meta}\``).catch(() => {});
    await connection.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
