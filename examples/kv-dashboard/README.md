# kv-dashboard

Vite + React example for the `@zeroship/kv` SDK. It keeps all state in
the native `env.kv` backend through the public SDK wrapper.

## What It Exercises

| API | Example |
| --- | --- |
| `get` | snapshots, quote cache reads, lease owner reads |
| `getString` | string + TTL panel |
| `set` | feature flag, sessions, quote cache, string value |
| `delete` | session delete, lease reset, string delete, demo cleanup |
| `incr` | page visits and rate-limit windows |
| `setIfAbsent` | short-lived deployment lease |
| `expire` | string + TTL panel |
| `ttl` | sessions, cache entries, lease expiry, string expiry, fixed-window counter |
| `persist` | string + TTL panel |
| `list` | keyspace browser and session list |
| `has` | string + TTL panel and memoized cache probe |
| `getOrSet` | memoized value panel |
| `namespace` | all keys live under `kv-demo:`; sessions use `kv-demo:session:` |

RPC IDs are dotted and explicit:

- `kv.snapshot`
- `kv.visit`
- `kv.flag.set`
- `kv.rate.hit`
- `kv.cache.quote`
- `kv.memo.get`
- `kv.lease.acquire`
- `kv.lease.clear`
- `kv.session.create`
- `kv.session.delete`
- `kv.string.set`
- `kv.string.expire`
- `kv.string.persist`
- `kv.string.delete`
- `kv.keys.list`
- `kv.clear`

## Run Locally

```bash
pnpm install
pnpm --dir examples/kv-dashboard dev
```

To run the native Rust acceptance test, with Docker running:

```bash
pnpm build
pnpm --dir examples/kv-dashboard smoke
```

This builds the app and platform, starts PostgreSQL and Redis through
Testcontainers, and checks the same SDK operations through an owned Vite
process and the deployed gateway. It manages its own runtime processes and
temporary state; an existing dev server is unnecessary. The test also works
with `cargo nextest run -p zeroship-cli --test kv_deployment` from the repo root.

The dev runtime uses redb by default at `examples/kv-dashboard/.zeroship/kv.redb`.
Set `ZEROSHIP_KV_CONFIG_FILE` to a Redis TOML configuration to select Redis;
see the [configuration reference](../../docs/reference/kv-configuration.md).

The Vite app runs on Vite's selected port. The zeroship runtime API defaults
to `http://localhost:3011`; set `KV_DASHBOARD_API_PORT` to use another port.
