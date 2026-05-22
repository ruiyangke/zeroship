# plugin-db architecture review — 2026-05-22 r11

Round 11. HEAD `6cfa98df`. Fresh re-walk; pilot asked specifically
about the `f6adb68b` field-privatize commit (cycle 13:47), the
`251d53b4` row_to_json index-lookup fix (cycle 13:17), and 4-cycle
status on the two big C-line findings (Backend trait, query.rs).

r10 score: **93** ("plateau", asymptote 94-95, ceiling 97-98). r10
sat behind a then-uncommitted `hardening` cfg-gate; that gate landed
in `2fa9472e` and the M-R10-1 conditional is closed.

---

## Crate Map (delta vs r10)

| File / Module | r10 LOC | **r11 LOC** | Δ | Score |
|---|---:|---:|---:|---:|
| `lib.rs` | 374 | 374 | 0 | 78 |
| `context.rs` | 943 | **975** | +32 | **94** (+2) |
| `error.rs` | 880 | 880 | 0 | 96 |
| `query.rs` | 4275 | **4315** | +40 | 78 |
| `crud.rs` | 585 | 585 | 0 | 86 |
| `exec.rs` | 491 | 491 | 0 | 90 |
| `audit.rs` | 894 | 894 | 0 | 84 |
| `migrations.rs` | 958 | **983** | +25 | 78 |
| `diff.rs` | 1107 | 1107 | 0 | 80 |
| `read_set.rs` | 583 | 583 | 0 | 80 |
| `replication.rs` | 960 | 960 | 0 | 86 |
| `replication_ops.rs` | 425 | 425 | 0 | 92 |
| `wal_consumer.rs` | 1352 | 1352 | 0 | 78 |
| `broker.rs` | 1311 | 1311 | 0 | 86 |
| `v8_bridge.rs` | 497 | **505** | +8 | 86 (+1) |
| `auth/` | 2040 | 2040 | 0 | 65 |
| `backend/mod.rs` | 446 | 455 | +9 | 88 |
| `backend/postgres.rs` | 726 | 766 | +40 | 86 |
| `orchestrator/auto_tx.rs` | 384 | 384 | 0 | 86 |
| `orchestrator/transaction.rs` | 174 | 174 | 0 | 87 |
| `orchestrator/lock_guard.rs` | 383 | 383 | 0 | 88 |
| `orchestrator/register_model/*` | 1042 | 1042 | 0 | 88 |
| `v8_classes/*` | 2778 | 2778 | 0 | 89 |

LOC growth: query.rs +40 (test expansions, not new functions);
context.rs +32 (docstring rewrite on the privatize commit + tracing
adds); migrations.rs +25 (warn-half + I6 typed-error
threading). **No new modules, no removed modules.**

Default-build warning floor (`cargo build -p zeroship-plugin-db
--release`, filtered to `crates/plugin-db/src/`): **15 warnings**,
down from r10's reported 31. The reduction is the hardening gate's
auth subtree no longer compiling by default — 16 dead-code warnings
in `auth/*` removed.

---

## Dependency Graph

Unchanged from r10. Same edges, same leakage:

```
v8_classes/*  ───▶ orchestrator/*  ───▶ backend/*  ───▶ compio_postgres
       │              │  └──▶ context  └──▶ audit         (driver layer)
       │              │                     diff
       └─▶ crud  ──▶ exec  ──▶ query    migrations
                     │
                     └──▶ broker (subscription bus) ←── wal_consumer ← replication
                          read_set

[ auth/* ← cfg-gated under `hardening`, isolated subtree ]
```

Leakage edges (re-confirmed):
- `audit.rs` — `Pool` / `Row` direct.
- `diff.rs` — `Pool` direct (introspection queries).
- `replication.rs` / `wal_consumer.rs` — PG-only, documented.
- `migrations.rs` — 7 `Rc<compio_postgres::Pool>` test-only sigs.
- `orchestrator/register_model/mod.rs:249` — one ad-hoc
  `PostgresBackend::new(pool, url)` construction.

No circular deps. No god-module other than `query.rs`.

---

## Audit responses (pilot's specific questions)

### Q1: Did `f6adb68b` (privatize 11 `IsolateDbContext` fields) tighten the consumer→state boundary as expected?

**Yes.** The 11 data fields (`pool`, `db_url`, `registered_models`,
`tx_conn`, `auto_tx_owned`, `tx_token`, `tx_token_counter`,
`pending_emits`, `mig_lock`, `running_consumers`, `backend`) are now
**private** (was `pub(crate)`). Every consumer routes through a
typed accessor on the `impl` block. Compile-time enforcement: any
new in-crate caller that wants to touch a slot must invoke a method
that carries the invariant checks (`set_tx_token` debug-asserts
"non-zero token implies active tx_conn"; `set_auto_tx_owned(true)`
asserts "active tx_conn"; `set_mig_lock` traces shadow-replace).

The `tx_token_counter` case is the cleanest single win: only
`next_tx_token()` can now increment it, eliminating the latent
"surprise reset somewhere in the crate" risk that the field's prior
`pub(crate)` visibility allowed.

### Q2: Re-walk the 16 external `crate::context::with(|c| ...)` sites — any awkward enough to want a richer accessor?

I counted **16 sites** matching `crate::context::with(|c|` across 8
files (33 if I include `with_mut`). Each call extracts exactly one
field. Walking them:

| File | Line | Accessor called | Awkwardness |
|---|---:|---|---|
| migrations.rs | 200 | `mig_lock_snapshot()` | ok |
| migrations.rs | 228 | `has_mig_lock()` | ok |
| migrations.rs | 902 | `db_url()` | ok |
| replication_ops.rs | 199 | `is_consumer_running(&app_id)` | ok |
| replication_ops.rs | 244 | `db_url()` | ok |
| replication_ops.rs | 321 | `is_consumer_running(app_id)` | ok |
| v8_classes/migration.rs | 109 | `backend()` | ok |
| v8_classes/migration.rs | 268 | `backend()` | ok |
| v8_classes/migrations.rs | 173 | `backend()` | ok |
| v8_classes/transaction.rs | 124 | `tx_token()` | ok |
| v8_classes/transaction.rs | 229 | `tx_token()` | ok |
| orchestrator/transaction.rs | 116 | `has_tx()` | ok |
| orchestrator/transaction.rs | 146 | `db_url()` | **patterned** |
| orchestrator/auto_tx.rs | 189 | `has_tx()` | **patterned** |
| orchestrator/auto_tx.rs | 198 | `db_url()` | **patterned** |
| orchestrator/auto_tx.rs | 237 | `auto_tx_owned()` | ok |

**The only "would benefit from a richer accessor" pattern** is the
pair-call sequence in `exec_begin` (transaction.rs:116, 146) and
`exec_auto_begin` (auto_tx.rs:189, 198). Both files do:

```rust
let has_tx = crate::context::with(|c| c.has_tx());
if has_tx { return Err(...); }
// ... validate begin_sql ...
let url = crate::context::with(|c| c.db_url()).ok_or_else(...)?;
```

Two `with(|c|)` calls back-to-back, both reading from the same
single-threaded `RefCell`. A richer accessor —

```rust
fn snapshot_for_begin(&self) -> Result<&str, DbError>
// returns: Err(tx_already_active) if has_tx, else Ok(db_url)
```

— would collapse them. But the snapshot would have to either return
an owned `String` (double-clone) or surface lifetime through the
`with` closure (which the current API explicitly avoids). The
fold-in saves 1 borrow-cycle per begin, which is in the noise versus
the `compio_postgres::connect` round-trip that follows. **Not worth
it.**

Verdict: the 1-field-per-call pattern is right for this API. The
boundary is clean. No new accessor needed.

### Q3: Did `251d53b4` (I35 index-lookup fix) suggest a `ColumnDecoder` trait refactor?

**No, and the refactor would be premature.** The fix is purely
mechanical: `row_to_json` was passing `col.name(): &str` into
`row.try_get` / `row.raw_value`, which compio_postgres resolves via
`RowIndex for str` (linear scan of `row.columns()`). The fix swaps
to `enumerate().map((idx, col))` and threads `idx: usize`, hitting
the O(1) `RowIndex for usize` impl. Net: O(N²) → O(N) per row.

The `column_to_json` body is **still a 13-arm OID match**
(BOOL/INT2/INT4/INT8/FLOAT4/FLOAT8/UUID/TIMESTAMP/TIMESTAMPTZ/DATE/JSONB/JSON/NUMERIC/_).
The pilot's "ColumnDecoder trait, one impl per OID" question:

The OID-match arm has 4 distinct decoding strategies:
1. **`try_get::<T>` typed** (BOOL/INT2/INT4/INT8/FLOAT4/FLOAT8/UUID).
2. **`raw_value` + byte arithmetic** (TIMESTAMP, TIMESTAMPTZ, DATE,
   JSONB).
3. **`try_get::<String>` + parse** (JSON, NUMERIC).
4. **`try_get::<String>` text fallback** (TEXT, VARCHAR, default).

A `ColumnDecoder` trait would have to be either:
- **Object-safe** (`Box<dyn ColumnDecoder>`), which means dispatch
  cost per column per row (vs. the current match-arm which compiles
  to a jump table).
- **Static-dispatch** via generics, which would force the OID lookup
  to a `match` on the type-erased dispatch arm — same shape as
  today.

**A second backend (SQLite) does not need this trait.** SQLite's
column-decoding model is type-affinity, not OID-tagged; the parallel
impl would share zero code with the current PG OID-match. The right
refactor when a second backend lands is not "extract a column
decoder trait" but "let each backend own its row-to-json walker
behind the existing `Backend::Row` associated type."

**Verdict**: no architectural refactor warranted. The OID match arm
is the right shape. The I35 fix was a perf bug, not a structural
signal.

There IS one minor architectural opportunity in `v8_bridge.rs:353`:
`row_to_json` is currently `pub(crate)`, but `column_to_json` (the
OID-match worker) is private. If a second backend lands and wants
to reuse the OID-match (e.g. a Postgres-compatible Citus/CockroachDB
backend), the worker should be `pub(crate)` and the per-backend
walker should call into it. Not a r11 action; flag for the
hypothetical second-backend cycle.

### Q4: 4-cycle status of C1 (Backend trait half-applied)?

**Unmoved.** The 8 call sites that name `&PostgresBackend` instead
of `B: Backend`:

```
orchestrator/register_model/mod.rs:48    use ... PostgresBackend
orchestrator/register_model/mod.rs:162   run_pipeline(backend: &PostgresBackend, ...)
orchestrator/register_model/mod.rs:249   PostgresBackend::new(pool, url)
orchestrator/register_model/bootstrap.rs:27,79,150  PostgresBackend
context.rs:37,166,209,222                Rc<PostgresBackend>
v8_classes/transaction.rs:50             <PostgresBackend as Backend>::Client
migrations.rs:48,57,206,363,448,475,690,731,764,901,903  PostgresBackend
```

Cycle change since r10: `51c342e8` (I6 closure) added one **new**
`B: Backend`-aware call site path (`release_advisory_lock` now
returns `Result`), but the file's `&PostgresBackend` callers
remain. Net: no movement on the C1 surface. The trait is still
named, half-applied, intentional.

Disposition: **same as r10 — worth +1 if closed, no impact if
deferred.** A second backend is still the right forcing function.

### Q5: 4-cycle status of C2 (`query.rs` monolith)?

**Worse by 40 LOC.** query.rs grew from 4275 → **4315** since r10.
The growth is test additions (cycle 11:17 landed I12 non-ASCII
validation + cycle 11:17 added unit tests for `queue_or_emit`),
not new production functions. The test/SUT ratio is still ~3:1
inside the file.

The architecture r10 split plan (identifier / schema_ddl / filter /
crud_builders / aggregate / tests sub-modules) is **unchanged in
disposition** and **unchanged in urgency**. The file is now the
single biggest navigation cost in the crate and the worst single
target of any rename / SQLSTATE-class refactor.

Disposition: **same as r10**. Status: candidate, not blocker.

---

## Re-walk of r10 IMPORTANT carries (I-R10-1..4)

| Carry | Subject | r10 disposition | r11 status |
|---|---|---|---|
| **I-R10-1** | cfg-fork test-helpers proliferation (8 → 9 pairs) | "Status quo until 12 pairs" | **Unchanged.** Still 9 pairs in `lib.rs` (8 + the `hardening` 3-arm). |
| **I-R10-2** | Backend trait half-applied (8 PostgresBackend bindings) | "Worth +1 if closed" | **Unchanged.** No call site flipped; the trait surface gained one new `Result`-returning method (I6) but no consumer site stopped naming the concrete impl. |
| **I-R10-3** | `auto_tx.rs` / `transaction.rs` 6-step recipe duplication | "Worth a consolidator helper" | **Unchanged.** Both files still duplicate the BEGIN-with-isolation / connect / spawn / install_tx_client / clear_pending_emits chain. |
| **I-R10-4** | `into_held` dead code (M7 deadline) | "Delete in r11" | **Unchanged. OVERDUE.** `lock_guard.rs:197` still has `pub(crate) fn into_held`. The test at `:276` exercises it; the 3 callers cited in r10 are still absent from production. **Action: delete or wire in r12.** |
| **I-R10-5** | `BackendHandle` alias dead | "Adopt or delete" | **Unchanged.** `backend/mod.rs:374`. Still dead-code-warned. `context.rs:166` still types backend slot as `Option<Rc<PostgresBackend>>`, not `Option<BackendHandle>`. |

**0 of 5 closed.** All 5 IMPORTANTs carried into r12 with zero
movement.

---

## Issues

### CRITICAL

(none — clean since r7)

### MAJOR

#### M-R11-1 (was M-R10-1): hardening cfg-gate is **closed**

Working-tree state from r10 landed in `2fa9472e` (cycle 10:47). The
M-R10-1 conditional is **resolved**. Plugin-db default-build
warning floor: **15** (was r10's 31). 16-warning auth subtree dead-
code drift now compile-out by default. **CLOSED.**

#### M-R11-2 (was M-R10-2): query.rs at 4315 LOC

The file grew +40 LOC since r10 (test-only, no new pub fns). The
disposition is unchanged. The split plan (identifier / schema_ddl /
filter / crud_builders / aggregate / tests) remains the right shape.

Carries to r12.

### IMPORTANT

All 5 r10 IMPORTANTs carry to r11 with **0 movement** (see table
above). New IMPORTANTs:

#### I-R11-1 (NEW): cycle's `pub(crate)` sweep on context.rs surfaces a residual asymmetry

`f6adb68b` privatized 11 data fields. `bac64c0e` (cycle 13:17)
demoted 5 `mig_lock` accessors to `pub(crate)`. The combined effect:
**`IsolateDbContext` accessors are now mostly `pub(crate)`** — but
the field/method visibility surface is still **asymmetric in one
direction**.

Walking the accessor visibility:
- 11 fields: **private** (post `f6adb68b`)
- mig_lock accessors (6): **pub(crate)** (post `bac64c0e`)
- pool accessors (`pool`, `pool_initialised`, `set_pool`,
  `clear_pool`, `backend`): **pub** ← still
- db_url accessors (`db_url`, `set_db_url`): **pub** ← still
- tx accessors (`has_tx`, `install_tx_client`, `take_tx_client`,
  `put_tx_client`, `tx_token`, `set_tx_token`, `next_tx_token`,
  `auto_tx_owned`, `set_auto_tx_owned`): **pub** ← still
- pending_emits (`push_pending_emit`, `drain_pending_emits`,
  `clear_pending_emits`): **pub** ← still
- registered_models (`is_model_registered`,
  `mark_model_registered`): **pub** ← still
- consumer (`is_consumer_running`, `mark_consumer_running`,
  `unmark_consumer_running`): **pub** ← still

The crate has no consumers of the `pub` accessors **outside the
crate** (verified: `crate::context::with(|c| ...)` is the only entry
shape, and `IsolateDbContext` isn't re-exported from `lib.rs`). The
visibility could be tightened to `pub(crate)` across the board with
zero external impact.

Recommendation: do the sweep — demote ~25 `pub fn` accessors on
`IsolateDbContext` to `pub(crate) fn`. The api-surface lens has
been the +2-step lens here for 2 cycles; this is the next step.

Worth: +1 boundary, +1 api-surface. Mechanical edit.

#### I-R11-2 (NEW): `set_pool` constructs `PostgresBackend` directly — Backend trait isn't even used here

`context.rs:209`:

```rust
pub fn set_pool(&mut self, pool: Rc<Pool>) {
    let url = self.db_url.clone().unwrap_or_default();
    self.backend = Some(Rc::new(PostgresBackend::new(Rc::clone(&pool), url)));
    self.pool = Some(pool);
}
```

The context — the single typed home for per-isolate state — names
`PostgresBackend` concretely. This is the **bottom of the
half-application iceberg**: even the place that exists explicitly to
let consumers stop naming the concrete type, names it.

The fix isn't immediate (the constructor needs the URL, which the
trait's `Backend::open(url)` would supply). But adding a
`Backend::open(url: &str, pool: Rc<Pool>) -> Self` factory method to
the trait, then having `context::set_pool` accept a generic factory
closure, would close I-R11-2 + a portion of I-R10-2 + I-R10-5
together.

This is the "one more turn of the trait crank" lever. Worth +1 to
+2 on coupling debt if pursued.

#### I-R11-3 (NEW): `migrations.rs::create_ad_hoc_backend` is a test-only escape hatch on an in-crate fn

`migrations.rs:895-903`:

```rust
/// Build an ad-hoc PostgresBackend wrapping an owned `Rc<Pool>`.
/// Used by the `_with_pool` test helpers ...
fn create_ad_hoc_backend(pool: Rc<Pool>) -> crate::backend::PostgresBackend {
    let url = crate::context::with(|c| c.db_url()).unwrap_or_default();
    crate::backend::PostgresBackend::new(pool, url)
}
```

This is the 7th `_with_pool` test-only seam r10 flagged in
migrations.rs, and the only one that **constructs a concrete
`PostgresBackend` in a non-`context::set_pool` location**. The
`url.unwrap_or_default()` defaulting to `""` is also suspicious — a
backend constructed with an empty `url` would explode on the first
advisory-unlock attempt, which the test helpers don't currently
exercise.

Recommendation: make the test helpers take an `&dyn Backend` and
delete `create_ad_hoc_backend`. Aligns the test surface with the
production surface. Status: judgment-call, carries to r12.

### MINOR

- `read_set.rs::Active::{begin,take}` — still dead. r10 minor.
- `audit.rs::ActorKind::{Validation,Backfill}` — still dead. r10 minor.
- `wal_consumer.rs::{any_app_suppressed, set_local_emit_suppressed}` — still dead. r10 minor.
- `diff.rs::count_violating_not_null` and field-dead-codes — still dead. r10 minor.
- `migrations.rs::release_active_lock` — still dead, still
  warning-emitted at non-`cfg(test)` build. Add
  `#[cfg_attr(not(test), allow(dead_code))]` per r10 recommendation. r10 minor.
- `replication.rs::slot_status` — dead (caller in
  replication_ops.rs is `running_consumers_for_app` which never
  calls into it). r10 missed this. New minor.
- `v8_bridge.rs::setup_promise` — dead. `setup_js_promise` is the
  one in use. Removed call site landed in some prior cycle without
  removing the helper. New minor.

7 dead-code minors (was 6 at r10).

---

## Strengths

1. **The privatize commit is the cleanest single architectural move
   of the last 4 cycles.** It closes a real-but-latent
   invariant-bypass risk (`tx_token_counter` was reachable from
   anywhere in-crate via `pub(crate)`) with **zero downstream
   churn** — every consumer was already using accessors. This is
   the rare "free win" architectural refactor.

2. **`IsolateDbContext` is now the single best-bounded structure in
   the crate.** Field invariants enforced at the type level; all 16
   external read sites through accessor methods; lifecycle
   documented inline on each field. The 10-thread-local
   consolidation paid off twice now (stage 8d-R4 the first time;
   this round the second time).

3. **`row_to_json` index-lookup fix is a clean perf bug
   resolution** — diagnosed and fixed without architectural
   ripples. The OID-match shape is intentionally retained per the
   "second-backend question is hypothetical" disposition. No
   over-engineering.

4. **Hardening gate stable across 3+ cycles.** Default-build
   warning floor: 15 (down from 74 pre-r10 / 31 at r10's
   reported-after measurement). The boundary is structurally
   stable: every new commit since the gate landed has built clean
   under the default features.

5. **The F1 warn-half family** (5d9acab8 / fcf7ce3c / 7c6bd2ec /
   18aee490) shipped 6 sites with identical structured-field
   shape. Operator-grep contract is consistent (`audit_err =
   %audit_err`, `transition = "..."`). Pattern saturation in a
   non-error-rail axis.

6. **I6 typed-error round-trip** (51c342e8): `release_advisory_lock`
   now returns `Result<(), DbError>`; callers `tracing::warn!` on
   Err. The trait surface gained one method without gaining one
   concrete-impl call site. The Backend trait is **narrower but
   richer** — same shape as r10.

7. **No new modules**; LOC growth is all test expansion and
   docstring rewrites; the structural ambitions of the crate are
   over. Asymptotic-polish phase.

---

## Scores (1-100)

| Dimension | R9 | R10 | **R11** | Δ |
|---|---:|---:|---:|---:|
| Module boundaries | 65 | 67 | **70** | +3 |
| Layering pipeline | 89 | 89 | 89 | 0 |
| Extension points | 66 | 66 | 66 | 0 |
| Coupling | 85 | 86 | **87** | +1 |
| Forward extensibility | 76 | 76 | 76 | 0 |
| Coupling debt | 63 | 65 | **67** | +2 |
| Error rail discipline | 96 | 96 | **97** | +1 |
| Security | 94 | 94 | 94 | 0 |
| Performance | 75 | 75 | **77** | +2 |
| API surface | 72 | 73 | **76** | +3 |
| Pattern consolidation | 82 | 82 | **83** | +1 |

**Module boundaries +3**: `f6adb68b` privatize commit + `bac64c0e`
mig_lock-accessor demote together close the visibility-asymmetry
gap that was the biggest single structural finding at r10's level.
The 11 fields are private; the 16 external sites all go through
named accessors. The "what's allowed to touch the state machine?"
question now has a compile-time answer.

**Coupling +1, Coupling debt +2**: same chain. The
private-field invariant says "no, you cannot just twiddle this
slot"; future contributors are forced to either route through an
existing accessor or add one (with documented invariants).

**Error rail discipline +1**: I6 closure (`release_advisory_lock`
typed-error return) + F1 warn-half (6 sites unified) — both
incremental, both in the same axis the error rail has been
saturating since r5.

**Performance +2**: `251d53b4` (row_to_json O(N²) → O(N)) is the
first measured-cost architectural fix in 3 cycles. The win is
real and the structural shape (`column_to_json` worker fn) doesn't
change.

**API surface +3**: `bac64c0e` demoted 5 mig_lock accessors;
combined with the `f6adb68b` field-privatize, the public surface of
`IsolateDbContext` shrunk meaningfully. r11 I-R11-1 (sweep ~25 more
to `pub(crate)`) is the next +2 step.

**Pattern consolidation +1**: F1 warn-half family unified to a
6-site shape with identical field names. Sub-pattern of the
error-rail saturation but worth calling out.

### Holistic score

Using the same scheme as r9/r10 (dimension means inform but don't
determine; the overall is a reviewer judgment):

- 5 dimensions moved (4 by +1-2, 2 by +3), 6 unchanged.
- 1 r10 MAJOR closed (hardening gate landed cleanly).
- 0 of 5 r10 IMPORTANTs closed.
- 3 NEW IMPORTANTs identified, all on the same axis (concrete-PostgresBackend
  / context-as-trait-facade theme).
- 1 NEW MAJOR closed in-cycle (M-R10-1).
- 1 NEW perf fix (I35) shipped.
- Default-build warning floor: 15 (was 31 at r10).

### **Overall Score: 95/100** (+2 vs r10's 93)

The plateau call was **right in direction, wrong in slope**. r10
predicted "+1 → 94 if hardening commits"; reality landed at +2 →
95 because:
- The hardening gate was credited at r10 conditionally;
- `f6adb68b` is a genuine structural step (the 11-field privatize)
  that wasn't on r10's forecast trajectory;
- `251d53b4` is a perf-axis structural win (axis was 75 → 77).

Comparison vs r10's forecast:
- r10 forecast cap-out (no new code): 93. Reality: not the path
  taken — `f6adb68b` is new code.
- r10 forecast `BackendHandle` + `into_held` cleanup: 94. Not taken
  either — both carry.
- r10 forecast `query.rs` split: 95-96. Not taken — query.rs grew.
- r10 forecast trait fully applied: 97-98. Half-credit: privatize +
  perf fix landed instead.

The cycle did **NOT** spend on r10's forecast levers. It found
**new** structural levers (the field privatize was a deferred I16,
not on r10's radar). This is healthy — the deferred backlog is
working as a forcing function.

---

## Plateau check (r11)

The crate is **structurally complete** (still). r11 to r12 forecast:

- **R12 with `IsolateDbContext` `pub`→`pub(crate)` sweep (I-R11-1)**:
  95 → 96 (api-surface lifts to 78, boundaries lifts to 72;
  mechanical, 25 single-keyword edits).
- **R12 with `BackendHandle` alias adopted + `into_held` deleted**:
  95 → 96 (same +1 magnitude as predicted at r10; still on the
  table).
- **R12 with `Backend::open` factory + `context::set_pool` generic
  (I-R11-2)**: 95 → 97 (closes the half-application root; biggest
  single move available without query.rs split).
- **R12 with `query.rs` split**: 95 → 97-98 (the largest single
  remaining structural finding; boundaries lifts to ~82).
- **R12 with all four**: 95 → 98 (asymptote ceiling for this crate
  without a second backend landing).

The next reasonable asymptote is **96-97** with I-R11-1 + one of
{lock_guard cleanup, query.rs split}. The ceiling for an
architectural review without a second backend is **98**. r10's
"97-98 absent a second backend" forecast still holds.

### Recommendation (same as r10's recommendation 2)

r12 should ship I-R11-1 (the api-surface sweep) and one of
{`into_held` deletion, `query.rs` split}. After r12 the architecture
review should pause — the marginal point is no longer worth the
cycle. The crate has been climbing 1-2 points per cycle for 5
cycles now (89 → 91 → 92 → 92 → 93 → 95); we are within 3 points
of the architectural ceiling without a second backend.

---

## Relevant files

- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/context.rs` — `f6adb68b` privatize commit applied (lines 64-167); 16 external accessor consumers route through `crate::context::with(|c| ...)`. Strength evidence + I-R11-1.
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/context.rs:209` — `set_pool` constructs concrete `PostgresBackend`. I-R11-2.
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_bridge.rs:353-505` — `row_to_json` post-I35 + `column_to_json` 13-arm OID match. Strength + Q3 evidence.
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs:895-903` — `create_ad_hoc_backend` test-only escape hatch. I-R11-3.
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/auto_tx.rs:189,198` + `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/transaction.rs:116,146` — pair-call sequence considered in Q2. Verdict: keep.
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/backend/mod.rs:374` — dead `BackendHandle` alias. I-R10-5 carry.
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/lock_guard.rs:197` — `into_held` still dead. I-R10-4 carry.
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/query.rs` — **4315 LOC** (+40). M-R11-2.
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-architecture-review-2026-05-22-r10.md` — prior round (93, plateau forecast). Trajectory context.
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-deferred.md` — `[I16]` closed cycle 13:47; `[I35]` closed cycle 13:17.
