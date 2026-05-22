# plugin-db API surface — round 4 (2026-05-22)

Scope: `crates/plugin-db/` audited for unintended public surface area.

**Prior round:** r3 — 81/100 (cycle 02:17). Re-audit takes the new
`lock_guard.rs` module + the recent demotes (`wal_consumer` shims,
auto_tx error rail) into account.

**Methodology:** mechanical sweep with `Grep`, then dead-code
cross-check with `cargo check -p zeroship-plugin-db --tests
--features=test-helpers`, plus call-site verification from the four
external test crates (`tests/auto_tx.rs`, `tests/capability.rs`,
`tests/db_v8_class.rs`, `tests/integration.rs`, `tests/subscription_finalizer.rs`).

The 8 audit dimensions follow.

---

## 1. `lock_guard.rs` — new pub surface

`crates/plugin-db/src/orchestrator/mod.rs:29` declares the module as
`pub(crate)`. Inside, the entire surface is `pub(crate)`:

| Item | Visibility | Reachable from outside `orchestrator/`? |
| --- | --- | --- |
| `OrchestratorLockGuard<'p>` struct | `pub(crate)` | No |
| `acquire()` async fn | `pub(crate)` | No |
| `release()` async fn | `pub(crate)` | No |
| `into_held()` fn | `pub(crate)` (+ `#[allow(dead_code)]`) | No |
| `Drop for OrchestratorLockGuard<'_>` | impl | No |
| `for_test_no_client` (test ctor) | `fn` in `#[cfg(test)] mod tests` | No |

The module declaration in `orchestrator/mod.rs` is `pub(crate) mod lock_guard;`
(only `lock_guard` line uses this stricter form — the other three are
`pub mod auto_tx`, `pub mod register_model`, `pub mod transaction`).
Net: zero new external surface introduced by `cbd12944`. Clean.

**No finding.** This is exemplary for new orchestrator infrastructure
— pattern to copy when extracting future RAII helpers.

One sub-observation worth flagging:

[I-LG1] `into_held()` is dead code in release builds.
  File: `crates/plugin-db/src/orchestrator/lock_guard.rs:144`
  Why: `#[allow(dead_code)]` was attached prospectively for a "future
  caller that needs to thread the locked client into an API that doesn't
  accept the guard type". A grep across `crates/` confirms zero
  non-test call sites. Carrying dead code with `#[allow]` is mild API
  surface bloat: the symbol is `pub(crate)` so it can't leak externally,
  but future readers may treat it as load-bearing.
  Fix: leave it for one more round, OR delete + reintroduce when an
  actual caller arrives. (Low priority — this kind of forward-looking
  RAII helper is fine to keep documented and dormant.)
  Verification: `Grep "into_held" crates/` → only the file itself + 3
  doc/test references.

---

## 2. `wal_consumer` demotion verification (5ceb6daa)

The three shims are now correctly `pub(crate)`:

| Item | Visibility | Used by |
| --- | --- | --- |
| `any_app_suppressed` | `pub(crate) fn` + `#[doc(hidden)]` | **Nobody** (dead) |
| `set_local_emit_suppressed` | `pub(crate) fn` + `#[doc(hidden)]` | In-file `#[cfg(test)]` test only |
| `local_emit_suppressed` | `pub(crate) fn` + `#[doc(hidden)]` | In-file `emit_local()` at line 216 |

`cargo check -p zeroship-plugin-db --tests --features=test-helpers`
emits two dead-code warnings:

```
warning: function `any_app_suppressed` is never used
   --> crates/plugin-db/src/wal_consumer.rs:131:15
warning: function `set_local_emit_suppressed` is never used
   --> crates/plugin-db/src/wal_consumer.rs:173:15
```

This confirms the demotion goal (no external leak) but uncovers a
**new finding**:

[L1] `wal_consumer::any_app_suppressed` is dead code.
  File: `crates/plugin-db/src/wal_consumer.rs:130-133`
  Why: surface impact is zero now (it's `pub(crate)`), but the symbol
  is also unused — even by the production `emit_local` path that the
  doc-comment claims it serves ("Diagnostic helper — the production
  code path always checks a specific app"). Carrying `#[doc(hidden)]
  pub(crate) fn` items that no caller touches is dead surface that
  costs review attention on every audit.
  Fix: delete it, OR `#[allow(dead_code)]` with a comment promising a
  future consumer. The compiler-warning telemetry is already pointing
  at this.
  Verification: `Grep "any_app_suppressed" crates/` → only declaration
  site (line 131).

[L2] `wal_consumer::set_local_emit_suppressed` is dead in the lib build.
  File: `crates/plugin-db/src/wal_consumer.rs:172-179`
  Why: only consumed by `emit_local_legacy_thread_wide_flag_still_works`
  in the same file's `#[cfg(test)] mod tests` block (line 849, 852). The
  lib build (excluding tests) sees it as unused — cargo's
  `unused-functions` lint fires (see warning above). This is the
  legacy thread-wide flag whose direct production callers were
  removed; the only thing exercising it is its own test for legacy
  back-compat semantics.
  Fix: wrap in `#[cfg(any(test, feature = "test-helpers"))]` so the
  warning disappears AND the symbol genuinely doesn't ship in
  release. Alternatively, if the test is the only reason for the
  shim's existence at this point, delete the shim + the test together.
  Verification: `cargo check -p zeroship-plugin-db --tests
  --features=test-helpers` (warning text quoted above); `Grep
  "set_local_emit_suppressed" crates/` → only the declaration + the
  in-file test that calls it.

[L3] `wal_consumer::local_emit_suppressed` survives because `emit_local`
  still calls it at line 216 (`if is_app_suppressed(app_id) ||
  local_emit_suppressed() { return; }`).
  Status: **Correctly retained.** Verification: `Grep
  "local_emit_suppressed\b" crates/plugin-db/src` → declaration + the
  one production caller.

Net: 5ceb6daa achieved its surface-tightening goal, but it left two of
the three shims dead. Bonus low-priority cleanup available.

---

## 3. `broker.rs` — `has_subscribers` surface

`Broker::has_subscribers(&self, app_id: &str, collection: &str) -> bool`
is `pub fn` on a `pub struct Broker`, in a `pub mod broker`.

External reachability:

- `crates/plugin-db/src/lib.rs:49` — `pub mod broker;` (always pub —
  external test crates `tests/subscription_finalizer.rs:24` reach
  `zeroship_plugin_db::broker`).
- Internal callers: `wal_consumer.rs:81` uses the free-function
  thread-local accessor `has_subscribers(app_id, collection)` (line 611,
  `pub(crate) fn`).
- The method on `Broker` itself is `pub fn` — JS / external callers
  *could* construct a `Broker` and call it, but `BROKER` is
  thread-local + `pub(crate)`, so the externally-reachable lever is
  only via owning a fresh broker.

[I-BR1] `Broker::has_subscribers` method is `pub` but only the
free-function wrapper is needed.
  File: `crates/plugin-db/src/broker.rs:460-468`
  Why: surface impact is small (external callers gain a probe on
  their own broker instance — harmless), but tightening to
  `pub(crate)` matches the free-function `pub(crate) fn has_subscribers(...)`
  at line 611. Consistency principle: if the THREAD-LOCAL accessor is
  crate-private (because callers should only probe the canonical
  broker, not random ones), the method should match.
  Counter-argument: tests/subscription_finalizer.rs already reaches
  the `Broker` type via `zeroship_plugin_db::broker`. A future test
  may want a method-level probe.
  Fix: leave `pub` as-is OR add a one-line doc-comment "Method exposed
  for tests; production callers use the thread-local wrapper." Low
  priority.
  Verification: the type signature already accepts `&str` (no
  `String` alloc), which was the actual ergonomic concern in r3.

The two-level HashMap rework (0e58c4e8) is internal; the public
signature `(&self, &str, &str) -> bool` is the same. **No surface
regression from the rework.**

---

## 4. `error.rs` — `DbError` variants since r3

Sweep against r3 baseline (10 variants):

| Variant | Status | OpError code |
| --- | --- | --- |
| `SchemaRefused` | unchanged | (carries static code) |
| `ValidationFailed` | unchanged | (carries static code) |
| `UniqueViolation` | unchanged | `unique_violation` |
| `FkViolation` | unchanged | `fk_violation` |
| `NotNullViolation` | unchanged | `not_null_violation` |
| `CheckViolation` | unchanged | `check_violation` |
| `Serialization` | unchanged | `serialization_failure` (hinted) |
| `LockContention` | unchanged | `lock_not_available` (hinted) |
| `Transient` | unchanged | `transient` (hinted) |
| `Configuration` | unchanged | (carries static code) |
| `Coded` | unchanged | (passthrough) |
| `Internal` | unchanged | `internal` |

The enum still carries `#[non_exhaustive]` (line 51) — adding a
variant remains source-compatible at the SDK boundary because
`to_op_error()` always materialises a `CodedError`. Two trait impls
exist (`From<compio_postgres::Error>`, `From<crate::query::QueryError>`)
that route through the canonical classifier — both unchanged.

**No regressions, no new leaks.** The 8ff1b2de commit ("preserve typed
DbError code through OpResult::Failed") was a callsite change in
`orchestrator/auto_tx.rs`, not in `error.rs`. The error type itself is
stable.

---

## 5. `#[doc(hidden)] pub fn` sweep — total count

A fresh sweep finds 18 `#[doc(hidden)]` annotations:

| File | Lines | Visibility | Count |
| --- | --- | --- | --- |
| `migrations.rs` | 773, 787, 799, 828, 840, 852 | `cfg(any(test, feature="test-helpers"))` + `#[doc(hidden)] pub` | 6 |
| `replication_ops.rs` | 279, 287 | same | 2 |
| `wal_consumer.rs` | 130, 166, 172, 184 | **mixed**: 130/166 are doc(hidden) + `pub(crate)` (post-5ceb6daa); 172/184 are doc(hidden) + `pub(crate)` | 4 |
| `lib.rs` | 209, 237, 261, 298, 313, 322, 330 | `cfg(any(test, feature="test-helpers"))` + `#[doc(hidden)] pub` | 7 |
| `exec.rs` | 333 | `cfg(any(test, feature="test-helpers"))` + `#[doc(hidden)] pub` | 1 |

Total `#[doc(hidden)]` annotations: 20. Of these:
- 16 are correctly cfg-gated test helpers (`#[cfg(...)] #[doc(hidden)] pub fn`)
- 4 are in `wal_consumer.rs` and are `pub(crate)` post-5ceb6daa (so
  the `#[doc(hidden)]` is now redundant — `pub(crate)` already hides
  from rustdoc, but it's harmless).

The constant `LEGACY_SUPPRESSION_KEY` at `wal_consumer.rs:167` is
`const` (no visibility modifier needed at file scope — defaults to
private). The `#[doc(hidden)]` there is also vacuous.

[L4] Redundant `#[doc(hidden)]` on `pub(crate)` items in `wal_consumer.rs`.
  File: `wal_consumer.rs:130, 166, 172, 184`
  Why: `pub(crate)` items don't appear in rustdoc by default. The
  `#[doc(hidden)]` attribute is now harmless noise (it was meaningful
  pre-5ceb6daa when the items were `pub`). Trivial cleanup.
  Fix: drop the four `#[doc(hidden)]` annotations on `pub(crate)`
  items (and the one on the private `const LEGACY_SUPPRESSION_KEY`).
  Verification: `Grep "#\[doc\(hidden\)\]" crates/plugin-db/src/wal_consumer.rs`
  → four hits, each one line above a `pub(crate)` item.

**No new doc-hidden `pub` leaks** — the lib.rs / migrations.rs /
replication_ops.rs / exec.rs sites are all correctly cfg-gated test
helpers.

---

## 6. Cfg-fork test-helpers visibility

Sweep against lib.rs:62-101 (8 cfg-forked modules):

```
audit, auth, exec, migrations, orchestrator, replication,
replication_ops, wal_consumer
```

Each follows the pattern:

```rust
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod <name>;
#[cfg(feature = "test-helpers")]
pub mod <name>;
```

Spot-checked all 8: pattern matches verbatim. No drift between modules.

**Always-pub modules** (lib.rs:49-52): `broker`, `error`, `query`,
`v8_classes`. These are reached by external test crates without the
`test-helpers` feature:

- `broker` — `tests/subscription_finalizer.rs:24`, `tests/integration.rs:4242`
- `error` — only via `DbError` re-flow from internal helpers (no
  direct external use)
- `query` — `tests/integration.rs:140` (`use zeroship_plugin_db::query::*;`)
- `v8_classes` — `tests/db_v8_class.rs:20-21`, `tests/subscription_finalizer.rs:25`

[I-CFG1] `error` module is `pub` unconditionally but no external
test crate imports it directly.
  File: `crates/plugin-db/src/lib.rs:50`
  Why: `pub mod error;` is justified by docs ("reached even without
  the feature — see tests/subscription_finalizer.rs and
  tests/db_v8_class.rs"), but a grep of those tests shows neither
  imports `zeroship_plugin_db::error::DbError` directly. The
  `DbError` type does appear in re-exports via other `pub` types
  (e.g., `DbError` as a `From` target on the always-pub `query::QueryError`),
  but no test file does `use zeroship_plugin_db::error::...`.
  Fix: could move to the cfg-fork like the others (`pub(crate)` in
  release, `pub` under `test-helpers`). The `error::DbError` is
  reachable via inherent impls on `query::QueryError` either way (the
  `From` impl is a tag, not a name binding). Tighten in a follow-up
  unless an external consumer materialises.
  Verification: `Grep "use zeroship_plugin_db::error" .` → zero hits
  outside `docs/`.

Net dimension grade: **A**. The cfg-fork pattern is consistent. One
module (`error`) is unconditionally `pub` without evident need.

---

## 7. `pub use` re-exports

Found two `pub use` sites:

```
crates/plugin-db/src/auth/mod.rs:68: pub use bootstrap::{ensure_admin_schema, BootstrapOutcome};
crates/plugin-db/src/auth/mod.rs:69: pub use keys::{rotate_session_keys, RotationOutcome};
crates/plugin-db/src/auth/mod.rs:70: pub use session::{init_session, mint_session_token, MintedToken, SessionInit};
crates/plugin-db/src/backend/mod.rs:50: pub use postgres::PostgresBackend;
```

Both parent modules (`auth`, `backend`) are `pub(crate)` in the release
build. `auth` flips to `pub` under `test-helpers`; `backend` does NOT
flip (always `pub(crate)`).

- `auth::*` items (`ensure_admin_schema`, `BootstrapOutcome`,
  `rotate_session_keys`, `RotationOutcome`, `init_session`,
  `mint_session_token`, `MintedToken`, `SessionInit`) — externally
  reachable ONLY in `test-helpers` builds. Confirmed clean.
- `backend::PostgresBackend` — externally unreachable (parent
  `pub(crate)`). The dead-code warning for `BackendHandle` (line 364)
  confirms it's not pulled out. Clean.

[no finding here]

The `v8_classes/mod.rs` declares `pub mod {collection, db, migration,
migrations, replication, subscription, transaction}` (lines 32-38) but
has **no `pub use` re-exports** — every mint helper is named via its
submodule path. Clean separation.

---

## 8. `mint_*` helpers — visibility audit

Six mint helpers; calls and visibility:

| Helper | File | Visibility | External callers? | Internal callers |
| --- | --- | --- | --- | --- |
| `mint_db` | `v8_classes/db.rs:367` | `pub` | **Yes** — `tests/db_v8_class.rs:21, 32, 63` | `lib.rs:174` |
| `mint_subscription` | `v8_classes/subscription.rs:157` | `pub` | **Yes** — `tests/subscription_finalizer.rs:25, 54, 100` | `v8_classes/db.rs:223`, `v8_classes/collection.rs:337` |
| `mint_collection` | `v8_classes/collection.rs:353` | `pub` | **No** | `v8_classes/db.rs:43, 130`, `v8_classes/transaction.rs:55, 185` |
| `mint_transaction` | `v8_classes/transaction.rs:288` | `pub` | **No** | `orchestrator/transaction.rs:55` |
| `mint_migrations` | `v8_classes/migrations.rs:251` | `pub` | **No** | `v8_classes/db.rs:288` |
| `mint_replication` | `v8_classes/replication.rs:117` | `pub` | **No** | `v8_classes/db.rs:263` |

Plus a sibling that fits the same pattern:

| `migration_start_with_spec` | `v8_classes/migration.rs:598` | `pub` | **No** | `v8_classes/migrations.rs:79` |

**M1 — Four mint helpers + one start-with-spec are `pub` but only used in-crate.**
  Files:
  - `crates/plugin-db/src/v8_classes/collection.rs:353` (`mint_collection`)
  - `crates/plugin-db/src/v8_classes/transaction.rs:288` (`mint_transaction`)
  - `crates/plugin-db/src/v8_classes/migrations.rs:251` (`mint_migrations`)
  - `crates/plugin-db/src/v8_classes/replication.rs:117` (`mint_replication`)
  - `crates/plugin-db/src/v8_classes/migration.rs:598` (`migration_start_with_spec`)
  Why: each appears in the public surface of the always-pub `v8_classes`
  module. An external consumer could call e.g. `mint_collection(scope,
  "users".into(), "attacker_app".into())` and get a JS object that, if
  attached to a tenant's `env.db`, performs CRUD on attacker_app's
  schema. The Rust signature accepts an `app_id` directly — no
  tenant-isolation gate at the function boundary (the v8_class IDL
  surface is the gate; bypassing it bypasses the gate).
  In practice the worker doesn't expose these to JS — they live behind
  `DbPlugin::register()`. But the principle "minimum public surface
  for security-sensitive minters" applies: each `pub fn mint_*` that
  isn't reached by a test crate should be `pub(crate)`.
  This is the same finding as r2 (M3) and r3 — still unaddressed.
  Fix: demote to `pub(crate)`:
  ```rust
  // v8_classes/collection.rs:353
  pub(crate) fn mint_collection<'s>(...) { ... }
  // v8_classes/transaction.rs:288
  pub(crate) fn mint_transaction<'s>(...) { ... }
  // v8_classes/migrations.rs:251
  pub(crate) fn mint_migrations<'s>(...) { ... }
  // v8_classes/replication.rs:117
  pub(crate) fn mint_replication<'s>(...) { ... }
  // v8_classes/migration.rs:598
  pub(crate) fn migration_start_with_spec<'s>(...) { ... }
  ```
  Leave `mint_db` (tests/db_v8_class.rs) and `mint_subscription`
  (tests/subscription_finalizer.rs) as `pub` — they are the genuine
  external consumers.
  Verification:
  ```
  Grep "mint_collection\b" crates/  → only declaration + 4 in-crate sites
  Grep "mint_transaction\b" crates/ → only declaration + orchestrator/transaction.rs:55
  Grep "mint_migrations\b" crates/  → only declaration + v8_classes/db.rs:288
  Grep "mint_replication\b" crates/ → only declaration + v8_classes/db.rs:263
  Grep "migration_start_with_spec\b" crates/ → only declaration + v8_classes/migrations.rs:79
  ```

This was scored at M3 in r2 (88 → 81), called out in r3, and still
not addressed by any of the four most recent commits.

---

## Bonus dead-code surface (compiler-detected)

`cargo check -p zeroship-plugin-db --tests --features=test-helpers`
surfaces these unused-`pub` warnings beyond the wal_consumer shims:

```
crates/plugin-db/src/backend/mod.rs:364:10
  type alias `BackendHandle` is never used
  → `pub type BackendHandle = Rc<PostgresBackend>;`
  Parent `pub(crate)` → not an external leak; internal dead code.

crates/plugin-db/src/diff.rs:375:14
  function `count_violating_not_null` is never used
  → `pub async fn count_violating_not_null(...)` in `pub(crate) mod diff`
  Internal dead code; not a leak.

crates/plugin-db/src/v8_bridge.rs:274:15
  function `setup_promise` is never used
  → `pub(crate) fn setup_promise(...)`
  Internal dead code; not a leak.

crates/plugin-db/src/migrations.rs:755:8
  function `release_active_lock` is never used
  → `pub fn release_active_lock()` in cfg-forked module
  In the `pub mod migrations` (test-helpers) build, this is externally
  reachable but only consumed by `crate::clear_migration_lock_for_tests`
  (lib.rs:247) which is itself `cfg(any(test, feature="test-helpers"))`.
  In a release build it's `pub(crate)` (cfg-fork) so still not leaked.
  Borderline: technically reachable in test-helpers feature, technically
  unused at link time. Mild surface bloat.

crates/plugin-db/src/replication.rs:506:14
  function `slot_status` is never used
  → `pub async fn slot_status(...)` in cfg-forked module
  Same shape as `release_active_lock`: under `test-helpers` this is
  externally reachable AND unused. Under release `pub(crate)` AND
  unused.
```

[L5] Two cfg-forked `pub fn`s are dead even in the `test-helpers` build.
  Files:
  - `crates/plugin-db/src/migrations.rs:755` — `release_active_lock`
  - `crates/plugin-db/src/replication.rs:506` — `slot_status`
  Why: external `test-helpers` builds expose these symbols, but
  nothing — neither internal code nor any test crate — calls them.
  Fix: delete each, OR keep + `#[allow(dead_code)]` if a future test
  is planned. `slot_status` doc-string ("Cheap probe used by the V8
  `replicationStatus` callback") suggests the callback was deleted in
  some prior refactor and the helper was left behind.
  Verification: `cargo check -p zeroship-plugin-db --tests
  --features=test-helpers` produces the warnings quoted above.
  `Grep "slot_status\b\|release_active_lock\b" crates/` →
  declarations + the `clear_migration_lock_for_tests` consumer (which
  is also cfg-gated and never actually called).

---

## Summary table — surface diff vs r3

| Dimension | r3 verdict | r4 verdict |
| --- | --- | --- |
| 1. `lock_guard` new pub surface | (new) | Clean — entire module `pub(crate)`; one dead-code helper (`into_held`) flagged L-prio |
| 2. `wal_consumer` shim demotion | I3 (pub) | Resolved (pub→pub(crate)). New L1/L2 dead-code on `any_app_suppressed` + `set_local_emit_suppressed`. |
| 3. `broker::has_subscribers` | (pre-existing pub) | Method `pub`, free-function `pub(crate)` — minor consistency nit (I-BR1) |
| 4. `DbError` variants | 12 variants | Unchanged: 12 variants, `#[non_exhaustive]`, no new leaks |
| 5. `#[doc(hidden)] pub fn` count | ~14 cfg-gated | 16 cfg-gated + 4 redundant (`pub(crate)` after demote) |
| 6. Cfg-fork visibility | 8 modules consistent | Still 8, still consistent. One module (`error`) unconditionally `pub` without evident external use |
| 7. `pub use` re-exports | clean | Still clean (auth + backend, both behind `pub(crate)` parents in release) |
| 8. `mint_*` helpers | M3 (4 over-exposed) | M1 — still 4 mint helpers + `migration_start_with_spec` are unjustifiably `pub` |

---

## Findings (consolidated)

```
[M1]  v8_classes/{collection,transaction,migrations,replication,migration}: 5 pub fn helpers should be pub(crate)
  Why: in the always-pub v8_classes module, these mint helpers accept
       app_id as a parameter — bypassing JS IDL gating.
  Fix: demote to pub(crate). Keep mint_db + mint_subscription pub.
  Verification: see dimension 8 table.

[I-BR1] broker::Broker::has_subscribers method is pub but only the free-fn wrapper is needed
  Why: minor consistency with the pub(crate) free-fn at broker.rs:611
  Fix: demote method to pub(crate), OR leave with doc comment
  Verification: Grep "has_subscribers" crates/

[I-CFG1] error module is pub unconditionally with no direct external use
  Why: could join the cfg-fork (pub(crate) in release)
  Fix: gate behind test-helpers feature like the other 8
  Verification: Grep "use zeroship_plugin_db::error" .

[I-LG1] OrchestratorLockGuard::into_held is #[allow(dead_code)] forward-looking helper
  Why: pub(crate) + zero callers; carries forward-looking documentation
  Fix: leave OR delete-and-reintroduce when first caller arrives
  Verification: Grep "into_held" crates/

[L1] wal_consumer::any_app_suppressed is dead code
  Why: pub(crate), zero callers; compiler emits unused warning
  Fix: delete OR #[allow(dead_code)] + doc
  Verification: cargo check --tests --features=test-helpers

[L2] wal_consumer::set_local_emit_suppressed is dead in lib build
  Why: only the in-file #[cfg(test)] test calls it
  Fix: cfg-gate the symbol behind test/test-helpers OR delete both
  Verification: cargo check warning + Grep

[L3] wal_consumer::local_emit_suppressed — correctly retained (used in emit_local)

[L4] wal_consumer.rs: 4 redundant #[doc(hidden)] attributes on pub(crate) items
  Why: pub(crate) already hides from rustdoc; #[doc(hidden)] is no-op
  Fix: drop the four attributes
  Verification: Grep "#\\[doc\\(hidden\\)\\]" wal_consumer.rs

[L5] migrations::release_active_lock + replication::slot_status are dead in both release AND test-helpers
  Why: cfg-fork makes them pub under test-helpers; no caller exists
  Fix: delete OR add caller
  Verification: cargo check warnings
```

---

## Score

**85 / 100** (+4 vs r3's 81)

Improvement drivers:
- `lock_guard.rs` ships entirely `pub(crate)` (no new leaks)
- `wal_consumer` shims demoted `pub → pub(crate)` (I3 from r3 resolved)
- Auto-tx error rail tightening (8ff1b2de) doesn't widen any surface
- Cfg-fork pattern across 8 modules remains consistent and verified

Held back from higher score:
- **M1 unchanged from r3** — 5 mint helpers + `migration_start_with_spec`
  in always-pub `v8_classes` are still externally reachable with no
  external caller. This is the single highest-impact finding.
- Three new low-priority dead-code symbols surfaced (L1, L2, L5) that
  the recent demote cleanups introduced or left behind.
- Minor doc-hygiene drift (L4) from the demote.

Score sub-ranges:
- 90+ would require: M1 demoted + L1/L2/L5 cleaned up + I-CFG1 acted on
- 95+ would require: also tighten I-BR1 + remove I-LG1's `#[allow(dead_code)]`

Highest-leverage next move: a one-commit `pub → pub(crate)` sweep on the
5 mint helpers — closes M1 entirely.
