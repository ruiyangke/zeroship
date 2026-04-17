import type { Plugin } from "vite";
import { createRequire } from "module";
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, relative, extname, dirname } from "node:path";
import { DEFAULT_RPC_ENDPOINT } from "./constants.js";

export interface TransformState {
  serverModuleCache: Map<string, boolean>;
  serverFunctionMap: Map<string, string[]>;
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

/** Remove a named export function or arrow export from code */
function removeFunction(code: string, name: string): string {
  // Pattern 1: export (async) function name(...) { ... }
  const fnPattern = new RegExp(
    `export\\s+(async\\s+)?function\\s+${name}\\s*\\([^)]*\\)[^{]*\\{`, "m"
  );
  const fnMatch = fnPattern.exec(code);
  if (fnMatch) {
    return removeBraceBlock(code, fnMatch.index, fnMatch[0].length);
  }

  // Pattern 2: export const/let/var name = ...;
  const arrowPattern = new RegExp(
    `export\\s+(const|let|var)\\s+${name}\\s*=`, "m"
  );
  const arrowMatch = arrowPattern.exec(code);
  if (arrowMatch) {
    const start = arrowMatch.index;
    let depth = 0;
    let inStr: string | null = null;
    let escaped = false;
    for (let i = start + arrowMatch[0].length; i < code.length; i++) {
      const ch = code[i];
      if (escaped) { escaped = false; continue; }
      if (ch === "\\") { escaped = true; continue; }
      if (inStr) { if (ch === inStr) inStr = null; continue; }
      if (ch === '"' || ch === "'" || ch === "`") { inStr = ch; continue; }
      if (ch === "{" || ch === "(" || ch === "[") depth++;
      if (ch === "}" || ch === ")" || ch === "]") depth--;
      if (depth === 0 && ch === ";") {
        return code.slice(0, start) + code.slice(i + 1);
      }
      if (depth < 0) {
        return code.slice(0, start) + code.slice(i);
      }
    }
  }
  return code;
}

function removeBraceBlock(code: string, matchStart: number, matchLen: number): string {
  const braceStart = code.indexOf("{", matchStart + matchLen - 1);
  let depth = 0;
  let inStr: string | null = null;
  let escaped = false;
  for (let i = braceStart; i < code.length; i++) {
    const ch = code[i];
    if (escaped) { escaped = false; continue; }
    if (ch === "\\") { escaped = true; continue; }
    if (inStr) { if (ch === inStr) inStr = null; continue; }
    if (ch === '"' || ch === "'" || ch === "`") { inStr = ch; continue; }
    if (ch === "{") depth++;
    if (ch === "}") { depth--; if (depth === 0) return code.slice(0, matchStart) + code.slice(i + 1); }
  }
  return code;
}

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

        // 5. Find server functions
        const serverFns: string[] = [];

        for (const node of ast.body) {
          if (node.type === "ExportNamedDeclaration" && node.declaration?.type === "FunctionDeclaration") {
            const name = node.declaration.id?.name;
            if (!name) continue;

            if (isFileServer) {
              serverFns.push(name);
            } else if (hasFnDirective(node.declaration, "use server")) {
              serverFns.push(name);
            } else if (fnReferencesAny(node.declaration, tainted)) {
              serverFns.push(name);
            }
          }
        }

        if (serverFns.length === 0) return null;

        // 6. Track for build report
        serverFunctionMap.set(relative(root, id), serverFns);

        // 7. Transform to client code
        // For file-level "use server" modules: replace ENTIRE file with stubs
        if (isFileServer) {
          const stubs = serverFns.map((name) => makeStub(name, rpcEndpoint)).join("\n\n");
          return { code: stubs + "\n", map: null };
        }

        // For mixed files: remove server fns, keep client code, append stubs
        let result = code;

        // Remove "use server" directive
        result = result.replace(/^\s*["']use server["'];?\s*\n/, "");

        // Remove each server function
        for (const fn of serverFns) {
          result = removeFunction(result, fn);
        }

        // Remove @zeroship/* imports (server-only)
        for (const src of knownServerSources) {
          result = result.replace(
            new RegExp(`^\\s*import\\s+.*from\\s+['"]${src.replace("/", "\\/")}['"]\\s*;?\\s*$`, "gm"),
            ""
          );
        }

        // Remove tainted variable declarations (handles multi-line model() calls)
        for (const name of tainted) {
          const declPattern = new RegExp(`(const|let|var)\\s+${name}\\s*=`);
          const match = declPattern.exec(result);
          if (match) {
            // Find the start of the line
            let lineStart = result.lastIndexOf("\n", match.index) + 1;
            // Find the end: scan for balanced parens/braces, then semicolon or newline
            let pos = match.index + match[0].length;
            let depth = 0;
            let inStr: string | null = null;
            while (pos < result.length) {
              const ch = result[pos];
              if (inStr) { if (ch === inStr && result[pos - 1] !== "\\") inStr = null; }
              else if (ch === '"' || ch === "'" || ch === "`") { inStr = ch; }
              else if (ch === "(" || ch === "{" || ch === "[") { depth++; }
              else if (ch === ")" || ch === "}" || ch === "]") { depth--; }
              else if (depth === 0 && (ch === ";" || ch === "\n")) { pos++; break; }
              pos++;
            }
            result = result.slice(0, lineStart) + result.slice(pos);
          }
        }

        // Append RPC stubs
        result = result.trim() + "\n\n" + serverFns.map((name) => makeStub(name, rpcEndpoint)).join("\n\n") + "\n";

        return { code: result, map: null };
      },
    },
  };
}
