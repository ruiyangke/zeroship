# Docker Compose runbook

## Platform stack

`docker-compose.yml` is the day-to-day local platform stack — the whole
zeroship stack, fronted by a Caddy reverse proxy on the `*.zeroship.localhost`
dev domain. From the repo root:

```bash
# Build everything ahead (so `up` never builds): the single shared image
# (control/gateway/worker/auth/sandbox + the `zeroship` CLI) plus the external
# images (postgres, hydra, caddy, verdaccio).
docker compose build                   # all Dockerfile-based services
docker compose pull                    # external images

docker compose up -d                   # boot the whole stack (build-free)
docker compose up -d --scale worker=10
docker compose logs -f
docker compose down -v
```

`docker compose up --build` also works (builds on the fly the first time). The
single image compiles the SDKs (needed by the runtime crate) and all the native
binaries incl. `zeroship-auth` — see [Image build](#image-build). There is no
separate frontend image: the AI builder is now the **console**, a regular
gateway-fronted zeroship app seeded by control (see [Console / AI builder](#console--ai-builder)).

> First boot is heavy: the image does a full `pnpm build` + release `cargo build`
> of the V8 runtime. Pre-building with `docker compose build` keeps later `up`s instant.

### Dev domain (via Caddy)

A `caddy` service listens on host `:80` and reverse-proxies the
`*.zeroship.localhost` domain. Browsers resolve any `*.localhost` hostname to
`127.0.0.1` automatically, so **no `/etc/hosts` edits are needed**. Once the
stack is up, open:

- **`http://console.zeroship.localhost`** — the creator console / AI app-builder:
  describe an app and it builds + deploys it (requires `OPENAI_API_KEY`, see
  below). The console is a gateway-fronted zeroship app, so Caddy proxies this
  host straight to the gateway.
- **`http://auth.zeroship.localhost`** — login / OIDC (Caddy splits this host:
  `/oauth2/*` and `/.well-known/*` go to Hydra `:4444`, everything else to the
  `zeroship-auth` UI on `:9092`)
- **`http://api.zeroship.localhost`** — the gateway (explicit API host)
- **`http://<app>.zeroship.localhost`** — any deployed creator app; the
  `*.zeroship.localhost` catch-all routes app subdomains to the gateway

Caddy chooses the most-specific matching site, so the explicit hosts above
always win over the `*.zeroship.localhost` catch-all. Site addresses use the
`http://` scheme so Caddy serves plain HTTP and never tries to provision TLS
for `.localhost`. Config: `ops/Caddyfile`.

#### Caddy network-alias trick (container-side OIDC)

Server-side OIDC steps — control/gateway exchanging codes, fetching JWKS, and
verifying tokens against the issuer `http://auth.zeroship.localhost` — run
*inside* the compose network, where that hostname would not otherwise resolve.
The `caddy` service therefore carries **network aliases** for
`auth.zeroship.localhost`, `console.zeroship.localhost`, and
`api.zeroship.localhost` on the default network, so containers resolve those
names to Caddy too. The net effect: the issuer URL the browser sees and the one
the servers verify against are identical, which OIDC requires.

Services and host ports from the live file (the proxy is the primary entry
point; these raw ports remain mapped for direct debugging):

- `caddy` → `localhost:80` (the dev domain front door)
- `postgres` → `localhost:5440`
- `control` (`zeroship-control`) → `localhost:9090`
- `gateway` (`zeroship-gate`) → `localhost:8000`
- `auth` (`zeroship-auth`) → `localhost:9092`
- `hydra` (`oryd/hydra`) → `localhost:4444` (public) / `localhost:4445` (admin, dev-only)
- `sandbox` (`zeroship-sandbox`) → `localhost:9091`
- `redis` (`env.kv` store) has no host port
- `worker` (`zeroship-worker`) has no host port; scale it with `--scale worker=N`

The one-shot `migrate` and `hydra-migrate` services run to completion and exit;
`verdaccio` publishes loopback-only on `localhost:4873`.

### Image build

The single `Dockerfile` builds all SIX binaries (`zeroship-control`,
`zeroship-gate`, `zeroship-worker`, `zeroship-auth`, `zeroship-sandbox`, and the
`zeroship` CLI) in three stages:

1. **`sdks` (node:22)** runs `pnpm install --frozen-lockfile && pnpm build` to
   emit `sdks/bootstrap/dist/{runtime-entry,dispatcher}.js`. The runtime crate
   `include_str!`s those files at compile time
   (`crates/runtime/src/core/init.rs`), so they must exist before cargo touches
   `zeroship-runtime`. This stage also builds the console app's `.zship`, which
   the runtime stage copies to `/opt/zeroship/console/app.zship` for control's
   `--bootstrap-console` seed.
2. **`builder` (rust)** copies `crates/`, the freshly-built `sdks/`, and the
   `policies/` tree (`crates/authz/build.rs` parses `policies/*.cedar` at build
   time) and compiles the six binaries.
3. The runtime stage copies all six binaries plus the docker CLI (for the
   sandbox's Docker-out-of-Docker).

Because the SDK dist files are gitignored and absent from a fresh checkout, the
image must be (re)built with `--build` the first time; `docker compose build`
regenerates them inside the image.

### OpenAI key (console)

The console's AI codegen calls OpenAI to generate apps. Export `OPENAI_API_KEY`
before `docker compose up`; the `control` service reads it from its own process
env and `--bootstrap-console` writes it onto the seeded console app's server-side
env store (never the browser). Absent, the stack still boots — the console's AI
features degrade.

```bash
export OPENAI_API_KEY=sk-...
docker compose up --build
```

### Bind addresses

All four web binaries default to a **loopback** bind for safety, so each compose
command explicitly opts into a non-loopback address to be reachable across the
container network: `control` and `gateway` pass `--bind 0.0.0.0`, `zeroship-worker`
passes `--bind 0.0.0.0` (paired with `--worker-key`), and `zeroship-auth` passes
`--addr 0.0.0.0:9092`. Outside compose (single-host dev), the loopback defaults
need no override. Under `--dev-insecure`, control/gateway emit a warning when bound
non-loopback because app/admin auth is relaxed — only do this on a trusted network.

### Auth service

The `auth` service runs `zeroship-auth`, the OIDC IdP UI + RP that sits in front
of the hydra kernel. The gateway redirects unauthenticated end users to it via
the Caddy-fronted host (`--auth-ui-url http://auth.zeroship.localhost`). It runs
with `--dev-insecure` (relaxes cookie/secret guards for the private compose
network), `--bootstrap` (first-boot JWK + client creation), and
`--allow-remote-hydra-admin` (the admin API lives at the non-loopback
`http://hydra:4445`). The hydra URLs are NOT passed as flags: `hydra_public_url`
(`http://auth.zeroship.localhost`) and `hydra_admin_url` (`http://hydra:4445`)
come from the `[auth]` section of `ops/zeroship.toml`, auto-discovered at the
well-known `/etc/zeroship/zeroship.toml` mount — the same overlay that supplies
`trusted_oauth_clients` and `frame_ancestor_origins`. The DB DSN comes from
`[secrets].auth_db_url` (a `urn:zeroship:env:AUTH_DB_URL` reference resolved from
the service's `AUTH_DB_URL` env), not a literal `--db-url`. It mounts the
localhost OIDC client config, `ops/auth-clients-dev.toml`, at the well-known
`--clients-config` path (`/etc/zeroship/auth-clients.toml`), which it reconciles
against hydra admin at boot. That file declares the `console`, `cli`, and
`gateway` clients with `*.zeroship.localhost` redirect/logout URIs. The stash
signing key is unset; under `--dev-insecure` it falls back to the built-in dev
key.

The hydra kernel itself mounts `ops/hydra-dev.yaml` (not the prod
`ops/hydra.yaml`): same `:4444`/`:4445` listeners, but the issuer/public base is
`http://auth.zeroship.localhost` and `strategies.access_token` is `jwt` so RPs
verify tokens locally against the JWKS at
`http://auth.zeroship.localhost/.well-known/jwks.json`.

### Blob store

`control`, `gateway`, and `worker` all mount the `bundles` volume at
`/data/bundles` and pass `--blob-store /data/bundles`, so every service reads
and writes the same content-addressed deploy blobs. The worker also passes
`--storage-url /data/app-storage` (the `app-storage` volume) for the
creator-facing `env.storage` namespace.

The compose file already sets the current service names, keys, and sandbox env vars. Use it as the source of truth before copying flags into ad-hoc commands.

### Production object storage (S3 / R2 / MinIO)

The local-volume defaults above are the dev path. For a production-like
multi-node run — or to exercise the real S3 code path locally — point both the
deploy blob store and `env.storage` at an S3-compatible provider. Both use the
**same URL grammar** and the **same** AWS credentials (one S3 identity per
process), differing only by prefix:

```yaml
# control / gateway / worker — deploy blobs:
- --blob-store
- s3://my-bucket/deploy?region=us-east-1

# worker only — env.storage objects:
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
with `--blob-store s3://…` and the worker with `--storage-url s3://…`, deploys
a real app whose blobs now live in S3, asserts gateway→worker dispatch reading
the bundle from S3, and byte-compares a large (> 8 MiB part size) multipart
`env.storage` streaming round-trip. It skips cleanly when Docker is
unavailable.

### Console / AI builder

The AI builder is the **console** — a regular zeroship app, not a separate
service. Control starts with `--bootstrap-console --console-host
console.zeroship.localhost --console-zship /opt/zeroship/console/app.zship` in
this stack. On first boot it ingests that prebuilt `.zship` (emitted by the
image's `sdks` stage) and registers it as a public-PKCE gateway-fronted app, so
Caddy proxies `console.zeroship.localhost` to the gateway like any creator app.
Its runtime config — `OPENAI_API_KEY`, `SANDBOX_URL`/`SANDBOX_TOKEN`,
`ZEROSHIP_CONTROL_URL` — is forwarded from control's process env onto the seeded
console app's server-side env store at install time (secrets encrypted, plain
URLs as vars), replacing what the retired `builder` Vite service used to inject.
The standalone Vite container and its confidential OIDC client
(`--bootstrap-builder-client`, `BUILDER_REDIRECT_URI`, `BUILDER_CLIENT_SECRET`)
were removed in the R5 cutover.

### Configuration overlay

`docker-compose.yml` mounts `./ops/zeroship.toml` into `control`, `gateway`,
`worker`, and `auth` at the well-known path `/etc/zeroship/zeroship.toml`. The
compose stack relies on auto-discovery: because the file lives at the system
well-known path, no service passes `--config` — each binary's config resolver
finds it automatically. (Any service that does not mount the file simply falls
back to compiled defaults — discovery only fires when the file is present at the
well-known path.)

The overlay is the source of truth for the config-covered values, so they are
defined ONCE instead of being repeated as per-service flags:

- `[auth]` — Hydra URLs (`hydra_public_url = http://auth.zeroship.localhost`,
  the Caddy-fronted issuer; `hydra_admin_url = http://hydra:4445`, network-internal),
  `trusted_oauth_clients`, and `frame_ancestor_origins`. `control`, `gateway`,
  and `auth` no longer pass `--hydra-public-url` / `--hydra-admin-url`; `auth` no
  longer passes `FRAME_ANCESTOR_ORIGINS`.
- `[observability]` — shared `rust_log` / `log_format` (every service, worker
  included).
- `[secrets]` — REFERENCE-only (`urn:zeroship:env:<VAR>`, never a plaintext
  literal). `control_key`, `master_key`, `worker_key`, `database_url`, and
  `auth_db_url` are resolved per-binary from each service's `environment:` block,
  so the command lines carry no literal `--control-key` / `--master-key` /
  `--worker-key` / `--db` / `--db-url`. The single `database_url` reference points
  every binary at `ZEROSHIP_DATABASE_URL`; each service sets its OWN role-specific
  DSN under that name (control → `zeroship_control`, gateway → `zeroship_gateway`,
  worker → the privileged `postgres` provisioning superuser), so one shared
  reference resolves to distinct per-role DSNs. `auth` uses its own
  `AUTH_DB_URL` (the `auth_db_url` slot, flag `--db-url`).

Precedence is CLI/env-flag > `[secrets]`/`[auth]` file reference > default, so a
leftover literal flag would silently WIN and defeat the file — keep config-covered
values OFF the command lines. Copy `ops/zeroship.example.toml` to
`ops/zeroship.toml` when customizing an environment. The file itself stays
secret-free: it carries only `urn:`/`arn:` references, never a plaintext secret
(a literal in `[secrets]` is rejected at resolve). The actual secret VALUES live
in the compose `environment:` blocks (dev) or a real secret store (prod).

Validate a web binary's resolved config by adding `--check-config` to the
normal command. It runs the same startup guards, so include the same required
secret or dev-mode flags you would use for startup:

```bash
zeroship-<bin> --check-config --config <file> <normal required flags>
```

## Redis cluster test stack

`docker-compose.cluster.yml` is separate. It does **not** boot the platform stack; it only starts a 3-node Dragonfly cluster for `compio-redis` integration tests:

```bash
docker compose -f docker-compose.cluster.yml up -d
./scripts/bootstrap-dragonfly-cluster.sh
DRAGONFLY_CLUSTER_SEEDS='redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002' \
  cargo test -p compio-redis --test cluster -- --nocapture
docker compose -f docker-compose.cluster.yml down -v
```

The cluster file exposes `dragonfly-0`, `dragonfly-1`, and `dragonfly-2` on host ports `7000`, `7001`, and `7002`. These use `network_mode: host` (not a `ports:` mapping), so the ports can't be remapped and must be free on the host before you start the stack.

## OpenMeter metering-export test stack

`docker-compose.openmeter.yml` is a **separate, opt-in** stack used only by the
faithful OpenMeter metering-export e2e (`tests/e2e_openmeter_export.sh`). It does
**not** boot the platform stack and shares **nothing** with `docker-compose.yml`:
it is a distinct compose project (`name: zeroship-openmeter`) with its own
network, volumes, and a private `127.0.0.1`-only port band. In particular its
internal Postgres is OpenMeter metadata only and is **not** published on `:5440`
(the zeroship billing PG the cargo integration tests use), so bringing it up or
tearing it down never touches the main stack or the billing tests.

It stands up the minimal real OpenMeter pipeline — Kafka + ClickHouse + Redis +
Postgres + the OpenMeter API + a sink-worker — with a single `compute_units`
meter pre-provisioned in `ops/openmeter-config.yaml` to match exactly what
`crates/control/src/metering/provider/openmeter.rs` emits (`eventType` /
`slug` = `compute_units`, `aggregation: SUM` over `$.value`).

```bash
# Bring it up (pulls ~1 GB of images on first run); the API lands on :48888.
docker compose -f docker-compose.openmeter.yml up -d
curl -s http://127.0.0.1:48888/api/v1/meters | grep compute_units   # meter live?

# Run the faithful e2e (owns its OWN ephemeral zeroship PG on :5481, NOT :5440):
./tests/e2e_openmeter_export.sh

# Tear it down (volumes too):
docker compose -f docker-compose.openmeter.yml down -v
```

The e2e script brings the stack up/down for you; run the raw compose only when
iterating manually. `KEEP_OPENMETER=1 ./tests/e2e_openmeter_export.sh` leaves the
OpenMeter stack running between iterations. See
[Billing & metering](../reference/billing-metering.md) (OpenMeter §"Real-API
divergences from the mock") for what this e2e catches that the in-test mock cannot
(eventual-consistency lag + the query-window/`time` interaction).

## Database migrations

The shared Postgres `zeroship` schema is owned by Liquibase.
The one-shot `migrate` service runs `liquibase update` (changesets in
`db/changelog/`) after Postgres is healthy and before control/auth start — they
`depends_on` it with `service_completed_successfully`, so they only ever boot
against a fully-migrated schema. Hydra migrates its own schema separately
(`hydra-migrate`). See [Database migrations](db-migrations.md) for the layout,
`ops/db-migrate.sh`, and how to add a migration.

## Related docs

- [Database migrations](db-migrations.md) — Liquibase changesets, the `migrate` service, `ops/db-migrate.sh`.
- [Local dev setup](../runbooks/local-dev.md) — the same platform stack run as three bare `cargo`-built binaries instead of containers.
- [Nomad + Cloud Hypervisor sandbox](../runbooks/sandbox-nomad-ch.md) — operating the bare-metal VM sandbox backend.
- [Distributed architecture](../architecture/distributed.md) — what the `control`/`gateway`/`worker` services are and how they coordinate.
