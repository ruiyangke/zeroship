# plugin-db: Migration / DDL Pipeline Correctness Review (R12)

**HEAD:** `89dbb6a8` · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior rounds:** r1 (72) · r2 (81) · r3 (81) · r4 (82) · r5 (83) · r6 (84) · r7 (85) · r8 (85) · r9 (85) · r10 (85) · r11 (86)

**Forcing function:** none specific to migration-pipeline this cycle —
r11's plateau-break held on observability; r12 checks whether the
cycle-13:17 three commits (one warn-shape fix, one read-path perf, one
visibility sweep) move any migration-pipeline component.

---

## 0. Pipeline delta since r11 (`71a457a1` → `89dbb6a8`)

| Commit | Module | Pipeline relevance |
|---|---|---|
| `251d53b4` | `v8_bridge.rs` | [I35] `row_to_json` O(N²) → O(N). User-table backfill batch render at `migrations.rs:432` benefits. Audit-row reads do NOT use `row_to_json` (verified — they use direct `row.get`/`row.try_get` in `audit.rs:491-496, 695-696`) so the parent's prediction about `find_latest_backfill_row` / `peek_latest_backfill_status` benefiting is **false**. The only migration-pipeline `row_to_json` call site is `migrations.rs:432` (fetchBatch user-table render). |
| `18aee490` | `migrations.rs:643-662` | F1 warn-shape drift fix. Renamed `error` → `audit_err`, `terminal` → `transition` at the 6th F1 warn site (`finalise_backfill` failure). Now all 6 sites grep on identical field NAMES (`transition=` + `audit_err=`). Value-shape differs by call site (5 sites use string literals, this one uses `?terminal` Debug enum) — acknowledged in the commit; field-name grep contract is preserved. |
| `bac64c0e` | `context.rs` | Visibility-only (`pub` → `pub(crate)` on 5 mig_lock accessors). No migration-pipeline semantic impact. |

`audit.rs` and `validate.rs` byte-identical to r11 (`git diff 71a457a1..89dbb6a8 -- crates/plugin-db/src/audit.rs crates/plugin-db/src/orchestrator/register_model/validate.rs` returns empty).

---

## 1. F1 warn-half re-audit after the 6th-site shape fix

### 1.1 Field-name grep contract across all 6 sites at HEAD

| # | File:line | transition= | audit_err= |
|---|---|---|---|
| 1 | `apply.rs:181-182` | `"Applied"` (str literal) | `%audit_err` |
| 2 | `apply.rs:212-214` | `"Failed"` (str literal) | `%audit_err` |
| 3 | `postgres.rs:499-501` | `"Failed/invalid_index"` (str literal) | `%audit_err` |
| 4 | `postgres.rs:551-553` | `"Failed/data_violation"` (str literal) | `%audit_err` |
| 5 | `postgres.rs:600-603` | `"Failed/index_build"` (str literal) | `%audit_err` |
| 6 | `migrations.rs:657-658` | `?terminal` (Debug enum) | `%audit_err` |

All six emit `transition=…` and `audit_err=…` field names. An operator
running `grep -E 'transition=.*audit_err='` over worker logs now
catches every audit-write failure in the F1 family. The 6th site's
value-shape difference (`AuditTerminal::Failed(...)` vs `"Failed/..."`)
is cosmetic — the field-name grep contract is the actual contract,
and that is now uniform.

### 1.2 Did the unification surface new state-machine drift?

No. The diff renames warn-block field names; the surrounding control
flow (the `if let Err(audit_err) = backend.finalise_backfill(...)`
block has no early-return; `release_advisory_lock` + `clear_mig_lock`
+ `return Ok(...)` still fire after the warn) is byte-identical to
r11. The state machine itself (`finalise_backfill`'s `UPDATE … WHERE
id=$1 AND status IN ('running','pending')` at `audit.rs:759`) is
unchanged.

### 1.3 Did the warn-half observability uplift surface drift now visible to operators?

I re-walked the 6 sites against the `__zeroship_migrations` status
CHECK at `audit.rs:218-220` (`'pending','running','applied',
'applied_with_dead_letter','failed','cancelled','rolled_back'`). No
warn emits a status the CHECK would reject; no transition crosses
the proposal-A3 state machine boundary. **No new drift surfaced.**

The pre-existing F2 lenient-mode anomaly (`validate.rs:66-95` writes
Pending rows then falls through to apply which skips them, leaving
Pending rows alive forever) is unchanged. F1 warn-half didn't
"surface" it because lenient-mode never *errors* the audit write —
it succeeds and just leaves rows in a steady state the operator
can't distinguish from queued work.

### 1.4 [I35] effect on migration-pipeline read paths — re-stated

The parent prompt asked whether `find_latest_backfill_row` /
`peek_latest_backfill_status` benefit from index lookup. **They do
not.** Both call sites in `audit.rs:486-489, 647-650` use
`exec.query_text(...)` then read columns via direct `row.get("id")`,
`row.try_get::<_, i64>("validate_cursor")` etc. Those go through
`compio_postgres::Row::try_get<&str>` — still the O(N) linear scan
[I35] cited. **[I35] does NOT touch the audit-row read paths.**

[I35] DOES affect one migration-pipeline read site: `migrations.rs:432`
(`rows.iter().map(crate::v8_bridge::row_to_json).collect()`), the
user-table backfill batch. For a 50-column user collection at
batch_size 1000, that's 50×1000 = 50k name lookups before → 50k
index lookups after, with the per-lookup cost dropping from O(50)
linear scan to O(1) bounds check. Backfill loops over wide tables
benefit; audit-row reads do not.

**No score component moves** on this — the backfill loop's correctness
score (82) is unchanged; perf isn't a lens this review scores.

---

## 2. F1 sweeper-half — landscape re-statement

`audit.rs:283-316` `write_audit_row` INSERT still omits
`owner_session_id` and `last_heartbeat_at` (verified — only 10 columns
populated in the INSERT, neither stamping column appears). DDL rows
remain unsweepable.

No commits in the cycle changed schema shape, heartbeat surface, or
sweeper scaffolding. The five r9-r11 carry-overs (schema add,
write-time stamping, heartbeat cadence, sweeper task, cadence/grace
policy) are all still open.

**One-liner status:** F1 sweeper-half **still open** (no change since
r9). Warn-half **closed at `fcf7ce3c` + tightened at `18aee490`**.

---

## 3. F2 status

`audit.rs:218-220` status CHECK byte-identical to r11.
`validate.rs:66-95` byte-identical to r11. The strict path still
short-circuits with a `validation_refused` envelope and leaves Pending
rows alive; lenient still falls through with destructive ops retained
in `plan.ops` (skipped by apply at `apply.rs:203,233`).

**One-liner status:** F2 **still open**, byte-identical to r10/r11.

---

## 4. Did this cycle introduce any new state-machine drift?

**No.**

- `251d53b4` (I35): pure read-path perf. Same JSON shape, same OID
  branches, same NULL-on-decode-fail semantics. No state-machine
  surface touched.
- `18aee490`: tracing-field rename only. No SQL, no transition logic,
  no control-flow change inside the warn block.
- `bac64c0e`: `pub` → `pub(crate)` on 5 accessors. No semantic change
  (callers all in-crate; access patterns identical).

The cycle is **net-positive observability** (6/6 F1 sites uniform
grep contract) with **zero new drift**.

---

## 5. Score

**86 / 100** (vs r11's **86**: **Δ 0**)

### Score components vs r11

- **Strict-deploy correctness:** 91/100 (unchanged — F2 still open)
- **Lenient/off-deploy correctness:** 75/100 (unchanged — F2 still open)
- **Audit-row state machine consistency:** 70/100 (unchanged — F1
  warn-half tightened from 5/5 to 6/6 sites with unified field names;
  but the gain is sub-point granularity. Schema asymmetry + missing
  `validation_refused` still cap the component.)
- **Backfill orchestrator:** 82/100 (unchanged — [I35] is perf, not
  correctness; backfill state-transition logic untouched)
- **CIC recovery loop:** 91/100 (unchanged)
- **Concurrent `register_model`:** 91/100 (unchanged)
- **Error-helper consistency / dedup:** 90/100 (unchanged)

No component moved. The 6th-site warn-shape fix is a sub-point
refinement of an already-credited component (audit-row state machine
consistency got +2 in r11 for the 5-site warn-half; uniformity of
the 6th site is too granular for another +1 here without
double-counting).

### Round-over-round signal

- r5 → r6: +1
- r6 → r7: +1
- r7 → r8: +0
- r8 → r9: +0
- r9 → r10: +0
- r10 → r11: +1 (warn-half forcing function fired)
- **r11 → r12: +0** (no forcing function; cycle commits orthogonal or sub-point)

The +1 r11 collected was the entire pipeline-observability uplift.
Without a structural change (F1 sweeper-half schema/stamping or F2
`validation_refused` write), this lens has no axis to budge.

### Next ceiling

- **F1 sweeper-half** (schema add `owner_session_id`+`last_heartbeat_at`
  to DDL row writes, plus heartbeat cadence + sweeper task) — would
  move audit-row state machine consistency 70 → ~80, aggregate
  86 → ~88-89.
- **F2** (write a final `validation_refused` audit row at the strict
  short-circuit; in lenient, transition skipped destructive Pending
  rows to a terminal `refused_lenient` status so they don't linger)
  — would move lenient/off correctness 75 → ~83, aggregate to ~89-90.

Both remain open. Either lands a structural +2-3.

---

## 6. Lens-specific notes

- The cycle-13:17 trio touched the migration pipeline lightly: one
  warn-shape rename (positive, sub-point), one read-path perf
  (orthogonal to this lens), one visibility sweep (orthogonal).
- The parent's prediction about [I35] helping audit-row reads is
  incorrect — `audit.rs` doesn't go through `row_to_json`. Worth
  noting for future review prompts.
- F1 warn-half is now structurally complete (6/6 sites, unified
  field names). The next correctness move on this lens has to be
  the sweeper-half or F2; observability is done.
