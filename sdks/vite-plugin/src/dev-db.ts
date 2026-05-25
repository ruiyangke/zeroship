// sdks/vite-plugin/src/dev-db.ts
//
// Zero-setup local-dev database for creators who don't want to install
// Postgres or run Docker. The runtime now ships both backends and
// dispatches by URL scheme, so dev just points DATABASE_URL at a
// project-local SQLite file and lets the in-process backend take over.
//
// Used by dev-server.ts when DATABASE_URL is not provided via the shell
// environment or .env.

const DEV_DB_DIR = ".zeroship";
const DEV_DB_FILE = "dev.sqlite";
const DEV_DB_URL = `sqlite:${DEV_DB_DIR}/${DEV_DB_FILE}`;

export interface DevDatabase {
  /** sqlite:.zeroship/dev.sqlite */
  databaseUrl: string;
}

/**
 * Return the default project-local SQLite URL for dev mode.
 */
export function resolveDevDatabase(_projectRoot: string): DevDatabase {
  return { databaseUrl: DEV_DB_URL };
}
