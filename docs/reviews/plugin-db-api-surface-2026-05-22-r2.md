# `crates/plugin-db` API Surface Review — 2026-05-22 R2

HEAD `d2e7e22`. Read-only audit. Lens: API surface, follow-up to
`plugin-db-api-surface-2026-05-22-r1.md` (commit `5b65e6f8`).

Round 1 commits applied: `2fe9e9f0` (broad demotion to `pub(crate)`) and
`b2496364`/`5be3c1a1` (partial revert promoting `broker` + `v8_classes` +
`query` back to `pub` for external test crates).

---

## Headline

The demotion landed but the revert was undersized: **six modules used by
`tests/integration.rs` were left `pub(crate)`** and `cargo test --features
test-helpers` no longer compiles. Several r1 findings (audit→DbError,
backend trait→DbError, error envelope on register_model, replication
dispatch wrapping, `_pub` suffix, `#[non_exhaustive] DbError`) were fixed
cleanly. Three r1 findings remained un-addressed (`IsolateDbContext` field
visibility, `wal_consumer` legacy shims, `auto_tx` dispatch still flattens
DbError to string). A few new findings emerge from the demotion: dead `pub`
items inside `broker` (`publish`, `ws_frame*`, `Broker`,
`DEFAULT_QUEUE_DEPTH`) and `backend::BackendHandle`.

---

## Findings

### CRITICAL

**C1 — Integration tests do not compile under `--features test-helpers`** (`crates/plugin-db/src/lib.rs:33,34,44,45,49,50,55`)

`cargo test -p zeroship-plugin-db --features test-helpers --no-run` fails
with 118 errors. Six modules consumed by the external test crate
`tests/integration.rs` were demoted to `pub(crate)` in commit `2fe9e9f0`
and the follow-up revert (`5be3c1a1`) only re-promoted three of them
(`broker`, `query`, `v8_classes`). The remaining six are still private:

  Why: A complete platform regression in the `--features test-helpers`
  build target — the integration suite that covers audit / orchestrator /
  migration / replication / auth bootstrap is currently un-runnable. CI
  (`.github/workflows/ci.yml:12`) calls `cargo test --workspace` without
  the feature so the breakage is invisible to it: the integration test
  declares `required-features = ["test-helpers"]` (`Cargo.toml:32`) and
  silently skips. The r1 review's `pub(crate)` recommendation table did
  NOT include `audit`, `orchestrator`, `migrations`, `replication`,
  `replication_ops`, `wal_consumer`, or `auth` — it specifically called
  out only their *internal helpers* (not the modules themselves) for
  demotion. The applied commit over-reached.
  Fix: Re-promote the six modules with the same one-line `// xxx stays
  pub: tests/integration.rs (external test crate) uses …` comment style
  the revert applied to `broker` / `query` / `v8_classes`. Concretely:
  ```rust
  // audit stays pub: tests/integration.rs uses zeroship_plugin_db::audit::ensure_audit_table_exists.
  pub mod audit;
  // auth stays pub: tests/integration.rs uses zeroship_plugin_db::auth::{ensure_admin_schema, mint_session_token, init_session}.
  pub mod auth;
  // migrations stays pub: tests/integration.rs uses zeroship_plugin_db::migrations as mig.
  pub mod migrations;
  // orchestrator stays pub: tests/integration.rs uses zeroship_plugin_db::orchestrator::register_model::exec_register_model_with_pool.
  pub mod orchestrator;
  // replication stays pub: tests/integration.rs reaches zeroship_plugin_db::replication::*.
  pub mod replication;
  // replication_ops stays pub: tests/integration.rs uses zeroship_plugin_db::replication_ops::{clear_consumer_registry_for_tests, is_consumer_registered_for_tests}.
  pub mod replication_ops;
  // wal_consumer stays pub: tests/integration.rs uses zeroship_plugin_db::wal_consumer::{emit_local, is_app_suppressed, suppress_app, unsuppress_app}.
  pub mod wal_consumer;
  ```
  Also fix CI to run `cargo test -p zeroship-plugin-db --features
  test-helpers --no-run` (build-only is enough; live DB tests need a
  Postgres listener that CI doesn't have).
  Verification: `cargo test -p zeroship-plugin-db --features
  test-helpers --no-run` → 118 errors, sample at
  `crates/plugin-db/tests/integration.rs:985,1027,1469,3379,4162,4170,4239,4290,4361`.
  Module references at `crates/plugin-db/src/lib.rs:33-55`.

---

### IMPORTANT

**I1 — `auto_tx` dispatch flattens `DbError` to bare string (lost `.code`)** (`crates/plugin-db/src/orchestrator/auto_tx.rs:67-71, 104-109`)

Both `auto_begin_transaction` and `auto_end_transaction` route their
error path through `OpResult::Failed { error: e.into_string() }`. Per
`error.rs:241-256`, `into_string()` discards everything except the
message body — `e.code` is dropped before the V8 boundary. This mirrors
r1 I1 for `register_model_dispatch`, which has since been fixed (now
uses `OpResult::JsValue { ResolveValue::RejectError(e.to_op_error()) }`
at `register_model/mod.rs:95-98`). The auto-tx path is the defense-in-
depth envelope around every `query()` / `mutation()` handler; any failure
to BEGIN / COMMIT / ROLLBACK reaches JS with `err.code = undefined`
instead of the canonical `serialization_failure` / `transient` /
`lock_not_available` / etc.

  Why: SDK error-handling code branches on `err.code`. Losing the code on
  the auto-tx envelope means every transactional retry loop in
  `@zeroship/db` mis-identifies serialization conflicts as opaque
  failures and skips the documented retry.
  Fix: Switch both dispatchers to `OpResult::JsValue {
  ResolveValue::RejectError(e.to_op_error()), … }`. Same one-line edit
  the register_model path landed.
  Verification: `auto_tx.rs:69` and `auto_tx.rs:106` both call
  `e.into_string()`. Successful migration template at
  `register_model/mod.rs:95-98`.

---

**I2 — `IsolateDbContext` fields remain `pub(crate)` despite r1 I3 recommendation** (`crates/plugin-db/src/context.rs:60-62, 70-158`)

R1 I3 recommended making the fields on `IsolateDbContext` private to
force callers through typed accessors. The fields are still `pub(crate)`
at `context.rs:60-62` (`MigrationLock`: `start_generation`, `client`)
and `context.rs:70-158` (every field on the struct). The accessors
themselves all exist and are exhaustive; the fields could be private
without functional impact. Module-level `pub(crate)` on `mod context`
prevents external leakage, so severity is IMPORTANT, not CRITICAL — the
internal consistency hazard remains (a future bug-fix could mutate
`tx_token` directly bypassing `next_tx_token`'s monotonicity invariant).

  Why: The next refactor that touches this file will be tempted to write
  `ctx.tx_token = 0` directly instead of going through the proper drain
  path. Today's stable invariant ("only `next_tx_token` increments;
  drains zero it") is encoded only in commentary, not the type.
  Fix: Drop `pub(crate)` from each field. Every external caller already
  routes through `with` / `with_mut` and the typed accessors that exist
  at `context.rs:187-431`.
  Verification: `grep -nE "pub\(crate\) [a-z_]+: " crates/plugin-db/src/context.rs` lists 12 fields. The accessor methods at lines 187-431 already cover every field.

---

**I3 — `wal_consumer` legacy `set_local_emit_suppressed` / `local_emit_suppressed` are unconditionally `#[doc(hidden)] pub`** (`crates/plugin-db/src/wal_consumer.rs:173, 185`)

R1 M1 recommended gating these legacy shims behind `cfg(any(test,
feature="test-helpers"))`. Status today: still unconditionally `pub` with
no cfg gate (`wal_consumer.rs:172, 184`). The compiler confirms they are
dead in production: `cargo check --tests` emits warning
`function 'set_local_emit_suppressed' is never used` (and same for
`local_emit_suppressed`). Bumping the severity from M1 to I3 because the
demotion of the parent `wal_consumer` module (currently `pub(crate)` per
C1) means these shims aren't externally reachable at all — and yet they
still ship in release builds wrapped in `#[doc(hidden)]` instead of cfg
elimination.

  Why: Dead code in release binaries; the `#[doc(hidden)]` annotation is
  misleading (suggests "kept for ABI" but the symbols are gone from the
  effective public surface anyway).
  Fix: Wrap both functions in `#[cfg(any(test, feature = "test-helpers"))]`. Better: remove them entirely once C1's `pub mod wal_consumer` re-promotion lands — confirm nothing in `tests/` actually calls them (grep below shows none).
  Verification: `crates/plugin-db/src/wal_consumer.rs:172-187` (no cfg gate); compiler warnings in the `cargo check -p zeroship-plugin-db --tests` output (`function 'set_local_emit_suppressed' is never used`). `grep -rn "set_local_emit_suppressed\|local_emit_suppressed" crates/plugin-db/tests` returns no matches.

---

**I4 — `broker::publish` (free fn), `ws_frame_for_change`, `ws_frame_for_control`, `ws_frame`, `Broker` (struct), `DEFAULT_QUEUE_DEPTH` are `pub` but reach zero external callers** (`crates/plugin-db/src/broker.rs:148, 386, 551, 660, 678, 698`)

After r1, `broker` is back to `pub mod` (C1 above keeps it that way).
Inside `broker`, six items are `pub` but have no external Rust caller
and no JS-surface need:

| Item | Site | Internal use | External use |
|---|---|---|---|
| `pub fn publish(event: &ChangeEvent)` | `broker.rs:551` | `wal_consumer.rs:81 (use), 219, 568` | none |
| `pub fn ws_frame_for_change` | `broker.rs:660` | self-tests only | none |
| `pub fn ws_frame_for_control` | `broker.rs:678` | self-tests + `broker.rs:701` | none |
| `pub fn ws_frame` | `broker.rs:698` | none | none |
| `pub struct Broker` | `broker.rs:386` | `wal_consumer.rs:764-798` (test use) | none |
| `pub const DEFAULT_QUEUE_DEPTH` | `broker.rs:148` | `broker.rs:414` only | none |

  Why: The reason `broker` had to stay `pub` is the four exact items the
  revert listed in the comment at `lib.rs:36-37` (`ChangeOp`,
  `SubscriptionMessage`, `ChangeEvent`, `subscribe`, `drop_app`, plus
  `live_subscription_count` for the finalizer test). The remaining six
  expand the nominal stable API for no callers. `ws_frame_for_change` /
  `ws_frame_for_control` / `ws_frame` are particularly suspect — there's
  no production WS code in the repo that consumes them
  (`crates/runtime/src/core/serve.rs` rolls its own `write_ws_frame` at
  `serve.rs:845`).
  Fix: Demote each to `pub(crate)`. If `ws_frame*` truly has no consumer,
  delete them and their tests (~50 LOC of dead WS frame builders).
  Verification: `grep -rn "broker::publish\b\|broker::ws_frame\|broker::Broker\b\|DEFAULT_QUEUE_DEPTH" crates --include='*.rs'` outside `crates/plugin-db/src/broker.rs` → only `wal_consumer.rs:81` (uses `publish`) and `wal_consumer.rs:764-798` (test imports `Broker`). No callers in `crates/runtime`, `crates/gateway`, or any non-plugin-db `tests/`.

---

**I5 — `backend::BackendHandle` is `pub` but unused outside the trait impl tests** (`crates/plugin-db/src/backend/mod.rs:364`)

`pub type BackendHandle = Rc<PostgresBackend>;` — declared as the
"opaque trait-object handle that the per-isolate context stores" in
its doc, but `context.rs:158` stores `Option<Rc<PostgresBackend>>`
directly, not `Option<BackendHandle>`. Compiler emits warning
`type alias 'BackendHandle' is never used` in `cargo check --tests`.
The only references are its own definition + a trait-shape
compile-time test (`backend/mod.rs:415, 432`).

  Why: Dead nominal API alias. The docstring promises a "type-erased
  stash" that the implementation never actually opted into.
  Fix: Either thread `BackendHandle` through `IsolateDbContext`'s
  `backend` field (the doc-stated intent) or delete the alias. Demote
  to `pub(crate)` at minimum.
  Verification: `grep -rn "BackendHandle" crates` → 3 hits, all inside
  `backend/mod.rs`. Compiler warning `type alias 'BackendHandle' is never used`.

---

### MINOR

**M1 — `QueryError` is `pub enum` without `#[non_exhaustive]`** (`crates/plugin-db/src/query.rs:20`)

R1 M3 recommended `#[non_exhaustive]` for `DbError` — applied
(`error.rs:40`). `QueryError` has the same shape (public enum, three
public variants, `From<QueryError> for DbError` impl carries them
across module boundaries) and the same forward-compat concern: any
external matcher that exhaustively branches on `QueryError` would
break on a new builder-side error class (e.g. `InvalidOperator`,
`InvalidProjection`).

  Why: Cheap futureproofing; consistency with `DbError`. With `query` now
  `pub` (C1 above), `QueryError` is in fact reachable from external
  callers (e.g. `tests/integration.rs:140` uses `use
  zeroship_plugin_db::query::*`).
  Fix: Add `#[non_exhaustive]` to the enum declaration.
  Verification: `crates/plugin-db/src/query.rs:20`; matching `From`
  impl at `crates/plugin-db/src/error.rs:340-354`.

---

**M2 — `v8_classes` submodules `migration`, `migrations`, `replication`, `transaction` are `pub mod` despite zero external use** (`crates/plugin-db/src/v8_classes/mod.rs:32-36`)

The parent module is `pub` (`lib.rs:54`) because `tests/db_v8_class.rs`
and `tests/subscription_finalizer.rs` import
`v8_classes::{db, collection, subscription}`. None of the four
remaining submodules (`migration`, `migrations`, `replication`,
`transaction`) is referenced by anyone outside `crates/plugin-db/src`.
Their mint functions (`mint_transaction`, `mint_migrations`,
`mint_replication`, `migration_start_with_spec`) are called only by
sibling files inside `v8_classes/`.

  Why: Inflates the externally-reachable API of `v8_classes` for no
  benefit — every external `use` site can be satisfied with the three
  modules the tests actually touch. Demoting the other four narrows
  the doc-page surface and stops `cargo doc` from indexing internal
  mint helpers.
  Fix: Change `pub mod migration; pub mod migrations; pub mod
  replication; pub mod transaction;` to `pub(crate) mod …`. Verify the
  external test files still build afterwards.
  Verification: `grep -rn "v8_classes::\(migration\|migrations\|replication\|transaction\)" crates --include='*.rs'` outside `crates/plugin-db/src/v8_classes/` → zero matches.

---

**M3 — `v8_classes::collection::mint_collection` is `pub` but only called inside `v8_classes`** (`crates/plugin-db/src/v8_classes/collection.rs:353`)

`mint_collection` is used by `v8_classes/db.rs:130` and
`v8_classes/transaction.rs:185`. No external caller. `Collection` (the
struct) is needed externally (`tests/db_v8_class.rs:20`); the minting
helper is not.

  Why: Minor nominal-surface bloat. The mint function's signature
  (`scope`, `name`, `app_id`) is a v8-internal contract that should
  not be part of the documented surface.
  Fix: `pub(crate) fn mint_collection(...)`. Cheap.
  Verification: `grep -rn "mint_collection" crates --include='*.rs'` → only `v8_classes/db.rs:43,130` and `v8_classes/transaction.rs:55,185`.

---

**M4 — `audit::read_processed_from_audit_row` / `read_dead_letter_pks_from_audit_row` are `pub` despite only internal use** (`crates/plugin-db/src/audit.rs:463, 478`)

Two row-decode helpers exposed as `pub fn`. They are called from
`audit.rs` itself + the read-set/diff layer, all crate-internal.
Bundle them into the `pub(crate) trait AuditExecutor` extension or
demote.

  Why: Bloats the audit module surface with implementation-detail
  decode helpers.
  Fix: `pub(crate) fn`. Same pattern that was applied to `AuditExecutor`
  itself.
  Verification: `grep -rn "read_processed_from_audit_row\|read_dead_letter_pks_from_audit_row" crates --include='*.rs'` → callers all inside `crates/plugin-db/src/`.

---

**M5 — Module doc on `orchestrator/mod.rs:20-22` claims "Each submodule is `pub(crate)`" but the code says `pub mod`** (`crates/plugin-db/src/orchestrator/mod.rs:26-28`)

Cosmetic — `mod orchestrator` is `pub(crate)` (per C1, that's wrong but
let's set that aside), so `pub mod` inside is effectively `pub(crate)`.
The doc is misleading regardless. Either change the declarations to
`pub(crate) mod` or amend the doc to say "the submodule declarations
are `pub` but the parent is `pub(crate)` so the effective visibility
is crate-internal."

  Why: Doc/code drift hides intent from future maintainers.
  Fix: One-line edit to either side.
  Verification: `crates/plugin-db/src/orchestrator/mod.rs:20-28`.

---

**M6 — `pub fn init_pool_async` has no external callers, but it is the documented bootstrap entry point** (`crates/plugin-db/src/lib.rs:301`)

`init_pool_async` is `pub` and the lib-level doc + the function's own
docstring (`lib.rs:287-300`) treat it as the entry point for embedding
this plugin into a Runtime. In practice no external crate calls it —
both `crates/worker/src/cache.rs:42` and `crates/cli/src/main.rs:103`
just register `DbPlugin::new(url)` and let the orchestrator's
`exec.rs:62-66` / `register_model/mod.rs:117-122` lazy-init the pool
on the first JS call.

  Why: Either the doc is stale (no caller needs to invoke it manually)
  or the entry point is missing (worker / cli should be calling it
  during startup so the first JS request doesn't pay the connect
  latency). Worth a one-paragraph decision recorded somewhere.
  Fix: Pick one — either delete the function (lazy init covers
  everything) or wire it into worker startup. Don't leave the docs
  promising an entry point nobody uses.
  Verification: `grep -rn "init_pool_async" crates --include='*.rs'` → all callers are inside `crates/plugin-db/`.

---

## R1 → R2 status

| R1 finding | Severity | Status at HEAD `d2e7e22` | Evidence |
|---|---|---|---|
| C1 audit.rs returns `Result<_, String>` | CRITICAL | **Fixed** | `audit.rs:192,277,294,338,541,568,698,725,760` all return `DbError` |
| C2 `Backend::create_index_with_recovery` returns `Result<(), String>` | CRITICAL | **Fixed** | `backend/mod.rs:347-355` returns `Result<(), DbError>` |
| I1 `register_model_dispatch` flattens to `String` | IMPORTANT | **Fixed** | `register_model/mod.rs:95-98` uses `RejectError(e.to_op_error())` |
| I2 `replication_ops.rs` dispatches flatten to `String` | IMPORTANT | **Fixed** | `replication_ops.rs:69-87, 102-122, 134-160, 184-272` wrap in `DbError::Internal { … }.to_op_error()` |
| I3 `IsolateDbContext` fields `pub(crate)` | IMPORTANT | **Not addressed** (carried as r2 I2) | `context.rs:70-158` |
| I4 `AuditExecutor` trait `pub` | IMPORTANT | **Fixed** | `audit.rs:431` is `pub(crate) trait` |
| I5 `_pub`-suffixed helpers | IMPORTANT | **Fixed** | `query.rs:380-386, 2084-2086` renamed to canonical, `pub(crate)` |
| I6 `migrations.rs::exec_*` unconditional `pub` | IMPORTANT | **Fixed** | `migrations.rs:732-819` are `#[cfg(any(test, feature="test-helpers"))] #[doc(hidden)] pub` |
| M1 `wal_consumer` legacy shims | MINOR | **Not addressed** (carried as r2 I3) | `wal_consumer.rs:172,184` |
| M2 `diff.rs` returns `Result<_, String>` | MINOR | Partially carried by C1's `Backend` trait wrap; `diff.rs` itself still has bare `Result<_, String>` at lines 184/355/375 | Re-confirm in a future pass |
| M3 `DbError` lacks `#[non_exhaustive]` | MINOR | **Fixed** | `error.rs:40` |

New in r2:
- C1 (regression — integration tests do not compile)
- I1 (auto_tx still flattens to string — same class of leak that I1 fixed for register_model)
- I4 / I5 (dead `pub` items inside broker and `BackendHandle`)
- M1–M6 (smaller polish items)

---

## Score

**74 / 100** (vs r1 `61 / 100`)

The big architectural rails — `DbError` typing, `Backend` trait error
contract, `register_model` dispatch — landed cleanly and lifted the
score by ~13 points. What holds it back: a CRITICAL regression that
shipped because CI doesn't exercise the `--features test-helpers`
target, plus three carried-over r1 findings (`IsolateDbContext` fields,
`wal_consumer` legacy shims, `auto_tx` still flattens). The bulk of
the round-1 "over-`pub` modules" complaint is materially resolved (only
six items remain `pub` for genuine external-test reasons), but the
revert was undersized and silently broke the integration test build.

Top three fixes, in order:

1. C1 — re-promote the six modules and add a CI step that builds the
   `test-helpers` feature. Without this, the integration suite is
   non-functional.
2. I1 — switch the two `auto_tx` dispatchers to `RejectError(e.to_op_error())`.
3. I2 — drop `pub(crate)` from `IsolateDbContext` fields so the type
   enforces its own invariants.
