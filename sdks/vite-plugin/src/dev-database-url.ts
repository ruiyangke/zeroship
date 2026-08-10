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

/** Shell env wins, then `.env`, then the dev default. */
export function resolveDatabaseUrl(
  parentEnv: NodeJS.ProcessEnv,
  dotenvVars: Record<string, string>,
  defaultDatabaseUrl: string,
): { databaseUrl: string; source: DatabaseUrlSource } {
  if (parentEnv.DATABASE_URL) {
    return { databaseUrl: parentEnv.DATABASE_URL, source: "shell" };
  }
  if (dotenvVars.DATABASE_URL) {
    return { databaseUrl: dotenvVars.DATABASE_URL, source: "dotenv" };
  }
  return { databaseUrl: defaultDatabaseUrl, source: "default" };
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
