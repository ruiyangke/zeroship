# plugin-db Security Review — R3 (2026-05-22)

Reviewed HEAD: `4d80651e` (latest on `main`).
Prior rounds:
- R1: `docs/reviews/plugin-db-security-2026-05-22-r1.md` (commit `de01b3a0`)
- R2: `docs/reviews/plugin-db-security-2026-05-22-r2.md` (commit `5be3c1a1`)

Reviewer: sub-agent (read-only, no edits made).

---

## 1. Headline

The two R2 CRITICAL findings (cross-app `appId` override on
`Replication::setup` and `Db::startReplicationConsumer`) are **closed**
by commit `309ed52f`. The fix follows the strongest hardening shape:
the security-critical app-id resolution is lifted out of the
`#[v8_method]` body into a separate `resolve_*_app_id` helper that
deliberately ignores `opts` entirely, with regression-guard unit tests
that exercise the legacy exploit shapes (string override, object
override, undefined opts). Verified — no `opts.appId` / opts-derived
app_id paths remain on any v8_class.

Across the rest of plugin-db, the R2 IMPORTANTs (`backend/postgres.rs`
raw interpolation, `audit.rs` duplicated allowlist) and R2 MINORs
(`count_violating_not_null` dead-pub-unvalidated, validators accept
leading digits, no length cap on `app_id`) **all carry over** — no
intervening commits touched any of those sites. The advisory-lock
RAII gap (R2 MINOR) is unchanged.

No new CRITICAL / IMPORTANT findings surfaced this round. One new
MINOR: `replication.rs:167-168` reproduces the post-`a00c41fd` raw
interpolation convention deviation in a fresh site (the
`pub_name`-quoted publication-create SQL) — same not-currently-exploitable,
same convention-regression severity as the existing
`backend/postgres.rs:400-404` deviation.

Net score moves up to **80 / 100** from R2's 68: the two CRITICALs
are closed and the remaining findings are all defense-in-depth
hardening rather than exploitable bypasses.

---

## 2. Findings

### Re-verified — appId override fix (R2 CRITICAL #1 & #2) — CLOSED

**Files:**
- `crates/plugin-db/src/v8_classes/replication.rs:54-68` (setup)
- `crates/plugin-db/src/v8_classes/db.rs:241-250` (startReplicationConsumer)

The R2 brief asks: confirm `self.app_id` is now the only scope; no
JS-supplied override path. **Confirmed.**

`Replication::setup` (lines 54-68):
```rust
fn setup<'s>(...) -> v8::Local<'s, v8::Value> {
    let opts_v = read_json_arg(scope, Some(opts));
    let app_id = resolve_setup_app_id(&self.app_id, &opts_v);
    replication_setup_dispatch(scope, app_id).into()
}
```

Routes through `resolve_setup_app_id` (replication.rs:106-112), which
takes `_opts: &Value` with a leading underscore signalling the
intentional unused-ness, and a load-bearing `INVARIANT` comment:
```rust
#[inline]
fn resolve_setup_app_id(stamped: &str, _opts: &Value) -> String {
    // INVARIANT: never read app-id-shaped fields from `_opts`.
    stamped.to_string()
}
```

`Db::start_replication_consumer` (lines 241-250) has the symmetric
shape, routing through `resolve_consumer_app_id` (db.rs:322-333).
Both helpers have unit-test guards for the legacy exploit shapes:
- `setup_app_id_ignores_string_override` (replication.rs:170-177)
- `setup_app_id_ignores_non_string_override` (lines 179-197) — covers
  numbers, booleans, nulls, arrays, nested objects.
- `setup_app_id_empty_opts_uses_stamped` (lines 199-204)
- `setup_app_id_preserves_unicode_stamped_id` (lines 206-212)
- `consumer_app_id_ignores_string_override` (db.rs:432-445) — V8-side
  reproduction of the legacy `opts: string` exploit shape.
- `consumer_app_id_ignores_object_override` (db.rs:447-463) —
  `{appId: "victim_app"}` shape.
- `consumer_app_id_undefined_opts_uses_stamped` (db.rs:465-480)

Cross-check across all v8_classes (`collection.rs`, `transaction.rs`,
`migration.rs`, `migrations.rs`, `subscription.rs`, `db.rs`,
`replication.rs`): no v8_method or constructor reads any
`appId`/`app_id`-shaped field from a JS-supplied argument. Every
class:
- `Db::app_id` — stamped from `build_instance(scope, app_id)` (the
  plugin's `NativePlugin::build_instance` hook, called from the runtime
  with the live `app_id`).
- `Collection::app_id` — passed in from `mint_collection(name,
  app_id)` (called only from `Db::collection`, which passes
  `self.app_id`).
- `Migrations::app_id` — passed in from `mint_migrations(scope,
  app_id)` (called only from `Db::migrations` getter with `self.app_id`).
- `Replication::app_id` — passed in from `mint_replication(scope,
  app_id)` (called only from `Db::replication` getter with `self.app_id`).
- `Migration` (migration.rs) — stamps `MigrationOwner{app_id, name,
  collection}` from `crate::v8_bridge::get_app_id_pub(&state)` (which
  reads the platform-set `APP_ID` env var, not JS), then passes that
  through `exec_begin` / `exec_status` / `exec_cancel` / `exec_reset` /
  `exec_commit_batch` / `exec_fetch_batch`.
- `Transaction` — receives `app_id: String` from
  `begin_transaction_dispatch(scope, isolation, self.app_id.clone())`
  in `Db::begin_transaction`, no `opts.appId` plumbing.
- `Subscription` — receives `app_id: &str` from
  `mint_subscription(scope, app_id, collection)` in
  `Db::open_subscription` and `Collection::watch`.

**Verification:**
```bash
$ grep -rn 'appId\|app_id' crates/plugin-db/src/v8_classes/ | grep -v test
# Returns only `self.app_id` reads and the doc-comments about the
# closed override path; no JS-derived override remains.
$ grep -n "fn build_instance\|build_instance(" crates/plugin-db/src/lib.rs
169:    fn build_instance<'s>(
174:        v8_classes::db::mint_db(scope, app_id)
```

Tests (in-tree at `crates/plugin-db/src/v8_classes/`) pass on the
current HEAD. The closure is robust to future contributors restoring
the override path — they would have to delete both the resolver
helper and its unit tests, which is loud-enough in code review to
catch.

### Re-verified — dispatch helpers `app_id: String` parameter — SAFE within callers

**Files:**
- `crates/plugin-db/src/replication_ops.rs:58-90` (`replication_setup_dispatch`)
- `crates/plugin-db/src/replication_ops.rs:93-124` (`replication_watchdog_dispatch`) — no `app_id` param
- `crates/plugin-db/src/replication_ops.rs:127-161` (`replication_drop_abandoned_dispatch`) — no `app_id` param
- `crates/plugin-db/src/replication_ops.rs:174-273` (`start_replication_consumer_dispatch`)

The R3 brief asks: do these dispatch helpers take `app_id: String`
that any caller can supply? If so, audit ALL callers to confirm they
never pass an untrusted value.

`replication_setup_dispatch(scope, app_id: String)` and
`start_replication_consumer_dispatch(scope, app_id: String)` take a
plain `String`. In **release builds** (default), both functions are
`pub(crate)` via the cfg fork in `lib.rs:93-96`:
```rust
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod replication_ops;
#[cfg(feature = "test-helpers")]
pub mod replication_ops;
```

So the only release-build callers are inside `crates/plugin-db/src/`.
Exhaustive call-site audit:
- `replication_setup_dispatch` — called from `Replication::setup`
  (replication.rs:67) with `resolve_setup_app_id(&self.app_id, &opts_v)`,
  which returns `self.app_id` unconditionally.
- `start_replication_consumer_dispatch` — called from
  `Db::start_replication_consumer` (db.rs:249) with
  `resolve_consumer_app_id(&self.app_id, scope, opts)`, which returns
  `self.app_id` unconditionally.

With `--feature test-helpers` enabled (only the `tests/integration.rs`
target), these helpers become reachable from external Rust test
crates — but those crates run in the test harness, not in production,
and never bridge to tenant JS. Safe.

**Verification:**
```bash
$ grep -rn "replication_setup_dispatch\|start_replication_consumer_dispatch" \
    crates/plugin-db/src/
# Two definition sites + two call sites — all internal, all pass
# self.app_id.
```

### IMPORTANT (carried from R2) — `backend/postgres.rs:400-404` raw `app_id` / `spec.name` interpolation

**File:** `crates/plugin-db/src/backend/postgres.rs:400-404`, `:461-464`
**Status:** **Unchanged since R2.** No commits between `5be3c1a1` and
`4d80651e` touched this site. Defence-in-depth gap; not currently
exploitable.

```rust
let qualified_idx = format!("\"{}\".\"{}\"", app_id, spec.name);
let drop_idx_sql = format!(
    "DROP INDEX CONCURRENTLY IF EXISTS \"{}\".\"{}\"",
    app_id, spec.name
);
// ...
let check_sql = format!(
    "SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass",
    qualified_idx.replace('\'', "''")
);
```

`app_id` and `spec.name` are pre-validated upstream
(`validate_schema` for `app_id`, content-addressed `index_name` /
`named_index_name` for `spec.name`), so `"` can never appear in either
today. Future loosening of `validate_schema` (e.g. for unicode
identifiers) would silently re-introduce an injection vector.

The post-`a00c41fd` convention is to route every identifier through
`quote_ident`. Keeping this divergent site means a reviewer auditing
the codebase has to remember "validate_schema is an allowlist, not a
quoter" — exactly the cognitive load that `a00c41fd` was meant to
eliminate.

**Fix:** Replace lines 400-404 with `quote_ident(app_id)` /
`quote_ident(&spec.name)`, and replace the `'{regclass}'` literal at
line 462-463 with a `$1::regclass` parameter bind.

**Verification:**
```bash
$ grep -n 'format!.*\\"' crates/plugin-db/src/backend/postgres.rs
400:    let qualified_idx = format!("\"{}\".\"{}\"", app_id, spec.name);
401:    let drop_idx_sql = format!(
462:                    "SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass",
```

### IMPORTANT (carried from R2) — `audit.rs` raw `"{app_id}"` interpolation with duplicate `validate_app_id`

**File:** `crates/plugin-db/src/audit.rs:198, 246, 257, 264, 280, 298, 348, 496, 547, 578, 633, 661, 680, 703, 733, 767, 797`
**Status:** **Unchanged since R2.** 18 raw-interpolation sites still use
the local `validate_app_id` (line 823-840), which mirrors but does not
re-use `query::validate_schema`.

```rust
// audit.rs:198-199 (representative):
let create_sql = format!(
    r#"CREATE TABLE IF NOT EXISTS "{app_id}"."__zeroship_migrations" (...)"#
);
```

Allowlist `[A-Za-z0-9_-]` matches `validate_schema` today. If the
canonical validator is tightened (e.g. to forbid leading digits, see
the MINOR below), `audit.rs` silently still accepts the looser shape.

**Fix:** Replace `"{app_id}"."__zeroship_migrations"` with
`{schema}.{audit}` where `schema = crate::query::quote_ident(app_id)`
and `audit = crate::query::quote_ident("__zeroship_migrations")`.
Drop the local `validate_app_id` and use `query::validate_schema`
directly. Eliminates the divergent-allowlist failure mode.

**Verification:**
```bash
$ grep -c '"{app_id}"' crates/plugin-db/src/audit.rs
18
$ grep -n 'fn validate_app_id' crates/plugin-db/src/audit.rs
823:fn validate_app_id(name: &str) -> Result<(), DbError> {
```

### MINOR (NEW) — `replication.rs:167-168` builds `pub_name` with raw `"..."` quoting

**File:** `crates/plugin-db/src/replication.rs:167-168`
**Actor:** Not an actor — convention-regression flag.

```rust
let pub_sql = format!(
    r#"CREATE PUBLICATION "{pub_name}" FOR TABLES IN SCHEMA {schema_ref};"#
);
```

`pub_name` comes from `publication_name(app_id)` which routes through
`sanitise_app_id` (rejects anything outside `[A-Za-z0-9_]`,
lowercases). `OBJECT_PREFIX` is a `const &str`. So `"` can never
appear in `pub_name` today — safe.

But this is the same shape of convention deviation R2 flagged for
`backend/postgres.rs:400-404`: hand-rolled `"…"` wrapping instead of
routing through `quote_ident`. Consistency matters because future
reviewers need to be able to read the codebase by pattern-matching
("every quoted identifier is `quote_ident`-emitted" is a stronger
invariant than "every quoted identifier is either `quote_ident`-emitted
or hand-quoted at a site where I've already verified the input
allowlist").

`schema_ref` (the FOR TABLES IN SCHEMA argument) already uses
`crate::query::quote_ident(app_id)` (line 158) — that side is
correct. The deviation is in the `"{pub_name}"` half only.

**Fix:** Replace `r#""{pub_name}""#` with
`crate::query::quote_ident(&pub_name)` in `replication.rs:168`.

**Verification:**
```bash
$ grep -n 'CREATE PUBLICATION' crates/plugin-db/src/replication.rs
168:        r#"CREATE PUBLICATION "{pub_name}" FOR TABLES IN SCHEMA {schema_ref};"#
```

### MINOR (carried from R2) — `validate_schema` / audit `validate_app_id` accept leading digits AND have no length cap

**File:**
- `crates/plugin-db/src/query.rs:125-140` (`validate_schema`)
- `crates/plugin-db/src/audit.rs:823-840` (audit-local `validate_app_id`)

**Status:** **Unchanged since R2.** R2 surfaced the leading-digit gap;
this round adds the length-cap gap (NAMEDATALEN).

Both validators accept:
- Leading digits (`"1users"`, `"42"`).
- Names of arbitrary length. Postgres' `NAMEDATALEN` is 63 bytes; any
  `app_id` longer than 63 bytes gets silently truncated at the
  storage layer when used as a schema identifier. Two long-but-similar
  `app_id` strings can alias the same schema. The unsafe combination
  is `app_id` chosen by the creator (today: server-minted UUIDv7-base62
  via `typed_id`, ~24 chars, so the gap is theoretical) — if future
  CLI/control-plane code accepts a creator-friendly `app_id`, the gap
  becomes load-bearing.

`query::validate_collection` (lines 61-97) **does** cap at 63 bytes
and reject reserved prefixes — that pass landed in r1's
`d27ea71e`. The asymmetry between collection and schema is the
problem: anything called "validate the identifier" should give the
same safety guarantees.

**Fix:** Tighten both validators in one step:
```rust
fn validate_schema(name: &str) -> Result<(), QueryError> {
    if name.is_empty() {
        return Err(QueryError::InvalidCollection(
            "schema name cannot be empty".to_string(),
        ));
    }
    if name.contains('\0') {
        return Err(QueryError::InvalidCollection(
            "schema name must not contain null bytes".to_string(),
        ));
    }
    if name.len() > 63 {
        return Err(QueryError::InvalidCollection(format!(
            "schema name exceeds 63-byte Postgres identifier limit: {name}"
        )));
    }
    let first = name.chars().next().unwrap();
    if first.is_ascii_digit() {
        return Err(QueryError::InvalidCollection(format!(
            "schema name must not start with a digit: {name}"
        )));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(QueryError::InvalidCollection(format!(
            "invalid schema name: {name}"
        )));
    }
    Ok(())
}
```
And drop audit's local copy in favour of `query::validate_schema`.

**Verification:**
```bash
$ grep -n "name.len()\|NAMEDATALEN\|63" crates/plugin-db/src/query.rs | head -5
# validate_collection at line 72 caps at 63 bytes.
# validate_schema (lines 125-140) does not.
$ grep -n "name.len()\|NAMEDATALEN\|63" crates/plugin-db/src/audit.rs
# No length check.
```

### MINOR (carried from R2) — `diff::count_violating_not_null` is `pub`, dead, and unvalidated

**File:** `crates/plugin-db/src/diff.rs:375-403`
**Status:** **Unchanged since R2.** Function is still `pub` (not
`pub(crate)`), still has no in-crate caller, still interpolates
`app_id` / `collection` / `field` raw into SQL.

```rust
pub async fn count_violating_not_null(
    pool: &Pool, app_id: &str, collection: &str, field: &str,
) -> Result<(i64, Vec<i64>), String> {
    let sql = format!(
        r#"SELECT id FROM "{app_id}"."{collection}" WHERE "{field}" IS NULL LIMIT 5"#
    );
    ...
    let count_sql = format!(
        r#"SELECT COUNT(*) AS n FROM "{app_id}"."{collection}" WHERE "{field}" IS NULL"#
    );
    ...
}
```

Within this crate it's dead code (the validation pass uses
`estimate_row_count` instead). Risk: a downstream consumer that picks
up plugin-db as a Rust library (today: `crates/control`,
`crates/worker`, `crates/cli` via `zeroship-plugin-db` workspace
dep) could call this and forget to validate the three string args.

**Fix:** Either demote to `pub(crate)` and add
`validate_schema(app_id)` + `validate_collection(collection)` +
`validate_field_name(field)` at the function entry, or delete the
function (no in-tree caller).

**Verification:**
```bash
$ grep -rn "count_violating_not_null" crates/
crates/plugin-db/src/diff.rs:375:pub async fn count_violating_not_null(...
# Only the definition site — no callers.
```

### MINOR (carried from R2) — Advisory-lock client lacks RAII unlock-on-drop

**File:** `crates/plugin-db/src/migrations.rs:328-340`
**Status:** **Unchanged since R2.**

`exec_begin` parks the lock client into the per-isolate
`IsolateDbContext.mig_lock.client` slot. If `exec_commit_batch` panics
or the compio task is cancelled while the slot owns the client, the
client drops without an explicit `pg_advisory_unlock` round-trip.
The Postgres backend session lives until the connection task is
reaped — bounded window, not unbounded.

`release_active_lock` (line 713-715) is called from worker shutdown
and the test-helper `clear_migration_lock_for_tests` (lib.rs:238-248)
but not from a `Drop` impl. The advisory-lock client is dropped
implicitly when `IsolateDbContext` itself drops (isolate teardown).

**Fix:** Wrap `MigrationLock` in a Drop impl that issues
`SELECT pg_advisory_unlock_all()` on the parked client before dropping
it — same shape as `SuppressGuard` in `wal_consumer.rs`.

**Verification:**
```bash
$ grep -n "release_active_lock\|impl Drop for MigrationLock" crates/plugin-db/src/migrations.rs
# `release_active_lock` is called from one place only (worker shutdown).
# No Drop impl.
```

### MINOR (carried from R1, re-confirmed) — WAL consumer cross-tenant visibility is Rust-enforced

**File:** `crates/plugin-db/src/wal_consumer.rs:526-536` (namespace check)
**Status:** **Unchanged since R1/R2.** The slot's owning role is still
the platform's superuser-equivalent. Cross-tenant isolation in the
WAL path lives at the Rust layer (`if rel.namespace != self.app_id
{ return; }`), not at Postgres.

Compounds with neither CRITICAL from R2 (both closed by `309ed52f`) —
without the `appId` override path, an attacker on app A can only
spawn a consumer claiming `self.app_id == "app_a"`, which sees only
`app_a` relations on the namespace check.

Deferred to P8c per the proposal.

### MINOR (carried from R1, re-confirmed) — `init_session` `search_path` includes `public`

**File:** `crates/plugin-db/src/auth/bootstrap.rs:357, 425, 505, 604, 635`
**Status:** **Unchanged.** Accepted-known-gap per R1.

---

## 3. Confirmed safe (re-verified)

- **Cross-app reach via `appId` override on any v8_class** — closed
  by `309ed52f`. The `resolve_*_app_id` helpers + unit tests
  defensively reject all override shapes (string, object, number,
  array, null, undefined). No path remains.
- **WAL slot DoS, within-app** — `ensure_publication_and_slot` is
  idempotent (probe then create with `IF NOT EXISTS` semantics);
  `slot_name(app_id)` is deterministic. So at most 1 slot per app
  on the cluster, regardless of how many times an app calls
  `setup()` / `startReplicationConsumer()`.
- **SQL injection via JSON-path operators (`$json_path`,
  `$json_contains`)** — these operators do not exist in
  `build_field_condition` (query.rs:1897-2022). Only `$eq` / `$ne` /
  `$gt` / `$gte` / `$lt` / `$lte` / `$in` / `$nin` / `$exists` /
  `$like` / `$ilike` / `$search` exist; every value goes through
  `params.push(value_to_param(val))` and lands as a parameterised
  bind. Field names go through `quote_ident(field)`. Closed.
- **Credential leak via error messages** — `compio_postgres::Error`'s
  `Display` impl (`crates/compio-postgres/src/error/mod.rs`) emits
  only the kind label. No `tracing::*!` site in plugin-db
  interpolates `url` / `db_url` / `self.url`. The connection-task
  error logging at `backend/postgres.rs:78`,
  `orchestrator/transaction.rs:154`, `orchestrator/auto_tx.rs:181`
  uses `error = ?e` (Debug on `compio_postgres::Error`, which
  does not carry the URL).
- **Identifier injection in `query.rs` builders** — every DDL/CRUD
  builder calls `validate_collection(collection)` +
  `validate_schema(app_id)` at function entry, then routes the
  identifier through `quote_ident`. Field names go through
  `validate_field_name` (length / null-byte check) plus
  `quote_ident`. No raw interpolation outside the four sites flagged
  above.
- **Reserved-prefix enforcement on collection names** — fires at
  `validate_collection` (query.rs:78-87), which runs as the FIRST
  validation step BEFORE any SQL is generated (every DDL builder
  starts with `validate_collection(collection)?` on line
  190 / 278 / 298 / 341 / 391 / 455 / 532 / etc.). Rejects `pg_*`
  (case-insensitive) and `__zeroship*` (case-insensitive).
- **Advisory-lock DoS via mass `acquire_dedicated_client`** —
  bounded. `exec_begin` checks `has_mig_lock()` before acquiring;
  the per-isolate slot holds at most one migration lock at a time.
  Single tenant cannot open > 1 dedicated client for migration
  purposes per worker thread. Likewise, `db.beginTransaction()` is
  gated by `has_tx()` (one tx-client per isolate). Aggregate
  worst case: 2 dedicated PG connections per app per worker thread
  (1 migration + 1 tx).

---

## 4. R2 → R3 delta

| R2 finding | R3 status |
| --- | --- |
| CRITICAL — `Replication::setup({appId})` cross-app override | **Resolved** (`309ed52f`). Robust regression-guard tests in place. |
| CRITICAL — `Db::startReplicationConsumer(opts: string)` cross-app override | **Resolved** (`309ed52f`). Robust regression-guard tests in place. |
| IMPORTANT — `backend/postgres.rs:400-404` raw `app_id` / `spec.name` interpolation | **Unchanged.** No intervening commit. |
| IMPORTANT — `audit.rs` 18 raw `"{app_id}"` sites + duplicate `validate_app_id` | **Unchanged.** No intervening commit. |
| MINOR — `validate_schema` / `validate_app_id` accept leading digits | **Unchanged.** R3 adds: same validators also lack a 63-byte length cap. |
| MINOR — `diff::count_violating_not_null` pub-dead-unvalidated | **Unchanged.** No intervening commit. |
| MINOR — Advisory-lock RAII gap (migrations) | **Unchanged.** No intervening commit. |
| MINOR — WAL consumer cross-tenant visibility Rust-enforced | **Unchanged.** Deferred to P8c per proposal; no longer compound-exploitable now that the CRITICALs are closed. |
| MINOR — `init_session` `search_path` includes `public` | **Unchanged.** Accepted-known-gap. |
| — | **NEW: MINOR — `replication.rs:167-168` builds `"{pub_name}"` raw, mirroring the `backend/postgres.rs:400-404` convention deviation.** Same not-exploitable-today, convention-regression severity. |

---

## 5. Score

**80 / 100** (up from 68 in R2).

Justification:

- **+15** for closing the two R2 CRITICALs. The fix shape is
  deliberately hard to regress: the security-critical resolver is
  extracted into a separate, doc-commented, unit-tested helper, and
  the unit tests exercise the exact legacy exploit shapes.
- **-1** for the NEW convention-regression MINOR in `replication.rs`.
- **-2** because all the R2 IMPORTANT / MINOR findings (`backend/postgres.rs`,
  `audit.rs` duplicated allowlist, `count_violating_not_null` pub-dead,
  advisory-lock RAII, validator gaps) are still open. The codebase
  has had ample post-r2 commits in plugin-db (`b2496364`, `ff220fce`,
  `d7cfc089`, `f1c475f5`, etc.) so the fact that none of these
  defense-in-depth gaps were closed is a deferred-debt signal.

What would push to 90+:
1. Replace the four remaining hand-quoted SQL identifier sites
   (`backend/postgres.rs:400-404`, `:462-463`, `replication.rs:168`,
   `audit.rs` ×18) with `quote_ident`.
2. Drop the duplicate `audit::validate_app_id` in favour of
   `query::validate_schema`.
3. Add 63-byte length cap + reject-leading-digit to `validate_schema`.
4. Wrap `MigrationLock` in a Drop impl that issues
   `pg_advisory_unlock_all()`.
5. Either delete `count_violating_not_null` or demote it to
   `pub(crate)` and add validation.

Each is a localised change. None require architectural shifts.

What would push past 90:
6. Per-app Postgres role ownership of the WAL slot (P8c — proposal-tracked).
7. Move SECURITY DEFINER `search_path` to a hardened `extensions`
   schema in prod provisioning so `public` can be dropped from the
   SECURITY DEFINER path.
