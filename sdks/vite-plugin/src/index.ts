/**
 * @zeroship/vite-plugin — full-stack Vite 8 plugin for zeroship.
 *
 * Uses only Rolldown/Vite APIs — no @swc/core dependency.
 *
 * Usage:
 *   import { zeroship } from '@zeroship/vite-plugin'
 *   export default defineConfig({ plugins: [react(), zeroship()] })
 *
 * How it works:
 *   1. transform hook: this.parse() detects "use server" + taint analysis
 *   2. Server functions replaced with RPC fetch() stubs in client code
 *   3. Dev: proxies /_rpc to a local zeroship runtime child process
 *   4. Build: Rolldown build() bundles server code after Vite finishes client
 */

import type { Plugin, ResolvedConfig, ViteDevServer } from "vite";
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, relative, extname } from "node:path";
import { ChildProcess, spawn } from "node:child_process";
import http from "node:http";

export interface ZeroshipOptions {
  /** RPC endpoint path (default: "/_rpc") */
  rpcEndpoint?: string;
  /** Server entry point (auto-detected if not specified) */
  serverEntry?: string;
  /** Port for the zeroship dev server (default: 3001) */
  devServerPort?: number;
}

export function zeroship(options: ZeroshipOptions = {}): Plugin[] {
  const rpcEndpoint = options.rpcEndpoint ?? "/_rpc";
  const devPort = options.devServerPort ?? 3001;

  let config: ResolvedConfig;
  let isDev = false;
  let root = "";
  let serverProcess: ChildProcess | null = null;

  // Caches
  const serverModuleCache = new Map<string, boolean>();
  const serverFunctionMap = new Map<string, string[]>();
  const knownServerSources = new Set<string>();

  // --- Helpers ---

  /** Check if a file's first non-comment line is "use server" */
  function isServerFile(filePath: string): boolean {
    if (serverModuleCache.has(filePath)) return serverModuleCache.get(filePath)!;
    try {
      const code = readFileSync(filePath, "utf-8");
      const result = checkDirective(code, "use server");
      serverModuleCache.set(filePath, result);
      return result;
    } catch {
      serverModuleCache.set(filePath, false);
      return false;
    }
  }

  /** Check if code starts with a directive string */
  function checkDirective(code: string, directive: string): boolean {
    for (const line of code.split("\n")) {
      const t = line.trim();
      if (t === "" || t.startsWith("//") || t.startsWith("/*")) continue;
      return t === `"${directive}"` || t === `"${directive}";`
          || t === `'${directive}'` || t === `'${directive}';`;
    }
    return false;
  }

  /** Resolve a package specifier to its entry file and check for "use server" */
  function isServerPackage(specifier: string): boolean {
    if (serverModuleCache.has(specifier)) return serverModuleCache.get(specifier)!;

    const pkgDir = resolve(root, "node_modules", specifier);
    if (!existsSync(pkgDir)) { serverModuleCache.set(specifier, false); return false; }

    const pkgPath = resolve(pkgDir, "package.json");
    if (!existsSync(pkgPath)) { serverModuleCache.set(specifier, false); return false; }

    try {
      const pkg = JSON.parse(readFileSync(pkgPath, "utf-8"));
      const entry = pkg.exports?.["."]?.import
        ?? pkg.exports?.["."]?.default
        ?? (typeof pkg.exports?.["."] === "string" ? pkg.exports["."] : null)
        ?? pkg.module ?? pkg.main;
      if (!entry) { serverModuleCache.set(specifier, false); return false; }

      const result = isServerFile(resolve(pkgDir, entry));
      serverModuleCache.set(specifier, result);
      return result;
    } catch {
      serverModuleCache.set(specifier, false);
      return false;
    }
  }

  /** Scan @zeroship/* packages for "use server" */
  function discoverServerPackages(): void {
    const scopeDir = resolve(root, "node_modules", "@zeroship");
    if (!existsSync(scopeDir)) return;
    try {
      for (const pkg of readdirSync(scopeDir)) {
        const spec = `@zeroship/${pkg}`;
        if (isServerPackage(spec)) knownServerSources.add(spec);
      }
    } catch { /* ignore */ }
  }

  /** Generate an RPC stub for a function name */
  function makeStub(name: string): string {
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

  /** Remove a named function (export async function name(...) { ... }) from code */
  function removeFunction(code: string, name: string): string {
    const pattern = new RegExp(
      `export\\s+(async\\s+)?function\\s+${name}\\s*\\([^)]*\\)[^{]*\\{`,
      "m"
    );
    const match = pattern.exec(code);
    if (!match) return code;

    const start = match.index;
    let depth = 0;
    let inStr: string | null = null;
    let escaped = false;
    const braceStart = code.indexOf("{", start + match[0].length - 1);

    for (let i = braceStart; i < code.length; i++) {
      const ch = code[i];
      if (escaped) { escaped = false; continue; }
      if (ch === "\\") { escaped = true; continue; }
      if (inStr) { if (ch === inStr) inStr = null; continue; }
      if (ch === '"' || ch === "'" || ch === "`") { inStr = ch; continue; }
      if (ch === "{") depth++;
      if (ch === "}") { depth--; if (depth === 0) return code.slice(0, start) + code.slice(i + 1); }
    }
    return code;
  }

  /** Find server entry point in project */
  function findServerEntry(): string | null {
    if (options.serverEntry) return options.serverEntry;
    const candidates = ["src/index.ts", "src/index.tsx", "src/server.ts", "src/index.js", "src/server.js"];
    for (const c of candidates) {
      if (existsSync(resolve(root, c))) return c;
    }
    return null;
  }

  // --- Plugins ---

  return [
    {
      name: "zeroship:transform",
      enforce: "pre" as const,

      configResolved(resolvedConfig: ResolvedConfig) {
        config = resolvedConfig;
        isDev = config.command === "serve";
        root = config.root;
        discoverServerPackages();
      },

      transform: {
        filter: {
          id: { include: /\.(ts|tsx|js|jsx)$/, exclude: /node_modules/ },
        },
        handler(this: any, code: string, id: string) {
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
                  && isServerFile(resolve(id, "..", src.replace(/\.(ts|tsx|js|jsx)$/, "") + extname(id)));

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
            const stubs = serverFns.map(makeStub).join("\n\n");
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
          result = result.trim() + "\n\n" + serverFns.map(makeStub).join("\n\n") + "\n";

          return { code: result, map: null };
        },
      },
    },

    // Dev server: zeroship runtime + proxy
    {
      name: "zeroship:dev-server",

      configureServer(server: ViteDevServer) {
        if (!isDev) return;

        // Start zeroship runtime
        const entry = findServerEntry();
        if (entry) {
          const bin = resolve(root, "node_modules/.bin/zeroship");
          const cmd = existsSync(bin) ? bin : "zeroship";
          try {
            serverProcess = spawn(cmd, ["serve", entry, `--port=${devPort}`, "--workers=1"], {
              cwd: root,
              stdio: ["ignore", "pipe", "pipe"],
            });
            serverProcess.stdout?.on("data", (d: Buffer) => {
              const msg = d.toString().trim();
              if (msg) console.log(`[zeroship:api] ${msg}`);
            });
            serverProcess.stderr?.on("data", (d: Buffer) => {
              const msg = d.toString().trim();
              if (msg) console.log(`[zeroship:api] ${msg}`);
            });
            console.log(`[zeroship] API server starting on :${devPort}`);
          } catch {
            console.warn("[zeroship] Failed to start API server — zeroship CLI not found");
          }
        }

        // Proxy /_rpc → zeroship runtime
        server.middlewares.use((req, res, next) => {
          if (!req.url?.startsWith(rpcEndpoint)) return next();

          const proxyReq = http.request(
            `http://localhost:${devPort}/rpc`,
            { method: req.method, headers: req.headers },
            (proxyRes) => {
              res.writeHead(proxyRes.statusCode ?? 502, proxyRes.headers);
              proxyRes.pipe(res);
            }
          );
          req.pipe(proxyReq);
          proxyReq.on("error", () => {
            res.writeHead(503, { "Content-Type": "application/json" });
            res.end('{"error":"zeroship API not ready"}');
          });
        });
      },

      handleHotUpdate({ file }: { file: string }) {
        const ext = extname(file);
        if (![".ts", ".tsx", ".js", ".jsx"].includes(ext)) return;

        const rel = relative(root, file);
        if (serverFunctionMap.has(rel) || rel.startsWith("src/index") || rel.startsWith("src/server")) {
          console.log(`[zeroship] API changed: ${rel} — restarting`);
          if (serverProcess) { serverProcess.kill("SIGTERM"); serverProcess = null; }

          const entry = findServerEntry();
          if (entry) {
            setTimeout(() => {
              const bin = resolve(root, "node_modules/.bin/zeroship");
              const cmd = existsSync(bin) ? bin : "zeroship";
              try {
                serverProcess = spawn(cmd, ["serve", entry, `--port=${devPort}`, "--workers=1"], {
                  cwd: root,
                  stdio: ["ignore", "pipe", "pipe"],
                });
              } catch { /* ignore */ }
            }, 300);
          }

          serverModuleCache.clear();
        }
      },

      buildEnd() {
        if (serverProcess) { serverProcess.kill("SIGTERM"); serverProcess = null; }
      },
    },

    // Production build: bundle server with Rolldown
    {
      name: "zeroship:build",

      async closeBundle() {
        if (isDev) return;

        const entry = findServerEntry();
        if (!entry) { console.log("[zeroship] No server entry — client-only build"); return; }

        const outDir = config.build?.outDir
          ? resolve(root, config.build.outDir)
          : resolve(root, "dist");

        const serverOut = resolve(outDir, "server");

        console.log("\n[zeroship] Building server bundle with Rolldown...");

        try {
          // Use esbuild for server bundle — simple, fast, no Vite recursion
          const { execSync } = await import("node:child_process" as string);
          const { mkdirSync } = await import("node:fs" as string);
          mkdirSync(serverOut, { recursive: true });
          const outFile = resolve(serverOut, "server.js");
          const entryFile = resolve(root, entry);
          const minFlag = config.build?.minify !== false ? "--minify" : "";
          execSync(
            `npx esbuild ${entryFile} --bundle --format=esm --platform=neutral --main-fields=module,main --outfile=${outFile} ${minFlag}`.trim(),
            { cwd: root, stdio: "pipe" }
          );

          console.log(`[zeroship] Server: ${outFile}`);
        } catch (e) {
          console.error("[zeroship] Server build failed:", e);
        }

        // Build report
        if (serverFunctionMap.size > 0) {
          console.log("\n[zeroship] Server/client split:");
          for (const [file, fns] of serverFunctionMap) {
            console.log(`  ${file}: ${fns.join(", ")}`);
          }
        }
      },
    },
  ];
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

/** Check if a function body contains any reference to tainted identifiers */
function fnReferencesAny(fn: any, tainted: Set<string>): boolean {
  if (tainted.size === 0) return false;
  const json = JSON.stringify(fn.body);
  for (const name of tainted) {
    // Match identifier nodes: "name":"<tainted>"
    if (json.includes(`"name":"${name}"`)) return true;
  }
  return false;
}

export default zeroship;
