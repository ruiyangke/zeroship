// Editing a migration that has already been applied, measured against a live server.
//
// `docs/security-model.md` states it as a guarantee of apply: "a reused identity
// with a different checksum fails". That is the tamper property. It is what stops
// an edited migration from being replayed against a database that already ran the
// original, whether the edit came from a bad rebase, a hand-fix on a hotfix
// branch, or someone changing history deliberately.
//
// WHAT WAS ALREADY COVERED, AND WHY IT IS NOT THIS. `e2e-pg.test.ts` applies a
// MODIFIED artifact into a FRESH schema and asserts it folds a different checksum
// anchor. That proves the checksum is a function of the ops; it never re-applies
// against the schema that holds the original, so it cannot show that apply
// REFUSES. `cli.test.ts` asserts the "checksum mismatch" wording, but over a
// fabricated status object with no database. A build that computed a perfect
// checksum and then ignored it on apply would keep both of those green.
//
// THREE ARMS, and the first is the control:
//
//   1. re-applying the UNCHANGED migration succeeds - so arm 2 is about the EDIT,
//      not about re-applying, and not about some unrelated second-run failure;
//   2. re-applying the EDITED migration under the same filename (same version
//      identity) is refused, naming both checksums;
//   3. `status --strict` reports the same thing, since that is the CI gate an
//      operator actually puts in front of a deploy.
//
// The load-bearing assertion is none of the exit codes: it is that the edited
// migration's new column is ABSENT afterwards. A refusal that had already applied
// half the edit would exit 1 too, and would be far worse than no check at all.
//
// BOTH DIALECTS, because the checksum being dialect-neutral does not make the
// REFUSAL dialect-neutral: the comparison runs through each backend's journal
// read, and F596 is a standing reminder that MySQL's leaf can differ from
// PostgreSQL's in ways the shared code does not reveal.
//
// The third test pins the neutrality itself. `e2e-pg` pins "the same artifact
// folds the same anchor" WITHIN PostgreSQL; nothing pinned that PostgreSQL and
// MySQL agree, which is what makes one checksum column meaningful across a fleet
// running both. It compares the two servers' recorded checksums to EACH OTHER
// rather than to a literal, so a legitimate IR change moves both and the test
// keeps testing agreement instead of a frozen hash.
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
const OWNER_APP = "app_tamper";
/** The identity. It is the FILENAME that fixes the version, so writing a different
 *  body to this same path is precisely "a reused identity with a different
 *  checksum" - the case the security model says must fail. */
const MIGRATION_FILE = "20260101000000_create_things.ts";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function body(extraColumn: string, tableName = "things"): string {
  return `import { table, t } from "zero-migrate";
export const name = "create_things";
export default {
  schema() {
    table("${tableName}").create({
      columns: { id: t.int().notNull()${extraColumn} },
      primaryKey: ["id"],
    });
  },
};
`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "tamper-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ things: OWNER_APP, victim: OWNER_APP }));
  writeFileSync(join(work, "migrations", MIGRATION_FILE), body(""));
  return work;
}

function cli(
  work: string,
  schema: string,
  databaseUrl: string,
  argv: string[],
): Promise<{ code: number | null; text: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, ...argv,
        "--dir", join(work, "migrations"),
        "--database-url", databaseUrl,
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        "--schema", schema,
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

/** The per-dialect plumbing the scenario needs: make the namespace, read the
 *  table's columns back, tear down. Everything else about the scenario is
 *  identical, which is the point — the same edit must be refused either way. */
interface Dialect {
  readonly name: string;
  readonly prefix: string;
  setUp(namespace: string): Promise<void>;
  databaseUrl(namespace: string): string;
  columns(namespace: string): Promise<string[]>;
  tables(namespace: string): Promise<string[]>;
  /** A bystander table the tampered `up` renames the rollback's target onto. */
  createBystander(namespace: string): Promise<void>;
  tearDown(namespace: string): Promise<void>;
  close(): Promise<void>;
}

async function postgres(): Promise<Dialect> {
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  return {
    name: "PostgreSQL",
    prefix: "tamper_pg",
    async setUp(namespace) {
      await client.query(`CREATE SCHEMA "${namespace}"`);
    },
    databaseUrl: () => pgUrl(),
    async columns(namespace) {
      const { rows } = await client.query(
        `SELECT column_name FROM information_schema.columns
          WHERE table_schema = $1 AND table_name = 'things' ORDER BY column_name`,
        [namespace],
      );
      return rows.map((row) => row.column_name as string);
    },
    async tables(namespace) {
      const { rows } = await client.query(
        `SELECT table_name FROM information_schema.tables
          WHERE table_schema = $1 ORDER BY table_name`,
        [namespace],
      );
      return rows.map((row) => row.table_name as string);
    },
    async createBystander(namespace) {
      await client.query(`CREATE TABLE "${namespace}".victim (id int PRIMARY KEY)`);
    },
    async tearDown(namespace) {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
           DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
        )
        .catch(() => {});
    },
    async close() {
      await client.end().catch(() => {});
    },
  };
}

async function mysql(): Promise<Dialect> {
  const driver = (await import("mysql2/promise")).default;
  const connection = await driver.createConnection({ uri: String(MYSQL_URL) });
  const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
  return {
    name: "MySQL",
    prefix: "tamper_my",
    async setUp(namespace) {
      await connection.query(`CREATE DATABASE \`${namespace}\``);
    },
    databaseUrl: (namespace) => `${base}/${namespace}`,
    async columns(namespace) {
      const [rows] = await connection.query(
        `SELECT COLUMN_NAME AS n FROM information_schema.COLUMNS
          WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'things' ORDER BY 1`,
        [namespace],
      );
      return (rows as Array<{ n: string }>).map((row) => row.n);
    },
    async tables(namespace) {
      const [rows] = await connection.query(
        `SELECT TABLE_NAME AS n FROM information_schema.TABLES
          WHERE TABLE_SCHEMA = ? ORDER BY 1`,
        [namespace],
      );
      return (rows as Array<{ n: string }>).map((row) => row.n);
    },
    async createBystander(namespace) {
      await connection.query(`CREATE TABLE \`${namespace}\`.victim (id int PRIMARY KEY)`);
    },
    async tearDown(namespace) {
      await connection.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
      await connection.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
    },
    async close() {
      await connection.end().catch(() => {});
    },
  };
}

/** Runs the whole scenario and asserts it, returning the two checksums the
 *  refusal quoted so a caller can compare them ACROSS dialects. */
async function tamperScenario(dialect: Dialect): Promise<{ journal: string; set: string }> {
  const namespace = uniqueNamespace(dialect.prefix);
  const work = project(namespace);
  const url = dialect.databaseUrl(namespace);
  try {
    await dialect.setUp(namespace);

    const applied = await cli(work, namespace, url, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `${dialect.name}: the original must apply; ${applied.text}`);
    assert.deepEqual(await dialect.columns(namespace), ["id"], "the original shape lands");

    // ARM 1, the control — the same bytes re-applied are a no-op, so a refusal in
    // arm 2 cannot be "re-applying is refused" or a second run failing generally.
    const unchanged = await cli(work, namespace, url, ["apply", "--approve"]);
    assert.equal(
      unchanged.code,
      0,
      `${dialect.name}: re-applying the UNCHANGED migration must succeed, or arm 2 ` +
        `proves nothing; ${unchanged.text}`,
    );

    // ARM 2 — the edit. Same filename, so the same version identity, different body.
    writeFileSync(join(work, "migrations", MIGRATION_FILE), body(", sneaky: t.text()"));
    const tampered = await cli(work, namespace, url, ["apply", "--approve"]);

    assert.equal(
      tampered.code,
      1,
      `${dialect.name}: the edited migration must be refused; ${tampered.text}`,
    );
    // By CONTENT: an unrelated failure would also exit 1.
    assert.match(
      tampered.text,
      /checksum drift/,
      `${dialect.name}: the refusal must be the checksum gate: ${tampered.text}`,
    );
    // Both sides, so an operator can see it is a disagreement rather than a
    // corrupted journal, and can tell which artifact is the odd one out.
    const journal = /journal has ([0-9a-f]{64})/.exec(tampered.text);
    const supplied = /set has ([0-9a-f]{64})/.exec(tampered.text);
    assert.ok(journal, `${dialect.name}: the refusal must quote the journal checksum: ${tampered.text}`);
    assert.ok(supplied, `${dialect.name}: and the supplied set's checksum: ${tampered.text}`);
    assert.notEqual(
      journal[1],
      supplied[1],
      "the two checksums must actually differ, or the message is describing nothing",
    );

    // THE ASSERTION THAT MATTERS. Exit 1 with the edit half-applied would be worse
    // than no gate: the schema would carry a change no journal entry describes.
    assert.deepEqual(
      await dialect.columns(namespace),
      ["id"],
      `${dialect.name}: the edited migration must not have applied any part of itself`,
    );

    // ARM 3 — the CI gate reports it too. An operator who never runs `apply`
    // manually meets this through `status --strict`.
    const strict = await cli(work, namespace, url, ["status", "--strict"]);
    assert.equal(
      strict.code,
      1,
      `${dialect.name}: strict status must fail on a tampered set; ${strict.text}`,
    );
    assert.match(
      strict.text,
      /checksum mismatch: create_things/,
      `${dialect.name}: and must name the migration: ${strict.text}`,
    );

    return { journal: journal[1], set: supplied[1] };
  } finally {
    await dialect.tearDown(namespace);
    rmSync(work, { recursive: true, force: true });
  }
}

test("PostgreSQL refuses an edited already-applied migration, and lands nothing", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PG tamper arm skipped");
    return;
  }
  const dialect = await postgres();
  try {
    await tamperScenario(dialect);
  } finally {
    await dialect.close();
  }
});

test("MySQL refuses an edited already-applied migration, and lands nothing", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL tamper arm skipped");
    return;
  }
  const dialect = await mysql();
  try {
    await tamperScenario(dialect);
  } finally {
    await dialect.close();
  }
});

test("the recorded checksum is the same on both servers", async (ctx) => {
  if (!PG_URL || !MYSQL_URL) {
    ctx.skip("both database URLs required for the cross-dialect checksum arm");
    return;
  }
  const pg = await postgres();
  const my = await mysql();
  try {
    const fromPg = await tamperScenario(pg);
    const fromMysql = await tamperScenario(my);

    // Compared to EACH OTHER, never to a literal: a legitimate IR change moves
    // both, and this keeps asserting agreement rather than a frozen hash.
    assert.equal(
      fromPg.journal,
      fromMysql.journal,
      "the same migration must record the same checksum on both servers, or one " +
        "checksum column cannot describe a fleet running both",
    );
    assert.equal(
      fromPg.set,
      fromMysql.set,
      "and the edited artifact must fold identically too",
    );
  } finally {
    await pg.close();
    await my.close();
  }
});

/** The same tamper, aimed at ROLLBACK instead of apply.
 *
 *  Rollback has no authored `down` to tamper with - the engine refuses a migration
 *  that declares one at all ("the recorder does not capture down(); rollback runs
 *  the engine's synthesised inverse", pinned in `rollback-atomicity`). That closes
 *  the obvious attack and moves the question rather than settling it: the inverse
 *  is synthesised from the migration's ops, so if those are re-read from the
 *  EDITABLE files, editing `up` redirects whatever the rollback drops.
 *
 *  The edit here repoints `up` from the table it created onto a PRE-EXISTING
 *  bystander. If the synthesis trusted the file, the rollback would drop `victim`
 *  - a table this migration never created and whose rows no journal entry
 *  describes. That is strictly worse than the apply case: apply's tamper adds
 *  something unrecorded, this one DESTROYS something unrelated.
 *
 *  The control is the reason the refusal means anything: an UNTAMPERED rollback of
 *  the same migration must succeed and drop the table it really created. Without
 *  it, "rollback refused" is equally consistent with rollback being broken. */
async function rollbackTamperScenario(dialect: Dialect): Promise<void> {
  const namespace = uniqueNamespace(`${dialect.prefix}_rb`);
  const work = project(namespace);
  const url = dialect.databaseUrl(namespace);
  const rollback = ["rollback", "--steps", "1", "--approve", "--backup-acknowledged"];
  try {
    await dialect.setUp(namespace);
    await dialect.createBystander(namespace);

    const applied = await cli(work, namespace, url, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `${dialect.name}: the original must apply; ${applied.text}`);

    // TAMPER: same filename, so the same identity; `up` now names the bystander.
    writeFileSync(join(work, "migrations", MIGRATION_FILE), body("", "victim"));
    const tampered = await cli(work, namespace, url, rollback);

    assert.equal(
      tampered.code,
      1,
      `${dialect.name}: the rollback of an edited migration must be refused; ${tampered.text}`,
    );
    assert.match(
      tampered.text,
      /checksum drift/,
      `${dialect.name}: and it must be the checksum gate that refuses: ${tampered.text}`,
    );

    // THE ASSERTION THAT MATTERS: nothing was dropped. Not the bystander the edit
    // aimed at, and not the real table either.
    const survivors = await dialect.tables(namespace);
    assert.ok(
      survivors.includes("victim"),
      `${dialect.name}: the bystander the edit aimed at must survive; saw ${JSON.stringify(survivors)}`,
    );
    assert.ok(
      survivors.includes("things"),
      `${dialect.name}: and the refused rollback must not have dropped its own table either; ` +
        `saw ${JSON.stringify(survivors)}`,
    );

    // THE CONTROL: restore the real bytes and the same rollback goes through,
    // dropping the table this migration actually created and nothing else.
    writeFileSync(join(work, "migrations", MIGRATION_FILE), body(""));
    const honest = await cli(work, namespace, url, rollback);
    assert.equal(
      honest.code,
      0,
      `${dialect.name}: the untampered rollback must succeed, or the refusal above ` +
        `proves only that rollback is broken; ${honest.text}`,
    );
    const after = await dialect.tables(namespace);
    assert.ok(!after.includes("things"), `${dialect.name}: it must drop its own table`);
    assert.ok(after.includes("victim"), `${dialect.name}: and still leave the bystander alone`);
  } finally {
    await dialect.tearDown(namespace);
    rmSync(work, { recursive: true, force: true });
  }
}

test("PostgreSQL refuses to roll back an edited migration, dropping nothing", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PG rollback tamper arm skipped");
    return;
  }
  const dialect = await postgres();
  try {
    await rollbackTamperScenario(dialect);
  } finally {
    await dialect.close();
  }
});

test("MySQL refuses to roll back an edited migration, dropping nothing", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL rollback tamper arm skipped");
    return;
  }
  const dialect = await mysql();
  try {
    await rollbackTamperScenario(dialect);
  } finally {
    await dialect.close();
  }
});
