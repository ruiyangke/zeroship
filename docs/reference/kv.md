# @zeroship/kv — Key-Value SDK

`@zeroship/kv` is the key-value SDK for zeroship apps. It wraps the
native `env.kv` v8_class surface (registered by the Rust `KvPlugin`)
with typed methods that JSON-encode values, normalize counters, and
return the platform's standard `Result<T>` envelope.

```ts
import { kv } from "@zeroship/kv";

await kv.set("user:42:session", { token: "…" }, { ttlMs: 3600_000 });
const { data: session } = await kv.get<{ token: string }>("user:42:session");
```

## When to use `env.kv` vs `env.db`

> **`env.kv` is for ephemeral, hot-path, expiring data. `env.db` is for
> anything that is a source of truth.**

This is the single most important thing to internalize. Almost every KV
scenario is *also* implementable on `env.db` (which is strongly
consistent too). KV earns its place in exactly three situations:

1. **Don't pollute the relational store with high-frequency ephemeral
   churn** — rate-limit counters, page-view tallies, cache fills.
2. **TTL is a first-class feature** — entries that should self-expire
   (sessions, short-lived tokens, fixed-window counters) without a
   sweep job.
3. **Offload the source-of-truth DB** — keep hot reads off Postgres.

Everything that must survive an eviction or be the authoritative record
belongs in `env.db`:

| Use case                                  | Use            |
|-------------------------------------------|----------------|
| Rate-limit counter (fixed window)         | `env.kv` ✅     |
| Cache of an expensive computation         | `env.kv` ✅     |
| Short-lived session / one-time token      | `env.kv` ✅     |
| Ephemeral lock (auto-release via TTL)     | `env.kv` ✅     |
| Durable "process this exactly once"       | `env.db` (unique index) |
| User config / preferences                 | `env.db`       |
| Anything you'd be unhappy to silently lose| `env.db`       |

A KV entry can expire or be evicted at any time. Never store a value in
`env.kv` whose loss would be a correctness bug.

## Return contract

Every method returns a `Result<T>`:

```ts
type Result<T> = { data: T; error: null } | { data: null; error: Error };
```

On a backend failure the `error` is non-null and carries a stable
`.code` — branch on `error.code`, never substring-match the message:

```ts
const { data, error } = await kv.incr("rate:user:42", { by: 1, ttlMs: 60_000 });
if (error?.code === "kv_overflow") { /* counter saturated */ }
```

**Validation failures throw synchronously** as `TypeError`s *before* the
Promise is created (bad key shape, oversized value, malformed options) —
they are not delivered through the `Result` envelope. Wrap a call in
`try/catch` only if you pass user-controlled keys/values.

## Surface

All operations are async. Values are JSON-encoded by the SDK, so any
plain JSON-serializable value round-trips through `set` → `get`.

### `get<T>(key)` → `Result<T | null>`

Retrieve a value, JSON-decoded. Returns `null` when the key is missing
or its TTL has elapsed.

```ts
const { data } = await kv.get<{ token: string }>("user:42:session");
// data: { token: string } | null
```

### `getString(key)` → `Result<string | null>`

Convenience for values written as plain strings — returns the decoded
string directly. `null` when missing.

```ts
await kv.set("greeting", "hello");
const { data } = await kv.getString("greeting"); // "hello"
```

### `set<T>(key, value, opts?: { ttlMs? })` → `Result<void>`

Store a value (JSON-encoded). Pass `{ ttlMs }` to expire it.

```ts
await kv.set("cfg", { theme: "dark" });
await kv.set("flash", "saved!", { ttlMs: 5_000 });
```

### `delete(key)` → `Result<{ deleted: boolean }>`

Delete a key. `deleted` is `false` if the key was already absent.

```ts
const { data } = await kv.delete("flash"); // { deleted: true }
```

### `incr(key, opts?: { by?: number; ttlMs?: number })` → `Result<number>`

Atomically add to a counter and return the new value. `by` defaults to
`+1` (may be negative). `ttlMs` sets the expiry **only when this call
creates the key** — an existing counter keeps its current TTL. This is
the fixed-window rate-limit shape:

```ts
// First call of the window creates the key with a 60s TTL; subsequent
// calls increment without resetting the window.
const { data: count } = await kv.incr("rate:user:42", { by: 1, ttlMs: 60_000 });
if (count! > 100) throw new Error("rate limited");
```

Counters are **exact up to `Number.MAX_SAFE_INTEGER` (2^53)**. Beyond
that the native layer resolves a `bigint`; the SDK normalizes it back to
`number` with `Number(...)`, so precision degrades past 2^53 — not a
KV-counter use case. Use `env.db` for exact large-integer accumulation.

Errors: `kv_non_numeric` (existing value isn't an integer),
`kv_overflow` (i64 over/underflow — counters do not saturate).

> **Backend note:** on the redb single-process tier each `incr` fsyncs
> the increment, so a high-frequency counter (e.g. a per-request
> rate-limit hot path) pays one fsync per call — prefer the
> Redis/Dragonfly tier at scale.

### `setIfAbsent<T>(key, value, opts?: { ttlMs? })` → `Result<{ stored: boolean }>`

Atomically store a value only if the key is absent. `stored` is `false`
when the key already existed.

```ts
const { data } = await kv.setIfAbsent("lock:job:7", "worker-a", { ttlMs: 30_000 });
if (data!.stored) {
  // we hold the lock — it auto-releases in 30s if we crash
}
```

Use `setIfAbsent` for **ephemeral** locks / idempotency that auto-release
via `ttlMs`. For durable "process this exactly once" semantics, use
`env.db` with a unique index — a KV lock can expire or be evicted, which
is exactly what you do **not** want for a once-only guarantee.

### `expire(key, ttlMs)` → `Result<{ updated: boolean }>`

Set or replace a key's TTL. `updated` is `false` if the key is absent.

```ts
await kv.expire("user:42:session", 3600_000);
```

### `ttl(key)` → `Result<{ ttlMs: number | null } | null>`

Inspect a key's TTL. Three distinct outcomes:

| Result            | Meaning                                  |
|-------------------|------------------------------------------|
| `null`            | key does not exist                       |
| `{ ttlMs: null }` | key exists, no expiry set                |
| `{ ttlMs: <n> }`  | key exists, `<n>` ms until it expires    |

```ts
const { data } = await kv.ttl("user:42:session");
if (data === null) { /* gone */ }
else if (data.ttlMs === null) { /* permanent */ }
else { /* expires in data.ttlMs ms */ }
```

### `persist(key)` → `Result<{ updated: boolean }>`

Remove a key's TTL (make it permanent). `updated` is `false` if the key
has no TTL or is absent.

### `list(prefix?, opts?: { cursor?: string; limit?: number })` → `Result<{ keys: string[]; cursor: string | null }>`

List keys under an optional **literal** `prefix` (no glob), paginated.

The `cursor` is **opaque and backend-specific** — do not parse, store
across deploys, or construct it. Pass the returned `cursor` back to fetch
the next page. **`cursor === null` means the listing is exhausted.**

```ts
let cursor: string | null = null;
do {
  const { data } = await kv.list("session:", { cursor: cursor ?? undefined, limit: 500 });
  for (const key of data!.keys) { /* … */ }
  cursor = data!.cursor;
} while (cursor !== null);
```

`limit` defaults to **1000** and is clamped to **10 000** by the runtime
(a larger value is capped, not rejected). If a backend ever returns a
runaway page over the cap, the call rejects with `kv_list_too_large`.

> **Backend note:** the Redis/Dragonfly tier maps `list` onto `SCAN`,
> whose at-least-once semantics mean a key may appear in more than one
> page if the keyspace changes mid-iteration; the redb tier paginates
> exactly (each key returned once).

## SDK-only conveniences

These are pure JS composed over the kernel — they issue no new native
primitive.

### `has(key)` → `Result<boolean>`

`true` if the key exists (and hasn't expired). A `get` + non-null check.

### `getOrSet<T>(key, opts: { ttlMs? }, factory)` → `Result<T>`

Cache-aside read-through: return the stored value, or compute it via
`factory`, store it (with optional `ttlMs`), and return it.

```ts
const { data } = await kv.getOrSet("report:daily", { ttlMs: 600_000 }, async () => {
  return await expensiveComputation();
});
```

**Not atomic.** Under a cache miss, concurrent callers may each run
`factory` (a "stampede"). That's acceptable for caching — the last `set`
wins and every caller gets a valid value. If you need exactly-once
computation, use `setIfAbsent` as a lock instead.

### `namespace(prefix)` → `Kv`

Return a sub-client that transparently prepends `prefix` (string concat)
to every key. Pure key-prefixing sugar — there is no native namespace
concept; `list()` queries the combined prefix and returned keys keep
their full stored form. Namespaces compose.

```ts
const sessions = kv.namespace("session:");
await sessions.set("abc", data);   // writes "session:abc"
const { data: s } = await sessions.get("abc");
```

## Error codes

The validation-class failures throw synchronously as `TypeError`s; the
rest surface on the async path with a stable `error.code`.

| Code / shape          | When                                                       |
|-----------------------|------------------------------------------------------------|
| `TypeError` (sync)    | Invalid key (empty / > 512 B / contains `{`, `}`, NUL, or control chars), invalid value (not a string / > 256 KiB), or a malformed option (negative / non-finite `ttlMs`, fractional / out-of-range `by`, non-string `cursor`, …). |
| `kv_non_numeric`      | `incr` on a key whose existing value isn't a base-10 integer. |
| `kv_overflow`         | `incr` over/underflowed the i64 range (counters do not saturate). |
| `kv_list_too_large`   | A backend returned more keys in one page than the hard cap (runaway protection). |
| `kv_connection`       | Backend connect / transport failure (credentials redacted from the message). Carries a retry hint — safe to retry after a short backoff. |
| `kv_backend`          | Catch-all backend failure (unexpected reply, server error).   |

Branch on `error.code`; never substring-match `error.message`.

## Limits

| Limit                 | Value     | Notes                                        |
|-----------------------|-----------|----------------------------------------------|
| Max key length        | 512 B     | Enough for namespaced keys (`user:123:session:abc`). Keys may not contain `{`, `}`, NUL, or control chars. |
| Max value size        | 256 KiB   | KV is for ephemeral hot-path data, not blobs — large payloads belong in `env.storage`. |
| `list` default page   | 1000 keys | Override with `opts.limit`.                  |
| `list` max page       | 10 000    | A larger `limit` is clamped, not rejected.   |

## Backends

The SDK speaks one wire contract; the runtime selects the backend at
boot. Both are functionally identical from JS:

- **redb** — embedded persistent tier (dev / self-host / single-node),
  and the test backend. Selected when no `ZEROSHIP_KV_URL` is set: the
  file is `ZEROSHIP_KV_PATH` if set, else the default `./.zeroship/kv.redb`.
  Survives restart with immediate durability. **Single-process only** —
  redb takes an exclusive file lock, so a multi-worker-process
  deployment must use Redis.
- **Redis / Dragonfly** — distributed production tier. Selected via
  `ZEROSHIP_KV_URL`. Atomic `incr`-with-TTL-on-create runs as a small
  Lua script; `list` maps to a cursor-passthrough `SCAN`. Use this for
  any multi-node deployment.

## Native surface (advanced)

The SDK calls into the native `env.kv` v8_class registered by the Rust
`KvPlugin`. App code uses the SDK; the native methods (`get`, `set`,
`delete`, `incr`, `setIfAbsent`, `expire`, `ttl`, `persist`, `list`) are
documented in `crates/plugin-kv/` and rarely called directly. The wire
contract: `get` resolves the raw stored string or `null`; `set` resolves
`{ ok: true }`; `incr` resolves a JS `number` (or `bigint` above 2^53);
`ttl` resolves `null` / `{ ttlMs }`. The runtime implementation lives in
`crates/plugin-kv/`.
