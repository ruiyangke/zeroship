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
 *   2. Maintains the dev RPC registry (transform-appended
 *      `globalThis.__registerModule(<file>, { <wireId>: <fn> })`
 *      calls land here).
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
  ENV_RUNTIME_DESCRIPTOR,
  ENV_VITE_ORIGIN,
  HMR_POLL_PATH,
} from "../constants.js";
import { startHmrPoll } from "./hmr";
import { createDevRpcRegistry } from "./rpc-registry";

const ENTRY = (globalThis as { process?: { env?: { ZEROSHIP_ENTRY?: string } } }).process?.env?.ZEROSHIP_ENTRY!;

let runner: ModuleRunner | null = null;
let runnerPromise: Promise<ModuleRunner> | null = null;
let stopHmrPoll: (() => void) | null = null;

// Procedure registry — transform-appended `__registerModule(file, handlers)`
// calls land here. Each module owns its current wire-id set, so a hot
// update can prune the previous registrations before the module is
// re-imported. That makes rename/delete stop resolving immediately.
const registry = createDevRpcRegistry();
(globalThis as Record<string, unknown>).__registerModule = (
  moduleId: string,
  handlers: Record<string, (input: unknown, ctx: unknown) => unknown>,
) => {
  registry.replaceModule(moduleId, handlers);
};
(globalThis as Record<string, unknown>).__lookup = (name: string) =>
  registry.registry.get(name);

function applyRuntimeDescriptorJson(
  json: string | null | undefined,
  source: string,
): void {
  const g = globalThis as {
    __zsRuntimeDescriptor?: Record<string, unknown>;
  };

  if (json == null || json.trim() === "") {
    delete g.__zsRuntimeDescriptor;
    return;
  }

  try {
    const parsed = JSON.parse(json);
    if (parsed != null && typeof parsed === "object" && !Array.isArray(parsed)) {
      g.__zsRuntimeDescriptor = parsed as Record<string, unknown>;
      return;
    }
    console.error(`[zeroship:dev] ignored ${source} runtime descriptor: expected JSON object`);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error(`[zeroship:dev] ignored ${source} runtime descriptor: ${msg}`);
  }

  delete g.__zsRuntimeDescriptor;
}

const initialRuntimeDescriptorJson = (globalThis as {
  process?: { env?: Record<string, string | undefined> };
}).process?.env?.[ENV_RUNTIME_DESCRIPTOR];
if (initialRuntimeDescriptorJson !== undefined) {
  applyRuntimeDescriptorJson(initialRuntimeDescriptorJson, "boot");
}

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
  registry: registry.registry,
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
  async getDbInternal() {
    const r = await getRunner();
    return await r.import("@zeroship/db/internal") as Parameters<typeof devEntry>[0]["getDbInternal"] extends (() => Promise<infer T>) | undefined ? T : never;
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
  stopHmrPoll = startHmrPoll(
    pollUrl,
    () => runner,
    console.log,
    (files) => {
      for (const file of files) {
        registry.pruneModule(file);
      }
    },
    (runtimeDescriptorJson) => {
      applyRuntimeDescriptorJson(runtimeDescriptorJson, "HMR");
      entry.resetSchemaInstalled();
    },
  );

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
// (`docs/reference/zeroship-standard.md`): dev's namespace may change per
// request so the dict resolves on every call. Dispatch + schema-install
// live in `@zeroship/bootstrap`; this module only owns the runner +
// registry + HMR poll.
export default { fetch: entry.fetch, rpc: entry.rpc };
