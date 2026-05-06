// sdks/vite-plugin/src/server-graph.ts
//
// Reference-graph walk for RPC v2 server-binding discovery. The rules
// come from `docs/proposals/rpc-v2.md` §1.
//
// Walk the module graph from the client entry. For each imported
// binding, decide whether it is a server reference by checking three
// rules:
//
//   1. Its declaring file has a file-level `"use server"` directive.
//      Every export of that file is a server reference.
//   2. Its declaring binding is a function whose body opens with
//      `"use server"` (function-level directive). Only that name is
//      a server reference; the rest of the file stays client-side.
//   3. It is a re-export of (1) or (2). The chain is followed
//      transitively: `export { add } from "./todos"` propagates the
//      tag through arbitrary depth.
//
// The walker is parser-agnostic and only depends on the host's
// `resolveModule` + `loadSource` callbacks. The Vite plugin wires
// these to `this.resolve()` / `this.load()` / `fs.readFile` as
// appropriate; the test harness uses an in-memory file map.

import { parse as acornParse } from "acorn";

import {
  detectFileLevelUseServer,
  detectFunctionLevelUseServer,
} from "./transform.js";

/**
 * One server-binding entry produced by the graph walk. The map is
 * keyed by `<sourceFile>::<exportName>` so the synthetic-entry
 * generator can emit a stable per-target import.
 *
 * `marker` records WHY the binding is a server reference; the strict-
 * mode gate rejects `"graph"`-only bindings in production builds since
 * `docs/proposals/rpc-v2.md` §1 requires a directly-declared
 * directive.
 */
export interface ServerBinding {
  wireId: string;
  sourceFile: string;
  exportName: string;
  kind: "query" | "mutation" | "stream" | "subscription";
  marker: "file" | "function" | "graph";
  /** Re-export chain from client to target. Empty for direct imports. */
  chain: string[];
  /** When true, the synthetic entry emits a dynamic-import wrapper so
   *  the procedure's source module loads only on first call. Defaults
   *  to false (eager). Detected from `fn.config.lazy = true` or the
   *  wrapper's `lazy: true` option (Wave #188). The dynamic import
   *  rides V8's host callback (Wave #187) — second call to the same
   *  lazy procedure resolves to the cached namespace. */
  lazy?: boolean;
}

export interface WalkClientEntryOptions {
  /** Absolute path of the client entry to walk from. */
  clientEntry: string;
  /** Resolve a specifier (relative or bare) to an absolute path. */
  resolveModule: (specifier: string, importer: string) => Promise<string | null>;
  /** Read the source of a resolved module path. */
  loadSource: (id: string) => Promise<string>;
  /** Optional: known wireId per (file, name) — pulled from per-file
   *  `fn.config.id` resolution. The graph walk does not re-parse to
   *  find these; the caller hands them in. Default: bare exportName. */
  pinnedWireIds?: Map<string, string>;
  /** Optional: known kind per (file, name). Default: "mutation". */
  knownKinds?: Map<string, "query" | "mutation" | "stream" | "subscription">;
  /** Optional: per-(file, name) lazy flag. The graph walk records it
   *  on each binding so the synthetic-entry generator can branch.
   *  Default: false (eager). */
  lazyFlags?: Map<string, boolean>;
}

// ── AST helpers (oxc/estree shape) ────────────────────────────────────────

interface ParseResult {
  body: AnyNode[];
}
interface AnyNode {
  type: string;
  source?: { value?: string };
  declaration?: AnyNode;
  declarations?: Array<{
    id?: { type?: string; name?: string };
    init?: AnyNode;
  }>;
  specifiers?: Array<{
    type: string;
    local?: { name?: string };
    imported?: { name?: string };
    exported?: { name?: string };
  }>;
  id?: { name?: string };
  init?: AnyNode;
  body?: { type?: string; body?: AnyNode[] } | AnyNode[];
  generator?: boolean;
  async?: boolean;
}

function parse(code: string, isTsx: boolean): ParseResult {
  // Acorn doesn't natively understand TS; the upstream Vite/Rolldown
  // pipeline feeds us pre-stripped TS. Here in the graph walker we
  // skip files we can't parse — the `"use server"` directive must be
  // a top-level expression statement so even in the worst case where
  // type annotations confuse Acorn, the directive at body[0] is
  // unaffected (it's a leading string literal, identical in TS and JS).
  // _isTsx kept for signature symmetry with the transform; unused
  // until we swap in oxc.
  void isTsx;
  try {
    return acornParse(code, {
      ecmaVersion: 2024,
      sourceType: "module",
      allowImportExportEverywhere: true,
    }) as unknown as ParseResult;
  } catch {
    // Fall back to a stripped re-parse on failure (TS syntax). The
    // graph walker only needs imports / exports / directives — the
    // function bodies are opaque to it.
    const stripped = code
      .replace(/^\s*import\s+type\b[\s\S]+?;[\r\n]+/gm, "")
      .replace(/:\s*[A-Za-z_$][\w$<>,\s|&\[\]?]*(?=\s*[=,);])/g, "")
      .replace(/\sas\s+[A-Za-z_$][\w$<>,\s|&\[\]]*/g, "");
    try {
      return acornParse(stripped, {
        ecmaVersion: 2024,
        sourceType: "module",
        allowImportExportEverywhere: true,
      }) as unknown as ParseResult;
    } catch {
      return { body: [] };
    }
  }
}

// ── Per-file analysis ──────────────────────────────────────────────────────

interface FileAnalysis {
  /** File-level `"use server"` directive present. */
  fileLevel: boolean;
  /** Set of function-level `"use server"` names. */
  functionLevel: Set<string>;
  /** Imports: localName -> { source, importedName | null (default) | "*" (namespace) }. */
  imports: Map<string, { source: string; imported: string | null | "*" }>;
  /** Direct named exports declared in this file (functions, vars). */
  ownExports: Set<string>;
  /** Re-exports: localExportName -> { source, importedName | null (default) | "*" }. */
  reExports: Map<string, { source: string; imported: string | null | "*" }>;
  /** `export *` star re-exports. */
  starReExports: Set<string>;
}

function analyzeFile(code: string, isTsx: boolean): FileAnalysis {
  const ast = parse(code, isTsx);
  const fileLevel = detectFileLevelUseServer(ast);
  const functionLevel = detectFunctionLevelUseServer(ast);
  const imports = new Map<string, { source: string; imported: string | null | "*" }>();
  const ownExports = new Set<string>();
  const reExports = new Map<string, { source: string; imported: string | null | "*" }>();
  const starReExports = new Set<string>();

  for (const node of ast.body) {
    if (node.type === "ImportDeclaration") {
      const src = node.source?.value;
      if (typeof src !== "string") continue;
      for (const spec of node.specifiers ?? []) {
        const local = spec.local?.name;
        if (!local) continue;
        if (spec.type === "ImportSpecifier") {
          imports.set(local, { source: src, imported: spec.imported?.name ?? null });
        } else if (spec.type === "ImportDefaultSpecifier") {
          imports.set(local, { source: src, imported: null });
        } else if (spec.type === "ImportNamespaceSpecifier") {
          imports.set(local, { source: src, imported: "*" });
        }
      }
      continue;
    }

    if (node.type === "ExportNamedDeclaration") {
      const src = node.source?.value;
      const decl = node.declaration as AnyNode | undefined;
      // Direct: `export function name() {}` / `export const name = ...`.
      if (decl && !src) {
        if (decl.type === "FunctionDeclaration" && decl.id?.name) {
          ownExports.add(decl.id.name);
        } else if (decl.type === "VariableDeclaration") {
          for (const d of decl.declarations ?? []) {
            if (d.id?.type === "Identifier" && d.id.name) ownExports.add(d.id.name);
          }
        } else if (decl.type === "ClassDeclaration" && decl.id?.name) {
          ownExports.add(decl.id.name);
        }
        continue;
      }
      // Re-export: `export { x } from "./y"` OR alias `export { x }`.
      //
      // ESTree shape per acorn/oxc:
      //   `export { foo as bar } from "./y"`:
      //     spec.local    = { name: "foo" }    // source-side name
      //     spec.exported = { name: "bar" }    // alias seen by importer
      //   `export { foo } from "./y"`:
      //     spec.local = { name: "foo" }
      //     spec.exported = { name: "foo" }
      //   `export { foo as bar }`:
      //     spec.local    = { name: "foo" }    // local binding
      //     spec.exported = { name: "bar" }
      for (const spec of node.specifiers ?? []) {
        const exported = spec.exported?.name;
        // Source-side name: prefer `local` (ESTree-correct); fall back
        // to `imported` if a future parser flips the shape.
        const sourceName = spec.local?.name ?? spec.imported?.name;
        if (!exported) continue;
        if (typeof src === "string") {
          reExports.set(exported, {
            source: src,
            imported: sourceName === "default" ? null : sourceName ?? exported,
          });
        } else {
          // `export { x as y }` — alias of a local binding. Tag the
          // alias name as an own export when the local is one.
          if (sourceName && ownExports.has(sourceName)) {
            ownExports.add(exported);
          }
        }
      }
      continue;
    }

    if (node.type === "ExportAllDeclaration") {
      const src = node.source?.value;
      if (typeof src === "string") starReExports.add(src);
      continue;
    }

    if (node.type === "ExportDefaultDeclaration") {
      ownExports.add("default");
      continue;
    }
  }

  return { fileLevel, functionLevel, imports, ownExports, reExports, starReExports };
}

// ── Graph walk ─────────────────────────────────────────────────────────────

interface ResolvedExport {
  /** Final declaring file (after re-export tracing). */
  sourceFile: string;
  /** Final export name in the declaring file. */
  exportName: string;
  /** Re-export chain from the original importer to the target. */
  chain: string[];
}

/**
 * Resolve a `(file, exportName)` pair to its ultimate declaring
 * `(sourceFile, exportName)` by following re-export chains. Returns
 * null when the chain dead-ends (unresolved bare specifier) or hits
 * a cycle.
 */
async function resolveExport(
  file: string,
  exportName: string,
  opts: WalkClientEntryOptions,
  analyses: Map<string, FileAnalysis>,
  visited: Set<string>,
): Promise<ResolvedExport | null> {
  const key = `${file}::${exportName}`;
  if (visited.has(key)) return null;
  visited.add(key);

  const a = await ensureAnalysis(file, opts, analyses);
  if (!a) return null;

  // Direct own export — done.
  if (a.ownExports.has(exportName)) {
    return { sourceFile: file, exportName, chain: [file] };
  }

  // `export { foo } from "./x"` — follow.
  const re = a.reExports.get(exportName);
  if (re) {
    const resolved = await opts.resolveModule(re.source, file);
    if (!resolved) return null;
    const targetName = re.imported ?? exportName;
    if (targetName === "*") {
      // Namespace re-export: the binding IS the namespace; treat as
      // own export at the re-source. Rare; not server-fn-friendly.
      return { sourceFile: resolved, exportName: "*", chain: [file, resolved] };
    }
    const inner = await resolveExport(resolved, targetName, opts, analyses, visited);
    if (!inner) return null;
    return { ...inner, chain: [file, ...inner.chain] };
  }

  // `export * from "./x"` — search every star source.
  for (const star of a.starReExports) {
    const resolved = await opts.resolveModule(star, file);
    if (!resolved) continue;
    const inner = await resolveExport(resolved, exportName, opts, analyses, visited);
    if (inner) return { ...inner, chain: [file, ...inner.chain] };
  }

  return null;
}

async function ensureAnalysis(
  file: string,
  opts: WalkClientEntryOptions,
  analyses: Map<string, FileAnalysis>,
): Promise<FileAnalysis | null> {
  const cached = analyses.get(file);
  if (cached) return cached;
  let src: string;
  try {
    src = await opts.loadSource(file);
  } catch {
    return null;
  }
  const isTsx = file.endsWith(".tsx") || file.endsWith(".jsx");
  const a = analyzeFile(src, isTsx);
  analyses.set(file, a);
  return a;
}

/**
 * Walk the module graph from the client entry, identifying server
 * bindings per the rules in `docs/proposals/rpc-v2.md` §1.
 *
 * The output map is keyed by `<sourceFile>::<exportName>` (the final
 * declaring location, not the importer). The synthetic-entry
 * generator deduplicates on this key when it emits per-target
 * `import { x } from "<sourceFile>"` lines.
 */
export async function walkClientEntry(
  opts: WalkClientEntryOptions,
): Promise<Map<string, ServerBinding>> {
  const out = new Map<string, ServerBinding>();
  const analyses = new Map<string, FileAnalysis>();
  const visiting = new Set<string>();

  async function visit(file: string): Promise<void> {
    if (visiting.has(file)) return;
    visiting.add(file);

    const a = await ensureAnalysis(file, opts, analyses);
    if (!a) return;

    // Walk every import edge. For each named/default specifier, follow
    // the re-export chain to the declaring file and decide whether the
    // binding is a server reference.
    for (const [, spec] of a.imports) {
      // Namespace / default imports rarely surface server fns at the
      // edge — skip namespace; treat default imports as nullable.
      if (spec.imported === "*") continue;

      const resolved = await opts.resolveModule(spec.source, file);
      if (!resolved) continue;

      // Recurse into the target so its own imports are explored too.
      await visit(resolved);

      const targetName = spec.imported;
      if (!targetName) continue; // default imports — not a server fn

      // Resolve through any re-export chain.
      const r = await resolveExport(
        resolved,
        targetName,
        opts,
        analyses,
        new Set(),
      );
      if (!r) continue;

      const targetAnalysis = await ensureAnalysis(r.sourceFile, opts, analyses);
      if (!targetAnalysis) continue;

      // Decide marker.
      //   "file"     — target file carries a file-level directive.
      //   "function" — target's function-level directive matches the name.
      //   "graph"    — at least one file in the re-export chain has a
      //                directive but the final target does not. The
      //                strict-mode gate rejects this in production.
      let marker: ServerBinding["marker"] | null = null;
      if (targetAnalysis.fileLevel) marker = "file";
      else if (targetAnalysis.functionLevel.has(r.exportName)) marker = "function";
      else {
        // Walk the chain: any intermediate file with a directive flags
        // this binding as graph-detected.
        let chainHadDirective = false;
        for (const intermediate of r.chain) {
          if (intermediate === r.sourceFile) continue;
          const ia = await ensureAnalysis(intermediate, opts, analyses);
          if (!ia) continue;
          if (ia.fileLevel || ia.functionLevel.size > 0) {
            chainHadDirective = true;
            break;
          }
        }
        // Direct import from a marked importer also qualifies.
        if (chainHadDirective || a.fileLevel || a.functionLevel.has(targetName)) {
          marker = "graph";
        }
      }

      if (marker === null) continue;

      const key = `${r.sourceFile}::${r.exportName}`;
      if (out.has(key)) continue;

      const wireId =
        opts.pinnedWireIds?.get(key) ?? r.exportName;
      const kind = opts.knownKinds?.get(key) ?? "mutation";
      const lazy = opts.lazyFlags?.get(key) === true;

      out.set(key, {
        wireId,
        sourceFile: r.sourceFile,
        exportName: r.exportName,
        kind,
        marker,
        chain: r.chain,
        ...(lazy ? { lazy: true } : {}),
      });
    }

    // Files with a file-level directive that the client itself opened
    // (e.g., the server entry being walked) — record their exports as
    // file-level bindings, even when they're never imported via a named
    // edge in the entry.
    if (a.fileLevel) {
      for (const name of a.ownExports) {
        if (name === "default") continue;
        const key = `${file}::${name}`;
        if (out.has(key)) continue;
        const wireId = opts.pinnedWireIds?.get(key) ?? name;
        const kind = opts.knownKinds?.get(key) ?? "mutation";
        const lazy = opts.lazyFlags?.get(key) === true;
        out.set(key, {
          wireId,
          sourceFile: file,
          exportName: name,
          kind,
          marker: "file",
          chain: [file],
          ...(lazy ? { lazy: true } : {}),
        });
      }
    }
    if (a.functionLevel.size > 0) {
      for (const name of a.functionLevel) {
        const key = `${file}::${name}`;
        if (out.has(key)) continue;
        const wireId = opts.pinnedWireIds?.get(key) ?? name;
        const kind = opts.knownKinds?.get(key) ?? "mutation";
        const lazy = opts.lazyFlags?.get(key) === true;
        out.set(key, {
          wireId,
          sourceFile: file,
          exportName: name,
          kind,
          marker: "function",
          chain: [file],
          ...(lazy ? { lazy: true } : {}),
        });
      }
    }
  }

  await visit(opts.clientEntry);
  return out;
}

// ── Strict-mode gate ───────────────────────────────────────────────────────

/**
 * Strict-mode gate from `docs/proposals/rpc-v2.md` §1 ("Strict mode
 * (production builds)").
 * Every server binding must have a directly-declared `"use server"`
 * marker — file-level or function-level. Graph-only bindings are a
 * build error in strict mode.
 *
 * The thrown message instructs the user to add the directive at file
 * or function level on the target.
 */
export function strictModeGate(
  bindings: Map<string, ServerBinding>,
  mode: "always" | "never",
): void {
  if (mode === "never") return;
  const offenders: ServerBinding[] = [];
  for (const b of bindings.values()) {
    if (b.marker === "graph") offenders.push(b);
  }
  if (offenders.length === 0) return;
  const lines = offenders.map(
    (b) =>
      `  - ${JSON.stringify(b.exportName)} reachable as a server function via the reference graph` +
      ` but lacks a "use server" directive.\n` +
      `    Declared in: ${b.sourceFile}\n` +
      `    Add the directive at function or file level for production builds.`,
  );
  throw new Error(
    `[zeroship:rpc] strict-mode gate failed:\n` + lines.join("\n"),
  );
}

// ── WireId collision check ─────────────────────────────────────────────────

/**
 * Collision detection across the assigned wireIds. Two distinct
 * bindings resolving to the same wireId is an unrecoverable build
 * error. Same shape as the manifest-side check in `manifest.ts`.
 */
export function checkWireIdCollisions(
  bindings: Map<string, ServerBinding>,
): void {
  const byWire = new Map<string, ServerBinding[]>();
  for (const b of bindings.values()) {
    const arr = byWire.get(b.wireId);
    if (arr) arr.push(b);
    else byWire.set(b.wireId, [b]);
  }
  const collisions: string[] = [];
  for (const [wireId, group] of byWire) {
    if (group.length < 2) continue;
    const lines = group.map(
      (b) => `    ${b.sourceFile} (export ${JSON.stringify(b.exportName)})`,
    );
    collisions.push(
      `wireId collision: ${JSON.stringify(wireId)} is the default for multiple procedures:\n${lines.join("\n")}\n` +
        `    Pin an explicit \`fn.config.id\` on at least one of them.`,
    );
  }
  if (collisions.length > 0) {
    throw new Error(`[zeroship:rpc] ` + collisions.join("\n"));
  }
}
