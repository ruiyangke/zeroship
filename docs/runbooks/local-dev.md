# Local dev setup

Get a working zeroship stack on your machine, end to end. Single-tenant first, then multi-component.

## Prerequisites

- Rust toolchain (stable, edition 2021)
- Node.js 20+ (for the SDK packages and Vite plugin)
- PostgreSQL 14+ running locally (`postgres://localhost:5432/zeroship` is the convention)
- Redis 7+ (optional — only needed if your app uses `zeroship.kv`)
- `mkcert` if you want HTTPS locally

## First-time bootstrap

```bash
# 1. Build the workspace (release for perf testing; debug is faster to iterate)
cargo build --release

# 2. Create a local Postgres database
createdb zeroship

# 3. Initialize tables (the control plane migrates on first run, but you can pre-create)
# (no migration tool yet — control_plane runs idempotent CREATE/ALTER on boot)
```

## Mode 1 — single-tenant (no platform, just the runtime)

Fastest iteration loop. No control plane, no gateway. Just `zeroship serve` running your code in V8.

```bash
# Run a single JS file
./target/release/zeroship serve myapp.js --port 3000

# Or if you have a project dir with package.json
./target/release/zeroship serve ./src --port 3000
```

The runtime reads `myapp.js`, wraps it in the bootstrap, loads it into V8, and serves HTTP on the port. Hit `http://localhost:3000`.

For Vite-driven dev (HMR, TypeScript, JSX): use the SDK plugin.

```bash
# In your app's directory
npm install -D @zeroship/vite-plugin
npm run dev
# Vite spawns `zeroship serve` with the dev bootstrap; HMR is module-level.
```

See `docs/reference/vite-environment-api.md` for what the plugin actually does inside V8.

## Mode 2 — full platform (control + gateway + worker)

Three processes. Run each in its own terminal.

```bash
# Terminal 1 — Control plane (port 9090)
./target/release/zeroship-control \
  --port 9090 \
  --db postgres://localhost:5432/zeroship \
  --bundles ./bundles \
  --master-key dev-master \
  --control-key dev-control \
  --auth-secret dev-auth-secret \
  --insecure-dev

# Terminal 2 — Worker (port 8080, talks to control)
./target/release/zeroship-worker \
  --port 8080 \
  --workers 4 \
  --control http://localhost:9090 \
  --control-key dev-control

# Terminal 3 — Gateway (port 80 or 8000 if you don't want sudo)
./target/release/zeroship-gate \
  --port 8000 \
  --control http://localhost:9090 \
  --control-key dev-control \
  --workers http://localhost:8080 \
  --auth-secret dev-auth-secret
```

`--insecure-dev` on the control plane disables the `/internal/*` auth check. Never set in production.

## Deploy an app to the local platform

```bash
# Create the app (gives you a UUID + api_key)
curl -X POST http://localhost:9090/api/apps \
  -H "Authorization: Bearer dev-master" \
  -H "Content-Type: application/json" \
  -d '{"name":"hello","plan_id":"free"}'
# → {"id":"<uuid>", "api_key":"...", ...}

# Build (.zsapp via vite-plugin)
cd examples/hello && npx vite build
# Produces dist/app.zsapp (tar.zst archive: manifest.json + blobs/<hash>)

# Deploy
./target/release/zeroship deploy ./examples/hello/dist/app.zsapp \
  --app=<uuid> \
  --control=http://localhost:9090 \
  --key=dev-master

# Hit it (path-based routing on the gateway)
curl http://localhost:8000/apps/hello/
# → "Hello from zeroship!"
```

Subdomain routing requires DNS pointing `*.zeroship.local` at the gateway and `/etc/hosts` entries. Path-based routing works out of the box.

## Running tests

```bash
# Per crate
cargo test -p zeroship-core
cargo test -p zeroship-gateway
cargo test -p zeroship-control
cargo test -p zeroship-runtime --lib

# Postgres-backed (needs a live DB at $DATABASE_URL)
cargo test -p compio-postgres -- --test-threads=1

# E2E platform tests (needs all 3 components running)
./tests/e2e_platform.sh
```

## Benchmarks

```bash
# Build the bench binaries
cargo build --release --bin zeroship-bench-server --bin zerobench

# Run the standard suite (uses NUMA pinning if available)
cd crates/runtime/benches
DURATION=10s CONNS=300 WORKERS=16 ./run_zerobench.sh
# Results in results-<date>.txt
```

`docs/reference/zerobench.md` documents the tool. `docs/benchmarks/` keeps long-running cross-runtime comparisons.

## Common gotchas

- **`Manifest::passthrough()` is synthesized when `manifest_json` is NULL.** Apps without their own manifest get the default routing: POST `/_rpc/*` goes to RPC, everything else to SSR.
- **Worker holds isolates per-thread.** If you change `--workers`, restart the whole binary; the LRU cache doesn't carry across processes.
- **Gateway polls control every 5s.** New deploys aren't instant — wait one cycle, or restart the gateway.
- **Deploy hash mismatch is loud.** The control plane verifies `sha256(blob bytes) == filename hash` for every blob in the `.zsapp`. Any mismatch returns 400 with the offending hash; nothing is partially written.
- **`init_error` shows up as 500.** A syntax error in your app's JS returns 500 with the message instead of a misleading 404 "no default.fetch handler exported."
