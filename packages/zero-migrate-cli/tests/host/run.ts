import { resolve } from "node:path";
import { finished } from "node:stream/promises";
import { run } from "node:test";
import { spec, tap } from "node:test/reporters";
import { fileURLToPath } from "node:url";

import { GenericContainer, Wait, type StartedTestContainer } from "testcontainers";

import { liveDbArgv } from "./live-db.js";

const POSTGRES_PORT = 5432;
const MYSQL_PORT = 3306;
const DATABASE = "zeroship_migrate_test";
const PASSWORD = "zeroship-migrate-test";

/** The package directory, which every test process runs in. */
const PACKAGE = fileURLToPath(new URL("../..", import.meta.url));
/** The preload that resolves the freshly built addon in every test process. */
const ADDON_PRELOAD = new URL("./addon.ts", import.meta.url).href;

// Every test file runs in its own process, started by `run()` rather than by a
// `node --test` child, so the container addresses reach each one as an argument
// (`liveDbArgv`) and nothing else carries them.
async function runTests(files: string[], postgresUrl: string, mysqlUrl: string): Promise<number> {
  if (files.length === 0) throw new Error("the host test runner received no test files");

  let failed = false;
  const stream = run({
    files: files.map((file) => resolve(file)),
    concurrency: true,
    timeout: 600_000,
    execArgv: ["--import", "tsx", "--import", ADDON_PRELOAD],
    argv: liveDbArgv({ postgres: postgresUrl, mysql: mysqlUrl }),
  });
  stream.on("test:fail", (data) => {
    // A failing `todo` test is reported and does not fail the run, as under
    // `node --test`.
    if (!data.todo) failed = true;
  });
  // The reporter `node --test` picks by default: spec on a terminal, TAP otherwise.
  const reporter = stream.compose(process.stdout.isTTY ? spec : tap);
  reporter.pipe(process.stdout);
  await finished(reporter);
  return failed ? 1 : 0;
}

async function stopAll(containers: StartedTestContainer[]): Promise<void> {
  const results = await Promise.allSettled(containers.reverse().map((container) => container.stop()));
  const failures = results
    .filter((result): result is PromiseRejectedResult => result.status === "rejected")
    .map((result) => result.reason);
  if (failures.length > 0) throw new AggregateError(failures, "database container cleanup failed");
}

const files = process.argv.slice(2).map((file) => resolve(file));
process.chdir(PACKAGE);

const containers: StartedTestContainer[] = [];
let testStatus = 1;
let runError: unknown;

try {
  const postgres = await new GenericContainer("postgres:16")
    .withEnvironment({ POSTGRES_DB: DATABASE, POSTGRES_PASSWORD: PASSWORD })
    .withExposedPorts(POSTGRES_PORT)
    .withWaitStrategy(
      Wait.forAll([
        Wait.forLogMessage("database system is ready to accept connections", 2),
        Wait.forListeningPorts(),
      ]),
    )
    .start();
  containers.push(postgres);

  const mysql = await new GenericContainer("mysql:8.4")
    .withEnvironment({ MYSQL_DATABASE: DATABASE, MYSQL_ROOT_PASSWORD: PASSWORD })
    .withExposedPorts(MYSQL_PORT)
    .withWaitStrategy(Wait.forLogMessage(/ready for connections.*port: 3306/i))
    .start();
  containers.push(mysql);

  const postgresUrl = `postgres://postgres:${PASSWORD}@${postgres.getHost()}:${postgres.getMappedPort(POSTGRES_PORT)}/${DATABASE}`;
  const mysqlUrl = `mysql://root:${PASSWORD}@${mysql.getHost()}:${mysql.getMappedPort(MYSQL_PORT)}/${DATABASE}`;
  testStatus = await runTests(files, postgresUrl, mysqlUrl);
} catch (error) {
  runError = error;
}

try {
  await stopAll(containers);
} catch (cleanupError) {
  runError =
    runError === undefined
      ? cleanupError
      : new AggregateError([runError, cleanupError], "host tests and cleanup failed");
}

if (runError !== undefined) throw runError;
process.exitCode = testStatus;
