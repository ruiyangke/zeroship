import type { Plugin } from "vite";
import { createRequire } from "module";
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, relative, extname, dirname } from "node:path";
import MagicString from "magic-string";

export interface TransformState {
  serverModuleCache: Map<string, boolean>;
  serverFunctionMap: Map<string, Set<string>>;
  knownServerSources: Set<string>;
}

// --- AST helpers (work with Rolldown's Oxc/ESTree AST) ---

/** Check if a function body starts with a directive */
function hasFnDirective(fn: any, directive: string): boolean {
  const stmts = fn.body?.body;
  if (!stmts || stmts.length === 0) return false;
  const first = stmts[0];
  return first.type === "ExpressionStatement"
    && first.expression?.type === "Literal"
    && first.expression.value === directive;
}

/** Check if an AST node references any of the given identifiers (AST walk). */
function fnReferencesAny(node: any, identifiers: Set<string>): boolean {
  if (!node || typeof node !== "object") return false;
  if (node.type === "Identifier" && identifiers.has(node.name)) return true;
  if (node.type === "MemberExpression" && fnReferencesAny(node.object, identifiers)) return true;
  for (const key of Object.keys(node)) {
    if (key === "type" || key === "start" || key === "end") continue;
    const child = node[key];
    if (Array.isArray(child)) {
      for (const item of child) {
        if (fnReferencesAny(item, identifiers)) return true;
      }
    } else if (child && typeof child === "object" && child.type) {
      if (fnReferencesAny(child, identifiers)) return true;
    }
  }
  return false;
}

/** Check if a file's first non-comment line is "use server" */
function isServerFile(filePath: string, serverModuleCache: Map<string, boolean>): boolean {
  if (serverModuleCache.has(filePath)) return serverModuleCache.get(filePath)!;
  try {
    const code = readFileSync(filePath, "utf-8");
    const result = checkDirective(code);
    serverModuleCache.set(filePath, result);
    return result;
  } catch {
    serverModuleCache.set(filePath, false);
    return false;
  }
}

/** Check if code starts with a directive string */
function checkDirective(code: string): boolean {
  let i = 0;
  const lines = code.split("\n");
  while (i < lines.length) {
    const t = lines[i].trim();
    if (t === "" || t.startsWith("//")) { i++; continue; }
    if (t.startsWith("/*")) {
      while (i < lines.length && !lines[i].includes("*/")) i++;
      i++;
      continue;
    }
    return t === '"use server"' || t === "'use server'" || t === '"use server";' || t === "'use server';";
  }
  return false;
}

/** Resolve a package specifier to its entry file and check for "use server" */
function isServerPackage(specifier: string, root: string, cache: Map<string, boolean>): boolean {
  const cached = cache.get(specifier);
  if (cached !== undefined) return cached;

  try {
    const require = createRequire(resolve(root, "package.json"));
    const pkgJsonPath = require.resolve(`${specifier}/package.json`);
    const pkg = JSON.parse(readFileSync(pkgJsonPath, "utf-8"));

    const entry =
      pkg.exports?.["."]?.import ??
      pkg.exports?.["."]?.default ??
      pkg.module ??
      pkg.main ??
      "index.js";

    const entryPath = resolve(dirname(pkgJsonPath), entry);
    const isServer = existsSync(entryPath) && checkDirective(readFileSync(entryPath, "utf-8"));
    cache.set(specifier, isServer);
    return isServer;
  } catch {
    cache.set(specifier, false);
    return false;
  }
}

/** Scan @zeroship/* packages for "use server" */
function discoverServerPackages(root: string, serverModuleCache: Map<string, boolean>, knownServerSources: Set<string>): void {
  const scopeDir = resolve(root, "node_modules", "@zeroship");
  if (!existsSync(scopeDir)) return;
  try {
    for (const pkg of readdirSync(scopeDir)) {
      const spec = `@zeroship/${pkg}`;
      if (isServerPackage(spec, root, serverModuleCache)) knownServerSources.add(spec);
    }
  } catch { /* ignore */ }
}

/**
 * Compute the URL-path method name for an export.
 *
 * Method names are rooted at the project root with the extension stripped:
 *   /abs/project/src/api/users.ts  →  "src/api/users"
 *
 * Per-export method is `<relPathNoExt>/<exportName>`. Paths keep forward
 * slashes so the wire (`POST /_rpc/src/api/users/getUser`) matches the
 * registry key `src/api/users/getUser` on both ends. No collisions
 * because file path is part of the key.
 */
function moduleBaseName(root: string, id: string): string {
  const rel = relative(root, id).replace(/\\/g, "/");
  const ext = extname(rel);
  return ext ? rel.slice(0, -ext.length) : rel;
}

/** Shared runtime: emitted once per client bundle. Minimal, no deps. */
const CLIENT_HELPERS = `
async function __rpcUnary(name, args) {
  const r = await fetch("/_rpc/" + name, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(args),
  });
  if (!r.ok) {
    let msg = r.statusText;
    try { msg = (await r.json()).message || msg; } catch {}
    throw new Error(msg);
  }
  return r.json();
}
async function* __rpcStream(name, args) {
  const r = await fetch("/_rpc/" + name, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(args),
  });
  if (!r.ok) {
    let msg = r.statusText;
    try { msg = (await r.json()).message || msg; } catch {}
    throw new Error(msg);
  }
  const reader = r.body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let idx;
    while ((idx = buf.indexOf("\\n\\n")) !== -1) {
      const frame = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      const evMatch = frame.match(/^event: (.*)$/m);
      const dataMatch = frame.match(/^data: (.*)$/s);
      if (!evMatch || !dataMatch) continue;
      const ev = evMatch[1];
      const data = dataMatch[1];
      const parsed = JSON.parse(data);
      if (ev === "yield") yield parsed;
      else if (ev === "error") throw new Error(parsed.message || "stream error");
      else if (ev === "return") return parsed;
    }
  }
}
`.trim();

/** Client stub for a non-streaming export */
function clientUnaryStub(name: string, methodName: string): string {
  return `export const ${name} = (...args) => __rpcUnary(${JSON.stringify(methodName)}, args);`;
}

/** Client stub for a streaming (async generator) export */
function clientStreamStub(name: string, methodName: string): string {
  return `export const ${name} = (...args) => __rpcStream(${JSON.stringify(methodName)}, args);`;
}

export function transformPlugin(_rpcEndpoint: string, state: TransformState): Plugin {
  const { serverModuleCache, serverFunctionMap, knownServerSources } = state;
  let root = "";

  return {
    name: "zeroship:transform",
    enforce: "pre" as const,

    configResolved(resolvedConfig: any) {
      root = resolvedConfig.root;
      discoverServerPackages(root, serverModuleCache, knownServerSources);
    },

    transform: {
      filter: {
        id: { include: /\.(ts|tsx|js|jsx)$/, exclude: /node_modules/ },
      },
      handler(this: any, code: string, id: string) {
        // Server environment detection:
        //  - dev: `this.environment.name === "zeroship"` (dev-server creates it)
        //  - prod build: Vite's `ssr: entry` build runs in environment `ssr`
        //    (rolldown sets `this.environment.name === "ssr"`). We treat both
        //    as the server side.
        //  - prod client build: environment name is `"client"`.
        const envName = this.environment?.name;
        const isServerEnv = envName === "zeroship" || envName === "ssr";

        // 1. Parse AST with Rolldown's built-in parser
        const isTsx = id.endsWith(".tsx") || id.endsWith(".jsx");
        const ast = this.parse(code, { lang: isTsx ? "tsx" : "ts" });

        // 2. Check file-level "use server"
        let isFileServer = false;
        if (ast.body.length > 0) {
          const first = ast.body[0];
          if (
            first.type === "ExpressionStatement" &&
            first.expression?.type === "Literal" &&
            first.expression.value === "use server"
          ) {
            isFileServer = true;
          }
        }

        // 3. Collect imports and build taint set
        const tainted = new Set<string>();

        for (const node of ast.body) {
          if (node.type === "ImportDeclaration") {
            const src = node.source?.value;
            if (!src) continue;

            const isServer = knownServerSources.has(src)
              || (src.startsWith("./") || src.startsWith("../"))
                && isServerFile(resolve(id, "..", src.replace(/\.(ts|tsx|js|jsx)$/, "") + extname(id)), serverModuleCache);

            if (isServer) {
              for (const spec of node.specifiers || []) {
                const name = spec.local?.name;
                if (name) tainted.add(name);
              }
            }
          }
        }

        // 4. Propagate taint: const x = taintedFn(...) → x tainted
        for (const node of ast.body) {
          if (node.type === "VariableDeclaration") {
            for (const decl of node.declarations || []) {
              if (decl.id?.name && decl.init) {
                const callee =
                  decl.init.type === "CallExpression" && decl.init.callee?.name
                    ? decl.init.callee.name
                    : decl.init.type === "Identifier"
                      ? decl.init.name
                      : null;
                if (callee && tainted.has(callee)) {
                  tainted.add(decl.id.name);
                }
              }
            }
          }
        }

        // 5. Find server functions — collect AST nodes with positions +
        //    flag async generators separately (their client stubs differ).
        interface ServerFn {
          name: string;
          node: any;
          isStream: boolean;
        }
        const serverFns: ServerFn[] = [];

        for (const node of ast.body) {
          if (node.type !== "ExportNamedDeclaration") continue;
          const decl = node.declaration;
          if (!decl) continue;

          // export function name() { ... }
          // export async function name() { ... }
          // export async function* name() { ... }
          if (decl.type === "FunctionDeclaration" && decl.id?.name) {
            const name = decl.id.name;
            if (isFileServer || hasFnDirective(decl, "use server") || fnReferencesAny(decl, tainted)) {
              serverFns.push({ name, node, isStream: !!decl.generator });
            }
          }

          // export const name = () => { ... }
          // export const name = async function() { ... }
          // export const name = async function*() { ... }
          // export const name = createServerFn(...)
          if (decl.type === "VariableDeclaration") {
            for (const d of decl.declarations || []) {
              const name = d.id?.name;
              if (!name || !d.init) continue;
              if (isFileServer || fnReferencesAny(d.init, tainted)) {
                // Only arrow/function-expression initializers can be generators;
                // other initializers (calls like `createServerFn(...)`) can't
                // reliably be inspected for generator-ness. Default to unary.
                const isStream =
                  (d.init.type === "FunctionExpression" && !!d.init.generator) ||
                  (d.init.type === "ArrowFunctionExpression" && !!d.init.generator);
                serverFns.push({ name, node, isStream });
                break; // one removal per VariableDeclaration node
              }
            }
          }
        }

        if (serverFns.length === 0) return null;

        const names = serverFns.map((f) => f.name);
        const modPath = moduleBaseName(root, id);

        // 6. Track for build report + export signature tracking
        serverFunctionMap.set(relative(root, id), new Set(names));

        // --- SERVER ENVIRONMENT ---------------------------------------------
        //
        // Append `__register(methodName, fn)` side effects so the V8 runtime
        // registry can resolve the URL-path-style method name to the export.
        // Keep all original exports (including `onRequest`, tainted helpers,
        // imports) untouched — only add registrations at the bottom of the
        // module. The transform is a superset of the source, never a
        // rewrite of the bodies.
        if (isServerEnv) {
          const s = new MagicString(code);
          const registrations = serverFns
            .map((fn) => {
              const methodName = `${modPath}/${fn.name}`;
              return `__register(${JSON.stringify(methodName)}, ${fn.name});`;
            })
            .join("\n");
          s.append(`\n\n// zeroship: register server functions for URL-path RPC\n${registrations}\n`);
          return {
            code: s.toString(),
            map: s.generateMap({ source: id, includeContent: true, hires: true }),
          };
        }

        // --- CLIENT ENVIRONMENT ---------------------------------------------
        //
        // Emit stubs that call the URL-path-based RPC wire. Unary exports
        // (plain async functions, regular functions) become `__rpcUnary`;
        // async generators become `__rpcStream`. The stubs live in the
        // client bundle; the actual implementation lives on the server
        // and is invoked over HTTP.
        const s = new MagicString(code);

        const stubs = serverFns.map((fn) => {
          const methodName = `${modPath}/${fn.name}`;
          return fn.isStream
            ? clientStreamStub(fn.name, methodName)
            : clientUnaryStub(fn.name, methodName);
        });

        if (isFileServer) {
          // Replace ENTIRE file with stubs + helpers
          s.overwrite(0, code.length, CLIENT_HELPERS + "\n\n" + stubs.join("\n") + "\n");
          return {
            code: s.toString(),
            map: s.generateMap({ source: id, includeContent: true, hires: true }),
          };
        }

        // Mixed file: remove server pieces, keep client code, append stubs

        // Remove "use server" directive (first statement if it's a string literal)
        if (ast.body[0]?.type === "ExpressionStatement" && ast.body[0].expression?.value === "use server") {
          s.remove(ast.body[0].start, ast.body[0].end);
        }

        // Remove each server function's export node (uses AST positions — no regex)
        for (const { node } of serverFns) {
          s.remove(node.start, node.end);
        }

        // Remove server-only imports (by AST position, not regex)
        for (const node of ast.body) {
          if (node.type !== "ImportDeclaration") continue;
          const src = node.source?.value;
          if (src && knownServerSources.has(src)) {
            s.remove(node.start, node.end);
          }
        }

        // Remove tainted variable declarations (by AST position)
        for (const node of ast.body) {
          if (node.type !== "VariableDeclaration") continue;
          for (const d of node.declarations || []) {
            if (d.id?.name && tainted.has(d.id.name)) {
              s.remove(node.start, node.end);
              break;
            }
          }
        }

        // Append helpers + RPC stubs
        s.append("\n\n" + CLIENT_HELPERS + "\n\n" + stubs.join("\n") + "\n");

        return {
          code: s.toString(),
          map: s.generateMap({ source: id, includeContent: true, hires: true }),
        };
      },
    },
  };
}
