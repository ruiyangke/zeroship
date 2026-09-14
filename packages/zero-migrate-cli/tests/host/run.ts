import { spawn } from "node:child_process";

import { GenericContainer, Wait, type StartedTestContainer } from "testcontainers";

import { MYSQL_URL_ENV, PG_URL_ENV } from "./live-db.js";

const POSTGRES_PORT = 5432;
const MYSQL_PORT = 3306;
const DATABASE = "zeroship_migrate_test";
const PASSWORD = "zeroship-migrate-test";

function runTests(files: string[], postgresUrl: string, mysqlUrl: string): Promise<number> {
  if (files.length === 0) throw new Error("the host test runner received no test files");

  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      [
        "--import",
        "tsx",
        "--import",
        "./tests/host/addon.ts",
        "--test",
        "--test-timeout=600000",
        ...files,
      ],
      {
        cwd: new URL("../..", import.meta.url),
        env: {
          ...process.env,
          NODE_ENV: "test",
          [PG_URL_ENV]: postgresUrl,
          [MYSQL_URL_ENV]: mysqlUrl,
        },
        stdio: "inherit",
      },
    );
    child.once("error", reject);
    child.once("close", (code, signal) => {
      if (signal !== null) reject(new Error(`host test process terminated by ${signal}`));
      else resolve(code ?? 1);
    });
  });
}

async function stopAll(containers: StartedTestContainer[]): Promise<void> {
  const results = await Promise.allSettled(containers.reverse().map((container) => container.stop()));
  const failures = results
    .filter((result): result is PromiseRejectedResult => result.status === "rejected")
    .map((result) => result.reason);
  if (failures.length > 0) throw new AggregateError(failures, "database container cleanup failed");
}

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
  testStatus = await runTests(process.argv.slice(2), postgresUrl, mysqlUrl);
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
