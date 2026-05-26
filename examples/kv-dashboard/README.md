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

In another shell:

```bash
pnpm --dir examples/kv-dashboard smoke
```

The dev runtime uses redb by default at `examples/kv-dashboard/.zeroship/kv.redb`.
Set `ZEROSHIP_KV_URL` to use Redis instead.

The Vite app runs on Vite's selected port. The zeroship runtime API defaults
to `http://localhost:3011`; set `KV_DASHBOARD_API_PORT` to use another port.
