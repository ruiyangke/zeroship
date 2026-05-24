# Sandbox snapshot-restore code-quality review — 2026-05-25 r21

**Reviewer**: code-quality-r21 (cron-pilot)
**HEAD**: `8718120b`
**Prior round**: r20 (HEAD `dea68995`)
**Lens**: code-quality
**Scope since r20**: R20-I1 ADR extract (`ed30f5d0` restore_handler.rs −125 / docs/decisions/2026-05-25-vm-index-retry-policy.md +112 / deferred.md +6) + R19-API1 takeover message rephrase + `closure_ref` tracing (`fde4f51c`) + R19-T1 §10.0 renderer test extension (`6fbfafb3`) + driver pin v6→v7 (`8718120b`).

## Summary

- **4 findings**: 0 critical, 0 important, 4 minor. r20's R20-I1 IMPORTANT is CLOSED clean.
- `cargo test -p zeroship-sandbox --lib --release`: **425 pass / 1 ignored / 0 failed** (unchanged from r20). No drift; the r20 +11 was the R19-C1/R19-I1/R19-I4 PR cluster; this round was doc-only + tracing-only + scripts-only.
- `cargo build -p zeroship-sandbox --tests --release`: **2 warnings, unchanged from r20** (unused `SandboxAuth` import in `restore.rs:43`; unused `WAKE_JOBS_T_KEEP` const in `sweep.rs:96`). No new warnings introduced this round.
- **R20-I1 (ADR extract) ships clean** at `ed30f5d0`. The rustdoc trimmed from 116 → 13 lines (a 91% reduction); the ADR at `docs/decisions/2026-05-25-vm-index-retry-policy.md` is self-contained (112 lines, Context / Decision / Consequences) with a forward-reference back to the implementation. The 13-line inline summary now reads: formula by mode (Sync/Async), the constants in plain English, and a single `See docs/decisions/...` pointer. r17-Q1 (3-round carry: r17 → r18-M2 → r19-M2 → r20-I1) finally CLOSED with the cheapest structural fix.
- No new `unwrap()` / `expect()` in production code across the four commits. The doc-extract commit is pure deletion + ADR creation; the rephrase commit edits a SQL literal + a tracing field; the test commit adds two assertions on an existing test loop; the scripts commit bumps a version string. Saturating-math discipline preserved (no narrowing casts touched).
- No new `pub` items. R19-API1 (rephrase commit) does not change visibility. R19-T1 only extends an existing test fixture (`render_uses_state_mismatch_message_for_internal_error`).

## CRITICAL

None.

## IMPORTANT

None.

## MINOR

### [R21-M1] r17-Q3 silent `WakeJobState::Failed` fallback at `db.rs:1621` still OPEN — unchanged across 4 rounds (r17 → r18-M4 → r19-M4 → r20 carry)

- **File**: `crates/sandbox/src/db.rs:1621`
- **Snippet**:
  ```rust
  state: WakeJobState::from_str_opt(state_str).unwrap_or(WakeJobState::Failed),
  ```
- **Issue**: when `state_str` is something the enum doesn't know about (e.g. a row inserted by a forward-incompatible binary), the row silently materialises as `WakeJobState::Failed` — defensive but mute. Migration 0009's CHECK constraint guarantees the column is in-domain, so this branch is structurally unreachable today. The fallback is paperwork for "if a future schema bump adds a new state, the reader doesn't crash". Cosmetic-only carry; promoting to `unreachable!("CHECK constraint guarantees in-domain state: {state_str:?}")` would be louder + truer to the actual invariant, but the `Failed` fallback is also defensible — a future operator who adds a state via migration but forgets to update the reader gets "wake-job stuck at failed" instead of a worker panic.
- **State**: r17-Q3 OPEN since r17. No change this round (no edits to `wake_job_row_from_pg` in any of `ed30f5d0`, `fde4f51c`, `6fbfafb3`, `8718120b`).
- **Suggested fix**: same as r19-M4 — change to `unreachable!()` with diagnostic; OR add a `// SAFETY: pg CHECK constraint at migration 0009 makes this branch unreachable...` comment lifting the trade-off into the code rather than the review backlog. One-line change either way. Deferred.

### [R21-M2] R19-API1 rephrase + closure_ref tracing land clean, but the breadcrumb is in TWO places (SQL literal + tracing message) — single source of truth still open from R20-M1

- **Files**: `crates/sandbox/src/db.rs:3334-3337` (SQL UPDATE breadcrumb) + `crates/sandbox/src/sweep.rs:398-405` (tracing message text)
- **Issue**: R19-API1 (`fde4f51c`) rephrased BOTH the SQL `error_message` literal AND the tracing message to refer to the same shape ("wake worker aborted: controller did not complete the wake within the timeout"). They are now consistent, but they're independent string copies — a future operator who rephrases one for readability will skew them. The R20-M1 recommendation (hoist to a single `const WAKE_TAKEOVER_ERROR_MESSAGE: &str = "..."` near `WakeErrorCode::WakeWorkerAborted`, interpolate into both the SQL via `format!` + the tracing field) would close that gap.
- **State**: R20-M1 carried unchanged. Not promoted; the SQL parameterisation cost (one extra string per UPDATE) is the only blocker, and the row count is bounded by orphan count (rare event).
- **Suggested fix**: same as R20-M1.

### [R21-M3] R19-API1 added `closure_ref = "R19-C1"` to the takeover tracing event — useful as a lineage breadcrumb, but the field semantics are non-obvious from the field name alone

- **File**: `crates/sandbox/src/sweep.rs:398-405`
- **Snippet** (post-`fde4f51c`):
  ```rust
  tracing::info!(
      target: "sandbox::wake::takeover",
      claimed = n,
      threshold_secs,
      closure_ref = "R19-C1",
      "sandbox wake_jobs takeover: claimed orphan rows \
       (wake worker aborted mid-wake; rows \
       transitioned to failed/wake_worker_aborted)"
  );
  ```
- **Issue**: `closure_ref = "R19-C1"` is a review-finding identifier — useful when correlating runtime logs with the deferred-review changelog, but new operators / dashboard authors have no way to know what "R19-C1" decodes to without grepping the review tree. The field name `closure_ref` doesn't telegraph its purpose either; `review_ref` or `lineage` would be more discoverable, OR a comment above the field could lift the contract. (The other tracing fields — `claimed`, `threshold_secs` — are self-documenting.)
- **State**: new in r21. Not behavioural; the field doesn't break log shape, only adds a literal.
- **Suggested fix**: either (a) rename to `lineage_ref` (broader umbrella), OR (b) add a `// closure_ref correlates this log with the deferred-review changelog entry — see docs/reviews/sandbox-snapshot-restore-deferred.md` comment above the field. Option (b) is cheaper. Deferred.

### [R21-M4] R19-M1 / R19-M5 carry tracker: sanitizer CIDR-table refactor + `insert_wake_job_fresh` helper still uncreated — breakevens still not crossed

- **Files**: `crates/sandbox/src/wake_machine.rs:751-833` (sanitizer) + `crates/sandbox/tests/sandbox_pg_e2e.rs` (10 fixture sites)
- **State**: unchanged since r20. No new IANA-reserved-block sanitizer prefix landed (R19-M1's breakeven trigger); no new fresh-insert pg-gated fixture site landed (R19-M5's). Both are correct as-is.
- **Suggested fix**: same as r19-M1 / r19-M5 — extract on next breakeven.

## Cross-lens consensus

- **R20-I1 ships clean.** The ADR at `docs/decisions/2026-05-25-vm-index-retry-policy.md` is self-contained — Context, Decision, Consequences. The C-7 / R14-A6 / C-8a / C-8b / smoke-r13 retrospective / C-7-LT-1 inflection points each get a dedicated subsection. The inline 13-line summary at `restore_handler.rs:184-197` covers the formula by mode, the constants by name, and the ADR pointer — sufficient for someone reading the code without forcing them to context-switch into the ADR unless they actually need the history. The doc-extraction pattern is now established for any future `from_X_timeout`-class function that grows past ~50 lines of rustdoc; none exist today (`from_host_fence_timeout` is the lone matching symbol per `Grep "fn from_\w+_timeout"`).
- **r19-API1 lineage is correctly recorded.** The takeover message ("wake worker aborted: controller did not complete the wake within the timeout") is now the authoritative string in both the SQL UPDATE and the tracing event. The R19-C1 lineage moved from the embedded `error_message` into a `closure_ref` tracing field (architecture cleaner: error_message is a user-facing operator breadcrumb; closure_ref is a developer-facing review-lineage breadcrumb). The two roles are now disambiguated.
- **r19-T1 §10.0 renderer test extension is correct.** `WakeErrorCode::WakeWorkerAborted` is now exhaustively covered alongside the other six variants in `render_uses_state_mismatch_message_for_internal_error`; the additional `state == "failed"` + `message.is_string()` assertions tighten the contract beyond what r19's `wire_code()` check pinned. No new test files, just +2 assertions inside the existing loop.
- **No new unwrap()/expect() in production code.** The four commits this round add ZERO unwrap/expect anywhere — doc-only + tracing-only + assertion-only + scripts version-bump only. Production code paths are byte-identical to r20 except for one tracing field addition and one SQL literal rephrase.
- **No new pub items.** R20's pub→pub(crate) tightening (`with_wake_response_mode`) is preserved; nothing new became pub this round.

## Lens hand-off — architecture / concurrency / api-surface / test-coverage / performance

1. **Architecture**: R20-I1's ADR is the structural precedent for any future doc that crosses ~50 lines + accumulates 3+ historical-inflection grafts. The pattern (rustdoc = formula + constants + ADR pointer; ADR = full retrospective + Decision + Consequences) is now established. Architecture-r21 may consider whether the `wait_for_agent_livez` rustdoc (60 lines, R19-I1's two-phase probe + fingerprint matching narrative) also crosses the threshold — borderline but presently the doc is sectioned (## R19-I1 subheader), which acts as a natural ADR alternative.
2. **Concurrency**: no changes this round. R20-M4 (R19-I4 retry-on-pg-error contract) carries forward — still cosmetic.
3. **Api-surface**: no new pub items; `closure_ref` is a tracing-internal field, not part of any public contract. R21-M3 (field naming) is api-surface-adjacent — defer to api-surface-r21 if they want to weigh in on the rename.
4. **Test coverage**: R19-T1 (+2 assertions) tightens an existing variant-loop test; no new test files. 425/0/1 unchanged from r20.
5. **Performance**: no changes this round. R20-M2 (`RETURNING wake_id` + discard) carries forward.
6. **No regressions**: lib tests 425/0/1 (=r20). No new warnings. Saturating-math discipline preserved. No new pub items.

## Carried-finding status

| Finding | Source | r21 state |
| --- | --- | --- |
| r17-Q1 (doc inflation in `from_host_fence_timeout`) | r17 → r18-M2 → r19-M2 → r20-I1 | **CLOSED** at `ed30f5d0`. Rustdoc 116 → 13 lines; ADR at `docs/decisions/2026-05-25-vm-index-retry-policy.md`. |
| r17-Q2 (doc off-by-one) | r17 → r18-M3 → r19-M3 → r20 closed | CLOSED at `ce66c10f` (per r20). |
| r17-Q3 (silent `WakeJobState::Failed` fallback at `db.rs:1621`) | r17 → r18-M4 → r19-M4 → r20 carry | **OPEN** (unchanged). Carried to R21-M1. Migration 0009 CHECK makes the branch unreachable; cosmetic only. |
| R19-M1 (sanitizer CIDR-table refactor) | r17-S1 → r19-M1 → r20-M5 | **OPEN** — carried to R21-M4. No new prefix; breakeven not crossed. |
| R19-M5 (`insert_wake_job_fresh` helper extraction) | r18 → r19 → r20-M6 | **OPEN** — carried to R21-M4. No new fresh-insert fixture site. |
| R20-I1 (ADR extract) | r20 IMPORTANT | **CLOSED** at `ed30f5d0`. |
| R20-M1 (breadcrumb const hoist) | r20 MINOR | **OPEN** — carried to R21-M2. R19-API1 rephrased the literal but did not hoist. |
| R20-M2 (`RETURNING wake_id` discards rows) | r20 MINOR | **OPEN** (unchanged). No edits to `claim_orphan_wake_for_recovery` body. |
| R20-M3 (Phase 2 residual 500ms ureq ceiling) | r20 MINOR | **OPEN** (unchanged). No edits to `wait_for_agent_livez` Phase 2 this round. |
| R20-M4 (R19-I4 retry-on-pg-error contract doc) | r20 MINOR | **OPEN** (unchanged). |
