// The synthetic entry supplies procedure references to native dispatch.
// Explicit bindings select their declaring modules; otherwise the entry
// normalizes the user's declared RPC dictionary. Named creator exports are
// forwarded unchanged for runtime-owned consumers such as workflow replay.

import type { Plugin } from "vite";
import { createHash } from "node:crypto";
import type { TransformState } from "./transform.js";

/** A discovered procedure, keyed by <sourceFile>::<exportName> in the binding map. */
export interface ServerBinding {
  wireId: string;
  sourceFile: string;
  exportName: string;
  kind: "query" | "mutation" | "action" | "stream" | "subscription";
  /** Return the actual procedure through a dynamic import when Rust requests it. */
  lazy?: boolean;
}

export interface ServerBindingSnapshot {
  version: string;
  bindings: ServerBinding[];
}

export const SERVER_ENTRY_VIRTUAL_ID = "virtual:zeroship/_server-entry";
export const SERVER_ENTRY_RESOLVED_ID = "\0" + SERVER_ENTRY_VIRTUAL_ID;

/** Use the same public name as the manifest's pickWireId. */
export function pickEntryWireId(p: {
  exportName: string;
  config?: Record<string, unknown>;
}): string {
  const explicit = p.config?.id;
  return typeof explicit === "string" && explicit.length > 0 ? explicit : p.exportName;
}

/** Convert the transform's current discovery records into entry imports. */
export function serverBindingsFromState(
  state: Pick<TransformState, "discoveredProcedures">,
): Map<string, ServerBinding> {
  const bindings = new Map<string, ServerBinding>();
  for (const procedure of state.discoveredProcedures) {
    const key = `${procedure.filePath}::${procedure.exportName}`;
    bindings.set(key, {
      wireId: pickEntryWireId(procedure),
      sourceFile: procedure.filePath,
      exportName: procedure.exportName,
      kind: procedure.kind,
      ...(procedure.lazy ? { lazy: true } : {}),
    });
  }
  return bindings;
}

/** Produce the stable host response consumed by the development loader. */
export function serverBindingSnapshotFromState(
  state: Pick<TransformState, "discoveredProcedures">,
): ServerBindingSnapshot {
  const bindings = sortedServerBindings(state);
  const owners = new Map<string, ServerBinding>();
  for (const binding of bindings) {
    const previous = owners.get(binding.wireId);
    if (previous) {
      throw new Error(
        `duplicate procedure id ${JSON.stringify(binding.wireId)}: ` +
        `${previous.sourceFile}::${previous.exportName} and ` +
        `${binding.sourceFile}::${binding.exportName}`,
      );
    }
    owners.set(binding.wireId, binding);
  }
  return {
    version: hashServerBindings(bindings),
    bindings,
  };
}

/** Track binding changes even while an invalid duplicate set cannot load. */
export function serverBindingVersionFromState(
  state: Pick<TransformState, "discoveredProcedures">,
): string {
  return hashServerBindings(sortedServerBindings(state));
}

function sortedServerBindings(
  state: Pick<TransformState, "discoveredProcedures">,
): ServerBinding[] {
  return [...serverBindingsFromState(state).values()].sort((left, right) =>
    left.sourceFile.localeCompare(right.sourceFile) ||
    left.exportName.localeCompare(right.exportName)
  );
}

function hashServerBindings(bindings: readonly ServerBinding[]): string {
  return createHash("sha256").update(JSON.stringify(bindings)).digest("hex");
}

// These statements only normalize exports. The host owns schema preparation,
// RPC invocation, validation, capability frames and response framing.
const normalizeDefault = `
const _zsUserDefaultExport = Reflect.get(_zsUser, "default");
const _zsUserDefault = (_zsUserDefaultExport && typeof _zsUserDefaultExport === "object")
  ? _zsUserDefaultExport
  : {};
const _zsDeclaredRpc = _zsUserDefault.rpc;
if (_zsDeclaredRpc != null && (typeof _zsDeclaredRpc !== "object" || Array.isArray(_zsDeclaredRpc))) {
  throw new TypeError("default.rpc must be a procedure dictionary");
}
const _zsRpc = Object.create(null);
if (_zsDeclaredRpc != null) {
  for (const _zsId of Object.getOwnPropertyNames(_zsDeclaredRpc)) {
    _zsRpc[_zsId] = _zsDeclaredRpc[_zsId];
  }
}
`;

const exportEntry = `
const _zsUserFetch = _zsUserDefault.fetch;
const _zsTopLevelFetch = Reflect.get(_zsUser, "fetch");
const _zsFetch = typeof _zsUserFetch === "function"
  ? Function.prototype.bind.call(_zsUserFetch, _zsUserDefault)
  : (typeof _zsTopLevelFetch === "function" ? _zsTopLevelFetch : undefined);

export default {
  fetch: _zsFetch,
  rpc: _zsRpc,
};
`;

/**
 * Build a module exporting { fetch?, rpc }. RPC values are actual
 * procedures or { load: () => Promise<Procedure> } records. The native runtime
 * retains their metadata and dispatches requests. Default fetch keeps its
 * original receiver through a bound function. Named creator exports pass
 * through without framework classification.
 */
export function buildServerEntrySource(opts: {
  userEntryRel: string;
  /** Explicit bindings take precedence over default.rpc. An empty map uses named exports. */
  bindings?: Map<string, ServerBinding>;
}): string {
  const userImport = JSON.stringify(opts.userEntryRel);
  if (opts.bindings && opts.bindings.size > 0) {
    return buildBindingEntry(userImport, opts.bindings);
  }

  return `// Generated by @zeroship/vite-plugin. Dispatch is supplied by the runtime.
import * as _zsUser from ${userImport};
export * from ${userImport};
${normalizeDefault}
${exportEntry}`;
}

function buildBindingEntry(
  userImport: string,
  bindings: Map<string, ServerBinding>,
): string {
  const byFile = new Map<string, ServerBinding[]>();
  for (const binding of bindings.values()) {
    const entries = byFile.get(binding.sourceFile);
    if (entries) entries.push(binding);
    else byFile.set(binding.sourceFile, [binding]);
  }
  const files = [...byFile.keys()].sort();
  const eagerFiles = files.filter((file) => byFile.get(file)!.some((binding) => !binding.lazy));
  const aliases = new Map(eagerFiles.map((file, index) => [file, `_zsTarget${index}`]));
  const imports = eagerFiles.map((file) =>
    `import * as ${aliases.get(file)} from ${JSON.stringify(file)};`,
  );

  const assignments: string[] = [];
  for (const file of files) {
    for (const binding of byFile.get(file)!) {
      const exported = JSON.stringify(binding.exportName);
      const target = binding.lazy
        ? `{ load: async () => (await import(${JSON.stringify(file)}))[${exported}] }`
        : `${aliases.get(file)}[${exported}]`;
      assignments.push(`_zsRpc[${JSON.stringify(binding.wireId)}] = ${target};`);
    }
  }

  return `// Generated by @zeroship/vite-plugin. Dispatch is supplied by the runtime.
import * as _zsUser from ${userImport};
export * from ${userImport};
${imports.join("\n")}
${normalizeDefault}
${assignments.join("\n")}
${exportEntry}`;
}

/** Resolve the host's synthetic entry and generate it from the current bindings. */
export function rpcRegistryPlugin(opts: {
  root?: string;
  userEntryRel: string;
  state?: TransformState;
  getBindings?: () => Map<string, ServerBinding> | undefined;
}): Plugin {
  return {
    name: "zeroship:server-entry",
    enforce: "pre",
    resolveId(id: string) {
      return id === SERVER_ENTRY_VIRTUAL_ID ? SERVER_ENTRY_RESOLVED_ID : null;
    },
    load(id: string) {
      if (id !== SERVER_ENTRY_RESOLVED_ID) return null;
      return buildServerEntrySource({
        userEntryRel: opts.userEntryRel,
        bindings: opts.getBindings?.() ?? (
          opts.state ? serverBindingsFromState(opts.state) : undefined
        ),
      });
    },
  };
}
