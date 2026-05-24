# plugin-kv redesign — implementation plan (2026-05-24)

Status: pre-ship proposal, drafted in worktree `kv-v8class`. Do NOT commit until the implementing PR. Pre-launch → no back-compat (rename/break freely).

> **Amended 2026-05-24 (commit 4): InMemory removed.** The shipped design is **two backends, not three**: `RedbBackend` (embedded persistent — also the test backend, a default feature) + `Redis` (distributed). The `InMemory` backend and `KvPlugin::in_memory()` / `Default` are gone; redb's test suite carries the canonical-semantics coverage that lived in `memory.rs`. `serve` defaults to redb at `./.zeroship/kv.redb` (override with `ZEROSHIP_KV_PATH`); `ZEROSHIP_KV_URL` still selects Redis. The text below is left as originally drafted; read it through this amendment.

## Goal

Three things at once, because the v8_class rewrite is the moment to get the "forever" shape right:

1. **Migrate `env.kv`** from hand-written V8 callbacks to a `Kv` `#[v8_class]` (consistency with `env.db`; typed errors; delete the `thread_local! KV_BACKEND`).
2. **Expand the native surface** to the atomic primitives that are unsafe/impossible to compose in JS: TTL-on-`incr`, `setIfAbsent`, `expire`/`ttl`/`persist`, paginated `list`.
3. **Add a persistent embedded backend** (`redb`) for the single-process / self-host tier, and reconcile InMemory↔Redis semantics.

## Guiding principle (bake into docs)

> **`env.kv` is for ephemeral, hot-path, expiring data. `env.db` is for anything that is a source of truth.**

Most KV scenarios are *implementable* on `env.db` (also strongly consistent). KV earns its place only for: (a) not polluting the relational store with high-frequency ephemeral churn (rate-limit counters, page views, cache fills), (b) TTL as a first-class feature, (c) offloading the source-of-truth DB. Durable idempotency/locks/config/tokens belong in `env.db`. `setIfAbsent` is documented for *ephemeral* locks (auto-release via TTL), not durable "process-once" (use a DB unique index).

---

## 1. Native surface (`env.kv.*`) — final shape

All async (return Promises). Errors are typed `KvError` carrying a `code` (mirrors `DbError`). Validation failures throw **synchronously** (before any dispatch/alloc).

| Method | Signature | Resolves | Notes |
|---|---|---|---|
| `get` | `get(key)` | `string \| null` | |
| `set` | `set(key, value, {ttlMs?})` | `{ok:true}` | value JSON-encoded by SDK |
| `delete` | `delete(key)` | `{deleted:boolean}` | |
| `incr` | `incr(key, {by?=1, ttlMs?})` | `number` (BigInt if \|n\|>2^53) | atomic; `ttlMs` sets expiry **only when the key is created** (fixed-window rate limit) |
| `setIfAbsent` | `setIfAbsent(key, value, {ttlMs?})` | `{stored:boolean}` | atomic compare-on-absence; ephemeral locks/idempotency |
| `expire` | `expire(key, ttlMs)` | `{updated:boolean}` | false if key missing |
| `ttl` | `ttl(key)` | `{ttlMs:number\|null}` resolves `null` if key missing; `{ttlMs:null}` if key exists with no expiry | |
| `persist` | `persist(key)` | `{updated:boolean}` | removes TTL; false if no TTL/missing |
| `list` | `list(prefix?, {cursor?, limit?})` | `{keys:string[], cursor:string\|null}` | cursor opaque + backend-specific; `cursor:null` = end; literal prefix (no glob) |

**Deferred** (not in this work): `compareAndSwap`, `getAndDelete`/`getAndSet`, batch `mget`/`mset`.
**Out of scope** (different primitive / never): rich structures (sets/lists/hashes/sorted-sets), pub/sub, cross-key transactions.

### Shape rules (AI-friendly, per api-design-guidelines)
English method names (`setIfAbsent` not `setNx`); options objects not positional booleans; explicit return shapes (`{stored}`, `{updated}`, `{deleted}`) not bare bools.

---

## 2. `Backend` trait (Rust) — final shape

Replace `Result<_, String>` with `Result<_, KvError>` throughout. New/changed methods:

```rust
async fn get(&self, app_id, key) -> Result<Option<String>, KvError>;
async fn set(&self, app_id, key, value, ttl_ms: Option<u64>) -> Result<(), KvError>;
async fn delete(&self, app_id, key) -> Result<bool, KvError>;
// incr: ttl_ms applies only when the key is created this call
async fn incr(&self, app_id, key, delta: i64, ttl_ms: Option<u64>) -> Result<i64, KvError>;
async fn set_if_absent(&self, app_id, key, value, ttl_ms: Option<u64>) -> Result<bool, KvError>;
async fn expire(&self, app_id, key, ttl_ms: u64) -> Result<bool, KvError>;
async fn ttl(&self, app_id, key) -> Result<TtlState, KvError>;   // Missing | NoExpiry | ExpiresInMs(u64)
async fn persist(&self, app_id, key) -> Result<bool, KvError>;
// paginated: returns (keys, next_cursor); next_cursor None = end
async fn list(&self, app_id, prefix, cursor: Option<&str>, limit: usize) -> Result<(Vec<String>, Option<String>), KvError>;
```

`scope(app_id, key)` helper reused verbatim (`{app_id}:key` hash-tag) by all backends.

### Canonical `incr` contract (all backends conform)
- overflow → `Err(KvError::Overflow)` (not saturate)
- existing value non-numeric → `Err(KvError::NonNumeric)`
- **preserve existing TTL** on increment (only set TTL when key created, per `ttl_ms`)

### `KvError` enum → `OpError` (codes to JS)
`InvalidKey` / `InvalidValue` / `InvalidArgument` (sync TypeErrors, thrown pre-dispatch), `NonNumeric` (`kv_non_numeric`), `Overflow` (`kv_overflow`), `ListTooLarge` (`kv_list_too_large` — over page cap), `Connection` (`kv_connection`), `Backend` (`kv_backend`).

---

## 3. v8_class binding (`Kv`)

Per the confirmed macro mechanics (Shape A — sync `#[v8_method] -> v8::Local<Value>` + a `dispatch_*` helper, because `incr` returns `i64` which the macro's async-return allowlist can't express):

- **`crates/plugin-kv/src/v8_class.rs`** (new): `Kv { backend: Arc<dyn Backend>, app_id: String }`; `#[v8_class] impl Kv` with illegal-`#[v8_constructor]` + the 9 `#[v8_method]`s; `mint_kv(scope, backend, app_id)` (mirrors `mint_db`).
- **`crates/plugin-kv/src/dispatch.rs`** (new): `spawn_kv_op` (promise/resolver/op-id boilerplate) + per-op resolve mappers. Replaces `callbacks.rs` (deleted).
- **`crates/plugin-kv/src/error.rs`** (new): `KvError`.
- **`crates/plugin-kv/src/limits.rs`** (new): constants + `validate_key`/`validate_value`/`validate_delta` + `#[cfg(test)]` unit tests.
- **`lib.rs`**: drop `KV_BACKEND` thread-local + `KvPlugin::new()` (back-compat); `build_instance` → `mint_kv`; `register` → no-op.

### Arg marshalling notes
- options objects (`{ttlMs}`, `{by,ttlMs}`, `{cursor,limit}`) read via `v8_value_to_serde_json`/`read_json_arg` then field-extracted + validated in-body (macro has no `u64`/options extractor).
- `value` arg uses `v8::Local<v8::Value>` (not `String`) to preserve the "null/undefined → throw, but empty-string allowed" distinction.
- `key` validated in-body (`is_empty`, braces, control chars, length) — the macro's `String` catchall does not throw on missing/empty.

### Limits/defaults
`MAX_KEY_LEN` 512 B (reject `{`,`}`,`\0`,control,empty); `MAX_VALUE_BYTES` 256 KiB; `list` default page `limit` 1000, max 10 000 (over-max → clamp to max, NOT error; `ListTooLarge` reserved for backend-side runaway); `incr` resolve number ≤2^53 else BigInt; TTL/`by` taken as f64, range-checked to u64/i64.

---

## 4. Backend impls

### InMemory (`memory.rs`) — dev/test (`#[cfg(test)]` + ephemeral default)
All ops under the existing `Mutex`. incr: `checked_add`→Overflow, parse→NonNumeric, preserve `expires_at`. `set_if_absent`: check-then-insert. `expire`/`persist`/`ttl`: mutate/read `expires_at`. `list`: sorted keys, slice from `cursor` (last key), take `limit`, return next cursor. Add full `#[cfg(test)]` suite (currently none).

### redb (`redb.rs`) — NEW, single-process persistent (dev + self-host); `feature = "redb"`
Per the redb 4.1 spike. `RedbBackend { db: Arc<Database> }`; table `KV: TableDefinition<&str, (&str, Option<u64>)>` (value = (payload, expires_at_ms) — built-in tuple Value, no custom impl). incr = single write-txn read-modify-write (serializable). `set_if_absent` = write-txn check+insert. `list` = `range(prefix..)` take-while `starts_with`, cursor = last key, `range((cursor, Excluded)..)`. Lazy expiry on read + optional sweep. `Durability::Immediate` (default) → survives restart. **Constraint**: exclusive file lock → single-process only (multi-worker-process stays on Redis). Blocking I/O: call redb directly in the async fn first (matches InMemory's blocking-under-lock); documented hook for `compio::dispatcher::Dispatcher` offload if fsync-stall is later measured.

### Redis/Dragonfly (`redis.rs`) — distributed prod; `feature = "redis"`
Map to compio-redis: `get`/`set`(ttl)/`del`; `set_if_absent`→`set_nx(.., ttl_ms)` ✓; `expire`→`pexpire` ✓; `ttl`→`pttl` (map -2→Missing, -1→NoExpiry) ✓; `list`→`scan` cursor passthrough ✓ + **escape glob metachars** in prefix (`*?[]\^`); classify INCRBY server errors → NonNumeric/Overflow.

**Driver prerequisites (compio-redis additions — small):**
- `persist(key)` (PERSIST) — not present today.
- `eval(script, keys, args)` (EVAL) — not present; needed for **atomic incr-with-TTL-on-create**. Lua: `local e=redis.call('EXISTS',KEYS[1]); local v=redis.call('INCRBY',KEYS[1],ARGV[1]); if e==0 and ARGV[2]~='' then redis.call('PEXPIRE',KEYS[1],ARGV[2]) end; return v`. Routes fine under hash-tag scoping (single slot per app). **Decision: add minimal `eval` rather than a 2-command non-atomic approximation** (atomic-TTL-incr is the feature's headline justification; the 2-command race can leave a counter without TTL on crash).

---

## 5. SDK (`@zeroship/kv`) + docs

- Update `sdks/kv/src/index.ts` to expose the 9 ops with typed shapes; keep JSON value handling, `get<T>`. Add SDK-only conveniences: `has(key)`, `getOrSet(key, {ttlMs}, factory)`, `kv.namespace(prefix)`.
- Write `docs/reference/kv.md` (currently missing): the surface, the ephemeral-vs-source-of-truth principle, `setIfAbsent`-for-ephemeral-locks framing, pagination cursor contract, error codes.

---

## 6. Commit staging (DO NOT push)

1. **`KvError` + Backend trait redesign + InMemory** — final trait shape & canonical semantics; InMemory conforms + gets its test suite. (Backend-core, no V8.)
2. **`Kv` v8_class binding + validation/limits + dispatch** — delete `callbacks.rs`; wire `build_instance`; drop thread-local + `KvPlugin::new()`. (env.kv now exposes final surface over InMemory.)
3. **Redis backend** to new trait — incl. compio-redis `persist`+`eval` additions, glob escaping, error classification, cursor `scan`.
4. **redb backend** (`feature="redb"`) + selection wiring (`InMemory` ephemeral → redb path → Redis url).
5. **SDK + `kv.md`** — new surface, conveniences, guidance doc.

Tests land with each commit; the `tests/redis_backend.rs` silent-no-op gate is fixed in commit 3 (`KV_REQUIRE_REDIS=1` → panic if URL unset).

## Open decisions (defaults chosen; veto-able)
- incr+ttl atomicity on Redis → **add `eval` to compio-redis** (vs 2-command approximation).
- list page: default 1000 / max 10 000, over-max clamps.
- `ttl()` missing vs no-expiry → distinct (`null` resolve vs `{ttlMs:null}`).
- All limits/contract values in §3.
