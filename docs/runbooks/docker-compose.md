# Docker Compose runbook

The repo ships two compose files for local multi-node simulation:

- `docker-compose.yml` — the day-to-day stack: `postgres`, `control`,
  `gateway`, `worker` (scalable), `sandbox`.
- `docker-compose.cluster.yml` — additional services for cluster-mode
  testing.

```bash
docker compose up -d                          # default: 1 gw + 3 workers
docker compose up -d --scale worker=10        # scale to 10 workers
docker compose logs -f                        # watch all logs
docker compose down -v                        # tear down (volumes too)
```

For day-zero setup details see [`local-dev.md`](./local-dev.md); for
multi-node prod-shape testing see the cluster file's inline comments.

## Postgres image — `pgvector/pgvector:pg16`

`plugin-db`'s P4 search capabilities (vector, full-text, geo) need two
Postgres extensions in production:

- `pgvector` — backs `t.vector(dims)` and `db.<coll>.search({ vector })`.
- `postgis` — backs `t.geoPoint()` and `db.<coll>.near({ point, radius })`.

The default `docker-compose.yml` currently pins `postgres:16`, which
ships **neither**. Calls into `Collection.search` (vector branch) or
`Collection.near` will return a typed `vector_extension_missing` /
`postgis_extension_missing` error from the PG backend probe. The
prescribed swap is:

```yaml
services:
  postgres:
    # Was: image: postgres:16
    image: pgvector/pgvector:pg16
    environment:
      POSTGRES_PASSWORD: zeroship
      POSTGRES_DB: zeroship
    # ... unchanged
```

The `pgvector/pgvector:pg16` image is a drop-in for `postgres:16` —
same major version, same wire protocol, same on-disk layout — but
bundles **both** `pgvector` AND `postgis` packages pre-installed. The
extensions still need a one-time `CREATE EXTENSION` per database:

```sql
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS postgis;
```

### Where to run the `CREATE EXTENSION` statements

The platform has no single "bootstrap migration" file — each component
that needs an extension creates it on first contact (`plugin-db`'s
auth bootstrap creates `pgcrypto`; `control/auth_service.rs` creates
`uuid-ossp`). `pgvector` and `postgis` are **operator-owned** today:
the PG backend probe surfaces the missing-extension error with a
hint, and the operator runs the `CREATE EXTENSION` statements once
against the platform database. Either:

- **(recommended)** run the two statements once during platform
  install — they're idempotent — using any client (`psql`, the
  control plane's connection, a `docker compose exec` one-liner):

  ```bash
  docker compose exec postgres \
    psql -U postgres -d zeroship -c \
    "CREATE EXTENSION IF NOT EXISTS vector; CREATE EXTENSION IF NOT EXISTS postgis;"
  ```

- **(alternative)** add them to your own infra-as-code init layer
  (`postgres-init.d/`, a sidecar Job, Terraform `postgresql_extension`,
  …). The platform code doesn't assume any particular install path.

### CI consideration

The image swap is **not yet** committed to `docker-compose.yml` — the
file still pins `postgres:16` for the default stack. The reason: the
`pgvector/pgvector:pg16` image is ~30% larger than vanilla `postgres:16`
and not every CI run exercises the P4 search paths. Track the swap as a
follow-up; until then operators who want to use vector/FTS/geo on PG
locally should override the image in a `docker-compose.override.yml` or
flip the line by hand.

## Other images

Every other service builds from the workspace `Dockerfile` (`build: .`).
Cache hits on a clean repo come from the workspace `cargo build --release`
during the image build — see the comments in `Dockerfile`.

## Networking notes

- `gateway` resolves `worker` DNS via Docker's built-in DNS round-robin;
  the entrypoint shell loop fan-outs to every IP behind the `worker`
  name.
- `sandbox` participates in two networks: `default` (for the control
  plane) and `zeroship-sandbox-net` (where the per-app sandbox
  containers it spawns live).
- Volumes: `bundles` (deploy artifacts), `/var/run/docker.sock`
  (Docker-out-of-Docker for `sandbox`), `/var/zeroship/projects` (the
  sandbox workspace bind mount).

See `docker-compose.yml` inline comments for the full why of every
flag.
