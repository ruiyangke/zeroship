export type RpcHandler = (input: unknown, ctx: unknown) => unknown;

export interface DevRpcRegistry {
  registry: Map<string, RpcHandler>;
  replaceModule(moduleId: string, handlers: Record<string, RpcHandler>): void;
  pruneModule(moduleId: string): void;
}

export function createDevRpcRegistry(): DevRpcRegistry {
  const registry = new Map<string, RpcHandler>();
  const owners = new Map<string, string>();
  const byModule = new Map<string, Map<string, RpcHandler>>();

  function pruneModule(moduleId: string): void {
    const previous = byModule.get(moduleId);
    if (!previous) return;

    for (const wireId of previous.keys()) {
      if (owners.get(wireId) !== moduleId) continue;
      owners.delete(wireId);
      registry.delete(wireId);
    }
    byModule.delete(moduleId);
  }

  function replaceModule(
    moduleId: string,
    handlers: Record<string, RpcHandler>,
  ): void {
    pruneModule(moduleId);

    const next = new Map<string, RpcHandler>(Object.entries(handlers));
    byModule.set(moduleId, next);
    for (const [wireId, fn] of next) {
      owners.set(wireId, moduleId);
      registry.set(wireId, fn);
    }
  }

  return {
    registry,
    replaceModule,
    pruneModule,
  };
}
