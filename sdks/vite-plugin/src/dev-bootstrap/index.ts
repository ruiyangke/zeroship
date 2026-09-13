/** ModuleRunner and HMR integration, pending the native dev entry loader. */
import { createRunner } from "./transport";
import type { ModuleRunner } from "vite/module-runner";
import { devEntry } from "@zeroship/bootstrap/dev";
import {
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
        const fresh = await getRunner();
        return await fresh.import(ENTRY);
      }
      throw err;
    }
  },
  registry: registry.registry,

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
// request so the dict resolves on every call. Dispatch
// live in `@zeroship/bootstrap`; this module only owns the runner +
// registry + HMR poll.
//
// `loadWorkflow` is the workflow analogue of the function-shape `rpc`: the
// creator's Workflow classes are not in THIS module's namespace (they live
// behind the module runner), so the runtime's workflow dispatch resolves them
// through this async hook instead of a static dict. Omitting it is what made
// every `pnpm dev` workflow run fail with `Workflow not found` while the same
// class ran fine under a raw `zeroship serve`.
export default { fetch: entry.fetch, rpc: entry.rpc, loadWorkflow: entry.loadWorkflow };
