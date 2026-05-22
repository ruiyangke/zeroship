# plugin-db Security Review — R2 (2026-05-22)

Reviewed HEAD: `5be3c1a1` (broker + v8_classes re-promoted to `pub`)  
Round 1: `docs/reviews/plugin-db-security-2026-05-22-r1.md` (reviewed `de01b3a0`).  
Reviewer: sub-agent (read-only, no edits made).

---

## 1. Headline

The three commits called out in the brief are mostly clean.

- **`a00c41fd` (replication quote_ident fix)** — correctly localised to `replication.rs`. The other two files in scope (`wal_consumer.rs`, `replication_ops.rs`) do not construct SQL by string interpolation, so they need no parallel change. Quoting is consistent.
- **`5be3c1a1` (re-promote `broker` + `v8_classes` to `pub`)** — these modules now have a wider Rust API surface, but the only added public surface (`broker::subscribe(app_id, collection)`, `broker::publish(event)`, `v8_classes::{collection, db}::mint_*`) is unreachable from JS. JS still goes through the v8_class wrappers that scope `app_id` to the bound instance.
- **`cac3e542` (dispatcher stream/subscription bypass)** — touches `crates/runtime/src/core/init.rs` and `sdks/bootstrap/src/dev-entry.ts`, NOT `plugin-db`. The plugin-db capability gate (`v8_bridge::refuse_if_query_capability`) checks `current_kind()` independently of which JS shim dispatched, so it is unaffected. The bypass DOES skip the dispatcher's `cfg.input.parse(input)` and `__zsEnterKind(kind)` calls for stream/subscription handlers, which is a defense-in-depth concern in `sdks/bootstrap`, not in `plugin-db`. Plugin-db re-validates every collection / app_id / field-name in Rust regardless of what JS validation ran.

Two **CRITICAL** findings remain, both new (not present in round 1):

- `Db::start_replication_consumer` and `Replication::setup` both accept a JS-supplied `appId` override with no authentication, letting app A request platform-level replication resources for app B. R1 missed this because it audited `wal_consumer::run`'s tenant-isolation check (`rel.namespace != self.app_id`) but did not look at the `appId` argument plumbing in the v8_class layer.

Several round-1 findings have been resolved by intervening commits (`d27ea71e`, `b4e533e2`, `2b4aff4b`). Net score moves up modestly because the resolved findings outweigh the newly surfaced critical, but the critical drops the absolute number.

---

## 2. Findings

### CRITICAL — `db.replication.setup({appId})` permits arbitrary app_id override

**File:** `crates/plugin-db/src/v8_classes/replication.rs:42–58`
**Actor:** App A's JS code calling `env.db.replication.setup({ appId: "victim_app" })`.

```rust
#[v8_method]
fn setup<'s>(
    &self,
    scope: &mut v8::PinScope<'s, '_>,
    opts: v8::Local<v8::Value>,
) -> v8::Local<'s, v8::Value> {
    let opts_v = read_json_arg(scope, Some(opts));
    let app_id = opts_v
        .get("appId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| self.app_id.clone());
    replication_setup_dispatch(scope, app_id).into()
}
```

`self.app_id` is the bound app_id minted in `mint_replication(app_id)`. The opts override silently replaces it. There is no `auth_ctx` check, no allowlist, no comparison against `self.app_id`. The dispatcher (`replication_ops::replication_setup_dispatch`) hands the spoofed id directly to `ensure_publication_and_slot`.

**Impact:**

1. **Resource DoS** — App A can call `setup({appId: "b1"})`, `setup({appId: "b2"})`, … repeatedly. Each call:
   - Provisions `__zs_pub_<b_i>` and `__zs_slot_<b_i>`.
   - Logical replication slots are server-global and limited by `max_replication_slots` (default 10, typical prod ≤ 64). Exhausting the pool blocks every other app's WAL setup.
   - Every active slot retains WAL — orphaned slots cause unbounded `pg_wal` growth and eventually disk-full on the Postgres host (control-plane DoS).
2. **WAL stream hijack** — Combined with the `startReplicationConsumer` finding below, app A can run a WAL consumer for app B on its own worker thread. If app B has subscriptions on the same worker thread (LRU-shared isolates), the spoofed consumer's `SuppressGuard::activate(&self.app_id)` toggles emit-suppression for app B — so app B's local-emit path falls silent while the spoofed consumer is the sole event source. App A controls the rate at which app B's WAL events are delivered.
3. **Audit / billing confusion** — replication setup is a measurable infrastructure event; it shows up in slot-watchdog metrics under the victim app's name.

**Fix:** In `Replication::setup`, refuse any `opts.appId` that does not match `self.app_id` (or remove the override entirely and document `setup()` as bound to the current app). If the operator path is genuinely needed, gate behind an `auth_ctx.role == "platform"` check, not a free-form string.

**PoC:**
```javascript
// Inside a malicious app's mutation() handler
await env.db.replication.setup({ appId: "victim_app_id" });
// Slot/publication for victim_app are now created against the platform DB.
```

**Verification:**
```bash
$ grep -n "appId" crates/plugin-db/src/v8_classes/replication.rs
# Lines 44, 52-56: opts.appId override is plain unauthenticated string.
$ grep -n "auth\|capability\|platform_role" crates/plugin-db/src/v8_classes/replication.rs
# No matches — no auth check.
```

### CRITICAL — `db.startReplicationConsumer(app_id_string)` permits cross-app consumer spawn

**File:** `crates/plugin-db/src/v8_classes/db.rs:230–246`
**Actor:** App A's JS code calling `env.db.startReplicationConsumer("victim_app_id")`.

```rust
#[v8_method]
#[v8_name = "startReplicationConsumer"]
fn start_replication_consumer<'s>(
    &self,
    scope: &mut v8::PinScope<'s, '_>,
    opts: v8::Local<v8::Value>,
) -> v8::Local<'s, v8::Value> {
    let app_id_override = if opts.is_null_or_undefined() {
        None
    } else if opts.is_string() {
        Some(opts.to_rust_string_lossy(scope))
    } else {
        None
    };
    let app_id = app_id_override.unwrap_or_else(|| self.app_id.clone());
    start_replication_consumer_dispatch(scope, app_id).into()
}
```

Same shape as the replication.setup finding. The string argument silently overrides `self.app_id`. `start_replication_consumer_dispatch` then:

1. Calls `ensure_publication_and_slot(&pool, &spoofed_app)` (provisions resources for the victim app — same DoS as above).
2. Calls `WalConsumer::new(&spoofed_app, &url)`, then `compio::runtime::spawn(run_supervised(consumer))`.
3. Stamps `running_consumers[spoofed_app] = true` on the per-isolate context (`context::with_mut(|c| c.mark_consumer_running(&app_id))`).

The supervised task runs for the lifetime of the isolate. On its first decode tick, `SuppressGuard::activate(&self.app_id)` (where `self.app_id` is the WalConsumer's spoofed value) inserts the victim app into the thread-local `SUPPRESSED_APPS` set. That means: every legitimate `emit_local(victim_app, …)` call on this worker thread becomes a no-op (`wal_consumer.rs:216`):

```rust
if is_app_suppressed(app_id) || local_emit_suppressed() {
    return;
}
```

So a victim-app isolate sharing this worker thread will fail to deliver events to local subscribers via the fast path; delivery falls solely on the spoofed consumer's WAL stream (which the attacker can stall by leaning on the supervisor's `compio::time::sleep` back-pressure loops, or by triggering exponential backoff via repeated start/stop cycles).

**Impact** (compounds with #1 above):

- **Reactive-query DoS on co-tenanted apps** — App A on a shared worker thread can silence app B's `emit_local` path indefinitely.
- **Slot ownership confusion** — `running_consumers` is keyed by app_id on the per-isolate context. App A's isolate now "owns" a consumer slot for the victim app id on this thread; the victim app's own `startReplicationConsumer()` call short-circuits with `alreadyRunning: true` (replication_ops.rs:184-196):

```rust
let already = crate::context::with(|c| c.is_consumer_running(&app_id));
if already {
    let value = serde_json::json!({
        "alreadyRunning": true,
        "app_id": app_id,
    })
    .to_string();
    return OpResult::JsValue { ... };
}
```

Result: victim app B "believes" its consumer is running, when in fact A's consumer is the only one — and A's consumer is silencing B's local emits.

**Fix:** Refuse the `opts` string argument entirely (use only `self.app_id`); OR require `opts` to deep-match `self.app_id` and reject otherwise; OR gate the override behind a platform-only auth context.

**PoC:**
```javascript
// Malicious app A
await env.db.startReplicationConsumer("victim_app_id");
// On a worker hosting app B isolates, app B's emit_local for victim_app
// is now suppressed.
```

**Verification:**
```bash
$ grep -n "app_id_override\|opts.is_string" crates/plugin-db/src/v8_classes/db.rs
237:        let app_id_override = if opts.is_null_or_undefined() {
239:        } else if opts.is_string() {
244:        let app_id = app_id_override.unwrap_or_else(|| self.app_id.clone());

$ grep -n "running_consumers\|is_consumer_running" crates/plugin-db/src/replication_ops.rs
184:        let already = crate::context::with(|c| c.is_consumer_running(&app_id));
250:        crate::context::with_mut(|c| c.mark_consumer_running(&app_id));
```

### IMPORTANT — `backend/postgres.rs:400–404` interpolates raw `app_id` without `quote_ident`

**File:** `crates/plugin-db/src/backend/postgres.rs:400–404`
**Actor:** Not directly exploitable; defense-in-depth gap that diverges from the post-`a00c41fd` norm.

```rust
let qualified_idx = format!("\"{}\".\"{}\"", app_id, spec.name);
let drop_idx_sql = format!(
    "DROP INDEX CONCURRENTLY IF EXISTS \"{}\".\"{}\"",
    app_id, spec.name
);
```

`app_id` and `spec.name` are wrapped in `"…"` by hand instead of by `query::quote_ident()`. Both inputs are pre-validated upstream:
- `app_id` passes through `validate_schema` (`[A-Za-z0-9_-]`).
- `spec.name` comes from `index_name` / `named_index_name` — content-addressed from already-validated inputs.

So today no `"` can appear inside either. But the post-`a00c41fd` convention is to route every identifier through `quote_ident`. A future loosening of `validate_schema` (e.g. accepting unicode identifiers for Postgres 18+) would silently re-introduce an injection vector here. Equally — the constructed `regclass`-cast at `postgres.rs:461-464`:

```rust
let check_sql = format!(
    "SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass",
    qualified_idx.replace('\'', "''")
);
```

Hand-doubles single quotes instead of using a parameterised `$1::regclass` bind. Again safe today, but inconsistent with the rest of the codebase.

**Fix:** Replace lines 400-404 with:
```rust
let schema_ident = crate::query::quote_ident(app_id);
let name_ident = crate::query::quote_ident(&spec.name);
let qualified_idx = format!("{schema_ident}.{name_ident}");
let drop_idx_sql = format!("DROP INDEX CONCURRENTLY IF EXISTS {qualified_idx}");
```
And replace the `'{regclass}'` literal at line 461-464 with a `$1::regclass` parameter bind.

**Verification:**
```bash
$ grep -n "format!.*\\\\\\\".*app_id\|format!.*\\\\\\\".*spec.name" crates/plugin-db/src/backend/postgres.rs
400:    let qualified_idx = format!("\"{}\".\"{}\"", app_id, spec.name);
401-:    let drop_idx_sql = format!(
402:        "DROP INDEX CONCURRENTLY IF EXISTS \"{}\".\"{}\"",
403:        app_id, spec.name
```

### IMPORTANT — `audit.rs` uses raw `"{app_id}"` interpolation in nine DDL/DML sites

**File:** `crates/plugin-db/src/audit.rs:198, 213, 245, 257, 264, 280, 298, 348, 496, 547, 578, 633, 661, 680, 703, 733, 767, 797`
**Actor:** Same as above — defense-in-depth gap.

Every SQL statement in `audit.rs` does `r#"… "{app_id}"."__zeroship_migrations" …"#` rather than `quote_ident(app_id)`. Each call site invokes `validate_app_id(app_id)?` first (allowlist `[A-Za-z0-9_-]`), so no `"` can appear. But:

1. The `validate_app_id` function is **local to `audit.rs`** (line 823) — a separate copy of `validate_schema`. If the canonical `query::validate_schema` were tightened (e.g. to also forbid leading digits), `audit.rs` would silently still accept the looser shape.
2. The post-`a00c41fd` convention dictates routing identifiers through `quote_ident`. A reviewer auditing the codebase has to remember "audit.rs has its own allowlist that mirrors query.rs's allowlist" — a fragile contract.

**Fix:** Replace `"{app_id}"."__zeroship_migrations"` with `{schema}.{audit}` where `schema = crate::query::quote_ident(app_id)` and `audit = crate::query::quote_ident("__zeroship_migrations")`. Drop the local `validate_app_id` and use `query::validate_schema` (or its eventual successor) directly. Eliminates the divergent-allowlist failure mode.

**Verification:**
```bash
$ grep -cn "\"{app_id}\"" crates/plugin-db/src/audit.rs
18    # 18 raw interpolation sites
$ grep -n "fn validate_app_id" crates/plugin-db/src/audit.rs
823:fn validate_app_id(name: &str) -> Result<(), DbError> {
```

### MINOR — Advisory-lock DoS via stalled migration client (carried over from R1)

**File:** `crates/plugin-db/src/migrations.rs:230–319`
**Actor:** Same as R1's IMPORTANT finding.

Status: **partially resolved**. Commit `b4e533e2` ("orchestrator/register_model: release advisory lock on plan/validate error") closed the `register_model`'s Pass-1 path. Commit `2b4aff4b` ("explicit ROLLBACK/unlock-all on test-only client teardown") closed the test-harness leak.

The migration lock client (`migrations.rs::exec_begin → mig_lock.client`) still lacks an RAII guard:

```rust
crate::context::with_mut(|c| {
    let _previous = c.set_mig_lock(MigrationLock {
        name: name.to_string(),
        collection: collection.to_string(),
        audit_id,
        dry_run,
        start_generation,
        client: Some(client),
    });
    ...
});
```

If `exec_commit_batch` panics or the compio task is cancelled while `mig_lock.client` is owned by the slot, the client is dropped without an explicit `pg_advisory_unlock` round-trip — the backend session lives until the connection task is reaped. R1's finding still applies; severity stays MINOR because the bounded-window argument from R1 is unchanged.

**Verification:**
```bash
$ grep -n "release_active_lock\|clear_mig_lock\|take_mig_client" crates/plugin-db/src/migrations.rs
171:    crate::context::with_mut(|c| c.take_mig_client())
605:        crate::context::with_mut(|c| c.clear_mig_lock());
713:pub(crate) fn release_active_lock() {
714:    crate::context::with_mut(|c| c.clear_mig_lock());
# `release_active_lock` is only called from worker shutdown, not from
# a Drop on `MigrationLock`. No RAII guard yet.
```

### MINOR — WAL consumer cross-tenant visibility is Rust-enforced, not SQL-enforced (carried over from R1)

**File:** `crates/plugin-db/src/wal_consumer.rs:526–536`

```rust
let Some(rel) = relations.get(&rel_id) else {
    return;
};
if rel.namespace != self.app_id {
    return;
}
```

Unchanged from R1. The slot's owning role is still the platform's superuser-equivalent, so a Relation message for any schema can in principle reach this match. R1's deferred-to-P8c track applies.

Compounds with the CRITICAL `startReplicationConsumer({appId})` finding above: if app A spawns a consumer claiming `self.app_id = "app_B"`, the namespace check passes for `app_B`'s relations (since `self.app_id == "app_B" == rel.namespace`), and events are published into the broker under `app_B` — visible to any subscriber on `app_B` on this thread. The Rust enforcement only stops the *consumer-mismatched* case, not the consumer-spoofed case.

### MINOR — `validate_app_id`/`validate_schema` allow leading digits

**File:** `crates/plugin-db/src/audit.rs:830–838`, `crates/plugin-db/src/query.rs:131–138`

Both validators accept names like `"1users"` or `"-foo"`. Postgres rejects unquoted identifiers starting with a digit, but our SQL always quotes — so `"1users"` is valid Postgres. The risk is operational confusion (a name like `"42"` shadowing internal numerics in introspection queries) rather than security. Round 1 didn't surface this; flagging for completeness now that R1's reserved-prefix gap has been closed.

### MINOR — `init_session` `search_path` includes `public` (carried over from R1)

**File:** `crates/plugin-db/src/auth/bootstrap.rs:357, 425, 505, 604, 635`

Same as R1. Several SECURITY DEFINER bodies still set `search_path = pg_catalog, public, pg_temp`. The `pgcrypto` placement comment is unchanged; the deferred-to-prod-hardening posture is unchanged.

### MINOR — `count_violating_not_null` builds DDL via `format!` without validation

**File:** `crates/plugin-db/src/diff.rs:375–402`

```rust
pub async fn count_violating_not_null(
    pool: &Pool,
    app_id: &str,
    collection: &str,
    field: &str,
) -> Result<(i64, Vec<i64>), String> {
    let sql = format!(
        r#"SELECT id FROM "{app_id}"."{collection}" WHERE "{field}" IS NULL LIMIT 5"#
    );
    ...
}
```

`app_id`, `collection`, `field` are all raw-interpolated without `validate_*` at the function boundary. The function is `pub` (not `pub(crate)`) so a downstream caller that forgets to validate could be exploited. It's currently dead — no in-crate caller; the validation path uses `estimate_row_count` (which is parameterised) instead. If kept dead, downgrade to `pub(crate)` and add a `validate_*` defence-in-depth pass. If removed, no concern.

**Verification:**
```bash
$ grep -rn "count_violating_not_null" crates/plugin-db
crates/plugin-db/src/diff.rs:375:pub async fn count_violating_not_null(
# No other call sites — function is dead code.
```

### Confirmed safe (re-verified)

- **`init_pool_async` credential leak** — `compio_postgres::Error`'s `Display` impl at `crates/compio-postgres/src/error/mod.rs:381-406` emits only the kind label (`"error connecting to server"`, `"invalid connection string"`, …). Source-chain walking in `lib.rs:309-319` includes nested errors but Postgres connect errors don't echo the URL. Confirmed safe.
- **`exec_auto_begin` connect-error** at `orchestrator/auto_tx.rs:174-178` — same `Display` impl, no credential leak.
- **`broker::subscribe`/`broker::publish` promotion to `pub`** — the only callers outside `plugin-db` are `tests/integration.rs`, `tests/subscription_finalizer.rs`, `tests/db_v8_class.rs` (all under `#[cfg(test)]` test crates that are not shipped). JS code reaches these only via `v8_classes::subscription::mint_subscription` which scopes `app_id` to the bound instance's value. No cross-app injection from JS.
- **`v8_classes::collection::mint_collection`, `v8_classes::db::mint_db`** — `pub` for test use; JS-reachable only via the v8_class IDL surface (which takes no `app_id` argument from JS). No bypass.
- **Round 1's IMPORTANT #1 (`pg_` / `__zeroship_` / >63-byte collection names)** — resolved by commit `d27ea71e` (in this round's HEAD). `validate_collection` and `validate_field_name` now reject reserved prefixes, oversized names, and null bytes. Tests `validate_collection_rejects_pg_prefix`, `validate_collection_rejects_zeroship_prefix`, `validate_collection_rejects_name_exceeding_63_bytes`, `validate_collection_rejects_null_byte`, `validate_field_name_rejects_name_exceeding_63_bytes`, `validate_field_name_rejects_null_byte` lock the behaviour.
- **`cac3e542` dispatcher stream/subscription bypass** — does NOT skip plugin-db's `refuse_if_query_capability` gate (that gate keys off `current_kind()` set by `__zsEnterKind`). For `stream`/`subscription` kinds the dispatcher's `__zsEnterKind` is not called, so `current_kind()` remains whatever the outer caller installed. Stream/subscription handlers are not subject to query-vs-mutation gating in the platform's design (the proposal explicitly excludes them from auto-tx wrapping). Plugin-db's defensive Rust-side validators run unchanged.

---

## 3. Round-1 → Round-2 delta

| R1 finding | R2 status |
| --- | --- |
| IMPORTANT — `pg_` / `__zeroship_` namespace collision in `validate_collection` | **Resolved** (commit `d27ea71e`). |
| IMPORTANT — Advisory-lock DoS via stalled migration client | **Partially resolved** (commits `b4e533e2`, `2b4aff4b` closed two of three paths). Remaining: `exec_begin → mig_lock.client` still lacks an RAII unlock-on-drop guard. Downgraded to MINOR. |
| IMPORTANT — WAL replication credentials scope | **Unchanged** — deferred to P8c per proposal. Downgraded to MINOR. |
| MINOR — `validate_collection` not called for field names in DDL | **Resolved** (commit `d27ea71e` added `validate_field_name` length/null-byte check). |
| MINOR — Credential not logged | **Re-confirmed safe.** |
| MINOR — `OBJECT_PREFIX` literal in `format!` strings | **Unchanged** — still safe-as-const, still uses LIKE-prefix in `replication.rs:326, 442`. |
| MINOR — `init_session` `search_path` includes `public` | **Unchanged.** |
| — | **NEW: CRITICAL — `Replication::setup` accepts unauthenticated `opts.appId` override.** |
| — | **NEW: CRITICAL — `Db::start_replication_consumer` accepts unauthenticated `opts` string app_id override.** |
| — | **NEW: IMPORTANT — `backend/postgres.rs:400-404` interpolates raw `app_id` without `quote_ident`.** |
| — | **NEW: IMPORTANT — `audit.rs` has 18 raw-interpolation sites under a duplicated `validate_app_id` allowlist.** |
| — | **NEW: MINOR — `validate_app_id`/`validate_schema` accept leading digits.** |
| — | **NEW: MINOR — `diff::count_violating_not_null` is `pub`, dead, and unvalidated.** |

---

## 4. Score

**68 / 100** (down from 76).

The CRUD + DDL hot path remains structurally sound — parameter binding everywhere user values land, identifier quoting (now with R1's reserved-prefix and length checks) everywhere structural SQL fragments form, schema-membership scoping enforced via `validate_schema` + `quote_ident`. The post-`a00c41fd` cutover is correct in its scope.

The score drops because two newly-surfaced CRITICAL findings (`Replication::setup({appId})` and `startReplicationConsumer("app_id_string")` accepting unauthenticated overrides) are exploitable from any tenant's JS code with two- to four-line PoCs. These are not theoretical: they bypass tenant isolation for a platform-level resource (replication slots, WAL stream ownership, broker emit-suppression) and have both DoS and confused-deputy variants. Until those two methods either drop the override or gate it behind a platform-only auth role, the plugin-db boundary cannot be called multi-tenant-safe.

The IMPORTANT findings (backend/postgres.rs raw interpolation, audit.rs duplicated allowlist) are not directly exploitable today but represent quoting convention regressions that future code reviewers must keep mentally tracking — exactly the failure mode the `a00c41fd` fix was meant to eliminate codebase-wide.
