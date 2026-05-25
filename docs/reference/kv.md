# `@zeroship/kv`

`@zeroship/kv` is the SDK wrapper around the native `env.kv` namespace. The native class is registered in [crates/plugin-kv/src/lib.rs](crates/plugin-kv/src/lib.rs); the JS wrapper lives in [sdks/kv/src/index.ts](sdks/kv/src/index.ts).

## Authoring surface

```ts
import { kv } from "@zeroship/kv";

await kv.set("session:123", { ok: true }, { ttl: 300 });
const session = await kv.get<{ ok: boolean }>("session:123");
```

The package name is `@zeroship/kv`. There is no separate client package.

## Native methods

The current native `env.kv` methods are defined by [crates/plugin-kv/src/v8_class.rs](crates/plugin-kv/src/v8_class.rs):

- `get`
- `set`
- `delete`
- `incr`
- `setIfAbsent`
- `expire`
- `ttl`
- `persist`
- `list`

## SDK helpers

The JS wrapper in [sdks/kv/src/index.ts](sdks/kv/src/index.ts) adds a small convenience layer:

- `getString`
- `has`
- `getOrSet`
- `namespace`

All SDK calls return a `Result<T>` envelope with `{ data, error }`.

## Scope

Use KV for simple key/value state, TTL-based records, counters, and prefix scans. For typed relational data and queries, use [`@zeroship/db`](docs/reference/db.md).
