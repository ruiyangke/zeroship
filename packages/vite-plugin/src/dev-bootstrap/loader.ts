/** Serialized entry loads for the native development loader. */
import {
  buildDevEntrySnapshot,
  type DevModuleRunner,
  type DevServerBindingSnapshot,
} from "./entry";
import { invalidateChangedFiles, type HmrUpdate, type ModuleRunnerLike } from "./hmr";

export type EntryRunner = DevModuleRunner & ModuleRunnerLike;

/** What a loader needs from the dev server it loads through. */
export interface EntryLoaderHost {
  /** The server entry the runner imports. */
  entry: string;
  /**
   * Take the source changes the dev server holds, clearing them there.
   * Rejects when the dev server cannot be reached.
   */
  takeChanges(): Promise<HmrUpdate>;
  /** Report every later batch of taken changes to `onChange`. */
  watchChanges(onChange: (update: HmrUpdate) => void): void;
  readBindings(): Promise<DevServerBindingSnapshot>;
  runner(): Promise<EntryRunner>;
  resetRunner(): void;
  warn(message: string): void;
}

function isDependencyRefresh(error: unknown): boolean {
  return error instanceof Error && error.message.includes("is in the optimize deps directory");
}

/**
 * The loader the native runtime calls once per entry generation. `invalidate`
 * tells the runtime that the source changed after a load began.
 */
export function createEntryLoader(
  host: EntryLoaderHost,
  invalidate: () => void,
): () => Promise<unknown> {
  const pendingChanged = new Set<string>();
  let bindingVersion: string | null = null;
  let watching = false;

  const receiveChanges = (update: HmrUpdate) => {
    for (const file of update.changed) pendingChanged.add(file);
    const bindingsChanged =
      bindingVersion !== null &&
      update.bindingsVersion !== undefined &&
      update.bindingsVersion !== bindingVersion;
    if (update.bindingsVersion !== undefined && bindingVersion !== null) {
      bindingVersion = update.bindingsVersion;
    }
    if (update.changed.length > 0 || bindingsChanged) invalidate();
  };

  async function loadWith(current: EntryRunner): Promise<unknown> {
    const changed = [...pendingChanged];
    pendingChanged.clear();
    if (changed.length > 0) invalidateChangedFiles(current, changed);
    const userModule = await current.import(host.entry);
    let bindings = await host.readBindings();
    const seenVersions = new Set<string>();
    while (true) {
      if (seenVersions.has(bindings.version)) {
        throw new Error("procedure bindings changed cyclically while loading the entry");
      }
      seenVersions.add(bindings.version);
      // Development resolves lazy bindings now so an older retained snapshot
      // cannot import replacement code after its ModuleRunner graph is invalidated.
      const snapshot = await buildDevEntrySnapshot(current, userModule, bindings.bindings);
      const currentBindings = await host.readBindings();
      if (currentBindings.version === bindings.version) {
        bindingVersion = bindings.version;
        return snapshot;
      }
      bindings = currentBindings;
    }
  }

  return async function loadEntry(): Promise<unknown> {
    if (!watching) {
      watching = true;
      // The dev server queues changes whether or not a runtime is loading, and
      // it spawns runtimes lazily and replaces them on edits, so its queue can
      // hold changes older than this runtime. The first load fetches every
      // module after them. Taken and dropped here, they cannot be mistaken for
      // an edit made during that load, which the runtime answers by refusing
      // to serve until it is replaced.
      try {
        await host.takeChanges();
      } catch (error) {
        host.warn(
          "[zeroship:dev] could not clear the dev server's change queue before the " +
            "first load, so a change queued before this runtime started may be " +
            `taken for an edit made while it loads: ${(error as Error).message}`,
        );
      } finally {
        host.watchChanges(receiveChanges);
      }
    }
    let current = await host.runner();
    try {
      return await loadWith(current);
    } catch (error) {
      if (!isDependencyRefresh(error)) throw error;
      console.log("[zeroship:dev] dependencies refreshed, resetting ModuleRunner");
      host.resetRunner();
      current = await host.runner();
      return loadWith(current);
    }
  };
}
