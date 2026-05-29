# Docker Compose runbook

## Platform stack

`docker-compose.yml` is the day-to-day local platform stack — the whole
zeroship stack, fronted by a Caddy reverse proxy on the `*.zeroship.localhost`
dev domain. From the repo root:

```bash
# Build everything ahead (so `up` never builds): the shared runtime image
# (control/gateway/worker/auth/sandbox) + the frontend image (builder), plus
# the external images (postgres, hydra, caddy, verdaccio).
docker compose build                   # all Dockerfile-based services
docker compose pull                    # external images

docker compose up -d                   # boot the whole stack (build-free)
docker compose up -d --scale worker=10
docker compose logs -f
docker compose down -v
```

`docker compose up --build` also works (builds on the fly the first time). The
image compiles the SDKs (needed by the runtime crate) and all six binaries incl.
`zeroship-auth` — see [Image build](#image-build). The `builder` service uses a
separate `frontend` image target (the runtime image + Node 22) because its
`vite dev` spawns the `zeroship` runtime for the app's server functions.

> First boot is heavy: the image does a full `pnpm build` + release `cargo build`
> of the V8 runtime. Pre-building with `docker compose build` keeps later `up`s instant.

### Dev domain (via Caddy)

A `caddy` service listens on host `:80` and reverse-proxies the
`*.zeroship.localhost` domain. Browsers resolve any `*.localhost` hostname to
`127.0.0.1` automatically, so **no `/etc/hosts` edits are needed**. Once the
stack is up, open:

- **`http://builder.zeroship.localhost`** — the AI builder; describe an app and
  it builds + deploys it (requires `OPENAI_API_KEY`, see below)
- **`http://console.zeroship.localhost`** — creator dashboard / control plane
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

Server-side OIDC steps — control/gateway/builder exchanging codes, fetching
JWKS, and verifying tokens against the issuer `http://auth.zeroship.localhost` —
run *inside* the compose network, where that hostname would not otherwise
resolve. The `caddy` service therefore carries **network aliases** for
`auth.zeroship.localhost`, `console.zeroship.localhost`, `api.zeroship.localhost`,
and `builder.zeroship.localhost` on the default network, so containers resolve
those names to Caddy too. The net effect: the issuer URL the browser sees and
the one the servers verify against are identical, which OIDC requires.

Services and host ports from the live file (the proxy is the primary entry
point; these raw ports remain mapped for direct debugging):

- `caddy` → `localhost:80` (the dev domain front door)
- `postgres` → `localhost:5440`
- `control` (`zeroship-control`) → `localhost:9090`
- `gateway` (`zeroship-gate`) → `localhost:8000`
- `auth` (`zeroship-auth`) → `localhost:9092`
- `sandbox` (`zeroship-sandbox`) → `localhost:9091`
- `builder` (`apps/zeroship-builder` Vite dev server) → `localhost:3001`
- `worker` (`zeroship-worker`) has no host port; scale it with `--scale worker=N`

### Image build

The single `Dockerfile` builds all SIX binaries (`zeroship-control`,
`zeroship-gate`, `zeroship-worker`, `zeroship-auth`, `zeroship-sandbox`, and the
`zeroship` CLI) in three stages:

1. **`sdks` (node:22)** runs `pnpm install --frozen-lockfile && pnpm build` to
   emit `sdks/bootstrap/dist/{runtime-entry,dispatcher}.js`. The runtime crate
   `include_str!`s those files at compile time
   (`crates/runtime/src/core/init.rs`), so they must exist before cargo touches
   `zeroship-runtime`.
2. **`builder` (rust)** copies `crates/`, the freshly-built `sdks/`, and the
   `policies/` tree (`crates/authz/build.rs` parses `policies/*.cedar` at build
   time) and compiles the six binaries.
3. The runtime stage copies all six binaries plus the docker CLI (for the
   sandbox's Docker-out-of-Docker).

Because the SDK dist files are gitignored and absent from a fresh checkout, the
image must be (re)built with `--build` the first time; `docker compose build`
regenerates them inside the image.

### OpenAI key (builder)

The builder calls OpenAI to generate apps. Export `OPENAI_API_KEY` before
`docker compose up` (it is passed through to the `builder` service):

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
network), `--bootstrap` (first-boot JWK + client creation),
`--hydra-public-url http://auth.zeroship.localhost`,
`--hydra-admin-url http://hydra:4445`, and `--allow-remote-hydra-admin` (the
admin API lives at the non-loopback `http://hydra:4445`). It mounts the
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
and writes the same content-addressed deploy blobs.

The compose file already sets the current service names, keys, and sandbox env vars. Use it as the source of truth before copying flags into ad-hoc commands.

Control starts with `--bootstrap-builder-client` in this stack. On first boot it
registers the `zeroship-builder` OAuth client with Hydra admin (using the
`BUILDER_REDIRECT_URI=http://builder.zeroship.localhost/auth/callback` env so
the client's redirect matches the dev domain) and writes the generated dev
client secret to `data/builder-client-secret`. That file is mounted into the
Builder container, which exports it as `BUILDER_CLIENT_SECRET` before starting
Vite. The file is local dev state and is ignored by git. The builder's Vite dev
server allowlists `.zeroship.localhost` (`server.allowedHosts` in
`apps/zeroship-builder/vite.config.ts`) so it accepts the proxied Host header.

### Configuration overlay

`docker-compose.yml` mounts `./ops/zeroship.toml` into `control`, `gateway`,
`worker`, and `auth` at the well-known path `/etc/zeroship/zeroship.toml`. The
compose stack relies on auto-discovery: because the file lives at the system
well-known path, no service passes `--config` — each binary's config resolver
finds it automatically. The worker reads only `[observability]` from it; `auth`
reads the `[auth]` Hydra URLs. (Any service that does not mount the file simply
falls back to compiled defaults — discovery only fires when the file is present
at the well-known path.)

The overlay provides the shared `[auth]` Hydra URLs, `trusted_oauth_clients`,
and `[observability]` defaults so those values are defined once instead of per
service. In this stack `[auth].hydra_public_url` is
`http://auth.zeroship.localhost` (the Caddy-fronted issuer), while
`hydra_admin_url` stays `http://hydra:4445` (admin API, network-internal).
`control`, `gateway`, and `auth` also pass the public URL explicitly on the
command line, which wins over the overlay. Copy `ops/zeroship.example.toml` to
`ops/zeroship.toml` when customizing an environment. Secrets do not belong in
this file; keep them in env, CLI flags, or secret file paths.

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

## Database migrations

The shared Postgres schema (`control`/`auth`/`platform`) is owned by Liquibase.
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
