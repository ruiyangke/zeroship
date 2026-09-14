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
./target/release/zeroship dev init
```

`zeroship dev init` creates the seven file-backed platform secrets in the
gitignored `deploy/compose/secrets` directory and the nine generated scalar
values in `deploy/compose/.env`. Rerunning validates and keeps existing values;
it never rotates them implicitly. The commands below use those default paths;
for another layout, source its env overlay and replace every file path below.

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

Run these from the repo root in four terminals. In each terminal, export the
generated overlay first. The generated values contain no shell metacharacters:

```bash
set -a
. deploy/compose/.env
set +a
```

The `set -a` step above already exported every generated overlay value under
its canonical `ZEROSHIP_*` name, so each binary picks its secrets straight out
of the process environment. Each terminal below only adds the settings that
are NOT in the generated overlay (database DSNs, key-material file paths, and
other operational flags).

Terminal 1:

```bash
ZEROSHIP_CONTROL_DATABASE_URL=postgres://localhost:5432/zeroship \
ZEROSHIP_AUTH_PLATFORM_ISSUER="http://localhost:9092/oauth2" \
./target/release/zeroship-control \
  --port 9090 \
  --blob-store ./bundles
```


`ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET` is not in the generated overlay and
`dev init` does not generate it: only Stripe issues a value that verifies.
Control starts without it, warns, and rejects every
`/internal/webhooks/stripe` delivery with 500. To work on webhooks locally,
set it to the secret `stripe listen --print-secret` prints, and point the
listener at this control instance.

Terminal 2:

```bash
ZEROSHIP_WORKER_DATABASE_URL=postgres://localhost:5432/zeroship \
./target/release/zeroship-worker \
  --port 8080 \
  --threads 4 \
  --control-url http://localhost:9090 \
  --blob-store ./bundles
```

Terminal 3:

```bash
ZEROSHIP_GATEWAY_DATABASE_URL=postgres://localhost:5432/zeroship \
./target/release/zeroship-gate \
  --port 8000 \
  --control-url http://localhost:9090 \
  --worker-urls http://localhost:8080 \
  --blob-store ./bundles \
  --auth-ui-url http://localhost:9092 \
  --public-url http://localhost:8000 \
  --signing-key-file deploy/compose/secrets/gateway-signing.pem \
  --broker-secret-file deploy/compose/secrets/broker-secret
```

Terminal 4:

```bash
ZEROSHIP_AUTH_DATABASE_URL=postgres://localhost:5432/zeroship \
./target/release/zeroship-auth \
  --addr 127.0.0.1:9092 \
  --public-url http://localhost:9092 \
  --signing-key-file deploy/compose/secrets/auth-signing.pem \
  --pairwise-salt-file deploy/compose/secrets/pairwise-salt \
  --broker-secret-file deploy/compose/secrets/broker-secret \
  --refresh-hash-key-file deploy/compose/secrets/refresh-hash-key \
  --refresh-idem-key-file deploy/compose/secrets/refresh-idem-key \
  --mailer stdout \
  --relay-forward-mailer stdout
```

Notes:

- The generated inputs are strong and stable across restarts. Local services
  execute the same authentication, signature, cookie, and secret-strength
  checks as every other deployment. There is no `--auth-secret` flag or local
  security-relaxation switch.
- Gateway and auth intentionally read the same physical `broker-secret` file.
  Auth reads `pairwise-salt` from a file while control and gateway read the
  byte-identical `ZEROSHIP_PAIRWISE_SALT` value from the overlay. The file has
  no trailing newline; changing either value would change every derived
  per-app `pws_`.
- `zeroship-worker` binds `127.0.0.1` by default (loopback, as above). Only add
  `--bind 0.0.0.0` together with `ZEROSHIP_WORKER_SERVICE_KEY_FILE` and
  `ZEROSHIP_WORKER_SERVICE_PEERS_FILE` if another host must
  reach it.
- Except for the worker, `--config <path>` or `ZEROSHIP_CONFIG=<path>` loads the optional TOML
  overlay; add `--check-config` to the normal command for a read-only dry run
  that validates CLI/env/file config and the startup guards, then exits before
  binding a port. `--check-config --check-config-format json` emits the resolved
  non-secret config as JSON instead of text.
- Absent an explicit `--config`/`ZEROSHIP_CONFIG`, those binaries auto-discover the
  fixed well-known path `/etc/zeroship/zeroship.toml` (the only auto-discovered
  location — no CWD/env redirect). Pass `--no-config` to disable discovery and
  use compiled defaults even if that file exists (`ZEROSHIP_NO_CONFIG=1` does the
  same). The worker exposes neither selector and never discovers an overlay;
  this prevents it from reading operator credentials. All five server binaries
  answer `--check-config`, including `zeroship-migrate-server`. A missing well-known file is
  fine (defaults apply); a present-but-broken one is a hard startup error. Dev
  usually just passes `--config deploy/ops/zeroship.toml` or sets `ZEROSHIP_CONFIG`
  rather than installing into `/etc`.
- To seed the console (the AI app-builder, now a gateway-fronted zeroship app)
  in local dev, start control with `--bootstrap-console` (env
  `BOOTSTRAP_CONSOLE=1`) after the native auth service is reachable. Control ingests the
  prebuilt console `.zship` and registers it as a public-PKCE gateway-fronted
  app. Override the served host with `--console-host` and the artifact path with
  `--console-zship`. (The retired `builder` Vite service and its confidential
  `--bootstrap-builder-client` / `BUILDER_REDIRECT_URI` knobs are gone.)

## Deploy an example app

Create an app:

```bash
curl -X POST http://localhost:9090/api/apps \
  -H "Authorization: Bearer $ZEROSHIP_CONTROL_MASTER_KEY" \
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
  --token=<PAT>
```

The CLI resolves the bearer token from `--token=<PAT>`, the `ZEROSHIP_TOKEN`
env var, or credentials saved by `zeroship login`, in that order. (The
`curl` example above uses the generated `ZEROSHIP_CONTROL_MASTER_KEY` because
control's master key is itself a bearer principal; the deploy CLI does not
read it.)

Then open `http://localhost:8000/apps/db-todos/`.

## Tests

The remaining shared PostgreSQL suites use the provisioner:

```bash
tests/provision_test_backends.sh
```

Database verification is part of ordinary `cargo test`; it has no opt-in
feature. Libtest runs serially by default because platform fixtures share
fleet-wide state. The suite runners below prepare the required migrations.

The provisioner writes the PostgreSQL test overlay. `PG_TEST_URL` redirects
suites that still use this shared database.
Auth, authn and mailer tests own their migrated PostgreSQL containers. Mailer
also owns its Mailpit SMTP sink and inspects captured messages through its API;
these tests require Docker and accept no external database or SMTP address.
Run `cargo xtask test migrations` to build their migration host, then
`cargo test -p zeroship-mailer` to verify delivery and suppression.
KV and Redis driver tests provision their required servers with Testcontainers;
they need Docker and do not read shared Redis URLs. See the
[KV test commands](../../crates/zeroship-kv/README.md).

```bash
cargo test -p zeroship-core
cargo test -p zeroship-gateway
cargo xtask test billing
cargo xtask test worker
cargo test -p zeroship-runtime --lib
cargo test -p compio-postgres -- --test-threads=1
./tests/e2e_platform.sh
```

### Test database ownership

`cargo xtask test auth` builds the platform migration host and runs the auth,
authn, authz, mailer and gateway packages. Their Rust fixtures own PostgreSQL
containers, SMTP listeners, HTTP servers and temporary files. Docker must be
available; no `PG_TEST_URL` or generated test overlay selects their databases.
Ordinary `cargo test -p <package>` runs the same required cases once the
migration host has been built.

`cargo xtask test worker` builds the same migration host and runs the worker
package. Its posture and workflow cases each own a PostgreSQL server. Workflow
requests connect as the migrated worker role; fixture setup and observations
use a separate administrator connection. The fixtures release connections,
isolates and temporary storage when the case ends, including on failure.

```bash
cargo xtask test auth
cargo xtask test billing
cargo xtask test worker
tests/sweep_test_databases.sh          # inspect reclaimable shared databases
tests/sweep_test_databases.sh --apply  # reclaim them
```

`cargo xtask test billing` builds the migration host and runs control,
migration-service, metering, and stream tests. Their Rust fixtures own
PostgreSQL and Redpanda; neither an external test URL nor a generated overlay
selects those services.
`cargo xtask test workflow` owns PostgreSQL through Testcontainers. Its control
plane tests clone private databases from a migrated, quiescent template. The
fixture helper removes the cached server when the test process exits.

## Benchmarks

Build the fixture binary with:

```bash
cargo build --release -p zeroship-runtime --bin zeroship-bench-server
```

The runner script is `./crates/zeroship-runtime/benches/run_zerobench.sh`. Read that script before using it; it shells out to an external `zerobench` binary and `nix`.

## Related docs

- [Multi-node / Docker Compose](../runbooks/docker-compose.md) - the same stack via `docker compose` instead of four terminals.
- [Architecture overview](../architecture/overview.md) — what each binary (`control`/`worker`/`gate`) does.
- [Distributed architecture](../architecture/distributed.md) — how control, gateway, and worker talk over the `/internal/*` feeds you wired above.
- [`.zship` artifact format](../reference/zship.md) — the deploy archive `pnpm build` emits and `zeroship deploy` uploads.
- [zeroship deploy contract](../reference/zeroship-standard.md) — what `default = { fetch?, rpc? }` an example app must export.
