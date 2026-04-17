import type { Plugin } from "vite";
import { createRequire } from "module";
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, relative, extname, dirname } from "node:path";
import MagicString from "magic-string";
import { DEFAULT_RPC_ENDPOINT } from "./constants.js";

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

/** Generate an RPC stub for a function name */
function makeStub(name: string, rpcEndpoint: string): string {
  return `export async function ${name}(...args) {
  const res = await fetch(${JSON.stringify(rpcEndpoint)}, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", method: ${JSON.stringify(name)}, params: args, id: Date.now() })
  });
  const json = await res.json();
  if (json.error) throw new Error(json.error.message || "RPC error");
  return json.result;
}`;
}

// No removeFunction / removeBraceBlock — we use AST node positions directly.
// The transform handler collects AST nodes with start/end and removes them
// via MagicString, which gives correct source maps for free.

export function transformPlugin(rpcEndpoint: string = DEFAULT_RPC_ENDPOINT, state: TransformState): Plugin {
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
        // Skip transform for the zeroship environment — server code should
        // run as-is in V8. Only transform for client (replace with RPC stubs).
        if (this.environment?.name === "zeroship") return null;

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

        // 5. Find server functions — collect AST nodes with positions
        //
        // Unlike the previous regex-based approach, we use the AST node's
        // start/end positions directly. This handles destructured params,
        // default values with parens, arrow function exports, and every
        // other syntax the regex couldn't parse. MagicString removes the
        // exact ranges, giving correct source maps for free.
        interface ServerFn { name: string; node: any; }
        const serverFns: ServerFn[] = [];

        for (const node of ast.body) {
          if (node.type !== "ExportNamedDeclaration") continue;
          const decl = node.declaration;
          if (!decl) continue;

          // export function name() { ... }
          // export async function name() { ... }
          if (decl.type === "FunctionDeclaration" && decl.id?.name) {
            const name = decl.id.name;
            if (isFileServer || hasFnDirective(decl, "use server") || fnReferencesAny(decl, tainted)) {
              serverFns.push({ name, node });
            }
          }

          // export const name = () => { ... }
          // export const name = async function() { ... }
          // export const name = createServerFn(...)
          if (decl.type === "VariableDeclaration") {
            for (const d of decl.declarations || []) {
              const name = d.id?.name;
              if (!name || !d.init) continue;
              if (isFileServer || fnReferencesAny(d.init, tainted)) {
                serverFns.push({ name, node });
                break; // one removal per VariableDeclaration node
              }
            }
          }
        }

        if (serverFns.length === 0) return null;

        const names = serverFns.map((f) => f.name);

        // 6. Track for build report + export signature tracking
        serverFunctionMap.set(relative(root, id), new Set(names));

        // 7. Transform to client code using MagicString (AST positions)
        const s = new MagicString(code);

        if (isFileServer) {
          // Replace ENTIRE file with stubs
          s.overwrite(0, code.length, names.map((n) => makeStub(n, rpcEndpoint)).join("\n\n") + "\n");
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

        // Append RPC stubs
        s.append("\n\n" + names.map((n) => makeStub(n, rpcEndpoint)).join("\n\n") + "\n");

        return {
          code: s.toString(),
          map: s.generateMap({ source: id, includeContent: true, hires: true }),
        };
      },
    },
  };
}
