# Local dev setup

Get a working zeroship stack on your machine. Build the JS SDKs first, then the Rust binaries.

## Prerequisites

- Rust toolchain
- Node.js 20+
- `pnpm` 9+
- PostgreSQL running locally if you run the control plane, full platform, or
  Postgres-backed tests
- Docker only if you want the compose stack

## First-time bootstrap

```bash
pnpm install
pnpm build
cargo build --release
mkdir -p bundles
```

For full-platform mode, create the control-plane database:

```bash
createdb zeroship
```

The control plane defaults to `postgres://localhost/zeroship`; `createdb
zeroship` matches that. Vite examples do not require this by default because
the dev runtime uses project-local SQLite at `.zeroship/dev.sqlite`.

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
pnpm dev
```

By default the Vite plugin spawns the zeroship dev runtime with
`DATABASE_URL=sqlite:.zeroship/dev.sqlite`. Override `DATABASE_URL` only when
you intentionally want a different backend.

## Mode 2: full local platform

Run these from the repo root in three terminals.

Terminal 1:

```bash
./target/release/zeroship-control \
  --port 9090 \
  --db postgres://localhost:5432/zeroship \
  --blob-store ./bundles \
  --control-key dev-control \
  --master-key dev-master
```

Terminal 2:

```bash
./target/release/zeroship-worker \
  --port 8080 \
  --worker-threads 4 \
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

- `zeroship-gate` uses `--auth-secret` / `AUTH_SECRET` for gateway auth paths.
- `zeroship-worker` binds `127.0.0.1` by default. Only add `--bind 0.0.0.0` together with `--worker-key`.
- `--config <path>` or `ZEROSHIP_CONFIG=<path>` loads the optional TOML
  overlay; add `--check-config` to the normal command to validate CLI/env/file
  config and exit before binding a port.
- Absent an explicit `--config`/`ZEROSHIP_CONFIG`, the binaries auto-discover the
  fixed well-known path `/etc/zeroship/zeroship.toml` (the only auto-discovered
  location — no CWD/env redirect). A missing well-known file is fine (defaults
  apply); a present-but-broken one is a hard startup error. Dev usually just
  passes `--config ops/zeroship.toml` or sets `ZEROSHIP_CONFIG` rather than
  installing into `/etc`.
- To seed the Builder OAuth client in local dev, start control with
  `BOOTSTRAP_BUILDER_OAUTH_CLIENT=1` or `--bootstrap-builder-client` while
  Hydra admin is reachable. Control registers `zeroship-builder` in Hydra and
  mirrors it in `control.oauth_clients`; the generated client secret is stored
  at `data/builder-client-secret` by default. Override the callback with
  `BUILDER_REDIRECT_URI` and the secret path with `BUILDER_CLIENT_SECRET_FILE`
  if your Builder dev server is not on `http://localhost:3001/auth/callback`.

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

## Related docs

- [Multi-node / Docker Compose](../runbooks/docker-compose.md) — the same stack via `docker compose` instead of three terminals.
- [Architecture overview](../architecture/overview.md) — what each binary (`control`/`worker`/`gate`) does.
- [Distributed architecture](../architecture/distributed.md) — how control, gateway, and worker talk over the `/internal/*` feeds you wired above.
- [`.zship` artifact format](../reference/zship.md) — the deploy archive `pnpm build` emits and `zeroship deploy` uploads.
- [ZS deploy contract](../reference/zs-standard.md) — what `default = { schema?, fetch?, rpc? }` an example app must export.
