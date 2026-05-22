# plugin-db — API Surface Review (r9)

- **Date:** 2026-05-22 (cycle 10:47)
- **Target:** `crates/plugin-db/` @ `05484878`
- **Lens:** Visibility scoping, re-exports, error envelope funnel,
  feature gating, `pub(crate)` field surface on `IsolateDbContext`.
- **Inputs since r8 (82/100):**
  - `389749ca` — close r8 MAJOR-R8-1: unify backend-missing code to
    `backend_not_initialized` across all three sites.
  - `2fa9472eef` — gate `auth/*` behind new `hardening` Cargo feature
    (closes r5-r8 carry MAJOR-R7-1 in-crate).
  - `5d9acab8d7` — surface `set_mig_lock` / `return_mig_client` state
    drift via `tracing::error!` / `tracing::warn!` (I23 from
    code-critique r9 §I7). Body-only; signatures unchanged.
  - `bed655c1`, `757026e3`, `7bd2187e` — docstring / bench scaffold;
    no surface change.

This audit re-walks the surface fresh; r8 verdicts re-checked, not
trusted by reference.

---

## Dimension 1 — r8 closures, verified

### r8 MAJOR-R8-1 backend-missing code drift — **CLOSED**

```bash
rg -n '"backend_not_initialized"' crates/plugin-db/src/
# 3 hits, all canonical:
#   orchestrator/register_model/mod.rs:127  code: "backend_not_initialized"
#   v8_classes/migration.rs:269             DbError::config("backend_not_initialized", "db: backend not initialized")
#   v8_classes/migrations.rs:180            DbError::config("backend_not_initialized", ...)
```

Spelling now uniform ("initialized", US) across all three sites.
`389749ca` collapsed the v8_classes pair to the orchestrator's code +
spelling. The conditional sibling-drift class is closed; the only
remaining `"not_configured"` callers are the **DB-URL-missing** /
**pool-missing** paths (different invariant — see Dimension 6).

[OK] r8 MAJOR-R8-1 is closed. +2 recovery.

### r8 MAJOR-R7-1 `auth/*` dormancy — **CLOSED in-crate**

```toml
# crates/plugin-db/Cargo.toml:48
hardening = []
[[test]]
required-features = ["test-helpers", "hardening"]
```

```rust
// crates/plugin-db/src/lib.rs:68-71
#[cfg(all(feature = "hardening", not(feature = "test-helpers")))]
pub(crate) mod auth;
#[cfg(all(feature = "hardening", feature = "test-helpers"))]
pub mod auth;
```

Default `cargo build -p zeroship-plugin-db --lib` no longer compiles
the four auth files; warning count drops from 73 → 15 (verified locally,
matches commit-message claim). Integration tests opt back in via the
two-flag required-features list.

**Verified the gate doesn't leak:**

```bash
rg -n '^pub use.*auth' crates/plugin-db/src/    # 0 hits
rg -n 'crate::auth::|super::auth::' crates/plugin-db/src/
# 5 hits, all *docstring* back-references in:
#   audit.rs:55, error.rs:357/396, auth/session.rs:166/191, auth/keys.rs:25
# No code-level re-export; no `use crate::auth` in src/.
```

The `auth::*` re-exports inside `auth/mod.rs:64-70` (3 modules + 7
re-exports + 5 consts) only exist when the feature is on, so they no
longer count as a default-build "dormant pub surface". The 4-cycle
carry is recovered in-crate. The full closure (wire `ensure_admin_schema`
into the control plane) remains cross-crate I5.

[OK] r8 MAJOR-R7-1 closed in-crate. +5 recovery.

### r8 INFO carries — **unchanged**

| Carry | Status | Notes |
|---|---|---|
| INFO-R8-1 `config_hinted()` 0 callers | unchanged | `rg DbError::config_hinted\( crates/plugin-db/src/` → 0 |
| INFO-R8-2 `validation_hinted()` 0 callers | unchanged | `rg DbError::validation_hinted\( crates/plugin-db/src/` → 0 |
| INFO-R8-3 `SuppressGuard::activate` (rename to `new`) | unchanged | `wal_consumer.rs:149` still `activate` |
| INFO-R8-4 `#[doc(hidden)]` on `pub(crate)` items | unchanged | 4 hits at `wal_consumer.rs:132/168/174/186` |
| INFO-R8-5 Configuration struct-literal vs ctor mix | unchanged | 5 struct-literal sites, 5 ctor sites |

[CARRY] Net surface impact: 0.

---

## Dimension 2 — `DbError` variants — full count + reachability

12 variants, all reachable from production. Variant set unchanged
since r7/r8. The `Configuration` code-set across 5 struct-literal
sites now reads:

| Site | code |
|---|---|
| `orchestrator/register_model/mod.rs:120` | `lazy_init_failed` |
| `orchestrator/register_model/mod.rs:127` | `backend_not_initialized` |
| `backend/postgres.rs:594` | `cic_configuration` |
| `wal_consumer.rs:350` | `not_provisioned` |
| `replication.rs:278` | `wal_level_not_logical` |

Plus 5 `DbError::config(...)` calls split now:

| Site | code |
|---|---|
| `exec.rs:68` | `not_configured` (pool missing) |
| `exec.rs:321` | `not_configured` (pool missing) |
| `orchestrator/transaction.rs:147` | `not_configured` (URL missing) |
| `orchestrator/auto_tx.rs:199` | `not_configured` (URL missing) |
| `v8_classes/migration.rs:269` | `backend_not_initialized` |
| `v8_classes/migrations.rs:180` | `backend_not_initialized` |

The `not_configured` / `backend_not_initialized` split is now
semantically coherent — `not_configured` = URL/pool missing
(plug-in itself disabled); `backend_not_initialized` = backend handle
absent (init_pool_async hasn't run yet). r8 conflated them; r9 sees
the post-fix shape and confirms the line is drawn correctly.

[OK] No dead variants. Code-set surface coherent.

---

## Dimension 3 — Error envelope funnel — fresh sweep

All `v8_async_method` / `v8_method` failure paths funnel through
`DbError::to_op_error()` (+ direct `OpError::type_error` / `range_error`
for arg-class refusals, which is the correct shape — TypeError-class
never needs `.code`):

```bash
rg -n 'RejectError\(' crates/plugin-db/src/ | wc -l    # 21 hits
# Of these:
#   18 wrap *.to_op_error() or DbError::to_op_error
#   2 wrap pre-converted OpError (e in match {Err(e)} where e: OpError) — migration.rs:400/438, 669/712
#   1 wraps OpError::type_error / from arg-parser at migrations.rs:205
# All paths reach the SDK via OpError; the SDK can branch on .code.
```

No new bypass paths discovered. The `to_op_error()` boundary is the
single coercion point for `DbError → JS Error`. The `OpError::type_error`
direct constructions are arg-parse / illegal-constructor / alloc
failures — all TypeError-class, never need `.code` (matches
`error.rs:25-28` policy).

[OK] Funnel consistency holds. No surface regression.

---

## Dimension 4 — `#[doc(hidden)]` sweep — fresh

20 `#[doc(hidden)]` annotations across 5 files (same count as r8):

| File | All gated by `cfg(any(test, feature = "test-helpers"))`? |
|---|---|
| `lib.rs` (7) | yes — cfg-gated `pub fn`s |
| `migrations.rs` (6) | yes — `_with_pool` shims |
| `wal_consumer.rs` (4) | partial — L132/L174/L186 are `pub(crate) fn`s (carry INFO-R8-4) |
| `replication_ops.rs` (2) | yes |
| `exec.rs` (1) | yes |

No new `doc(hidden) pub fn` snuck in. Surface unchanged.

[OK] No surface holes via `doc(hidden) pub fn`.

---

## Dimension 5 — Cfg-fork test-helpers visibility — fresh enumeration

`lib.rs:48-101`:

```
Always pub:                broker, error, query, v8_classes      (4)
Always crate-private:      backend, context, crud, diff,
                           read_set, v8_bridge                   (6)
Crate-private in release,
  pub under test-helpers:  audit, exec, migrations,
                           orchestrator, replication,
                           replication_ops, wal_consumer         (7)
Crate-private in release,
  pub under
  hardening + test-helpers: auth                                 (1)  ← NEW
```

The new `auth` cfg path needs **both** features set to flip `pub`.
For default builds (`cargo build --lib`) the module isn't compiled at
all. The four-way cfg matrix:

| `test-helpers` | `hardening` | `auth` mod state |
|---|---|---|
| off | off | not compiled |
| off | on  | `pub(crate)` |
| on  | off | not compiled |
| on  | on  | `pub` |

[OK] Gate placement correct. Default build sheds the entire subtree
including all its `pub fn`s, `pub use`s, and `pub const`s.

---

## Dimension 6 — `IsolateDbContext` `pub(crate)` field surface — re-checked

The deferred-entry concern [I16]: fields like `pool`, `db_url`,
`tx_conn`, `mig_lock`, `backend`, `pending_emits` are `pub(crate)`,
so in-crate consumers *could* mutate them directly and bypass the
accessor invariants (`debug_assert!`s in `set_tx_token` /
`set_auto_tx_owned`, the new `tracing::error!` in `set_mig_lock`,
etc.).

**Verification — direct field-mutation outside `context.rs`:**

```bash
rg -n '\.(pool|db_url|registered_models|tx_conn|auto_tx_owned|tx_token|tx_token_counter|pending_emits|mig_lock|running_consumers|backend)\s*=' crates/plugin-db/src/
# 13 hits, ALL inside context.rs itself (lines 201, 202, 209, 210, 231,
# 279, 298, 306, 329, 352, 389; plus 2 docstring lines at 629-630).
# 0 hits from any consumer module.
```

```bash
rg -n 'c\.(pool|db_url|tx_conn|mig_lock|backend|pending_emits|registered_models|running_consumers|auto_tx_owned|tx_token|tx_token_counter)\b' crates/plugin-db/src/
# 17 hits — every one calls an *accessor method* on `c`
# (c.pool(), c.db_url(), c.backend(), c.tx_token(), c.auto_tx_owned()).
# Not a single field-direct dereference.
```

5d9acab8 added the `tracing::error!` inside `set_mig_lock` and the
`match` arm in `return_mig_client`, but did NOT promote either to a
panic. So a hypothetical contributor who skipped the accessor and
wrote `c.mig_lock = Some(...)` directly would bypass the new tracing
guard — but the consumer pattern is uniform: nobody does that today.
The risk is structural, not observed.

**Per-r8 deferred-entry status: unchanged.** The hardening commit and
the mig_lock tracing commit didn't add or remove any field-direct
mutation path. r9 is *equivalent* to r8 on [I16]; the fields remain
`pub(crate)` for the same reason r8 articulated (slot-by-slot removal
of legacy thread-locals in subsequent commits — the comment at
`context.rs:27-29` still references this migration).

The principled fix (privatising fields to force accessor use) is a
larger refactor than r9's review scope. r8 didn't deduct for it; r9
also doesn't — but flags it as a long-standing structural footgun.

[CARRY-NO-DEDUCT] Same posture as r8 on this dimension.

---

## Dimension 7 — `Backend` trait — fresh check

`backend/mod.rs:69-357` defines `Backend: 'static` with `Client` /
`LiveSchema` associated types, 21 `async fn` methods, all `pub`. The
module itself is `pub(crate) mod backend` in `lib.rs:55` (always
crate-private — NOT cfg-flipped by `test-helpers`), so the wide
`pub fn` surface stays intra-crate.

Consumer pattern matches trait surface:

```bash
rg -n '<B: Backend' crates/plugin-db/src/
# 4 hits — orchestrator/lock_guard.rs:117, register_model/{apply,plan,validate}.rs
# Every consumer is `pub(crate) async fn ... <B: Backend>`.
# `validate.rs` ties `B::LiveSchema = LiveSchema`; `lock_guard.rs` ties
# `B::Client = compio_postgres::Client` — consumer code expresses bounds
# at the point of use, not by re-exporting the associated types.
```

The trait isn't over-pub for outside consumers (gated by
`pub(crate) mod backend`) and isn't under-pub for in-crate consumers
(every method has at least one caller — verified by the absence of
dead-code warnings on `Backend` methods in the default build).

[OK] Trait surface matches consumer pattern.

---

## Dimension 8 — Pub re-export sweep

```bash
rg -n '^pub use' crates/plugin-db/src/
# auth/mod.rs:68-70 (3 hits, only compile when hardening on)
# backend/mod.rs:50  (PostgresBackend — intra-crate ergonomic; backend mod is pub(crate))
```

Under default build: 1 active `pub use` (`backend/mod.rs:50`). The
3 `auth/*` re-exports compile-out entirely when `hardening = off`.

[OK] No name leaks, no collisions.

---

## Dimension 9 — Pub-surface tally — coarse

Default build (no features):

```bash
rg -c '^pub (fn|struct|enum|trait|const|static|type|use|mod) ' crates/plugin-db/src/
# 153 across 30 files (r8: 123 across 28 — discrepancy is the auth files
# *with the hardening feature off*. The previous tally was an
# unconditional grep that ignored cfg gates. Re-tallying *only the
# files that compile* under default features:
```

```bash
# Files that compile under default features (no auth/*):
rg -c '^pub (fn|struct|enum|trait|const|static|type|use|mod) ' \
  $(find crates/plugin-db/src -name '*.rs' -not -path '*/auth/*')
# 138 hits across 26 files.

rg -c '^pub\(crate\) (fn|struct|enum|trait|const|static|type|use|mod) ' \
  $(find crates/plugin-db/src -name '*.rs' -not -path '*/auth/*')
# 80 hits across 23 files.
```

138 / (138 + 80) = **63.3 % crate-public** in the default build —
down from r8's 66.8 % (which included `auth/*`). The `hardening`
gate removes 15 default-build `pub` items from compilation (17
`auth/*` items, less 2 that were pre-counted to `pub(crate)` in the
prior cfg fork). Net surface shrunk.

Roughly the 138 = 12 `DbError` variants + ~6 ctor/conversion fns +
~27 query-builder + ~14 broker + ~16 v8_classes + ~30 cfg-flipped
test-helper surface + handful of plug-in glue.

---

## Findings

### [NEW-R9-1] `replication::slot_status` — `pub async fn`, zero callers

`crates/plugin-db/src/replication.rs:609`

```rust
/// Cheap probe used by the V8 `replicationStatus` callback to surface
/// the current state of an app's slot without re-emitting the full
/// SetupOutcome.
pub async fn slot_status(
    pool: &Pool,
    app_id: &str,
) -> Result<Option<serde_json::Value>, DbError> {
```

The docstring claims a "V8 `replicationStatus` callback" consumes
this. No such callback exists:

```bash
rg -n 'replicationStatus|slot_status' crates/ | grep -v docs/reviews
# Only the definition + its own docstring. Zero callers.
```

`replication` is the cfg-fork module (`pub(crate)` in release, `pub`
under `test-helpers`). The `pub async fn` therefore collapses to
`pub(crate) async fn` for external builds — but emits a `dead_code`
warning in the default build (one of the 15 surviving warnings I
verified). This is a never-called native primitive promising a JS
binding that was never wired.

- **Fix:** Either (a) wire the JS-side `replication.status()` getter
  through `v8_classes/replication.rs` and have it call `slot_status`,
  or (b) demote `pub async fn slot_status` → `pub(crate) async fn`
  + `#[allow(dead_code)]` to silence the warning while preserving the
  symbol for future wire-up. The (a) shape is the better answer
  (closes a documented-but-missing surface promise).
- **Severity:** INFO. Net surface waste: 1 `pub fn` with 0 callers
  + 1 dead-code warning + 1 false docstring promise.

### [NEW-R9-2] `set_mig_lock` docstring drift — claims `debug_assert!` that doesn't exist

`crates/plugin-db/src/context.rs:404`

```rust
/// ... paired with the `set_mig_lock` debug_asserts above).
pub fn return_mig_client(&mut self, client: Client) {
```

The docstring on `return_mig_client` references "`set_mig_lock`
debug_asserts" but `set_mig_lock` (lines 373-384) uses
`tracing::error!`, not `debug_assert!`. The commit message for
`5d9acab8` explicitly states the panicking debug_assert was
**rejected** (because unit tests deliberately exercise the
swap-on-replace shape). The docstring on `return_mig_client` was
copy-pasted from a prior draft and never updated.

- **Fix:** Trivial — rewrite the docstring tail as "(paired with the
  `set_mig_lock` tracing::error! above)".
- **Severity:** INFO. Cosmetic. Net surface impact: 0.

### [NEW-R9-3] mig_lock accessor visibility asymmetry

`crates/plugin-db/src/context.rs:373/395/405`

```rust
pub(crate) fn set_mig_lock(&mut self, lock: MigrationLock) -> Option<MigrationLock>
pub fn take_mig_client(&mut self) -> Option<Client>
pub fn return_mig_client(&mut self, client: Client)
```

The `set_mig_lock` constructor (which installs the migration lock
state — the privileged operation) is `pub(crate)`, but
`take_mig_client` / `return_mig_client` / `clear_mig_lock` /
`mig_lock_snapshot` / `has_mig_lock` are all `pub`. The lifecycle
methods that *don't* allocate the lock are reachable from outside
the crate (under `test-helpers`); the constructor isn't.

The asymmetry is harmless under the current cfg fork
(`context` is `pub(crate)` even under `test-helpers`, so neither
`pub` nor `pub(crate)` escapes the crate — they all collapse to
`pub(crate)`). But the asymmetry is still confusing: it suggests
`take`/`return`/`clear` were promoted at some point and `set` was
forgotten, or vice versa.

- **Fix:** Demote `take_mig_client`, `return_mig_client`,
  `clear_mig_lock`, `mig_lock_snapshot`, `has_mig_lock` to
  `pub(crate) fn`. Same effective visibility (because the module is
  `pub(crate)`), but the surface is honest. Or, conversely, promote
  `set_mig_lock` to `pub fn` for symmetry. Either is fine — the
  inconsistency is the bug.
- **Severity:** INFO. Net surface impact: 0 (no actual external
  reachability change).

### [CARRY] r8 INFOs — unchanged

- INFO-R8-1 `config_hinted` 0 callers (cosmetic)
- INFO-R8-2 `validation_hinted` 0 callers (cosmetic)
- INFO-R8-3 `SuppressGuard::activate` (rename to `new`)
- INFO-R8-4 `#[doc(hidden)]` on `pub(crate)` items (4 sites)
- INFO-R8-5 Configuration struct-literal vs ctor mix
- [I16] `IsolateDbContext` `pub(crate)` fields bypass-able by
  hypothetical in-crate consumer. No new bypass path observed in r8 →
  r9 commits. Carry without deduction.

---

## Plateau check — asymptote with closures applied

| Bound | What it would take | Score |
|---|---|---|
| r9 baseline (today) | — | **89** |
| + wire `slot_status` JS binding OR demote it to `pub(crate)` | NEW-R9-1; ~5 LOC | **90** |
| + fix `return_mig_client` docstring + symmetric `pub` on mig_lock accessors | NEW-R9-2, NEW-R9-3 | **91** |
| + migrate `_hinted` ctor sites or delete them + rename `SuppressGuard::activate` → `new` + drop redundant `doc(hidden)` | r8 carries | **94** |
| + privatise `IsolateDbContext` fields (force accessor use) | [I16] structural fix | **96** |
| + wire `auth::ensure_admin_schema` into the control plane | cross-crate I5 | **98** |

The previous r8 ceiling (95 under the perimeter) is recovered and
narrowly extended (96) by the `IsolateDbContext` field privatisation
option, which the hardening gate makes a smaller-scope refactor than
it used to be (the auth module no longer participates in the default
build's surface).

---

## Score

**89 / 100** (**+7 vs r8's 82**)

**Improvements that drove the +7:**

- **r8 MAJOR-R8-1 closed via `389749ca`** (+2): `backend_not_initialized`
  is now canonical across all three sites (orchestrator +
  v8_classes/{migration,migrations}) with uniform US spelling. The SDK
  branches on one code for "backend handle missing". This was the
  fresh r8 finding; closed in a single ~10-LOC PR.

- **r8 MAJOR-R7-1 (4-cycle carry) closed in-crate via `2fa9472eef`**
  (+5): the `hardening` Cargo feature gates the entire `auth/*`
  subtree out of default builds. Verified: `cargo build --lib`
  warning count drops 73 → 15; no `pub use` re-export leaks the
  subtree's types into the crate root; integration tests opt back in
  via the two-flag required-features. The four-cycle carry is
  recovered. Full closure (control-plane wire-up) is still cross-crate
  I5; that's outside plugin-db's perimeter.

**Deductions that prevented a steeper rise:**

- **NEW-R9-1 `replication::slot_status` 0-caller `pub async fn`**
  (−1): a `pub` symbol promising a "V8 `replicationStatus` callback"
  binding that doesn't exist anywhere in the workspace. Dead-code
  warning fires; docstring contract unfulfilled. Either wire it or
  demote it.

- **NEW-R9-2 / NEW-R9-3 mig_lock cosmetics** (−0, noted): docstring
  drift on `return_mig_client` (references nonexistent `debug_assert`s)
  and `pub` / `pub(crate)` asymmetry across the 6-method mig_lock
  surface. Neither reaches the SDK; flagged for hygiene.

- **r8 carry items unchanged** (−1 aggregate): `config_hinted` +
  `validation_hinted` adoption gap; `SuppressGuard::activate` rename
  opportunity; `doc(hidden)` on `pub(crate)` redundancy. Same shape
  as r5-r8.

**Score sub-ranges:**

- **90+** would require: close NEW-R9-1 (wire `slot_status` OR
  demote to `pub(crate)`).
- **91+** would require: also close NEW-R9-2, NEW-R9-3 (~3 LOC of
  visibility / docstring polish on `context.rs`).
- **94+** would require: also resolve the r8 INFO carries
  (`_hinted` ctors, `SuppressGuard::activate`, `doc(hidden)` on
  `pub(crate)`).
- **96** is the in-crate ceiling: privatise `IsolateDbContext` fields
  + force every mutation through an accessor.
- **98** is the asymptote: cross-crate I5 (wire `auth::ensure_admin_schema`
  into control-plane provisioning). Outside plugin-db's perimeter.

**Highest-leverage next move:**

```rust
// crates/plugin-db/src/replication.rs:609
- pub async fn slot_status(
+ pub(crate) async fn slot_status(
```

Plus a docstring tweak (drop the "V8 `replicationStatus` callback"
claim until the binding actually ships). Two-line change closes
NEW-R9-1.

Or, if the intent IS to ship the binding: register a `status()`
v8_method on `v8_classes/replication.rs` that calls `slot_status` —
that's the (a)-shape fix; closes the dormant-`pub` and delivers a
documented capability in one PR.
