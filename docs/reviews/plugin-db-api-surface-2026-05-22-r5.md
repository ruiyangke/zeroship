# plugin-db API surface — round 5 (2026-05-22)

Scope: `crates/plugin-db/` audited for unintended public surface area.

**Prior round:** r4 — 85/100 (cycle 03:25). This re-audit folds in the
six commits landed after r4: `0049d9be` + `91830cca` (the [I28]
`Result<_, String>` → `Result<_, DbError>` sweep across `auth/*` +
`replication.rs`), `bd1e7ce1` (lock_guard release reorder), `808a32af`
(`OrchestratorLockGuard` `#[must_use]`), `ffb1e101` (unlock-SQL
`tracing::warn`), `c0590506` (replication watchdog / dropAbandoned
cross-app scope tightening — CRITICAL security), and `eda96ead`
(`first_row_or_internal` helper extraction).

**Methodology:**
- Mechanical `Grep` sweep for `^pub`, `^pub use`, `pub fn`, `#[doc(hidden)]`,
  `#[must_use]`, and `pub(crate)` patterns.
- `cargo check -p zeroship-plugin-db --lib` (no features) →
  warnings list.
- `cargo check -p zeroship-plugin-db --tests --features=test-helpers`
  → integration / lib-test compile.
- External test-crate call-site verification:
  `tests/auto_tx.rs`, `tests/capability.rs`, `tests/db_v8_class.rs`,
  `tests/integration.rs`, `tests/subscription_finalizer.rs`.

The 8 audit dimensions follow.

---

## 1. `#[must_use]` on `OrchestratorLockGuard` (808a32af)

Source: `crates/plugin-db/src/orchestrator/lock_guard.rs:66-68`.

```rust
#[must_use = "OrchestratorLockGuard must be released via .release().await or .into_held(); \
              dropping it leaks the session-scoped advisory lock"]
pub(crate) struct OrchestratorLockGuard<'p> { … }
```

**Cascade analysis:** the `#[must_use]` attribute applies to a
`pub(crate)` type. Rust only warns on a dropped `must_use` value at the
call site that *binds* it (e.g. `let _ = OrchestratorLockGuard::acquire(...)`
or a bare expression-statement that returns the guard). Three call
sites in this crate touch the type:

| File | Line | Usage | Cascades cleanly? |
| --- | --- | --- | --- |
| `register_model/bootstrap.rs` | 107 | `let guard = OrchestratorLockGuard::acquire(...).await?;` then returned through `Ok((ctx, guard))` | Yes — wrapped in `Ok(…)` consumes the value, no warning |
| `register_model/bootstrap.rs` | 85 | `-> Result<(RegisterContext, OrchestratorLockGuard<'p>), DbError>` (return-type-level binding) | N/A (binding by ownership transfer, no `#[must_use]` warning on returns through a `Result`) |
| `register_model/apply.rs` | 40 | `lock_guard: OrchestratorLockGuard<'p>,` parameter — passed in by ownership, dropped or `release().await`'d in-fn | If `release()` not reached on every exit path, you get the runtime `tracing::error!` from `Drop`, but **no compile-time `must_use` warning** because the parameter binds the value |

**Test-side check.** The `#[cfg(test)] mod tests` block at lines
222-312 constructs guards via `for_test_no_client` and either
`drop(guard)` (explicit, no warning) or `block_on(async move
{ guard.release().await })` (consumed). Both are `must_use`-safe.

**Cascade verdict:** clean. No unwanted warnings. The annotation
catches the canonical accident pattern (`let _ = acquire(...).await`)
and nothing else.

[N-MU1] (Note, no severity) — the `into_held()` method still carries
`#[allow(dead_code)]` (line 176). r4's L-prio observation stands: zero
non-test callers, kept as a forward-looking RAII handoff. The
`#[must_use]` on the *struct* + `#[allow(dead_code)]` on the *method*
don't conflict (the `must_use` on a method return value would be
redundant given the type itself is `must_use`).

**Verification:**
```
Grep "#\[must_use\]" crates/plugin-db/src/orchestrator/lock_guard.rs   → one hit
Grep "OrchestratorLockGuard" crates/plugin-db/src                       → 3 production call sites + 5 test sites + the guard impl
cargo check -p zeroship-plugin-db --lib                                 → no must_use warnings
```

---

## 2. `first_row_or_internal` visibility (eda96ead)

Declared at `crates/plugin-db/src/error.rs:322`:

```rust
pub(crate) fn first_row_or_internal<'a, R>(
    rows: &'a [R],
    op: &'static str,
) -> Result<&'a R, DbError> { … }
```

**Callers (production):**
- `crates/plugin-db/src/audit.rs:325` — `insert_audit_*` family
- `crates/plugin-db/src/audit.rs:611` — `insert_backfill_running`
- `crates/plugin-db/src/replication.rs:307` — `pg_create_logical_replication_slot`

**Callers (test):**
- `crates/plugin-db/src/error.rs:586, 598` — unit tests for the helper
- `crates/plugin-db/src/audit.rs:872, 878` — regression guard for the
  audit-id=0 empty-RETURNING bug class (commit d7cfc089)

All consumers live inside the `plugin-db` crate, so `pub(crate)` is
the correct ceiling. **Not** `pub` (would expose to external test
crates without need); **not** module-private (audit + replication +
self-tests are in three different modules). Right scope.

**One micro-observation worth flagging:**

[I-FR1] The function is generic over `R` purely for the test path.
  File: `crates/plugin-db/src/error.rs:322-329`
  Why: the generic parameter exists so tests can pass `Vec<i64>`
  instead of `Vec<compio_postgres::Row>` (whose constructors are
  crate-private). Production callers always instantiate it as
  `&[compio_postgres::Row]`. The generic is harmless but inflates the
  monomorphisation table by one entry per call site that uses a
  non-Row type — i.e. only the two test sites. This is a deliberate
  trade-off, documented in the doc-comment.
  Surface impact: none. The doc-comment is excellent — it tells future
  readers *why* the generic exists.
  Fix: leave as-is. (Documenting the trade-off in the surface review
  for completeness.)
  Verification: `Grep "first_row_or_internal" crates/plugin-db` → 7
  hits, 3 production + 4 test/doc.

---

## 3. `resolve_watchdog_app_id` / `resolve_drop_abandoned_app_id` (c0590506)

`crates/plugin-db/src/v8_classes/replication.rs:143, 158`:

```rust
#[inline]
fn resolve_watchdog_app_id(stamped: &str, _opts: &Value) -> String { … }

#[inline]
fn resolve_drop_abandoned_app_id(stamped: &str, _opts: &Value) -> String { … }
```

**Visibility:** both are **module-private** (no visibility modifier),
same as the pre-existing `resolve_setup_app_id` they were modelled
after (line 123). The doc comments deliberately call out the
"regression trip-wire" pattern: a future contributor restoring
caller-controlled overrides would have to delete the helper and its
tests, making the regression visible in code review.

**Test reachability:** the `#[cfg(test)] mod tests` block at lines
207-341 imports both via `use super::{resolve_drop_abandoned_app_id,
resolve_setup_app_id, resolve_watchdog_app_id};` (line 217). All
three are exercised by 11 regression tests pinning the
"ignore caller `appId`" invariant across:
- string override (`opts.appId = "victim_app"`)
- non-string override (numbers, booleans, null, arrays, objects)
- empty / null opts
- unicode in stamped id

The pattern mirrors `resolve_setup_app_id`'s test suite verbatim,
including the same "trip-wire" doc-comment convention.

**Verdict:** module-private is the right scope — narrower than
`pub(crate)` would be, since these helpers are only callable from
inside `v8_classes/replication.rs`. The test block reaches them via
`super::` which works against any visibility ≥ private.

**No finding.** This is the model implementation for the trip-wire
pattern. Recommend it as the template for any future "tenant scoping
decision" helpers (e.g. if `migration::start` or
`migration::cancel` ever grow an `opts` shape that could be hijacked).

**Verification:**
```
Grep "resolve_watchdog_app_id|resolve_drop_abandoned_app_id" crates/  →
  declaration + use-statement + 8 unit tests; zero external references
```

---

## 4. [I28] sweep API impact — boundary functions exposed via `test-helpers`

The 0049d9be commit changed ~30 function signatures from
`Result<_, String>` to `Result<_, DbError>` across `auth/bootstrap.rs`,
`auth/keys.rs`, `auth/session.rs`, `replication.rs`,
`backend/postgres.rs`, and `diff.rs`. Most of these modules are
`pub(crate)` in release and `pub` under `test-helpers` — meaning the
signature change *is* SDK-visible from the perspective of an external
test crate.

**`auth/*` sweep:** the `pub use` re-exports at `auth/mod.rs:68-70`
(`ensure_admin_schema`, `BootstrapOutcome`, `rotate_session_keys`,
`RotationOutcome`, `init_session`, `mint_session_token`, `MintedToken`,
`SessionInit`) all now return `Result<_, DbError>` instead of
`Result<_, String>`. Integration tests at `tests/integration.rs:3379…4011`
consume them via `.unwrap()` / `.expect()`, both of which are agnostic
to the error type — so the change is source-compatible for the
test-crate side **even though the error type changed**.

**`replication.rs` sweep:** `pub fn sanitise_app_id`, `publication_name`,
`slot_name`, `ensure_publication_and_slot`, `watchdog_query`,
`drop_abandoned_slots`, `slot_status` all now return `Result<_, DbError>`.

**[H1] [I28] sweep + c0590506 broke `tests/integration.rs` compile.**
  File: `crates/plugin-db/tests/integration.rs:2906, 2947, 2956`
  Why: c0590506 added an `app_id: &str` parameter to
  `replication::watchdog_query` and `replication::drop_abandoned_slots`
  for tenant scoping. The two integration tests `c1_watchdog_reports_slot_health`
  (2906) and `c1_drop_abandoned_reaps_inactive_slot` (2947, 2956) still
  call the pre-c0590506 signatures:
  ```
  cargo check -p zeroship-plugin-db --tests --features=test-helpers
  error[E0061]: this function takes 2 arguments but 1 argument was supplied
     --> crates/plugin-db/tests/integration.rs:2906:17
        zeroship_plugin_db::replication::watchdog_query(&pool)
  error[E0061]: this function takes 3 arguments but 2 arguments were supplied
     --> crates/plugin-db/tests/integration.rs:2947:19
        zeroship_plugin_db::replication::drop_abandoned_slots(&pool, 0)
  error[E0061]: this function takes 3 arguments but 2 arguments were supplied
     --> crates/plugin-db/tests/integration.rs:2956:13
        zeroship_plugin_db::replication::drop_abandoned_slots(&pool, 0)
  ```
  Surface impact: **the `test-helpers` build is broken.** Every CI
  invocation of `cargo test -p zeroship-plugin-db --tests
  --features=test-helpers` will fail at compile, not just at the three
  affected tests. This blocks ALL integration tests in this crate —
  none of them can run because the test binary doesn't compile.
  This is the single highest-severity API-surface issue: a wire-level
  contract change (the function signature) wasn't propagated to the
  consumer.
  Fix: pass `app` as the second argument at each site:
  ```rust
  // line 2906
  let slots = zeroship_plugin_db::replication::watchdog_query(&pool, app).await.unwrap();
  // line 2947, 2956
  let dropped = zeroship_plugin_db::replication::drop_abandoned_slots(&pool, app, 0).await.unwrap();
  ```
  (The `app` variable is already in scope at both call sites — line
  2897 for the watchdog test, line 2935 for the abandoned test.)
  Verification:
  ```
  cargo check -p zeroship-plugin-db --tests --features=test-helpers
  ```
  must compile cleanly.

**[I-I28a] `init_string` callers in `replication.rs` were swept clean by 91830cca.**
  Status: resolved. The follow-up commit removed the one stale
  `.into_string()` that 0049d9be left dangling. `Grep
  "\.into_string\(\)" crates/plugin-db/src` shows zero hits — the
  `error.rs` `into_string()` bridge fn (line 252) is still defined for
  callers but no in-crate site uses it. (External crates *might*; the
  fn is `pub` on the `pub` error module.)

**[I-I28b] `replication_*_dispatch` signatures unchanged.**
  `replication_ops.rs:55, 96, 137` — all three take `app_id: String` by
  value. The wrapping `DbError::Internal` at the dispatch boundary that
  used to flatten typed errors was removed (the commit message confirms
  this); each dispatch now `?`-flows typed errors through to
  `e.to_op_error()`. **Surface improvement:** SDK now sees `.code =
  "wal_level_not_logical"` instead of `.code = "internal"` for the wal-
  level mis-config path, plus the canonical `session_*` codes for the
  init_session validation refusals. No public-surface regression.

---

## 5. `#[doc(hidden)] pub fn` count — sweep total

Fresh `Grep "#\[doc\(hidden\)\]" crates/plugin-db/src` returns 20
hits:

| File | Lines | Visibility shape | Count |
| --- | --- | --- | --- |
| `migrations.rs` | 773, 787, 799, 828, 840, 852 | `cfg(any(test, feature="test-helpers"))` + `#[doc(hidden)] pub` | 6 |
| `replication_ops.rs` | 288, 296 | same | 2 |
| `wal_consumer.rs` | 130, 166, 172, 184 | `pub(crate)` + redundant `#[doc(hidden)]` (legacy from pre-5ceb6daa) | 4 |
| `lib.rs` | 209, 237, 261, 298, 313, 322, 330 | `cfg(any(test, feature="test-helpers"))` + `#[doc(hidden)] pub` | 7 |
| `exec.rs` | 333 | `cfg(any(test, feature="test-helpers"))` + `#[doc(hidden)] pub` | 1 |

Total: 20, identical to r4's count. **No new `#[doc(hidden)] pub fn`
added by any of the six cycle commits.** Cycle commits added zero
test-only helpers — the new code is all production-path. Clean.

[L1 — carries forward from r4 unchanged] The four redundant
`#[doc(hidden)]` on `pub(crate)` items in `wal_consumer.rs:130, 166,
172, 184` still stand. `pub(crate)` already hides from rustdoc;
`#[doc(hidden)]` is a no-op here. Trivial cleanup, no surface impact.

---

## 6. `DbError` reachability — variants since r4

`Grep` against r4's baseline (12 variants):

| Variant | Status r5 |
| --- | --- |
| `SchemaRefused` | unchanged |
| `ValidationFailed` | unchanged |
| `UniqueViolation` | unchanged |
| `FkViolation` | unchanged |
| `NotNullViolation` | unchanged |
| `CheckViolation` | unchanged |
| `Serialization` | unchanged |
| `LockContention` | unchanged |
| `Transient` | unchanged |
| `Configuration` | unchanged |
| `Coded` | unchanged |
| `Internal` | unchanged |

**No new variants.** `#[non_exhaustive]` (line 51) still in place,
guarding future additions at the SDK boundary.

The [I28] sweep affects *reachability* without changing the variant
set:
- `replication::ensure_publication_and_slot` now constructs
  `DbError::Configuration { code: "wal_level_not_logical", … }` for
  the wal_level mis-config path (was `DbError::Internal` previously
  via the dispatch wrap).
- `auth/session::init_session` now constructs typed
  `DbError::ValidationFailed { code: "session_signature_expired" /
  "session_nonce_replay" / "session_invalid_signature" }` for the
  SECURITY DEFINER refusals (was opaque `String` previously).
- `auth/*::coded_sql` maps SQLSTATE → typed `DbError` variants
  consistently across bootstrap.rs, keys.rs, session.rs (and a
  duplicate `coded_sql` in `replication.rs`).

**Surface impact:** SDK code that previously had to substring-match
`/wal_level/` in `e.message` can now branch on `e.code ===
"wal_level_not_logical"`. Net win for the SDK API.

**[I-DR1] Three `coded_sql` helpers — duplication.**
  Files:
  - `crates/plugin-db/src/auth/bootstrap.rs` (private)
  - `crates/plugin-db/src/auth/keys.rs` (private)
  - `crates/plugin-db/src/auth/session.rs` (private)
  - `crates/plugin-db/src/replication.rs` (private)
  Why: four near-identical `coded_sql` functions all mapping
  `compio_postgres::Error` SQLSTATE → `DbError` variant. Each is
  module-private, so no surface leak. But the duplication is a
  surface-adjacent concern: a future variant addition has to land in
  four places, and the compiler-detected dead-code warnings already
  show three of the four are unused in the lib build (cargo check
  reports `function 'coded_sql' is never used` in all four modules,
  meaning they're only reached by the lib-test target's reachability
  graph).
  Fix: extract to `crate::error` as a `pub(crate) fn classify_pg_or(...)`
  helper. Same place as `first_row_or_internal` lives. Or — simpler —
  use the existing `DbError::from_pg` + a `prefix_message` adapter at
  each call site.
  Verification: `Grep "fn coded_sql" crates/plugin-db/src` → 4 hits;
  `cargo check -p zeroship-plugin-db --lib` shows 4 unused-fn warnings.

---

## 7. `slot_name_like_prefix` (new in c0590506)

Declared at `crates/plugin-db/src/replication.rs:150`:

```rust
pub(crate) fn slot_name_like_prefix(app_id: &str) -> Result<String, DbError> {
    Ok(format!("{}%", slot_name(app_id)?))
}
```

**Visibility:** `pub(crate)`. **Necessary surface check:**

Callers — all in-crate:
- `replication.rs:406` (`watchdog_query`)
- `replication.rs:544` (`drop_abandoned_slots`)
- `replication.rs:944, 949, 957, 964, 973, 988, 992, 997` — the
  `*_filters_by_app_id` regression-guard unit tests in the same file's
  `#[cfg(test)] mod tests` block.

Zero external test references. `pub(crate)` is the right ceiling:
narrower than `pub` (no external need), wider than module-private
(test sites at the bottom of the same file actually live in a child
`mod tests` and reach the function via `use super::*;` so private
would also work — but `pub(crate)` is consistent with `slot_name`'s
sibling `pub` visibility).

**One sub-observation:** `slot_name` itself (line 134) and
`publication_name` (line 129) are both `pub`, but `slot_name_like_prefix`
is `pub(crate)`. The asymmetry is intentional: the first two are
naming primitives that the SDK / control-plane tier might consume to
introspect a per-app namespace (e.g. "show me my app's slot name");
the LIKE prefix is purely an internal SQL-injection-safety helper and
has no SDK utility.

[I-SLP1] `slot_name_like_prefix` is `pub(crate)` but `slot_name` is `pub`.
  File: `crates/plugin-db/src/replication.rs:134, 150`
  Why: minor consistency observation. The two functions are
  semantically a pair (one returns the per-app slot name; the other
  returns the per-app slot LIKE prefix). One is exposed externally,
  the other isn't. If `slot_name_like_prefix` has internal-only
  semantics (SQL escape correctness), the asymmetry is justified. If
  external callers will eventually want to introspect "all my app's
  shard-slot names start with this prefix", upgrade to `pub`.
  Surface impact: minimal — `pub(crate)` is the conservative default
  and easy to widen later.
  Fix: leave as-is for now; documented here so a future reviewer
  doesn't flag the asymmetry as a bug.
  Verification: `Grep "slot_name_like_prefix\b" crates/` → declaration
  + 2 in-file production sites + 8 in-file test sites; zero external.

**Net dimension grade: A.** Right scope, no leak.

---

## 8. Cfg-fork test-helpers visibility — 8 modules

Sweep against `lib.rs:62-101`:

```
audit, auth, exec, migrations, orchestrator, replication,
replication_ops, wal_consumer
```

Pattern is invariant:

```rust
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod <name>;
#[cfg(feature = "test-helpers")]
pub mod <name>;
```

Spot-checked all 8 modules: identical. **No drift.**

[I-CFG2] (carries forward from r4 unchanged) — `error` module is `pub
mod error;` (line 50) unconditionally. The justification in r4 was
"reached even without the feature"; a fresh grep `Grep "use
zeroship_plugin_db::error" .` still returns zero external use. The
`DbError` type is reached indirectly via `From<QueryError>` /
`From<compio_postgres::Error>` impls but no external `use`
statement names `zeroship_plugin_db::error::` directly. Could join
the cfg-fork. Same status as r4.

**Always-pub modules** unchanged from r4: `broker`, `error`, `query`,
`v8_classes`.

**Test crate consumers** (under `test-helpers`):
- `tests/integration.rs` — reaches `auth`, `migrations`, `replication`,
  `replication_ops`, `exec` (also `query`, `broker`, `error` indirectly)
- `tests/db_v8_class.rs` — reaches `v8_classes`, `broker`
- `tests/subscription_finalizer.rs` — reaches `broker`, `v8_classes`
- `tests/auto_tx.rs`, `tests/capability.rs` — reach `orchestrator::auto_tx`

All cfg-fork modules continue to be reached only via the `test-helpers`
build. **Dimension grade: A.**

---

## Bonus: dead-code surface from compiler telemetry

`cargo check -p zeroship-plugin-db --lib` (no test-helpers, no
tests) emits **59 warnings**, dominated by the [I28] sweep aftermath.
Filtered to actual surface concerns (excluding fields-never-read on
internal structs that derive Debug):

```
crates/plugin-db/src/auth/mod.rs:68    unused imports: BootstrapOutcome, ensure_admin_schema
crates/plugin-db/src/auth/mod.rs:69    unused imports: RotationOutcome, rotate_session_keys
crates/plugin-db/src/auth/mod.rs:70    unused imports: MintedToken, SessionInit, init_session, mint_session_token
crates/plugin-db/src/auth/mod.rs:77    constant ADMIN_SCHEMA is never used
crates/plugin-db/src/auth/mod.rs:82    constant PLATFORM_ROLE is never used
crates/plugin-db/src/auth/mod.rs:89    constant APP_ROLE_TEMPLATE is never used
crates/plugin-db/src/auth/mod.rs:96    constant DEFAULT_TOKEN_TTL_SECS is never used
crates/plugin-db/src/auth/mod.rs:102   constant NONCE_RETENTION_SECS is never used
crates/plugin-db/src/auth/bootstrap.rs (many): coded_sql, ensure_admin_schema, install_* functions never used
crates/plugin-db/src/auth/keys.rs       (many): coded_sql, rotate_session_keys, current_key_id, previous_key_id never used
crates/plugin-db/src/auth/session.rs    (many): mint_session_token, init_session, civil_from_days, hex_* never used
crates/plugin-db/src/migrations.rs:755  function release_active_lock is never used
crates/plugin-db/src/replication.rs:613 function slot_status is never used
crates/plugin-db/src/wal_consumer.rs:131 function any_app_suppressed is never used
crates/plugin-db/src/wal_consumer.rs:173 function set_local_emit_suppressed is never used
```

[M1] The entire `auth/*` module surface is dead in the lib build.
  File: `crates/plugin-db/src/auth/mod.rs:64-70` and all of
  `auth/bootstrap.rs`, `auth/keys.rs`, `auth/session.rs`.
  Why: the lib build (no `test-helpers`) shows that nothing in
  `crates/plugin-db/src/**` outside `auth/*` calls `auth::*`. A `Grep
  "use crate::auth\|crate::auth::"` of the src tree returns zero hits.
  The whole subsystem is reached only via the `test-helpers` cfg-fork
  (so external test crates can drive `ensure_admin_schema`,
  `mint_session_token`, etc.) and via its own `#[cfg(test)] mod tests`
  inside each file. **There is no production caller.**
  Surface impact: this is a >900-LOC module hierarchy that ships in
  every release binary (the lib build pulls them in via the
  `pub(crate) mod auth` declaration at lib.rs:69 even though the SDK
  surface is the `pub use` re-exports at `auth/mod.rs:68-70` that are
  themselves unused). The constants `ADMIN_SCHEMA`, `PLATFORM_ROLE`,
  etc., are advertised as "re-exported for other modules to address
  them without stringly-typed literals" (comment at mod.rs:72-74) but
  nobody addresses them.
  Two interpretations:
  1. **P8c is a planned-but-unwired feature.** The `auth` module
     ships the SECURITY DEFINER trust anchor + HMAC session init that
     the proposal calls for, but the runtime hasn't wired it into the
     `replication_ops` consumer path yet. In that case the surface is
     a forward-looking placeholder.
  2. **Dead code.** If P8c was wired and then unwired, the dead-code
     warnings are legitimate.
  Without context on which is true, this audit can only flag the
  symptom. Surface-wise, it's >40 `pub fn` items on a tree that
  nothing in the lib build reaches.
  Fix: confirm which interpretation applies; then either (a) wire
  `ensure_admin_schema` into `init_pool_async` or the
  `start_replication_consumer_dispatch` setup path, or (b) gate the
  whole `auth/*` tree behind a `harden` Cargo feature so release
  binaries don't ship the unreached code.
  Verification:
  ```
  Grep "use crate::auth\|crate::auth::" crates/plugin-db/src
  → zero hits
  cargo check -p zeroship-plugin-db --lib 2>&1 | grep -c "auth/"
  → 30+ dead-code warnings
  ```

[L2] `wal_consumer::any_app_suppressed` + `set_local_emit_suppressed` still dead.
  (Carries forward from r4 L1/L2 — unchanged.)
  File: `crates/plugin-db/src/wal_consumer.rs:131, 173`
  Verification: `cargo check -p zeroship-plugin-db --lib` warnings.

[L3] `migrations::release_active_lock` + `replication::slot_status` still dead in both builds.
  (Carries forward from r4 L5 — unchanged.)

[I-CMP1] [I28] sweep left ~12 unused-fn warnings in `auth/*`.
  File: see compiler output above.
  Why: 0049d9be added typed-DbError signatures to `auth/*` functions
  but no production code consumes them; the warnings are dormant
  precisely because of M1 above. If M1 is resolved (the auth module
  gets wired up), these warnings go away naturally.
  Fix: subsumed by M1's resolution.

---

## Surface diff vs r4

| Dimension | r4 verdict | r5 verdict |
| --- | --- | --- |
| 1. `OrchestratorLockGuard` | (new) — no findings | `#[must_use]` cascades cleanly, no unwanted warnings; `into_held()` still dead (L-prio, unchanged) |
| 2. `wal_consumer` shim demotion | resolved + L1/L2 (dead) | Unchanged — L1/L2 carry forward |
| 3. `broker::has_subscribers` | I-BR1 (consistency) | Unchanged — not touched by cycle commits |
| 4. `DbError` variants | 12 variants, no leaks | 12 variants, no leaks; reachability *broadened* by typed `auth/*` + `replication` returns (improvement) |
| 5. `#[doc(hidden)] pub fn` count | 20 (16 cfg-gated, 4 redundant) | Unchanged: 20 |
| 6. Cfg-fork visibility | 8 modules, consistent | Unchanged: 8 modules, consistent |
| 7. `pub use` re-exports | clean | Unchanged — `auth::*` + `backend::PostgresBackend` still behind `pub(crate)` parents in release |
| 8. `mint_*` helpers (r4 M1) | M1 unaddressed | **Resolved** by 07205e54 (just before r4 cycle). Only `mint_db` + `mint_subscription` remain `pub`, matching test-crate consumers; the other four are `pub(crate)`. r4's M1 was already closed when r4 ran but missed verification. |

**Reverification of r4 M1.** Grep confirms 07205e54 demoted the five
helpers:
```
crates/plugin-db/src/v8_classes/replication.rs:166: pub(crate) fn mint_replication
crates/plugin-db/src/v8_classes/transaction.rs:288: pub(crate) fn mint_transaction
crates/plugin-db/src/v8_classes/migrations.rs:251:  pub(crate) fn mint_migrations
crates/plugin-db/src/v8_classes/collection.rs:353:  pub(crate) fn mint_collection
crates/plugin-db/src/v8_classes/migration.rs:598:   pub(crate) fn migration_start_with_spec
crates/plugin-db/src/v8_classes/db.rs:367:          pub fn mint_db
crates/plugin-db/src/v8_classes/subscription.rs:167: pub fn mint_subscription
```
r4 graded M1 as "still 5 over-exposed" but the demotions had already
landed; the M1 finding was retrospectively obsolete.

---

## Findings (consolidated)

```
[H1] tests/integration.rs broken by c0590506 signature change (CRITICAL — blocks test-helpers build)
  Files: crates/plugin-db/tests/integration.rs:2906, 2947, 2956
  Why: watchdog_query/drop_abandoned_slots gained `app_id` param;
       integration tests still call with the old signatures, breaking
       compile of EVERY integration test in the test-helpers build.
  Fix: pass `app` as 2nd arg at each site (variable already in scope).
  Verification: cargo check -p zeroship-plugin-db --tests --features=test-helpers

[M1] entire auth/* module surface is dead in lib build
  Files: crates/plugin-db/src/auth/{mod,bootstrap,keys,session}.rs
  Why: 30+ dead-code warnings; no production caller of any auth fn or
       const. Either P8c is unwired (forward-looking placeholder) or
       legitimately dead.
  Fix: wire ensure_admin_schema into a production path, OR gate auth/*
       behind a `harden` Cargo feature, OR delete if truly dead.
  Verification: Grep "use crate::auth\|crate::auth::" crates/plugin-db/src

[I-CFG2] error module is pub unconditionally without direct external use
  (carries forward from r4 I-CFG1 — unchanged)
  File: lib.rs:50
  Fix: could join the cfg-fork (pub(crate) in release).

[I-DR1] Three+ duplicate `coded_sql` helpers across auth/* + replication.rs
  Why: same SQLSTATE→DbError logic in 4 modules; future variant
       addition has to land in 4 places.
  Fix: extract to crate::error or use DbError::from_pg + prefix_message
       at call sites.
  Verification: Grep "fn coded_sql" crates/plugin-db/src → 4 hits.

[I-SLP1] slot_name_like_prefix is pub(crate) but slot_name is pub
  File: replication.rs:134, 150
  Status: minor asymmetry; pub(crate) is conservative and easy to
          widen later. Document the decision or upgrade.

[I-FR1] first_row_or_internal generic over R is for test-only convenience
  Status: deliberate, well-documented. No fix needed.

[I-I28a] .into_string() bridge fn (error.rs:252) is defined but no
         in-crate caller; external callers may still use it
         Status: documented bridge for the conversion sweep — leave.

[I-I28b] replication_ops dispatchers preserve typed DbError through
         the V8 boundary (improvement over r4)
         Status: net SDK API win.

[N-MU1] OrchestratorLockGuard::into_held() still #[allow(dead_code)]
         (carries forward from r4 I-LG1 — unchanged)

[L1] wal_consumer.rs: 4 redundant #[doc(hidden)] on pub(crate) items
     (carries forward from r4 L4 — unchanged)

[L2] wal_consumer::any_app_suppressed + set_local_emit_suppressed dead
     (carries forward from r4 L1/L2 — unchanged)

[L3] migrations::release_active_lock + replication::slot_status dead
     in both release AND test-helpers builds
     (carries forward from r4 L5 — unchanged)
```

---

## Score

**72 / 100**  (−13 vs r4's 85)

**Drop drivers:**

- **H1 is the big penalty.** The `test-helpers` build is broken —
  `cargo check -p zeroship-plugin-db --tests --features=test-helpers`
  fails with three E0061 errors. This is a wire-level surface
  contract violation: c0590506 changed `watchdog_query` and
  `drop_abandoned_slots` signatures (an externally-reachable surface
  under `test-helpers`) without updating the test consumer. Even
  though the production V8 dispatcher is fine, the *audited surface*
  (everything reachable under `test-helpers`) has a broken edge. CI
  for this crate cannot run integration tests until this is fixed.
  (−10)

- **M1 newly surfaced.** The `auth/*` module is a dead surface in
  release binaries — 30+ dead-code warnings, no production caller. r4
  didn't audit the lib-build dead-code carefully; this round catches a
  much larger surface bloat than the wal_consumer L-prio items r4
  flagged. Either a forward-looking placeholder or genuinely dead, but
  either way it's >40 `pub fn` items contributing to release-binary
  size and SDK surface ambiguity. (−5)

**Improvements that prevented a steeper drop:**

- **r4 M1 retrospectively resolved.** 07205e54 demoted the 5 mint
  helpers to `pub(crate)` *before* the r4 audit ran; r4 missed the
  verification. Now confirmed. (+3)

- **DbError reachability improved across the SDK.** [I28] sweep means
  SDK callers can now branch on `e.code === "wal_level_not_logical"`,
  `e.code === "session_signature_expired"`, etc. — the typed error
  rail extends much further down into auth/ and replication/ paths
  than before. Net SDK ergonomics win. (+1)

- **resolve_*_app_id helpers (c0590506) are exemplary.** Module-private,
  unit-tested, doc-commented "regression trip-wire". This is the model
  pattern for any future tenant-scoping helper. (+1)

- **OrchestratorLockGuard `#[must_use]` cascade is clean.** No
  unwanted warnings, catches the canonical accident pattern. (+1)

- **first_row_or_internal is correctly `pub(crate)`** — right scope,
  clean tests, used in 3 production sites. (+0; expected outcome but
  worth noting)

**Score sub-ranges:**
- 85+ would require: fix H1 (one-line change at each of three test
  sites).
- 90+ would require: also resolve M1 (either wire `auth/*` or gate it
  behind a feature flag) + tighten I-CFG2.
- 95+ would require: also dedupe I-DR1's `coded_sql` helpers + clean
  up the carry-forward L-prio items (L1/L2/L3).

**Highest-leverage next move:**

```rust
// crates/plugin-db/tests/integration.rs:2906
-    let slots = zeroship_plugin_db::replication::watchdog_query(&pool)
+    let slots = zeroship_plugin_db::replication::watchdog_query(&pool, app)

// crates/plugin-db/tests/integration.rs:2947, 2956
-    let dropped = zeroship_plugin_db::replication::drop_abandoned_slots(&pool, 0)
+    let dropped = zeroship_plugin_db::replication::drop_abandoned_slots(&pool, app, 0)
```

Three one-line fixes unblock the entire test-helpers build, recovering
~10 points. The c0590506 commit shipped a CRITICAL security fix but
left a CRITICAL build break — these are coupled and both need attention.
