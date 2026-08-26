// The live-database contract for the host suites: the gate must FAIL CLOSED.
//
// A gated suite has two outcomes and they must be distinguishable from the outside,
// because a skip and a pass print the same exit code:
//   - a DSN is configured and it works    -> the suite runs;
//   - anything else                       -> the suite FAILS, carrying the reason.
//
// "Anything else" is the whole point of this file. It used to be three outcomes, and
// the third one - no DSN configured, so skip - is the defect: a machine that never
// started a database reported the identical green summary as a machine that
// exercised every verb. An opt-IN environment variable could turn that skip into a
// failure, but a safety mechanism that must be asked for protects only the runs that
// already remembered to ask. There is no such variable, and no skip for it to
// control.
//
// The arms below drive a real gated suite in a child process and read its exit code,
// because the exit code is the only thing CI reads. `crates/zero-migrate/tests/
// support/mod.rs` holds the same contract for the Rust side.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { PG_URL_ENV, liveDbGate, pgUrl, pgUrlFromEnv, requireLiveDb } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const PACKAGE_ROOT = join(HERE, "../..");

// One gated suite stands in for all of them: every PostgreSQL host suite reaches the
// live database through the same shared gate, so the contract holds at one call
// site exactly when it holds at all of them.
const GATED_SUITE = "tests/host/e2e-pg.test.ts";

type SuiteRun = { status: number | null; output: string };

/**
 * Run `GATED_SUITE` in a child `node --test` with an explicit live-database
 * environment, and return its exit code plus its combined output.
 *
 * The child inherits this process's environment MINUS the DSN variable, so the arm
 * under test decides it and a developer's exported DSN cannot mask a failure.
 * `NODE_TEST_CONTEXT` goes too: node's test runner reads it as "you are already
 * inside a test run", warns about a recursive `run()` and skips every file, which
 * would report a green exit code for a suite that never executed.
 */
function runGatedSuite(gateEnv: Record<string, string>): SuiteRun {
  const env: NodeJS.ProcessEnv = { ...process.env, NODE_ENV: "test" };
  delete env.ZERO_MIGRATE_TEST_PG_URL;
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
// THE ARM THIS FILE EXISTS FOR. With no DSN configured there is nothing to prove
// coverage against, so the suite fails instead of reporting green. Before this
// became unconditional, this exact command exited 0 with every gated test marked
// SKIP - which is why "did the suite pass?" was not a question worth asking.
// ---------------------------------------------------------------------------
test("an unset DSN fails the suite, with no variable needed to ask for that", () => {
  const run = runGatedSuite({});

  // A killed child reports a null status and `notEqual(null, 0)` passes, so
  // "did not succeed" and "never ran" would collapse together without this.
  assert.equal(typeof run.status, "number", "the child must exit on its own, not be killed");
  assert.notEqual(run.status, 0, "an unset DSN must fail the suite");
  assert.match(
    run.output,
    /ZERO_MIGRATE_TEST_PG_URL/,
    "the failure must name the DSN variable the run has to export",
  );
  assert.match(
    run.output,
    /live PostgreSQL/,
    "the failure must name the server the operator has to start",
  );
  assert.doesNotMatch(
    run.output,
    /# SKIP/,
    "an unset DSN must never report as a skip, which is the whole contract",
  );
});

// ---------------------------------------------------------------------------
// A configured DSN that cannot connect is also a FAILURE, and the driver's own
// message travels with it. A bare `catch` that turned every connect error into a
// skip made a driver regression read exactly like a contributor without a database.
//
// The broken DSN is derived from the DSN this run actually uses, with only the
// password changed, rather than hardcoded. A hardcoded DSN names a host and port,
// and the port the local `docker-compose.test.yml` publishes is not the port CI
// publishes: on CI nothing answered there, the child died on ECONNREFUSED instead of
// on authentication, and this arm failed every run. Deriving the DSN points the child
// at whatever server this run has, so the only thing it can fail on is the password.
// ---------------------------------------------------------------------------
test("a configured DSN that cannot authenticate fails the suite and names the driver error", () => {
  const run = runGatedSuite({ ZERO_MIGRATE_TEST_PG_URL: withUnusablePassword(pgUrl()) });

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
// The decision itself, over every quadrant of (DSN configured?, connect ok?). The
// child-process arms above prove the decision reaches the exit code; these prove the
// decision and the text it carries. There is no "required" input to vary any more:
// the requirement is not a parameter, so it cannot be turned off.
// ---------------------------------------------------------------------------
const CONNECT_ERROR = 'password authentication failed for user "postgres"';
const ENV_DSN = "postgres://postgres:secret@db.example:5432/zero_migrate_test";

test("a configured DSN that connects runs against that DSN", () => {
  assert.deepEqual(liveDbGate({ envDsn: ENV_DSN, connectError: undefined }), {
    action: "run",
    dsn: ENV_DSN,
  });
});

test("no configured DSN fails rather than falling back to any default", () => {
  const gate = liveDbGate({ envDsn: undefined, connectError: undefined });

  assert.equal(gate.action, "fail", "an unset DSN is a failure even when a default would connect");
  assert.match(gate.reason, new RegExp(PG_URL_ENV), "the reason names the variable to export");
  assert.match(gate.reason, /PostgreSQL/, "the reason names the server to start");
});

test("a blank configured DSN counts as unset, and so fails too", () => {
  const gate = liveDbGate({ envDsn: "  ", connectError: undefined });

  assert.equal(gate.action, "fail", "a blank export must not read as a configured DSN");
  assert.match(gate.reason, new RegExp(PG_URL_ENV), "the reason names the variable to export");
});

test("a configured DSN that cannot connect fails, carrying the driver's own message", () => {
  const gate = liveDbGate({ envDsn: ENV_DSN, connectError: CONNECT_ERROR });

  assert.equal(gate.action, "fail", "a configured DSN that does not work is a failure, not a skip");
  assert.match(gate.reason, new RegExp(PG_URL_ENV), "the reason names the configured variable");
  assert.ok(gate.reason.includes(ENV_DSN), "the reason names the DSN that was tried");
  assert.ok(gate.reason.includes(CONNECT_ERROR), "the reason carries the driver error verbatim");
});

test("the gate has no outcome other than run and fail", () => {
  const actions = new Set(
    [
      liveDbGate({ envDsn: ENV_DSN, connectError: undefined }),
      liveDbGate({ envDsn: ENV_DSN, connectError: CONNECT_ERROR }),
      liveDbGate({ envDsn: undefined, connectError: undefined }),
      liveDbGate({ envDsn: undefined, connectError: CONNECT_ERROR }),
      liveDbGate({ envDsn: "   ", connectError: undefined }),
    ].map((gate) => gate.action),
  );

  assert.deepEqual([...actions].sort(), ["fail", "run"], "a skip outcome must not come back");
});

// ---------------------------------------------------------------------------
// `requireLiveDb` is the same requirement for the ~150 call sites that hold their DSN
// in a module-level const rather than going through `connectLivePg`. It is the arm
// that keeps the MySQL half honest: MySQL has no `liveDbGate` of its own.
// ---------------------------------------------------------------------------
test("requireLiveDb throws on an absent DSN and names the variable and the server", () => {
  for (const absent of [undefined, "", "   "]) {
    assert.throws(
      () => requireLiveDb(absent, "ZERO_MIGRATE_MYSQL_URL", "MySQL"),
      (error: Error) =>
        /ZERO_MIGRATE_MYSQL_URL/.test(error.message) && /MySQL/.test(error.message),
      `an absent DSN (${JSON.stringify(absent)}) must throw, naming the variable and server`,
    );
  }
});

test("requireLiveDb accepts a configured DSN", () => {
  assert.doesNotThrow(() => requireLiveDb(ENV_DSN, PG_URL_ENV, "PostgreSQL"));
});

test("pgUrl returns the configured DSN, which this run is required to have", () => {
  assert.equal(pgUrl(), pgUrlFromEnv(), "pgUrl must be the exported DSN, with no fallback");
});
