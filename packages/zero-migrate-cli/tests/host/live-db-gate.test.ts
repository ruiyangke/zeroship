// The live-database gate contract for the Postgres host suites.
//
// A gated suite has three outcomes and they must be distinguishable from the
// outside, because a skip and a pass print the same exit code:
//   - a DSN is configured and it works       -> the suite runs;
//   - a DSN is configured and it does NOT    -> the suite FAILS, carrying the
//     driver's own error text (a wrong password, a missing database and a driver
//     regression are all "configured but broken", not "no database here");
//   - no DSN is configured                   -> the suite skips, unless
//     `ZERO_MIGRATE_REQUIRE_LIVE_DB` says this run was expected to have one.
//
// The arms below drive a real gated suite in a child process and read its exit
// code, because the exit code is the only thing CI reads. `crates/zero-migrate/
// tests/support/mod.rs` holds the same contract for the Rust side.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  DEFAULT_PG_URL,
  PG_URL_ENV,
  REQUIRE_LIVE_DB_ENV,
  liveDbGate,
  liveDbRequired,
  pgUrlFromEnv,
} from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const PACKAGE_ROOT = join(HERE, "../..");

// One gated suite stands in for all of them: every Postgres host suite reaches the
// live database through the same shared gate, so the contract holds at one call
// site exactly when it holds at all of them.
const GATED_SUITE = "tests/host/e2e-pg.test.ts";

type SuiteRun = { status: number | null; output: string };

/**
 * Run `GATED_SUITE` in a child `node --test` with an explicit live-database
 * environment, and return its exit code plus its combined output.
 *
 * The child inherits this process's environment MINUS the two gate variables, so
 * the arm under test decides them and a developer's exported DSN cannot mask a
 * failure. `NODE_TEST_CONTEXT` goes too: node's test runner reads it as "you are
 * already inside a test run", warns about a recursive `run()` and skips every file,
 * which would report a green exit code for a suite that never executed.
 */
function runGatedSuite(gateEnv: Record<string, string>): SuiteRun {
  const env: NodeJS.ProcessEnv = { ...process.env, NODE_ENV: "test" };
  delete env.ZERO_MIGRATE_TEST_PG_URL;
  delete env.ZERO_MIGRATE_REQUIRE_LIVE_DB;
  delete env.NODE_TEST_CONTEXT;
  Object.assign(env, gateEnv);

  const child = spawnSync(process.execPath, ["--import", "tsx", "--test", GATED_SUITE], {
    cwd: PACKAGE_ROOT,
    env,
    encoding: "utf8",
    timeout: 120_000,
  });
  return { status: child.status, output: `${child.stdout ?? ""}${child.stderr ?? ""}` };
}

/**
 * Connect `dsn` with the `pg` client and return the driver's message on failure,
 * or undefined when it connects.
 *
 * Deliberately does NOT go through `connectLivePg` or `liveDbGate`. This file is the
 * independent auditor of that gate, and a gate that decided whether its own auditor
 * runs could silence the arm that would have caught it breaking.
 */
async function probeConnectError(dsn: string): Promise<string | undefined> {
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: dsn });
  try {
    await client.connect();
    return undefined;
  } catch (e) {
    return (e as Error).message;
  } finally {
    await client.end().catch(() => {});
  }
}

/**
 * `dsn` with its password replaced by one no server will accept, and everything else
 * (host, port, user, database, query parameters) left alone, so the child reaches the
 * same server this run reaches and fails there on authentication and nothing else.
 *
 * An empty username becomes `postgres`: pg would otherwise fall back to the OS user,
 * and the server answers "role does not exist" instead of an authentication error.
 */
function withUnusablePassword(dsn: string): string {
  let url: URL;
  try {
    url = new URL(dsn);
  } catch {
    throw new Error(
      `cannot derive a wrong-password DSN from ${dsn}: ${PG_URL_ENV} must be a URL DSN ` +
        `("postgres://user:password@host:port/database"), not libpq keyword form`,
    );
  }
  if (url.hostname === "" || url.hostname.startsWith("%2F") || url.hostname.startsWith("/")) {
    throw new Error(
      `cannot derive a wrong-password DSN from ${dsn}: ${PG_URL_ENV} must name a TCP host, ` +
        `and a unix socket has no password to get wrong`,
    );
  }
  if (url.username === "") url.username = "postgres";
  url.password = "definitely_wrong";
  return url.toString();
}

// ---------------------------------------------------------------------------
// A configured DSN that cannot connect is a FAILURE, and the driver's own message
// travels with it. A bare `catch` that turns every connect error into a skip makes
// a driver regression read exactly like a contributor without a database.
//
// The broken DSN is derived from the DSN this run actually uses, with only the
// password changed, rather than hardcoded. A hardcoded DSN names a host and port,
// and the port the local `docker-compose.test.yml` publishes is not the port CI
// publishes: on CI nothing answered there, the child died on ECONNREFUSED instead of
// on authentication, and this arm failed every run. Deriving the DSN points the child
// at whatever server this run has, so the only thing it can fail on is the password.
//
// Reaching that server is the arm's precondition, so it is probed first, and the probe
// decides the three outcomes the gate itself draws: a DSN was configured and does not
// answer, or one was demanded, and the arm fails; otherwise there is no server to
// authenticate against and the arm skips.
// ---------------------------------------------------------------------------
test("a configured DSN that cannot authenticate fails the suite and names the driver error", async (t) => {
  const envDsn = pgUrlFromEnv();
  const baseDsn = envDsn ?? DEFAULT_PG_URL;
  const probeError = await probeConnectError(baseDsn);

  if (probeError !== undefined) {
    assert.equal(
      envDsn,
      undefined,
      `${PG_URL_ENV} is set to ${baseDsn} but connecting to it failed: ${probeError}`,
    );
    assert.equal(
      liveDbRequired(),
      false,
      `${REQUIRE_LIVE_DB_ENV} demands a live database but the default ${baseDsn} is ` +
        `unreachable: ${probeError}`,
    );
    t.skip(
      `wrong-password coverage skipped: proving a configured-but-broken DSN fails the suite ` +
        `needs a reachable PostgreSQL to reject the password, and ${PG_URL_ENV} is unset while ` +
        `the default ${baseDsn} is unreachable: ${probeError}. Export ${PG_URL_ENV}, or set ` +
        `${REQUIRE_LIVE_DB_ENV}=1 to turn this skip into a failure`,
    );
    return;
  }

  const run = runGatedSuite({ ZERO_MIGRATE_TEST_PG_URL: withUnusablePassword(baseDsn) });

  // `spawnSync` reports a null status when the child was killed, including on the
  // timeout above, and `notEqual(null, 0)` passes. A child that never ran to a verdict
  // proves nothing about the gate, so the status has to be a real exit code.
  assert.equal(typeof run.status, "number", "the child must exit on its own, not be killed");
  assert.notEqual(run.status, 0, "a configured-but-broken DSN must fail the suite");
  assert.match(
    run.output,
    new RegExp(`${PG_URL_ENV} is set to .* but connecting to it failed`),
    "the failure must be the gate's configured-DSN verdict, not some other child error",
  );
  assert.match(
    run.output,
    /password authentication failed/,
    "the failure must carry the pg error text, not a generic message",
  );
  assert.doesNotMatch(
    run.output,
    /# SKIP/,
    "a configured-but-broken DSN must never report as a skip",
  );
});

// ---------------------------------------------------------------------------
// `ZERO_MIGRATE_REQUIRE_LIVE_DB` is how a run declares that it was SUPPOSED to have
// a database. With no DSN configured there is nothing to prove coverage against, so
// the run fails instead of reporting green.
// ---------------------------------------------------------------------------
test("an unset DSN under ZERO_MIGRATE_REQUIRE_LIVE_DB fails the suite", () => {
  const run = runGatedSuite({ ZERO_MIGRATE_REQUIRE_LIVE_DB: "1" });

  assert.notEqual(run.status, 0, "a required live database with no DSN must fail the suite");
  assert.match(
    run.output,
    /ZERO_MIGRATE_REQUIRE_LIVE_DB/,
    "the failure must name the variable that demanded a live database",
  );
  assert.match(
    run.output,
    /ZERO_MIGRATE_TEST_PG_URL/,
    "the failure must name the DSN variable the run has to export",
  );
});

// ---------------------------------------------------------------------------
// A contributor with no database and no demand for one still gets a green run: the
// gate skips rather than fails. This holds whether or not the default DSN happens
// to reach a local test Postgres, so the arm is stable on any machine.
// ---------------------------------------------------------------------------
test("an unset DSN with no live-database demand keeps the suite green", () => {
  const run = runGatedSuite({});

  assert.equal(run.status, 0, "an unset DSN must not fail a contributor without a database");
});

// ---------------------------------------------------------------------------
// The gate decision itself, over every quadrant of (DSN configured?, connect ok?,
// live database demanded?). The child-process arms above prove the decision reaches
// the exit code; these prove the decision, and the text it carries, for the cases a
// machine with a running test Postgres cannot stage.
// ---------------------------------------------------------------------------
const CONNECT_ERROR = 'password authentication failed for user "postgres"';
const ENV_DSN = "postgres://postgres:secret@db.example:5432/zero_migrate_test";

test("a configured DSN that connects runs against that DSN", () => {
  assert.deepEqual(liveDbGate({ envDsn: ENV_DSN, required: false, connectError: undefined }), {
    action: "run",
    dsn: ENV_DSN,
  });
});

test("no configured DSN falls back to the DSN docker-compose.test.yml serves", () => {
  assert.deepEqual(liveDbGate({ envDsn: undefined, required: false, connectError: undefined }), {
    action: "run",
    dsn: DEFAULT_PG_URL,
  });
});

test("a blank configured DSN counts as unset", () => {
  assert.deepEqual(liveDbGate({ envDsn: "  ", required: false, connectError: undefined }), {
    action: "run",
    dsn: DEFAULT_PG_URL,
  });
});

test("a configured DSN that cannot connect fails, carrying the driver's own message", () => {
  const gate = liveDbGate({ envDsn: ENV_DSN, required: false, connectError: CONNECT_ERROR });

  assert.equal(gate.action, "fail", "a configured DSN that does not work is a failure, not a skip");
  assert.match(gate.reason, new RegExp(PG_URL_ENV), "the reason names the configured variable");
  assert.ok(gate.reason.includes(ENV_DSN), "the reason names the DSN that was tried");
  assert.ok(gate.reason.includes(CONNECT_ERROR), "the reason carries the driver error verbatim");
});

test("an unconfigured DSN that cannot connect skips, naming what was skipped and why", () => {
  const gate = liveDbGate({ envDsn: undefined, required: false, connectError: CONNECT_ERROR });

  assert.equal(gate.action, "skip", "a contributor without a database keeps a green run");
  assert.match(gate.reason, /live PostgreSQL/i, "the reason names the coverage being skipped");
  assert.match(gate.reason, new RegExp(PG_URL_ENV), "the reason names the variable to export");
  assert.match(
    gate.reason,
    new RegExp(REQUIRE_LIVE_DB_ENV),
    "the reason names the variable that turns this skip into a failure",
  );
  assert.ok(gate.reason.includes(CONNECT_ERROR), "the reason carries the driver error verbatim");
});

test("a demanded live database with no configured DSN fails whether or not the default connects", () => {
  for (const connectError of [undefined, CONNECT_ERROR]) {
    const gate = liveDbGate({ envDsn: undefined, required: true, connectError });

    assert.equal(gate.action, "fail", `demanded live coverage must not skip (connect ${connectError})`);
    assert.match(
      gate.reason,
      new RegExp(REQUIRE_LIVE_DB_ENV),
      "the reason names the variable that demanded a live database",
    );
    assert.match(gate.reason, new RegExp(PG_URL_ENV), "the reason names the variable to export");
  }
});
