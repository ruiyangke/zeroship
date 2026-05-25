# Local dev setup

Get a working zeroship stack on your machine. Build the JS SDKs first, then the Rust binaries.

## Prerequisites

- Rust toolchain
- Node.js 20+
- `pnpm` 9+
- PostgreSQL running locally
- Docker only if you want the compose stack

## First-time bootstrap

```bash
pnpm install
pnpm build
cargo build --release
createdb zeroship
mkdir -p bundles
```

The control plane defaults to `postgres://localhost/zeroship`; `createdb zeroship` matches that.

## Mode 1: single-file runtime

`zeroship serve` now accepts a JavaScript file, not a project directory:

```bash
DATABASE_URL=postgres://localhost:5432/zeroship \
  ./target/release/zeroship serve examples/http-handler.js --port=3000
```

Open `http://localhost:3000`.

For Vite-based apps, run the app's dev server instead of pointing `zeroship serve` at a directory. Example:

```bash
cd examples/db-todos
pnpm install
DATABASE_URL=postgres://localhost:5432/zeroship pnpm dev
```

## Mode 2: full local platform

Run these from the repo root in three terminals.

Terminal 1:

```bash
./target/release/zeroship-control \
  --port 9090 \
  --db postgres://localhost:5432/zeroship \
  --bundles ./bundles \
  --control-key dev-control \
  --master-key dev-master \
  --jwt-secret dev-jwt-secret
```

Terminal 2:

```bash
./target/release/zeroship-worker \
  --port 8080 \
  --workers 4 \
  --control http://localhost:9090 \
  --control-key dev-control \
  --blob-store ./bundles
```

Terminal 3:

```bash
./target/release/zeroship-gate \
  --port 8000 \
  --control http://localhost:9090 \
  --control-key dev-control \
  --workers http://localhost:8080 \
  --auth-secret dev-jwt-secret \
  --blob-store ./bundles
```

Notes:

- `zeroship-control` uses `--jwt-secret`; `zeroship-gate` must use the same value via `--auth-secret`.
- `zeroship-worker` binds `127.0.0.1` by default. Only add `--bind 0.0.0.0` together with `--worker-key`.

## Deploy an example app

Create an app:

```bash
curl -X POST http://localhost:9090/api/apps \
  -H "Authorization: Bearer dev-master" \
  -H "Content-Type: application/json" \
  -d '{"name":"db-todos","plan_id":"free"}'
```

Build an example that actually emits `dist/app.zship`:

```bash
cd examples/db-todos
pnpm install
pnpm build
cd ../..
```

Deploy it:

```bash
./target/release/zeroship deploy ./examples/db-todos/dist/app.zship \
  --app=<uuid> \
  --control=http://localhost:9090 \
  --key=dev-master
```

Then open `http://localhost:8000/apps/db-todos/`.

## Tests

```bash
cargo test -p zeroship-core
cargo test -p zeroship-gateway
cargo test -p zeroship-control
cargo test -p zeroship-runtime --lib
cargo test -p compio-postgres -- --test-threads=1
./tests/e2e_platform.sh
```

## Benchmarks

Build the fixture binary with:

```bash
cargo build --release -p zeroship-runtime --bin zeroship-bench-server
```

The runner script is `./crates/runtime/benches/run_zerobench.sh`. Read that script before using it; it shells out to an external `zerobench` binary and `nix`.
