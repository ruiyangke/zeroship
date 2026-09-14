export interface DevModuleRunner {
  import(id: string): Promise<unknown>;
}

export interface DevServerBinding {
  wireId: string;
  sourceFile: string;
  exportName: string;
  kind: "query" | "mutation" | "action" | "stream" | "subscription";
  lazy?: boolean;
}

export interface DevServerBindingSnapshot {
  version: string;
  bindings: DevServerBinding[];
}

type Procedure = (input: unknown, context: unknown) => unknown;

const PROCEDURE_KINDS = new Set([
  "query",
  "mutation",
  "action",
  "stream",
  "subscription",
]);

function asObject(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object"
    ? value as Record<string, unknown>
    : undefined;
}

function isTaggedProcedure(value: unknown): value is Procedure {
  if (typeof value !== "function") return false;
  const procedure = value as unknown as {
    kind?: unknown;
    config?: { kind?: unknown };
  };
  const kind = procedure.kind ?? procedure.config?.kind;
  return typeof kind === "string" && PROCEDURE_KINDS.has(kind);
}

function procedureId(name: string, procedure: Procedure): string {
  const metadata = procedure as unknown as {
    id?: unknown;
    config?: { id?: unknown };
  };
  const id = metadata.id ?? metadata.config?.id;
  return typeof id === "string" && id.length > 0 ? id : name;
}

function copyDeclaredProcedures(
  target: Record<string, Procedure>,
  userDefault: Record<string, unknown> | undefined,
): void {
  const declared = userDefault?.rpc;
  if (declared == null) return;
  if (typeof declared !== "object" || Array.isArray(declared)) {
    throw new TypeError("default.rpc must be a procedure dictionary");
  }
  for (const name of Object.getOwnPropertyNames(declared)) {
    target[name] = (declared as Record<string, Procedure>)[name];
  }
}

/** Return callable targets without invoking handlers or encoding transport. */
export async function buildDevEntrySnapshot(
  runner: DevModuleRunner,
  userModule: unknown,
  bindings: readonly DevServerBinding[],
): Promise<{
  fetch?: unknown;
  fetchFast?: unknown;
  rpc: Record<string, Procedure>;
  userDefault?: Record<string, unknown>;
}> {
  const moduleObject = asObject(userModule) ?? {};
  const userDefault = asObject(Reflect.get(moduleObject, "default"));
  const rpc = Object.create(null) as Record<string, Procedure>;
  copyDeclaredProcedures(rpc, userDefault);

  for (const name of Object.keys(moduleObject)) {
    if (name === "default" || name === "fetch" || name === "fetchFast") continue;
    const procedure = moduleObject[name];
    if (!isTaggedProcedure(procedure)) continue;
    rpc[procedureId(name, procedure)] = procedure;
  }

  const modules = new Map<string, Record<string, unknown>>();
  for (const binding of bindings) {
    let namespace = modules.get(binding.sourceFile);
    if (!namespace) {
      const loaded = await runner.import(binding.sourceFile);
      namespace = asObject(loaded);
      if (!namespace) {
        throw new TypeError(`procedure module ${JSON.stringify(binding.sourceFile)} must be an object`);
      }
      modules.set(binding.sourceFile, namespace);
    }
    const procedure = Reflect.get(namespace, binding.exportName);
    if (typeof procedure !== "function") {
      throw new TypeError(
        `procedure ${JSON.stringify(binding.wireId)} from ` +
        `${binding.sourceFile}::${binding.exportName} must be a function`,
      );
    }
    rpc[binding.wireId] = procedure as Procedure;
  }

  const defaultFetch = userDefault?.fetch;
  const topLevelFetch = Reflect.get(moduleObject, "fetch");
  const defaultFetchFast = userDefault?.fetchFast;
  const topLevelFetchFast = Reflect.get(moduleObject, "fetchFast");
  return {
    fetch: typeof defaultFetch === "function" ? defaultFetch : topLevelFetch,
    fetchFast: typeof defaultFetchFast === "function" ? defaultFetchFast : topLevelFetchFast,
    rpc,
    ...(userDefault ? { userDefault } : {}),
  };
}
