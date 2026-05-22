# plugin-db — API Surface Review (r10)

- **Date:** 2026-05-22 (cycle 12:47)
- **Target:** `crates/plugin-db/` @ `81226451`
- **Lens:** Visibility scoping, re-exports, error envelope funnel,
  feature gating, `pub(crate)` field surface on `IsolateDbContext`,
  trait-signature change in `Backend::release_advisory_lock`.
- **Inputs since r9 (89/100, HEAD `05484878`):**
  - `403b3891` — `validate_field_name` rejects non-ASCII (I12). Signature unchanged.
  - `3b9b458d` — backlog audit only, no code change.
  - `4cab871a` — three doc/visibility cleanups: closes r9 NEW-R9-1
    (`slot_status` → `pub(crate)`), closes r9 NEW-R9-2 (docstring drift
    on `return_mig_client`).
  - `ae5570dc` — `exec.rs` unit tests for queue_or_emit / drain / clear
    (I13). Test-only.
  - `2d34061e`, `81226451` — review reports only.
  - `51c342e8` — **TRAIT signature change**: `Backend::release_advisory_lock`
    now returns `Result<(), DbError>` (was `()`). I6.
  - `fcf7ce3c` — F1 warn-half: 5 `let _ = update_audit_status(...).await;`
    sites converted to `if let Err(audit_err) = ...` + `tracing::warn!`.
  - `71a457a1` — doc-only.
  - `7c6bd2ec` — unify F1 warn-half shape across 5 sites + add
    `name`/`collection` to the `finalise_backfill` warn.

Re-walked the surface fresh; r9 verdicts re-verified by inspection,
not trusted by reference.

---

## Dimension 1 — r9 closures, verified

### r9 NEW-R9-1 `slot_status` 0-caller `pub` — **CLOSED** (`4cab871a`)

`crates/plugin-db/src/replication.rs:613`:

```rust
/// Cheap probe over `pg_replication_slots` used by future callers that
/// want the current state of an app's slot without re-emitting the
/// full `SetupOutcome`. No production caller today (api-surface r9
/// NEW-R9-1 noted the prior "V8 `replicationStatus` callback"
/// docstring was aspirational); kept `pub(crate)` so a future
/// `replicationStatus` v8_class method can adopt it without surface
/// churn.
pub(crate) async fn slot_status(
```

Visibility demoted; docstring rewritten honestly ("no production
caller today"). Dead-code warning silenced (no `#[allow(dead_code)]`
needed since the test module exercises it — see r9 footnote). Net
surface shrunk by 1 `pub fn`.

[OK] r9 NEW-R9-1 closed. +1 recovery.

### r9 NEW-R9-2 `return_mig_client` docstring drift — **CLOSED** (`4cab871a`)

`crates/plugin-db/src/context.rs:404`:

```rust
/// ... (paired with the `tracing::error!` on `set_mig_lock`'s shadow-replace
/// branch above).
pub fn return_mig_client(&mut self, client: Client) {
```

Docstring tail rewritten — no longer claims a nonexistent
`debug_assert!`. Cosmetic only; no surface change.

[OK] r9 NEW-R9-2 closed. +0 (was -0 deduction).

### r9 NEW-R9-3 mig_lock accessor asymmetry — **STILL OPEN**

`crates/plugin-db/src/context.rs:358/373/388/395/406/418`:

```rust
pub fn has_mig_lock(&self) -> bool                       // L358
pub(crate) fn set_mig_lock(...) -> Option<MigrationLock> // L373  ← only pub(crate)
pub fn clear_mig_lock(&mut self)                         // L388
pub fn take_mig_client(&mut self) -> Option<Client>      // L395
pub fn return_mig_client(&mut self, client: Client)      // L406
pub fn mig_lock_snapshot(&self) -> Option<(...)>         // L418
```

5 of 6 mig_lock accessors are `pub`; the constructor
(`set_mig_lock`) alone is `pub(crate)`. Net surface impact remains 0
(the `context` module is `pub(crate)` even under `test-helpers`, so
both forms collapse to `pub(crate)` at the crate boundary), but the
asymmetry is still confusing. r9 ceiling +1 still gated on this.

[CARRY] NEW-R9-3 unchanged. -0 deduction (cosmetic), but the cheap
path to 90+ remains: 5 LOC to demote the five `pub fn` to
`pub(crate) fn`, or one LOC to promote `set_mig_lock` to `pub fn`.

---

## Dimension 2 — `Backend::release_advisory_lock` signature change (`51c342e8`)

Before:

```rust
async fn release_advisory_lock(&self, client: &Self::Client, key1: &str, key2: &str);
```

After:

```rust
async fn release_advisory_lock(
    &self, client: &Self::Client, key1: &str, key2: &str,
) -> Result<(), DbError>;
```

### Trait + impl agreement

`crates/plugin-db/src/backend/postgres.rs:157-169` returns
`Ok(())` on success and `Err(DbError::from_pg(&e))` on driver error.
The trait docstring at `backend/mod.rs:151-157` explicitly invites
callers to treat `Err` as observability-only ("warn-and-continue").

[OK] Impl matches trait.

### Caller audit — both call sites

`crates/plugin-db/src/migrations.rs:287-297` (cancelled-refusal path):

```rust
if let Err(e) = backend.release_advisory_lock(&client, &lock_key, name).await {
    tracing::warn!(app_id, name, error = %e,
        "release_advisory_lock failed on cancelled-refusal path
         (lock auto-releases on session end)");
}
```

`crates/plugin-db/src/migrations.rs:661-671` (backfill-finalise path):

```rust
if let Err(e) = backend.release_advisory_lock(&client, &lock_key, &name).await {
    tracing::warn!(app_id, name, error = %e,
        "release_advisory_lock failed on backfill-finalise path
         (lock auto-releases on session end)");
}
```

Both follow the `OrchestratorLockGuard::release` precedent from
`ffb1e101` (code-critique MAJOR-R5-5). Zero `let _ = ...await`
patterns, zero `.unwrap()`, zero discarding `?` propagation
(the unlock is best-effort and shouldn't propagate failure up the
migration error rail).

```bash
rg -n 'let _ = .*release_advisory_lock' crates/plugin-db/src/   # 0 hits
rg -n '\.release_advisory_lock\(' crates/plugin-db/src/         # 2 call sites + 2 defs
```

[OK] Trait signature flip propagates cleanly. No leaks.

### SDK-facing impact

`release_advisory_lock` is a `Backend` trait method invoked only by
the orchestrator and migration helpers. Both call sites swallow the
`Err` at the tracing layer — it never reaches `DbError::to_op_error()`
nor any `RejectError(...)` path. SDK contract unchanged.

[OK] No `OpError` envelope leak from the new trait signature.

---

## Dimension 3 — F1 warn-half shape (`fcf7ce3c` + `7c6bd2ec`)

5 sites unified on the same `tracing::warn!` field shape:

| Site | Fields (in order) |
|---|---|
| `apply.rs:178-184` ("Applied" terminal-status on Ok) | `app_id`, `audit_id`, `transition="Applied"`, `audit_err` |
| `apply.rs:209-216` ("Failed" terminal-status on DDL Err) | `app_id`, `audit_id`, `transition="Failed"`, `ddl_err`, `audit_err` |
| `backend/postgres.rs:497-503` (INVALID-index loop) | `app_id`, `audit_id`, `transition="Failed/invalid_index"`, `attempt`, `audit_err` |
| `backend/postgres.rs:546-555` (data-violation retry) | `app_id`, `audit_id`, `transition="Failed/data_violation"`, `sqlstate`, `audit_err` |
| `backend/postgres.rs:597-605` (transient/non-transient build) | `app_id`, `audit_id`, `transition="Failed/index_build"`, `attempt`, `transient`, `audit_err` |

Common skeleton: `app_id`, `audit_id`, `transition`, `audit_err`,
identical message. Operators can `grep '"update_audit_status failed"'`
or filter on `transition=...` for slice-and-dice.

The 6th site in `migrations.rs:647-657` (the `finalise_backfill`
warn) is a **separate shape family** — `app_id`, `name`, `collection`,
`audit_id`, `terminal`, `error`. The commit message of `7c6bd2ec`
described the alignment as "in line with the I6 release_advisory_lock
warn 12 lines below" (the lock-release warn, NOT the F1 family). The
distinction is:

- F1 family (5 sites) = `update_audit_status` *secondary* failure.
- finalise_backfill family (1 site) + release_advisory_lock family
  (2 sites) = primary helper failure where row identity matters.

Both families are log-only; neither traverses `to_op_error()` or
`RejectError(...)`. **No `OpError` envelope leak.** Verified:

```bash
rg -n 'to_op_error|OpError::|RejectError\(' \
   crates/plugin-db/src/orchestrator/register_model/apply.rs
# 1 docstring hit; 0 code-level hits in the F1 warn blocks.
```

[OK] F1 warn-half shape is log-only, operationally consistent within
each family, and orthogonal to the SDK error contract.

---

## Dimension 4 — `#[doc(hidden)]` sweep

Same 20 hits as r9, same files, same gating:

| File | Count | Cfg-gated? |
|---|---|---|
| `lib.rs` | 7 | all under `cfg(any(test, feature = "test-helpers"))` |
| `migrations.rs` | 6 | all `_with_pool` shims, cfg-gated |
| `wal_consumer.rs` | 4 | partial — 3 are `pub(crate) fn` + `doc(hidden)` (carry INFO-R8-4) |
| `replication_ops.rs` | 2 | cfg-gated |
| `exec.rs` | 1 | cfg-gated |

No new `#[doc(hidden)] pub fn` introduced. The wal_consumer carry is
unchanged from r8/r9 — `pub(crate) fn` with redundant `doc(hidden)`,
cosmetic only.

[OK] No surface holes via `doc(hidden) pub fn`.

---

## Dimension 5 — Cfg-fork visibility, fresh re-enumeration

`lib.rs:58-111`:

```
Always pub:                broker, error, query, v8_classes        (4)
Always crate-private:      backend, context, crud, diff,
                           read_set, v8_bridge                     (6)
Crate-private in release,
  pub under test-helpers:  audit, exec, migrations,
                           orchestrator, replication,
                           replication_ops, wal_consumer           (7)
Crate-private in release,
  pub under
  hardening + test-helpers: auth                                   (1)
```

Identical to r9. The `release_advisory_lock` flip lives inside
`backend/`, which is `pub(crate) mod backend` always — so the
signature change doesn't widen the crate's external surface in any
build configuration.

[OK] Gate placement unchanged. Default build sheds `auth/*` entirely
(both `cfg!(feature="hardening")` arms compile-out when the feature
is off).

---

## Dimension 6 — `IsolateDbContext` `pub(crate)` field surface

Re-checked by direct grep:

```bash
rg -n '\.(pool|db_url|registered_models|tx_conn|auto_tx_owned|tx_token|tx_token_counter|pending_emits|mig_lock|running_consumers|backend)\s*=' \
   crates/plugin-db/src/
# 13 hits — all inside context.rs (L201/202/209/210/231/279/298/306/329/352/389 + 2 docstrings)
# 0 hits from any consumer module.

rg -n 'c\.(pool|db_url|tx_conn|mig_lock|backend|pending_emits)\b' \
   crates/plugin-db/src/
# 17 hits — every one calls an *accessor method*, not field-direct.
```

[CARRY-NO-DEDUCT] [I16] unchanged. The release_advisory_lock flip
didn't touch field access patterns. Same posture as r8/r9.

---

## Dimension 7 — `Backend` trait — fresh check

The trait shape post-`51c342e8`: 21 `async fn` methods, all `pub`,
inside a `pub(crate) mod backend` — same envelope as r9, with one
return type widened. The compile-time tests in `backend/mod.rs:445-454`
assert `PostgresBackend` implements `Backend` — these would fail at
build time if `release_advisory_lock`'s impl drifted from the trait,
giving us a structural canary for future signature edits.

```bash
rg -n '<B: Backend' crates/plugin-db/src/
# 4 hits — orchestrator/lock_guard.rs:117, register_model/{apply,plan,validate}.rs
# Same consumer pattern as r9.
```

[OK] Trait surface still scopes correctly.

---

## Dimension 8 — Pub re-export sweep

```bash
rg -n '^pub use' crates/plugin-db/src/
# auth/mod.rs:68-70 (3 hits, only compile when hardening on)
# backend/mod.rs:50 (PostgresBackend — intra-crate, backend is pub(crate))
```

Under default build: 1 active `pub use` (the `PostgresBackend`
re-export, intra-crate only since `mod backend` is `pub(crate)`).

[OK] No name leaks, no collisions. Unchanged from r9.

---

## Dimension 9 — Pub-surface tally — recount

r9's count regex omitted `async fn`. Recounting properly:

```bash
find crates/plugin-db/src -name '*.rs' -not -path '*/auth/*' |
  xargs grep -cE '^pub (async fn|fn|struct|enum|trait|const|static|type|use|mod) '
# Sum: 173

find crates/plugin-db/src -name '*.rs' -not -path '*/auth/*' |
  xargs grep -cE '^pub\(crate\) (async fn|fn|struct|enum|trait|const|static|type|use|mod) '
# Sum: 96
```

Default build: **173 pub / 96 pub(crate) = 64.3% pub**. The
`slot_status` demotion (`4cab871a`) moved 1 from the `pub`
bucket to `pub(crate)`. The signature-only flip on
`release_advisory_lock` didn't change the count.

(r9's reported 138/80 is the same metric *without* counting
`async fn` lines; rerun under the same regex against HEAD `81226451`
yields 137/81 — a net 1-item shrink. The bucket motion matches the
single `slot_status` demotion.)

[OK] Surface shrunk by 1 `pub fn`; no new external promotions.

---

## Findings

### [NEW-R10-1] `pub` / `pub(crate)` asymmetry on the 6 mig_lock accessors — **CARRY from NEW-R9-3**

Unchanged at `context.rs:358/373/388/395/406/418`. r9 documented
this fully; no commit in the cycle 11-12 range touched the
visibility shape. Net surface impact: 0. Hygiene-only. -0 deduction.

### [CARRY] r8 INFOs — unchanged

- INFO-R8-1 `config_hinted` 0 callers (cosmetic).
- INFO-R8-2 `validation_hinted` 0 callers (cosmetic).
- INFO-R8-3 `SuppressGuard::activate` (rename to `new`).
- INFO-R8-4 `#[doc(hidden)] pub(crate)` items (4 sites, wal_consumer).
- INFO-R8-5 Configuration struct-literal vs ctor mix.
- [I16] `IsolateDbContext` `pub(crate)` fields — no new bypass path.

### [NEW-R10-OBSERVATION] F1 warn-half is two shape families, not one

The `7c6bd2ec` commit message implies "unified shape across 5 sites
+ finalise_backfill = 6 sites." On inspection, the actual shape
unification covers only the 5 `update_audit_status` sites (F1
proper); the `finalise_backfill` warn uses a different (but
internally coherent) shape that matches the `release_advisory_lock`
warn instead. This is **not a bug** — it reflects two genuinely
different secondary-failure categories — but the commit message
phrasing could mislead a future reader doing operator-side log
filtering. Hygiene observation only. -0.

---

## Plateau check — asymptote with closures applied

| Bound | What it would take | Score |
|---|---|---|
| r10 baseline (today) | — | **91** |
| + symmetric `pub`/`pub(crate)` on mig_lock accessors | NEW-R9-3 carry; 5 LOC | **92** |
| + migrate `_hinted` ctor sites or delete + rename `SuppressGuard::activate` → `new` + drop redundant `doc(hidden)` | r8 carries | **94** |
| + privatise `IsolateDbContext` fields (force accessor use) | [I16] structural fix | **96** |
| + wire `auth::ensure_admin_schema` into the control plane | cross-crate I5 | **98** |

The r9 ceiling (96 in-crate, 98 asymptote) is preserved — the cycle
11-12 commits closed two of r9's three findings without raising any
new structural ones.

---

## Score

**91 / 100** (**+2 vs r9's 89**)

**Improvements that drove the +2:**

- **r9 NEW-R9-1 closed via `4cab871a`** (+1): `slot_status` demoted
  to `pub(crate) async fn`; docstring rewritten to drop the
  aspirational "V8 `replicationStatus` callback" claim. Net surface
  shrinks by 1 `pub fn`. Dead-code warning resolved.

- **r9 NEW-R9-2 closed via `4cab871a`** (+0, but reduces noise): the
  `return_mig_client` docstring no longer references a nonexistent
  `debug_assert!`. Already a -0 in r9; closure leaves it tidy.

- **`Backend::release_advisory_lock` signature flip executed cleanly**
  (+1): the trait return type widened from `()` to `Result<(), DbError>`,
  both call sites adopt the `if let Err(e)` + structured warn shape,
  no unwrap-discarding, no `let _ = ...await`, no `?`-propagation
  that would have surfaced the best-effort unlock as a JS error. The
  pattern mirrors the established `OrchestratorLockGuard::release`
  precedent (MAJOR-R5-5 / I44). The 12 LOC of caller updates landed
  with zero leaks into the SDK error envelope.

- **F1 warn-half landed without `OpError` contamination** (+0,
  worth noting): 5 sites converted from `let _ = update_audit_status`
  to structured `tracing::warn!`. All log-only; none traverses
  `to_op_error()`. The shape is now operationally greppable
  (`transition="..."`, `audit_err=...`). The 6th site
  (`finalise_backfill`) belongs to a separate family with its own
  consistent shape.

**Deductions / carries preventing a steeper rise:**

- **NEW-R9-3 mig_lock asymmetry still open** (-0, but the cheap
  +1 ceiling is gated on it): 5 of 6 accessors are `pub`, the
  constructor alone is `pub(crate)`. Net external surface impact 0
  because the module is `pub(crate)`, but the inconsistency reads
  like an unfinished refactor.

- **r8 INFO carries** (-1 aggregate): same shape as r5-r9 —
  `_hinted` ctors with 0 callers, `SuppressGuard::activate` naming,
  redundant `#[doc(hidden)]` on `pub(crate) fn`.

**Score sub-ranges:**

- **92+** would require: close NEW-R9-3 (5 LOC of `pub`/`pub(crate)`
  alignment on the mig_lock accessors).
- **94+** would require: also resolve the r8 INFO carries.
- **96** is the in-crate ceiling: privatise `IsolateDbContext`
  fields, force every mutation through an accessor.
- **98** is the asymptote: cross-crate I5 (wire
  `auth::ensure_admin_schema` into control-plane provisioning).

**Highest-leverage next move:**

```rust
// crates/plugin-db/src/context.rs:358/388/395/406/418
- pub fn has_mig_lock(&self) -> bool
- pub fn clear_mig_lock(&mut self)
- pub fn take_mig_client(&mut self) -> Option<Client>
- pub fn return_mig_client(&mut self, client: Client)
- pub fn mig_lock_snapshot(&self) -> Option<(...)>
+ pub(crate) fn has_mig_lock(&self) -> bool
+ pub(crate) fn clear_mig_lock(&mut self)
+ pub(crate) fn take_mig_client(&mut self) -> Option<Client>
+ pub(crate) fn return_mig_client(&mut self, client: Client)
+ pub(crate) fn mig_lock_snapshot(&self) -> Option<(...)>
```

5-LOC visibility-keyword sweep closes NEW-R9-3 (carry into NEW-R10-1),
reaches 92.

**Sanity check on the trait flip's blast radius:**

```bash
rg -n '\.release_advisory_lock\(' crates/plugin-db/src/        # 2 callers, both warn-on-Err
rg -n 'release_advisory_lock' crates/plugin-db/src/            # 2 defs + 2 callers + 2 log-message strings
rg -n 'release_advisory_lock' crates/ --glob '!plugin-db/'     # 0 — no cross-crate consumer
rg -n 'release_advisory_lock' sdks/ examples/ tests/           # 0 — no SDK / e2e surface
```

The trait method is fully contained inside plugin-db; no cross-crate
or SDK call sites needed updates. I6 is a clean, scope-bounded fix.
