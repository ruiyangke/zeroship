// sdks/vite-plugin/src/dev-database-url.ts
//
// The ONE resolution of the dev `DATABASE_URL`, shared by the two processes that
// must agree on it: the dev server (which spawns the runtime) and the standalone
// migrate CLI (which applies the schema ahead of it).
//
// Sharing the resolution is the whole point. The migrate step must write to the
// same file the worker later opens, and `DATABASE_URL` is overridable at two
// layers (shell, then `.env`) before the dev default applies. A second, re-derived
// copy of this logic is a silent failure mode, not a duplication nit: the apply
// reports `applied: [...]`, the dev boot looks healthy, and every `env.db` call
// still fails with `no such table` — which is exactly what a hardcoded
// `.zeroship` did to `tests/e2e-browser` when it gave each demo a private state
// dir. See `gen-types/dev-apply.ts`'s `devSqliteDir`.

import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

/** Which layer supplied the effective `DATABASE_URL`. */
export type DatabaseUrlSource = "shell" | "dotenv" | "default";

/** Both the runtime configuration and migration path use this file selector. */
const SQLITE_URL_PREFIX = "sqlite:";

/** Source, described in words, for the error message below. */
function sourceLabel(source: DatabaseUrlSource): string {
  switch (source) {
    case "shell":
      return "the shell environment";
    case "dotenv":
      return "your .env file";
    case "default":
      return "the built-in dev default";
  }
}

/**
 * The scheme token before the first `:`, or `null` when the value has none.
 * A prefix check, not a substring search — `sqlite:./postgres-backup/db`
 * must read as scheme `sqlite`, never as "contains postgres".
 */
function schemeOf(url: string): string | null {
  const colon = url.indexOf(":");
  return colon === -1 ? null : url.slice(0, colon);
}

/**
 * Thrown by `resolveDatabaseUrl` when the resolved `DATABASE_URL` is not the
 * SQLite form the zeroship dev tier supports (`sqlite:<path>`). Named so a
 * caller can tell this apart from any other resolution failure.
 *
 * Carries the SCHEME and SOURCE, never the raw URL: a rejected value is very
 * often a production Postgres connection string with an embedded password,
 * and this module already declines to log the raw shell/`.env` value for
 * exactly that reason — see `logDatabaseUrlSource` below, which prints the
 * URL only for the (non-secret, fixed) default.
 */
export class DevDatabaseUrlSchemeError extends Error {
  constructor(
    readonly scheme: string | null,
    readonly source: DatabaseUrlSource,
  ) {
    super(
      `DATABASE_URL from ${sourceLabel(source)} uses ${
        scheme ? `the "${scheme}:" scheme` : `a value with no "sqlite:" scheme`
      }, but the zeroship dev tier only supports the SQLite dev database, via a ` +
        `"sqlite:<path>" URL. The dev tier is deliberately a different tier from ` +
        `production, not a smaller production, so a Postgres DATABASE_URL exported ` +
        `for a production tool will NOT work here — it would otherwise be silently ` +
        `misapplied to the SQLite dev file while the runtime is told something else. ` +
        `Unset DATABASE_URL to use the project-local default ` +
        `(sqlite:.zeroship/dev.sqlite), or point it at a sqlite: URL instead.`,
    );
    this.name = "DevDatabaseUrlSchemeError";
  }
}

/** Resolve the dev database file, refusing ephemeral modes and URI options. */
export function sqliteDevFilePath(databaseUrl: string, source: DatabaseUrlSource = "default"): string {
  if (!databaseUrl.startsWith(SQLITE_URL_PREFIX)) {
    throw new DevDatabaseUrlSchemeError(schemeOf(databaseUrl), source);
  }
  const prefix = databaseUrl.startsWith("sqlite://") ? "sqlite://" : SQLITE_URL_PREFIX;
  const path = databaseUrl.slice(prefix.length);
  if (!path.trim() || path.toLowerCase() === ":memory:" || /[?#]/.test(path) || /^[a-z][a-z0-9+.-]*:/i.test(path)) {
    throw new Error(
      `DATABASE_URL from ${sourceLabel(source)} must name a SQLite file, such as sqlite:.zeroship/dev.sqlite. ` +
      "Memory databases and SQLite URI options are unsupported.",
    );
  }
  return path;
}

/**
 * Parse `<root>/.env` into a plain record. Deliberately minimal — this reads the
 * one variable the dev tier cares about, and is not a dotenv implementation.
 */
export function parseDotenvVars(root: string): Record<string, string> {
  const envPath = resolve(root, ".env");
  if (!existsSync(envPath)) return {};

  const dotenvVars: Record<string, string> = {};
  for (const line of readFileSync(envPath, "utf-8").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const eq = trimmed.indexOf("=");
    if (eq === -1) continue;
    const key = trimmed.slice(0, eq).trim();
    let val = trimmed.slice(eq + 1).trim();
    if ((val.startsWith("\"") && val.endsWith("\"")) || (val.startsWith("'") && val.endsWith("'"))) {
      val = val.slice(1, -1);
    }
    dotenvVars[key] = val;
  }
  return dotenvVars;
}

/**
 * Shell env wins, then `.env`, then the dev default.
 *
 * Rejects a non-SQLite result before returning it — SC-4 decision 1
 * (`docs/proposals/2026-08-26-sc4-dev-and-hmr-mechanism.md`). This is the ONE
 * resolution both dev entry points share (`dev-server.ts`, `cli/migrate-dev.ts`);
 * validating here, rather than at each call site, is what keeps a Postgres
 * `DATABASE_URL` from reaching the SQLite-only apply path AND the runtime
 * child with two different silent outcomes.
 *
 * @throws {DevDatabaseUrlSchemeError} when the resolved value is not `sqlite:<path>`.
 */
export function resolveDatabaseUrl(
  parentEnv: NodeJS.ProcessEnv,
  dotenvVars: Record<string, string>,
  defaultDatabaseUrl: string,
): { databaseUrl: string; source: DatabaseUrlSource } {
  let databaseUrl: string;
  let source: DatabaseUrlSource;
  if (parentEnv.DATABASE_URL) {
    databaseUrl = parentEnv.DATABASE_URL;
    source = "shell";
  } else if (dotenvVars.DATABASE_URL) {
    databaseUrl = dotenvVars.DATABASE_URL;
    source = "dotenv";
  } else {
    databaseUrl = defaultDatabaseUrl;
    source = "default";
  }

  sqliteDevFilePath(databaseUrl, source);
  return { databaseUrl, source };
}

export function logDatabaseUrlSource(source: DatabaseUrlSource, databaseUrl: string): void {
  if (source === "shell") {
    console.log("[zeroship] using DATABASE_URL from shell environment");
    return;
  }
  if (source === "dotenv") {
    console.log("[zeroship] using DATABASE_URL from .env");
    return;
  }
  console.log(`[zeroship] using default DATABASE_URL ${databaseUrl}`);
}
