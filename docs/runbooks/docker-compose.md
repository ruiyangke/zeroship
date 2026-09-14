# Docker Compose runbook

## Platform stack

`deploy/compose/docker-compose.yml` is the day-to-day local platform stack - the
whole zeroship stack, fronted by a Caddy reverse proxy on the
`*.zeroship.localhost` dev domain. Run from the repo root and point compose at
the file (or `export COMPOSE_FILE=deploy/compose/docker-compose.yml` once to drop
the `-f` from every call):

```bash
# Provision strong, stable local secrets once. Reruns keep existing values.
zeroship dev init
deploy/ops/init-cdc-tls.sh

# Build everything ahead (so `up` never builds): the single shared image
# (control/gateway/worker/auth/migrate-server/CDC relay + the CLI) plus the external
# images (postgres, caddy, verdaccio, redpanda).
docker compose -f deploy/compose/docker-compose.yml build   # all Dockerfile-based services
docker compose -f deploy/compose/docker-compose.yml pull     # external images

docker compose -f deploy/compose/docker-compose.yml up -d    # boot the whole stack (build-free)
docker compose -f deploy/compose/docker-compose.yml up -d --scale worker=10
docker compose -f deploy/compose/docker-compose.yml logs -f
docker compose -f deploy/compose/docker-compose.yml down -v
```

`docker compose -f deploy/compose/docker-compose.yml up --build` also works (builds on the fly the first time). The
single image compiles the SDKs (needed by the runtime crate) and all the native
binaries incl. `zeroship-auth` - see [Image build](#image-build). There is no
separate frontend image: the AI builder is now the **console**, a regular
gateway-fronted zeroship app seeded by control (see [Console / AI builder](#console--ai-builder)).

> First boot is heavy: the image does a full `pnpm build` + release `cargo build`
> of the V8 runtime. Pre-building with `docker compose build` keeps later `up`s instant.

### Local secret provisioning

Run `zeroship dev init` from the repository root before the first compose
command. It defaults to the gitignored `deploy/compose/secrets` directory and
the sibling `deploy/compose/.env` file. The deployment runbook covers custom
paths because Compose must receive both the custom env-file and mount path.

The secret directory contains exactly seven files:

`migrate-dsn` `gateway-signing.pem` `auth-signing.pem` `broker-secret`
`pairwise-salt` `refresh-hash-key` `refresh-idem-key`

That is `secret_specs()` in `crates/zeroship-cli/src/dev.rs` (six) plus `pairwise-salt`,
which is written separately because its bytes must equal the `.env` scalar
below. This list said "eight" and named six until 2026-08-21; the one it left
out was `migrate-dsn`, the privileged DSN, which is also the file
`tests/config_name_alignment_gate.sh` cited `dev.rs` as proof did not exist.

The env overlay contains eight generated scalar values:

`ZEROSHIP_CONTROL_KEY` `ZEROSHIP_CONTROL_MASTER_KEY`
`ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY` `ZEROSHIP_GATEWAY_STASH_SIGNING_KEY`
`ZEROSHIP_PAIRWISE_SALT` `ZEROSHIP_AUTH_STASH_SIGNING_KEY`
`ZEROSHIP_AUTH_TOTP_ENC_KEY`

The command is idempotent: it validates and retains existing material, creates
only missing entries, and refuses invalid or conflicting values instead of
rotating them. On Unix it applies 0700 to the secret directory and 0600 to the
secret files and `.env`.

Two shared-value rules are load-bearing. Gateway and auth read the same physical
`broker-secret` file. The raw bytes of `pairwise-salt` equal `ZEROSHIP_PAIRWISE_SALT`
in `.env` with no trailing line ending; auth, control, and gateway must derive the
same per-app `pws_`. The generator enforces both shapes. Do not export a
different `ZEROSHIP_PAIRWISE_SALT` in the shell that launches Compose; shell
values take precedence over `.env` interpolation.

### Dev domain (via Caddy)

A `caddy` service listens on host `:80` and reverse-proxies the
`*.zeroship.localhost` domain. Browsers resolve any `*.localhost` hostname to
`127.0.0.1` automatically, so **no `/etc/hosts` edits are needed**. Once the
stack is up, open:

- **`http://console.zeroship.localhost`** - the creator console / AI app-builder:
  describe an app and it builds + deploys it (requires `OPENAI_API_KEY`, see
  below). The console is a gateway-fronted zeroship app, so Caddy proxies this
  host straight to the gateway.
- **`http://auth.zeroship.localhost`** - login UI and native OIDC OP
  endpoints, proxied to `zeroship-auth` on `:9092`
- **`http://api.zeroship.localhost`** - the gateway (explicit API host)
- **`http://<app>.zeroship.localhost`** - any deployed creator app; the
  `*.zeroship.localhost` catch-all routes app subdomains to the gateway

Caddy chooses the most-specific matching site, so the explicit hosts above
always win over the `*.zeroship.localhost` catch-all. Site addresses use the
`http://` scheme so Caddy serves plain HTTP and never tries to provision TLS
for `.localhost`. Config: `deploy/ops/Caddyfile`.

The Caddyfile exposes `control.<domain>` through Caddy. Control still requires
the generated control-key bearer on every protected route; it has no host port,
and Caddy reaches it over the private compose network.

#### Caddy network-alias trick (container-side OIDC)

Server-side OIDC steps - control/gateway exchanging codes, fetching JWKS, and
verifying tokens against the issuer `http://auth.zeroship.localhost` - run
*inside* the compose network, where that hostname would not otherwise resolve.
The `caddy` service therefore carries **network aliases** for
`auth.zeroship.localhost`, `console.zeroship.localhost`, and
`api.zeroship.localhost` on the default network, so containers resolve those
names to Caddy too. The net effect: the issuer URL the browser sees and the one
the servers verify against are identical, which OIDC requires.

Services and host ports from the live file (the proxy is the primary entry
point; non-edge publications are loopback-only debugging paths):

- `caddy` -> `localhost:80` (the dev domain front door)
- `postgres` -> `localhost:5440`
- `gateway` (`zeroship-gate`) -> `localhost:8000`
- `auth` (`zeroship-auth`) -> `localhost:9092`
- `redpanda` (Kafka-wire billing stream) -> `127.0.0.1:19092`. The producers
  take it as `--metering-brokers` / `ZEROSHIP_METERING_BROKERS`;
  `REDPANDA_BROKERS` is now only the name the host-side `zeroship-stream` and
  `zeroship-control` integration tests gate themselves on.
- `redis` (`env.kv` store) has no host port
- `worker` (`zeroship-worker`) has no host port; scale it with `--scale worker=N`

The one-shot `migrate` service runs to completion and exits; `verdaccio`
publishes loopback-only on `localhost:4873`.

Control has no direct host port in the base file. Browser and CLI traffic can
reach it through `http://control.zeroship.localhost`; a local tool that needs a
raw loopback port can add this explicit override:

```yaml
services:
  control:
    ports:
      - "127.0.0.1:9090:9090"
```

There is no `sandbox` service here: the sandbox/preview backend lives in the
standalone `zeroship-sandbox` project and is run from there. The `control`
service reaches it over HTTP via `SANDBOX_URL` / `SANDBOX_TOKEN`.

### Image build

The single `Dockerfile` builds the platform services and the Node migration
command in separate image stages. The `sdks` stage runs the root JavaScript
build, including `sdks/db/dist/internal.js`, before Rust compilation because
`zeroship-data-v8` embeds that DB facade. The Rust builder then compiles the
native services with the SDK output and authorization policies. Runtime images
carry native binaries; the Node migration command has its own image stage.

Because SDK dist files are generated, the image build creates them with:

```sh
docker compose -f deploy/compose/docker-compose.yml build
```

### OpenAI key (console)

The console's AI codegen calls OpenAI to generate apps. Export `OPENAI_API_KEY`
before `docker compose -f deploy/compose/docker-compose.yml up`; the `control` service reads it from its own process
env and `--bootstrap-console` writes it onto the seeded console app's server-side
env store (never the browser). Absent, the stack still boots - the console's AI
features degrade.

```bash
export OPENAI_API_KEY=sk-...
docker compose -f deploy/compose/docker-compose.yml up --build
```

### Bind addresses

All four web binaries default to a **loopback** bind for safety, so each compose
command explicitly opts into a non-loopback address to be reachable across the
container network: `control` and `gateway` pass `--bind 0.0.0.0`, `zeroship-worker`
passes `--bind 0.0.0.0` (paired with its service key and peer document), and `zeroship-auth`
passes `--addr 0.0.0.0:9092`. Outside compose (single-host dev), the loopback defaults
need no override. Authentication and secret checks remain active on those
non-loopback container binds.

### Auth service

The `auth` service runs `zeroship-auth`, the native OIDC OP and login UI. The
gateway redirects unauthenticated end users to the Caddy-fronted host
(`--auth-ui-url http://auth.zeroship.localhost`). Auth loads strong generated
secrets and applies its normal cookie, CSRF, signature, and secret-strength
checks in the local topology.

`ZEROSHIP_AUTH_PUBLIC_URL` is built from `ZEROSHIP_ORIGIN_SCHEME`, so its local default
is `http://auth.zeroship.localhost` and a TLS-terminating deployment can set the
public scheme to `https`. Discovery and token `iss` use that URL plus `/oauth2`.
The DB DSN comes from auth's canonical secret name
`ZEROSHIP_AUTH_DATABASE_URL`; there is no `--db-url` value flag, because a secret
gets a `--database-url-file PATH` flag and nothing else. The shared
`deploy/ops/zeroship.toml` overlay supplies `trusted_oauth_clients` and
`frame_ancestor_origins`.

### Blob store

`control`, `gateway`, and `worker` all mount the `bundles` volume at
`/data/bundles` and pass `--blob-store /data/bundles`, so every service reads
and writes the same content-addressed deploy blobs. The worker also passes
`--storage-url /data/app-storage` (the `app-storage` volume) for the
creator-facing `env.storage` namespace.

The compose file already sets the current service names, keys, and sandbox env vars. Use it as the source of truth before copying flags into ad-hoc commands.

### Production object storage (S3 / R2 / MinIO)

The local-volume defaults above are the dev path. For a production-like
multi-node run - or to exercise the real S3 code path locally - point both the
deploy blob store and `env.storage` at an S3-compatible provider. Both use the
**same URL grammar** and the **same** AWS credentials (one S3 identity per
process), differing only by prefix:

```yaml
# control / gateway / worker - deploy blobs:
- --blob-store
- s3://my-bucket/deploy?region=us-east-1

# worker only - env.storage objects:
- --storage-url
- s3://my-bucket/storage?region=us-east-1
```

Credentials come from the standard AWS environment variables on each service:

```yaml
environment:
  AWS_ACCESS_KEY_ID: "<key>"
  AWS_SECRET_ACCESS_KEY: "<secret>"
  # AWS_SESSION_TOKEN: "<token>"   # only for temporary/STS credentials
```

Provider-specific URL parameters:

| Provider | Example URL suffix |
| --- | --- |
| **AWS S3** | `?region=us-east-1` (endpoint inferred; virtual-host style) |
| **Cloudflare R2** | `?provider=r2&endpoint=https://<acct>.r2.cloudflarestorage.com&region=auto&style=path` |
| **MinIO** (local) | `?provider=minio&endpoint=http://minio:9000&region=us-east-1&style=path&dev_http=true` |

`dev_http=true` is loopback/localhost-only (plain HTTP is rejected for any
non-loopback host). R2 must use `region=auto` and `checksum=none` (it rejects
the AWS checksum headers); the parser enforces these.

The example-owned suites in `examples/storage-gallery/tests/` and
`examples/storage-probe/tests/` provision MinIO, Postgres and their test issuer
through Testcontainers. They boot control, gateway and worker with S3-backed
deploy blobs and object storage, deploy the built examples, and check browser
loading, RPC operations, multipart transfers and LocalFs/S3 parity. Docker is
required; unavailable dependencies fail setup. Run `cargo xtask test storage`
for the Rust suites and the examples' Vitest/Playwright tests.

### Console / AI builder

The AI builder is the **console** - a regular zeroship app, not a separate
service. Control starts with `--bootstrap-console --console-host
console.zeroship.localhost --console-zship /opt/zeroship/console/app.zship` in
this stack. On first boot it ingests that prebuilt `.zship` (emitted by the
image's `sdks` stage) and registers it as a public-PKCE gateway-fronted app, so
Caddy proxies `console.zeroship.localhost` to the gateway like any creator app.
Its runtime config - `OPENAI_API_KEY`, `SANDBOX_URL`/`SANDBOX_TOKEN`,
`ZEROSHIP_CONTROL_URL` - is forwarded from control's process env onto the seeded
console app's server-side env store at install time (secrets encrypted, plain
URLs as vars), replacing what the retired `builder` Vite service used to inject.
The standalone Vite container and its confidential OIDC client
(`--bootstrap-builder-client`, `BUILDER_REDIRECT_URI`, `BUILDER_CLIENT_SECRET`)
were removed in the R5 cutover.

### Configuration overlay

`deploy/compose/docker-compose.yml` mounts `../ops/zeroship.toml` into `control`, `gateway`,
`worker`, and `auth` at the well-known path `/etc/zeroship/zeroship.toml`. The
compose stack relies on auto-discovery: because the file lives at the system
well-known path, no service passes `--config` - each binary's config resolver
finds it automatically. (Any service that does not mount the file simply falls
back to compiled defaults - discovery only fires when the file is present at the
well-known path.)

The overlay is the source of truth for the config-covered values, so they are
defined ONCE instead of being repeated as per-service flags:

- Top level - optional `origin_scheme` and `trusted_origins` topology settings.
  Compose supplies `ZEROSHIP_ORIGIN_SCHEME` to control and gateway, so its env
  tier wins over a file value. It does not inject an empty trusted-origin env;
  add exact origins to the TOML only when the deployment needs them.
- `[auth]` - native OP trust settings: `trusted_oauth_clients` and
  `frame_ancestor_origins`. `ZEROSHIP_AUTH_PUBLIC_URL` stays on the `auth` service env
  because it is the auth-service issuer setting for this deployment.
- `[observability]` - shared `log_filter` / `log_format` (every service, worker
  included).
- There is NO `[secrets]` table. A secret sits at its canonical path beside its
  operational siblings, so location encodes sharing: `control_key`
  and `pairwise_salt` are root keys because several binaries read the one value,
  while `[control] master_key` and each service's `database_url` belong to one
  binary. A value is either the secret itself or a `urn:zeroship:file:<path>`
  reference; the env-to-env form that made the old table an alias hop is gone,
  and so are the Vault and AWS Secrets Manager forms, which always failed at
  boot. A literal is permitted, but never in a TRACKED file.
- Each service now supplies its secrets under its OWN canonical environment
  names - `ZEROSHIP_CONTROL_KEY`, `ZEROSHIP_CONTROL_DATABASE_URL`,
  `ZEROSHIP_GATEWAY_DATABASE_URL`, `ZEROSHIP_WORKER_DATABASE_URL`,
  `ZEROSHIP_AUTH_DATABASE_URL` - so one shared spelling no longer stands for four
  role-specific DSNs. The command lines carry no secret value flags at all,
  because none are generated.

For local compose, the referenced scalar secret values come from the gitignored
`.env` written by `zeroship dev init`. In particular, one generated
`ZEROSHIP_CONTROL_KEY` is interpolated into control, gateway, worker, and
migrate-server.

Precedence is CLI/env-flag > `[secrets]`/`[auth]` file reference > default, so a
leftover literal flag would silently WIN and defeat the file - keep config-covered
values OFF the command lines. Copy `deploy/ops/zeroship.example.toml` to
`deploy/ops/zeroship.toml` when customizing an environment. The file itself stays
secret-free: it carries only `urn:`/`arn:` references, never a plaintext secret
(a literal in `[secrets]` is rejected at resolve). The actual secret values live
in the generated compose `.env` and narrowly mounted files (local dev), or a
real secret store (prod).

Validate a web binary's resolved config by adding `--check-config` to the
normal command. It runs the same startup guards, so include the same required
secret or dev-mode flags you would use for startup:

```bash
zeroship-<bin> --check-config --config <file> <normal required flags>
```

## Redis cluster test stack

Redis driver integration tests start their own Redis and Dragonfly containers
through Testcontainers. Docker must be running; Compose provisioning and backend
environment variables are unnecessary:

```bash
cargo test -p compio-redis --tests
```

The fixtures allocate mapped ports, configure cluster slots, and remove their
containers when tests finish. The same fixtures support the KV and V8 binding
suites; see [KV test commands](../../crates/zeroship-kv/README.md).

## OpenMeter metering-export test stack

`deploy/compose/openmeter.yml` is a **separate, opt-in** stack used only by the
faithful OpenMeter metering-export e2e (`tests/e2e_openmeter_export.sh`). It does
**not** boot the platform stack and shares **nothing** with `deploy/compose/docker-compose.yml`:
it is a distinct compose project (`name: zeroship-openmeter`) with its own
network, volumes, and a private `127.0.0.1`-only port band. In particular its
internal Postgres is OpenMeter metadata only and is **not** published on `:5440`
(the zeroship billing PG the cargo integration tests use), so bringing it up or
tearing it down never touches the main stack or the billing tests.

It stands up the minimal real OpenMeter pipeline - Kafka + ClickHouse + Redis +
Postgres + the OpenMeter API + a sink-worker - with a single `compute_units`
meter pre-provisioned in `deploy/ops/openmeter-config.yaml` to match exactly what
`crates/zeroship-control/src/metering/provider/adapters/openmeter.rs` emits (`eventType` /
`slug` = `compute_units`, `aggregation: SUM` over `$.value`).

```bash
# Bring it up (pulls ~1 GB of images on first run); the API lands on :48888.
docker compose -f deploy/compose/openmeter.yml up -d
curl -s http://127.0.0.1:48888/api/v1/meters | grep compute_units   # meter live?

# Run the faithful e2e (owns its OWN ephemeral zeroship PG on :5481, NOT :5440):
./tests/e2e_openmeter_export.sh

# Tear it down (volumes too):
docker compose -f deploy/compose/openmeter.yml down -v
```

The e2e script brings the stack up/down for you; run the raw compose only when
iterating manually. `KEEP_OPENMETER=1 ./tests/e2e_openmeter_export.sh` leaves the
OpenMeter stack running between iterations. See
[Billing & metering](../reference/billing-metering.md) (OpenMeter section "Real-API
divergences from the mock") for what this e2e catches that the in-test mock cannot
(eventual-consistency lag + the query-window/`time` interaction).

## Postgres connections a worker holds

Worker data pools are thread-local and open lazily when a thread first uses
`env.db`. Their `PoolConfig` bounds connection acquisition and idle retention;
scaling worker processes or threads multiplies those pools.

CDC adds no worker database pool. Workers connect to the relay over TLS, and the
relay shares capture for each app across workers. Its database pool and logical
replication sessions have separate capacity settings. See
`docs/runbooks/cdc-relay.md` for sizing and deployment.

## Service health

The HTTP services (`control`, `gateway`, `worker`, `migrate-server`, `auth`)
expose the same pair, and Compose gives each a `healthcheck` pointed at
`/readyz`:

| Endpoint | Meaning | Checks |
| --- | --- | --- |
| `GET /healthz` | Liveness. Constant 200. | Nothing. It must never fail because a dependency did, or an outage in Postgres would get every container killed on top of it. |
| `GET /readyz` | Readiness. 200 or 503. | control / migrate-server / auth: Postgres. worker: control poll current AND blob store reachable. gateway: control route pull current. |

```bash
docker compose -f deploy/compose/docker-compose.yml ps   # STATUS shows (healthy)
curl -sf http://127.0.0.1:9090/readyz    # control, 200 = can serve
curl -sf http://127.0.0.1:8000/readyz    # gateway, 503 until the first route pull
```

Both are unauthenticated, so `/readyz` is built not to be a lever: each
dependency probe has a short explicit timeout, the outcome is cached for a
couple of seconds and concurrent probes collapse into one, and the response
body is `{"ready":true|false}` with no DSN, host, driver text or version in
it. The gateway and worker read a stamp written by the background poll they
already run, so their probes issue no upstream request at all.

There is no `/health`. It was deleted rather than aliased.

`tests/health_endpoints.sh` walks the whole contract against the real
binaries, including the arms where a dependency is taken away.

## Database migrations

The shared Postgres `zeroship` schema is managed by the `zero-migrate` CLI, a
Node program that ships in its own `migrate` image stage. After Postgres is
healthy and before control/auth start, the one-shot `migrate` service runs:

```bash
# deploy/ops/migrate-entrypoint.sh, the migrate image's entrypoint
--migrations-dir /app/db/migrations-ts \
--database-url-file /etc/zeroship/secrets/migrate-dsn
```

THIS SECTION DESCRIBED A DELETED BINARY UNTIL 2026-09-04. It said the runner was
`zeroship-platform-migrate`, "built from `zeroship-migrate-adapter` with its
`platform-cli` feature"; that crate and that binary were both deleted on
2026-08-28, and three details of the invocation went with them. `--project-schema`
and `--project-id` are gone - neither was a knob, since the corpus spells
`schema: "zeroship"` itself and the CLI derives its advisory-lock key from the
schema. And the corpus path is `/app/db/migrations-ts`, not `/db/migrations-ts`:
the `.ts` files resolve `import ... from "@zeroship/migrate"` by Node's upward
walk, so they must sit above the pnpm store at `/app/node_modules`.

The privileged DSN is mounted as a file, not passed as an argument, because a
container's argv is published by `docker inspect`, `docker ps --no-trunc` and
/proc/<pid>/cmdline. The CLI's own flag IS `--database-url <value>` and this path
deliberately does not use it: `deploy/ops/migrate-entrypoint.sh` takes the path
and builds the 0600 config the CLI reads. The file must be mode 0600.

`migrate-server` mounts the SAME file and reads it through
`ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL`, which defaults to
`urn:zeroship:file:/etc/zeroship/secrets/migrate-dsn`. Those are the only two
readers of a superuser credential in the deployment and they now take it from
one place: repointing `secrets/migrate-dsn` at a real database moves both. Until
2026-08-21 `migrate-server` carried its own inline superuser DSN default, so the same
repointing moved the one-shot and silently left `migrate-server` provisioning against
the in-compose Postgres.

The source of truth is the committed JS DSL corpus in `db/migrations-ts/`; the
runner records each `.ts` file to transient IR and applies that plan.
control/auth `depends_on` it with
`service_completed_successfully`, so they only ever boot against a fully-migrated
schema. See [Database migrations](db-migrations.md) for the layout,
`deploy/ops/db-migrate.sh`, and how to add a migration.

## Related docs

- [Database migrations](db-migrations.md) - platform JS DSL migrations, the `migrate` service, and `deploy/ops/db-migrate.sh`.
- [Local dev setup](../runbooks/local-dev.md) - the same platform stack run as four bare `cargo`-built binaries instead of containers.
- [Builder sandbox](../architecture/builder.md) - where the sandbox/preview backend lives now and how `control` reaches it.
- [Distributed architecture](../architecture/distributed.md) - what the `control`/`gateway`/`worker` services are and how they coordinate.
