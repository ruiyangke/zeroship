// Database addresses the host test runner hands each test process.
//
// `tests/host/run.ts` owns the PostgreSQL and MySQL containers and starts every test
// file through `node:test`'s `run()`, passing their addresses as arguments of each
// test process ([`liveDbArgv`]). An argument is set at the one call that spawns the
// process and reaches that process alone: the CLI processes a test spawns inherit
// nothing from it. There is no other source, so a test file started any other way
// fails loudly instead of finding a database somewhere else.

import type { Client } from "pg";

const POSTGRES_ARGUMENT = "--zero-migrate-host-postgres=";
const MYSQL_ARGUMENT = "--zero-migrate-host-mysql=";

/** The arguments the runner passes each test process, naming its containers. */
export function liveDbArgv(urls: { postgres: string; mysql: string }): string[] {
  return [`${POSTGRES_ARGUMENT}${urls.postgres}`, `${MYSQL_ARGUMENT}${urls.mysql}`];
}

/** The one value of a runner argument, or a failure naming what is missing. */
function runnerArgument(prefix: string, server: string): string {
  const values = process.argv
    .filter((argument) => argument.startsWith(prefix))
    .map((argument) => argument.slice(prefix.length));
  if (values.length !== 1 || values[0].trim() === "") {
    throw new Error(
      `this test process was not given the address of the ${server} container its ` +
        `runner owns. Run the suite through the package test command ` +
        `(\`pnpm --filter zero-migrate-cli test\`), whose runner, tests/host/run.ts, ` +
        `starts the containers.`,
    );
  }
  return values[0];
}

/** The PostgreSQL container address for this run. */
export function pgUrl(): string {
  return runnerArgument(POSTGRES_ARGUMENT, "PostgreSQL");
}

/** The MySQL container address for this run. */
export function mysqlUrl(): string {
  return runnerArgument(MYSQL_ARGUMENT, "MySQL");
}

/**
 * Connect a client to the run's PostgreSQL container.
 *
 * Returns a connected `pg.Client` the caller owns (and must `end()`). It never
 * returns null: a test that needs the database either reaches it or fails.
 */
export async function connectLivePg(): Promise<Client> {
  const dsn = pgUrl();
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: dsn });
  await client.connect();
  return client;
}
