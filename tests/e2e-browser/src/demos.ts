// The demo registry — one entry per example under examples/ that this tier
// drives in a browser.
//
// PORTS. Every example that does not pass `devServerPort` falls back to the
// plugin's DEFAULT_DEV_PORT (3001), so two examples running at once fight over
// one port and the loser fails opaquely (the vite proxy just forwards to
// whoever won). Measured on this box: examples/db-todos was already holding
// :3001 from another shell when this tier was written. So every demo here gets
// an explicit, unique pair out of a private band:
//
//   vite dev server (the browser's origin)   5310 + n
//   zeroship dev runtime (RPC, proxied to)   3310 + n
//
// The `apiPortEnv` var is the one that example's vite.config.ts reads. Examples
// that hardcoded the default were changed to read an env var with the old
// default as fallback (the pattern examples/kv-dashboard etc. already used).
//
// STATE. Ports are not the only shared resource: the dev runtime opens
// `.zeroship/kv.redb` with an exclusive file lock and `.zeroship/dev.sqlite`,
// both inside the example directory. Two runs of the SAME example therefore
// crash-loop with "Database already open. Cannot acquire lock." regardless of
// ports. Each run gets a private state dir via ZEROSHIP_KV_PATH + DATABASE_URL.

import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");

/**
 * An external dependency a demo needs to work at all. When one is missing the
 * demo FAILS naming it exactly (never skips) — see `missingRequirements`.
 */
export type Requirement = "DATABASE_URL";

export interface Demo {
  /** Directory name under examples/. Also the name in the summary table. */
  readonly name: string;
  /** Port the vite dev server listens on — the origin the browser visits. */
  readonly vitePort: number;
  /** Port the zeroship dev runtime listens on; vite proxies /__zeroship/* here. */
  readonly apiPort: number;
  /** The env var this example's vite.config.ts reads for `devServerPort`. */
  readonly apiPortEnv: string;
  /**
   * Dependencies that must be present in the environment. Empty for demos the
   * dev runtime can serve from its built-in SQLite/redb defaults.
   */
  readonly requires: readonly Requirement[];
  /** One line, shown in the summary, describing what the suite drives. */
  readonly drives: string;
}

export const DEMOS: readonly Demo[] = [
  {
    name: "db-todos",
    vitePort: 5310,
    apiPort: 3310,
    apiPortEnv: "DB_TODOS_API_PORT",
    // The dev runtime defaults env.db to a project-local SQLite file when
    // DATABASE_URL is unset, so this demo does NOT require Postgres to boot.
    requires: [],
    drives: "seed a user, add a todo, see it listed, reload, still there",
  },
  {
    name: "csr-todo",
    vitePort: 5311,
    apiPort: 3311,
    apiPortEnv: "CSR_TODO_API_PORT",
    requires: [],
    drives: "list todos over RPC, add a local todo, drive the search stream",
  },
  {
    name: "starter",
    vitePort: 5312,
    apiPort: 3312,
    apiPortEnv: "STARTER_API_PORT",
    requires: [],
    drives: "type a message, submit, see it in the list, reload, still there",
  },
];

export function demoByName(name: string): Demo {
  const demo = DEMOS.find((d) => d.name === name);
  if (!demo) throw new Error(`unknown demo ${name}; known: ${DEMOS.map((d) => d.name).join(", ")}`);
  return demo;
}

export function demoDir(demo: Demo): string {
  return resolve(REPO_ROOT, "examples", demo.name);
}

/**
 * Requirements this environment does not satisfy. A non-empty result makes the
 * suite fail with the exact variable name — the brief is explicit that
 * "DATABASE_URL unset" must be distinguishable from a generic failure, and that
 * it must never become a skip.
 */
export function missingRequirements(demo: Demo): Requirement[] {
  return demo.requires.filter((req) => {
    const value = process.env[req];
    return value === undefined || value.trim() === "";
  });
}
