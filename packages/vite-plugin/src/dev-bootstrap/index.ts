/** ModuleRunner-backed entry snapshots for the native development loader. */
import type { ModuleRunner } from "vite/module-runner";
import {
  ENV_VITE_ORIGIN,
  HMR_FIRST_LOAD_PARAM,
  HMR_POLL_PATH,
  PROCEDURE_BINDINGS_PATH,
} from "../constants.js";
import type { DevServerBindingSnapshot } from "./entry";
import { fetchHmrChanges, startHmrPoll } from "./hmr";
import { createEntryLoader } from "./loader";
import { createRunner } from "./transport";

const processEnv = (globalThis as {
  process?: { env?: Record<string, string | undefined> };
}).process?.env;
const ENTRY = processEnv?.ZEROSHIP_ENTRY;
const VITE_ORIGIN = processEnv?.[ENV_VITE_ORIGIN];

let runner: ModuleRunner | null = null;
let runnerPromise: Promise<ModuleRunner> | null = null;
let stopHmrPoll: (() => void) | null = null;

async function getRunner(): Promise<ModuleRunner> {
  if (runner) return runner;
  if (runnerPromise) return runnerPromise;
  runnerPromise = createRunner().then((created) => {
    runner = created;
    console.log(`[zeroship:dev] ModuleRunner ready, entry: ${ENTRY}`);
    return created;
  }).catch((error) => {
    runnerPromise = null;
    throw error;
  });
  return runnerPromise;
}

function resetRunner(): void {
  runner = null;
  runnerPromise = null;
}

async function readBindingSnapshot(): Promise<DevServerBindingSnapshot> {
  if (!VITE_ORIGIN) throw new Error(`[zeroship] ${ENV_VITE_ORIGIN} not set`);
  const response = await fetch(`${VITE_ORIGIN}${PROCEDURE_BINDINGS_PATH}`);
  const payload = await response.json() as {
    version?: unknown;
    bindings?: unknown;
    error?: { message?: unknown };
  };
  if (!response.ok) {
    const message = typeof payload.error?.message === "string"
      ? payload.error.message
      : `procedure binding request failed with HTTP ${response.status}`;
    throw new Error(message);
  }
  if (typeof payload.version !== "string" || !Array.isArray(payload.bindings)) {
    throw new TypeError("invalid procedure binding response");
  }
  return payload as DevServerBindingSnapshot;
}

export function createDevEntryLoader(invalidate: () => void): () => Promise<unknown> {
  if (typeof invalidate !== "function") {
    throw new TypeError("development entry invalidation callback must be a function");
  }
  if (!ENTRY) throw new Error("[zeroship] ZEROSHIP_ENTRY not set");
  if (!VITE_ORIGIN) throw new Error(`[zeroship] ${ENV_VITE_ORIGIN} not set`);
  const pollUrl = `${VITE_ORIGIN}${HMR_POLL_PATH}`;

  return createEntryLoader({
    entry: ENTRY,
    takeChanges: () => fetchHmrChanges(`${pollUrl}?${HMR_FIRST_LOAD_PARAM}`),
    watchChanges(onChange) {
      if (stopHmrPoll) return;
      stopHmrPoll = startHmrPoll(pollUrl, onChange, console.log);
      const proc = (globalThis as {
        process?: { once?: (event: string, listener: () => void) => void };
      }).process;
      proc?.once?.("exit", () => {
        stopHmrPoll?.();
        stopHmrPoll = null;
      });
    },
    readBindings: readBindingSnapshot,
    runner: getRunner,
    resetRunner,
    warn: (message) => console.warn(message),
  }, invalidate);
}
