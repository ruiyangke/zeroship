# `@zeroship/kv`

`@zeroship/kv` is the public SDK wrapper around the native `env.kv`
namespace. It is for ephemeral, hot-path, expiring state: rate-limit
counters, short-lived leases, cache-aside values, session-like scratch data,
and prefix scans. Use `@zeroship/db` for relational source-of-truth data,
audited workflows, durable idempotency, and exact large-number accounting.

The native class is registered by `crates/plugin-kv/`; the TypeScript wrapper
lives in `sdks/kv/src/index.ts`.

## Authoring surface

```ts
import { kv } from "@zeroship/kv";

await kv.set("session:abc", { userId: "user_..." }, { ttlMs: 30 * 60_000 });

const { data: session, error } = await kv.get<{ userId: string }>("session:abc");
if (error) throw error;

if (session) {
  // ...
}
```

All SDK calls return `Result<T>`:

```ts
type Result<T> = { data: T; error: null } | { data: null; error: Error };
```

Branch on `error.code` for known native failures such as
`kv_non_numeric` and `kv_overflow`; do not branch on message text.

## Methods

| Method | Behavior |
| --- | --- |
| `get<T>(key)` | Reads a JSON value and returns `T | null`; `null` means missing or expired. |
| `getString(key)` | Reads a JSON-encoded string and returns the string directly. |
| `set(key, value, { ttlMs? })` | JSON-encodes and stores a value, optionally with a TTL. |
| `delete(key)` | Removes a key; returns `{ deleted }`. |
| `incr(key, { by?, ttlMs? })` | Atomically increments an integer counter; `ttlMs` applies only when the key is created. |
| `setIfAbsent(key, value, { ttlMs? })` | Atomic "create if missing"; returns `{ stored }`. |
| `expire(key, ttlMs)` | Sets or replaces a key's TTL; returns `{ updated }`. |
| `ttl(key)` | Returns `null` when missing, `{ ttlMs: null }` when permanent, or `{ ttlMs: number }`. |
| `persist(key)` | Removes a key's TTL; returns `{ updated }`. |
| `list(prefix?, { cursor?, limit? })` | Paginates keys by literal prefix. Cursor is opaque. |
| `has(key)` | SDK helper over `get`. |
| `getOrSet(key, opts, factory)` | Cache-aside helper; not atomic under concurrent misses. |
| `namespace(prefix)` | Returns a sub-client that prepends `prefix` to every key. |

## TTL and counters

TTLs are in milliseconds:

```ts
await kv.set("cache:quote:starter", quote, { ttlMs: 30_000 });
const ttl = await kv.ttl("cache:quote:starter");
```

`incr` is the fixed-window rate-limit primitive:

```ts
const key = `rate:${userId}:${Math.floor(Date.now() / 60_000)}`;
const { data: count, error } = await kv.incr(key, {
  by: 1,
  ttlMs: 60_000,
});
if (error) throw error;

if (count > 100) throw new Error("rate limited");
```

The TTL on `incr` is set only when the counter is created. Existing counters
keep their current expiry, which is what fixed-window counters need.

Counters are exact in the native layer up to i64, but the JS wrapper returns a
`number`. Values past `Number.MAX_SAFE_INTEGER` lose precision; use DB rows for
exact large accumulators.

## Leases and idempotency

`setIfAbsent` is useful for short-lived, auto-releasing coordination:

```ts
const lease = await kv.setIfAbsent(
  "lease:deploy",
  { owner: "worker-a", startedAt: Date.now() },
  { ttlMs: 30_000 },
);

if (!lease.data?.stored) return "already running";
```

This is deliberately ephemeral. If the operation must be durably processed
exactly once, model it in `@zeroship/db` with a unique key.

## Prefix listing

`list` takes a literal prefix, not a glob. Returned keys keep their full stored
form, including namespace prefixes.

```ts
let cursor: string | null = null;
do {
  const page = await kv.list("session:", {
    cursor: cursor ?? undefined,
    limit: 100,
  });
  if (page.error) throw page.error;

  for (const key of page.data.keys) {
    // key is "session:..."
  }
  cursor = page.data.cursor;
} while (cursor !== null);
```

`namespace(prefix)` is pure SDK sugar over string prefixes:

```ts
const sessions = kv.namespace("session:");
await sessions.set("abc", { userId });

// Stored key is "session:abc".
// Listing through the namespace queries the combined prefix.
const page = await sessions.list("");
```

## Backends

The runtime selects the backend; the SDK contract is the same:

- redb: single-process persistent local backend, used by dev by default.
- Redis: distributed backend, selected with `ZEROSHIP_KV_URL`.

Dev redb state lives under the app's `.zeroship/` directory. Treat it as local
runtime state that should survive restarts, like the SQLite dev database.

## Demo coverage

`examples/kv-dashboard/` exercises every SDK method: JSON values, strings,
TTL, counters, leases, cache-aside, namespacing, prefix list pagination, and
cleanup.
