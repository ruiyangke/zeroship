# plugin-db API-surface review — round 6 (2026-05-22)

**Lens:** API surface (what's `pub`, what's `pub(crate)`, what crosses the crate boundary, what is dead).

**Baseline:** r5 — 72/100, taken at cycle 05:25. The H1 build break
(`watchdog_query` / `drop_abandoned_slots` signature drift) was closed
by f1f06900 at the end of cycle 05:25; r5 itself was written against
the pre-fix tree.

**Commits since r5 baseline:**

| SHA | Subject | Surface impact |
|-----|---------|----------------|
| 4b2e7046 | rewrite ConsumerRunningGuard comment block | comment-only, no surface change |
| 70921112 | atomic try-claim closes startReplicationConsumer race | NEW `pub fn try_mark_consumer_running` on `IsolateDbContext` |
| a272d1af | classify P0001 RAISE via DETAIL token | NEW private `fn classify_p0001_detail` in `auth::session` |
| aa639715 | `WalConsumer::new` returns `Result<_, DbError>` | REMOVED `ConsumerError::NotProvisioned` variant; signature change on `WalConsumer::new` |

`cargo build -p zeroship-plugin-db --tests --features test-helpers` is
clean (13 warnings, all dead-code; no errors). H1 stays closed.

---

## Findings

### [CRITICAL] (none)

The previous r5 CRITICAL (broken test-helpers build) is closed and
no new build-breaks were introduced.

### [MAJOR-R6-1] `crates/plugin-db/src/context.rs:415` — `mark_consumer_running` is dead

```rust
/// Mark a replication consumer as running for this app.
pub fn mark_consumer_running(&mut self, app_id: &str) {
    self.running_consumers.insert(app_id.to_string());
}
```

  Why: 70921112 introduced `try_mark_consumer_running` and rewired
  the only production caller in `replication_ops.rs` to use it
  exclusively (line 290). The old unconditional `mark_consumer_running`
  is now called only by its own unit tests (`context.rs:884, 893, 896,
  904, 905, 917, 926, 927, 928`). The compiler agrees:

  ```
  warning: method `mark_consumer_running` is never used
     --> crates/plugin-db/src/context.rs:415:12
  ```

  Surface impact: the API now ships TWO mutator entry points with
  near-identical signatures on `IsolateDbContext` — the safe atomic
  one (`try_mark_consumer_running`) and the dead unconditional one.
  A future contributor reading `context.rs` cannot tell which is the
  canonical mark without grepping the consumers. The presence of both
  is also a footgun: someone adding a second consumer-spawn site
  might re-introduce the same race 70921112 closed.

  Fix: delete `mark_consumer_running`. Update the three unit tests
  (`consumer_running_round_trip`, `consumer_running_is_per_app`,
  `mark_consumer_running_is_idempotent`, `clear_consumer_registry_drops_all_entries`)
  to use `try_mark_consumer_running` (ignore the bool return for the
  "is_idempotent" test by asserting that a second call returns
  `false`).

  Verification:
  ```bash
  rg "\.mark_consumer_running\(" crates/plugin-db/src/
  # Only context.rs lines (tests + def). Production call sites: 0.
  ```

### [MAJOR-R6-2] `crates/plugin-db/src/replication_ops.rs:34-46` — module docstring contradicts the new error rail

The dispatch boundary's module-level docstring still claims:

```rust
//! - `WalConsumer::new` failures map to [`crate::error::DbError::Configuration`]
//!   with `code = "not_provisioned"` (the only thing that can fail
//!   pre-spawn is sanitisation of `app_id`).
```

  Why: post-aa639715, `WalConsumer::new` returns TWO distinct DbError
  variants:
  - `DbError::ValidationFailed { code: "invalid_app_id" }` for
    sanitise failures (the docstring's own claimed reason),
  - `DbError::Configuration { code: "not_provisioned" }` for missing
    `db_url`.

  Both flow verbatim through the dispatch boundary (line 246:
  `e.to_op_error()`). The inline comment at lines 233-239 has the
  correct shape; the module-level docstring is stale and asserts the
  exact opposite of the test pinning behaviour in
  `wal_consumer_new_invalid_app_id_returns_typed_error`
  (`wal_consumer.rs:949`).

  Surface impact: the module docstring is the canonical SDK-author
  contract for "what `.code`s does this dispatch surface emit". The
  SDK author who reads only the docstring will write the wrong
  branch. This is also a wire-format documentation bug — the rdoc
  output for this module mis-describes the protocol.

  Fix: rewrite the bullet to mirror the inline comment:

  ```rust
  //! - `WalConsumer::new` failures preserve their typed DbError
  //!   variant verbatim — two distinct `.code`s reach the SDK:
  //!     * `ValidationFailed { code: "invalid_app_id" }` for
  //!       sanitise failures (developer/deploy error)
  //!     * `Configuration { code: "not_provisioned" }` for missing
  //!       `db_url` (operator error)
  ```

  Verification:
  ```bash
  rg -nC2 "not_provisioned" crates/plugin-db/src/replication_ops.rs
  # Should show inline comment match and NO doc-comment that
  # describes only one variant.
  ```

### [MAJOR-R6-3] `crates/plugin-db/src/replication_ops.rs:50` — unused `DbError` import

```rust
use crate::error::DbError;
```

  Why: when the dispatch site re-stamped `WalConsumer::new` errors
  it needed `DbError`; aa639715 dropped that re-stamp but left the
  `use` line. The compiler warns:

  ```
  warning: unused import: `crate::error::DbError`
    --> crates/plugin-db/src/replication_ops.rs:50:5
  ```

  Surface impact: clutter at the surface that someone scanning
  "what does this module need from `error`" must reason about. Not
  a wire bug; a hygiene bug at the surface boundary.

  Fix: delete the line.

  Verification:
  ```bash
  cargo build -p zeroship-plugin-db --tests --features test-helpers 2>&1 \
      | grep "unused import" | grep replication_ops
  ```

### [MINOR-R6-1] WalConsumer::new shape change — no external callers, clean migration

```rust
pub fn new(app_id: &str, db_url: &str) -> Result<Self, DbError>
```

Down from r5 baseline:
```rust
pub fn new(app_id: &str, db_url: &str) -> Result<Self, ConsumerError>
```

  Why: searched the whole repo for `WalConsumer::new(` callers —
  result is one production call (`replication_ops.rs:241`) plus the
  unit tests in `wal_consumer.rs`. Both are updated in the same
  commit. No external crate names `ConsumerError::NotProvisioned`.

  Verification:
  ```bash
  rg "WalConsumer::new|ConsumerError::NotProvisioned" \
      crates/ examples/ tests/
  ```

  This is the right call — the variant carried opaque "not
  provisioned" semantics that conflated developer error with
  operator error. The two-leg split (ValidationFailed for bad ids,
  Configuration for missing url) gives the SDK the distinct `.code`s
  it needs.

  Status: closed cleanly; no follow-up needed beyond MAJOR-R6-2's
  docstring fix.

### [MINOR-R6-2] `crates/plugin-db/src/context.rs:424` — `try_mark_consumer_running` visibility is fine

```rust
pub fn try_mark_consumer_running(&mut self, app_id: &str) -> bool {
    self.running_consumers.insert(app_id.to_string())
}
```

  Why: `pub` looks open but the containing module `context` is
  `pub(crate) mod context;` in `lib.rs:56`, and `IsolateDbContext`
  is never re-exported. So effective visibility is `pub(crate)` —
  matching the rest of the file (every IsolateDbContext method is
  `pub fn`). The new method's signature follows the
  `Self::is_consumer_running` / `Self::unmark_consumer_running`
  surface convention; no churn at the boundary.

  No fix needed. Noted for completeness.

  Verification:
  ```bash
  rg "pub mod context|pub use.*context" crates/plugin-db/src/lib.rs
  # No matches → context is pub(crate); ergo IsolateDbContext methods
  # cannot escape the crate even when declared `pub`.
  ```

### [MINOR-R6-3] `crates/plugin-db/src/auth/session.rs:174` — `classify_p0001_detail` correctly scoped

```rust
fn classify_p0001_detail(
    e: &compio_postgres::Error,
) -> Option<(&'static str, &'static str)>
```

  Why: a272d1af introduced this helper as `fn` (private) in
  `auth::session` — the only caller is `init_session` 35 lines below.
  Right scope; no surface impact.

  Worth noting positively: this is the canonical pattern for
  module-private classification helpers (cf. `resolve_*_app_id` from
  c0590506 that r5 called "exemplary"). The helper is short, has a
  clear doc comment explaining the DETAIL-vs-substring rationale,
  and is fully exercised by the integration tests at
  `tests/integration.rs:3641`.

  No fix needed.

### [MINOR-R6-4] DbError variants unchanged — 11 total

Same 11 variants as r5: `SchemaRefused`, `ValidationFailed`,
`UniqueViolation`, `FkViolation`, `NotNullViolation`,
`CheckViolation`, `Serialization`, `LockContention`, `Transient`,
`Configuration`, `Coded`, `Internal`. (`#[non_exhaustive]` is
retained.)

  Why: aa639715 leverages existing variants
  (`ValidationFailed` + `Configuration`) — no new variant required.
  Right call; smaller surface is better.

  No action.

### [INFO-R6-1] `#[doc(hidden)] pub fn` count unchanged — 22 occurrences

Same shape as r5. Distribution:
- `lib.rs`: 6 (test-only fixture helpers)
- `wal_consumer.rs`: 4 (legacy suppression shims +
  `any_app_suppressed` + `LEGACY_SUPPRESSION_KEY` const)
- `replication_ops.rs`: 2 (test-only probes)
- `exec.rs`: 1
- `migrations.rs`: 6 (`exec_*_with_pool` family)
- (3 others on items grep counts in multi-line context)

  No change; carrying r5 verdict.

### [INFO-R6-2] cfg-fork test-helpers visibility — still 8 modules, still the right shape

`audit`, `auth`, `exec`, `migrations`, `orchestrator`, `replication`,
`replication_ops`, `wal_consumer`. Unchanged from r5. The cfg-fork
keeps release surface tight while exposing what `tests/integration.rs`
needs. No churn.

### [INFO-R6-3] `auth/*` dormant module — r5 M1 unchanged

```bash
rg "zeroship_plugin_db::auth|plugin_db::auth" crates/ --type rust
# Only matches: crates/plugin-db/tests/integration.rs (35 hits)
```

  Why: the entire `auth/{bootstrap,keys,session}.rs` subtree (8
  `pub (async )?fn` items, 4 `pub struct`s, 4 `pub const`s) is
  consumed exclusively by `tests/integration.rs` — same as at r5.
  No production crate (`control/`, `gateway/`, `runtime/`,
  `worker/`, `sandbox-agent/`) imports anything from
  `zeroship_plugin_db::auth::*`.

  The `auth/session.rs` did gain typed-DETAIL classification in a272d1af,
  which improves the surface meaningfully (locale/formatter
  independent) — but the module is still a forward-looking placeholder
  in the lib build. Dead weight at the lib level until P8c wires the
  consumer.

  Carrying forward as M1 — same severity as r5 (minor on its own,
  significant when paired with the cfg-fork's 8-module weight).

### [I-CARRY-1] `pub(crate)` items in wal_consumer used only by their own module

```
crates/plugin-db/src/wal_consumer.rs:132:pub(crate) fn any_app_suppressed
crates/plugin-db/src/wal_consumer.rs:174:pub(crate) fn set_local_emit_suppressed
crates/plugin-db/src/wal_consumer.rs:670:pub(crate) const INITIAL_BACKOFF
crates/plugin-db/src/wal_consumer.rs:672:pub(crate) const MAX_BACKOFF
crates/plugin-db/src/wal_consumer.rs:679:pub(crate) const STABILITY_THRESHOLD
```

Each is referenced only inside `wal_consumer.rs`. `pub(crate)` is
unnecessary — they could be `fn` / `const`. r5 carryover.

The compiler flagged two of them as dead:

```
warning: function `any_app_suppressed` is never used
warning: function `set_local_emit_suppressed` is never used
```

Both are kept as `#[doc(hidden)]` back-compat shims, but no caller
exists inside or outside the crate. Same status as r5.

  Verification:
  ```bash
  rg "any_app_suppressed|set_local_emit_suppressed|INITIAL_BACKOFF|\
      MAX_BACKOFF|STABILITY_THRESHOLD" crates/plugin-db/
  ```

### [I-CARRY-2] `auth/*` dead-code warnings under release lib build

(Same as r5 M1 / inherited from r4.)

---

## Score

**78 / 100** (**+6 vs r5's 72**)

**Improvements that drove the +6:**

- **r5 H1 closed** (+10): the test-helpers build break was the
  dominant penalty in r5. f1f06900 closed it before the cycle ended.
  CI for the integration test target can run again. This single fix
  recovers most of the r5 deficit.

- **`WalConsumer::new` shape change is exemplary** (+2): aa639715
  removes the `ConsumerError::NotProvisioned` opaque variant and
  splits it into two typed DbError legs with distinct `.code`s. The
  SDK can now branch on `invalid_app_id` vs `not_provisioned`
  remediation. Tests pin both legs
  (`wal_consumer_new_invalid_app_id_returns_typed_error`,
  `wal_consumer_new_missing_db_url_returns_configuration`). Net wire
  surface improvement — fewer variants AND more information.

- **`classify_p0001_detail` is the model pattern** (+1): a272d1af
  replaces fragile substring matching with stable DETAIL token
  classification AND is correctly module-private. This is the
  canonical shape for surface-classification helpers.

- **`try_mark_consumer_running` closes a real race** (+1): 70921112
  fixes a concurrency r7 NEW MINOR (race between the outer
  `is_consumer_running` gate and the spawned task's first poll). The
  fix is `pub fn` on `IsolateDbContext` (effectively `pub(crate)`
  because the containing module is `pub(crate)`), atomic via
  `HashSet::insert`'s bool return. Correct scope.

**Deductions that prevented a steeper rise:**

- **MAJOR-R6-1 dead pub method** (−2): `mark_consumer_running` is
  dead, the compiler flags it, the replacement
  `try_mark_consumer_running` is in production. Two near-identical
  mutator entry points on the same struct is a footgun for future
  contributors.

- **MAJOR-R6-2 docstring contradicts implementation** (−1):
  `replication_ops.rs` module docstring claims `WalConsumer::new`
  emits only `Configuration { code: "not_provisioned" }` — but it
  ALSO emits `ValidationFailed { code: "invalid_app_id" }`. The
  inline comment got updated; the module docstring did not. SDK
  authors reading the rdoc see the wrong contract.

- **MAJOR-R6-3 dead `use crate::error::DbError`** (−1): trivial
  hygiene break left by aa639715 — unused import the compiler
  already flags.

- **M1 carryover** (−5, unchanged from r5): `auth/*` is still a
  dormant 8-module subtree with zero production callers.

**Score sub-ranges:**
- 82+ would require: fix MAJOR-R6-1/2/3 (three small mechanical
  edits — delete one method + its tests, rewrite one bullet, delete
  one `use`).
- 88+ would require: also resolve M1 (wire `auth/*` into the runtime
  consumer or gate it behind a `test-helpers`-style feature flag
  that stops production builds from compiling it).
- 93+ would require: also drop the I-CARRY-1 `pub(crate)` items that
  never escape their own module — small cosmetic win, but it'd take
  the crate's "every pub(crate) item has a cross-module consumer"
  invariant from "mostly" to "always".

**Highest-leverage next move:**

```rust
// crates/plugin-db/src/context.rs:414-417
-    /// Mark a replication consumer as running for this app.
-    pub fn mark_consumer_running(&mut self, app_id: &str) {
-        self.running_consumers.insert(app_id.to_string());
-    }
```

…plus the four unit-test call-site updates inside the same file
(`tests/consumer_running_round_trip`, `is_per_app`,
`mark_consumer_running_is_idempotent`,
`clear_consumer_registry_drops_all_entries`) — switch each to
`try_mark_consumer_running(...)` and discard the bool. This is
maybe 12 LOC of edits and recovers the MAJOR-R6-1 deduction.

The MAJOR-R6-2 docstring fix and MAJOR-R6-3 dead-import deletion
combined are <10 LOC and recover the other two MAJOR deductions —
putting the score at ~84 after a single tight pass.
