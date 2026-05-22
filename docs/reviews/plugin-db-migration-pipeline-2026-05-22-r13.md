# plugin-db: Migration / DDL Pipeline Correctness Review (R13)

**HEAD:** `0bf71f27` · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior rounds:** r1 (72) · r2 (81) · r3 (81) · r4 (82) · r5 (83) · r6 (84) · r7 (85) · r8 (85) · r9 (85) · r10 (85) · r11 (86) · r12 (86)

**Forcing function:** cycle 14:47+15:17 added a tracing capture layer
(`test_support::capture`) plus three contract tests pinning the
[I23] mig_lock shadow-replace ERROR, the [I6] release_advisory_lock
WARN, and the destructive-invariant ERROR. Plus a `pub(crate)`
demotion sweep on six mig_lock accessors. Migration-pipeline state
machine code is byte-identical to r12.

---

## 0. Pipeline delta since r12 (`89dbb6a8` → `0bf71f27`)

| Commit | Module | Pipeline relevance |
|---|---|---|
| `f6adb68b` | `context.rs` | I16 — privatise IsolateDbContext fields (`pub(crate)` → private). No pipeline behaviour change; the new accessor methods carry the same signatures the field reads had. The doc-comment uplift on the struct (`/// All fields are private...`) is a contract anchor for future reviewers. |
| `bf75e866` | `compio-postgres` | `test-utils` feature exposes `Row`/`Statement`/`Column` builders. Not used by migration-pipeline code yet — the r13 capture tests rely only on the tracing harness, not the postgres builders. |
| `75d9ae5c` · `4e9dbafb` | `benches/` | Two new benches for [I35] (`bench_row_to_json`) and C3 (`bench_first_row_or_null`). Measurement-only. Zero pipeline-state surface. |
| `91771aaf` | `context.rs` | Accessor demotion sweep — six mig_lock methods (`set_mig_lock`, `return_mig_client`, `take_mig_client`, `clear_mig_lock`, `has_mig_lock`, `mig_lock_snapshot`) demoted from `pub` to `pub(crate)`. **Audited callers** (grep `Mig|mig_lock`): all in-crate (`migrations.rs` + `lib.rs` + `context.rs` itself). No worker/runtime crate touches them. Production behaviour byte-identical. |
| `0bf71f27` | `context.rs` · `migrations.rs` · `apply.rs` · `test_support/mod.rs` · `Cargo.toml` · `lib.rs` | Tracing capture layer + three contract tests. Pipeline production code unchanged. |

`audit.rs` and `validate.rs` byte-identical to r12
(`git diff 89dbb6a8..0bf71f27 -- crates/plugin-db/src/audit.rs
crates/plugin-db/src/orchestrator/register_model/validate.rs`
returns empty).

---

## 1. Accessor-demotion audit (`91771aaf`)

The brief asked to verify migration code still compiles + behaves
identically. Reviewed:

- **Callers of `set_mig_lock`**: only `migrations.rs::exec_begin` (one
  call site at `migrations.rs:~330` — confirmed via grep). In-crate.
- **Callers of `return_mig_client`**: `migrations.rs` only.
  `return_lock_client(client)` helper is the only entry.
- **Callers of `take_mig_client`**: `migrations.rs::take_lock_client`
  helper only.
- **Callers of `clear_mig_lock`**: `migrations.rs` (4 sites:
  `exec_cancel`, `exec_commit_batch` is-done branch,
  `finalise_backfill`, `exec_reset`) and the `tests/integration.rs`
  test scaffolding.
- **Callers of `has_mig_lock`**: `migrations.rs::exec_begin` gate.
- **Callers of `mig_lock_snapshot`**: `migrations.rs` (multiple
  read-only inspections).

Worker, runtime, gateway, control: **zero** uses of any of these. No
production accessor leaked through. The `pub(crate)` demotion is
sound — the demotion does not break a tx_token-style invariant
(those accessors are also `pub(crate)` since r12's `bac64c0e`).

**One-liner verdict:** demotion clean; migration-pipeline behaviour
byte-identical.

---

## 2. Tracing capture-layer tests audit (`0bf71f27`)

The brief asked: "**VERIFY** these tests document the actual
production contract correctly." Walked each test against the
production site.

### 2.1 `destructive_invariant_error_emits_named_fields_at_error_level`

**Test type:** end-to-end. Calls `destructive_invariant_error(&op)`
directly and asserts on captured events.

**Production site:** `apply.rs:309-325`. Emits
`tracing::error!(change_kind, class, collection, "apply:
destructive-invariant violation")`.

**Verified:**
- Field NAMES: `change_kind`, `class`, `collection` — match.
- Level: ERROR — match.
- `change_kind` value rendering: `op.change_kind.as_sql()` — Display
  for `change_kind` (no `?` or `%` prefix on the production line
  319: `change_kind = op.change_kind.as_sql()`). The capture layer's
  `record_str` path renders this as `"drop_column"`. Test asserts
  `Some("drop_column")` — match.
- `class` value rendering: `?op.class` (line 320) — Debug. Capture
  renders `"Additive"`. Test asserts `contains("Additive")` — match.
- `collection` value rendering: `%op.collection` (line 321) —
  Display. Test asserts `Some("posts")` — match.
- Message: `"apply: destructive-invariant violation"`. Test asserts
  `contains("destructive-invariant violation")` — match.

**Verdict:** test exercises the live function. Drift in field names,
level, or message body in the production source IS caught at
unit-test time. Contract pin is genuine.

### 2.2 `set_mig_lock_shadow_replace_emits_error_with_prev_and_new`

**Test type:** end-to-end. Calls `ctx.set_mig_lock(...)` twice; the
second call hits the shadow-replace branch.

**Production site:** `context.rs:381-392`. Emits
`tracing::error!(prev_name, prev_audit_id, new_name, new_audit_id,
"set_mig_lock called while another lock is active — begin path
should gate on has_mig_lock")`.

**Verified:**
- Field NAMES: all four match.
- `prev_name` / `new_name`: `%` Display. Test asserts `"first"` /
  `"second"` — match (the `mig_lock("first", "users", 11, false)`
  helper sets `name = "first"`).
- `prev_audit_id` / `new_audit_id`: Display (no prefix on production
  lines 385 + 387). The helper sets `audit_id = 11 / 22`. Test
  asserts `"11" / "22"` — match.
- Level: ERROR — match.
- Message: production says `"set_mig_lock called while another lock
  is active..."`. Test asserts `ev.message.contains("set_mig_lock")`
  — match (and the contains-check is intentionally narrow, so a
  future refactor that keeps "set_mig_lock" but rewords the rest
  still passes — this is a reasonable contract).

**Verdict:** test exercises the live accessor. Drift IS caught.

### 2.3 `set_mig_lock_first_install_emits_no_event` (negative)

**Test type:** end-to-end. Calls `ctx.set_mig_lock(...)` once on a
fresh context.

**Production site:** same as 2.2 but the `if let Some(prev)` gate
skips the warn on first install.

**Verified:** test pins the gate. A future refactor that removes the
`if let Some(prev)` guard and unconditionally emits would fail this
test. Genuine pin.

### 2.4 `i6_release_advisory_lock_warn_shape_documentation_snapshot`

**Test type:** snapshot / documentation. Re-emits the same
`tracing::warn!` syntax inline; does NOT call the production sites
in `migrations.rs:291` or `migrations.rs:670`.

**Production sites verified:**
- `migrations.rs:291-296`: cancelled-refusal path. Fields:
  `app_id, name, error = %e`. Message:
  `"release_advisory_lock failed on cancelled-refusal path (lock
  auto-releases on session end)"`.
- `migrations.rs:670-675`: backfill-finalise path. Fields:
  `app_id, name, error = %e`. Message:
  `"release_advisory_lock failed on backfill-finalise path (lock
  auto-releases on session end)"`.

**Test inline emission (line ~330 of migrations.rs test block):**
- Fields: `app_id, name, error = %e` — match.
- Message: `"release_advisory_lock failed on cancelled-refusal path
  (lock auto-releases on session end)"` — match cancelled-refusal
  site only.

**Caveat (test does not catch what one might assume):** because the
test re-emits the literal `tracing::warn!` syntax in the test body,
a contributor who renames `error` → `lock_err` in BOTH the
production sites AND the test in one commit will see the test pass.
The test catches: (a) someone who edits only one production site
without updating the snapshot, OR (b) someone who edits only the
snapshot. It does NOT catch a coordinated rename across all three.

The test's own doc-comment explicitly acknowledges this:
> What this test does NOT catch: a contributor who renames
> `audit_err` → `error` in BOTH the production sites and this test
> in one commit.

So the **contract is honestly documented**. The pin is weaker than
2.1-2.3 but documented as such. **Acceptable** for the migration-
pipeline lens, given the alternative (mocking `Backend` to drive
`release_advisory_lock` failure) is non-trivial.

### 2.5 `f1_warn_shape_documentation_snapshot` (in apply.rs tests)

**Test type:** snapshot. Re-emits the F1 warn syntax inline; does
NOT call any production path.

Same caveat as 2.4: pins field NAMES across sites via the test
acting as a fifth coordination point. Coordinated rename across the
6 production sites + this test in one commit slips through.

**Production sites (6 total, per r12 table):** all carry
`app_id, audit_id, transition, audit_err` field names. Test pins
those. Match.

### 2.6 `return_mig_client_message` — acknowledged hole

The capture-layer module's doc-comment + the comment block at
`context.rs:1055-1069` explicitly call out: the `return_mig_client`
empty-slot WARN cannot be unit-tested because
`return_mig_client(client: Client)` requires a real
`compio_postgres::Client`, and `test-utils` exposes only
`Row`/`Statement`/`Column` builders. End-to-end coverage punted to
`tests/integration.rs`. **Honestly documented gap**, not a missed
contract.

---

## 3. F1 sweeper-half — still open

`audit.rs:283-316` `write_audit_row` INSERT byte-identical to r12 —
INSERT lists 10 columns (`collection, phase, change_class,
change_kind, details, ddl_sql, status, deploy_id, applied_by_kind,
schema_version`); `owner_session_id` and `last_heartbeat_at` are
**not** stamped at row insert. Schema CHECK at lines 208-209 still
lists them as nullable columns (added at cold-start DDL by Gap X
style ADD COLUMN IF NOT EXISTS — verified, no schema patch).

No commits in cycle 14:47+15:17 touched the DDL writer, heartbeat
surface, or sweeper scaffolding. The five r9-r12 carry-overs (schema
add, write-time stamping, heartbeat cadence, sweeper task,
cadence/grace policy) all still open.

**One-liner status:** F1 sweeper-half **still open** (no change
since r9). Warn-half **closed at r11/r12 and now pinned by a
documentation snapshot** in 2.5 above.

---

## 4. F2 status + in-flight fix alignment

### 4.1 F2 at HEAD `0bf71f27`

`audit.rs:218-220` status CHECK byte-identical to r12
(`'pending','running','applied','applied_with_dead_letter',
'failed','cancelled','rolled_back'`). `validate.rs:66-95`
byte-identical to r12 — strict path returns the
`validation_refused` envelope after writing Pending audit rows
(lines 69-88), leaving Pending rows alive. Lenient falls through
with destructive ops in `plan.ops` (skipped by apply at
`apply.rs:235-236`).

`grep -r 'write_pending_validation_refused\|ValidationRefused'`
returns no `validate.rs` hit — the in-flight F2 fix has NOT yet
landed on disk. Working tree clean.

### 4.2 Alignment review for the in-flight F2 fix

The brief says the fix will write+terminate to `ValidationRefused`
via `validate.rs::write_pending_validation_refused`. Reviewing the
design against the current pipeline:

**Constraint 1 — status CHECK.** The current CHECK at
`audit.rs:218-220` does NOT include `validation_refused` as a valid
status. Three options:

- **Option A (best):** add a new terminal `validation_refused` to
  the CHECK constraint. Requires a Gap-X-style
  `ALTER TABLE … DROP CONSTRAINT … ADD CONSTRAINT …` patch in
  `ensure_audit_table` so existing tables pick it up at cold start.
  Aligns with the proposal-A3 state machine — the row is in fact in
  a refused-terminal state, not a generic `failed` state.
- **Option B:** reuse `cancelled`. Semantically wrong — operator
  didn't cancel; validate refused. Loses the discriminator at the
  audit-table level.
- **Option C:** reuse `failed`. Conflates with apply-time DDL
  failure. Loses observability.

**The brief recommends Option A** (separate `ValidationRefused`
terminal). r13 endorses this — the schema discriminator is the
whole point of a per-state terminal.

**Constraint 2 — strict vs lenient symmetry.** The brief mentions
"write+terminate to `ValidationRefused`" without saying which
strictness modes apply. Both should terminate:

- **strict:** insert (status=`pending`) → update (status=
  `validation_refused`) → return envelope. End state: terminal.
- **lenient:** insert (status=`pending`) → update (status=
  `validation_refused`) → fall through to apply (which already
  skips destructive ops at the loop gate). End state: terminal.
- **off:** no Pending row written today (line 66 gate
  `ctx.strictness != "off"`). Stay that way — `off` mode is the
  test/CI no-op path.

If the fix terminates **only** strict and leaves lenient writing
Pending rows that never resolve, that's an F2-residual. The fix
must hit both strict and lenient.

**Constraint 3 — write+terminate atomicity.** Two writes (INSERT
then UPDATE) vs one (INSERT with status=`validation_refused`
directly). The latter is one round-trip. **Recommend INSERT-direct
with terminal status** — no intermediate `pending` row, no risk of
crash between INSERT and UPDATE leaving an orphan. This requires
`InitialStatus::ValidationRefused` (or equivalent) in
`audit::InitialStatus` — currently the type lists only `Pending`
and `Running` (verified via `grep -r 'enum InitialStatus' crates/plugin-db/`).
Adding `ValidationRefused` as an `InitialStatus` variant is the
cleanest path.

**Constraint 4 — terminal-state grep contract.** The destructive
rows are the things operators search the audit log for. Today
`SELECT * FROM __zeroship_migrations WHERE status='pending' AND
change_class='destructive'` returns an unbounded queue of refused
rows (the F2 leak). Post-fix, the same query returns the queue of
*currently-unresolved* refusals (i.e. empty in the steady state).
Operators get a new query: `... WHERE status='validation_refused'`
to find refused rows in a deploy. **Document this in the migration
runbook** alongside the fix.

### 4.3 Pred-check: r12 said "F2 resolution worth +2-3"

r12's projection: "lenient/off correctness 75 → ~83, aggregate to
~89-90". That projection assumed F2 lands as designed. At HEAD
`0bf71f27` the fix has NOT landed (working tree clean). r13's score
therefore does NOT include the projected +2-3 yet.

If the in-flight fix lands per the design above (Option A schema
add + InitialStatus variant + both strict+lenient terminating), r14
should observe lenient/off correctness moving 75 → ~83 and the
aggregate moving to ~88-90. r12's prediction holds **conditional
on the fix landing as designed**. The brief is internally consistent.

If the fix lands in a weaker form (e.g. strict-only, or reusing
`cancelled`, or two-write non-atomic), r14 should credit less.

**One-liner status:** F2 **still open** at HEAD; in-flight fix
design is **aligned with r12's projection** under Option A; r14
will validate against actual landed code.

---

## 5. Did this cycle introduce any new state-machine drift?

**No.**

- `f6adb68b` (privatise fields): visibility-only, no semantic
  change. Doc-comment uplift on the struct is a positive
  contract anchor.
- `bf75e866` (test-utils feature on compio-postgres): scaffolding,
  no pipeline use yet.
- `75d9ae5c` · `4e9dbafb` (benches): measurement-only.
- `91771aaf` (accessor demotion): visibility-only, no semantic
  change.
- `0bf71f27` (capture layer + 3 contract tests): test-only — adds
  proactive drift detection. **Net positive observability.**

The cycle is **net-positive proactive drift detection** (3 new
contract tests; one true end-to-end, two snapshots with honestly
documented limits) with **zero new state-machine drift**.

---

## 6. Score

**87 / 100** (vs r12's **86**: **Δ +1**)

### Score components vs r12

- **Strict-deploy correctness:** 91/100 (unchanged — F2 still open
  at HEAD)
- **Lenient/off-deploy correctness:** 75/100 (unchanged — F2 still
  open at HEAD; the in-flight fix is design-aligned but not landed)
- **Audit-row state machine consistency:** 70/100 (unchanged — F1
  sweeper-half still open; warn-half now has executable snapshot
  pins but they don't move the state-machine score)
- **Backfill orchestrator:** 82/100 (unchanged)
- **CIC recovery loop:** 91/100 (unchanged)
- **Concurrent `register_model`:** 91/100 (unchanged)
- **Error-helper consistency / dedup:** 92/100 (**+2** — the
  destructive-invariant ERROR contract test in 2.1 is genuine
  end-to-end and pins the operator-grep contract at the source.
  This is the first proactive drift-detection mechanism on the
  error-helper layer; previously the contract lived only in code
  comments. Net +2 lifts this component without double-counting.)

The +1 aggregate comes from the error-helper component lift (+2
weighted) tempered by the snapshot-only nature of 2.4/2.5 (no lift
on F1/F2 since the production path is unchanged).

### Round-over-round signal

- r5 → r6: +1
- r6 → r7: +1
- r7 → r8: +0
- r8 → r9: +0
- r9 → r10: +0
- r10 → r11: +1 (F1 warn-half landed)
- r11 → r12: +0
- **r12 → r13: +1** (proactive drift detection on destructive-
  invariant; F2 fix in flight but not yet landed)

### Next ceiling

- **F2 (lands this cycle):** Option-A schema add + InitialStatus
  variant + both strict+lenient terminating. **Projected +2-3**
  per r12's prediction. r13 endorses the projection conditional
  on the design specified in §4.2.
- **F1 sweeper-half:** schema columns are already on the table
  (`owner_session_id` + `last_heartbeat_at` nullable since the
  initial DDL — verified at `audit.rs:208-209`). Missing: write-
  time stamping in `write_audit_row` INSERT, heartbeat cadence,
  sweeper task. Lands +2-3 on audit-row state machine consistency.

Both remain open at HEAD; F2 is in flight. r14's forcing function
is likely the F2 landing.

---

## 7. Lens-specific notes

- The destructive-invariant ERROR test (2.1) is the **first genuine
  end-to-end contract test** on the migration-pipeline observability
  surface — earlier rounds had only code-comment pins. This is a
  structural step forward on observability hygiene; future
  contributors who rename `change_kind` → `kind` get a failing
  test, not a quiet log drift.
- The two snapshot tests (2.4 + 2.5) are **honestly documented as
  weaker** by the test authors themselves. They catch single-site
  drift; they do not catch coordinated multi-site renames. r13
  rates this as acceptable engineering given the alternative
  (mocking `Backend` to drive `release_advisory_lock` failure)
  costs significantly more than the contract value protects.
- The brief's prediction "r12 said F2 resolution worth +2-3" is
  internally consistent with the in-flight fix design specified in
  §4.2. r14 will validate against the landed code.
- The `f6adb68b` doc-comment uplift on `IsolateDbContext`
  (`"All fields are private. Every consumer goes through an
  accessor method..."`) is a small but real contract anchor for
  future reviewers — prevents the "let me just add a `pub` field"
  drift class.
