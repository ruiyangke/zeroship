/** ModuleRunner-backed entry snapshots for the native development loader. */
import type { ModuleRunner } from "vite/module-runner";
import {
  ENV_VITE_ORIGIN,
  HMR_POLL_PATH,
  PROCEDURE_BINDINGS_PATH,
} from "../constants.js";
import { buildDevEntrySnapshot, type DevServerBindingSnapshot } from "./entry";
import { invalidateChangedFiles, startHmrPoll, type HmrUpdate } from "./hmr";
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

function isDependencyRefresh(error: unknown): boolean {
  return error instanceof Error && error.message.includes("is in the optimize deps directory");
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
  const entry = ENTRY;
  const viteOrigin = VITE_ORIGIN;

  const pendingChanged = new Set<string>();
  let bindingVersion: string | null = null;
  let loadStarted = false;

  const receiveHmr = (update: HmrUpdate) => {
    for (const file of update.changed) pendingChanged.add(file);
    const bindingsChanged =
      bindingVersion !== null &&
      update.bindingsVersion !== undefined &&
      update.bindingsVersion !== bindingVersion;
    if (update.bindingsVersion !== undefined && bindingVersion !== null) {
      bindingVersion = update.bindingsVersion;
    }
    if (loadStarted && (update.changed.length > 0 || bindingsChanged)) invalidate();
  };

  if (!stopHmrPoll) {
    stopHmrPoll = startHmrPoll(`${viteOrigin}${HMR_POLL_PATH}`, receiveHmr, console.log);
    const proc = (globalThis as {
      process?: { once?: (event: string, listener: () => void) => void };
    }).process;
    proc?.once?.("exit", () => {
      stopHmrPoll?.();
      stopHmrPoll = null;
    });
  }

  async function loadWith(current: ModuleRunner): Promise<unknown> {
    const changed = [...pendingChanged];
    pendingChanged.clear();
    if (changed.length > 0) invalidateChangedFiles(current, changed);
    const userModule = await current.import(entry);
    let bindings = await readBindingSnapshot();
    const seenVersions = new Set<string>();
    while (true) {
      if (seenVersions.has(bindings.version)) {
        throw new Error("procedure bindings changed cyclically while loading the entry");
      }
      seenVersions.add(bindings.version);
      // Development resolves lazy bindings now so an older retained snapshot
      // cannot import replacement code after its ModuleRunner graph is invalidated.
      const snapshot = await buildDevEntrySnapshot(current, userModule, bindings.bindings);
      const currentBindings = await readBindingSnapshot();
      if (currentBindings.version === bindings.version) {
        bindingVersion = bindings.version;
        return snapshot;
      }
      bindings = currentBindings;
    }
  }

  return async function loadEntry(): Promise<unknown> {
    loadStarted = true;
    let current = await getRunner();
    try {
      return await loadWith(current);
    } catch (error) {
      if (!isDependencyRefresh(error)) throw error;
      console.log("[zeroship:dev] dependencies refreshed, resetting ModuleRunner");
      resetRunner();
      current = await getRunner();
      return loadWith(current);
    }
  };
}
