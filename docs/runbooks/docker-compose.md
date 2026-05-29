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
- `sandbox` (`zeroship-sandbox`) → `localhost:9091`
- `builder` (`apps/zeroship-builder` Vite dev server) → `localhost:3001`
- `worker` (`zeroship-worker`) has no host port; scale it with `--scale worker=N`

The compose file already sets the current service names, keys, and sandbox env vars. Use it as the source of truth before copying flags into ad-hoc commands.

Control starts with `--bootstrap-builder-client` in this stack. On first boot it
registers the `zeroship-builder` OAuth client with Hydra admin and writes the
generated dev client secret to `data/builder-client-secret`. That file is
mounted into the Builder container, which exports it as `BUILDER_CLIENT_SECRET`
before starting Vite. The file is local dev state and is ignored by git.

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
