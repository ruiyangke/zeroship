# appbase

**AI-native app platform — describe what you want, deploy in seconds.**

appbase runs JavaScript apps in sandboxed V8 isolates with built-in metering, multi-tenant routing, and a control plane API. Write your app in JavaScript or TypeScript — from a single file to a full multi-module project — deploy via API or dashboard, and call your functions over HTTP.

## Key Features

- **V8 Isolates** — each app runs in its own V8 isolate with CPU time limits, memory isolation, and automatic eviction
- **Multi-Tenant** — route requests to apps by subdomain, `X-App-Id` header, or default
- **JSON-RPC** — define functions in JS, call them via `POST /rpc` with JSON-RPC 2.0
- **Built-in Metering** — CPU time, request count, egress bytes, per-app quotas with IETF RateLimit headers
- **Fetch API** — full `fetch()` support inside isolates with SSRF protection
- **KV Store** — per-isolate in-memory key-value store (`kv.get`, `kv.set`, `kv.delete`, `kv.list`)
- **Plan-Based Quotas** — free and pro tiers with configurable limits in `appbase.toml`
- **Control Plane API** — create, deploy, delete apps; manage plans; view logs
- **Web Dashboard** — React-based UI for managing apps, viewing usage, deploying code
- **AI Agent** — describe what you want, the agent generates code, deploys, and tests it

## Quick Start

### 1. Build

```bash
cargo build --release
```

### 2. Create a server.js

```javascript
var __rpc = {
  ping: function() {
    return "pong";
  },
  hello: function(name) {
    return "Hello, " + (name || "world") + "!";
  }
};
```

### 3. Run

```bash
./target/release/appbase-cli serve server.js --port=3000
```

Your app is live. Call it:

```bash
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}'

# {"jsonrpc":"2.0","result":"pong","id":1}
```

## Architecture

```
                    ┌─────────────────────────────────────────────┐
                    │              appbase server                  │
                    │                                             │
  HTTP request      │  ┌──────────┐    ┌────────────────────┐    │
 ──────────────────►│  │  Router   │───►│  Enforcement       │    │
                    │  │  (axum)   │    │  - Rate limit      │    │
                    │  └──────────┘    │  - Concurrency     │    │
                    │       │          │  - Quota check      │    │
                    │       │          │  - Spending limit   │    │
                    │       ▼          └────────┬───────────┘    │
                    │  ┌──────────┐             │                │
                    │  │ Control  │             ▼                │
                    │  │  Plane   │    ┌────────────────────┐    │
                    │  │ (SQLite) │    │   V8 Pool          │    │
                    │  └──────────┘    │                    │    │
                    │                  │  ┌──────┐ ┌──────┐ │    │
                    │                  │  │App A │ │App B │ │    │
                    │                  │  │(V8)  │ │(V8)  │ │    │
                    │                  │  └──────┘ └──────┘ │    │
                    │                  └────────────────────┘    │
                    │                          │                 │
                    │                  ┌───────┴───────┐        │
                    │                  │   Metering    │        │
                    │                  │  (SQLite)     │        │
                    │                  └───────────────┘        │
                    └─────────────────────────────────────────────┘
```

## How It Works

### V8 Isolates

Each app gets its own V8 isolate running on a dedicated thread. Isolates are created lazily on the first request and evicted when idle (default: 60s) or when the pool reaches capacity (default: 1000). LRU eviction ensures the pool stays within bounds.

### Multi-Tenant Routing

Requests are routed to apps using three-level resolution:

1. **Subdomain** — `my-app.platform.dev` routes to app `my-app`
2. **Header** — `X-App-Id: my-app` header
3. **Default** — falls back to the `default` app

### Metering & Quotas

Every request is metered for CPU time, wall time, egress bytes, and ingress bytes. Usage counters are flushed to SQLite every 5 seconds and survive restarts. Plans define per-dimension quotas:

| Dimension | Free Tier | Pro Tier |
|-----------|-----------|----------|
| CPU time/mo | 50s | 30,000s |
| Requests/mo | 100,000 | 10,000,000 |
| CPU/request | 10ms | 30,000ms |
| Rate limit | 10 req/s (burst 50) | 1,000 req/s (burst 5,000) |

Responses include IETF RateLimit headers and quota warning headers when usage exceeds 80%.

### Request Lifecycle

1. Rate limit check
2. Concurrency guard (max 100 concurrent per app)
3. Spending limit check
4. Quota check (requests, CPU)
5. Load app bundle (cache or registry)
6. Dispatch to V8 isolate
7. Record usage metrics
8. Return JSON-RPC response with metering headers

## API Reference

See [docs/api-reference.md](docs/api-reference.md) for the full API reference.

### Summary

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/rpc` | Call an app's RPC method |
| `POST` | `/api/apps` | Create app |
| `GET` | `/api/apps` | List apps |
| `GET` | `/api/apps/:id` | Get app (includes code) |
| `DELETE` | `/api/apps/:id` | Delete app |
| `POST` | `/api/apps/:id/deploy` | Deploy JS code |
| `PUT` | `/api/apps/:id/plan` | Change plan |
| `GET` | `/api/apps/:id/logs` | Get console logs |
| `GET` | `/api/templates` | List starter templates |
| `GET` | `/_health` | Health check |
| `GET` | `/_stats` | Pool stats |
| `GET` | `/_usage` | All app usage |
| `GET` | `/_apps/:id/usage` | Per-app usage |
| `DELETE` | `/_apps/:id` | Evict isolate |

## App Format

Apps are ESM modules (single file or multi-file projects) that export functions or an `onRequest` handler. The runtime supports `import`/`export` across modules.

```javascript
var __rpc = {
  // Sync handler
  add: function(a, b) {
    return a + b;
  },

  // Async handler (returns a Promise)
  fetchWeather: async function(city) {
    var resp = await fetch("https://wttr.in/" + city + "?format=j1");
    var data = await resp.json();
    return {
      city: city,
      temp_c: data.current_condition[0].temp_C
    };
  }
};
```

Clients call methods via JSON-RPC 2.0:

```bash
curl -X POST http://localhost:3000/rpc \
  -H "Content-Type: application/json" \
  -H "X-App-Id: my-app" \
  -d '{"jsonrpc":"2.0","method":"add","params":[2,3],"id":1}'

# {"jsonrpc":"2.0","result":5,"id":1}
```

**Important:** Use `var` for top-level declarations (V8 runs in classic script mode, not module mode).

## Available JS APIs

The following globals are available inside V8 isolates:

### `fetch(url, options)`

Full Web Fetch API (Headers, Request, Response). SSRF-protected — blocks private IPs, loopback, and non-HTTP(S) schemes. Max response body: 10 MB.

```javascript
var resp = await fetch("https://api.example.com/data", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ key: "value" })
});
var data = await resp.json();
```

### `kv.get(key)` / `kv.set(key, value)` / `kv.delete(key)` / `kv.list()`

Per-isolate in-memory key-value store. Data persists across requests but is lost when the isolate is evicted.

```javascript
kv.set("counter", "0");
var count = parseInt(kv.get("counter") || "0") + 1;
kv.set("counter", String(count));
var allKeys = kv.list();
kv.delete("counter");
```

### `console.log()` / `console.warn()` / `console.error()` / `console.info()`

Logs are captured per-isolate (last 100 entries) and retrievable via `GET /api/apps/:id/logs`.

### `setTimeout(fn, ms)` / `clearTimeout(id)`

Schedule deferred execution within the isolate's event loop.

### `setInterval(fn, ms)` / `clearInterval(id)`

Repeating timers within the isolate's event loop.

### `env.get(key)`

Read per-app configuration from process environment variables. Keys are mapped to `APPBASE_APP_{KEY}` (uppercased). Returns the value as a string, or `null` if not set.

```javascript
// Set APPBASE_APP_API_KEY=secret123 in the server environment
var key = env.get("api_key"); // "secret123"
```

### Standard JS Built-ins

`Promise`, `async/await`, `JSON.parse`, `JSON.stringify`, `Array`, `Map`, `Set`, `Date`, `Math`, `RegExp`, and all other standard JavaScript built-in objects.

## Dashboard

The web dashboard provides a UI for managing your appbase instance.

The dashboard provides a single-page interface with a sidebar for navigation. The main views include a system overview with health indicators and usage graphs, an app list with inline status badges, a code editor with deploy button, and an AI chat panel for generating apps from natural language.

### Features

- **Overview** — system health, active isolates, aggregate usage
- **App List** — view all deployed apps with plan and version info
- **App Detail** — view/edit code, see usage metrics, view console logs
- **Create App** — deploy from starter templates or custom code
- **AI Chat** — describe what you want, the agent builds and deploys it

### Running the Dashboard

```bash
cd web/dashboard
npm install
npm run dev
```

The dashboard connects to the appbase API and authenticates with the master key.

## AI Agent

The AI agent generates and deploys apps from natural language descriptions. It uses Claude with custom tools to deploy, test, and iterate on app code.

### How to Use

1. Open the AI Chat page in the dashboard
2. Describe what you want: "Build a URL shortener" or "Create a weather API"
3. The agent generates code, deploys it, tests it, and returns working curl examples

### What It Does

1. Understands your request
2. Generates a `server.js` using the `__rpc` pattern
3. Deploys it via `POST /api/apps/:id/deploy`
4. Tests methods via `POST /rpc`
5. Iterates if tests fail
6. Reports the result with example commands

### Agent Tools

- `deploy_app` — deploy JS code to an app
- `test_app` — call an RPC method and check the result
- `list_apps` — list all deployed apps

## Configuration

### appbase.toml

```toml
[server]
port = 3000

[isolates]
max = 1000                # Max warm isolates in pool
idle_timeout_secs = 60    # Evict idle isolates after this
cpu_limit_ms = 50         # CPU time limit per request (ms)

[plans.free]
description = "Free tier"

[plans.free.quotas]
cpu_ms = { max = 50000, period = "monthly", policy = "warn_then_block" }
requests = { max = 100000, period = "monthly", policy = "warn_then_block" }
egress_bytes = { max = 1000000000, period = "monthly", policy = "warn_then_block" }
"db.reads" = { max = 500000, period = "monthly", policy = "warn_then_block" }
"db.writes" = { max = 50000, period = "monthly", policy = "warn_then_block" }
"kv.reads" = { max = 100000, period = "monthly", policy = "warn_then_block" }
"kv.writes" = { max = 100000, period = "monthly", policy = "warn_then_block" }
cpu_per_request = { max = 10, period = "per_request", policy = "hard_kill" }

[plans.free.rate_limits]
default = { max_per_second = 10, burst = 50, policy = "reject" }

[plans.pro]
description = "Pro tier"

[plans.pro.quotas]
cpu_ms = { max = 30000000, period = "monthly", policy = "warn_then_block" }
requests = { max = 10000000, period = "monthly", policy = "warn_then_block" }
cpu_per_request = { max = 30000, period = "per_request", policy = "hard_kill" }

[plans.pro.rate_limits]
default = { max_per_second = 1000, burst = 5000, policy = "reject" }

[defaults]
plan = "free"

[apps.default]
plan = "pro"
```

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `APPBASE_MASTER_KEY` | Master key for admin API authentication | `dev-master-key` |
| `APPBASE_APP_*` | Per-app config variables, readable via `env.get(key)` in JS (e.g., `APPBASE_APP_API_KEY` is read as `env.get("api_key")`) | — |

### CLI Arguments

```
appbase serve <server.js> [options]

Options:
  --port=PORT        HTTP port (default: 3000)
  --db=PATH          SQLite database path (default: appbase.db)
  --static=PATH      Static HTML file to serve on non-API routes
  --config=PATH      Config file path (default: appbase.toml)
```

## Development

### Prerequisites

- Rust (stable, edition 2021)
- Node.js (for the dashboard and agent)

### Building from Source

```bash
# Build all crates
cargo build --release

# Run tests
cargo test

# Run the server
./target/release/appbase-cli serve examples/test_app.js --port=3000

# Build the dashboard
cd web/dashboard && npm install && npm run build
```

### Project Structure

```
crates/
  cli/          — CLI entry point (appbase serve, appbase dev)
  server/       — Axum HTTP router, V8 pool, middleware
  core/         — Shared types, config, plugin trait
  control/      — App registry (SQLite/Postgres via sqlx)
  isolate_v8/   — V8 isolate runtime (globals, fetch, kv, timers, event loop)
  plugins/      — Built-in plugins (db, kv, env)
  metering/     — Usage counters, SQLite persistence, flusher, rollover
  enforcement/  — Rate limiter, concurrency guard, quota checks
  plan/         — Quota plan definitions (free, pro)
  billing/      — Spending reconciler, pricing table
  compiler/     — SWC-based JSX/TS compiler
agent/          — AI agent (TypeScript, deepagents/LangGraph)
web/dashboard/  — React dashboard (Vite + TypeScript)
examples/       — Example apps and config
```

## License

See the LICENSE file for details.
