# plugin-db — API Surface Review (r11)

- **Date:** 2026-05-22 (cycle 13:17)
- **Target:** `crates/plugin-db/` @ `89dbb6a8`
- **Lens:** Visibility scoping, re-exports, error envelope funnel, feature
  gating, `pub(crate)` field surface on `IsolateDbContext`, op-contract
  drift from tracing-field renames and private-fn signature flips.
- **Inputs since r10 (91/100, HEAD `81226451`):**
  - `251d53b4` — `column_to_json` private signature flip (`name: &str` → `idx: usize`).
  - `18aee490` — `finalise_backfill` warn field rename (`error`→`audit_err`, `terminal`→`transition`).
  - `bac64c0e` — **5 `mig_lock` accessor demotions to `pub(crate)`** (closes r10 NEW-R10-1 / r9 NEW-R9-3, 2-cycle carry).
  - `89dbb6a8` — review reports + deferred update only.

Re-walked fresh; r10 verdicts re-verified by inspection.

---

## Dimension 1 — `bac64c0e` cleanliness (r10 NEW-R10-1 closure)

### Diff verification

`crates/plugin-db/src/context.rs` (5 single-keyword edits, lines 358/388/395/406/418):

```rust
- pub fn has_mig_lock(&self) -> bool                                  // L358
- pub fn clear_mig_lock(&mut self)                                    // L388
- pub fn take_mig_client(&mut self) -> Option<Client>                 // L395
- pub fn return_mig_client(&mut self, client: Client)                 // L406
- pub fn mig_lock_snapshot(&self) -> Option<(...)>                    // L418
+ pub(crate) fn has_mig_lock(&self) -> bool
+ pub(crate) fn clear_mig_lock(&mut self)
+ pub(crate) fn take_mig_client(&mut self) -> Option<Client>
+ pub(crate) fn return_mig_client(&mut self, client: Client)
+ pub(crate) fn mig_lock_snapshot(&self) -> Option<(...)>
```

`set_mig_lock` (L373) was already `pub(crate)` from `5d9acab8` (the
I23 shadow-replace tracing commit). All 6 mig_lock accessors now
read symmetrically.

### Caller grep — confirms zero external consumers

```bash
rg -n '\b(c|ctx)\.(has_mig_lock|set_mig_lock|clear_mig_lock|take_mig_client|return_mig_client|mig_lock_snapshot)\b' \
   crates/ --glob '!plugin-db/'                # 0 hits
rg -n '\b(has_mig_lock|set_mig_lock|clear_mig_lock|take_mig_client|return_mig_client|mig_lock_snapshot)\b' \
   sdks/ examples/ tests/                       # 0 hits
rg -n '\.(has_mig_lock|...|mig_lock_snapshot)\(' \
   crates/plugin-db/src/                        # 6 call sites, all in migrations.rs + context's own test module
```

The intra-crate call sites (`migrations.rs:191/196/200/228/337/672/781`)
were already going through the accessors, not field-direct. The
demotion is a pure visibility-keyword sweep — no callsite churn.

### Visibility-asymmetry follow-on

None. The 6 accessors now read consistently `pub(crate)`. Carries
zero residual hygiene findings on this surface.

[OK] r10 NEW-R10-1 / r9 NEW-R9-3 **CLOSED**. Net surface shrinks by
5 items at the impl-block tally; the column-0 pub-surface count
(r10's regex) is unaffected because impl methods are indented.

---

## Dimension 2 — Pub-surface tally — column-0 vs impl-method

r10's regex (`^pub `) anchored at column 0 and missed indented impl
methods. Recounting at HEAD `89dbb6a8` with both anchorings:

```bash
# Column-0 (matches `pub fn`, `pub mod`, `pub struct`, etc. at top level)
find crates/plugin-db/src -name '*.rs' -not -path '*/auth/*' |
  xargs grep -cE '^pub (async fn|fn|struct|enum|trait|const|static|type|use|mod) '
# Sum: 173                                  (unchanged from r10)

find crates/plugin-db/src -name '*.rs' -not -path '*/auth/*' |
  xargs grep -cE '^pub\(crate\) (async fn|fn|struct|enum|trait|const|static|type|use|mod) '
# Sum: 96                                   (unchanged from r10)

# Including indented impl methods (the surface bac64c0e actually moved)
find crates/plugin-db/src -name '*.rs' -not -path '*/auth/*' |
  xargs grep -cE '^\s*pub (async fn|fn|struct|enum|trait|const|static|type|use|mod) '
# Sum: 242                                  (was 247 pre-bac64c0e — 5 demoted)

find crates/plugin-db/src -name '*.rs' -not -path '*/auth/*' |
  xargs grep -cE '^\s*pub\(crate\) (async fn|fn|struct|enum|trait|const|static|type|use|mod) '
# Sum: 113                                  (was 108 pre-bac64c0e — 5 promoted into bucket)
```

Default build: **242 pub / 113 pub(crate) = 68.2% pub** (impl-aware).
Net delta from `bac64c0e`: 5 items moved from `pub` to `pub(crate)`.
The 5 are all on a `pub(crate) mod context`, so the *external* surface
of the crate boundary doesn't change (both forms collapse to `pub(crate)`
at the mod boundary) — but the hygiene noise is gone.

[OK] Surface shrank by 5 impl-block `pub fn` items. Asymmetry gone.

---

## Dimension 3 — `column_to_json` signature flip (`251d53b4`)

```rust
- fn column_to_json(row: &compio_postgres::Row, name: &str, oid: u32) -> Value
+ fn column_to_json(row: &compio_postgres::Row, idx: usize, oid: u32) -> Value
```

- Visibility: `fn` (no `pub`, no `pub(crate)`). Module-private.
- Callers: 1 in-file (`row_to_json` at `v8_bridge.rs:359`).
- Cross-crate / SDK / test reachability: 0.

```bash
rg -n 'column_to_json\b' crates/ sdks/ tests/    # 2 hits (def + 1 caller)
```

[OK] Private function. **Zero API-surface impact.** Worth noting that
the rename (`name` → `idx`) is also a parameter-name change, which
would matter if any caller used named-argument syntax — Rust doesn't
have that, so it's invisible at the call site. The 9 inner
`try_get::<_, T>(idx)` / `raw_value(idx)` sites compile because
`compio_postgres::RowIndex` is implemented for both `&str` and `usize`.

---

## Dimension 4 — `finalise_backfill` warn field rename (`18aee490`)

```rust
- error = %e,
- terminal = ?terminal,
+ transition = ?terminal,
+ audit_err = %audit_err,
```

This is a **renames-on-an-operational-contract** change. The fields
themselves are `tracing::warn!` structured fields — not part of the
crate's compile-time API, but they ARE part of the operator-facing
log-grep contract. Operators with grafana / loki queries on
`error=` or `terminal=` see the field rename as a breaking change at
the observability layer.

### Scope of the rename

Confined to ONE site (`migrations.rs:647-657`, the
`finalise_backfill` failure warn). It now aligns with the 5 sites
unified in `7c6bd2ec` (cycle 12:47). Net result: the F1 warn-half
family is **fully unified at 6 sites** under one shape:

| Field name | Old (pre-r11) | New |
|---|---|---|
| Identifier of the secondary failure | `error = %e` | `audit_err = %audit_err` |
| Discriminator (which terminal state) | `terminal = ?terminal` | `transition = ?terminal` |

The value-shape distinction r10's "two shape families" observation
called out (the 5 sites used `transition="Applied"` string-literals,
this 6th uses `transition=?AuditTerminal::Failed(...)` Debug-format)
is preserved — that's a deliberate per-call-site choice, not a
contract.

### Documented operational-contract impact

Two log-grep patterns operators may have written break across this
upgrade:

```
# Old (would have matched only the finalise_backfill warn)
loki: {app="zeroship-worker"} |= "terminal=" |= "finalise_backfill failed"

# New (matches all 6 F1 warns)
loki: {app="zeroship-worker"} |= "transition=" |= "audit_err="
```

The commit message explicitly notes "Operators grep'ing on
`transition=` would miss this one" — i.e. the rename was the FIX,
not the break. The 5 prior sites already shipped with
`transition=/audit_err=`; this commit aligns the 6th. Net direction
is **toward** operational coherence, not away.

[OBSERVATION-R11-1] Tracing-field renames are operational-contract
changes worth calling out in deploy notes / runbooks, even when the
in-crate impact is `tracing::warn!` shape only. The cycle-13:17
commit message does call this out, and the deferred backlog SUPERSEDED
[S87]+[S88] log the closure. Hygiene observation only. **-0 deduction.**

---

## Dimension 5 — `IsolateDbContext` `pub(crate)` field surface ([I16])

### Re-check: are fields still `pub(crate)` and is bypass possible?

```bash
rg -nE '^\s*pub\(crate\) (pool|db_url|registered_models|tx_conn|auto_tx_owned|tx_token|tx_token_counter|pending_emits|mig_lock|running_consumers|backend):' \
   crates/plugin-db/src/context.rs
# 11 hits — all 11 fields are pub(crate) (verified at context.rs:70/74/78/89/99/113/119/136/143/149/158).

rg -n '\bc\.(pool|db_url|registered_models|tx_conn|auto_tx_owned|tx_token|tx_token_counter|pending_emits|mig_lock|running_consumers|backend)\b' \
   crates/plugin-db/src/ --glob '!context.rs'
# 16 hits — EVERY single one calls an *accessor method*, not field-direct:
#   c.pool() / c.db_url() / c.auto_tx_owned() / c.tx_token() / c.backend()
# 0 hits of bare field access (no `c.pool` without `()`).
```

### Is [I16] now mechanical?

**Yes, mechanically.** All 16 consumer access sites already use
accessor methods. Privatising the 11 fields from `pub(crate)` to
private requires zero call-site changes outside `context.rs` itself.
The only intra-`context.rs` field-direct writes (set_pool at L201/202,
clear_pool at L209/210, etc.) live in the same module and continue to
work after privatisation.

**Where r11 lands [I16]**: this is now a **5-LOC-equivalent** sweep
(11 `pub(crate)` keyword deletions on field decls). No design needed.
The only risk is that the docstring at `context.rs:25-29` says:

> The fields stay `pub(crate)` so the lib.rs shim thread-locals can be
> removed slot-by-slot in subsequent commits without churn.

That justification has fully aged out — every consumer is on the
accessor path; the lib.rs shim removal is done. Drop the
`pub(crate)` on the fields and update the docstring.

[CARRY] [I16] **promoted to mechanical-now**. Same -0 deduction (not
a live defect, just structural debt), but the path to 96 is no longer
gated on design work. Per r10's plateau math: this is the +2 step.

---

## Dimension 6 — r8 INFO carries — actionable check

r8 listed five INFOs / structural carries that have ridden through
r9 / r10 unchanged. r11 re-evaluates each for actionability now that
the mig_lock asymmetry is closed.

| INFO | Description | Actionable now? |
|---|---|---|
| INFO-R8-1 | `config_hinted()` 0 callers | YES — 2 hint-bearing struct literals at `wal_consumer.rs:349` + `replication.rs:277` are 1-line migrations |
| INFO-R8-2 | `validation_hinted()` 0 callers | LESS-CLEAR — no production caller naturally wants a hint; could either delete or migrate auth/session.rs sites once `hardening` ships consumers |
| INFO-R8-3 | `SuppressGuard::activate` rename to `new` | YES — 1 call site (`wal_consumer.rs:464`), 2-LOC rename |
| INFO-R8-4 | `#[doc(hidden)]` on `pub(crate)` items (4 sites) | YES — 4-LOC deletions in `wal_consumer.rs` |
| INFO-R8-5 | Configuration ctor vs struct-literal mix | YES — same 5 struct-literal sites listed in r8 Dimension 2; subsumes INFO-R8-1 |

Three of five are pure mechanical edits. The remaining two
(`validation_hinted` adoption, INFO-R8-5 broader unification) need
mild design judgement on whether to migrate or delete.

**Bundle estimate**: INFO-R8-3 + INFO-R8-4 + INFO-R8-1 together is
~10 LOC across 3 files, no design tax. That's the +2 to 94 in
r10's plateau math.

---

## Dimension 7 — `#[doc(hidden)]` sweep — fresh

Same 20 hits, same files, same gating as r10. The cycle-13:17 commits
didn't touch any `#[doc(hidden)]` annotation.

[OK] No new surface holes. INFO-R8-4 carry unchanged.

---

## Dimension 8 — Cfg-fork visibility — fresh

Identical to r10. No module additions / promotions / demotions at
the `lib.rs:58-111` cfg-fork. The `bac64c0e` demotions live INSIDE
`mod context` which is already `pub(crate)` in both arms — gate
placement isn't affected.

[OK] Unchanged.

---

## Dimension 9 — `Backend` trait — fresh

Same 21 `async fn` methods. The `release_advisory_lock` r10 flip is
stable; no further signature edits this cycle.

[OK] Unchanged.

---

## Dimension 10 — Pub re-export sweep

Same as r10: 1 active re-export under default build (`PostgresBackend`
intra-crate facade), 3 cfg-gated under `hardening` (`auth/mod.rs`).

[OK] Unchanged.

---

## Findings

### [CARRY] r10 NEW-R10-1 — **CLOSED** by `bac64c0e`

5 of 6 mig_lock accessors demoted to `pub(crate)`. Symmetry restored.
+1 recovery.

### [CARRY] r8 INFO bundle — **partially actionable**

INFO-R8-1 / INFO-R8-3 / INFO-R8-4 are mechanical now (~10 LOC, 3 files).
INFO-R8-2 and INFO-R8-5 need 1 design call each. -1 aggregate, same
shape as r5-r10.

### [CARRY-MECHANICAL] [I16] `IsolateDbContext` field privatisation

All 16 consumer sites already use accessors. The 11 `pub(crate)`
field declarations can be demoted to private with zero non-context
churn. Docstring at `context.rs:25-29` also needs the "fields stay
pub(crate)" justification removed. -0 (not a defect), but this is
the +2 step to 96 in the plateau ladder.

### [OBSERVATION-R11-1] Tracing-field rename is operational-contract change

`18aee490` renamed two `tracing::warn!` fields. The crate's
compile-time API is unchanged; the operator log-grep contract DID
shift. Direction of motion is *toward* coherence (closing the
finalise_backfill drift the 5-site unification missed). Worth
calling out in release notes for ops. -0 deduction.

### [OBSERVATION-R11-2] `column_to_json` signature flip — no surface impact

`251d53b4` flipped a private fn's parameter from `&str` to `usize`.
Module-private, 1 caller. Confirmed zero external blast. -0.

---

## Plateau check — asymptote with closures applied

| Bound | What it would take | Score |
|---|---|---|
| r11 baseline (today, `bac64c0e` landed) | — | **92** |
| + INFO-R8-3 (SuppressGuard rename) + INFO-R8-4 (drop redundant doc(hidden)) + INFO-R8-1 (config_hinted migration) | r8 INFOs, mechanical | **94** |
| + privatise `IsolateDbContext` fields (11 `pub(crate)` → `pub` removed) | [I16] mechanical-now | **96** |
| + wire `auth::ensure_admin_schema` into the control plane | cross-crate I5 | **98** |

The r10 ladder (91 → 92 → 94 → 96 → 98) is now compressed by one
rung — the cycle-13:17 commit landed the +1 step that r10 predicted,
and [I16] is no longer design-gated, only mechanical.

---

## Score

**92 / 100** (**+1 vs r10's 91**)

**Improvements that drove the +1:**

- **r10 NEW-R10-1 closed via `bac64c0e`** (+1): 5 of 6 mig_lock
  accessors demoted from `pub fn` to `pub(crate) fn`; symmetry with
  the already-`pub(crate)` `set_mig_lock` restored. 2-cycle carry
  from r9 NEW-R9-3 retired. Net surface shrinks by 5 items at the
  impl-block tally (242 → was 247 pub; 113 → was 108 pub(crate)).
  Zero callsite churn — all consumers were already on the accessor
  path.

- **`251d53b4` `column_to_json` signature flip is clean** (+0): private
  fn, 1 caller, no surface impact. Worth confirming explicitly so
  it doesn't get mis-categorised as a contract change in a future
  audit. The `&str` → `usize` flip is invisible at the API boundary.

- **`18aee490` finalise_backfill warn-shape rename completes F1
  unification** (+0): tracing fields renamed `error` → `audit_err`
  and `terminal` → `transition`. NOT a compile-time API change; IS
  an operational log-grep contract change, but the direction of
  motion aligns the 6th site with the 5 already-unified sites. Net
  positive for operators despite the rename surface; called out as
  OBSERVATION-R11-1 for release-note awareness, no deduction.

**Deductions / carries preventing a steeper rise:**

- **r8 INFO bundle still open** (-1 aggregate, same as r5-r10): three
  pure-mechanical edits (SuppressGuard naming, redundant doc(hidden)
  drops, config_hinted adoption) waiting on a single touch-up PR.
  This is now the cheapest path to +2.

- **[I16] field privatisation deferred** (-0, but the +2 ceiling
  step): all 11 `pub(crate)` fields on `IsolateDbContext` are
  ready to demote to private — every consumer is already on the
  accessor path. Promoted from "needs design" to "mechanical-now"
  this cycle.

**Score sub-ranges:**

- **94+** would require: r8 INFO bundle (~10 LOC, 3 files, no design tax).
- **96** is the in-crate ceiling: privatise `IsolateDbContext` fields +
  update the L25-29 docstring.
- **98** is the asymptote: cross-crate I5 (wire
  `auth::ensure_admin_schema` into control-plane provisioning).

**Highest-leverage next move:**

```rust
// crates/plugin-db/src/context.rs:70/74/78/89/99/113/119/136/143/149/158
- pub(crate) pool: Option<Rc<Pool>>,
- pub(crate) db_url: Option<String>,
- pub(crate) registered_models: HashSet<String>,
- pub(crate) tx_conn: Option<Client>,
- pub(crate) auto_tx_owned: bool,
- pub(crate) tx_token: u64,
- pub(crate) tx_token_counter: u64,
- pub(crate) pending_emits: Option<Vec<ChangeEvent>>,
- pub(crate) mig_lock: Option<MigrationLock>,
- pub(crate) running_consumers: HashSet<String>,
- pub(crate) backend: Option<Rc<PostgresBackend>>,
+ pool: Option<Rc<Pool>>,
+ db_url: Option<String>,
+ registered_models: HashSet<String>,
+ tx_conn: Option<Client>,
+ auto_tx_owned: bool,
+ tx_token: u64,
+ tx_token_counter: u64,
+ pending_emits: Option<Vec<ChangeEvent>>,
+ mig_lock: Option<MigrationLock>,
+ running_consumers: HashSet<String>,
+ backend: Option<Rc<PostgresBackend>>,
```

Plus delete the L25-29 "fields stay `pub(crate)` so the lib.rs shim
…" justification — it's stale. Then a `cargo build -p
zeroship-plugin-db --tests --features test-helpers,hardening` to
confirm zero churn. That's the +4 jump to 96 — the in-crate ceiling.

**Sanity check on the demote's blast radius:**

```bash
rg -n '\b(c|ctx)\.(has_mig_lock|set_mig_lock|clear_mig_lock|take_mig_client|return_mig_client|mig_lock_snapshot)\b' \
   crates/ --glob '!plugin-db/'                 # 0 — no cross-crate consumer
rg -n '(has_mig_lock|set_mig_lock|clear_mig_lock|take_mig_client|return_mig_client|mig_lock_snapshot)' \
   sdks/ examples/ tests/                       # 0 — no SDK / e2e surface
```

The bac64c0e demote is fully contained inside plugin-db; no cross-crate
or SDK call sites needed updates. Clean closure.
