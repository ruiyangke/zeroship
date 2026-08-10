// The `rollback` command driven end to end through the shipped CLI against live
// PostgreSQL and live MySQL: apply a migration, roll it back, and read the catalog
// to see whether the table actually went away.
//
// The addon verb is already proven against real SQLite
// (`crates/zero-migrate-node/tests/rollback_sqlite.rs`), but SQLite runs in-process
// and never crosses the host-driver seam. These arms are the only place the
// host-driven rollback path executes at all: the `pg` and `mysql2` adapters, the
// dialect-native journal SQL, and the blocking project-lock bracket a rollback takes
// because it writes.
//
// What each arm pins:
//
//   1. the reverse SQL the addon reconstructs from the authored envelope actually
//      removes the object the apply created;
//   2. the journal records a `rolled_back` event, so the version returns to pending
//      rather than silently disappearing;
//   3. a second rollback of the same range is a clean no-op rather than an error --
//      nothing is left applied to unwind;
//   4. a rollback with no target and one with no --approve are both refused, and
//      the schema is untouched.
//
// GATING: PostgreSQL via `connectLivePg` (see `live-db.ts`); MySQL skips cleanly
// unless `ZERO_MIGRATE_MYSQL_URL` is set. Runs under `node --import tsx --test`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";
import { createSchemaPolicy, noInjectPolicy } from "./policy.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const TABLE = "rollback_notes";
const SEQUENCE = "rollback_counter";

function spawnCli(args: readonly string[], cwd: string, timeoutMs?: number) {
  return spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...args], {
    encoding: "utf8",
    cwd,
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    // Only the PostgreSQL lock arm passes this. Its subject is a command that never
    // returns, so the test needs its own deadline rather than a result to inspect.
    ...(timeoutMs === undefined ? {} : { timeout: timeoutMs }),
  });
}

function uniqueSchema(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** A migration directory holding one file that authors exactly one create table,
 *  which is the shape a rollback can reverse from its authored source. */
function scaffold(schema: string): string {
  const dir = mkdtempSync(join(HERE, "rollback-live-"));
  writeFileSync(
    join(dir, "20260101000000_create_rollback_notes.ts"),
    `import { table, t } from "zero-migrate";
export const name = "create_rollback_notes";
export function up() {
  table(${JSON.stringify(TABLE)}).create({ columns: { id: t.int(), title: t.string() } });
}
`,
  );
  writeFileSync(join(dir, "policy.toml"), noInjectPolicy(schema));
  return dir;
}

function baseArgs(schema: string, url: string): string[] {
  return [
    "--dir=.",
    `--database-url=${url}`,
    `--schema=${schema}`,
    "--policy=policy.toml",
  ];
}

/** Run the whole scenario against one live dialect. The catalog probe differs per
 *  dialect; nothing else does, which is the point of running both. */
async function rollbackRoundTrip(
  url: string,
  schema: string,
  tableExists: () => Promise<boolean>,
  rolledBackEvents: () => Promise<number>,
): Promise<void> {
  const dir = scaffold(schema);
  try {
    const applied = spawnCli([...baseArgs(schema, url), "apply", "--approve"], dir);
    assert.equal(applied.status, 0, `apply failed: ${applied.stderr}`);
    assert.ok(await tableExists(), "the apply must create the table this test unwinds");

    // Both refusals come before anything runs, so the table is still standing after.
    const noTarget = spawnCli([...baseArgs(schema, url), "rollback", "--approve"], dir);
    assert.equal(noTarget.status, 1);
    assert.match(noTarget.stderr, /rollback needs a target/);
    const noApprove = spawnCli([...baseArgs(schema, url), "rollback", "--all"], dir);
    assert.equal(noApprove.status, 1);
    assert.match(noApprove.stderr, /needs --approve/);
    assert.ok(await tableExists(), "a refused rollback leaves the schema untouched");

    const rolledBack = spawnCli(
      [...baseArgs(schema, url), "rollback", "--all", "--approve"],
      dir,
    );
    assert.equal(rolledBack.status, 0, `rollback failed: ${rolledBack.stderr}`);
    assert.match(rolledBack.stdout, /rollback: 1 rolled back/);
    assert.equal(await tableExists(), false, "the reversed table is gone from the catalog");
    assert.equal(
      await rolledBackEvents(),
      1,
      "the journal records the unwind, so the version returns to pending",
    );

    // Nothing is left applied, so asking again is a no-op and not a failure: the
    // planner selects an empty set rather than refusing a version it cannot find.
    const again = spawnCli([...baseArgs(schema, url), "rollback", "--all", "--approve"], dir);
    assert.equal(again.status, 0, `second rollback failed: ${again.stderr}`);
    assert.match(again.stdout, /nothing was applied in the requested range/);

    // The migration is pending again, so the same file re-applies and the table
    // comes back. This is what distinguishes a rollback from a delete.
    const reapplied = spawnCli([...baseArgs(schema, url), "apply", "--approve"], dir);
    assert.equal(reapplied.status, 0, `re-apply failed: ${reapplied.stderr}`);
    assert.ok(await tableExists(), "a rolled-back migration is pending and re-applies");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

test("PostgreSQL: the CLI rolls an applied migration back and leaves it pending", async (t) => {
  const client = await connectLivePg(t);
  if (!client) return;

  const schema = uniqueSchema("rb_live_pg");
  const metaSchema = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    await rollbackRoundTrip(
      pgUrl(),
      schema,
      async () => {
        const result = await client.query(
          `SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2`,
          [schema, TABLE],
        );
        return result.rows.length > 0;
      },
      async () => {
        const result = await client.query(
          `SELECT count(*)::int AS n FROM "${metaSchema}".schema_migrations
            WHERE event_kind = 'rolled_back'`,
        );
        return Number((result.rows[0] as { n: number }).n);
      },
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE; DROP SCHEMA IF EXISTS "${metaSchema}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

/** Two migrations, where the second drops what the first created. Rolling back only the
 *  drop has to re-create the sequence from the definition the EARLIER migration authored,
 *  which is the only place that definition exists once the drop has run - the catalog no
 *  longer has it, and the dropped migration's own text says nothing about how to rebuild it. */
function scaffoldSequenceDrop(schema: string): string {
  const dir = mkdtempSync(join(HERE, "rollback-live-seq-"));
  writeFileSync(
    join(dir, "20260101000000_create_counter.ts"),
    `import { sequence } from "zero-migrate";
export const name = "create_counter";
export function up() {
  sequence(${JSON.stringify(SEQUENCE)}).create({});
}
`,
  );
  writeFileSync(
    join(dir, "20260101000001_drop_counter.ts"),
    `import { sequence } from "zero-migrate";
export const name = "drop_counter";
export function up() {
  sequence(${JSON.stringify(SEQUENCE)}).drop({});
}
`,
  );
  writeFileSync(join(dir, "policy.toml"), noInjectPolicy(schema));
  return dir;
}

test("PostgreSQL: rolling back a dropSequence rebuilds it from the migration that created it", async (t) => {
  const client = await connectLivePg(t);
  if (!client) return;

  const schema = uniqueSchema("rb_live_seq");
  const metaSchema = `${schema}_migrations`;
  const sequenceExists = async (): Promise<boolean> => {
    const result = await client.query(
      `SELECT 1 FROM pg_sequences WHERE schemaname = $1 AND sequencename = $2`,
      [schema, SEQUENCE],
    );
    return result.rows.length > 0;
  };

  let dir: string | undefined;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    dir = scaffoldSequenceDrop(schema);

    const applied = spawnCli([...baseArgs(schema, pgUrl()), "apply", "--approve"], dir);
    assert.equal(applied.status, 0, `apply failed: ${applied.stderr}`);
    assert.equal(
      await sequenceExists(),
      false,
      "both migrations ran, so the sequence is created and then dropped",
    );

    // Unwind ONLY the drop. The create stays applied, which is what makes the earlier
    // envelope part of the history the inverse is reconstructed from rather than
    // something the rollback is also undoing.
    const rolledBack = spawnCli(
      [...baseArgs(schema, pgUrl()), "rollback", "--steps", "1", "--approve"],
      dir,
    );
    assert.equal(rolledBack.status, 0, `rollback failed: ${rolledBack.stderr}`);
    assert.ok(
      await sequenceExists(),
      "the reversed dropSequence rebuilds the sequence the earlier migration authored",
    );
  } finally {
    if (dir) rmSync(dir, { recursive: true, force: true });
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE; DROP SCHEMA IF EXISTS "${metaSchema}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

/** The same two-migration shape as the sequence arm, over a VENDOR op. The drop carries
 *  no `cascade`: the engine refuses to invert a cascading drop, so a cascading author
 *  here would earn a correct `down: None` and fail this test for a reason that is not
 *  the thing it is testing. */
function scaffoldSchemaDrop(projectSchema: string, authored: string): string {
  const dir = mkdtempSync(join(HERE, "rollback-live-schema-"));
  writeFileSync(
    join(dir, "20260101000000_create_reporting.ts"),
    `import { schema } from "zero-migrate";
export const name = "create_reporting";
export function up() {
  schema(${JSON.stringify(authored)}).create({});
}
`,
  );
  writeFileSync(
    join(dir, "20260101000001_drop_reporting.ts"),
    `import { schema } from "zero-migrate";
export const name = "drop_reporting";
export function up() {
  schema(${JSON.stringify(authored)}).drop({});
}
`,
  );
  writeFileSync(join(dir, "policy.toml"), createSchemaPolicy(projectSchema, authored));
  return dir;
}

test("PostgreSQL: rolling back a dropSchema rebuilds the schema its create authored", async (t) => {
  const client = await connectLivePg(t);
  if (!client) return;

  const schemaName = uniqueSchema("rb_live_sch");
  const metaSchema = `${schemaName}_migrations`;
  const authored = `${schemaName}_reporting`;
  const authoredExists = async (): Promise<boolean> => {
    const result = await client.query(
      `SELECT 1 FROM information_schema.schemata WHERE schema_name = $1`,
      [authored],
    );
    return result.rows.length > 0;
  };

  let dir: string | undefined;
  try {
    await client.query(`CREATE SCHEMA "${schemaName}"`);
    dir = scaffoldSchemaDrop(schemaName, authored);

    const applied = spawnCli([...baseArgs(schemaName, pgUrl()), "apply", "--approve"], dir);
    assert.equal(applied.status, 0, `apply failed: ${applied.stderr}`);
    assert.equal(
      await authoredExists(),
      false,
      "both migrations ran, so the schema is created and then dropped",
    );

    const rolledBack = spawnCli(
      [...baseArgs(schemaName, pgUrl()), "rollback", "--steps", "1", "--approve"],
      dir,
    );
    assert.equal(rolledBack.status, 0, `rollback failed: ${rolledBack.stderr}`);
    assert.ok(
      await authoredExists(),
      "the reversed dropSchema rebuilds the schema the earlier migration authored",
    );
  } finally {
    if (dir) rmSync(dir, { recursive: true, force: true });
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${authored}" CASCADE; DROP SCHEMA IF EXISTS "${schemaName}" CASCADE; DROP SCHEMA IF EXISTS "${metaSchema}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

/** The same two migrations, except the CREATE is guarded. A guarded create can journal
 *  `completed` while its `IF NOT EXISTS` skipped the work, so its definition is not
 *  evidence that the object was ever made this way -- and rebuilding from it would
 *  invent a schema nobody created. */
function scaffoldGuardedSchemaDrop(projectSchema: string, authored: string): string {
  const dir = mkdtempSync(join(HERE, "rollback-live-guarded-"));
  writeFileSync(
    join(dir, "20260101000000_create_guarded.ts"),
    `import { schema } from "zero-migrate";
export const name = "create_guarded";
export function up() {
  schema(${JSON.stringify(authored)}).create({ ifNotExists: true });
}
`,
  );
  writeFileSync(
    join(dir, "20260101000001_drop_guarded.ts"),
    `import { schema } from "zero-migrate";
export const name = "drop_guarded";
export function up() {
  schema(${JSON.stringify(authored)}).drop({});
}
`,
  );
  writeFileSync(join(dir, "policy.toml"), createSchemaPolicy(projectSchema, authored));
  return dir;
}

test("PostgreSQL: a guarded create is not trusted to rebuild what a later drop removed", async (t) => {
  const client = await connectLivePg(t);
  if (!client) return;

  const schemaName = uniqueSchema("rb_live_grd");
  const metaSchema = `${schemaName}_migrations`;
  const authored = `${schemaName}_guarded`;
  const authoredExists = async (): Promise<boolean> => {
    const result = await client.query(
      `SELECT 1 FROM information_schema.schemata WHERE schema_name = $1`,
      [authored],
    );
    return result.rows.length > 0;
  };

  let dir: string | undefined;
  try {
    await client.query(`CREATE SCHEMA "${schemaName}"`);
    dir = scaffoldGuardedSchemaDrop(schemaName, authored);

    const applied = spawnCli([...baseArgs(schemaName, pgUrl()), "apply", "--approve"], dir);
    assert.equal(applied.status, 0, `apply failed: ${applied.stderr}`);
    assert.equal(await authoredExists(), false, "the guarded create ran and the drop removed it");

    const rolledBack = spawnCli(
      [...baseArgs(schemaName, pgUrl()), "rollback", "--steps", "1", "--approve"],
      dir,
    );
    assert.notEqual(
      rolledBack.status,
      0,
      "a drop whose create was guarded has no trustworthy definition to rebuild from",
    );
    // The refusal is the SAFE one: no inverse was synthesised at all, rather than one
    // synthesised from a definition that may describe an object the create never made.
    assert.match(rolledBack.stderr, /is irreversible \(down: None\); rollback refuses by default/);
    assert.equal(await authoredExists(), false, "the refusal leaves the catalog untouched");
  } finally {
    if (dir) rmSync(dir, { recursive: true, force: true });
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${authored}" CASCADE; DROP SCHEMA IF EXISTS "${schemaName}" CASCADE; DROP SCHEMA IF EXISTS "${metaSchema}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

test("PostgreSQL: a rollback whose project lock is held waits instead of failing", async (t) => {
  const client = await connectLivePg(t);
  if (!client) return;

  const schema = uniqueSchema("rb_lock_pg");
  const metaSchema = `${schema}_migrations`;
  const holder = await connectLivePg(t);
  if (!holder) {
    await client.end().catch(() => {});
    return;
  }

  let dir: string | undefined;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    dir = scaffold(schema);

    const applied = spawnCli([...baseArgs(schema, pgUrl()), "apply", "--approve"], dir);
    assert.equal(applied.status, 0, `apply failed: ${applied.stderr}`);

    // The engine's own acquire, run verbatim rather than re-derived: the key is
    // `hashtext(project_id)` (crates/zero-migrate/src/apply/backend/postgres/session.rs:60-74)
    // and the CLI passes the project SCHEMA as the project id
    // (packages/zero-migrate-cli/src/index.ts:544).
    await holder.query(`SELECT pg_advisory_lock(hashtext($1)::bigint)`, [schema]);

    // `pg_advisory_lock` has no timeout, so this rollback never returns. The deadline
    // belongs to the TEST, not the command: awaiting it would hang the suite rather than
    // fail it. Being killed mid-wait IS the observation - a CLI that had acquired the
    // lock would have finished in well under this budget, as the arms above do.
    const blocked = spawnCli(
      [...baseArgs(schema, pgUrl()), "rollback", "--all", "--approve"],
      dir,
      6000,
    );
    assert.equal(
      blocked.signal,
      "SIGTERM",
      `the rollback should still have been waiting when the deadline hit; it exited ` +
        `with status ${blocked.status} instead: ${blocked.stderr}`,
    );

    // Releasing lets the same command through, which is what proves the wait was the
    // lock rather than anything else about this scenario.
    await holder.query(`SELECT pg_advisory_unlock(hashtext($1)::bigint)`, [schema]);
    const afterRelease = spawnCli(
      [...baseArgs(schema, pgUrl()), "rollback", "--all", "--approve"],
      dir,
      30000,
    );
    assert.equal(afterRelease.status, 0, `rollback after release failed: ${afterRelease.stderr}`);
  } finally {
    if (dir) rmSync(dir, { recursive: true, force: true });
    await holder.query(`SELECT pg_advisory_unlock_all()`).catch(() => {});
    await holder.end().catch(() => {});
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE; DROP SCHEMA IF EXISTS "${metaSchema}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

test("MySQL: a rollback whose project lock is held fails with the holder named", async (t) => {
  if (!MYSQL_URL) {
    t.skip("ZERO_MIGRATE_MYSQL_URL unset - live-MySQL lock exclusion skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const schema = uniqueSchema("rb_lock_my");
  const metaSchema = `${schema}_migrations`;
  // Mirrors `project_lock_name` (crates/zero-migrate/src/apply/backend/mysql/session.rs:109):
  // `zero_migrate:<project_id>` while that fits in 64 chars, and the CLI passes the project
  // SCHEMA as the project id (packages/zero-migrate-cli/src/index.ts:544).
  //
  // A hand-mirrored derivation cannot go quietly wrong here: if this name stopped matching the
  // engine's, the rollback below would ACQUIRE the lock and succeed, and the refusal assertion
  // would fail loudly. The mirror is checked by the thing it is used for.
  const lockName = `zero_migrate:${schema}`;

  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const holder = await mysql.createConnection({ uri: MYSQL_URL });
  let dir: string | undefined;
  try {
    await admin.query(`CREATE DATABASE \`${schema}\``);
    dir = scaffold(schema);

    const applied = spawnCli([...baseArgs(schema, MYSQL_URL), "apply", "--approve"], dir);
    assert.equal(applied.status, 0, `apply failed: ${applied.stderr}`);

    const [gotRows] = await holder.query(`SELECT GET_LOCK(?, 0) AS got`, [lockName]);
    assert.equal(
      Number((gotRows as { got: number | string }[])[0].got),
      1,
      "the test must actually hold the project lock, or it proves nothing",
    );

    // MySQL's acquire is BOUNDED - GET_LOCK(name, PROJECT_LOCK_TIMEOUT_SECS) with a 10s
    // constant - so this returns a real error rather than hanging. The PostgreSQL acquire is
    // `pg_advisory_lock`, unbounded, which is why there is no PostgreSQL twin of this arm: it
    // would wait forever instead of failing.
    const blocked = spawnCli(
      [...baseArgs(schema, MYSQL_URL), "rollback", "--all", "--approve"],
      dir,
    );
    assert.notEqual(blocked.status, 0, "a rollback cannot proceed while a peer holds the lock");
    // Matched in two pieces on purpose: the engine joins them with an em dash, and the source
    // of this file stays ASCII. The lock NAME is asserted too, which is what proves the mirror
    // above resolved to the same lock the engine took.
    assert.match(
      blocked.stderr,
      new RegExp(`failed to acquire project lock: .*GET_LOCK\\('${lockName}'\\) timed out after 10s`),
    );
    assert.match(blocked.stderr, /another apply holds the project lock/);
  } finally {
    if (dir) rmSync(dir, { recursive: true, force: true });
    await holder.query(`SELECT RELEASE_LOCK(?)`, [lockName]).catch(() => {});
    await holder.end().catch(() => {});
    await admin.query(`DROP DATABASE IF EXISTS \`${schema}\``).catch(() => {});
    await admin.query(`DROP DATABASE IF EXISTS \`${metaSchema}\``).catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("MySQL: the CLI rolls an applied migration back and leaves it pending", async (t) => {
  if (!MYSQL_URL) {
    t.skip("ZERO_MIGRATE_MYSQL_URL unset - live-MySQL rollback skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const schema = uniqueSchema("rb_live_my");
  const metaSchema = `${schema}_migrations`;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  try {
    await admin.query(`CREATE DATABASE \`${schema}\``);
    await rollbackRoundTrip(
      MYSQL_URL,
      schema,
      async () => {
        const [rows] = await admin.query(
          `SELECT 1 FROM information_schema.tables WHERE table_schema = ? AND table_name = ?`,
          [schema, TABLE],
        );
        return (rows as unknown[]).length > 0;
      },
      async () => {
        const [rows] = await admin.query(
          `SELECT count(*) AS n FROM \`${metaSchema}\`.schema_migrations
            WHERE event_kind = 'rolled_back'`,
        );
        return Number((rows as { n: number | string }[])[0].n);
      },
    );
  } finally {
    await admin.query(`DROP DATABASE IF EXISTS \`${schema}\``).catch(() => {});
    await admin.query(`DROP DATABASE IF EXISTS \`${metaSchema}\``).catch(() => {});
    await admin.end().catch(() => {});
  }
});
