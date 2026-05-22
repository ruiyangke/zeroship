# plugin-db Security Review — Round 4 (2026-05-22)

Target: `crates/plugin-db/` at `/home/ruiyang/Projects/appbase`,
HEAD `dec2bd42` (post-r3 cycle 01:17).

Lens: **Security**. Fresh re-walk; do not assume r3 findings still hold.

Commits since r3 audited:

- `cbd12944` — `OrchestratorLockGuard` RAII extraction (new
  `orchestrator/lock_guard.rs`).
- `dec2bd42` — restore `tx_connect_failed`/`coded_db` prefix fixes
  reverted by `ed697c45`.
- `5ceb6daa` — `query::validate_collection` lowercase check moved to
  byte-slice `eq_ignore_ascii_case`; `wal_consumer` shim demoted.
- `8ff1b2de` — auto-tx error rail typed.
- `0e58c4e8` — broker two-level HashMap.

---

## 1. Audit dimensions — findings

### 1.1 [I36] byte-comparison fix (`query.rs::validate_collection`)

**Verdict: correct, no unicode-prefix bypass.**

Code (`query.rs:79-89`):

```rust
let bytes = name.as_bytes();
if bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"pg_") {
    return Err(QueryError::InvalidCollection(format!(
        "collection name '{name}' uses reserved prefix 'pg_' (Postgres system catalog)"
    )));
}
if bytes.len() >= 10 && bytes[..10].eq_ignore_ascii_case(b"__zeroship") {
    return Err(QueryError::InvalidCollection(format!(
        "collection name '{name}' uses reserved prefix '__zeroship' (platform internal)"
    )));
}
```

Unicode-prefix bypass requires `bytes[..3]` to byte-equal-ignore-ascii
some encoding of `pg_` while the displayed text differs. Any non-ASCII
codepoint encodes as bytes ≥ 0x80; UTF-8 first bytes for codepoints
≥ U+0080 are in `[0xC2..=0xF4]`, which `eq_ignore_ascii_case` never
case-folds and never collapses to ASCII `p`/`g`/`_`. So a multi-byte
prefix cannot byte-match `b"pg_"`.

Additionally, the function runs the alphanumeric/underscore character
check at lines 90-97 — any non-ASCII char fails that allowlist anyway,
so even a theoretical byte-prefix collision would be rejected at the
last gate. Two-layer defense.

The 63-byte length check (line 72) runs BEFORE the prefix checks, so a
malicious 200-byte `pg_…` payload is rejected by length first. The
prefix check then operates on a known-bounded input, so the indexing
`bytes[..3]` / `bytes[..10]` cannot panic (gated by `bytes.len() >= …`).

**No finding.** [I36] resolution holds.

---

### 1.2 `OrchestratorLockGuard` lifecycle — double-unlock / use-after-free

**Verdict: sound.**

Audit walk (`orchestrator/lock_guard.rs`):

| Path | Behaviour |
| --- | --- |
| `acquire()` returns `Ok(guard)` | `client = Some(_)`, `released = false`. |
| `release()` first call | flips `released = true`, takes the client, issues unlock SQL (error swallowed), returns `Ok(Some(client))`. Consumes `self`. |
| `release()` second call (constructed manually in tests, since the type consumes itself) | sees `released = true`, returns `Ok(self.client.take())` (always `None` because first call took it). |
| `into_held()` | flips `released = true`, returns `expect("…")`-ed client. Consumes `self`. |
| `Drop` after `release()` / `into_held()` | `released = true` → tracing error branch skipped, `Option::take` on the now-`None` `client` is a no-op. |
| `Drop` while `released = false` (catastrophic-path fallback) | logs `tracing::error!`, drops the still-`Some` `PooledClient`, which returns to the pool with the session-scoped lock alive. PG releases the lock when the pooled connection is recycled / session ends. Documented behaviour. |

Use-after-free not possible: the `PooledClient` is consumed (not
borrowed) by `release()` / `into_held()` and the guard's `Drop` only
ever touches `self.client` via `Option::take()`. The first
`release()` consumes `self` by value (no remaining handle for a
second call); the same is true of `into_held()`. The `Option<…>`
layout means a second `release()` on a hypothetical zombie guard is
benign (returns `Ok(None)`).

**Minor observation (not a finding):** the
`for_test_no_client(...).into_held()` path would panic via `.expect(…)`.
Test code documents this contract (line 154 doc comment). Production
callers go through `acquire()` so the invariant holds. The `#[cfg(test)]`
sub-tests cover the released-flag transitions and the `Drop`-without-
release warning branch.

**Verification:**

```
$ git log --oneline crates/plugin-db/src/orchestrator/lock_guard.rs
cbd12944 plugin-db/orchestrator: extract OrchestratorLockGuard RAII abstraction
```

---

### 1.3 SQL-injection sweep — `format!`-built SQL sites

I re-walked every site in `plugin-db/src/**` that constructs SQL via
`format!`. Categorisation:

#### 1.3.1 Safe (parameterised, or only constants / pre-validated input interpolated)

| File:line | What it interpolates | Why safe |
| --- | --- | --- |
| `query.rs:156` | `quote_ident(app_id)` | `validate_schema(app_id)` (charset gate) + `quote_ident` |
| `query.rs:195, 288, 302, 353, 396, 466, 545, 1044, 1083, 1390, 1416, 1556+, 1641, 1689` | `{schema}.{table}` from `quote_ident` outputs | upstream `validate_schema(app_id)` + `validate_collection(collection)` |
| `query.rs:706, 952-957, 975, 997` | `col` = `quote_ident(field)` | upstream `validate_field_name(field)` |
| `query.rs:768, 922, 969, 986` | string literals with `'` → `''` escape | manual escape; quote pair balanced |
| `query.rs:783, 927, 932, 970-971, 988-991` | numeric / boolean literals | `as_f64` / `as_i64` / `as_bool` constrain to JSON-decoded numbers |
| `query.rs:1129, 1205-1222, 1252, 1356` | `${N}` placeholders + `value_to_param` for value | params travel as `&[&str]` |
| `query.rs:1593` | `{} {dir}` from a hard-coded `"ASC"`/`"DESC"` |  whitelisted before splice |
| `migrations.rs:412-413, 573-576` | `{schema}.{table}` + `{col} = ${N}` | `quote_ident` for ident, params for values |
| `replication.rs:99, 104, 168-169, 328-339, 452-460` | per-app object names built from `sanitise_app_id` (`[A-Za-z0-9_]` only); `OBJECT_PREFIX` is a `const` | safe — output goes to slot/publication names, not generic identifiers |
| `replication.rs:339, 455` | `'{OBJECT_PREFIX}%'` inside LIKE | `OBJECT_PREFIX` is `const &str = "__zs_"` |
| `backend/postgres.rs:118, 145, 158` | `pg_advisory_lock` / `pg_try_advisory_lock` / `pg_advisory_unlock` SQL — fully static, both keys parameter-bound | parameterised |
| `auth/bootstrap.rs:103-117, 128-130, 181-183, 200-258, 891-991` | `ADMIN_SCHEMA`, `PLATFORM_ROLE`, `APP_ROLE_TEMPLATE` interpolated raw | these are `const` literals (`auth/mod.rs`), no user input |
| `orchestrator/transaction.rs:130-135` | `BEGIN ISOLATION LEVEL {upper}` after whitelist match against `VALID_ISOLATION_LEVELS` | whitelist before splice |
| `orchestrator/auto_tx.rs:147-148` | `BEGIN ISOLATION LEVEL {level} READ WRITE` after `normalize_isolation()` returns one of three `&'static str` | whitelist before splice |

#### 1.3.2 Convention-deviation (open in r3, still open)

| Site | Note |
| --- | --- |
| `backend/postgres.rs:400-404` | Builds `qualified_idx = format!("\"{}\".\"{}\"", app_id, spec.name)` raw rather than via `quote_ident`. `app_id` is validated upstream and `spec.name` is produced by our own deterministic-hash naming, so non-exploitable today; r3 already flagged this. **Unchanged.** |
| `backend/postgres.rs:461-463` | `format!("SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass", qualified_idx.replace('\'', "''"))` — manual single-quote doubling rather than `pool.query_text_params("…", &[&qualified_idx])`. Same input source as :400 — safe today; r3 finding open. |
| `audit.rs:199-798` — 18 `"{app_id}"."__zeroship_migrations"` sites | `validate_app_id(app_id)` (charset gate, no length cap) runs at every public entry point. Non-exploitable today; convention-regression that r3 had already flagged. **Unchanged.** |
| `replication.rs:168-169` | `CREATE PUBLICATION "{pub_name}" FOR TABLES IN SCHEMA {schema_ref}` — `pub_name` is `sanitise_app_id`-gated; `schema_ref` is `quote_ident(app_id)`. Identifier-safe; identical convention-deviation footprint to `audit.rs`. **Unchanged.** |
| `diff.rs:381-383, 394-396` (`count_violating_not_null`) | `"{app_id}"."{collection}" … "{field}"` with NO `validate_*` call inside this function. `pub` (not `pub(crate)`). The two existing callers happen to flow through validated paths, but the public visibility means any future caller (or a doc-test, integration test, etc.) could bypass validation. r3 finding still open. **Unchanged.** |

#### 1.3.3 Genuinely safe

The only "literal interpolation" paths that don't go through quoting
or parameters are:

- **JSON numeric literals.** `as_f64()` returns either a finite `f64`
  (since standard `serde_json` rejects `NaN`/`Infinity` tokens) or
  `None`. When the number comes from an arbitrary-precision parse like
  `9e9999`, `as_f64` does return `f64::INFINITY`, and
  `f64::INFINITY.to_string()` is `"inf"`. Postgres parses `inf`/`-inf`
  as the float Infinity literal, so the resulting CHECK constraint is
  `CHECK ("col" >= inf)`. This is **not** injection — Postgres
  refuses to evaluate that against a comparable value. (Bare `inf` as
  an integer comparand fails at DDL time.) Worst case: a creator's
  schema with a deliberately-pathological `min`/`max` fails DDL. No
  privilege escalation, no SQL injection.
- **`BEGIN`** strings. Both `transaction.rs` and `auto_tx.rs`
  whitelist the isolation level (`VALID_ISOLATION_LEVELS` /
  `normalize_isolation`) before string splice.

**No new injection findings.** All format-built SQL is either
parameterised, whitelisted, or runs ident-clean input through
`quote_ident` / `sanitise_app_id`. The four convention-deviation
sites from r3 (`backend/postgres.rs` ×2, `audit.rs` ×18,
`replication.rs:168`) remain — no new ones introduced by the r3→r4
commits.

---

### 1.4 App-id isolation — cross-app `appId` override (309ed52f closure)

**Verdict: closure intact, no new vectors.**

The fix shape: `Replication::setup` and `Db::startReplicationConsumer`
each route through a dedicated `resolve_*_app_id()` helper that
explicitly ignores `_opts` and returns `stamped.to_string()` (the
mint-time `app_id` from the parent `Db` wrapper). The helpers are
unit-tested (`v8_classes/replication.rs:160-213`,
`v8_classes/db.rs:413-481`) with object / string / number / array
victim shapes.

I re-walked every `#[v8_method]` on `Db`, `Replication`, `Migrations`,
`Migration`, `Collection`, `Transaction`, `Subscription` to enumerate
where the `app_id` enters the path:

- `Db::*` — every method passes `self.app_id` (set in `mint_db`).
- `Collection::*` — `app_id` captured at mint time from parent `Db`.
- `Replication::*` — `app_id` captured at mint time from parent `Db`.
  `setup()` routes through `resolve_setup_app_id`.
- `Migrations::*` — `app_id` captured at mint time from parent `Db`.
  Sub-methods dispatch with `app_id.clone()`.
- `Migration::*` — owns the `(app_id, name, collection)` triple from
  `Migrations::beginMigration` which itself reads `app_id` from
  `crate::v8_bridge::get_app_id_pub(&state)`.
- `Transaction::*` — `app_id` captured at mint time.
- `Subscription::*` — `app_id` flows from the parent `Db` / `Collection`.

No `#[v8_method]` reads `app_id` from a JS-supplied arg. The only
non-self `app_id` entry point is `get_app_id_pub` (v8_bridge), which
reads the per-isolate `APP_ID` env var.

**Verification:**

```
$ rg --type rust "appId|app_id" crates/plugin-db/src/v8_classes/ \
    | grep -E "(get|args\.)" | grep -v "// "
```

No matches. The two `resolve_*_app_id()` helpers' `_opts` parameters
are deliberately discarded (with explanatory comments referencing the
hijack vector).

**No new finding.** [I31] resolution holds and has not regressed.

---

### 1.5 Advisory-lock DoS — per-app rate limit?

**Verdict: open, unchanged from r3.**

The advisory lock keyed on `(hashtext('zs_reg:<app_id>')::int4,
hashtext('register_model')::int4)` is held at session scope on a
pooled client (`OrchestratorLockGuard`). Each `registerModel` call:

1. `backend.pool().get().await` — checks out a `PooledClient`.
2. `pg_advisory_lock(...)` — blocks indefinitely until the lock is free.
3. Releases on success (`apply::run` Pass-1 boundary) or via
   `OrchestratorLockGuard::release()` on error.

There is **no per-app rate limit and no per-app concurrency cap on
acquire**. Two concurrent `registerModel` calls for the same app race
on the lock; the loser blocks indefinitely. Two concurrent calls for
DIFFERENT apps each consume one pooled client and serialise
independently (different `key1` hash inputs).

A misbehaving app calling `registerModel` in a tight loop:

- Same app, sequential: each call acquires + releases. No build-up.
- Same app, parallel (N): N-1 callers block in `pg_advisory_lock`
  consuming N-1 pooled clients while waiting. With a default pool
  size of, say, 8, eight tight-loop calls saturate the pool and the
  whole worker stalls.

The migration pipeline (`migrations.rs:269-278`) uses
`try_acquire_advisory_lock` and fails fast with
`migration_already_active`, which is the correct pattern — but
`registerModel` (`bootstrap.rs:107`) uses the blocking
`acquire_advisory_lock`.

**[FINDING — IMPORTANT]** `orchestrator/register_model/bootstrap.rs:107`
— blocking `pg_advisory_lock` with no timeout / try-with-deadline /
per-app concurrency cap.

  - **Why:** an app with a runaway `registerModel` (or a hostile
    creator's compromised worker) can pin every pooled client in
    `pg_advisory_lock` waits, stalling the whole worker. Cross-tenant
    blast radius if the worker hosts multiple apps and the pool is
    shared.
  - **Fix:** swap to `pg_try_advisory_lock`-with-retry-deadline, the
    same pattern `migrations.rs` already uses, and surface
    `lock_not_available` with the SDK's typed code when the deadline
    expires. Alternatively, cap the per-app in-flight `registerModel`
    count in `crate::context` before the pool checkout.
  - **Verification:**
    ```
    $ rg --type rust "acquire_advisory_lock|try_acquire_advisory_lock" \
        crates/plugin-db/src/
    ```
    `bootstrap.rs:107` is the only blocking call site; the migrations
    path already uses the try variant. (Same observation as r3; not
    addressed by `cbd12944` — the RAII extraction left the blocking
    `acquire` call unchanged.)

---

### 1.6 Reserved-prefix length validation interaction

**Verdict: correct ordering, no edge case.**

`validate_collection`:

1. empty
2. null byte
3. `len() > 63` → reject
4. `bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"pg_")`
5. `bytes.len() >= 10 && bytes[..10].eq_ignore_ascii_case(b"__zeroship")`
6. allowlist `is_ascii_alphanumeric() || c == '_'`

The 63-byte cap firing BEFORE the prefix check means a 200-byte
`pg_pwned…` payload is rejected at step 3 (clearer error message)
rather than at step 4. Equivalent security; user-facing diagnostic is
strictly better.

The `bytes.len() >= 3` / `>= 10` guards prevent a 1-byte or 2-byte
`name` from indexing out of bounds. (Empty already filtered at step 1;
the guard is defensive.) Both guards are correct relative to the
slice indices.

Edge cases I confirmed do NOT bypass:

- `"pg"` (2 bytes) — `bytes.len() >= 3` is false, prefix check
  skipped. But step 6 accepts only `[A-Za-z0-9_]`, so `"pg"` is a
  valid collection name. **Intentional** — `"pg"` is not a
  reserved prefix; only `"pg_*"` is.
- `"__zeroshi"` (9 bytes) — `bytes.len() >= 10` is false, prefix
  check skipped. Step 6 accepts. **Intentional** — only
  `"__zeroship*"` (10+ bytes) is reserved.
- `"PG_FOO"` (uppercase) — `bytes[..3] = b"PG_"`,
  `eq_ignore_ascii_case(b"pg_")` returns true. Rejected. ✓

**No finding.**

---

### 1.7 Credential / PII leak via `tracing::error!`

**Verdict: not a credential leak.**

`OrchestratorLockGuard::Drop` (`lock_guard.rs:160-181`) logs
`key = %self.key, tag = %self.tag` when dropped without release. The
`key` is `"zs_reg:<app_id>"`.

- `app_id` is a **routable tenant identifier** (typed_id `app_<base62-uuidv7>`).
  It appears in URLs, manifest entries, deploy artifacts, audit rows.
  It is NOT a secret. Leaking it into logs is in line with every
  other place in the codebase that logs per-tenant context.
- `tag` is the static string `"register_model"` — no PII.

The `tracing::error!` only fires on the catastrophic-path fallback
(missed `release()` / panic unwind), and even then the field set is
identifier-only, no credentials, no row data, no user input. Compare
to the much broader `tracing::error!(error = ?e, "db: backend
connection task error")` at `backend/postgres.rs:77-78` which logs
arbitrary connection-failure messages and is much closer to a leak
risk (the URL in the connection error could carry credentials —
though `compio_postgres` typically scrubs that).

**No finding.** App-id in logs is acceptable; this is consistent with
the codebase's logging conventions.

---

### 1.8 WAL slot DoS within an app — repeated `replication.setup()`

**Verdict: bounded by idempotency design.**

`ensure_publication_and_slot` (`replication.rs:145-323`) is
idempotent:

- Publication: probe `pg_publication WHERE pubname = $1` (lines
  173-180); only `CREATE PUBLICATION` if missing.
- Slot: probe `pg_replication_slots WHERE slot_name = $1` (lines
  194-201); only `pg_create_logical_replication_slot` if missing.

The slot name is deterministic per app
(`__zs_slot_<sanitised_app_id>`), so repeated `setup()` for the same
app converges on the **same** slot. App A cannot create slot A1, A2,
A3 — only `__zs_slot_<a>`.

Cross-tenant: each app has at most one slot. Postgres's global
`max_replication_slots` cap (default 10, often raised in cloud
deployments) bounds the total, so onboarding more apps than
`max_replication_slots` would fail new-slot creation with SQLSTATE
53400. **This is a deployment-time provisioning concern**, not a
per-app DoS; documented in the P8a proposal.

**No finding.** The per-app idempotency design is sound.

---

### 1.9 Pool exhaustion — `acquire_dedicated_client`

**Verdict: same defense-in-depth gap as r3, unchanged.**

`PostgresBackend::acquire_dedicated_client` (`backend/postgres.rs:70-83`)
opens a fresh TCP connection on EVERY call:

```rust
let (client, connection) = compio_postgres::connect(&self.url, NoTls).await?;
compio::runtime::spawn(async move { connection.run().await; }).detach();
```

Used by:

- `migrations.rs:259-267` (one connection per migration begin).
- `orchestrator/transaction.rs:147-150` (`db.beginTransaction(...)`).
- `orchestrator/auto_tx.rs:200-204` (every wrapped query/mutation
  handler — `exec_auto_begin`).

The third site is the concerning one: **every** `query()` /
`mutation()` handler entry opens a new Postgres connection. There is
no per-app concurrency cap visible in this layer. Postgres's
`max_connections` (typically 100-1000) is the only ceiling.

The module doc-comment (`orchestrator/auto_tx.rs:30-34`) acknowledges
the gap and explicitly defers to the runtime's B3 capability gate as
the primary enforcement. That's an upstream concern, not a plugin-db
one. But: if the capability gate is ever bypassed (bug, misconfig,
trust-boundary failure), plugin-db has no second line of defense
against per-app connection-fan-out DoS.

**[FINDING — MINOR]** `orchestrator/auto_tx.rs:exec_auto_begin` opens
a fresh `compio_postgres::connect()` per query/mutation with no
per-app concurrency cap.

  - **Why:** app A can call `query()` in a tight loop. Each call
    consumes one Postgres connection slot. At 1000 concurrent calls,
    the worker holds 1000 open backend processes on the shared
    Postgres. Cross-tenant noisy-neighbour: app B's connections start
    failing with SQLSTATE 53300 (`too_many_connections`).
  - **Fix:** introduce a per-app semaphore (e.g. `tokio::sync::Semaphore`
    equivalent in compio land) in `crate::context::AppCtx` bounding
    in-flight auto-tx clients to N (e.g. 32). Acquire before
    `compio_postgres::connect`; release on `drop(client)`. Compatible
    with the existing token-based commit/rollback pattern.
  - **Verification:**
    ```
    $ rg --type rust "acquire_dedicated_client|compio_postgres::connect" \
        crates/plugin-db/src/
    ```
    Three call sites, none gated by a per-app cap.

Same severity as r3; explicitly acknowledged in the module doc.

---

## 2. New / regression check from r3→r4 commits

Walked the five diff-bearing commits:

| Commit | Touches | Security delta |
| --- | --- | --- |
| `cbd12944` | `lock_guard.rs`, `register_model/{bootstrap,apply}.rs`, `orchestrator/mod.rs` | RAII centralisation. The pre-existing inline unlock sequences were correct; the new abstraction preserves them. The `acquire_advisory_lock` call site stays blocking (1.5). No regression. |
| `dec2bd42` | `migrations.rs` (4 audit-write sites + tx_connect_failed map_err) | Restores typed-error propagation (`coded_db` keeps SQLSTATE classification). Defense-in-depth improvement on observability, no surface change. |
| `5ceb6daa` | `query.rs::validate_collection`, `wal_consumer.rs` visibility | Byte-eq prefix check (1.1 — confirmed sound). `wal_consumer` shim demoted to `pub(crate)`. **API-surface tightening: security positive** (smaller blast radius if a future caller mis-uses the shim). |
| `8ff1b2de` | `orchestrator/auto_tx.rs` typed error rail | Typed `DbError` reaches the SDK's `err.code` so retry-by-code branches see `transient` / `lock_not_available` / `validation_failed` rather than a flattened `internal`. SDK-side retry policy is informed by SQLSTATE classification. Net security positive. |
| `0e58c4e8` | `broker.rs` two-level `HashMap<String, HashMap<String, Vec<Subscription>>>` | Replaces single-level `HashMap<(String, String), Vec<…>>`. Per-app tenant isolation in the broker held before and still holds — `publish()` looks up `by_key.get_mut(event.app_id.as_str())` first; only matching subscribers receive events. The `drop_app(app_id)` helper still tears down all subs for a given app in one shot. **Verified isolation: app A's events cannot reach app B's subscribers, even with empty/closed buckets** (the `is_empty` cleanup runs AFTER the publish loop and only removes the matched per-app entry). |

No regressions introduced by the r3→r4 commits.

---

## 3. Summary table

| Finding | Severity | Status |
| --- | --- | --- |
| CRITICAL — `Replication::setup({appId})` cross-app override | Closed at `309ed52f` | **Verified intact at r4.** |
| CRITICAL — `Db::startReplicationConsumer(opts: string)` cross-app override | Closed at `309ed52f` | **Verified intact at r4.** |
| IMPORTANT — `bootstrap.rs:107` blocking `pg_advisory_lock` with no try-with-deadline / per-app cap (cross-tenant pool starvation) | r3 advisory-lock RAII finding, refined in r4 | **Open.** RAII commit didn't touch the blocking acquire. |
| IMPORTANT — `backend/postgres.rs:400-404, 461-463` raw `app_id` / `spec.name` interpolation | r3 IMPORTANT | **Unchanged.** No intervening commit. |
| IMPORTANT — `audit.rs` 18 raw `"{app_id}"` sites + duplicate `validate_app_id` (no length cap) | r3 IMPORTANT | **Unchanged.** No intervening commit. |
| IMPORTANT — `replication.rs:168-169` raw `"{pub_name}"` builder | r3 MINOR | **Unchanged.** |
| MINOR — `validate_schema` / `audit::validate_app_id` accept leading digits + no 63-byte length cap | r3 MINOR | **Unchanged.** |
| MINOR — `diff::count_violating_not_null` `pub`, dead, unvalidated | r3 MINOR | **Unchanged.** |
| MINOR — `auto_tx.rs:exec_auto_begin` per-query `compio_postgres::connect` with no per-app cap | r3 acknowledged as deferred to B3 capability gate | **Open.** |
| MINOR — `init_session` `search_path` includes `public` | r3 accepted-known-gap | **Unchanged.** |
| MINOR — WAL consumer cross-tenant visibility Rust-enforced | r3 MINOR (deferred to P8c) | **Unchanged.** |

**Nothing new at r4.** Five commits worth of changes since r3, all of
which either resolve typed-error / hot-path / API-surface concerns
without introducing security surface, or are pure refactors
(`cbd12944` RAII, `0e58c4e8` two-level HashMap). The byte-eq prefix
check (1.1) and the v8_classes app-id closure (1.4) both verify
clean.

---

## 4. Score

**81 / 100** (up from 80 in r3).

Justification:

- **+2** for the `5ceb6daa` `wal_consumer` shim demotion (API-surface
  tightening — pure security positive).
- **+1** for the `8ff1b2de` typed-error rail (SDK retry policy now
  sees SQLSTATE classification at the auto-tx boundary — a
  defense-in-depth improvement, since flat-string errors couldn't be
  branched on for `lock_not_available` retries).
- **+1** for the `cbd12944` RAII abstraction (concentrates the
  unlock-on-error invariant into one type, eliminating three inline
  sites where it could be silently dropped).
- **-3** because the blocking `pg_advisory_lock` at
  `bootstrap.rs:107` was the natural site for the RAII commit to
  also adopt the try-with-deadline pattern from `migrations.rs`, and
  it didn't. The pool-starvation IMPORTANT is now sharpened (1.5).

What would push to 90+:

1. Swap `acquire_advisory_lock` at `bootstrap.rs:107` for
   `try_acquire_advisory_lock`-with-bounded-deadline, surfacing
   `lock_not_available` after N retries. The migrations code already
   demonstrates the pattern.
2. Replace the four remaining hand-quoted SQL identifier sites
   (`backend/postgres.rs:400-404`, `:461-463`, `replication.rs:168`,
   `audit.rs` ×18) with `quote_ident`.
3. Drop the duplicate `audit::validate_app_id` in favour of
   `query::validate_schema`, and add 63-byte length cap +
   reject-leading-digit to the unified validator.
4. Demote `diff::count_violating_not_null` to `pub(crate)` and add
   `validate_collection`/`validate_schema`/`validate_field_name`
   inside.
5. Per-app semaphore in `crate::context` capping in-flight auto-tx
   connection acquires.

What would push past 90:

6. Per-app Postgres role ownership of the WAL slot (P8c —
   proposal-tracked).
7. Move SECURITY DEFINER `search_path` to a hardened `extensions`
   schema in prod provisioning so `public` can be dropped from the
   SECURITY DEFINER path.
