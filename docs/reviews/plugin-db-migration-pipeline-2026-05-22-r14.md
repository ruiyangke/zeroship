# plugin-db: Migration / DDL Pipeline Correctness Review (R14)

**HEAD:** `6e54ebb9` · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior rounds:** r1 (72) · r2 (81) · r3 (81) · r4 (82) · r5 (83) · r6 (84) · r7 (85) · r8 (85) · r9 (85) · r10 (85) · r11 (86) · r12 (86) · r13 (87)

**Forcing function:** F2 LANDED at cycle 15:17 `6afab751` — the `ValidationRefused` terminal that r13 recommended *verbatim* (Option A schema add + `InitialStatus` variant + INSERT-direct + idempotent CHECK ALTER + both strict and lenient routed through the same path). Cycle 15:47 added a warn-shape drift fix (`d07616a2`) and the audit.rs:818 error-ux alphabet fix (`02ead3f4`).

---

## 0. Pipeline delta since r13 (`0bf71f27` → `6e54ebb9`)

| Commit | Module | Pipeline relevance |
|---|---|---|
| `14d7608f` | `validate.rs` (intermediate) | F2 partial — wrote `Pending` then UPDATEd to `Failed` with `validation_refused` error-message marker. Two writes, orphan window between them on crash/UPDATE-failure. **Superseded by `6afab751` before HEAD.** Reviewed only to confirm it is no longer present on disk. |
| `6afab751` | `audit.rs` · `validate.rs` · `tests/integration.rs` | **F2 final — landed verbatim per r13 §4.2.** (a) `InitialStatus::ValidationRefused` + `TerminalStatus::ValidationRefused` enum variants symmetric for INSERT-direct AND legacy-row drive-forward. (b) `CREATE TABLE` CHECK widened to include `validation_refused`. (c) `ensure_audit_table_exists` runs an idempotent `ALTER TABLE … DROP CONSTRAINT IF EXISTS … ADD CONSTRAINT …` (named constraint, so DROP+ADD on a fresh table is a no-op rewrite; on legacy tables the DROP succeeds and the ADD widens; on really-old tables without the named constraint the IF EXISTS swallows the miss). (d) `validate.rs` writes destructive-op rows directly with `status='validation_refused'` — single round-trip, no INSERT→UPDATE orphan window. (e) `tests/integration.rs::a2_destructive_drop_column_refused_strict` updated to assert `status == "validation_refused"`. |
| `02ead3f4` | `audit.rs:818` | Error-UX only — `validate_app_id` invalid-name error now names the allowed alphabet (`ASCII alphanumeric + underscore + hyphen`). No pipeline-state surface. Closes 5-cycle error-ux carry. |
| `d07616a2` | `validate.rs:101-109` · `audit.rs:368-375` | (a) Warn-shape drift: the secondary-failure `tracing::warn!` in the F2 path was emitting `error = ?e` (single field) while the F1 warn-half family (`7c6bd2ec`) standardised on `app_id` + `transition` + `audit_err = %audit_err`. Updated to match — operators grep'ing `audit_err=` now find this site too. Discriminator: `transition = "ValidationRefused/insert_failed"`. (b) Docstring: `update_audit_status` doc-comment now enumerates all five terminal states (was: 2 of 5). |
| `44ec83db` · `6e54ebb9` | `docs/reviews/` | Cycle 15:17 + 15:47 reviewer reports. No code surface. |

`apply.rs`, `migrations.rs`, `audit.rs::write_audit_row` (INSERT column list) byte-identical to r13.

---

## 1. F2 status at HEAD: **CLOSED**

r13 recommended five things; all five landed at `6afab751`:

| r13 recommendation | Landed | Notes |
|---|---|---|
| **Option A**: separate `ValidationRefused` terminal | yes | `TerminalStatus::ValidationRefused` enum variant + `validation_refused` SQL string |
| **Schema CHECK ALTER (idempotent)** | yes | `audit.rs:263-290` — `DROP CONSTRAINT IF EXISTS … ADD CONSTRAINT … CHECK (status IN …)`. Same constraint name as `CREATE TABLE`, so idempotent on fresh + legacy + really-old tables alike |
| **InitialStatus::ValidationRefused variant** | yes | `audit.rs:131-138` — symmetric with `TerminalStatus::ValidationRefused` |
| **INSERT-direct with terminal status** | yes | `validate.rs:90` writes `status: InitialStatus::ValidationRefused` in a single INSERT; no UPDATE round-trip; no orphan window |
| **Both strict and lenient terminate** | yes | `validate.rs:67-118` — the `if !destructive.is_empty() && ctx.strictness != "off"` gate runs for both, then the `if ctx.strictness == "strict"` returns the envelope while `lenient` falls through to apply (which already skips destructive ops). The INSERT-direct write happens *before* the strict-vs-lenient branch — identical termination for both modes |

**r13's `+2-3` prediction held under the conditions r13 specified.** Landed shape matches r13 §4.2 byte-for-byte. The post-fix audit-table state is what r13 projected: `SELECT * FROM __zeroship_migrations WHERE status='pending' AND change_class='destructive'` returns the steady-state empty queue; refused rows surface via `WHERE status='validation_refused'` without parsing `error`.

---

## 2. F1 sweeper-half: **STILL OPEN — no movement**

`audit.rs::write_audit_row` INSERT byte-identical to r13 (10 columns: `collection, phase, change_class, change_kind, details, ddl_sql, status, deploy_id, applied_by_kind, schema_version`). `owner_session_id` and `last_heartbeat_at` still not stamped at row insert. `migrations.rs` byte-identical to r13. No sweeper task, no heartbeat cadence, no write-time stamping.

Schema columns are still on the table since the cold-start DDL (`audit.rs:229-230`). Five r9-r13 carry-overs all open:
1. Write-time stamping in `write_audit_row` INSERT
2. Heartbeat cadence (periodic UPDATE of `last_heartbeat_at`)
3. Sweeper task definition
4. Cadence/grace policy
5. Background scheduler integration

**One-liner status:** F1 sweeper-half **still open** (no change since r9). Warn-half remains pinned by snapshot test from r12.

---

## 3. Audit-row state machine — re-walked with `ValidationRefused` as terminal

The proposal A3 state machine, post-F2:

```
                  ┌─────────────────────────────────────────┐
                  │ INSERT phase='ddl' or 'backfill'         │
                  │ status ∈ {pending, running,              │
                  │           validation_refused}            │
                  └────────────┬──────────────┬──────────────┘
                               │              │
              (pending|running)│              │(validation_refused — INSERT-direct terminal)
                               ▼              ▼
              ┌───────────────────────┐    ┌───────────────────────────────┐
              │ pending|running       │    │ validation_refused (TERMINAL) │
              │ (writable via         │    └───────────────────────────────┘
              │  update_audit_status) │
              └────────┬──────────────┘
                       │
   ┌───────────────────┼───────────────────────────────────────┐
   │                   │                                       │
   ▼                   ▼                                       ▼
applied         applied_with_dead_letter            failed / cancelled
(TERMINAL)         (TERMINAL)                          (TERMINAL)
```

**Verified properties:**

- **`update_audit_status` cannot regress a terminal row.** `audit.rs:394` WHERE clause is `status IN ('running','pending')` — `validation_refused` rows are not eligible UPDATE targets, so a stale caller cannot overwrite a refused-terminal row with `applied`/`failed`/etc. ✓
- **`validate.rs` INSERT skips Pending entirely for destructive ops.** Path is INSERT-direct with `status='validation_refused'` (single round-trip). No window for crash/UPDATE-failure to leak. ✓
- **State machine documentation matches implementation.** Docstring at `audit.rs:368-375` now enumerates all five terminals: `applied | applied_with_dead_letter | failed | cancelled | validation_refused`. The "INSERT-direct, normally not transitioned" caveat is documented inline. Comment block at `audit.rs:418-433` (backfill state diagram) does NOT need a `validation_refused` arm because that's a DDL-phase terminal, never a backfill-phase one. ✓
- **`InitialStatus::ValidationRefused` symmetry.** The variant exists for INSERT but `write_audit_row` is the only writer that consumes it. Legacy rows in `pending` from before `6afab751` are addressed by the symmetric `TerminalStatus::ValidationRefused` arm (callers that already have a Pending row can still drive it forward, though no in-tree caller does — the symmetry is forward-compat scaffolding). ✓
- **`is_done` semantics** (`audit.rs:559-562`) — backfill helper does NOT list `validation_refused` in its terminal set; correctly so, that status is unreachable for `phase='backfill'` rows. ✓

**Minor observation (not new in r14):** `CHECK` constraint lists `rolled_back` but no `TerminalStatus::` variant for it. Pre-existing legacy slot, not r14-relevant.

**State-machine consistency verdict:** the documentation and the implementation are aligned post-F2. The orphan-Pending invariant is closed for the destructive-validate path. The remaining inconsistency in the state machine is F1's "rows that COULD be orphan-Pending mid-backfill if the worker crashes between INSERT and heartbeat" — that's F1's territory, untouched this cycle.

---

## 4. Advisory-lock coordination

Byte-identical to r13. `migrations.rs::exec_begin` gate on `has_mig_lock` unchanged. The `set_mig_lock` shadow-replace ERROR (pinned by r13's contract test 2.2) is unchanged. The `release_advisory_lock` warn at the cancelled-refusal + backfill-finalise sites is unchanged. No new advisory-lock surface.

---

## 5. Backfill orchestrator

Byte-identical to r13. F1 sweeper-half remains the only outstanding gap on this component.

---

## 6. Strictness-mode coherence

Re-walked all three modes with the F2 fix landed:

| Mode | Destructive op behaviour | Audit row state | Apply behaviour |
|---|---|---|---|
| `strict` (default) | INSERT-direct `validation_refused` → return envelope (short-circuit) | Terminal at INSERT | Never reached |
| `lenient` | INSERT-direct `validation_refused` → fall through to apply | Terminal at INSERT | Apply loop skips destructive ops via the `if op.class == Destructive { continue; }` gate (single source of truth) |
| `off` | No INSERT (gated by `ctx.strictness != "off"` at `validate.rs:67`) → fall through to apply | No audit row written | Apply loop skips destructive ops (same gate) |

**Coherence verified.** Strict and lenient both terminate; the only behavioural difference is whether the SDK envelope is returned to the caller. `off` writes no row, preserving the test/CI no-op contract per proposal A2 line 122. ✓

---

## 7. Lenient strictness integration test — GAP-3 STILL OPEN (8 cycles)

r14-test-coverage flagged it at cycle 15:17 and r13 §4.2 endorsed it. **Status at HEAD:**

`grep -n 'lenient' crates/plugin-db/tests/integration.rs` returns no hits. `grep -n 'strictness.*lenient' crates/plugin-db/tests/integration.rs` returns no hits. The lenient path's "INSERT-direct terminal + apply-skips" behaviour is **untested at integration level**.

The strict path got its test updated at `6afab751` (`a2_destructive_drop_column_refused_strict` now asserts `status == "validation_refused"`). The lenient path got no equivalent. The 8-cycle gap is real and remains the largest single unaddressed test-coverage hole on the F2 surface.

**What a lenient integration test would assert:**
1. v1 deploy creates `posts(name, legacy_score)` with `strictness=lenient` (or default lenient via `_meta`)
2. v2 deploy without `legacy_score` returns `Ok(_)` (no envelope)
3. `__zeroship_migrations` has one `drop_column` row with `status='validation_refused'`
4. The `legacy_score` column still exists (lenient skips the DDL, doesn't apply it)

The current `a2_strictness_off_skips_validation_refused` test (lines 1158-1197) covers the `off` mode — superficially similar but writes **no** audit row, so it cannot exercise the F2 fix for lenient.

**Status:** GAP-3 (lenient integration test) **still open — 8 cycles**.

---

## 8. CHECK ALTER upgrade-path test — NEW-R14-1 STATUS

r14-mandate flagged: "is a focused test being landed (in flight)? If not, note it."

`grep -n 'status_chk\|drop_constraint' crates/plugin-db/tests/integration.rs` returns no hits matching the F2 CHECK upgrade. The idempotent `DROP CONSTRAINT IF EXISTS … ADD CONSTRAINT …` path at `audit.rs:263-290` has **no focused test** verifying that a legacy table (created before `6afab751`) gets its CHECK widened correctly on the next `ensure_audit_table_exists` call.

The existing strict test exercises the path only on a freshly created schema (`DROP SCHEMA IF EXISTS … CASCADE` then create) — that hits the fresh-table branch where DROP+ADD is a no-op rewrite. The legacy-table branch (DROP finds the old constraint, ADD installs the new one) and the really-old-table branch (DROP IF EXISTS swallows the miss, ADD still installs) are **unexercised**.

**What a focused test would do:**
1. Manually create a `__zeroship_migrations` table with the **old** CHECK (no `validation_refused`)
2. Call `ensure_audit_table_exists` for that app
3. Assert that an INSERT with `status='validation_refused'` succeeds (would have failed on the old CHECK)
4. As a control, also test the really-old-table case where the constraint has no name

**Status:** NEW-R14-1 (CHECK ALTER upgrade-path focused test) **not landed**. The path is reasonably defended by the idempotent DROP+ADD shape (low risk of post-upgrade failure), but it's an untested upgrade-path branch. **Acceptable risk** for now given the simplicity of the DDL and the fact that PostgreSQL's `DROP CONSTRAINT IF EXISTS` behaviour is well-defined, but worth recording as an open coverage gap.

---

## 9. Score

**89 / 100** (vs r13's **87**: **Δ +2**)

### r13's prediction holds

r12's projection: "F2 resolution worth +2-3, aggregate to ~88-90." r13's projection: "F2 lands per design → +2-3, aggregate to ~88-90." Landed shape matches the design verbatim. **Aggregate moved 87 → 89 (Δ +2).** Inside r12+r13's predicted band of ~88-90.

The +2 (not +3) reflects:
- Lenient integration test still open at HEAD (GAP-3, 8-cycle carry) — would have been worth +1 if landed alongside F2.
- CHECK ALTER upgrade-path focused test not landed (NEW-R14-1) — minor, acceptable risk.

### Score components vs r13

- **Strict-deploy correctness:** 94/100 (**+3** — `91 → 94`. INSERT-direct terminal closes the orphan-Pending window for the destructive-validate path; strict-path integration test updated to pin the new terminal at the source.)
- **Lenient/off-deploy correctness:** 82/100 (**+7** — `75 → 82`. Lenient now terminates identically to strict at the row level. Off unchanged. **The component is held below ~88 by the missing lenient integration test** — the code change is +10 worth of correctness, the test gap recovers ~3.)
- **Audit-row state machine consistency:** 75/100 (**+5** — `70 → 75`. The destructive-validate arm of the state machine now has a documented terminal that matches its implementation. F1 sweeper-half still capping the component at 75; closing F1 would move this to ~85.)
- **Backfill orchestrator:** 82/100 (unchanged — F1 sweeper-half still open).
- **CIC recovery loop:** 91/100 (unchanged).
- **Concurrent `register_model`:** 91/100 (unchanged).
- **Error-helper consistency / dedup:** 94/100 (**+2** — `92 → 94`. The warn-shape drift fix at `d07616a2` brought the F2 audit-failure warn into the F1 warn-half family. Operators grep'ing `audit_err=` now find six F1 sites + the F2 site uniformly. Drift detection on the destructive-invariant ERROR remains pinned from r13.)
- **DDL upgrade-path / schema migrations:** 78/100 (new component this round — the idempotent CHECK ALTER is the first real "schema migrates itself across a deployed cluster" surface; it's well-shaped but untested at the legacy-table branch. NEW-R14-1 caps it below 85.)

The +2 aggregate comes from lenient correctness (+7 weighted partially) + strict correctness (+3 weighted partially) + state-machine (+5 weighted partially) + error-helper (+2 weighted) − the new DDL-upgrade component pulling the average toward its 78 score (mild dampening).

### Round-over-round signal

- r5 → r6: +1
- r6 → r7: +1
- r7 → r8: +0
- r8 → r9: +0
- r9 → r10: +0
- r10 → r11: +1 (F1 warn-half landed)
- r11 → r12: +0
- r12 → r13: +1 (proactive drift detection on destructive-invariant; F2 in flight)
- **r13 → r14: +2** (F2 landed per design; warn-shape drift cleanup)

The +2 step is the largest since r10→r11 and the largest substantive correctness step since F1 warn-half. The migration-pipeline component now has only F1 sweeper-half as a major outstanding correctness gap; the other open items (lenient integration test, CHECK ALTER focused test) are test-coverage carries.

### Next ceiling

- **F1 sweeper-half (5-step):** write-time stamping (+1) + heartbeat cadence (+1) + sweeper task (+1) + cadence/grace policy (+0.5) + scheduler integration (+0.5) → projected **+3-4**. Lifts state-machine + backfill components.
- **Lenient integration test (GAP-3):** projected **+1** on lenient/off correctness.
- **CHECK ALTER focused test (NEW-R14-1):** projected **+0.5-1** on DDL-upgrade-path component.

Aggregate ceiling at HEAD: ~93-94 with F1 closure + both test gaps filled. F1 remains the dominant remaining lift.

---

## 10. Lens-specific notes

- The `6afab751` commit message accurately characterises the supersession of `14d7608f` ("Failed+marker pattern"). The intermediate commit's design carried an orphan window between INSERT and UPDATE that the final design eliminates by collapsing to INSERT-direct. The commit history preserves the design evolution honestly; reviewers can trace why the simpler Failed+marker shape was rejected.
- The idempotent CHECK ALTER at `audit.rs:263-290` is the **first real "platform-managed schema migration"** in plugin-db's own table. The shape (named constraint + DROP IF EXISTS + ADD) is the textbook safe idiom — but the lack of a focused test for the legacy-table branch is a real coverage gap. Acceptable risk given the simplicity, but worth a follow-up.
- The warn-shape drift fix at `d07616a2` is the **second** F1-family drift caught in three cycles (first was `18aee490` in r12). Pattern: when new emission sites land, the F1 family's shape isn't yet a structural pin. r13's snapshot-test-pin caught this site at review time, not before. Worth considering a `macro_rules!` or a thin helper that emits the canonical field set, so future emission sites can't drift by accident. Not blocking; minor structural debt.
- The `02ead3f4` alphabet-naming fix on `validate_app_id` is error-UX hygiene — uplifts the error message to match the `validate_field_name` pattern from `403b3891` and `replication.rs:91-97`. No pipeline-state surface.
- r13's prediction held byte-for-byte: F2 landed in Option A shape with both strict + lenient terminating + INSERT-direct + InitialStatus variant + idempotent CHECK ALTER. The forcing function discipline of writing predictions in r13 §4.2 is paying off — r14's job here is verification, not redesign.
