/**
 * Dev bootstrap — entry module for the zeroship V8 runtime in dev mode.
 * Bundled into `dist/dev-bootstrap.js` by esbuild.
 *
 * Stage 7 of the refactor moved every piece of dispatch + schema-install
 * coordination into `@zeroship/bootstrap` (the framework-internal
 * package the runtime crate also consumes). This module is now a thin
 * shell that:
 *
 *   1. Constructs a `ModuleRunner` connected to Vite over HTTP.
 *   2. Maintains the `__register` registry (transform-appended
 *      `globalThis.__register(<wireId>, <fn>)` calls land here).
 *   3. Polls Vite for changed files every 500ms and invalidates the
 *      runner's evaluated-module cache (HMR proxy — V8 can't open the
 *      outbound WS Vite expects).
 *   4. Hands all of that to `devEntry(...)` and re-exports the
 *      returned `{ fetch, rpc }` as `default`.
 *
 * The dispatch + schema-install + normalisation logic lives in
 * `@zeroship/bootstrap/dev`. Single source of truth — same
 * `__zsDispatch` runs in dev and prod, same `installSchema` is called
 * on first request (dev) or at module-eval (prod).
 */
import { createRunner } from "./transport";
import type { ModuleRunner } from "vite/module-runner";
import { devEntry } from "@zeroship/bootstrap/dev";
import {
  ENV_VITE_ORIGIN,
  HMR_POLL_PATH,
} from "../constants.js";
import { startHmrPoll } from "./hmr";

const ENTRY = (globalThis as { process?: { env?: { ZEROSHIP_ENTRY?: string } } }).process?.env?.ZEROSHIP_ENTRY!;

let runner: ModuleRunner | null = null;
let runnerPromise: Promise<ModuleRunner> | null = null;
let stopHmrPoll: (() => void) | null = null;

// Procedure registry — transform-appended `__register(name, fn)` calls
// land here. Importing the user module triggers those side-effects.
// Last-write-wins so HMR replacements land cleanly. The dev-entry's
// `normalizeUserModule` reads this map per call.
const registry: Map<string, (input: unknown, ctx: unknown) => unknown> = new Map();
(globalThis as Record<string, unknown>).__register = (name: string, fn: (input: unknown, ctx: unknown) => unknown) => {
  registry.set(name, fn);
};
(globalThis as Record<string, unknown>).__lookup = (name: string) => registry.get(name);

async function getRunner(): Promise<ModuleRunner> {
  if (runner) return runner;
  if (runnerPromise) return runnerPromise;
  runnerPromise = createRunner().then((r) => {
    runner = r;
    console.log(`[zeroship:dev] ModuleRunner ready, entry: ${ENTRY}`);
    return r;
  }).catch((err) => {
    runnerPromise = null;
    throw err;
  });
  return runnerPromise;
}

// Module-local handle on the dev entry — exposed so the
// deps-reoptimize path can reset the schema-install latch when the
// runner is rebuilt (a fresh runner carries a separate
// `@zeroship/bootstrap` copy; schema must be re-registered against
// the new instanceof anchors).
const entry = devEntry({
  async loadUserModule() {
    const r = await getRunner();
    try {
      return await r.import(ENTRY);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      if (msg.includes("is in the optimize deps directory")) {
        console.log("[zeroship:dev] deps re-optimized, resetting runner");
        runner = null;
        runnerPromise = null;
        entry.resetSchemaInstalled();
        const fresh = await getRunner();
        return await fresh.import(ENTRY);
      }
      throw err;
    }
  },
  getEnvDb() {
    const envObj = (globalThis as { __zs_env?: () => { db?: unknown } | undefined }).__zs_env?.();
    return envObj?.db;
  },
  registry,
  // Load `installSchema` THROUGH the ModuleRunner so the `TypeBuilder`
  // class identity matches the one the user's `t.*` builders use.
  // Without this, the esbuild-bundled `installSchema` carries its own
  // bundled copy of `TypeBuilder` from @zeroship/db — and `instanceof`
  // checks inside validateRefTargets / normalizeSchema then return
  // `false` for builders the user constructed.
  async getInstallSchema() {
    const r = await getRunner();
    const mod = await r.import("@zeroship/bootstrap/install-schema") as {
      installSchema: Parameters<typeof devEntry>[0]["getInstallSchema"] extends (() => Promise<infer T>) | undefined ? T : never;
    };
    return mod.installSchema;
  },
});

// Kick off connection immediately + start HMR poll.
getRunner()
  .then(() => ensureHmrPollStarted())
  .catch((e) => console.error("[zeroship:dev] Runner init failed:", e));

/**
 * Poll Vite for changed files every 500ms and invalidate the
 * ModuleRunner's evaluated-module cache for each changed path. Causes
 * the next import() to re-fetch from Vite (which re-transforms).
 *
 * The runtime doesn't support outbound WebSocket connections (V8 server-
 * side only), so we can't use Vite's WS-based HMR. HTTP poll is the
 * dev-only fallback.
 */
function ensureHmrPollStarted() {
  if (stopHmrPoll) return;

  const viteOrigin = (globalThis as { process?: { env?: Record<string, string | undefined> } })
    .process?.env?.[ENV_VITE_ORIGIN];
  if (!viteOrigin) return;

  const pollUrl = `${viteOrigin}${HMR_POLL_PATH}`;
  stopHmrPoll = startHmrPoll(pollUrl, () => runner, console.log);

  const proc = (globalThis as {
    process?: {
      once?: (event: string, listener: () => void) => void;
    };
  }).process;
  if (typeof proc?.once === "function") {
    proc.once("exit", () => {
      stopHmrPoll?.();
      stopHmrPoll = null;
    });
  }
}

// Function-shape `default.rpc` per the ZS standard
// (`docs/reference/zs-standard.md`): dev's namespace may change per
// request so the dict resolves on every call. Dispatch + schema-install
// live in `@zeroship/bootstrap`; this module only owns the runner +
// registry + HMR poll.
export default { fetch: entry.fetch, rpc: entry.rpc };
