// The live-database gate the Postgres host suites share.
//
// A gated suite has three outcomes, and a skip and a pass print the same exit code,
// so the gate has to keep them apart:
//
//   - `ZERO_MIGRATE_TEST_PG_URL` is set and connects -> run against it;
//   - `ZERO_MIGRATE_TEST_PG_URL` is set and does NOT connect -> FAIL, carrying the
//     driver's own message. A wrong password, a missing database and a driver
//     regression are all "a database was configured and it did not work", which is
//     a different thing from "this machine has no database". Swallowing the connect
//     error into a skip makes a `pg` regression report exactly like a contributor
//     who never started Docker;
//   - `ZERO_MIGRATE_TEST_PG_URL` is unset -> fall back to the DSN
//     `docker-compose.test.yml` serves, so the documented local setup gets real
//     coverage without exporting anything, and skip (naming the coverage lost and
//     how to get it back) when nothing answers there.
//
// `ZERO_MIGRATE_REQUIRE_LIVE_DB` turns that last skip into a failure, for runs that
// are supposed to have a database: it demands an EXPLICIT DSN, because a default
// that happens to answer on one machine is not evidence that a run was configured
// for live coverage. `crates/zero-migrate/tests/support/mod.rs` gates the Rust
// suites on the same two variables with the same asymmetry.

import type { TestContext } from "node:test";
import type { Client } from "pg";

/** The DSN of the Postgres the gated suites run against. */
export const PG_URL_ENV = "ZERO_MIGRATE_TEST_PG_URL";

/** Turns a missing-DSN skip into a failure, for runs expected to have a database. */
export const REQUIRE_LIVE_DB_ENV = "ZERO_MIGRATE_REQUIRE_LIVE_DB";

/**
 * The DSN `docker-compose.test.yml` serves (`postgres` service: host port 5434,
 * `POSTGRES_PASSWORD: postgres`). It is the fallback so `docker compose -f
 * docker-compose.test.yml up -d` plus `pnpm test:host` is a complete local setup;
 * a fallback pointing anywhere else silently downgrades every gated suite to a skip
 * on a machine that has the database running.
 */
export const DEFAULT_PG_URL = "postgres://postgres:postgres@127.0.0.1:5434/zero_migrate_test";

/** What the gate decided, and the text explaining it. */
export type LiveDbGate =
  | { action: "run"; dsn: string }
  | { action: "skip"; reason: string }
  | { action: "fail"; reason: string };

/** The configured DSN, or undefined when unset or blank (a blank export is unset). */
export function pgUrlFromEnv(): string | undefined {
  const raw = process.env[PG_URL_ENV];
  return raw === undefined || raw.trim() === "" ? undefined : raw;
}

/** The DSN the gated suites use: the configured one, else the compose default. */
export function pgUrl(): string {
  return pgUrlFromEnv() ?? DEFAULT_PG_URL;
}

/**
 * Whether this run declared that it must have a live database.
 *
 * Anything but unset, empty, `0`, `false` or `no` counts as a demand, matching the
 * Rust `live_db_required`.
 */
export function liveDbRequired(): boolean {
  const raw = process.env[REQUIRE_LIVE_DB_ENV];
  if (raw === undefined) return false;
  return !["", "0", "false", "no"].includes(raw.trim().toLowerCase());
}

/**
 * Decide run / skip / fail from the three facts the gate turns on, with no I/O so
 * every quadrant is testable on a machine that has a test Postgres running.
 *
 * @param envDsn the configured DSN, or undefined/blank when unset.
 * @param required whether `REQUIRE_LIVE_DB_ENV` demands live coverage.
 * @param connectError the driver's message when the connect failed, else undefined.
 */
export function liveDbGate(input: {
  envDsn: string | undefined;
  required: boolean;
  connectError: string | undefined;
}): LiveDbGate {
  const configured =
    input.envDsn !== undefined && input.envDsn.trim() !== "" ? input.envDsn : undefined;
  const dsn = configured ?? DEFAULT_PG_URL;

  if (input.required && configured === undefined) {
    return {
      action: "fail",
      reason:
        `${REQUIRE_LIVE_DB_ENV} demands a live database but ${PG_URL_ENV} is unset, so this ` +
        `run has no live PostgreSQL coverage to offer; export ${PG_URL_ENV} or clear ` +
        `${REQUIRE_LIVE_DB_ENV}`,
    };
  }

  if (input.connectError === undefined) return { action: "run", dsn };

  if (configured !== undefined) {
    return {
      action: "fail",
      reason: `${PG_URL_ENV} is set to ${dsn} but connecting to it failed: ${input.connectError}`,
    };
  }

  return {
    action: "skip",
    reason:
      `live PostgreSQL coverage skipped: ${PG_URL_ENV} is unset and the default ${dsn} ` +
      `(the DSN docker-compose.test.yml serves) is unreachable: ${input.connectError}. Start it ` +
      `with "docker compose -f docker-compose.test.yml up -d", export ${PG_URL_ENV}, or set ` +
      `${REQUIRE_LIVE_DB_ENV}=1 to turn this skip into a failure`,
  };
}

/**
 * Connect the gated suites' Postgres client, or skip the calling test.
 *
 * Returns a connected `pg.Client` the caller owns (and must `end()`), or null after
 * marking the test skipped. Throws on the two cases a skip would hide: a configured
 * DSN that does not connect, and a demanded live database with no DSN configured.
 *
 * The connect is attempted before the demanded-but-unconfigured check so one pure
 * `liveDbGate` call decides every case; the client is closed again on any non-run
 * outcome.
 */
export async function connectLivePg(t: TestContext): Promise<Client | null> {
  const envDsn = pgUrlFromEnv();
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: envDsn ?? DEFAULT_PG_URL });

  let connectError: string | undefined;
  try {
    await client.connect();
  } catch (e) {
    connectError = (e as Error).message;
  }

  const gate = liveDbGate({ envDsn, required: liveDbRequired(), connectError });
  if (gate.action === "run") return client;

  await client.end().catch(() => {});
  if (gate.action === "fail") throw new Error(gate.reason);
  t.skip(gate.reason);
  return null;
}
