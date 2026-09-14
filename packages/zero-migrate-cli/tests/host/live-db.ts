// Database addresses supplied by the parent test runner.
//
// `tests/host/run.ts` owns the PostgreSQL and MySQL containers and passes their
// mapped addresses to Node's isolated test processes. These variables are an
// internal process boundary, not configuration a developer supplies.

import type { Client } from "pg";

/** The DSN of the PostgreSQL container owned by this test run. */
export const PG_URL_ENV = "ZERO_MIGRATE_TEST_PG_URL";

/** The DSN of the MySQL container owned by this test run. */
export const MYSQL_URL_ENV = "ZERO_MIGRATE_MYSQL_URL";

/**
 * Require the runner-owned address to be present in this test process.
 *
 * The assertion signature preserves narrowing at existing call sites.
 */
export function requireLiveDb(
  dsn: string | undefined,
  envVar: string,
  server: string,
): asserts dsn is string {
  if (dsn === undefined || dsn.trim() === "") {
    throw new Error(
      `${envVar} is missing from the ${server} test process. Run the suite through ` +
        `the package test command so tests/host/run.ts can own its containers.`,
    );
  }
}

/** The PostgreSQL container address for this run. */
export function pgUrl(): string {
  const dsn = process.env[PG_URL_ENV];
  requireLiveDb(dsn, PG_URL_ENV, "PostgreSQL");
  return dsn;
}

/** The MySQL container address for this run. */
export function mysqlUrl(): string {
  const dsn = process.env[MYSQL_URL_ENV];
  requireLiveDb(dsn, MYSQL_URL_ENV, "MySQL");
  return dsn;
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
  const dsn = pgUrl();
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: dsn });
  await client.connect();
  return client;
}
