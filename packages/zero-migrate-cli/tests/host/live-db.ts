// The live-database requirement the host suites share.
//
// A gated suite used to have three outcomes, and a skip and a pass print the same
// exit code. That is the whole problem: a run with no database reported exactly
// like a run with one, so a machine that never started Docker and a machine that
// exercised every verb produced the same green summary. An opt-in environment
// variable existed to turn the skip into a failure, but opt-IN meant the DEFAULT
// was a suite that passed while testing nothing.
//
// There are two outcomes now:
//
//   - the DSN is set and connects -> run against it;
//   - anything else               -> FAIL, carrying the reason. An unset DSN, a
//     wrong password, a missing database and a driver regression are all "this run
//     has no live coverage", and none of them may report green.
//
// There is also no compose-DSN fallback. A default that happens to answer on one
// machine is not evidence that a run was configured for live coverage, and a
// fallback is just a gate that decides silently. `crates/zeroship-migrate/tests/support/
// mod.rs` holds the same requirement for the Rust side.

import type { Client } from "pg";
import { MYSQL_URL_ENV, PG_URL_ENV, requireLiveDb } from "./live-db.js";

/** The DSN of the PostgreSQL the gated suites run against. Required. */
export const PG_URL_ENV = "ZERO_MIGRATE_TEST_PG_URL";

/** The DSN of the MySQL the gated suites run against. Required. */
export const MYSQL_URL_ENV = "ZERO_MIGRATE_MYSQL_URL";

/** What the requirement decided, and the text explaining it. */
export type LiveDbGate = { action: "run"; dsn: string } | { action: "fail"; reason: string };

/**
 * The failure text for a live-database variable that is unset or blank.
 *
 * Names the variable AND the server, so an operator reading it knows which service
 * to start as well as which DSN to export.
 */
export function missingLiveDbDsn(envVar: string, server: string): string {
  return (
    `${envVar} is unset, so this test has no live ${server} to run against and cannot ` +
    `report coverage it never gathered. Start a ${server} and export ${envVar} with its ` +
    `DSN (see CONTRIBUTING.md, "Live-database tests").`
  );
}

/**
 * Require `dsn` to be a real DSN, throwing [`missingLiveDbDsn`] when it is not.
 *
 * An assertion signature rather than a `string` return, so the ~150 call sites that
 * already hold the value in a module-level `const` keep their narrowing without
 * rebinding it.
 */
export function requireLiveDb(
  dsn: string | undefined,
  envVar: string,
  server: string,
): asserts dsn is string {
  if (dsn === undefined || dsn.trim() === "") {
    throw new Error(missingLiveDbDsn(envVar, server));
  }
}

/** The configured DSN, or undefined when unset or blank (a blank export is unset). */
export function pgUrlFromEnv(): string | undefined {
  const raw = process.env[PG_URL_ENV];
  return raw === undefined || raw.trim() === "" ? undefined : raw;
}

/** The DSN the gated suites use. Throws when it is not configured. */
export function pgUrl(): string {
  const dsn = pgUrlFromEnv();
  requireLiveDb(dsn, PG_URL_ENV, "PostgreSQL");
  return dsn;
}

/**
 * Decide run / fail from the two facts the requirement turns on, with no I/O so
 * every case is testable on a machine that has a test PostgreSQL running.
 *
 * @param envDsn the configured DSN, or undefined/blank when unset.
 * @param connectError the driver's message when the connect failed, else undefined.
 */
export function liveDbGate(input: {
  envDsn: string | undefined;
  connectError: string | undefined;
}): LiveDbGate {
  const configured =
    input.envDsn !== undefined && input.envDsn.trim() !== "" ? input.envDsn : undefined;

  if (configured === undefined) {
    return { action: "fail", reason: missingLiveDbDsn(PG_URL_ENV, "PostgreSQL") };
  }

  if (input.connectError !== undefined) {
    return {
      action: "fail",
      reason: `${PG_URL_ENV} is set to ${configured} but connecting to it failed: ${input.connectError}`,
    };
  }

  return { action: "run", dsn: configured };
}

/**
 * Connect the gated suites' PostgreSQL client, or throw.
 *
 * Returns a connected `pg.Client` the caller owns (and must `end()`). It never
 * returns null, because the outcome null used to stand for - "no database here, so
 * report a pass" - is the one this file exists to remove.
 *
 * The connect is attempted before the unconfigured check so one pure `liveDbGate`
 * call decides both cases; the client is closed again on a failure.
 */
export async function connectLivePg(): Promise<Client> {
  const envDsn = pgUrlFromEnv();
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: envDsn ?? "" });

  let connectError: string | undefined;
  try {
    await client.connect();
  } catch (e) {
    connectError = (e as Error).message;
  }

  const gate = liveDbGate({ envDsn, connectError });
  if (gate.action === "run") return client;

  await client.end().catch(() => {});
  throw new Error(gate.reason);
}
