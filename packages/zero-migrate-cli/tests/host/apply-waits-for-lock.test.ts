// What a WRITER does when the project lock is already held.
//
// `cli.test.ts` covers the reader side thoroughly: `status` and `plan` try for
// the lock without waiting, report the holder, and read nothing. The writer side
// is the opposite promise, and `docs/cli.md` states it: "apply and squash still
// wait for the lock: they are the writers the lock exists to serialize, and a
// writer that gave up would leave the deploy undone."
//
// Nothing tested that. It matters more than the reader side does, because the
// failure modes are worse in both directions: a writer that gave up would leave
// a deploy half-done, and a writer that waited on the WRONG key would run
// concurrently with the deploy it was supposed to queue behind.
//
// THE LOCK IS HELD BY HAND rather than by a competing apply. A real apply holds
// it for a few hundred milliseconds - most of a CLI run is process startup and
// IR lowering, not database work - and a cold CLI takes about a second to reach
// the acquire. Racing that window would make this test flaky in the direction
// that matters least: it would pass when it missed. Taking the lock from a known
// session makes the wait deterministic, and lets the test assert that the key it
// took really is the one apply contends on.
//
// The key is `hashtext(project_id)`, and the project id is the schema. That is
// asserted rather than assumed - if it were wrong, the "apply waits" arm would
// pass for the wrong reason, since an apply that never contended would also
// still be running at the sample point if it were merely slow.
//
// WHAT IS PINNED, and it is a contract worth pinning in both halves:
//   - apply blocks while the lock is held, and emits NOTHING while blocking
//   - the wait is unbounded by default
//   - `--query-timeout` is the only bound, and names the lock in its error
//   - on release, the waiter completes and its migration really did apply
//
// The silence is recorded as today's behaviour, not endorsed: a deploy that
// hangs with an empty log is indistinguishable from one wedged on slow DDL. If
// apply ever learns to say it is queued - `status` already has the machinery, in
// `project_lock_holders` and `formatStatusBusy` - the stdout/stderr assertion is
// what will fail, and it should be replaced with one asserting that notice.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
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

/** How long to let a blocked apply sit before concluding it really is waiting. */
const WAIT_SAMPLE_MS = 2500;

const MIGRATION = `import { table, t } from "zero-migrate";
export const name = "create_one";
export default {
  up() {
    table("t1").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "applywait-"));
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
`,
  );
  writeFileSync(join(work, "migrations", "20260101000000_create_one.ts"), MIGRATION);
  return work;
}

interface Running {
  readonly child: ChildProcess;
  readonly state: { out: string; err: string; code: number | null; done: boolean };
  readonly exited: Promise<void>;
}

/** Start a verb and keep a live handle, so output can be read WHILE it runs. */
function start(work: string, schema: string, verb: string, extra: string[] = []): Running {
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, verb,
      "--dir", join(work, "migrations"),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
      "--schema", schema,
      "--owner-app", "app_apply_wait",
      ...extra,
    ],
    { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
  );
  const state = { out: "", err: "", code: null as number | null, done: false };
  child.stdout.on("data", (chunk) => (state.out += chunk));
  child.stderr.on("data", (chunk) => (state.err += chunk));
  const exited = new Promise<void>((resolvePromise) =>
    child.on("close", (code) => {
      state.code = code;
      state.done = true;
      resolvePromise();
    }),
  );
  return { child, state, exited };
}

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

/** Warnings are noise from the loader, not output the CLI chose to emit. */
const meaningful = (text: string): string => text.replace(/^WARNING.*$/gm, "").trim();

test("apply waits for a held project lock, silently, and completes when it is released", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("applywait");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  const holder = await connectLivePg(ctx);
  if (!holder) return;

  let running: Running | undefined;
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    // Bootstrap the journal, so the wait below is on the lock and not on setup.
    await start(work, schema, "status").exited;

    await holder.query(`SELECT pg_advisory_lock(hashtext($1)::bigint)`, [schema]);

    // The key really is hashtext(schema): without this, "apply is still running"
    // would also be satisfied by an apply that never contended at all.
    const { rows: seen } = await client.query(
      `SELECT count(*)::int AS n FROM pg_locks
        WHERE locktype = 'advisory' AND granted AND objsubid = 1
          AND ((classid::bigint << 32) | objid::bigint) = hashtext($1)::bigint`,
      [schema],
    );
    assert.equal(seen[0].n, 1, "the hand-taken lock must be the project lock apply will contend on");

    running = start(work, schema, "apply");
    await sleep(WAIT_SAMPLE_MS);

    assert.equal(running.state.done, false, "apply must still be waiting, not have given up");
    // TODAY: nothing at all is printed while blocked. See the header - if apply
    // learns to announce that it is queued, this is the assertion to replace.
    assert.equal(meaningful(running.state.out), "", "apply prints nothing on stdout while blocked");
    assert.equal(meaningful(running.state.err), "", "apply prints nothing on stderr while blocked");

    // Nothing may have been applied while it waited.
    const { rows: early } = await client.query(
      `SELECT count(*)::int AS n FROM information_schema.tables WHERE table_schema = $1`,
      [schema],
    );
    assert.equal(early[0].n, 0, "a waiting apply must not have touched the schema");

    await holder.query(`SELECT pg_advisory_unlock(hashtext($1)::bigint)`, [schema]);
    await running.exited;

    assert.equal(running.state.code, 0, `apply must succeed once released; ${running.state.err}`);
    const { rows: after } = await client.query(
      `SELECT count(*)::int AS n FROM information_schema.tables WHERE table_schema = $1`,
      [schema],
    );
    assert.equal(after[0].n, 1, "the waiter's own migration must really have been applied");
  } finally {
    if (running && !running.state.done) {
      running.child.kill("SIGKILL");
    }
    await holder.end().catch(() => {});
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("--query-timeout bounds the lock wait and names the lock in the error", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("applybound");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  const holder = await connectLivePg(ctx);
  if (!holder) return;

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    await start(work, schema, "status").exited;
    await holder.query(`SELECT pg_advisory_lock(hashtext($1)::bigint)`, [schema]);

    // The escape hatch an operator has when a deploy is queued behind another:
    // it must give up rather than hang, and say what it gave up on.
    const bounded = start(work, schema, "apply", ["--query-timeout", "1500"]);
    const outcome = await Promise.race([
      bounded.exited.then(() => "exited" as const),
      sleep(30000).then(() => "hung" as const),
    ]);
    if (outcome === "hung") {
      bounded.child.kill("SIGKILL");
      assert.fail("--query-timeout must bound the lock wait rather than hang");
    }

    assert.notEqual(bounded.state.code, 0, "a timed-out lock acquire must not report success");
    assert.match(
      meaningful(bounded.state.err),
      /project lock/,
      `the error must name the lock so the wait is diagnosable; got: ${bounded.state.err}`,
    );

    // And it gave up without doing any of the work.
    const { rows } = await client.query(
      `SELECT count(*)::int AS n FROM information_schema.tables WHERE table_schema = $1`,
      [schema],
    );
    assert.equal(rows[0].n, 0, "an apply that never got the lock must not have applied anything");
  } finally {
    await holder.end().catch(() => {});
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
