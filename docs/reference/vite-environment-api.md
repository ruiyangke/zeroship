# @zeroship/vite-plugin — Vite Environment API Integration

## Overview

Upgrade `@zeroship/vite-plugin` from child-process+proxy dev server to Vite's Environment API with ModuleRunner executing inside the zeroship V8 runtime. This gives true runtime parity in development — code runs in the same V8 environment as production, with granular HMR (no isolate restart).

## Goals

1. **True runtime parity** — dev code runs in the actual zeroship V8 runtime with all `env.*` namespaces and the `zeroship` module available
2. **Granular HMR** — module-level invalidation via Vite's ModuleRunner, no process restart
3. **Standard Vite integration** — uses the official Environment API (Vite 6+), same pattern as `@cloudflare/vite-plugin`
4. **Minimal Rust changes** — all complexity lives in JS/TypeScript; Rust only needs eval() enabled

## Architecture

Two processes connected by WebSocket. ModuleRunner lives inside V8.

```
                          ┌─────────────────────────────────┐
                          │     Vite Dev Server (Node.js)    │
                          │                                  │
                          │  ┌───────────────────────────┐   │
Browser ──── HTTP ──────► │  │  "use server" transforms  │   │
  (client code,           │  │  (existing — unchanged)   │   │
   Vite HMR)              │  └───────────────────────────┘   │
                          │                                  │
                          │  ┌───────────────────────────┐   │
                          │  │ ZeroshipDevEnvironment     │   │
                          │  │   moduleGraph              │   │
                          │  │   fetchModule()            │◄──┼── invoke (module fetch)
                          │  │   HotChannel ──────────────┼───┼── WebSocket ──┐
                          │  └───────────────────────────┘   │               │
                          │                                  │               │
                          │  ┌───────────────────────────┐   │               │
  /_rpc, /api/* ────────► │  │ Proxy middleware           │───┼── HTTP ─┐    │
                          │  └───────────────────────────┘   │         │    │
                          └─────────────────────────────────┘         │    │
                                                                      │    │
                          ┌───────────────────────────────────────────┤    │
                          │     Zeroship Runtime (Rust + V8)          │    │
                          │                                           │    │
                          │  ┌─────────────────────────────────┐      │    │
                          │  │ Dev Bootstrap Module (JS)        │◄─────┘    │
                          │  │   ModuleRunner (bundled)        │◄───────────┘
                          │  │   ModuleEvaluator (eval-based)  │
                          │  │   WebSocket transport → Vite    │
                          │  │                                 │
                          │  │   onRequest(req):               │
                          │  │     mod = runner.import(entry)  │
                          │  │     return mod.onRequest(req)   │
                          │  └─────────────────────────────────┘
                          │                                     │
                          │  compio HTTP server (:3001)          │
                          │  V8 isolate + env.* + zeroship mod    │
                          └─────────────────────────────────────┘
```

### Key Design Decisions

1. **Single WebSocket** carries both HMR events and module fetch (invoke). Piggybacks on Vite's HTTP server at `/__zeroship_hmr` — no extra port.

2. **Dev bootstrap module** — pre-built JS file shipped with the plugin npm package. Contains bundled `vite/module-runner`, WebSocket transport, and eval-based ModuleEvaluator. Replaces user code as the V8 entry point in dev mode.

3. **Module evaluation** — ModuleRunner uses `eval()` to execute Vite-transformed code inside V8. User code runs in the real zeroship runtime with all `env.*` namespaces (and the `zeroship` named-import module) available. Same pattern as Cloudflare's `__VITE_UNSAFE_EVAL__`.

4. **Request flow**: Browser → Vite middleware → HTTP proxy → zeroship runtime → dev bootstrap → ModuleRunner imports user module → calls handler → response back through proxy.

5. **"use server" transforms** stay in the existing `zeroship:transform` plugin. Unchanged by this work.

## File Structure

```
sdks/vite-plugin/
  package.json
  tsconfig.json
  src/
    index.ts              — Plugin factory: exports zeroship()
    transform.ts          — "use server" taint analysis + RPC stubs (extracted from current index.ts)
    environment.ts        — ZeroshipDevEnvironment extends DevEnvironment, HotChannel
    dev-server.ts         — configureServer: spawn runtime, WS bridge, proxy middleware
    build.ts              — Production build with Rolldown/esbuild (extracted from current index.ts)
    constants.ts          — WS_PATH, env var names, headers
  src/dev-bootstrap/
    index.ts              — Entry: ModuleRunner setup, onRequest handler
    evaluator.ts          — ZeroshipModuleEvaluator (eval-based, for V8)
    transport.ts          — WebSocket transport to Vite HotChannel
  scripts/
    build-bootstrap.ts    — Bundles dev-bootstrap/ into dist/dev-bootstrap.js
  dist/
    index.js              — Plugin (built)
    index.d.ts            — Types
    dev-bootstrap.js      — Pre-bundled bootstrap for V8 runtime
```

## Component Design

### 1. Plugin Factory (`index.ts`)

Returns an array of Vite plugins:

```typescript
export function zeroship(options: ZeroshipOptions = {}): Plugin[] {
  return [
    transformPlugin(options),   // "use server" transforms (unchanged)
    environmentPlugin(options), // NEW: register zeroship environment
    devServerPlugin(options),   // REWRITTEN: Environment API + WS bridge
    buildPlugin(options),       // Production build (unchanged)
  ];
}
```

### 2. Environment Plugin (`environment.ts`)

Registers the `zeroship` environment in Vite's config.

**Environment options:**

```typescript
environments: {
  zeroship: {
    resolve: {
      conditions: ["zeroship", "worker", "module"],
      noExternal: true,
    },
    dev: {
      createEnvironment(name, config) {
        return new ZeroshipDevEnvironment(name, config);
      },
    },
    build: {
      target: "es2024",
      ssr: true,
    },
    keepProcessEnv: true,
  },
}
```

**ZeroshipDevEnvironment:**

Extends `vite.DevEnvironment`. Manages the HotChannel and WebSocket lifecycle.

```typescript
class ZeroshipDevEnvironment extends vite.DevEnvironment {
  #wsContainer: { ws?: WebSocket; buffer: string[] };

  constructor(name: string, config: vite.ResolvedConfig) {
    const wsContainer = { buffer: [] };
    super(name, config, {
      hot: true,
      transport: createHotChannel(wsContainer),
    });
    this.#wsContainer = wsContainer;
  }

  setWebSocket(ws: WebSocket): void {
    this.#wsContainer.ws = ws;
    for (const msg of this.#wsContainer.buffer) ws.send(msg);
    this.#wsContainer.buffer = [];
  }
}
```

**HotChannel:**

WebSocket-backed, buffers messages until connection is established.

```typescript
function createHotChannel(container: WsContainer): vite.HotChannel {
  const listeners = new Map<string, Set<vite.HotChannelListener>>();

  const client: vite.HotChannelClient = {
    send(payload) {
      const msg = JSON.stringify(payload);
      if (!container.ws) { container.buffer.push(msg); return; }
      container.ws.send(msg);
    },
  };

  return {
    send(payload) { client.send(payload); },
    on(event, listener) { /* add to listeners map */ },
    off(event, listener) { /* remove from listeners map */ },
    listen() { container.ws?.on("message", onMessage); },
    close() { container.ws?.off("message", onMessage); },
  };
}
```

### 3. Dev Server Plugin (`dev-server.ts`)

Replaces the current child-process+proxy implementation.

**`configureServer` hook:**

1. Accept WebSocket upgrades on `/__zeroship_hmr`:

```typescript
server.httpServer.on("upgrade", (req, socket, head) => {
  if (new URL(req.url, "http://localhost").pathname === WS_PATH) {
    wss.handleUpgrade(req, socket, head, (ws) => {
      zeroshipEnv.setWebSocket(ws);
    });
  }
});
```

2. Spawn zeroship runtime:

```typescript
const bootstrapPath = resolve(__dirname, "dev-bootstrap.js");
serverProcess = spawn("zeroship", ["serve", bootstrapPath, `--port=${devPort}`, "--workers=1"], {
  cwd: root,
  env: {
    ...process.env,
    ZEROSHIP_DEV: "1",
    ZEROSHIP_VITE_WS: `ws://localhost:${server.config.server.port}${WS_PATH}`,
    ZEROSHIP_ENTRY: findServerEntry(),
  },
  stdio: ["ignore", "pipe", "pipe"],
});
```

3. Register proxy middleware:

```typescript
return () => {
  server.middlewares.use((req, res, next) => {
    if (!isServerRoute(req.url)) return next();
    proxyToRuntime(req, res, devPort);
  });
};
```

**`handleHotUpdate` hook:**

For server file changes, notify the zeroship environment (no process restart):

```typescript
handleHotUpdate({ file, modules }) {
  // ModuleRunner handles invalidation via HMR channel — no manual restart needed
  // Only clear the server module cache for "use server" re-detection
  if (isServerFile(file)) serverModuleCache.delete(file);
}
```

**`buildEnd` hook:**

Kill the runtime child process on shutdown.

### 4. Dev Bootstrap Module (`dev-bootstrap/`)

Pre-bundled JS that runs inside the zeroship V8 runtime. Contains the bundled `vite/module-runner` package.

**Entry (`index.ts`):**

```typescript
import { createRunner } from "./transport";

const runner = await createRunner();
const ENTRY = process.env.ZEROSHIP_ENTRY;

export async function onRequest(req) {
  const mod = await runner.import(ENTRY);
  if (typeof mod.onRequest === "function") return mod.onRequest(req);
  return new Response("No onRequest handler", { status: 404 });
}
```

**Evaluator (`evaluator.ts`):**

Eval-based module execution for V8. Same pattern as Cloudflare's `__VITE_UNSAFE_EVAL__`.

```typescript
import { ssrModuleExportsKey } from "vite/module-runner";

export const zeroshipEvaluator = {
  async runInlinedModule(context, code, module) {
    const keys = Object.keys(context).join(",");
    const wrapped = `"use strict";async (${keys})=>{${code}\n}`;
    const fn = (0, eval)(wrapped);
    await fn(...Object.values(context));
    Object.seal(context[ssrModuleExportsKey]);
  },

  async runExternalModule(filepath) {
    return import(filepath);
  },
};
```

Key: uses indirect eval `(0, eval)(...)` for global scope. The trailing `\n` ensures `//` comments on the last line don't swallow the closing brace.

**Transport (`transport.ts`):**

WebSocket transport connecting to Vite's HotChannel.

```typescript
import { ModuleRunner } from "vite/module-runner";
import { zeroshipEvaluator } from "./evaluator";

export async function createRunner(): Promise<ModuleRunner> {
  const wsUrl = process.env.ZEROSHIP_VITE_WS;
  const ws = new WebSocket(wsUrl);

  await new Promise<void>((resolve, reject) => {
    ws.addEventListener("open", () => resolve());
    ws.addEventListener("error", reject);
  });

  // Send + connect only. Vite synthesizes invoke() from these
  // with built-in request/response correlation (no custom protocol needed).
  const transport = {
    connect({ onMessage }) {
      ws.addEventListener("message", ({ data }) => {
        onMessage(JSON.parse(data));
      });
    },
    send(data) {
      ws.send(JSON.stringify(data));
    },
  };

  return new ModuleRunner(
    { transport, hmr: true, sourcemapInterceptor: "prepareStackTrace" },
    zeroshipEvaluator,
  );
}
```

**Bundling:**

The `dev-bootstrap/` directory is bundled at build time (`scripts/build-bootstrap.ts`) using esbuild:

```typescript
await esbuild.build({
  entryPoints: ["src/dev-bootstrap/index.ts"],
  bundle: true,
  format: "esm",
  platform: "neutral",     // No Node.js builtins
  target: "es2024",
  outfile: "dist/dev-bootstrap.js",
  external: [],            // Bundle everything including vite/module-runner
});
```

The output is a single ESM file (~30KB) that the zeroship V8 runtime can load directly.

### 5. Rust Runtime Changes

**One change: ensure `eval()` is not disabled in V8.**

The V8 isolate may have `SetAllowCodeGenerationFromStringsCallback` set to block `eval()` for security. In dev mode (detected via `ZEROSHIP_DEV=1` env var), this callback should allow eval.

```rust
// In runtime creation, if dev mode:
isolate.set_allow_code_generation_from_strings_callback(|_| true);
```

In production, eval remains blocked (security best practice for user code).

No other Rust changes. The runtime already provides:
- `process.env` polyfill (passes `ZEROSHIP_VITE_WS`, `ZEROSHIP_ENTRY`)
- WebSocket client (RFC 6455)
- `fetch()` (if needed as fallback)
- Top-level `await` in ES modules
- `onRequest` HTTP dispatch

### 6. Request Dispatch Flow

**Server request (e.g., `POST /_rpc`):**

```
1. Browser sends POST /_rpc to Vite (:5173)
2. Vite middleware: isServerRoute("/_rpc") → true
3. Proxy request to zeroship runtime (:3001)
4. Runtime parses HTTP, dispatches to V8: dispatch_http("POST", "/_rpc", ...)
5. V8 calls dev bootstrap's onRequest(req)
6. Bootstrap: runner.import("src/index.ts")
   a. ModuleRunner checks cache — if cached, return
   b. If not cached: transport.invoke() → WebSocket → Vite → fetchModule()
   c. Vite transforms src/index.ts through plugin pipeline
   d. Sends transformed code back via WebSocket
   e. Evaluator: eval(transformed) → module loaded in V8
7. Bootstrap calls mod.onRequest(req)
8. User handler runs with real `env.*` namespaces (and the `zeroship` named-import module) available
9. Response flows back: V8 → Rust HTTP → proxy → Vite → browser
```

**Module caching**: After first import, ModuleRunner caches the module. Subsequent requests skip the Vite roundtrip — only eval'd once.

### 7. HMR Flow

```
1. Developer saves src/api.ts
2. Vite file watcher detects change
3. ZeroshipDevEnvironment.hot.send({ type: "update", updates: [...] })
4. HotChannel → WebSocket → V8 bootstrap
5. ModuleRunner receives update via transport.connect({ onMessage })
6. Runner invalidates src/api.ts in its evaluated modules cache
7. Next request: runner.import("src/index.ts") re-imports the dependency graph
8. src/api.ts is re-fetched from Vite (fresh transform) and re-evaluated via eval()
9. Updated handler is active — no process restart, no V8 isolate restart
```

**With `import.meta.hot.accept()`**: If user modules opt into fine-grained HMR (via a virtual entry wrapper), only the changed module is re-evaluated. Without it, the full module tree from the entry point is invalidated.

### 8. Production Build

Unchanged. The existing `zeroship:build` plugin uses esbuild to bundle server code. The Environment API is dev-only; production bundles go through the existing pipeline (or Vite's build environment if we want to unify later).

## Dependencies

**New npm dependencies for the plugin:**

```json
{
  "dependencies": {
    "ws": "^8.0.0"
  },
  "peerDependencies": {
    "vite": ">=6.0.0"
  }
}
```

`vite/module-runner` is bundled into `dev-bootstrap.js` at build time — not a runtime dependency.

**No new Rust crate dependencies.**

## What Changes vs. Current Plugin

| Aspect | Current (`v0.2.0`) | New |
|---|---|---|
| Dev server | `spawn("zeroship", ["serve", entry])` + HTTP proxy | `spawn("zeroship", ["serve", "dev-bootstrap.js"])` + WS + proxy |
| Module loading | V8 loads pre-compiled user code | ModuleRunner fetches from Vite, eval() in V8 |
| HMR | Kill + restart child process (300ms delay) | Module invalidation via ModuleRunner (instant) |
| Environment API | Not used | `ZeroshipDevEnvironment extends DevEnvironment` |
| "use server" transforms | `zeroship:transform` plugin | Unchanged |
| Production build | esbuild via `closeBundle` | Unchanged |
| Runtime parity | Same V8 runtime | Same V8 runtime (unchanged) |
| Rust changes | None | `eval()` allowed in dev mode (1 line) |

## Scope Exclusions

- **"use server" transform rework** — separate effort, existing transforms stay
- **Production build via Vite Environment** — build path stays esbuild-based for now
- **Multi-worker dev mode** — dev uses `--workers=1` (single V8 isolate)
- **Client HMR** — handled by Vite's built-in client environment (already works)
