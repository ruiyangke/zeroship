# Vite Environment API Integration — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the child-process+proxy dev server in `@zeroship/vite-plugin` with Vite's Environment API, running a ModuleRunner inside the zeroship V8 runtime for true runtime parity and granular HMR.

**Architecture:** Vite registers a custom `zeroship` environment with `ZeroshipDevEnvironment extends DevEnvironment`. The plugin spawns `zeroship serve` with a pre-bundled dev bootstrap module that contains `vite/module-runner`. The ModuleRunner in V8 connects to Vite's HotChannel via WebSocket, fetches transformed modules, and evaluates them with `eval()`. HMR invalidation is module-level — no process restart.

**Tech Stack:** Vite 6+ Environment API, `vite/module-runner`, WebSocket (`ws` package), esbuild (bootstrap bundling)

---

## File Structure

```
sdks/vite-plugin/
  package.json              — (modify) add ws dependency, bump vite peer dep
  tsconfig.json             — (modify) exclude dev-bootstrap from main build
  src/
    index.ts                — (rewrite) thin factory, delegates to sub-plugins
    constants.ts            — (create) shared constants
    transform.ts            — (create) extracted "use server" transforms
    environment.ts          — (create) ZeroshipDevEnvironment + HotChannel
    dev-server.ts           — (create) configureServer, spawn runtime, proxy, WS bridge
    build.ts                — (create) extracted production build
  src/dev-bootstrap/
    index.ts                — (create) entry: ModuleRunner setup + onRequest handler
    evaluator.ts            — (create) eval-based ModuleEvaluator for V8
    transport.ts            — (create) WebSocket transport to Vite HotChannel
  scripts/
    build-bootstrap.ts      — (create) esbuild script to bundle dev-bootstrap
  tsconfig.bootstrap.json   — (create) separate tsconfig for dev-bootstrap
```

---

### Task 1: Constants + shared types

**Files:**
- Create: `sdks/vite-plugin/src/constants.ts`

- [ ] **Step 1: Create constants file**

```typescript
// sdks/vite-plugin/src/constants.ts

/** WebSocket path for HMR channel, appended to Vite's HTTP server URL. */
export const WS_PATH = "/__zeroship_hmr";

/** Environment variable names passed to the zeroship child process. */
export const ENV_DEV = "ZEROSHIP_DEV";
export const ENV_VITE_WS = "ZEROSHIP_VITE_WS";
export const ENV_ENTRY = "ZEROSHIP_ENTRY";

/** Default port for the zeroship dev runtime. */
export const DEFAULT_DEV_PORT = 3001;

/** Default RPC endpoint path. */
export const DEFAULT_RPC_ENDPOINT = "/_rpc";
```

- [ ] **Step 2: Commit**

```bash
git add sdks/vite-plugin/src/constants.ts
git commit -m "feat(vite-plugin): add shared constants for Environment API"
```

---

### Task 2: Extract "use server" transforms

Extract the existing `zeroship:transform` plugin from the monolithic `index.ts` into its own file. No behavior changes — pure extraction.

**Files:**
- Create: `sdks/vite-plugin/src/transform.ts`

- [ ] **Step 1: Create transform.ts with extracted code**

Extract the following from the current `index.ts`:
- `isServerFile()`, `checkDirective()`, `isServerPackage()`, `discoverServerPackages()`
- `makeStub()`, `removeFunction()`
- The `zeroship:transform` plugin object (lines 166-317 of current index.ts)
- `hasFnDirective()`, `fnReferencesAny()` helper functions (lines 453-472)

The new file exports a single function:

```typescript
// sdks/vite-plugin/src/transform.ts
import type { Plugin, ResolvedConfig } from "vite";
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, relative, extname } from "node:path";
import { DEFAULT_RPC_ENDPOINT } from "./constants";

export interface TransformState {
  serverModuleCache: Map<string, boolean>;
  serverFunctionMap: Map<string, string[]>;
  knownServerSources: Set<string>;
}

export function transformPlugin(
  rpcEndpoint: string = DEFAULT_RPC_ENDPOINT,
  state: TransformState,
): Plugin {
  let config: ResolvedConfig;
  let root = "";

  // --- Helpers (copied from current index.ts, unchanged) ---

  function isServerFile(filePath: string): boolean {
    if (state.serverModuleCache.has(filePath)) return state.serverModuleCache.get(filePath)!;
    try {
      const code = readFileSync(filePath, "utf-8");
      const result = checkDirective(code, "use server");
      state.serverModuleCache.set(filePath, result);
      return result;
    } catch {
      state.serverModuleCache.set(filePath, false);
      return false;
    }
  }

  function checkDirective(code: string, directive: string): boolean {
    for (const line of code.split("\n")) {
      const t = line.trim();
      if (t === "" || t.startsWith("//") || t.startsWith("/*")) continue;
      return t === `"${directive}"` || t === `"${directive}";`
          || t === `'${directive}'` || t === `'${directive}';`;
    }
    return false;
  }

  function isServerPackage(specifier: string): boolean {
    if (state.serverModuleCache.has(specifier)) return state.serverModuleCache.get(specifier)!;
    const pkgDir = resolve(root, "node_modules", specifier);
    if (!existsSync(pkgDir)) { state.serverModuleCache.set(specifier, false); return false; }
    const pkgPath = resolve(pkgDir, "package.json");
    if (!existsSync(pkgPath)) { state.serverModuleCache.set(specifier, false); return false; }
    try {
      const pkg = JSON.parse(readFileSync(pkgPath, "utf-8"));
      const entry = pkg.exports?.["."]?.import
        ?? pkg.exports?.["."]?.default
        ?? (typeof pkg.exports?.["."] === "string" ? pkg.exports["."] : null)
        ?? pkg.module ?? pkg.main;
      if (!entry) { state.serverModuleCache.set(specifier, false); return false; }
      const result = isServerFile(resolve(pkgDir, entry));
      state.serverModuleCache.set(specifier, result);
      return result;
    } catch {
      state.serverModuleCache.set(specifier, false);
      return false;
    }
  }

  function discoverServerPackages(): void {
    const scopeDir = resolve(root, "node_modules", "@zeroship");
    if (!existsSync(scopeDir)) return;
    try {
      for (const pkg of readdirSync(scopeDir)) {
        const spec = `@zeroship/${pkg}`;
        if (isServerPackage(spec)) state.knownServerSources.add(spec);
      }
    } catch { /* ignore */ }
  }

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

  function removeFunction(code: string, name: string): string {
    const pattern = new RegExp(
      `export\\s+(async\\s+)?function\\s+${name}\\s*\\([^)]*\\)[^{]*\\{`, "m"
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

  // --- Plugin ---

  return {
    name: "zeroship:transform",
    enforce: "pre" as const,

    configResolved(resolvedConfig: ResolvedConfig) {
      config = resolvedConfig;
      root = config.root;
      discoverServerPackages();
    },

    transform: {
      filter: {
        id: { include: /\.(ts|tsx|js|jsx)$/, exclude: /node_modules/ },
      },
      handler(this: any, code: string, id: string) {
        const isTsx = id.endsWith(".tsx") || id.endsWith(".jsx");
        const ast = this.parse(code, { lang: isTsx ? "tsx" : "ts" });

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

        const tainted = new Set<string>();
        for (const node of ast.body) {
          if (node.type === "ImportDeclaration") {
            const src = node.source?.value;
            if (!src) continue;
            const isServer = state.knownServerSources.has(src)
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

        state.serverFunctionMap.set(relative(root, id), serverFns);

        if (isFileServer) {
          const stubs = serverFns.map(makeStub).join("\n\n");
          return { code: stubs + "\n", map: null };
        }

        let result = code;
        result = result.replace(/^\s*["']use server["'];?\s*\n/, "");
        for (const fn of serverFns) {
          result = removeFunction(result, fn);
        }
        for (const src of state.knownServerSources) {
          result = result.replace(
            new RegExp(`^\\s*import\\s+.*from\\s+['"]${src.replace("/", "\\/")}['"]\\s*;?\\s*$`, "gm"),
            ""
          );
        }
        for (const name of tainted) {
          const declPattern = new RegExp(`(const|let|var)\\s+${name}\\s*=`);
          const match = declPattern.exec(result);
          if (match) {
            let lineStart = result.lastIndexOf("\n", match.index) + 1;
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
        result = result.trim() + "\n\n" + serverFns.map(makeStub).join("\n\n") + "\n";
        return { code: result, map: null };
      },
    },
  };
}

// --- AST helpers ---

function hasFnDirective(fn: any, directive: string): boolean {
  const stmts = fn.body?.body;
  if (!stmts || stmts.length === 0) return false;
  const first = stmts[0];
  return first.type === "ExpressionStatement"
    && first.expression?.type === "Literal"
    && first.expression.value === directive;
}

function fnReferencesAny(fn: any, tainted: Set<string>): boolean {
  if (tainted.size === 0) return false;
  const json = JSON.stringify(fn.body);
  for (const name of tainted) {
    if (json.includes(`"name":"${name}"`)) return true;
  }
  return false;
}
```

- [ ] **Step 2: Verify the plugin builds**

```bash
cd sdks/vite-plugin && npx tsc --noEmit
```

Expected: no errors (the file is self-contained).

- [ ] **Step 3: Commit**

```bash
git add sdks/vite-plugin/src/transform.ts
git commit -m "refactor(vite-plugin): extract transform plugin to own file"
```

---

### Task 3: Extract production build plugin

Extract the existing `zeroship:build` plugin from `index.ts`. No behavior changes.

**Files:**
- Create: `sdks/vite-plugin/src/build.ts`

- [ ] **Step 1: Create build.ts**

```typescript
// sdks/vite-plugin/src/build.ts
import type { Plugin, ResolvedConfig } from "vite";
import { resolve } from "node:path";
import type { TransformState } from "./transform";

export function buildPlugin(state: TransformState): Plugin {
  let config: ResolvedConfig;
  let isDev = false;
  let root = "";

  return {
    name: "zeroship:build",

    configResolved(resolvedConfig: ResolvedConfig) {
      config = resolvedConfig;
      isDev = config.command === "serve";
      root = config.root;
    },

    async closeBundle() {
      if (isDev) return;

      const entry = findServerEntry(root);
      if (!entry) { console.log("[zeroship] No server entry — client-only build"); return; }

      const outDir = config.build?.outDir
        ? resolve(root, config.build.outDir)
        : resolve(root, "dist");

      const serverOut = resolve(outDir, "server");

      console.log("\n[zeroship] Building server bundle with Rolldown...");

      try {
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

      if (state.serverFunctionMap.size > 0) {
        console.log("\n[zeroship] Server/client split:");
        for (const [file, fns] of state.serverFunctionMap) {
          console.log(`  ${file}: ${fns.join(", ")}`);
        }
      }
    },
  };
}

export function findServerEntry(root: string, explicit?: string): string | null {
  if (explicit) return explicit;
  const candidates = ["src/index.ts", "src/index.tsx", "src/server.ts", "src/index.js", "src/server.js"];
  for (const c of candidates) {
    if (existsSync(resolve(root, c))) return c;
  }
  return null;
}
```

- [ ] **Step 2: Commit**

```bash
git add sdks/vite-plugin/src/build.ts
git commit -m "refactor(vite-plugin): extract build plugin to own file"
```

---

### Task 4: ZeroshipDevEnvironment + HotChannel

The core Environment API integration. Creates the custom dev environment class and WebSocket-backed HotChannel.

**Files:**
- Create: `sdks/vite-plugin/src/environment.ts`

- [ ] **Step 1: Create environment.ts**

```typescript
// sdks/vite-plugin/src/environment.ts
import * as vite from "vite";
import { WS_PATH } from "./constants";
import type { WebSocket } from "ws";

// --- Types ---

interface WsContainer {
  ws?: WebSocket;
  buffer: string[];
}

// --- HotChannel ---

function createHotChannel(container: WsContainer): vite.HotChannel {
  const listenersMap = new Map<string, Set<vite.HotChannelListener>>();

  const client: vite.HotChannelClient = {
    send(payload) {
      const msg = JSON.stringify(payload);
      if (!container.ws) {
        container.buffer.push(msg);
        return;
      }
      container.ws.send(msg);
    },
  };

  function onMessage(rawData: Buffer | string) {
    const payload = JSON.parse(rawData.toString()) as vite.CustomPayload;
    const listeners = listenersMap.get(payload.event) ?? new Set();
    for (const listener of listeners) {
      listener(payload.data, client);
    }
  }

  return {
    send(payload) {
      client.send(payload);
    },
    on(event: string, listener: vite.HotChannelListener) {
      const listeners = listenersMap.get(event) ?? new Set();
      listeners.add(listener);
      listenersMap.set(event, listeners);
    },
    off(event: string, listener: vite.HotChannelListener) {
      listenersMap.get(event)?.delete(listener);
    },
    listen() {
      container.ws?.on("message", onMessage);
    },
    close() {
      container.ws?.off("message", onMessage);
    },
  };
}

// --- ZeroshipDevEnvironment ---

export class ZeroshipDevEnvironment extends vite.DevEnvironment {
  #wsContainer: WsContainer;

  constructor(name: string, config: vite.ResolvedConfig) {
    const wsContainer: WsContainer = { buffer: [] };
    super(name, config, {
      hot: true,
      transport: createHotChannel(wsContainer),
    });
    this.#wsContainer = wsContainer;
  }

  /** Called by dev-server plugin once the runtime's WebSocket connects. */
  setWebSocket(ws: WebSocket): void {
    this.#wsContainer.ws = ws;
    // Flush any HMR messages buffered before the WS was established.
    for (const msg of this.#wsContainer.buffer) {
      ws.send(msg);
    }
    this.#wsContainer.buffer = [];

    // Re-call listen() so the onMessage handler is wired to the new WS.
    this.hot.listen();
  }
}

// --- Environment options factory ---

export function createZeroshipEnvironmentOptions(): vite.EnvironmentOptions {
  return {
    resolve: {
      conditions: ["zeroship", "worker", "module"],
      noExternal: true,
    },
    dev: {
      createEnvironment(name: string, config: vite.ResolvedConfig) {
        return new ZeroshipDevEnvironment(name, config);
      },
    },
    build: {
      target: "es2024",
      ssr: true,
    },
    keepProcessEnv: true,
  };
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cd sdks/vite-plugin && npx tsc --noEmit
```

Expected: may need `ws` types. If so, install in step 3.

- [ ] **Step 3: Install ws dependency**

```bash
cd sdks/vite-plugin && npm install ws && npm install -D @types/ws
```

- [ ] **Step 4: Re-verify compilation**

```bash
cd sdks/vite-plugin && npx tsc --noEmit
```

Expected: no errors.

- [ ] **Step 5: Commit**

```bash
git add sdks/vite-plugin/src/environment.ts sdks/vite-plugin/package.json sdks/vite-plugin/package-lock.json
git commit -m "feat(vite-plugin): add ZeroshipDevEnvironment + HotChannel"
```

---

### Task 5: Dev server plugin

The new `zeroship:dev-server` plugin — spawns zeroship runtime, sets up WebSocket bridge, registers proxy middleware.

**Files:**
- Create: `sdks/vite-plugin/src/dev-server.ts`

- [ ] **Step 1: Create dev-server.ts**

```typescript
// sdks/vite-plugin/src/dev-server.ts
import type { Plugin, ViteDevServer } from "vite";
import { resolve, relative, extname } from "node:path";
import { existsSync } from "node:fs";
import { ChildProcess, spawn } from "node:child_process";
import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import { WS_PATH, ENV_DEV, ENV_VITE_WS, ENV_ENTRY, DEFAULT_DEV_PORT } from "./constants";
import { ZeroshipDevEnvironment, createZeroshipEnvironmentOptions } from "./environment";
import { findServerEntry } from "./build";
import type { TransformState } from "./transform";

export interface DevServerOptions {
  devServerPort?: number;
  serverEntry?: string;
}

export function devServerPlugin(options: DevServerOptions, state: TransformState): Plugin[] {
  const devPort = options.devServerPort ?? DEFAULT_DEV_PORT;

  let root = "";
  let isDev = false;
  let serverProcess: ChildProcess | null = null;

  return [
    // Plugin 1: Register the zeroship environment
    {
      name: "zeroship:environment",

      config() {
        return {
          environments: {
            zeroship: createZeroshipEnvironmentOptions(),
          },
        };
      },

      configResolved(config) {
        root = config.root;
        isDev = config.command === "serve";
      },
    },

    // Plugin 2: Dev server lifecycle
    {
      name: "zeroship:dev-server",

      configureServer(server: ViteDevServer) {
        if (!isDev) return;

        const zeroshipEnv = server.environments["zeroship"] as ZeroshipDevEnvironment | undefined;

        // --- WebSocket bridge ---
        const wss = new WebSocketServer({ noServer: true });

        server.httpServer!.on("upgrade", (req, socket, head) => {
          const url = new URL(req.url ?? "/", "http://localhost");
          if (url.pathname !== WS_PATH) return;

          wss.handleUpgrade(req, socket as any, head, (ws: WebSocket) => {
            console.log("[zeroship] Runtime connected via WebSocket");
            if (zeroshipEnv) {
              zeroshipEnv.setWebSocket(ws);
            }

            ws.on("close", () => {
              console.log("[zeroship] Runtime WebSocket disconnected");
            });
          });
        });

        // --- Start zeroship runtime ---
        const entry = findServerEntry(root, options.serverEntry);
        if (!entry) {
          console.warn("[zeroship] No server entry found — skipping API server");
          return;
        }

        const vitePort = server.config.server.port ?? 5173;
        const bootstrapPath = resolve(__dirname, "dev-bootstrap.js");

        if (!existsSync(bootstrapPath)) {
          console.warn("[zeroship] dev-bootstrap.js not found — run the bootstrap build first");
          return;
        }

        const bin = resolve(root, "node_modules/.bin/zeroship");
        const cmd = existsSync(bin) ? bin : "zeroship";

        serverProcess = spawn(cmd, ["serve", bootstrapPath, `--port=${devPort}`, "--workers=1"], {
          cwd: root,
          env: {
            ...process.env,
            [ENV_DEV]: "1",
            [ENV_VITE_WS]: `ws://localhost:${vitePort}${WS_PATH}`,
            [ENV_ENTRY]: entry,
          },
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
        serverProcess.on("exit", (code) => {
          if (code !== null && code !== 0) {
            console.error(`[zeroship] Runtime exited with code ${code}`);
          }
          serverProcess = null;
        });

        console.log(`[zeroship] API server starting on :${devPort} (entry: ${entry})`);

        // --- Proxy middleware (returned = post-middleware) ---
        return () => {
          server.middlewares.use((req, res, next) => {
            if (!req.url?.startsWith("/_rpc") && !req.url?.startsWith("/rpc")) return next();

            const targetUrl = `http://localhost:${devPort}${req.url}`;
            const proxyReq = http.request(
              targetUrl,
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
        };
      },

      handleHotUpdate({ file }: { file: string }) {
        const ext = extname(file);
        if (![".ts", ".tsx", ".js", ".jsx"].includes(ext)) return;

        // Clear "use server" cache so directive changes are re-detected
        const rel = relative(root, file);
        if (state.serverFunctionMap.has(rel)) {
          state.serverModuleCache.delete(file);
        }
        // ModuleRunner handles module invalidation via the HMR channel — no restart needed.
      },

      buildEnd() {
        if (serverProcess) {
          serverProcess.kill("SIGTERM");
          serverProcess = null;
        }
      },
    },
  ];
}
```

- [ ] **Step 2: Verify compilation**

```bash
cd sdks/vite-plugin && npx tsc --noEmit
```

- [ ] **Step 3: Commit**

```bash
git add sdks/vite-plugin/src/dev-server.ts
git commit -m "feat(vite-plugin): add dev server with Environment API + WS bridge"
```

---

### Task 6: Dev bootstrap — evaluator

The eval-based ModuleEvaluator that runs inside zeroship's V8 runtime.

**Files:**
- Create: `sdks/vite-plugin/src/dev-bootstrap/evaluator.ts`

- [ ] **Step 1: Create evaluator.ts**

```typescript
// sdks/vite-plugin/src/dev-bootstrap/evaluator.ts

/**
 * Eval-based ModuleEvaluator for the zeroship V8 runtime.
 *
 * Same pattern as Cloudflare's __VITE_UNSAFE_EVAL__. Uses indirect eval
 * to execute Vite-transformed modules in global scope. The trailing \n
 * ensures // comments on the last line don't swallow the closing brace.
 */

const SSR_MODULE_EXPORTS_KEY = "__vite_ssr_exports__";

export const zeroshipEvaluator = {
  async runInlinedModule(
    context: Record<string, any>,
    code: string,
    _module: { id: string },
  ): Promise<void> {
    const keys = Object.keys(context).join(",");
    const wrapped = `"use strict";async (${keys})=>{${code}\n}`;
    // Indirect eval: (0, eval)(...) ensures global scope, not local.
    const fn = (0, eval)(wrapped);
    await fn(...Object.values(context));
    Object.seal(context[SSR_MODULE_EXPORTS_KEY]);
  },

  async runExternalModule(filepath: string): Promise<any> {
    return import(filepath);
  },
};
```

- [ ] **Step 2: Commit**

```bash
git add sdks/vite-plugin/src/dev-bootstrap/evaluator.ts
git commit -m "feat(vite-plugin): add eval-based ModuleEvaluator for V8"
```

---

### Task 7: Dev bootstrap — WebSocket transport

The transport layer connecting ModuleRunner in V8 to Vite's HotChannel.

**Files:**
- Create: `sdks/vite-plugin/src/dev-bootstrap/transport.ts`

- [ ] **Step 1: Create transport.ts**

```typescript
// sdks/vite-plugin/src/dev-bootstrap/transport.ts

/**
 * Creates a ModuleRunner connected to Vite's dev server via WebSocket.
 *
 * This code runs inside the zeroship V8 runtime (not Node.js).
 * It uses the global WebSocket class provided by the runtime.
 * The transport implements send/connect only — Vite synthesizes invoke()
 * from these with built-in request/response correlation.
 */

// These imports will be resolved by the esbuild bundler at build time.
// In the bundled output, vite/module-runner is inlined.
import { ModuleRunner } from "vite/module-runner";
import { zeroshipEvaluator } from "./evaluator";

declare const WebSocket: {
  new (url: string): {
    addEventListener(event: string, handler: (e: any) => void): void;
    send(data: string): void;
    close(): void;
  };
};

export async function createRunner(): Promise<ModuleRunner> {
  const wsUrl = (globalThis as any).process?.env?.ZEROSHIP_VITE_WS;
  if (!wsUrl) {
    throw new Error("[zeroship] ZEROSHIP_VITE_WS not set — cannot connect to Vite dev server");
  }

  const ws = new WebSocket(wsUrl);

  // Wait for connection
  await new Promise<void>((resolve, reject) => {
    ws.addEventListener("open", () => resolve());
    ws.addEventListener("error", (e: any) => {
      reject(new Error(`[zeroship] WebSocket connection failed: ${e.message ?? e}`));
    });
  });

  const transport = {
    connect({ onMessage }: { onMessage: (data: any) => void }) {
      ws.addEventListener("message", (event: { data: any }) => {
        const parsed = typeof event.data === "string"
          ? JSON.parse(event.data)
          : JSON.parse(event.data.toString());
        onMessage(parsed);
      });
    },
    send(data: any) {
      ws.send(JSON.stringify(data));
    },
  };

  return new ModuleRunner(
    {
      transport,
      hmr: true,
      sourcemapInterceptor: "prepareStackTrace",
    },
    zeroshipEvaluator,
  );
}
```

- [ ] **Step 2: Commit**

```bash
git add sdks/vite-plugin/src/dev-bootstrap/transport.ts
git commit -m "feat(vite-plugin): add WebSocket transport for ModuleRunner"
```

---

### Task 8: Dev bootstrap — entry module

The main entry point loaded by the zeroship V8 runtime in dev mode. Sets up the ModuleRunner and exports the `onRequest` handler.

**Files:**
- Create: `sdks/vite-plugin/src/dev-bootstrap/index.ts`

- [ ] **Step 1: Create index.ts**

```typescript
// sdks/vite-plugin/src/dev-bootstrap/index.ts

/**
 * Dev bootstrap — entry module for the zeroship V8 runtime in dev mode.
 *
 * This file is bundled into dist/dev-bootstrap.js and loaded by
 * `zeroship serve` when the Vite plugin spawns the runtime.
 *
 * It creates a ModuleRunner that fetches transformed modules from Vite,
 * evaluates them with eval(), and dispatches HTTP requests to the user's
 * onRequest handler.
 */

import { createRunner } from "./transport";

const runner = await createRunner();
const ENTRY = (globalThis as any).process?.env?.ZEROSHIP_ENTRY;

if (!ENTRY) {
  throw new Error("[zeroship] ZEROSHIP_ENTRY not set — no server entry point");
}

console.log(`[zeroship:dev] ModuleRunner ready, entry: ${ENTRY}`);

/**
 * HTTP request handler — called by the Rust runtime's HTTP dispatch.
 *
 * Uses the ModuleRunner to import the user's entry module (with caching),
 * then delegates to the user's onRequest export.
 */
export async function onRequest(req: any): Promise<any> {
  const mod = await runner.import(ENTRY);

  if (typeof mod.onRequest === "function") {
    return mod.onRequest(req);
  }

  // If no onRequest, check for default export that's a function (handler pattern)
  if (typeof mod.default === "function") {
    return mod.default(req);
  }

  return new Response(
    JSON.stringify({ error: `No onRequest or default handler in ${ENTRY}` }),
    { status: 404, headers: { "Content-Type": "application/json" } },
  );
}
```

- [ ] **Step 2: Commit**

```bash
git add sdks/vite-plugin/src/dev-bootstrap/index.ts
git commit -m "feat(vite-plugin): add dev bootstrap entry module"
```

---

### Task 9: Bootstrap bundler script

Esbuild script that bundles the dev-bootstrap directory (including `vite/module-runner`) into a single ESM file for the V8 runtime.

**Files:**
- Create: `sdks/vite-plugin/scripts/build-bootstrap.ts`
- Create: `sdks/vite-plugin/tsconfig.bootstrap.json`
- Modify: `sdks/vite-plugin/package.json` — add build script

- [ ] **Step 1: Create tsconfig.bootstrap.json**

The dev-bootstrap files use different globals (no Node.js, has WebSocket/Response). Separate tsconfig prevents cross-contamination.

```json
{
  "compilerOptions": {
    "target": "ES2024",
    "module": "ES2022",
    "moduleResolution": "bundler",
    "strict": true,
    "esModuleInterop": true,
    "skipLibCheck": true,
    "outDir": "dist",
    "rootDir": "src/dev-bootstrap",
    "noEmit": true
  },
  "include": ["src/dev-bootstrap"]
}
```

- [ ] **Step 2: Create build-bootstrap.ts**

```typescript
// sdks/vite-plugin/scripts/build-bootstrap.ts
import * as esbuild from "esbuild";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");

await esbuild.build({
  entryPoints: [resolve(root, "src/dev-bootstrap/index.ts")],
  bundle: true,
  format: "esm",
  platform: "neutral",
  target: "es2024",
  outfile: resolve(root, "dist/dev-bootstrap.js"),
  // Bundle everything — vite/module-runner has no Node.js deps
  external: [],
  // Sourcemap for debugging
  sourcemap: true,
  // Keep readable for debugging
  minify: false,
  banner: {
    js: "// @zeroship/vite-plugin dev bootstrap — runs inside V8 runtime",
  },
});

console.log("[zeroship] dev-bootstrap.js built successfully");
```

- [ ] **Step 3: Add scripts to package.json**

Add to the `scripts` section:

```json
{
  "scripts": {
    "build": "tsc && node --import tsx scripts/build-bootstrap.ts",
    "build:bootstrap": "node --import tsx scripts/build-bootstrap.ts",
    "dev": "tsc --watch"
  }
}
```

Also add `esbuild` as a devDependency if not already present, and `tsx` for running the script:

```bash
cd sdks/vite-plugin && npm install -D esbuild tsx
```

- [ ] **Step 4: Exclude dev-bootstrap from main tsconfig**

Modify `sdks/vite-plugin/tsconfig.json` to exclude the bootstrap directory (it has its own tsconfig):

Change `"include": ["src"]` to `"include": ["src"], "exclude": ["src/dev-bootstrap"]`.

- [ ] **Step 5: Build the bootstrap and verify**

```bash
cd sdks/vite-plugin && node --import tsx scripts/build-bootstrap.ts
```

Expected: `dist/dev-bootstrap.js` is created. Verify it contains the ModuleRunner code:

```bash
head -5 sdks/vite-plugin/dist/dev-bootstrap.js
grep -c "ModuleRunner" sdks/vite-plugin/dist/dev-bootstrap.js
```

Expected: The file starts with the banner comment and contains ModuleRunner references.

- [ ] **Step 6: Commit**

```bash
git add sdks/vite-plugin/scripts/ sdks/vite-plugin/tsconfig.bootstrap.json sdks/vite-plugin/tsconfig.json sdks/vite-plugin/package.json sdks/vite-plugin/package-lock.json sdks/vite-plugin/dist/dev-bootstrap.js sdks/vite-plugin/dist/dev-bootstrap.js.map
git commit -m "feat(vite-plugin): add bootstrap bundler script + built output"
```

---

### Task 10: Rewrite plugin entry point

Replace the monolithic `index.ts` with the thin factory that delegates to the extracted sub-plugins.

**Files:**
- Modify: `sdks/vite-plugin/src/index.ts`

- [ ] **Step 1: Rewrite index.ts**

```typescript
// sdks/vite-plugin/src/index.ts

/**
 * @zeroship/vite-plugin — full-stack Vite plugin for zeroship.
 *
 * Usage:
 *   import { zeroship } from '@zeroship/vite-plugin'
 *   export default defineConfig({ plugins: [react(), zeroship()] })
 *
 * How it works:
 *   1. transform: detects "use server" + taint analysis → RPC stubs in client
 *   2. environment: registers zeroship DevEnvironment with Vite
 *   3. dev-server: spawns V8 runtime, WS bridge, proxy middleware
 *   4. build: bundles server code for production via esbuild
 */

import type { Plugin } from "vite";
import { DEFAULT_RPC_ENDPOINT } from "./constants";
import { transformPlugin, type TransformState } from "./transform";
import { devServerPlugin, type DevServerOptions } from "./dev-server";
import { buildPlugin } from "./build";

export interface ZeroshipOptions {
  /** RPC endpoint path (default: "/_rpc") */
  rpcEndpoint?: string;
  /** Server entry point (auto-detected if not specified) */
  serverEntry?: string;
  /** Port for the zeroship dev server (default: 3001) */
  devServerPort?: number;
}

export function zeroship(options: ZeroshipOptions = {}): Plugin[] {
  const rpcEndpoint = options.rpcEndpoint ?? DEFAULT_RPC_ENDPOINT;

  // Shared state across plugins
  const state: TransformState = {
    serverModuleCache: new Map(),
    serverFunctionMap: new Map(),
    knownServerSources: new Set(),
  };

  return [
    transformPlugin(rpcEndpoint, state),
    ...devServerPlugin(options, state),
    buildPlugin(state),
  ];
}

export default zeroship;
```

- [ ] **Step 2: Build the plugin**

```bash
cd sdks/vite-plugin && npx tsc
```

Expected: compiles with no errors. `dist/index.js` is generated.

- [ ] **Step 3: Smoke test with existing example**

Verify the hr-system example still references the plugin correctly:

```bash
cd examples/hr-system && npx tsc --noEmit
```

Or just check that the import resolves:

```bash
node -e "const { zeroship } = require('/home/ruiyang/Projects/appbase/sdks/vite-plugin/dist/index.js'); console.log(typeof zeroship)"
```

Expected: `function`

- [ ] **Step 4: Commit**

```bash
git add sdks/vite-plugin/src/index.ts
git commit -m "refactor(vite-plugin): rewrite entry point to use extracted sub-plugins"
```

---

### Task 11: Verify eval() works in V8 runtime

Before claiming zero Rust changes, verify that `eval()` works in the zeroship V8 runtime.

**Files:**
- None (verification only, may need to modify `crates/runtime/src/runtime.rs` if eval is blocked)

- [ ] **Step 1: Create a test script**

```bash
cat > /tmp/test-eval.js << 'EOF'
export function onRequest(req) {
  try {
    const fn = (0, eval)('"use strict";(function(a,b){ return a + b; })');
    const result = fn(2, 3);
    return new Response(JSON.stringify({ eval_works: true, result }), {
      headers: { "Content-Type": "application/json" },
    });
  } catch (e) {
    return new Response(JSON.stringify({ eval_works: false, error: e.message }), {
      status: 500,
      headers: { "Content-Type": "application/json" },
    });
  }
}
EOF
```

- [ ] **Step 2: Build and run the test**

```bash
cd /home/ruiyang/Projects/appbase && cargo build --release -p zeroship-cli 2>/dev/null
./target/release/zeroship serve /tmp/test-eval.js --port 3099 &
sleep 1
curl -s http://localhost:3099/ | python3 -m json.tool
kill %1
```

Expected output: `{ "eval_works": true, "result": 5 }`

If eval is blocked: `{ "eval_works": false, "error": "..." }`

- [ ] **Step 3: If eval is blocked, add dev mode callback**

Only needed if step 2 shows `eval_works: false`. In `crates/runtime/src/runtime.rs`, after the isolate is created (around line 254):

```rust
// Allow eval() in dev mode for Vite's ModuleRunner
if std::env::var("ZEROSHIP_DEV").is_ok() {
    isolate.set_allow_code_generation_from_strings_callback(
        |_context| true
    );
}
```

Rebuild and re-test.

- [ ] **Step 4: Commit (only if Rust change was needed)**

```bash
git add crates/runtime/src/runtime.rs
git commit -m "feat(runtime): allow eval() in dev mode for Vite ModuleRunner"
```

---

### Task 12: End-to-end integration test

Test the full flow: Vite dev server → WebSocket → zeroship runtime → ModuleRunner → user module.

**Files:**
- Create: `sdks/vite-plugin/test/e2e-dev.ts` (manual test script)

- [ ] **Step 1: Create a minimal test app**

```bash
mkdir -p /tmp/zeroship-vite-test/src
```

```bash
cat > /tmp/zeroship-vite-test/src/index.ts << 'EOF'
"use server";

export function onRequest(req: any) {
  const url = new URL(req.url);
  if (url.pathname === "/api/hello") {
    return new Response(JSON.stringify({ message: "Hello from zeroship V8!" }), {
      headers: { "Content-Type": "application/json" },
    });
  }
  return new Response("Not found", { status: 404 });
}
EOF
```

```bash
cat > /tmp/zeroship-vite-test/vite.config.ts << 'EOF'
import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [zeroship()],
});
EOF
```

```bash
cat > /tmp/zeroship-vite-test/package.json << 'EOF'
{
  "name": "zeroship-vite-test",
  "type": "module",
  "dependencies": {
    "@zeroship/vite-plugin": "file:../../sdks/vite-plugin"
  },
  "devDependencies": {
    "vite": "^8.0.0"
  }
}
EOF
```

- [ ] **Step 2: Install and start**

```bash
cd /tmp/zeroship-vite-test && npm install
npx vite dev &
sleep 3
```

- [ ] **Step 3: Verify the connection**

Check Vite console output for:
```
[zeroship] API server starting on :3001
[zeroship] Runtime connected via WebSocket
[zeroship:dev] ModuleRunner ready, entry: src/index.ts
```

- [ ] **Step 4: Test a request**

```bash
curl -s http://localhost:5173/_rpc -X POST -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"hello","params":[],"id":1}'
```

Or test the onRequest path:

```bash
curl -s http://localhost:5173/api/hello
```

Expected: `{"message":"Hello from zeroship V8!"}`

- [ ] **Step 5: Test HMR**

Modify the test app's response:

```bash
sed -i 's/Hello from zeroship V8!/Updated via HMR!/' /tmp/zeroship-vite-test/src/index.ts
sleep 1
curl -s http://localhost:5173/api/hello
```

Expected: `{"message":"Updated via HMR!"}` (without runtime restart)

- [ ] **Step 6: Clean up**

```bash
kill %1
rm -rf /tmp/zeroship-vite-test /tmp/test-eval.js
```

- [ ] **Step 7: Commit any fixes found during e2e testing**

```bash
cd /home/ruiyang/Projects/appbase
git add -A sdks/vite-plugin/
git commit -m "fix(vite-plugin): fixes from e2e integration testing"
```

---

### Task 13: Update package.json metadata

Update the plugin's package.json to reflect the new architecture and dependencies.

**Files:**
- Modify: `sdks/vite-plugin/package.json`

- [ ] **Step 1: Update package.json**

```json
{
  "name": "@zeroship/vite-plugin",
  "version": "0.3.0",
  "description": "Vite plugin for zeroship — full-stack apps with Environment API + ModuleRunner in V8",
  "type": "module",
  "main": "dist/index.js",
  "types": "dist/index.d.ts",
  "exports": {
    ".": {
      "types": "./dist/index.d.ts",
      "import": "./dist/index.js"
    }
  },
  "files": ["dist", "src"],
  "scripts": {
    "build": "tsc && node --import tsx scripts/build-bootstrap.ts",
    "build:bootstrap": "node --import tsx scripts/build-bootstrap.ts",
    "dev": "tsc --watch"
  },
  "dependencies": {
    "ws": "^8.0.0"
  },
  "peerDependencies": {
    "vite": ">=6.0.0"
  },
  "devDependencies": {
    "vite": "^8.0.0",
    "typescript": "^5.0.0",
    "@types/node": "^22.0.0",
    "@types/ws": "^8.0.0",
    "esbuild": "^0.25.0",
    "tsx": "^4.0.0"
  },
  "license": "MIT"
}
```

Key changes:
- Version bump to 0.3.0
- `ws` added to dependencies
- `vite` peer dep bumped to >=6.0.0 (Environment API requires Vite 6+)
- `esbuild` and `tsx` added to devDependencies
- Build script includes bootstrap bundling

- [ ] **Step 2: Rebuild everything**

```bash
cd sdks/vite-plugin && npm install && npm run build
```

- [ ] **Step 3: Commit**

```bash
git add sdks/vite-plugin/package.json sdks/vite-plugin/package-lock.json
git commit -m "chore(vite-plugin): bump to v0.3.0, update deps for Environment API"
```
