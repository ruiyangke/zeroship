import type { ModuleRunner } from "vite/module-runner";

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
  getCurrentRunner: () => ModuleRunnerLike | null;
  fetchImpl?: typeof fetch;
  clearKind?: () => number;
  restoreKind?: (token: number) => void;
  log?: (message: string) => void;
  pendingChanged?: Set<string>;
  onBeforeInvalidate?: (changed: string[]) => void;
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
  getCurrentRunner,
  fetchImpl = fetch,
  clearKind,
  restoreKind,
  log,
  pendingChanged = new Set<string>(),
  onBeforeInvalidate,
}: PollHmrChangesOptions): Promise<number> {
  const token = typeof clearKind === "function" ? clearKind() : -1;
  try {
    const resp = await fetchImpl(pollUrl);
    const payload = await resp.json() as { changed?: unknown };
    if (Array.isArray(payload.changed)) {
      for (const file of payload.changed) {
        if (typeof file === "string") {
          pendingChanged.add(file);
        }
      }
    }

    if (pendingChanged.size === 0) {
      return 0;
    }

    const changed = [...pendingChanged];
    pendingChanged.clear();
    onBeforeInvalidate?.(changed);

    const runner = getCurrentRunner();
    if (!runner) {
      return 0;
    }

    const invalidated = invalidateChangedFiles(runner, changed);
    if (invalidated > 0) {
      log?.(`[zeroship:hmr] ${changed.length} module(s) updated`);
    }
    return invalidated;
  } catch {
    // Vite not ready or restarting — silently ignore
    return 0;
  } finally {
    if (token >= 0 && typeof restoreKind === "function") {
      restoreKind(token);
    }
  }
}

export function startHmrPoll(
  pollUrl: string,
  getCurrentRunner: () => ModuleRunner | null,
  log?: (message: string) => void,
  onBeforeInvalidate?: (changed: string[]) => void,
): () => void {
  const pendingChanged = new Set<string>();
  const readGlobal = globalThis as {
    __zsClearKind?: () => number;
    __zsExitKind?: (token: number) => void;
  };
  const handle = setInterval(() => {
    void pollHmrChanges({
      pollUrl,
      getCurrentRunner,
      clearKind: readGlobal.__zsClearKind,
      restoreKind: readGlobal.__zsExitKind,
      log,
      pendingChanged,
      onBeforeInvalidate,
    });
  }, 500);

  return () => clearInterval(handle);
}
