# plugin-db code-quality critique — round 12 (2026-05-22)

**Scope**: `crates/plugin-db/` at HEAD `6e54ebb9` (plus cycle 15:47 fixes
`d07616a2` + `02ead3f4`). Prior rounds r1–r11; r11 scored 95 ("freeze
the lens").

**Forcing function (in scope only)**:
- `14d7608f` + `6afab751` — F2 ValidationRefused state-machine extension
  (`InitialStatus::ValidationRefused`, `TerminalStatus::ValidationRefused`,
  `ALTER TABLE … DROP CONSTRAINT IF EXISTS + ADD CONSTRAINT` widening).
- `0bf71f27` — 339-LOC `test_support::CaptureLayer` infrastructure +
  11 warn-shape contract tests.
- `f6adb68b` — privatize 11 `IsolateDbContext` data fields.
- `91771aaf` — demote 27 accessors `pub(crate)`.
- `d07616a2` — validate.rs warn-shape drift fix (F1-half pattern).
- `02ead3f4` — error-message inline-alphabet on `validate_app_id`.

r11 findings NOT re-litigated.

**Verification**:
- `cargo build -p zeroship-plugin-db --lib`: builds (same warning count as r11).
- `cargo test -p zeroship-plugin-db --lib`: 363 pass.

---

## TL;DR

The four new subsystem changes land cleanly. The F2 state-machine
extension is correct — idempotent ALTER constraint, named symmetry
between Initial/Terminal, no orphan-Pending window. The capture-layer
test infrastructure is a real test-quality win (2 of 11 tests actually
drive production code; the other 9 are documented snapshots, which is
honest). The privatization sweeps lose no public surface that any
external caller still needs.

**One genuine MINOR finding**: `apply.rs:84`'s `tracing::warn!(error =
?e, "audit: failed to insert running row")` is structurally the same
class of secondary-failure as `validate.rs:102` (which `d07616a2` just
fixed) — both are `write_audit_row` failures whose audit row never
landed. `d07616a2` aligned validate.rs:102 to the F1 family
(`app_id`, `transition`, `audit_err`); `apply.rs:84` still emits
`error = ?e` with no `app_id`, no `transition`. Test-coverage r14
NEW-R14-2 caught the validate site; missed this cousin. Runbook
greps for `audit_err=` won't find this site.

**Score: 95/100 (delta vs r11: 0).** No CRITICAL, no MAJOR, one MINOR
field-shape drift carried from before the F1 unification. The new code
itself is r11-quality; the open MINOR is a pre-existing fossil the
recent fixes happened to expose.

**Did the new code merit re-firing the lens?** Yes for due-diligence
(the F2 state-machine extension is a real state-machine touch, the
339-LOC test_support is a new subsystem). The lens still floors at
95 — the changes don't move the needle either way. **r12 is a
zero-delta confirmation round.** The freeze recommendation stands.
Next code-critique fires only on a CRITICAL bug or a new subsystem
≥200 LOC (the test_support module would have qualified; nothing on
the horizon does).

---

## CRITICAL findings

None.

---

## IMPORTANT findings

None.

---

## MINOR findings

### MINOR-R12-1 — `apply.rs:84` audit-INSERT-failure warn drifts from the F1 family that just absorbed `validate.rs:102`

`crates/plugin-db/src/orchestrator/register_model/apply.rs:84`:

```rust
let audit_id = match backend
    .write_audit_row(&app_id, &AuditRow { … })
    .await
{
    Ok(id) => Some(id),
    Err(e) => {
        tracing::warn!(error = ?e, "audit: failed to insert running row");
        None
    }
};
```

Compare to the freshly-fixed `validate.rs:102` (commit `d07616a2`,
cycle 15:47):

```rust
if let Err(audit_err) = backend.write_audit_row(&ctx.app_id, &row).await {
    tracing::warn!(
        app_id = %ctx.app_id,
        collection = %op.collection,
        transition = "ValidationRefused/insert_failed",
        audit_err = %audit_err,
        "audit: failed to log destructive op",
    );
}
```

Both are `write_audit_row` secondary-failure sites where the audit row
never landed (so `audit_id` is unavailable). `validate.rs:102` was
brought into the F1 family by `d07616a2`. `apply.rs:84` is the same
shape and was NOT.

Effect:
- A runbook search for `audit_err=` finds 7 sites (6 F1 + the
  validate fix). It does NOT find this site.
- A runbook search for `app_id=…` looking for failures on a given app
  misses this site — it emits no `app_id` at all (even though
  `app_id: String` is in scope from the destructure at line 44).
- No `transition` discriminator — the 7 cousins all use one
  (`"Applied"`, `"Failed"`, `"Failed/invalid_index"`,
  `"Failed/data_violation"`, `"Failed/index_build"`,
  `?terminal`, `"ValidationRefused/insert_failed"`). This site
  needs the symmetric `"Running/insert_failed"`.

Recommended fix (one-line family alignment):
```rust
tracing::warn!(
    app_id = %app_id,
    collection = %op.collection,
    transition = "Running/insert_failed",
    audit_err = %e,
    "audit: failed to insert running row",
);
```

Severity is MINOR because this is identical drift to the
NEW-R14-2 finding test-coverage r14 raised against `validate.rs:100`
last cycle — same root cause (a pre-12:47 `error = ?e` site that the
unification sweep missed). The validate-site fix corrected one, this
one slipped through. Suggest filing as `NEW-R14-3` (or the next test-
coverage round number) so it pairs with the r14 finding.

---

### MINOR-R12-2 — `audit.rs` CHECK-widening ADD constraint is technically idempotent on fresh tables but the DROP-then-ADD rewrite produces a redundant catalog write on every cold-start

`crates/plugin-db/src/audit.rs:275-290`:

```rust
let drop_status_chk = format!(
    r#"ALTER TABLE "{app_id}"."__zeroship_migrations"
        DROP CONSTRAINT IF EXISTS __zeroship_migrations_status_chk"#
);
pool.query_text_params(&drop_status_chk, &empty).await?;
let add_status_chk = format!(
    r#"ALTER TABLE "{app_id}"."__zeroship_migrations"
        ADD CONSTRAINT __zeroship_migrations_status_chk CHECK (…)"#
);
pool.query_text_params(&add_status_chk, &empty).await?;
```

Correctness: ok. The `DROP … IF EXISTS` swallows the miss on really
old tables, and on fresh tables the constraint name matches what
`CREATE TABLE` emitted on line 239 so the drop+add is a no-op
rewrite. The constraint-list at line 240 and line 285 are identical
(double-verified the string literally).

Performance smell: every cold-start, on every per-app
`__zeroship_migrations`, runs DROP CONSTRAINT + ADD CONSTRAINT.
Postgres scans every row of the table to validate the new CHECK
(even though the new CHECK is the same as the old one). For an app
with a large audit history (cold-start orchestrator runs every
deploy), this is ms-to-seconds of redundant table scan on every cold
start, forever — not just during the migration window.

Recommended gate: `SELECT 1 FROM pg_constraint WHERE conname =
'__zeroship_migrations_status_chk' AND consrc LIKE '%validation_refused%'`
before doing the DROP+ADD. Or `pg_get_constraintdef(oid)` if `consrc`
is too pg-version-dependent. Skip the rewrite if the constraint
already names `validation_refused`.

Severity MINOR because (a) the constraint is on a low-cardinality
column with a CHECK list of 8 entries — the table scan still has to
read every row but the per-row predicate is trivial, and (b) the
audit table is in a per-app schema, so the operation is per-app, not
per-cluster. But across a fleet of 1000+ apps cold-starting at a deploy
event, this is real cost. The comment block at lines 263-274 documents
the idempotence intent but does not flag the perf cost.

---

### MINOR-R12-3 — `test_support::capture()` panics on poisoned mutex — fine for tests but the panic-on-panic shape can mask the original test failure

`crates/plugin-db/src/test_support/mod.rs:248-252`:

```rust
let events = buffer
    .lock()
    .expect("CaptureLayer buffer mutex poisoned — a record path panicked")
    .clone();
```

The docstring acknowledges this can only happen if `f` panics while
inside an event-record call. In practice that's vanishingly rare —
the on_event impl at line 154 uses `if let Ok(mut buf) = self.events.lock()`
which CANNOT poison the mutex (no panic path), so the only way to
poison is if the test closure `f` itself panics while holding the lock.
The lock is only held inside `on_event` (line 154-158), so this would
require `f` to be currently inside a `tracing::warn!` invocation that
itself records and then panics. Threading-wise this is single-threaded
under `with_default`, so the poisoning chain is:

1. test code calls `tracing::warn!(...)`.
2. capture layer's `on_event` runs, lock taken.
3. inside the visitor's `record_str` / `record_debug`, the `Debug`
   impl of one of the values panics.
4. unwind propagates out through `on_event` while holding the lock.
5. lock poisons.
6. test catches the panic (e.g. via `should_panic`).
7. `capture()`'s expect at line 250 fires a SECOND panic with a less
   informative message than the original.

Recommended: use `.unwrap_or_else(|e| e.into_inner())` instead of
`.expect(...)`. Lock poisoning is harmless here — the buffer is still
in a valid state (Vec::push is the only mutation, and a partially-
applied push would only fail by OOM). Returning the inner data
preserves the event log even in the poisoned case, so the test's own
panic message reaches the runner instead of being masked by the
mutex-poisoning expect.

Severity MINOR because the `Debug` impls of the field types
(`tracing::Span`, `String`, integers, etc.) don't panic in practice.
The fix is a 6-character `expect → unwrap_or_else` swap.

---

### MINOR-R12-4 — `test_support::CaptureLayer::on_new_span` no-op is functionally correct but the trait-bound rationale in the doc comment overstates the case

`crates/plugin-db/src/test_support/mod.rs:163-164`:

```rust
/// Spans don't carry warn-shape contract — implement a no-op so
/// the bound is satisfied without spending allocator cycles
/// during tests.
fn on_new_span(&self, _attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {}
```

`Layer<S>::on_new_span` already has a default no-op impl in
`tracing_subscriber` — the explicit no-op is redundant. The comment
"implement a no-op so the bound is satisfied" is misleading; the
bound is satisfied without this override. Removing the explicit impl
saves 2 LOC and an unnecessary `Attributes` / `Id` import (already
imported but only used here).

Severity: NIT. No effect on behavior. Just a slight over-explanation
of trait machinery in the comment. Consider removing the explicit
override + the `use tracing::span::{Attributes, Id};` import line if
nothing else uses them.

---

## NIT-level observations

- **`audit.rs:880`**: error message format string interpolates `{name}`
  via `format!` directly. Mirrors `validate_field_name` (good — `02ead3f4`
  closed the 5-cycle carry). No issue.
- **`audit.rs:208-242` `create_sql`**: still a 35-line `format!` literal.
  Could be `const &str` + `str::replace` for `{app_id}`, but the
  current shape is grep-friendly and the cost is one-time per cold
  start. Acceptable.
- **`context.rs` privatization (`f6adb68b` + `91771aaf`)**: clean. Every
  field is private, every accessor is `pub(crate)`, every cross-module
  caller routes through `context::with` / `context::with_mut`. Grep
  confirms zero external direct-field accesses from tests/ or other
  crates. The cfg-gated `mark_consumer_running` and
  `clear_consumer_registry` are correctly reached from test code via
  `replication_ops::clear_consumer_registry_for_tests` (which IS
  `pub`); the inner accessors stay `pub(crate)`. No accidental drift.
- **`audit.rs:233-240` CHECK constraint string**: matches between the
  inline `CREATE TABLE` (line 240) and the post-create `ADD
  CONSTRAINT` (line 285). Byte-for-byte identical except for whitespace,
  including the `'rolled_back'` entry that's never written by Rust
  callers (proposal-A3 reserved). Constraint-name (`__zeroship_migrations_status_chk`)
  matches across both sites. Idempotence confirmed.
- **`validate.rs:101-108`**: the `audit_err = %audit_err` shape is now
  consistent with the F1 family. Discriminator
  `"ValidationRefused/insert_failed"` is unique across the 7 sites —
  good for log-grep disambiguation. `collection = %op.collection` adds
  a per-collection dimension that the apply-side F1 sites don't carry
  (those use `audit_id` which is unavailable here). Reasonable
  asymmetry given the audit row never landed.
- **No `RefCell`-across-`await`** introduced in any of the new code.
  Every `crate::context::with_mut(|c| …)` closure body is synchronous;
  the audit ALTER TABLE statements run via `pool.query_text_params`
  outside any `with_mut` scope.
- **No new `unwrap()` on user input** in the production-code surface.
  The 3 `unwrap()` calls in test_support's `self_tests` mod are on
  controlled fixtures (deterministic event vectors).
- **`Arc<Mutex<Vec<TestEvent>>>` not held across `.await`**: confirmed.
  The lock is taken inside `on_event` (synchronous) and inside `capture`'s
  post-scope read (also synchronous after `with_default` returns).
- **Send + Sync + 'static bounds on `CaptureLayer`**: correctly satisfied
  via `Arc<Mutex<Vec<TestEvent>>>`. `TestEvent` is `Send + Sync` by
  construction (`Level`, `String`, `HashMap<String, String>` all
  implement both).
- **`FieldVisitor` covers all production field types**: `record_str`,
  `record_debug`, `record_i64`, `record_u64`, `record_bool`,
  `record_f64`, `record_error`. The production sites emit:
  - `%` sigil (str/Display) — covered by `record_str` for `&str` and
    `record_debug` for non-`str` Displayed values (tracing routes
    `Display` through `record_debug` after `format!("{}", value)`).
  - `?` sigil (Debug) — covered by `record_debug`.
  - `i64` (e.g. `audit_id`) — covered by `record_i64`.
  - `u64` (e.g. `ran_for_ms`, `backoff_ms`) — covered by `record_u64`.
  - `bool` (e.g. `transient` in postgres.rs:602) — covered by `record_bool`.
  All production emission shapes round-trip.

---

## Score

**95/100, no delta vs r11.**

Per-dimension:

| Dimension | r11 | r12 | Notes |
|---|---|---|---|
| Correctness | 96 | 96 | F2 state-machine extension correct; no new bugs. |
| Performance | 90 | 89 | DROP+ADD CONSTRAINT redundant rewrite on every cold start (MINOR-R12-2). |
| Security | 100 | 100 | `validate_app_id` allowlist still gates every format! interpolation. |
| API Design | 96 | 97 | Privatization + accessor demotion tightens the surface; `InitialStatus`/`TerminalStatus` symmetry is good API design. |
| Rust Idioms | 95 | 95 | One MINOR field-shape drift (MINOR-R12-1); two NITs (R12-3 + R12-4) on test_support. |

The +1 on API Design and −1 on Performance net to zero. r11's freeze
recommendation **stands**.

---

## Pickup hints for r13 (or skip)

r13 should fire ONLY if:
- A new subsystem ≥200 LOC lands (test_support's 339 LOC would qualify;
  nothing comparable is in flight).
- A CRITICAL bug surfaces from operations.
- The F1 family expands (e.g. write_audit_row gains a sub-failure mode
  that warrants a new transition discriminator). MINOR-R12-1's fix would
  be the natural rollup point.

If none of the above by next cycle, **stop firing code-critique**.
The plateau at 95 is real and durable.

---

## Closing

r12 is a zero-delta confirmation. The new code is r11-quality. The
one MINOR field-shape carry (apply.rs:84) is a pre-existing fossil the
F1-unification sweeps in `7c6bd2ec` and `d07616a2` happened to miss
the second time. The test_support module is a net-positive
infrastructure landing whose runtime panic surface (MINOR-R12-3) is
sub-finding-grade.

Freeze the lens.
