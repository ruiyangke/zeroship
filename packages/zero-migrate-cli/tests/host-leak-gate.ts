// The host suite must leave the servers exactly as it found them.
//
// WHY THIS IS A WRAPPER AND NOT A TEST FILE. Every host test already ends in a
// `finally` that drops what it created, and reading those teardowns is how this
// defect survived: 118 of 119 files that create a namespace also drop the
// `<name>_migrations` meta namespace the engine creates itself on the first
// `ensure_journal`, and `mysql-composite-and-nonid-fk.test.ts` dropped only the
// project database. It looked correct. The live MySQL container it ran against
// had accumulated 218 `fkmy_*_migrations` databases from that one line.
//
// A teardown that merely looks correct is what shipped that, so the instrument
// here is a COUNT, not a reading: snapshot the servers, run the whole suite, and
// snapshot again. Nothing about the shape of any teardown is consulted. That
// question cannot be asked from inside `node --test`, because no test file can
// observe the state before the first file ran and after the last one finished -
// files run concurrently and in unspecified order. Hence a wrapper that owns the
// run.
//
// HOW THE COUNT STAYS HONEST WITH OTHER WORK ON THE SAME SERVER. These are shared
// development servers; the Rust suite and other agents create and drop namespaces
// while this runs, so a naive global before/after count is racy in both directions.
// The gate scopes the failure to names THIS RUN can be shown to own:
//
//   - a name is new (present after, absent before), AND
//   - some underscore-delimited segment of it is EXACTLY 8 characters of
//     `[0-9a-z]` that reads as base36 into a millisecond timestamp inside this
//     run's window.
//
// The second clause is not a guess about naming. Every namespace generator in the
// host suite - `uniqueNamespace`, `uniqueSchema`, `uniqueDatabase`, `uniqueName`
// and the handful of inline ones - builds its name around
// `Date.now().toString(36)`, which is 8 characters from 2004 until 2059. The
// foreign shapes on these servers are the Rust suite's `proj_<pid>_<nanos>_<n>` /
// `meta_<pid>_<nanos>_<n>` and `zm_<pid>_<counter>_<suffix>`, whose segments are
// decimal. An 8-character decimal segment cannot alias: its largest base36 value
// is `99999999` = 7.1e11 ms, which is 1992, and the smallest 9-character one that
// starts with a nonzero digit is 2.8e12 ms, which is 2059. So the window test is
// provably outside the range of a decimal segment of that length, rather than
// merely unlikely to collide with one.
//
// What that scoping deliberately does NOT cover is stated rather than hidden: a
// leaked name carrying no timestamp, and a second host suite run by someone else
// inside this run's window. Both are printed as UNATTRIBUTED below - loudly, and
// without failing, because failing on another job's namespace would make this gate
// the flaky thing it exists to prevent.
//
// GATE: the same two variables the suite itself uses. `ZERO_MIGRATE_TEST_PG_URL`
// (or the `docker-compose.test.yml` default) for PostgreSQL, `ZERO_MIGRATE_MYSQL_URL`
// for MySQL. A server the suite will not touch is a server this gate does not
// snapshot: with `ZERO_MIGRATE_MYSQL_URL` unset every MySQL test skips, so there is
// nothing to leak there and nothing to count.

import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { liveDbGate, liveDbRequired, pgUrl, pgUrlFromEnv } from "./host/live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const PKG = resolve(HERE, "..");

/** Namespaces PostgreSQL owns; the suite never creates one of these. */
const PG_SYSTEM = /^(pg_|information_schema$|public$)/;

/** Databases MySQL owns; the suite never creates one of these. */
const MYSQL_SYSTEM = /^(information_schema|performance_schema|mysql|sys)$/;

/**
 * The millisecond slack around the run window.
 *
 * A name is minted a moment before its namespace exists and the snapshots bracket
 * the child process, so the window is already generous; this only absorbs clock
 * skew between this process and a namespace minted by a CLI subprocess.
 */
const WINDOW_SLACK_MS = 60_000;

/**
 * Whether `name` carries a `Date.now().toString(36)` stamp inside `[from, to]`.
 *
 * Exactly 8 characters, because that is what `Date.now().toString(36)` produces
 * for every millisecond between 2004 and 2059, and because widening it to 7 or 9
 * is what would let a decimal pid or nanosecond segment alias into the window.
 */
function mintedInWindow(name: string, from: number, to: number): boolean {
  return name.split("_").some((segment) => {
    if (!/^[0-9a-z]{8}$/.test(segment)) return false;
    const ms = Number.parseInt(segment, 36);
    return Number.isFinite(ms) && ms >= from && ms <= to;
  });
}

/** The non-system namespaces a server currently carries. */
async function pgNamespaces(dsn: string): Promise<string[]> {
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: dsn });
  await client.connect();
  try {
    const { rows } = await client.query<{ nspname: string }>(
      "SELECT nspname FROM pg_namespace ORDER BY 1",
    );
    return rows.map((row) => row.nspname).filter((n) => !PG_SYSTEM.test(n));
  } finally {
    await client.end().catch(() => {});
  }
}

async function mysqlDatabases(dsn: string): Promise<string[]> {
  const mysql = (await import("mysql2/promise")).default;
  const connection = await mysql.createConnection({ uri: dsn });
  try {
    const [rows] = await connection.query(
      "SELECT schema_name AS s FROM information_schema.schemata ORDER BY 1",
    );
    return (rows as unknown as Array<{ s: string }>)
      .map((row) => row.s)
      .filter((n) => !MYSQL_SYSTEM.test(n));
  } finally {
    await connection.end().catch(() => {});
  }
}

/** The exact command `package.json` gives `test:host`, so the two cannot drift. */
function hostSuiteCommand(): string {
  const manifest = JSON.parse(readFileSync(join(PKG, "package.json"), "utf8")) as {
    scripts?: Record<string, string>;
  };
  const command = manifest.scripts?.["test:host"];
  if (command === undefined) {
    throw new Error("package.json has no `test:host` script for the leak gate to wrap");
  }
  return command;
}

function runHostSuite(command: string): Promise<number> {
  return new Promise((done) => {
    const child = spawn(command, { cwd: PKG, shell: true, stdio: "inherit" });
    child.on("close", (code) => done(code ?? 1));
  });
}

/**
 * Resolve the PostgreSQL DSN to snapshot, or null when this machine has no
 * PostgreSQL and the suite will skip its PostgreSQL coverage anyway.
 *
 * Routed through the SAME `liveDbGate` the suite uses so a configured-but-broken
 * DSN fails here exactly as it fails there, instead of quietly becoming "no
 * PostgreSQL to count".
 */
async function pgTarget(): Promise<string | null> {
  const envDsn = pgUrlFromEnv();
  const dsn = pgUrl();
  let connectError: string | undefined;
  try {
    await pgNamespaces(dsn);
  } catch (e) {
    connectError = (e as Error).message;
  }
  const gate = liveDbGate({ envDsn, required: liveDbRequired(), connectError });
  if (gate.action === "run") return dsn;
  if (gate.action === "fail") throw new Error(gate.reason);
  console.error(`leak gate: ${gate.reason}`);
  return null;
}

function report(server: string, before: string[], after: string[], from: number, to: number) {
  const fresh = after.filter((name) => !before.includes(name));
  const owned = fresh.filter((name) => mintedInWindow(name, from, to));
  const unattributed = fresh.filter((name) => !owned.includes(name));

  console.error(
    `leak gate: ${server}: ${before.length} namespaces before, ${after.length} after, ` +
      `${owned.length} left behind by this run`,
  );
  for (const name of unattributed) {
    console.error(`leak gate: ${server}: UNATTRIBUTED new namespace (not failed): ${name}`);
  }
  for (const name of owned) {
    console.error(`leak gate: ${server}: LEAKED by this run: ${name}`);
  }
  return owned;
}

const pgDsn = await pgTarget();
const mysqlDsn = process.env.ZERO_MIGRATE_MYSQL_URL?.trim() || null;
if (mysqlDsn === null) {
  console.error(
    "leak gate: ZERO_MIGRATE_MYSQL_URL is unset, so every MySQL test skips and there is " +
      "no MySQL state to count",
  );
}

const pgBefore = pgDsn === null ? [] : await pgNamespaces(pgDsn);
const mysqlBefore = mysqlDsn === null ? [] : await mysqlDatabases(mysqlDsn);

const from = Date.now() - WINDOW_SLACK_MS;
const suiteExit = await runHostSuite(hostSuiteCommand());
const to = Date.now() + WINDOW_SLACK_MS;

const pgAfter = pgDsn === null ? [] : await pgNamespaces(pgDsn);
const mysqlAfter = mysqlDsn === null ? [] : await mysqlDatabases(mysqlDsn);

const leaked = [
  ...(pgDsn === null ? [] : report("postgresql", pgBefore, pgAfter, from, to)),
  ...(mysqlDsn === null ? [] : report("mysql", mysqlBefore, mysqlAfter, from, to)),
];

// The suite's own verdict comes first: a red suite that also leaked is a red suite,
// and reporting the leak as the failure would bury the tests that actually failed.
if (suiteExit !== 0) {
  console.error(`leak gate: the host suite exited ${suiteExit}; leak verdict is advisory above`);
  process.exit(suiteExit);
}
if (leaked.length > 0) {
  console.error(
    `leak gate: FAILED - the host suite left ${leaked.length} namespace(s) on the servers it ran ` +
      `against. Every namespace a test creates must be dropped together with its ` +
      `"<name>_migrations" meta namespace, which the engine creates on the first ensure_journal.`,
  );
  process.exit(1);
}
console.error("leak gate: the host suite left the servers as it found them");
