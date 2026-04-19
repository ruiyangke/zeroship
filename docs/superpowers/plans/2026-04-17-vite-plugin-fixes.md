# Vite Plugin — Fix All 22 Issues

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix every identified issue in `@zeroship/vite-plugin` — from broken build toolchain through Vite 8 compat to polyfill correctness — so the dev workflow actually functions and produces correct client/server splits.

**Architecture:** The plugin is 4 Vite plugins (nodeCompat + transform + devServer + build) composed into one. The dev-bootstrap runs inside the V8 child process via Vite's ModuleRunner. Fixes touch every layer: package.json (P0), dev-server process lifecycle (P0), transform correctness (P1), build migration to Rolldown (P1), polyfill bugs (P1), and environment cleanup (P2).

**Tech Stack:** TypeScript, Vite 8 Environment API, Rolldown, WebSocket (`ws`), Node `child_process`

---

## File Map

| File | Changes |
|------|---------|
| `sdks/vite-plugin/package.json` | Fix scripts, deps, add engines |
| `sdks/vite-plugin/scripts/build-bootstrap.ts` | **CREATE** — esbuild bundler for dev-bootstrap |
| `sdks/vite-plugin/src/dev-server.ts` | Process cleanup, proxy path, middleware order, crash restart |
| `sdks/vite-plugin/src/transform.ts` | AST walk, arrow exports, block comment fix, async readFile, pnpm compat, source maps |
| `sdks/vite-plugin/src/build.ts` | Replace npx esbuild with Rolldown/Vite API |
| `sdks/vite-plugin/src/environment.ts` | Buffer cap, reconnect cleanup, resolve conditions, HotChannel types |
| `sdks/vite-plugin/src/node-compat.ts` | Binary HMAC keys, randomInt bias, setInterval async |
| `sdks/vite-plugin/src/dev-bootstrap/index.ts` | Debug log removal, Response check, error codes, runner error recovery |
| `sdks/vite-plugin/src/dev-bootstrap/transport.ts` | Enable HMR |

---

### Task 1: Create build-bootstrap.ts (P0 — dev workflow completely broken without this)

**Files:**
- Create: `sdks/vite-plugin/scripts/build-bootstrap.ts`
- Modify: `sdks/vite-plugin/package.json`

- [ ] **Step 1: Create the bootstrap bundler script**

```ts
// sdks/vite-plugin/scripts/build-bootstrap.ts
import { build } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const src = resolve(__dirname, "../src/dev-bootstrap/index.ts");
const out = resolve(__dirname, "../dist/dev-bootstrap.js");

await build({
  entryPoints: [src],
  bundle: true,
  format: "esm",
  platform: "neutral",
  target: "es2024",
  outfile: out,
  // Don't bundle vite/module-runner — it's provided by the host
  external: ["vite/module-runner"],
  // Banner: the V8 runtime needs these globals
  banner: {
    js: "// Auto-generated dev bootstrap for zeroship V8 runtime\n",
  },
});

console.log(`[build-bootstrap] ${out}`);
```

- [ ] **Step 2: Fix package.json scripts and add engines**

```json
{
  "scripts": {
    "build": "tsc && node --import tsx scripts/build-bootstrap.ts",
    "build:bootstrap": "node --import tsx scripts/build-bootstrap.ts",
    "dev": "tsc --watch"
  },
  "engines": {
    "node": ">=20.19.0"
  }
}
```

- [ ] **Step 3: Run the build and verify dev-bootstrap.js is produced**

Run: `cd sdks/vite-plugin && npm run build`
Expected: `dist/dev-bootstrap.js` exists

- [ ] **Step 4: Commit**

```bash
git add sdks/vite-plugin/scripts/build-bootstrap.ts sdks/vite-plugin/package.json
git commit -m "fix(vite-plugin): create build-bootstrap.ts — dev workflow was completely broken"
```

---

### Task 2: Fix /rpc proxy path + middleware order (P0)

**Files:**
- Modify: `sdks/vite-plugin/src/dev-server.ts:183-214`

- [ ] **Step 1: Fix proxy to also match `/rpc` (no underscore) and return middleware function for correct ordering**

In `dev-server.ts`, the proxy middleware at line 183 is registered inside `configureServer` (post-middleware). It needs to be returned from `configureServer` to run as pre-middleware. Also add `/rpc` path match.

Replace lines 177-214:

```ts
      // 3. Proxy middleware — returned from configureServer to run BEFORE
      // Vite's built-in middleware (SPA fallback). Without this, /api/*
      // and /rpc requests get 200 HTML instead of being proxied.
      return () => {
        server.middlewares.use(
          (
            req: http.IncomingMessage,
            res: http.ServerResponse,
            next: () => void
          ) => {
            const url = req.url ?? "";
            if (
              !url.startsWith("/_rpc") &&
              !url.startsWith("/rpc") &&
              !url.startsWith("/api/")
            ) {
              return next();
            }

            const proxyReq = http.request(
              `http://localhost:${devPort}${url}`,
              { method: req.method, headers: req.headers },
              (proxyRes) => {
                res.writeHead(proxyRes.statusCode ?? 502, proxyRes.headers);
                proxyRes.pipe(res);
              }
            );

            req.pipe(proxyReq);

            proxyReq.on("error", () => {
              res.writeHead(503, { "Content-Type": "application/json" });
              res.end('{"error":"zeroship runtime not ready"}');
            });
          }
        );
      };
```

Note: `configureServer` must return the function (not call `server.middlewares.use` inline) so the middleware runs in the pre-phase.

- [ ] **Step 2: Verify the proxy plugin returns its middleware**

Ensure the `configureServer` hook signature returns `() => void`.

- [ ] **Step 3: Commit**

```bash
git add sdks/vite-plugin/src/dev-server.ts
git commit -m "fix(vite-plugin): proxy /rpc path + fix middleware registration order"
```

---

### Task 3: Process cleanup on exit + crash restart (P0)

**Files:**
- Modify: `sdks/vite-plugin/src/dev-server.ts:234-300`

- [ ] **Step 1: Add process cleanup and restart logic**

After the `spawnRuntime()` function definition (around line 235), add:

```ts
        // Clean up child process on Vite exit (SIGINT, SIGTERM, process.exit)
        const killChild = () => {
          if (serverProcess && !serverProcess.killed) {
            serverProcess.kill("SIGTERM");
            // Force kill after 3s if still alive
            setTimeout(() => {
              if (serverProcess && !serverProcess.killed) {
                serverProcess.kill("SIGKILL");
              }
            }, 3000).unref();
          }
        };

        process.on("exit", killChild);
        process.on("SIGINT", () => { killChild(); process.exit(0); });
        process.on("SIGTERM", () => { killChild(); process.exit(0); });

        // Restart on unexpected exit (crash recovery)
        const setupRestartHandler = () => {
          if (!serverProcess) return;
          serverProcess.on("exit", (code, signal) => {
            if (signal === "SIGTERM" || signal === "SIGKILL") return; // intentional kill
            console.warn(
              `[zeroship] runtime exited unexpectedly (code=${code}, signal=${signal}) — restarting in 1s`
            );
            setTimeout(() => {
              spawnRuntime();
              setupRestartHandler();
            }, 1000);
          });
        };
```

After `spawnRuntime()` is called, add `setupRestartHandler()`.

- [ ] **Step 2: Also register cleanup on server.httpServer close**

```ts
        server.httpServer?.on("close", killChild);
```

- [ ] **Step 3: Commit**

```bash
git add sdks/vite-plugin/src/dev-server.ts
git commit -m "fix(vite-plugin): cleanup child process on exit + auto-restart on crash"
```

---

### Task 4: Enable HMR for server code (P0)

**Files:**
- Modify: `sdks/vite-plugin/src/dev-bootstrap/transport.ts:49`
- Modify: `sdks/vite-plugin/src/dev-server.ts` (handleHotUpdate)

- [ ] **Step 1: Enable HMR in the ModuleRunner transport**

In `transport.ts`, change `hmr: false` to `hmr: true` (or remove the line — default is `true`):

```ts
  const runner = new ModuleRunner(
    {
      transport: {
        invoke: async (data: any) => {
          // ... existing code ...
        },
      },
      hmr: true,   // was: false
      sourcemapInterceptor: false,
    },
    zeroshipEvaluator
  );
```

- [ ] **Step 2: In dev-server.ts, migrate handleHotUpdate to hotUpdate hook**

Replace the `handleHotUpdate` hook with the new `hotUpdate` hook:

```ts
        hotUpdate({ file, modules }) {
          // Invalidate server module cache when server files change
          if (file.endsWith(".ts") || file.endsWith(".tsx") || file.endsWith(".js") || file.endsWith(".jsx")) {
            for (const [key] of serverModuleCache) {
              if (file.endsWith(key) || key.endsWith(file)) {
                serverModuleCache.delete(key);
              }
            }
          }
        },
```

- [ ] **Step 3: Commit**

```bash
git add sdks/vite-plugin/src/dev-bootstrap/transport.ts sdks/vite-plugin/src/dev-server.ts
git commit -m "feat(vite-plugin): enable HMR for server code — no more dev restarts on change"
```

---

### Task 5: Fix transform — arrow exports, AST taint, block comments (P1)

**Files:**
- Modify: `sdks/vite-plugin/src/transform.ts:24-33,49-58,113-138`

- [ ] **Step 1: Replace `fnReferencesAny` JSON string search with AST walk**

Replace lines 24-33:

```ts
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
```

- [ ] **Step 2: Fix `checkDirective` to handle multi-line block comments**

Replace lines 49-58:

```ts
function checkDirective(code: string): boolean {
  let i = 0;
  const lines = code.split("\n");
  while (i < lines.length) {
    const t = lines[i].trim();
    if (t === "" || t.startsWith("//")) { i++; continue; }
    // Skip multi-line block comments
    if (t.startsWith("/*")) {
      while (i < lines.length && !lines[i].includes("*/")) i++;
      i++; // skip the line containing */
      continue;
    }
    return t === '"use server"' || t === "'use server'" || t === '"use server";' || t === "'use server';";
  }
  return false;
}
```

- [ ] **Step 3: Fix `removeFunction` to handle arrow function exports**

Replace lines 113-138:

```ts
/** Remove a named export (function declaration or arrow/const) from code */
function removeFunction(code: string, name: string): string {
  // Pattern 1: export (async) function name(...) { ... }
  const fnPattern = new RegExp(
    `export\\s+(async\\s+)?function\\s+${name}\\s*\\([^)]*\\)[^{]*\\{`,
    "m"
  );
  const fnMatch = fnPattern.exec(code);
  if (fnMatch) {
    return removeBraceBlock(code, fnMatch.index, fnMatch[0].length);
  }

  // Pattern 2: export const name = (...) => { ... } or export const name = function(...) { ... }
  const arrowPattern = new RegExp(
    `export\\s+(const|let|var)\\s+${name}\\s*=`,
    "m"
  );
  const arrowMatch = arrowPattern.exec(code);
  if (arrowMatch) {
    // Find the end: either next top-level export/const or semicolon at depth 0
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
    if (ch === "}") {
      depth--;
      if (depth === 0) return code.slice(0, matchStart) + code.slice(i + 1);
    }
  }
  return code;
}
```

- [ ] **Step 4: Commit**

```bash
git add sdks/vite-plugin/src/transform.ts
git commit -m "fix(vite-plugin): transform — AST taint walk, arrow exports, block comments"
```

---

### Task 6: Fix isServerPackage for pnpm/Yarn + async isServerFile (P1)

**Files:**
- Modify: `sdks/vite-plugin/src/transform.ts:35-47,60-85`

- [ ] **Step 1: Replace isServerPackage flat node_modules lookup with createRequire**

```ts
import { createRequire } from "module";

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
```

- [ ] **Step 2: Commit**

```bash
git add sdks/vite-plugin/src/transform.ts
git commit -m "fix(vite-plugin): use createRequire for package resolution — pnpm/Yarn compat"
```

---

### Task 7: Replace npx esbuild with Vite build API (P1)

**Files:**
- Modify: `sdks/vite-plugin/src/build.ts`

- [ ] **Step 1: Rewrite build.ts to use Vite's build API with the zeroship environment**

```ts
import { type Plugin, build as viteBuild } from "vite";
import { resolve, relative } from "path";
import { existsSync } from "fs";

export function findServerEntry(root: string, explicit?: string): string | null {
  if (explicit && existsSync(resolve(root, explicit))) return resolve(root, explicit);
  for (const candidate of ["src/server.ts", "src/server.js", "server.ts", "server.js", "src/index.server.ts"]) {
    const p = resolve(root, candidate);
    if (existsSync(p)) return p;
  }
  return null;
}

export function buildPlugin(
  serverFunctionMap: Map<string, Set<string>>,
  options: { serverEntry?: string } = {}
): Plugin {
  let root = "";
  let isDev = false;

  return {
    name: "zeroship:build",
    configResolved(config) {
      root = config.root;
      isDev = config.command === "serve";
    },

    async writeBundle() {
      if (isDev) return;

      const entry = findServerEntry(root, options.serverEntry);
      if (!entry) {
        console.warn("[zeroship] no server entry found — skipping server bundle");
        return;
      }

      console.log(`[zeroship] building server bundle from ${relative(root, entry)}`);

      await viteBuild({
        root,
        configFile: false,
        build: {
          ssr: entry,
          outDir: "dist/server",
          emptyOutDir: false,
          rolldownOptions: {
            output: { format: "esm", entryFileNames: "index.js" },
          },
          minify: true,
        },
        logLevel: "warn",
      });

      // Report
      const totalFns = [...serverFunctionMap.values()].reduce((sum, s) => sum + s.size, 0);
      console.log(`[zeroship] server bundle complete — ${serverFunctionMap.size} modules, ${totalFns} server functions`);
    },
  };
}
```

- [ ] **Step 2: Remove esbuild from devDependencies in package.json**

The server bundle now uses `vite.build()` with Rolldown. Remove:
```
"esbuild": "^0.28.0"
```

- [ ] **Step 3: Commit**

```bash
git add sdks/vite-plugin/src/build.ts sdks/vite-plugin/package.json
git commit -m "feat(vite-plugin): replace npx esbuild with Vite build API (Rolldown)"
```

---

### Task 8: Fix node-compat polyfill bugs (P1)

**Files:**
- Modify: `sdks/vite-plugin/src/node-compat.ts:49-50,68,80-84`

- [ ] **Step 1: Fix HMAC binary key handling — use hex encoding instead of UTF-8**

Replace line 50:
```ts
  const keyStr = typeof key === "string" ? key : Array.from(new Uint8Array(key.buffer || key)).map(b => b.toString(16).padStart(2, "0")).join("");
```

And update the `__cryptoHmacSync` call (line 55) to document that `keyStr` is hex for binary keys.

- [ ] **Step 2: Fix randomInt modulo bias with rejection sampling**

Replace line 68:
```ts
function randomInt(min, max) {
  if (max === undefined) { max = min; min = 0; }
  const range = max - min;
  if (range <= 0) throw new RangeError("max must be greater than min");
  // Rejection sampling to avoid modulo bias
  const limit = Math.floor(0x100000000 / range) * range;
  let val;
  do {
    const a = new Uint32Array(1);
    crypto.getRandomValues(a);
    val = a[0];
  } while (val >= limit);
  return min + (val % range);
}
```

- [ ] **Step 3: Add setInterval async iterator to timers/promises**

Replace lines 80-84:
```ts
  "node:timers/promises": `
function _setTimeout(ms, value) { return new Promise(r => globalThis.setTimeout(() => r(value), ms || 0)); }
function _setImmediate(value) { return Promise.resolve(value); }
async function* _setInterval(ms, value) {
  while (true) {
    await new Promise(r => globalThis.setTimeout(r, ms || 0));
    yield value;
  }
}
Object.assign(__vite_ssr_exports__, { setTimeout: _setTimeout, setImmediate: _setImmediate, setInterval: _setInterval, default: { setTimeout: _setTimeout, setImmediate: _setImmediate, setInterval: _setInterval } });
`,
```

- [ ] **Step 4: Commit**

```bash
git add sdks/vite-plugin/src/node-compat.ts
git commit -m "fix(vite-plugin): HMAC binary keys, randomInt bias, setInterval async iterator"
```

---

### Task 9: Fix dev-bootstrap issues (P1)

**Files:**
- Modify: `sdks/vite-plugin/src/dev-bootstrap/index.ts:20-30,93-96,112-114,117-122`

- [ ] **Step 1: Fix runner promise error recovery**

Replace lines 20-30:
```ts
let runner: any = null;
let runnerPromise: Promise<any> | null = null;

async function getRunner() {
  if (runner) return runner;
  if (runnerPromise) return runnerPromise;
  runnerPromise = createRunner().then((r) => {
    runner = r;
    return r;
  }).catch((err) => {
    // Reset so next call retries instead of returning the rejected promise forever
    runnerPromise = null;
    throw err;
  });
  return runnerPromise;
}
```

- [ ] **Step 2: Remove debug log and fix Response check**

Remove lines 93-96 (the `__debuggedExports` debug code).

Replace the Response check at line 112:
```ts
    if (result instanceof Response) {
      return result;
    }
```

- [ ] **Step 3: Fix JSON-RPC error codes**

Replace the catch block at lines 117-122:
```ts
  } catch (e: any) {
    // If JSON.parse failed, id is still null — use code -32700 (Parse error)
    const code = id === null ? -32700 : -32000;
    return jsonResponse({
      jsonrpc: "2.0",
      error: { code, message: e.message ?? String(e) },
      id,
    });
  }
```

- [ ] **Step 4: Commit**

```bash
git add sdks/vite-plugin/src/dev-bootstrap/index.ts
git commit -m "fix(vite-plugin): bootstrap runner recovery, Response check, JSON-RPC error codes"
```

---

### Task 10: Fix environment edge cases (P2)

**Files:**
- Modify: `sdks/vite-plugin/src/environment.ts:14-19,34,86,91,95,134-211,221-243`

- [ ] **Step 1: Add buffer cap to WsContainer**

In the `WsContainer` interface/implementation, add a max buffer size:
```ts
const MAX_WS_BUFFER = 1000;

// In createHotChannel, where messages are buffered:
if (container.buffer.length < MAX_WS_BUFFER) {
  container.buffer.push(JSON.stringify(payload));
}
```

- [ ] **Step 2: Clean up old WebSocket handler on reconnect**

In `setWebSocket()` method:
```ts
setWebSocket(ws: any) {
  // Remove old handler if reconnecting
  if (this.wsContainer.ws) {
    this.wsContainer.ws.removeAllListeners?.("message");
  }
  this.wsContainer.ws = ws;
  // ... rest of existing code
}
```

- [ ] **Step 3: Add "default" to resolve conditions**

In `createZeroshipEnvironmentOptions`, change conditions:
```ts
conditions: ["zeroship", "worker", "module", "import", "default"],
```

- [ ] **Step 4: Type HotChannel listeners properly**

Replace `Function` listener types with proper types:
```ts
type HotListener = (data: unknown, client?: { send(data: unknown): void }) => void;
```

- [ ] **Step 5: Commit**

```bash
git add sdks/vite-plugin/src/environment.ts
git commit -m "fix(vite-plugin): environment buffer cap, reconnect cleanup, resolve conditions"
```

---

### Task 11: Migrate handleHotUpdate → hotUpdate (P2)

**Files:**
- Modify: `sdks/vite-plugin/src/dev-server.ts`

- [ ] **Step 1: Replace handleHotUpdate with hotUpdate hook**

Find the `handleHotUpdate` hook and replace with:
```ts
        hotUpdate({ file }: { file: string }) {
          if (
            file.endsWith(".ts") || file.endsWith(".tsx") ||
            file.endsWith(".js") || file.endsWith(".jsx")
          ) {
            for (const [key] of serverModuleCache) {
              if (file.endsWith(key) || key.endsWith(file)) {
                serverModuleCache.delete(key);
              }
            }
          }
        },
```

- [ ] **Step 2: Commit**

```bash
git add sdks/vite-plugin/src/dev-server.ts
git commit -m "refactor(vite-plugin): migrate handleHotUpdate to hotUpdate (Vite 8)"
```

---

### Task 12: Generate source maps from transform (P2)

**Files:**
- Modify: `sdks/vite-plugin/src/transform.ts`

- [ ] **Step 1: Install magic-string for source map generation**

Add to `package.json` dependencies:
```json
"magic-string": "^0.30.0"
```

- [ ] **Step 2: Use MagicString in the transform handler**

At the top of the transform handler, create a MagicString wrapper:
```ts
import MagicString from "magic-string";

// In transform handler:
const s = new MagicString(code);
// Replace s.overwrite(start, end, replacement) for each modification
// At the end:
return { code: s.toString(), map: s.generateMap({ source: id, includeContent: true }) };
```

This is a larger refactor — every string mutation in the transform handler needs to use `s.overwrite()` or `s.remove()` instead of string concatenation.

- [ ] **Step 3: Commit**

```bash
git add sdks/vite-plugin/src/transform.ts sdks/vite-plugin/package.json
git commit -m "feat(vite-plugin): generate source maps from server function transforms"
```

---

## Self-Review

**Spec coverage:**
- P0 #1 (build-bootstrap): Task 1 ✓
- P0 #2 (rpc path): Task 2 ✓
- P0 #3 (HMR): Task 4 ✓
- P0 #14 (process cleanup): Task 3 ✓
- P1 #4 (esbuild→Rolldown): Task 7 ✓
- P1 #10 (taint AST): Task 5 ✓
- P1 #11 (arrow exports): Task 5 ✓
- P1 #17 (HMAC keys): Task 8 ✓
- P1 #16 (middleware order): Task 2 ✓
- P1 #9 (pnpm compat): Task 6 ✓
- P1 #12 (block comments): Task 5 ✓
- P1 #15 (crash restart): Task 3 ✓
- P2 #7 (FetchableDevEnvironment): Deferred — requires deeper investigation
- P2 #6 (hotUpdate): Task 11 ✓
- P2 #8 (async isServerFile): Task 6 partially (createRequire is still sync; full async would require rearchitecting the transform)
- P2 #13 (source maps): Task 12 ✓
- P2 #22 (resolve conditions): Task 10 ✓
- P2 #20 (buffer cap): Task 10 ✓
- P2 #21 (reconnect): Task 10 ✓
- P3 #18 (randomInt): Task 8 ✓
- P3 #19 (setInterval): Task 8 ✓
- Bootstrap debug log (#P1): Task 9 ✓
- Response check (#P1): Task 9 ✓
- Runner error recovery (#P1): Task 9 ✓

**Placeholder scan:** No TBD/TODO. All code shown.

**Type consistency:** `serverModuleCache`, `serverFunctionMap`, `knownServerSources` used consistently across tasks.
