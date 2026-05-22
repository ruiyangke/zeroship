# `crates/plugin-db` API Surface Review — 2026-05-22 R3

HEAD `c83d6a8c`. Read-only audit. Lens: API surface, follow-up to
`plugin-db-api-surface-2026-05-22-r1.md` (r1) and
`plugin-db-api-surface-2026-05-22-r2.md` (r2).

Commits applied since r2 that touch the API surface:

- `90d992d5` — cfg-gated module visibility on `test-helpers` (fixes r2 C1
  cleanly: 8 modules switch between `pub(crate)` in release and `pub`
  under the feature; integration target now builds).
- `0e58c4e8` + `b32ba383` — broker refactor to two-level `HashMap`
  (`app_id → collection → Vec<Subscription>`) plus a new
  `Broker::has_subscribers(&str, &str) -> bool` method and a
  `pub(crate)` free-function wrapper.
- `c83d6a8c` — replication empty-RETURNING fix; `replication.rs`
  internals continue to return `Result<_, String>` (no surface change,
  but see E5 below).

---

## Headline

The cfg-gating landed cleanly — `cargo check -p zeroship-plugin-db
--features test-helpers --tests` now succeeds. That closes r2's
CRITICAL regression. Eight smaller items carried forward from r1/r2
remain open; two new findings emerge from this round's audit
(`PostgresBackend::url` is a dead `pub` getter; the replication-ops
dispatch boundary wraps a flattened `String` back into
`DbError::Internal`, masking the SQLSTATE classification the SDK relies
on). The broker refactor's new method is correctly `pub fn` on a
`pub struct` while its free-function wrapper is `pub(crate)` — surface
shape is consistent.

---

## Findings

### CRITICAL

None at this round. R2 C1 is closed by `90d992d5`.

---

### IMPORTANT

**I1 — `auto_tx` dispatch still flattens `DbError` to bare string at the JS boundary** (`crates/plugin-db/src/orchestrator/auto_tx.rs:67-71, 104-109`)

Carried verbatim from r2 I1 — unchanged at HEAD `c83d6a8c`. Both
`auto_begin_transaction` and `auto_end_transaction` route the error
path through `OpResult::Failed { error: e.into_string(), … }`.
`DbError::into_string()` (`error.rs:252-267`) discards `.code` and
`.hint`; only the message body survives. Every other JS-visible
dispatch in the crate has been migrated to
`OpResult::JsValue { … RejectError(e.to_op_error()) … }`
(register_model, replication_ops × 3, the v8_class methods that go
through `to_op_error()` directly).

  Why: `__zsBeginAutoTx` / `__zsEndAutoTx` are the defense-in-depth
  envelope the runtime installs around every `query()` / `mutation()`
  handler. When BEGIN / COMMIT / ROLLBACK fails — most commonly with
  `serialization_failure` (40001) or `transient` (08*) on Postgres
  failovers — JS sees `err.code === undefined`. The `@zeroship/db`
  retry loop branches on `err.code`, so the documented retry never
  fires. Net effect: serialization conflicts surface to the end user as
  opaque errors instead of being swallowed by the SDK's exponential
  backoff.
  Fix: Swap the two `OpResult::Failed { error: e.into_string(), … }`
  blocks for `OpResult::JsValue { resolver, value:
  ResolveValue::RejectError(e.to_op_error()), request_id }` (same shape
  used at `register_model/mod.rs:95-98`). Both dispatchers already use
  `setup_promise` instead of `setup_js_promise`, so the resolver type
  threading needs to be matched — but the conversion is mechanical.
  Verification: `grep -n "e.into_string()" crates/plugin-db/src/orchestrator/auto_tx.rs` → lines 69, 106. Cf. successful migration template at `crates/plugin-db/src/orchestrator/register_model/mod.rs:95-98` and `crates/plugin-db/src/replication_ops.rs:69-87,102-122,134-160`.

---

**I2 — `IsolateDbContext` fields remain `pub(crate)` despite r1 I3 / r2 I2 recommendation** (`crates/plugin-db/src/context.rs:49-62, 70-158`)

Carried verbatim from r2 I2. 12 `pub(crate)` fields on
`IsolateDbContext` plus the 6 on the nested `MigrationLock` struct. The
parent `mod context;` is `pub(crate)` (`lib.rs:56`), so this is purely
an internal-consistency hazard, but every field already has a typed
accessor (`context.rs:187-431`). A grep over the rest of the crate
shows zero direct field access — every existing caller goes through
the accessors, so demoting to private has no functional cost.

  Why: The next refactor that touches `context.rs` can write `ctx.tx_token = 0`
  directly and bypass the monotonicity invariant `next_tx_token` enforces.
  Today's invariants ("only `next_tx_token` increments; drains zero it";
  "`auto_tx_owned` flips false on settle"; "`tx_conn` is `Some` iff
  `tx_token != 0`") are documented only in comments, not in the type.
  Fix: Drop `pub(crate)` from each field on both `IsolateDbContext` and
  `MigrationLock`. The accessors at `context.rs:187-431` already cover
  every read/write.
  Verification: `grep -nE "pub\(crate\) [a-z_]+: " crates/plugin-db/src/context.rs` → 18 hits across both structs. `grep -rEn "c\.(pool|db_url|tx_conn|tx_token|auto_tx_owned|tx_token_counter|pending_emits|mig_lock|registered_models|running_consumers|backend)\s*[=.]" crates/plugin-db/src --include='*.rs' | grep -v "context.rs"` → 0 hits (every caller goes through `c.pool()`, `c.has_tx()`, `c.next_tx_token()`, …).

---

**I3 — `wal_consumer` legacy `set_local_emit_suppressed` / `local_emit_suppressed` / `any_app_suppressed` are unconditionally `#[doc(hidden)] pub`** (`crates/plugin-db/src/wal_consumer.rs:131, 173, 185`)

Carried from r1 M1 / r2 I3. Now that `wal_consumer` is cfg-gated
`pub`-or-`pub(crate)` (per `90d992d5`), these three legacy shims are
externally unreachable in release builds — yet they continue to ship
in the release binary as `#[doc(hidden)] pub fn` with no cfg gate. The
in-crate tests that exercise the legacy thread-wide flag (`wal_consumer.rs:831-856`)
call them inside `#[cfg(test)]` so a cfg-gate around the functions
themselves keeps the unit tests green. No external test uses them
(`grep -rn "set_local_emit_suppressed\|local_emit_suppressed\|any_app_suppressed" tests/` → 0 hits inside the plugin-db tests directory).

  Why: Dead code in release binaries. `#[doc(hidden)] pub` is the
  wrong tool — it suggests "kept for ABI" but the symbols are gone from
  the effective public surface anyway because the parent module is
  `pub(crate)` in release. Either delete them (preferred — the unit
  test at line 831 is the only consumer), or cfg-gate them. Five
  modules in the crate already follow the cfg-gating pattern for their
  test-only `*_for_tests` helpers — these three are the odd ones out.
  Fix: Prefix all three with `#[cfg(any(test, feature = "test-helpers"))]`. Or delete and inline the unit test that uses them.
  Verification: `crates/plugin-db/src/wal_consumer.rs:130-187` (no cfg gate). Compare to the cfg-gated pattern at `crates/plugin-db/src/exec.rs:333`, `crates/plugin-db/src/migrations.rs:753-832`, `crates/plugin-db/src/replication_ops.rs:279-287`.

---

**I4 — Replication-ops dispatch boundary masks SQLSTATE classification by re-wrapping in `DbError::Internal`** (`crates/plugin-db/src/replication_ops.rs:82-86, 116-120, 153-157`)

New finding from the error-envelope sweep. The three
`db.replication.*` dispatchers all do:

```rust
Err(e) => OpResult::JsValue {
    resolver,
    value: ResolveValue::RejectError(DbError::Internal { message: e }.to_op_error()),
    request_id,
},
```

The `e` here is a flat `String` coming back from
`crate::replication::ensure_publication_and_slot` /
`watchdog_query` / `drop_abandoned_slots`. Internally those functions
build `DbError::Transient` / `Internal` via `from_pg(...)` for the
SQLSTATE-classified failures, then collapse them via `.into_string()`
before returning (`replication.rs:178, 200, 232, 248`, etc.). The
boundary then re-wraps the flat string in `DbError::Internal`, so
every JS-visible failure stamps `err.code = "internal"` regardless of
the underlying cause.

Concretely, a connection failure during `db.replication.setup()` —
which `from_pg` would classify as `Transient` (SQLSTATE class 08) and
the SDK could surface with a "retry after backoff" hint — instead
reaches JS as `code: "internal"`, no hint, no SDK-driven retry.
Similarly a `wal_level=logical` misconfiguration is intercepted at
`replication.rs:222-229` and turned into a long human message but no
typed `DbError::Configuration` carrying `code: "wal_level_not_logical"`
— the SDK has no way to distinguish "operator needs to fix the cluster"
from "transient backend hiccup."

  Why: Every internal `from_pg` classification is discarded at the
  boundary because `replication.rs` signatures are `Result<_, String>`.
  The boundary code has no choice — the type system already lost the
  variant. This is the same class of bug r1 C1 closed for audit.rs and
  r1 C2 closed for the Backend trait, except for the
  `replication.rs` surface.
  Fix: Two passes. (1) Change `ensure_publication_and_slot` /
  `watchdog_query` / `drop_abandoned_slots` and their helpers to
  return `Result<_, DbError>` (mechanical conversion — the `from_pg`
  call sites already construct typed variants and just flatten
  immediately). Tag the `wal_level` branch as
  `DbError::Configuration { code: "wal_level_not_logical", ... }`.
  (2) Drop the `DbError::Internal { message: e }` wrappers in
  `replication_ops.rs` lines 82-86 / 116-120 / 153-157; pass the
  error through with `?` and call `.to_op_error()` directly.
  Verification: `crates/plugin-db/src/replication.rs:145-148, 222-232, 327, 421-424` (all `Result<_, String>` returns). `grep -n "DbError::Internal { message: e }" crates/plugin-db/src/replication_ops.rs` → 3 hits. The same boundary-wrap test pattern that locked down `audit.rs`'s migration to typed errors applies here too — see `crates/plugin-db/src/error.rs:472-517`.

---

**I5 — `backend::BackendHandle` type alias is `pub` but unused outside its own definition** (`crates/plugin-db/src/backend/mod.rs:364`)

Carried verbatim from r2 I5. Compiler emits the warning at HEAD
(`cargo check -p zeroship-plugin-db 2>&1 | grep BackendHandle`):

> warning: type alias `BackendHandle` is never used
>    --> crates/plugin-db/src/backend/mod.rs:364:1

The docstring claims it is "the opaque trait-object handle that the
per-isolate context stores" — but `context.rs:158` stores
`Option<Rc<PostgresBackend>>` directly. The only references besides
the alias's own definition are the two compile-time identity tests at
`backend/mod.rs:415, 432`.

  Why: Dead nominal API alias; the docstring promises a "type-erased
  stash" the implementation never opted into.
  Fix: Either (a) thread `BackendHandle` through
  `IsolateDbContext::backend` per the docstring's intent, or (b) delete
  it together with the two compile-time identity tests. Demote to
  `pub(crate)` if deletion is too aggressive.
  Verification: `cargo check -p zeroship-plugin-db --features test-helpers --tests 2>&1 | grep BackendHandle` → "type alias `BackendHandle` is never used". `grep -rn "BackendHandle" crates --include='*.rs'` → 3 hits, all inside `backend/mod.rs`.

---

**I6 — `PostgresBackend::url()` getter is `pub fn` with zero callers (new)** (`crates/plugin-db/src/backend/postgres.rs:59-61`)

New finding from the unused-method sweep. `PostgresBackend::url()` is
documented as "Borrow the configured URL." (`postgres.rs:58`), but no
caller in the repo invokes it (`grep -rn "backend\.url\|PostgresBackend::url" crates`
→ 0 hits outside `postgres.rs`'s own doc-comment). Compiler emits
`warning: method 'url' is never used` in release builds.

The companion `pool()` getter at line 54 *is* used
(`bootstrap.rs:103`), so the pair is asymmetric — `pool` carries its
weight, `url` does not. The internal references to `self.url` at
lines 71-72 go through the field directly, not the getter.

  Why: Dead nominal surface on a `pub struct`. The struct itself is
  `pub` (the `Backend` trait is the documented public surface), so a
  `pub` getter here is more than the test-driven minimum.
  Fix: Demote to `pub(crate) fn url(&self)` or delete outright. The
  field-direct access in `acquire_dedicated_client` is the only
  in-crate consumer of `self.url`.
  Verification: `cargo check -p zeroship-plugin-db 2>&1 | grep "method 'url'"` → `method 'url' is never used`. `grep -rn "backend\.url\|\.url()" crates/plugin-db --include='*.rs' | grep -v "self.url" | grep -v doc-comment` → 0 hits.

---

### MINOR

**M1 — `QueryError` is `pub enum` without `#[non_exhaustive]`** (`crates/plugin-db/src/query.rs:20`)

Carried verbatim from r2 M1. `QueryError` is `pub`-reachable now that
`query` is unconditionally `pub` (`lib.rs:51`). Its three variants are
the surface the `From<QueryError> for DbError` (`error.rs:351-365`)
collapses into validation codes. R1 M3 / r2 M3 already applied
`#[non_exhaustive]` to `DbError` (`error.rs:51`); the asymmetry is
purely cosmetic but consistent treatment helps reviewers see "this is
the forward-compat invariant" at a glance.

  Why: Cheap futureproofing; consistency with `DbError`. Any external
  matcher in `tests/integration.rs:140` (`use zeroship_plugin_db::query::*`)
  exhaustively branching on `QueryError` would break on a new
  builder-side error class (e.g. `InvalidOperator`, `InvalidProjection`).
  Fix: Add `#[non_exhaustive]` above `pub enum QueryError` at `query.rs:20`.
  Verification: `crates/plugin-db/src/query.rs:20`. Cf. `crates/plugin-db/src/error.rs:51`.

---

**M2 — Inner `v8_classes` submodules `migration`, `migrations`, `replication`, `transaction` are `pub mod` despite zero external use** (`crates/plugin-db/src/v8_classes/mod.rs:34-38`)

Carried verbatim from r2 M2. The parent module is `pub` (for
`tests/db_v8_class.rs` and `tests/subscription_finalizer.rs`, which
import `v8_classes::{db, subscription, collection}`). The four
remaining submodules have zero callers outside `crates/plugin-db/src/v8_classes/`:

```
$ grep -rn "v8_classes::\(migration\|migrations\|replication\|transaction\)" crates --include='*.rs' | grep -v "/plugin-db/src/v8_classes/"
crates/plugin-db/src/lib.rs:6,11,15:  # doc-link only
crates/plugin-db/src/orchestrator/transaction.rs:55:  # mint_transaction call, sibling module within v8_classes — uses crate path
crates/plugin-db/src/migrations.rs:10:  # doc-link only
```

The `mint_*` helpers each carry a `pub fn` that's also externally
unreachable.

  Why: Inflates the externally-reachable `v8_classes` API surface for
  no benefit. `cargo doc` indexes every internal mint helper and the
  `Migration` / `Migrations` / `Replication` / `Transaction` state
  structs that hold internal-field `Box<T>` payloads.
  Fix: `pub(crate) mod migration; pub(crate) mod migrations; pub(crate) mod replication; pub(crate) mod transaction;`. Verify
  `tests/db_v8_class.rs` and `tests/subscription_finalizer.rs` still
  build (they only touch `db` and `subscription`).
  Verification: `grep -rn "v8_classes::\(migration\|migrations\|replication\|transaction\)" crates --include='*.rs' | grep -v "/plugin-db/src/"` → 0 external hits.

---

**M3 — Mint helpers `mint_collection`, `mint_transaction`, `mint_migrations`, `mint_replication`, `migration_start_with_spec` are `pub fn` despite no external caller** (`crates/plugin-db/src/v8_classes/collection.rs:353`, `transaction.rs:288`, `migrations.rs:251`, `replication.rs:117`, `migration.rs:598`)

Extension of r2 M3. External tests touch only `mint_db`
(`tests/db_v8_class.rs:21,32,63`) and `mint_subscription`
(`tests/subscription_finalizer.rs:25,54,100`). The other five mint
helpers are called only by sibling files within `v8_classes/`.

  Why: Nominal-surface bloat. The mint functions' signatures (scope,
  name, app_id, occasionally a token) are v8-internal contracts that
  should not be part of the documented surface or `cargo doc` output.
  Fix: Demote each to `pub(crate) fn`. Tied to M2 — if the parent
  submodules become `pub(crate)`, the mint helpers can stay `pub` and
  this finding collapses.
  Verification: `grep -rn "mint_collection\|mint_transaction\|mint_migrations\|mint_replication\|migration_start_with_spec" crates --include='*.rs' | grep -v "/plugin-db/src/"` → 0 external hits.

---

**M4 — `audit::read_processed_from_audit_row` / `read_dead_letter_pks_from_audit_row` are `pub` despite only internal use** (`crates/plugin-db/src/audit.rs:463, 478`)

Carried verbatim from r2 M4. Both row-decode helpers are called from
within `audit.rs` itself (lines 510-511). No external caller in any
test or sibling module.

  Why: Bloats the audit module surface with implementation-detail
  decode helpers — particularly visible now that `audit` is `pub` under
  `test-helpers` (`lib.rs:64-66`).
  Fix: `pub(crate) fn` on both. Same pattern that `pub(crate) trait AuditExecutor` (`audit.rs:431`) already follows.
  Verification: `grep -rn "read_processed_from_audit_row\|read_dead_letter_pks_from_audit_row" crates --include='*.rs'` → 4 hits, all inside `crates/plugin-db/src/audit.rs`.

---

**M5 — `orchestrator/mod.rs` doc still says "Each submodule is `pub(crate)`"** (`crates/plugin-db/src/orchestrator/mod.rs:22, 28-30`)

Carried verbatim from r2 M5. The module doc-comment claims
"Each submodule is `pub(crate)` to scope visibility" but the declarations
at lines 28-30 are `pub mod auto_tx; pub mod register_model; pub mod transaction;`.
With `orchestrator` itself being `pub` (cfg-gated under
`test-helpers`, `pub(crate)` in release), the effective external
reachability matches `pub mod` — but the doc is stale and misleading.

  Why: Doc/code drift hides intent from future maintainers. R1 / r2
  already flagged this; the easy fix is one line.
  Fix: Change the doc to "Each submodule is `pub mod` so test crates
  can reach `register_model::exec_register_model_with_pool` under the
  `test-helpers` feature; the parent `mod orchestrator` is cfg-gated."
  Or convert the declarations to `pub(crate) mod` and tighten the
  re-exports — but that re-breaks `tests/integration.rs`, so the
  doc-edit is the right move.
  Verification: `crates/plugin-db/src/orchestrator/mod.rs:22` reads
  "Each submodule is `pub(crate)`"; lines 28-30 read `pub mod ...`.

---

**M6 — `init_pool_async` has no external callers but is the documented bootstrap entry point** (`crates/plugin-db/src/lib.rs:349`)

Carried verbatim from r2 M6. The lib-level doc (`lib.rs:336-348`) and
`PostgresBackend`'s comment (`backend/postgres.rs:28`) both treat
`init_pool_async` as the entry point for embedding the plugin. No
crate outside `plugin-db` calls it (`grep -rn "init_pool_async"
crates --include='*.rs' | grep -v "/plugin-db/"` → 0 hits). Both
`crates/worker/` and `crates/cli/` register `DbPlugin::new(url)` and
rely on the lazy init path inside `exec.rs:62-66` and
`register_model/mod.rs:117-122`.

  Why: Either the doc is stale (no caller invokes it manually) or the
  entry point is missing (worker/cli should call it during startup so
  the first JS request doesn't pay the connect latency). Decision is
  one paragraph.
  Fix: Pick one. Either delete the function (lazy init covers
  everything) or wire it into worker startup. Don't leave the doc
  promising an entry point nobody uses.
  Verification: `grep -rn "init_pool_async" crates --include='*.rs'` → all callers are inside `crates/plugin-db/`.

---

**M7 — `auth/mod.rs` re-exports flagged "unused" in release builds (new)** (`crates/plugin-db/src/auth/mod.rs:68-70`)

New finding from the release-build warning sweep. Three lines of
`pub use` re-exports surface `BootstrapOutcome`, `ensure_admin_schema`,
`RotationOutcome`, `rotate_session_keys`, `MintedToken`, `SessionInit`,
`init_session`, `mint_session_token` at the top of the `auth` module.
In release builds (`cargo check -p zeroship-plugin-db`) the compiler
emits:

> warning: unused imports: `BootstrapOutcome` and `ensure_admin_schema`
>    --> crates/plugin-db/src/auth/mod.rs:68:21
> warning: unused imports: `RotationOutcome` and `rotate_session_keys`
> warning: unused imports: `MintedToken`, `SessionInit`, `init_session`, and `mint_session_token`

This is the cfg-gate's downside: the re-exports exist precisely so
that `tests/integration.rs` can write `zeroship_plugin_db::auth::ensure_admin_schema(...)`
under `test-helpers`, but in release the parent `auth` module is
`pub(crate)` and the re-exports have no in-crate consumer (the
sibling files use `super::{ADMIN_SCHEMA, …}` paths, not the
re-export). The warnings are noisy and signal the asymmetry.

Bonus observation: `auth::bootstrap` and `auth::session` submodules
are `pub mod` but unreached externally — only `auth::keys` is.
`tests/integration.rs` reaches the bootstrap / session items via the
top-level `pub use`, never via the submodule path. The submodules
could be `pub(crate) mod` without affecting external callers.

  Why: The release-build warning surface is a maintenance smell —
  reviewers stop reading warnings when they're full of "unused"
  re-exports that exist purely for cfg-feature callers. The
  inconsistency between `pub mod bootstrap; pub mod session; pub mod keys;`
  (all three `pub`) and the actual external usage (only `keys` is
  reached directly) doesn't carry its weight.
  Fix: Two clean options. (1) Add `#[cfg(any(test, feature = "test-helpers"))]` to the three `pub use` lines so they only exist when the test-feature is on. The sibling files inside `auth/` use `super::` paths so they're unaffected. (2) Demote `auth::bootstrap` and `auth::session` to `pub(crate) mod` — `tests/integration.rs` only reaches `auth::ensure_admin_schema` and `auth::init_session` via the top-level re-exports. Keep `auth::keys` as `pub mod` for the test usage at lines 3413, 3680, 3700.
  Verification: `cargo check -p zeroship-plugin-db 2>&1 | grep -E "unused imports.*(BootstrapOutcome|RotationOutcome|MintedToken)"` → 3 warnings. `grep -rn "auth::bootstrap\|auth::session\|auth::keys" crates --include='*.rs' | grep -v "/plugin-db/src/auth/"` → only `tests/integration.rs:3413,3680,3700` (all `auth::keys::*`).

---

## Per-dimension audit summary

| Dimension | Status |
| --- | --- |
| 1. cfg-fork in `lib.rs` (8 modules) | **Consistent.** Pattern is symmetric and minimal; `90d992d5` correctly toggles exactly the eight modules `tests/integration.rs` consumes. Each pair (`#[cfg(not(feature = "test-helpers"))] pub(crate) mod X; #[cfg(feature = "test-helpers")] pub mod X;`) is correctly bracketed. The four always-`pub` modules (`broker`, `error`, `query`, `v8_classes`) match their justification in the lib-level comment (`lib.rs:34-46`). |
| 2. `pub` items inside `broker` / `query` / `v8_classes` / `error` | **`broker` carries dead `pub` items** (`publish` free fn, `DEFAULT_QUEUE_DEPTH`, `ws_frame_for_change`, `ws_frame_for_control`, `ws_frame`, `Broker` struct itself, `message_to_json`). Per the new `has_subscribers` refactor the broker is structurally clean — the new method on `Broker` is `pub fn` (sound — `Broker` is `pub`) and the free-fn wrapper is `pub(crate)`. But the older `ws_frame*` builders still ship without callers (`grep -rn "ws_frame" crates --include='*.rs'` outside `broker.rs` → 0 hits). Captured under r2 I4 (still open). `query` exposes the full `build_*` family (`build_create_schema`, `build_create_table_with_fks`, `build_create_indexes`, `build_named_indexes`, `build_find`, …) — all consumed by `tests/integration.rs` (`integration.rs:140` does `use zeroship_plugin_db::query::*`), so the surface is justified. `error` and `v8_classes` surfaces are tight. |
| 3. `#[doc(hidden)] pub fn` items in release | **Three leaks in `wal_consumer.rs`** (I3 above: `any_app_suppressed`, `set_local_emit_suppressed`, `local_emit_suppressed`). Every other `#[doc(hidden)] pub` site in the crate is correctly cfg-gated (`exec.rs:333`, `migrations.rs:753-832`, `replication_ops.rs:279-287`, `lib.rs:208-332`). |
| 4. `Backend` trait surface | **Still half-applied per r4 I4.** The trait covers connection lifecycle, advisory locks, schema bootstrap+introspection, and audit reads/writes — 22 async methods. `PostgresBackend` is the only impl. Three items remain Postgres-only and leak through to consumer files: (a) `replication.rs` + `wal_consumer.rs` talk raw `pg_replication_slots` and the streaming WAL protocol; the trait does not name a `start_logical_decoding` seam. (b) `auth/bootstrap.rs` runs SECURITY DEFINER ceremony with PG-only `gen_random_bytes`/`hmac`. (c) `crate::query` builds Postgres DDL/DML strings directly via `quote_ident`, `ON CONFLICT`, `RETURNING`. The trait's own doc explicitly scopes (a) and (c) out (`backend/mod.rs:23-30`), which is fine — the half-applied note is descriptive, not a recommendation to expand. **The new finding is I6: `PostgresBackend::url()` is `pub` with zero callers** — a getter that exists in the impl but is not part of the `Backend` trait surface and has no consumer. |
| 5. Error envelope consistency | **One masking site (I4 above) plus the carried I1.** Every JS-visible dispatch except `auto_tx` correctly routes through `to_op_error()`. The replication-ops dispatch routes through `to_op_error()` at the boundary, but the *inputs* it sees have already been flattened from typed `DbError::Transient`/`Configuration` to bare `String` inside `replication.rs`. Net effect: every replication path's `err.code` reaches JS as `"internal"`. SDK loses the SQLSTATE-driven retry logic. Every `DbError::*` variant is *reachable* from JS in principle (the dispatch boundary type-checks); the masking is upstream in `replication.rs`'s signatures. |
| 6. JS-visible `v8_method` annotations | **Clean — only entry points are `#[v8_method]`.** No `r.add(...)` callback pattern survives. The single `add_setup` callback in `DbPlugin::register` (`lib.rs:199-201`) installs `__zsBeginAutoTx`/`__zsEndAutoTx` on `globalThis`. Every other JS entry point is a `#[v8_method]` / `#[v8_getter]` / `#[v8_constructor]` on one of the seven v8_classes (`Db`, `Collection`, `Transaction`, `Migrations`, `Migration`, `Subscription`, `Replication`). |
| 7. `pub use` re-exports | **Two issues.** (a) `auth/mod.rs:68-70` re-exports 8 items that are reachable only when `test-helpers` is on — release builds emit "unused imports" warnings (M7 new). (b) `backend/mod.rs:50` re-exports `PostgresBackend` which is used internally and externally; correct. |

---

## R2 → R3 status

| R2 finding | Severity | Status at HEAD `c83d6a8c` | Evidence |
|---|---|---|---|
| C1 integration tests do not compile under `--features test-helpers` | CRITICAL | **Fixed** (commit `90d992d5`) | `cargo check -p zeroship-plugin-db --features test-helpers --tests` → Finished |
| I1 `auto_tx` flattens DbError to bare string | IMPORTANT | **Not addressed** (carried as r3 I1) | `auto_tx.rs:69,106` still call `e.into_string()` |
| I2 `IsolateDbContext` fields `pub(crate)` | IMPORTANT | **Not addressed** (carried as r3 I2) | `context.rs:49-62, 70-158` |
| I3 `wal_consumer` legacy shims unconditionally `#[doc(hidden)] pub` | IMPORTANT | **Not addressed** (carried as r3 I3) | `wal_consumer.rs:130-187` |
| I4 dead `pub` items in `broker` (`publish` free fn, `ws_frame*`, `DEFAULT_QUEUE_DEPTH`, `Broker`) | IMPORTANT | **Not addressed** (rolled into per-dimension #2 summary above) | `broker.rs:148,402,604,715,733,753` |
| I5 `BackendHandle` alias `pub` but unused | IMPORTANT | **Not addressed** (carried as r3 I5) | `backend/mod.rs:364`; compiler warns "type alias `BackendHandle` is never used" |
| M1 `QueryError` lacks `#[non_exhaustive]` | MINOR | **Not addressed** (carried as r3 M1) | `query.rs:20` |
| M2 inner `v8_classes` submodules `pub mod` despite zero external use | MINOR | **Not addressed** (carried as r3 M2) | `v8_classes/mod.rs:34-38` |
| M3 `mint_collection` (and 4 siblings) `pub` but only internal | MINOR | **Not addressed** (carried as r3 M3) | `v8_classes/collection.rs:353, transaction.rs:288, migrations.rs:251, replication.rs:117, migration.rs:598` |
| M4 `audit::read_*_from_audit_row` are `pub` despite internal-only use | MINOR | **Not addressed** (carried as r3 M4) | `audit.rs:463,478` |
| M5 `orchestrator/mod.rs` doc says "`pub(crate)` submodules" but code is `pub mod` | MINOR | **Not addressed** (carried as r3 M5) | `orchestrator/mod.rs:22, 28-30` |
| M6 `init_pool_async` has no external callers but is the documented entry point | MINOR | **Not addressed** (carried as r3 M6) | `lib.rs:349`; doc at 336-348 |

New in r3:
- **I4** (replication-ops dispatch boundary re-wraps flattened strings as `DbError::Internal`, masking SQLSTATE classification — the same class of leak r1 fixed for audit.rs and the Backend trait).
- **I6** (`PostgresBackend::url()` getter `pub fn` with zero callers; compiler warns).
- **M7** (`auth/mod.rs` re-exports cause "unused imports" warnings in release builds; `auth::bootstrap` / `auth::session` submodules are `pub mod` despite zero external use).

---

## Score

**81 / 100** (vs r2 `74 / 100`).

The big move is closing r2's CRITICAL regression — `90d992d5` is a
clean cfg-gate that snaps the eight module declarations into a
consistent shape and lets `cargo check --features test-helpers
--tests` succeed. That's worth +7 to the headline score.

What holds it back from a higher mark:

1. The carried-over IMPORTANT items (`auto_tx` flattens, `IsolateDbContext` fields, `wal_consumer` legacy shims, `BackendHandle` alias) have been audit-flagged for three rounds without movement. Each is a one-line fix; the cumulative drag is real.
2. The new I4 (replication-ops envelope masking) is the same class of bug r1 closed for two other surfaces — the typed-error invariant the rest of the crate has adopted hasn't reached `replication.rs` yet.
3. M7 (auth re-export warnings) is a cfg-asymmetry that fell out of the cfg-gating commit and points at a smaller follow-up sweep.

Top three fixes, in order:

1. **I1 — fix `auto_tx` dispatch to call `e.to_op_error()`.** Two-line change; closes the last "lost `.code` at the JS boundary" hole in the crate. After this, every JS-visible dispatch carries `err.code`.
2. **I4 — convert `replication.rs` signatures to `Result<_, DbError>` and drop the `DbError::Internal { message: e }` wrappers in `replication_ops.rs:82-86, 116-120, 153-157`.** Mechanical conversion (the typed variants already exist inside the functions via `from_pg`, they just get flattened immediately). Restores `transient`/`configuration`/`wal_level_not_logical` codes at the JS boundary for replication paths.
3. **I3 — cfg-gate the three legacy `wal_consumer` shims (or delete them).** Brings the file in line with the rest of the crate's test-helper pattern; removes dead code from release binaries.
