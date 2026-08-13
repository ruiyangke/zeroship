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

# Build everything ahead (so `up` never builds): the single shared image
# (control/gateway/worker/auth/platform-migrate + the `zeroship` CLI) plus the external
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

`control-signing.pem` `gateway-signing.pem` `auth-signing.pem` `broker-secret`
`pairwise-salt` `refresh-hash-key` `refresh-idem-key`

The env overlay contains eight generated scalar values:

`ZEROSHIP_CONTROL_KEY` `ZEROSHIP_MASTER_KEY` `ZEROSHIP_WORKER_KEY`
`GATEWAY_OIDC_SECRET` `STASH_SIGNING_KEY` `PAIRWISE_SALT`
`AUTH_STASH_SIGNING_KEY` `AUTH_TOTP_ENC_KEY`

The command is idempotent: it validates and retains existing material, creates
only missing entries, and refuses invalid or conflicting values instead of
rotating them. On Unix it applies 0700 to the secret directory and 0600 to the
secret files and `.env`.

Two shared-value rules are load-bearing. Gateway and auth read the same physical
`broker-secret` file. The raw bytes of `pairwise-salt` equal `PAIRWISE_SALT` in
`.env` with no trailing line ending; auth, control, and gateway must derive the
same per-app `pws_`. The generator enforces both shapes. Do not export a
different `PAIRWISE_SALT` in the shell that launches Compose; shell values take
precedence over `.env` interpolation.

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
- `redpanda` (Kafka-wire billing stream) -> `REDPANDA_BROKERS=127.0.0.1:19092`
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

The single `Dockerfile` builds all SIX binaries (`zeroship-control`,
`zeroship-gate`, `zeroship-worker`, `zeroship-auth`, the `zeroship` CLI, and
`zeroship-platform-migrate` - the platform DB migration one-shot the `migrate`
service runs) in three stages:

1. **`sdks` (node:22)** runs `pnpm install --frozen-lockfile && pnpm build` to
   emit `sdks/bootstrap/dist/{runtime-entry,dispatcher}.js`. The runtime crate
   `include_str!`s those files at compile time
   (`crates/runtime/src/core/init.rs`), so they must exist before cargo touches
   `zeroship-runtime`. This stage also builds the console app's `.zship`, which
   the runtime stage copies to `/opt/zeroship/console/app.zship` for control's
   `--bootstrap-console` seed.
2. **`builder` (rust)** copies `crates/`, the freshly-built `sdks/`, and the
   `deploy/policies/` tree (`crates/authz/build.rs` parses
   `../../deploy/policies/*.cedar` at build time) and compiles the six binaries.
3. The runtime stage (ubuntu:24.04) copies all six binaries plus the prebuilt
   console `.zship`.

Because the SDK dist files are gitignored and absent from a fresh checkout, the
image must be (re)built with `--build` the first time; `docker compose -f deploy/compose/docker-compose.yml build`
regenerates them inside the image.

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
passes `--bind 0.0.0.0` (paired with `--worker-key`), and `zeroship-auth` passes
`--addr 0.0.0.0:9092`. Outside compose (single-host dev), the loopback defaults
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
The DB DSN comes from
`[secrets].auth_db_url` (a `urn:zeroship:env:AUTH_DB_URL` reference resolved from
the service's `AUTH_DB_URL` env), not a literal `--db-url`. The shared
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

A self-contained MinIO smoke is available at `tests/e2e_s3_storage.sh`: it
brings up a MinIO container, creates a bucket, boots control/worker/gateway
with `--blob-store s3://...` and the worker with `--storage-url s3://...`, deploys
a real app whose blobs now live in S3, asserts gateway->worker dispatch reading
the bundle from S3, and byte-compares a large (> 8 MiB part size) multipart
`env.storage` streaming round-trip. It skips cleanly when Docker is
unavailable.

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
- `[secrets]` - REFERENCE-only (`urn:zeroship:env:<VAR>`, never a plaintext
  literal). `control_key`, `master_key`, `worker_key`, `database_url`, and
  `auth_db_url` are resolved per-binary from each service's `environment:` block,
  so the command lines carry no literal `--control-key` / `--master-key` /
  `--worker-key` / `--db` / `--db-url`. The single `database_url` reference points
  every binary at `ZEROSHIP_DATABASE_URL`; each service sets its OWN role-specific
  DSN under that name (control -> `zeroship_control`, gateway -> `zeroship_gateway`,
  worker -> the privileged `postgres` provisioning superuser), so one shared
  reference resolves to distinct per-role DSNs. `auth` uses its own
  `AUTH_DB_URL` (the `auth_db_url` slot, flag `--db-url`).

For local compose, the referenced scalar secret values come from the gitignored
`.env` written by `zeroship dev init`. In particular, one generated
`ZEROSHIP_CONTROL_KEY` is interpolated into all five consumers together; there
is no per-service fallback that can move auth alone.

Precedence is CLI/env-flag > `[secrets]`/`[auth]` file reference > default, so a
leftover literal flag would silently WIN and defeat the file - keep config-covered
values OFF the command lines. Copy `deploy/ops/zeroship.example.toml` to
`deploy/ops/zeroship.toml` when customizing an environment. The file itself stays
secret-free: it carries only `urn:`/`arn:` references, never a plaintext secret
(a literal in `[secrets]` is rejected at resolve). The actual secret VALUES live
in the generated compose `.env` (local dev) or a real secret store (prod).

Validate a web binary's resolved config by adding `--check-config` to the
normal command. It runs the same startup guards, so include the same required
secret or dev-mode flags you would use for startup:

```bash
zeroship-<bin> --check-config --config <file> <normal required flags>
```

## Redis cluster test stack

`deploy/compose/cluster.yml` is separate. It does **not** boot the platform stack; it only starts a 3-node Dragonfly cluster for `compio-redis` integration tests:

```bash
docker compose -f deploy/compose/cluster.yml up -d
./deploy/scripts/bootstrap-dragonfly-cluster.sh
DRAGONFLY_CLUSTER_SEEDS='redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002' \
  cargo test -p compio-redis --test cluster -- --nocapture
docker compose -f deploy/compose/cluster.yml down -v
```

The cluster file exposes `dragonfly-0`, `dragonfly-1`, and `dragonfly-2` on host ports `7000`, `7001`, and `7002`. These use `network_mode: host` (not a `ports:` mapping), so the ports can't be remapped and must be free on the host before you start the stack.

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
`crates/control/src/metering/provider/adapters/openmeter.rs` emits (`eventType` /
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

## Service health

Every platform service (`control`, `gateway`, `worker`, `migrated`, `auth`)
exposes the SAME pair, and compose gives each one a `healthcheck` pointed at
`/readyz`:

| Endpoint | Meaning | Checks |
| --- | --- | --- |
| `GET /healthz` | Liveness. Constant 200. | Nothing. It must never fail because a dependency did, or an outage in Postgres would get every container killed on top of it. |
| `GET /readyz` | Readiness. 200 or 503. | control / migrated / auth: Postgres. worker: control poll current AND blob store reachable. gateway: control route pull current. |

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

The shared Postgres `zeroship` schema is managed by the platform migration
runner, built from `zeroship-migrate-adapter` with its `platform-cli` feature.
After Postgres is healthy and before control/auth start, the one-shot `migrate`
service runs:

```bash
zeroship-platform-migrate \
  --database-url postgres://postgres:zeroship@postgres:5432/zeroship \
  --migrations-dir /db/migrations-ts \
  --project-schema zeroship \
  --project-id zeroship
```

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
