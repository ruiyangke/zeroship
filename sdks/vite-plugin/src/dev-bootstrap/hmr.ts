interface EvaluatedModuleNodeLike {
  id: string;
  importers: Set<string>;
}

interface EvaluatedModulesLike {
  getModuleById(id: string): EvaluatedModuleNodeLike | undefined;
  getModulesByFile(file: string): Set<EvaluatedModuleNodeLike> | undefined;
  invalidateModule(node: EvaluatedModuleNodeLike): void;
}

interface ModuleRunnerLike {
  evaluatedModules: EvaluatedModulesLike;
}

interface PollHmrChangesOptions {
  pollUrl: string;
  fetchImpl?: typeof fetch;
  log?: (message: string) => void;
  onChange?: (update: HmrUpdate) => void;
}

export interface HmrUpdate {
  changed: string[];
  bindingsVersion?: string;
}

function collectAffectedModules(
  mods: EvaluatedModulesLike,
  changed: string[],
): EvaluatedModuleNodeLike[] {
  const queue: string[] = [];
  const affected = new Map<string, EvaluatedModuleNodeLike>();

  for (const file of changed) {
    for (const mod of mods.getModulesByFile(file) ?? []) {
      affected.set(mod.id, mod);
      queue.push(mod.id);
    }
  }

  while (queue.length > 0) {
    const id = queue.shift()!;
    const mod = affected.get(id) ?? mods.getModuleById(id);
    if (!mod) continue;

    affected.set(id, mod);
    for (const importerId of mod.importers) {
      if (affected.has(importerId)) continue;
      queue.push(importerId);
    }
  }

  return [...affected.values()];
}

export function invalidateChangedFiles(
  runner: ModuleRunnerLike,
  changed: string[],
): number {
  const affected = collectAffectedModules(runner.evaluatedModules, changed);
  for (const mod of affected) {
    runner.evaluatedModules.invalidateModule(mod);
  }
  return affected.length;
}

export async function pollHmrChanges({
  pollUrl,
  fetchImpl = fetch,
  log,
  onChange,
}: PollHmrChangesOptions): Promise<HmrUpdate> {
  try {
    const resp = await fetchImpl(pollUrl);
    if (!resp.ok) throw new Error(`HMR poll failed with HTTP ${resp.status}`);
    const payload = await resp.json() as {
      changed?: unknown;
      bindingsVersion?: unknown;
    };
    const changed: string[] = [];
    if (Array.isArray(payload.changed)) {
      for (const file of payload.changed) {
        if (typeof file === "string") {
          changed.push(file);
        }
      }
    }
    const update: HmrUpdate = {
      changed,
      ...(typeof payload.bindingsVersion === "string"
        ? { bindingsVersion: payload.bindingsVersion }
        : {}),
    };
    if (changed.length > 0) {
      log?.(`[zeroship:hmr] ${changed.length} module(s) updated`);
    }
    onChange?.(update);
    return update;
  } catch {
    // Vite not ready or restarting — silently ignore
    return { changed: [] };
  }
}

export function startHmrPoll(
  pollUrl: string,
  onChange: (update: HmrUpdate) => void,
  log?: (message: string) => void,
): () => void {
  const handle = setInterval(() => {
    void pollHmrChanges({
      pollUrl,
      log,
      onChange,
    });
  }, 500);

  return () => clearInterval(handle);
}
