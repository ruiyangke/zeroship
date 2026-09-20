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

## Tests

All test code, configuration, and dependencies belong to this example.
Vitest checks the public RPC contract; Playwright opens the real dashboard
from Vitest to check UI actions and recovery from request failures.

Against an existing dev server:

```bash
pnpm --dir examples/kv-dashboard smoke
pnpm --dir examples/kv-dashboard test:browser
```

`ZEROSHIP_URL` selects the API endpoint; `KV_DASHBOARD_UI_URL` selects the UI
endpoint for browser tests. Both suites reset the demo's `kv-demo:` data.
For a deployed app, set both URLs to its app origin, such as
`http://kvdash.localhost:<gateway-port>`.

The complete acceptance test provisions its own platform:

```bash
pnpm build
pnpm --dir examples/kv-dashboard exec playwright install chromium
pnpm --dir examples/kv-dashboard test
```

On NixOS, use Chromium from Nix on `PATH`, or set
`PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH` to its executable. The tests otherwise
use Playwright's browser installation. Missing browsers or Docker fail the run.

`pnpm test` runs Vitest directly. Its TypeScript global setup uses
Testcontainers for Node to start PostgreSQL, Redis, and an issuer JWKS fixture.
It builds the demo and platform binaries, applies platform migrations, and
creates and deploys the app through authenticated control APIs and the CLI.
The RPC and browser suites then run against local redb and deployed Redis.
Teardown cleans up the owned processes, containers, and temporary app data.
Logs and browser failure screenshots live in `tests/.artifacts/`.

`pnpm test:acceptance` runs the dashboard suites with their platform fixture.
`pnpm test:unit` checks the HTTP helper and process cleanup without starting
the platform. The `smoke` and `test:browser` commands use Vitest's `existing`
mode to target running servers without provisioning them.

The dev runtime uses redb by default at `examples/kv-dashboard/.zeroship/kv.redb`.
Set `ZEROSHIP_KV_CONFIG_FILE` to a Redis TOML configuration to select Redis;
see the [configuration reference](../../docs/architecture/kv-configuration.md).

The Vite app runs on Vite's selected port. The zeroship runtime API defaults
to `http://localhost:3011`; set `KV_DASHBOARD_API_PORT` to use another port.
