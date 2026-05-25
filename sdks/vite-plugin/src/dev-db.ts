// sdks/vite-plugin/src/dev-db.ts
//
// Zero-setup local-dev database for creators who don't want to install
// Postgres or run Docker. The runtime now ships both backends and
// dispatches by URL scheme, so dev just points DATABASE_URL at a
// project-local SQLite file and lets the in-process backend take over.
//
// Used by dev-server.ts when DATABASE_URL is not provided via .env or
// the parent environment.

import { mkdirSync } from "node:fs";
import { resolve } from "node:path";

const DEV_DB_DIR = ".zeroship";
const DEV_DB_FILE = "dev.sqlite";
const DEV_DB_URL = `sqlite:${DEV_DB_DIR}/${DEV_DB_FILE}`;

export interface DevDatabase {
  /** sqlite:.zeroship/dev.sqlite */
  databaseUrl: string;
}

/**
 * Ensure the project-local SQLite directory exists and return the URL
 * the runtime should receive via DATABASE_URL.
 */
export function resolveDevDatabase(projectRoot: string): DevDatabase {
  mkdirSync(resolve(projectRoot, DEV_DB_DIR), { recursive: true });
  return { databaseUrl: DEV_DB_URL };
}
