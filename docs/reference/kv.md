# `@zeroship/kv`

`@zeroship/kv` is the key-value SDK a creator app imports to store ephemeral,
hot-path, expiring state: rate-limit counters, short-lived leases, cache-aside
values, session-like scratch data, and prefix scans. Every operation is atomic
for a single key and strongly consistent — a write is immediately visible to a
later read, and counters and create-if-missing are atomic. Use `@zeroship/db`
for relational source-of-truth data, audited workflows, durable idempotency,
and exact large-number accounting.

The simplest working call writes a value and reads it back:

```ts
import { kv } from "@zeroship/kv";

await kv.set("session:abc", { userId: "usr_..." }, { ttlMs: 30 * 60_000 });
const { data: session } = await kv.get<{ userId: string }>("session:abc");

// session is the stored object, or null if the key was missing or expired.
```

The backend is selected by the host; the SDK contract is identical either way.

## Authoring surface

```ts
import { kv } from "@zeroship/kv";

await kv.set("session:abc", { userId: "usr_..." }, { ttlMs: 30 * 60_000 });

const { data: session, error } = await kv.get<{ userId: string }>("session:abc");
if (error) throw error;

if (session) {
  // ...
}
```

The `kv` export is a ready-to-use client for the running app. `createKv()` is
the test factory: it returns a fresh client, and accepts a mock native handle
so you can exercise your code without the runtime.

All calls return a `Result<T>`:

```ts
type Result<T> = { data: T; error: null } | { data: null; error: Error };
```

On success `error` is `null`. On failure `data` is `null` and `error` is set;
branch on `error.code` for known native failures such as `kv_non_numeric` and
`kv_overflow`, never on message text. See [Errors](#errors).

## Testing

`createKv()` returns a fresh client over a mock you supply instead of the
runtime, so you can exercise your code without a running app. The mock is a
plain object with nine async methods mirroring the handle the runtime gives a
real client; the SDK owns the JSON round-trip, so your mock stores and returns
strings, never objects:

```ts
import { createKv } from "@zeroship/kv";

const store = new Map<string, string>();

const kv = createKv({
  async get(key) { return store.get(key) ?? null; },            // string | null
  async set(key, value) { store.set(key, value); return { ok: true }; },
  async delete(key) { return { deleted: store.delete(key) }; },
  async incr(key, opts) { /* ... */ return 1; },                // number | bigint
  async setIfAbsent(key, value) { /* ... */ return { stored: true }; },
  async expire(key, ttlMs) { return { updated: false }; },
  async ttl(key) { return { ttlMs: null }; },                   // { ... } | null
  async persist(key) { return { updated: false }; },
  async list(prefix, opts) { return { keys: [], cursor: null }; },
});
```

A mock method may also throw to simulate a failure; the SDK wraps the thrown
value into `error` exactly as it does for a real backend.

## Methods

| Method | Returns | Behavior |
| --- | --- | --- |
| `get<T>(key)` | `Result<T \| null>` | JSON-decoded value, or `null` when missing or expired. |
| `getString(key)` | `Result<string \| null>` | Equivalent to `get<string>(key)`: returns the stored string, JSON-decoded, or `null` when missing or expired. |
| `set(key, value, { ttlMs? })` | `Result<void>` | JSON-encodes and stores; replaces the value and any existing expiry. |
| `delete(key)` | `Result<{ deleted: boolean }>` | Removes a key; `deleted` is `false` if it was already absent. |
| `incr(key, { by?, ttlMs? })` | `Result<number>` | Atomic increment; `by` defaults to `1`; `ttlMs` applies only on creation. |
| `setIfAbsent(key, value, { ttlMs? })` | `Result<{ stored: boolean }>` | Atomic create-if-missing; `stored` is `false` if the key existed. |
| `expire(key, ttlMs)` | `Result<{ updated: boolean }>` | Sets or replaces a key's TTL; `updated` is `false` if the key is absent. |
| `ttl(key)` | `Result<{ ttlMs: number \| null } \| null>` | `data` is `null` when missing; otherwise `{ ttlMs: number \| null }` — `null` when permanent, else remaining ms. |
| `persist(key)` | `Result<{ updated: boolean }>` | Removes a key's TTL; `updated` is `false` if it had none or was absent. |
| `list(prefix?, { cursor?, limit? })` | `Result<{ keys: string[], cursor: string \| null }>` | Paginates keys by literal prefix; the cursor is opaque. |
| `has(key)` | `Result<boolean>` | SDK helper over `get`. |
| `getOrSet<T>(key, { ttlMs? }, factory)` | `Result<T>` | Read-through: returns the stored value, or runs `factory() => T \| Promise<T>`, stores it, and returns it; not atomic under concurrent misses. |
| `namespace(prefix)` | a `Kv` sub-client | Same surface as `kv`; prepends `prefix` to every key. |

## Keys, values, and limits

Keys are strings that identify an entry. A key must be non-empty and no longer
than 512 bytes, and may not contain `{`, `}`, the NUL character, or any control
character.

Values are JSON-serialized on write and JSON-parsed on read, so store anything
JSON-serializable: objects, arrays, strings, numbers, booleans, or `null`. A
stored value may be at most 256 KiB. Values that JSON cannot represent, such as
`undefined`, functions, or `bigint`, cannot be stored.

TTLs are whole milliseconds. A TTL must be greater than `0` and no more than
100 years (`3,153,600,000,000` ms); fractional and negative values are
rejected. `incr`'s `by` must be an integer and defaults to `1`; fractions are
rejected, and the resulting counter stays within the 64-bit signed range.

## Errors

Every failure comes back through `Result` as `error`. Two kinds occur:

- An invalid key, value, or option is rejected before the operation runs and
  surfaces in `error` as a `TypeError` with no `.code`.
- A known native failure carries an `.code`. Branch on it:

| Code | Meaning | Retry? |
| --- | --- | --- |
| `kv_non_numeric` | `incr` on a key whose value is not a base-10 integer. | no |
| `kv_overflow` | `incr` would push the counter past the signed 64-bit range. | no |
| `kv_connection` | Transient connection failure; `error.hint` carries the retry guidance string. | yes, with backoff |
| `kv_backend` | Any other backend failure; carries no retry hint. | depends: retry a read; retry a write only if idempotent |

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

Counters are exact up to `Number.MAX_SAFE_INTEGER` (2^53). Above that the
result loses precision past 2^53; use DB rows for exact large accumulators.

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

### `limit` is a request, not a bound

`limit` asks for a page size; it does not cap one. The default is `1000` and
any value above `10000` is clamped to `10000`. During local development the
store fills a page to exactly `limit` and stops, but a deployed app can return
a page shorter or longer than asked, including empty while keys still remain.

That difference bites one common idiom: stopping once a page comes back short.
Against the local store it terminates correctly, because that store returns a
cursor only when it filled the page. Against a deployed app it truncates the
listing silently. `cursor === null` is the only exhaustion signal, which is why
the loop above tests the cursor and not the page length.

### Key order is unspecified

`list` makes no ordering guarantee, and the order genuinely differs between
backends: the local store iterates keys in sorted order, while a deployed app
may return them in any order. Code that renders or compares a key list will
therefore see one order locally and another in production.

Sort explicitly whenever order matters:

```ts
const page = await kv.list("session:", { limit: 100 });
if (page.error) throw page.error;
const keys = [...page.data.keys].sort();
```

Sorting a single page does not sort the whole keyspace. If you need a globally
ordered result, collect every page first and sort the combined array.

`namespace(prefix)` is pure SDK sugar over string prefixes:

```ts
const sessions = kv.namespace("session:");
await sessions.set("abc", { userId });

// Stored key is "session:abc".
// Listing through the namespace queries the combined prefix.
const page = await sessions.list("");
```

## Backends

Local development and a deployed app may run on different underlying stores;
the host picks, and the SDK contract — every method, error code, and return
shape — is identical either way. The only creator-visible difference is `list`
ordering and page fill, described above.