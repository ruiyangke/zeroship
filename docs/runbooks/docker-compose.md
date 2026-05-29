# Docker Compose runbook

## Platform stack

`docker-compose.yml` is the day-to-day local platform stack. From the repo root:

```bash
docker compose up -d
docker compose up -d --scale worker=10
docker compose logs -f
docker compose down -v
```

Services and host ports from the live file:

- `postgres` → `localhost:5440`
- `control` (`zeroship-control`) → `localhost:9090`
- `gateway` (`zeroship-gate`) → `localhost:8000`
- `auth` (`zeroship-auth`) → `localhost:9092`
- `sandbox` (`zeroship-sandbox`) → `localhost:9091`
- `builder` (`apps/zeroship-builder` Vite dev server) → `localhost:3001`
- `worker` (`zeroship-worker`) has no host port; scale it with `--scale worker=N`

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
of the hydra kernel. The gateway redirects unauthenticated end users to it
(`--auth-ui-url http://auth:9092`). It runs with `--dev-insecure` (relaxes
cookie/secret guards for the private compose network), `--bootstrap` (first-boot
JWK + client creation), and `--allow-remote-hydra-admin` because the shared
overlay points it at the non-loopback `http://hydra:4445` admin API. It mounts
the shared overlay (for `hydra_admin_url` / `hydra_public_url`) and
`ops/auth-clients.example.toml` at the well-known `--clients-config` path
(`/etc/zeroship/auth-clients.toml`), which it reconciles against hydra admin at
boot. The stash signing key is unset; under `--dev-insecure` it falls back to
the built-in dev key.

### Blob store

`control`, `gateway`, and `worker` all mount the `bundles` volume at
`/data/bundles` and pass `--blob-store /data/bundles`, so every service reads
and writes the same content-addressed deploy blobs.

The compose file already sets the current service names, keys, and sandbox env vars. Use it as the source of truth before copying flags into ad-hoc commands.

Control starts with `--bootstrap-builder-client` in this stack. On first boot it
registers the `zeroship-builder` OAuth client with Hydra admin and writes the
generated dev client secret to `data/builder-client-secret`. That file is
mounted into the Builder container, which exports it as `BUILDER_CLIENT_SECRET`
before starting Vite. The file is local dev state and is ignored by git.

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
service. Copy `ops/zeroship.example.toml` to `ops/zeroship.toml` when
customizing an environment. Secrets do not belong in this file; keep them in
env, CLI flags, or secret file paths.

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

## Related docs

- [Local dev setup](../runbooks/local-dev.md) — the same platform stack run as three bare `cargo`-built binaries instead of containers.
- [Nomad + Cloud Hypervisor sandbox](../runbooks/sandbox-nomad-ch.md) — operating the bare-metal VM sandbox backend.
- [Distributed architecture](../architecture/distributed.md) — what the `control`/`gateway`/`worker` services are and how they coordinate.
