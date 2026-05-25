# Sandbox snapshot-restore code-quality review — 2026-05-25 r22

**Reviewer**: code-quality-r22 (cron-pilot)
**HEAD**: `ef11edb3`
**Prior round**: r21 (HEAD `8718120b`)
**Lens**: code-quality
**Scope since r21**: R20-C1 SQL guard at `update_wake_job_state` (`ccb2abc8` tests/deferred + `afa5da96` SQL + rustdoc) · r17-Q3 explicit `DataIntegrity` path (`17d65f83`) · readyz envelope alignment (`528c3c44`, +2 lib tests) · C-7-LT-7 cold-boot `user_id` (`03d2f4a8`) · r21-A1 restore-path `user_id` (`fcac5355`) · driver pins v7→v9 (`4d73a5d1`, `f2641e88`) · r25-r29 reviewer artifacts.

## Summary

- **5 findings**: 0 critical, 1 important, 4 minor.
- `cargo test -p zeroship-sandbox --lib --release`: **429 pass / 1 ignored / 0 failed** (r21: 425). Delta `+4` = r17-Q3 (+2 precondition tests) + R10-API4 readyz (+2 envelope tests). R20-C1's +2 tests live in `tests/sandbox_pg_e2e.rs` (integration, not `--lib`).
- `cargo build -p zeroship-sandbox --tests --release`: **2 warnings, unchanged from r21** (unused `SandboxAuth` import in `restore.rs:43`; unused `WAKE_JOBS_T_KEEP` const in `sweep.rs:96`). No new warnings introduced this round.
- **R20-C1 SQL guard is correct** but adds one new lib-level race vector worth tracking (R22-I1 below). The predicate `AND state NOT IN ('ok','failed')` does the right thing at all 3 call sites in `wake_machine.rs`, but the no-op-on-stale-write semantics cannot be observed by callers because all three call sites discard `rows_affected` (the `Phase::Ok`/`Phase::Failed` terminal writes log Err but ignore `Ok(0)`; `set_state` is best-effort). Combined with `WakeMachine`'s own state-transitions, no actual bug — but the rustdoc claim "callers do not need to handle this specially" is true only because no caller looks at `n`.
- **r17-Q3 cascade analysis**: `DatabaseError` enum has 9 variants now (`Pg`, `SchemaTooOld`, `BootTimeout`, `MigrationFailed`, `Validation`, `CasLost`, `NotFound`, `SelfTakeoverRefused`, **`DataIntegrity(String)`** new). The enum is `pub` but **not** `#[non_exhaustive]`. A `Grep "match.*DatabaseError\|DatabaseError::.* =>"` confirms NO exhaustive `match` on `DatabaseError` anywhere in `crates/sandbox/src/` — all sites pattern-match a specific variant (e.g. `Err(DatabaseError::CasLost { .. })`) and fall through `_ => ...`. **No code breaks** from adding the variant. External consumers (none in-tree) could break if they exhaustively matched; the pub-token re-baseline in api-surface r21 (1080 → 1081) acknowledges this is a public-surface addition.
- **r21-A1 quality**: field-list parity between cold-boot (`nomad_ch.rs:2436-2462`) and restore (`restore_handler.rs:2344-2371`) is now **identical** post-fix: both emit 14 fields (`vm_index`, `kernel`, `cpus`, `memory_mb`, `restore_from`, `sandbox_id`, `user_id`, `workspace_img`, `user_home_img`, `pubkey_hex`, `subnet_base_octet`, `disks`, `fs`, `net`). Two intentional value-divergences (restore's `pubkey_hex = ""` because CH ignores cmdline on `--restore`; cold-boot's `restore_from = ""`) are documented inline. r21-A1 CLOSED clean.
- **No new `unwrap()` / `expect()` in production code** across this round's commits. R20-C1's SQL change is a pure WHERE-clause addition; r17-Q3's path swaps `.unwrap_or` for `match { ... Err(...) }`; r21-A1 adds one map entry; the readyz commit threads `ErrorEnvelope::new()`. None introduce panics.
- **One pub addition**: `DatabaseError::DataIntegrity(String)` (r17-Q3). api-surface r21 baseline-bumped accordingly.

## CRITICAL

None.

## IMPORTANT

### [R22-I1] R20-C1 SQL guard ships clean, but **all 3 call sites discard `rows_affected`** — the terminal-overwrite-attempt is now invisible to operators and tests

- **Files**: `crates/sandbox/src/wake_machine.rs:128-145` (`Phase::Ok` terminal write), `:161-178` (`Phase::Failed` terminal write), `:487-500` (`set_state` intermediate write); contract function `crates/sandbox/src/db.rs:3207-3260` (`update_wake_job_state`).
- **Behaviour**: the new `AND state NOT IN ('ok','failed')` predicate silently turns racing terminal+terminal writes into `Ok(0)`. All three callers either swallow the result entirely (`set_state` doesn't bind `n`) or log on `Err` but ignore `Ok(0)`. So a takeover-sweep → wake-machine `Phase::Ok` race that previously would have *overwritten* the failed row (data loss; the bug R20-C1 fixed) now leaves the failed row intact (correct behaviour) — but the wake-machine logs `"wake_machine: terminal ok"` and proceeds, while pg actually still holds `failed`. The client's subsequent poll sees `failed`, not `ok`. Correctness is preserved (the sweep's authoritative-failure recording wins), but the lineage is invisible: there is no `if n == 0 { trace::warn!(...) }` anywhere.
- **Why this matters**: the rustdoc claim at `db.rs:3202-3206` — *"Callers do not need to handle this specially — the terminal state already reflects the correct outcome"* — is true on the data plane, but **observability** suffers. A future operator triaging "client saw `failed`, controller logs say `ok`" has no breadcrumb that the terminal-overwrite guard tripped. The R20-C1 pg-gated tests at `sandbox_pg_e2e.rs:4934,4999` cover `Ok→Restoring` and `Failed→Restoring` (i.e. stale-write-to-non-terminal), but **not** the more likely race `Failed→Ok` (sweep wins, then wake-machine's terminal-ok write arrives) — the exact code path the rustdoc cites as the motivating example.
- **Suggested fix** (additive, no SQL change): at the three call sites, branch on `Ok(0)` and emit a `tracing::warn!(target: "sandbox::wake::terminal_overwrite_blocked", wake_id, attempted_state, "stale terminal write blocked by R20-C1 guard")`. Optional: add a counter `sandbox_wake_terminal_overwrite_blocked_total{attempted=ok|failed|restoring|...}` to quantify how often the race fires in prod. Two ~5-line edits in `wake_machine.rs`. Plus a pg-gated `update_wake_job_state_after_failed_to_ok_is_noop` test that pins the more interesting flavour. ~40 LOC total.
- **Severity**: IMPORTANT. The data-plane correctness is sound; the observability gap is the real risk (smoke debugging blind on this race), and the rustdoc currently overstates the no-special-handling claim. Pairs naturally with R19-API1's `closure_ref` tracing pattern (R21-M3 carry).

## MINOR

### [R22-M1] r17-Q3's `DataIntegrity(String)` variant uses an opaque `String` payload — typed fields would let callers do more than log

- **File**: `crates/sandbox/src/db.rs:310-317`.
- **Snippet**:
  ```rust
  #[error("data integrity: {0}")]
  DataIntegrity(String),
  ```
- **Issue**: every other `DatabaseError` variant carries typed fields (`CasLost { sandbox_id, expected_generation, observed_generation, current_host_id }`; `NotFound { sandbox_id }`; `SelfTakeoverRefused { host_id }`). `DataIntegrity(String)` regresses to free-form text. The current call site at `db.rs:1630-1633` builds `"wake_jobs row has unknown state {:?} — schema/code drift?"`, which forces any future caller-side branching to substring-match. A typed form — e.g. `DataIntegrity { table: &'static str, column: &'static str, observed: String, hint: &'static str }` — would let callers / tests pattern-match without coupling to message format, mirroring r19-M4's prior recommendation for `WakeJobState::Failed`.
- **State**: new in r22. Cosmetic; no current caller branches on `DataIntegrity` (one call path, log-and-bubble).
- **Suggested fix**: convert to a struct variant once a second `DataIntegrity` call site lands. Premature today.

### [R22-M2] R20-C1's pg-gated tests cover stale-write-to-`Restoring` but not the more interesting terminal→terminal races

- **File**: `crates/sandbox/tests/sandbox_pg_e2e.rs:4934-5066`.
- **Issue**: both new tests (`update_wake_job_state_after_ok_is_noop`, `update_wake_job_state_after_failed_is_noop`) attempt `terminal → Restoring` stale writes. The race the rustdoc *actually* cites (sweep writes `failed`, then wake-machine writes `ok`) is the terminal→terminal flavour — `Failed→Ok` or `Ok→Failed`. Both are also blocked by the predicate, but neither is pinned in test. Combined with R22-I1's call-site discard, this means a regression that flipped the predicate to `AND state != 'ok'` would slip through CI.
- **Suggested fix**: extend the two existing tests to also drive `update_wake_job_state(row, Ok, ...)` and `update_wake_job_state(row, Failed, ...)` after the row reaches the opposite terminal, asserting `n == 0` and that the original state survives. ~30 LOC.

### [R22-M3] r21-A1's `user_id` flows into `cfg.user_home_dir_root.join(user_id)` *without* `validate_typed_id` — pure code-quality angle on security-r21's R21-S1

- **Files**: `crates/sandbox/src/restore_handler.rs:2256-2259` (`join(user_id)`); `:2034-2055` (`submit_restore_job` entry — no `validate_typed_id`).
- **Issue**: r21-A1 wires `user_id` end-to-end correctly (field appears in driver Config; field-list parity restored), but the value is `&str` with no validator guard. Cold-boot validates: `validate_typed_id(user_id, "usr", "user_id")?` at `nomad_ch.rs:472`. Restore-path skips the check. The string flows into a `PathBuf::join` (path-traversable: `"../foo"` would escape the user root) AND into the driver Config field that feeds the per-user-home allow-list. Security r21 R21-S1 flags this as IMPORTANT on the security lens; on the code-quality lens it's the second instance of the same "two-emitter, no shared validator" pattern that produced the bug r21-A1 itself fixed.
- **Suggested fix**: lift the cold-boot validator to a top-of-function precondition in `submit_restore_job` (or in `build_restore_nomad_job_json` itself for full symmetry). One-line change; reuses `zeroship_core::typed_id::parse_with_prefix`.
- **Severity**: MINOR here (security r21 owns the IMPORTANT). Cross-lens consensus: ship the validator addition.

### [R22-M4] r17-Q1 / R20-M1 / R20-M2 / R20-M3 / R20-M4 / R19-M1 / R19-M5 carry — none became cosmetic-only this round

- **State**: R20-M1 (single-source breadcrumb hoist) — `fde4f51c` rephrased the SQL literal + the tracing message in parallel, but did not extract to a `const`. Still OPEN, not yet cosmetic; one future-rephrase divergence away from biting. R20-M2 (`RETURNING wake_id` returns rows that are immediately discarded; `Ok(rows.len() as u64)`) — pure cosmetic but unchanged; carry forward. R20-M3 (Phase 2 residual 500ms ureq ceiling) — unchanged; the smoke-r19 perf decomp at perf r21 R21-P1 identifies this same 60s ch.sock probe as the top WAKE lever, so it's no longer cosmetic-only at the perf lens. R20-M4 (R19-I4 retry-on-pg-error contract doc) — unchanged. R19-M1 (sanitizer CIDR-table refactor) + R19-M5 (`insert_wake_job_fresh` helper) — breakevens still not crossed.
- **r21 carries delisted**: R21-M1 (silent `WakeJobState::Failed` fallback) **CLOSED at `17d65f83`** — the explicit `Err(DatabaseError::DataIntegrity(...))` path now fires loud + grep-able on schema drift. R21-M2 (R20-M1 breadcrumb const hoist) — unchanged; not delisted. R21-M3 (`closure_ref` field naming) — unchanged; cosmetic-only carry-forward acceptable.

## Cross-lens consensus

- **R20-C1 SQL guard ships clean on the data plane.** Concurrent terminal+terminal write (sweep writes `failed`, wake-machine writes `ok` concurrently) is correctly serialised by pg row-level locks: whichever transaction reaches the WHERE-evaluation first wins, and the loser's UPDATE returns 0 rows. Both directions safe. The new variant of the bug is observability, not correctness — R22-I1.
- **r17-Q3 cascade lands without breakage.** `DatabaseError::DataIntegrity(String)` is a pure-addition to a non-`#[non_exhaustive]` enum; no in-tree code exhaustively matches `DatabaseError`. api-surface r21 baselined the pub-token bump (1080 → 1081). External consumers: none in-tree, so no observable breakage.
- **r21-A1 closes the field-list-parity gap, but adds the second instance of the "no shared validator" pattern.** Cold-boot validates `user_id` via `validate_typed_id`; restore does not (R22-M3 / security r21 R21-S1). The api-surface r21 recommendation (R21-API2 field-list-parity contract test, ~30 LOC) covers field *presence*; a separate validator-call-symmetry test (security r21 R21-S2) covers field *validation*. Both should ship as a pair.
- **No new unwrap()/expect() in production.** R20-C1 SQL guard is a pure SQL/rustdoc change; r17-Q3's match swap replaces `.unwrap_or` with `match { Err(...) }`; r21-A1 adds one JSON map entry; readyz envelope is `ErrorEnvelope::new(...)`. Production code paths gain ZERO new panic vectors.
- **Build remains clean at 2 stale warnings.** The unused `SandboxAuth` import in `restore.rs:43` and the unused `WAKE_JOBS_T_KEEP` const in `sweep.rs:96` have persisted ≥5 rounds. Drive-by fixes cost ~30 seconds each; defer until a future commit touches those files for cause.

## Lens hand-off — architecture / concurrency / api-surface / test-coverage / performance / security

1. **Architecture**: r21-A1 closure flags the "two-emitter no-shared-rule" pattern in deferred.md. R22-M3 surfaces the security-lens manifestation. Architecture-r22 may want to consider whether `build_restore_nomad_job_json` and the cold-boot equivalent should share a typed input struct that *enforces* `validate_typed_id` at construction. ~50 LOC + 1 contract test would close the entire R21-API2 + R22-M3 + security r21 R21-S2 triplet.
2. **Concurrency**: R20-C1 CLOSED on data-plane. R22-I1 (observability of guard-fires) and R22-M2 (terminal→terminal tests) are the natural concurrency-r22 hand-off. The takeover sweep itself (`claim_orphan_wake_for_recovery`) is unchanged this round.
3. **Api-surface**: R21-API2 (field-list parity contract test) is the natural follow-up to r21-A1. `DatabaseError::DataIntegrity(String)` is a pub-token addition the r21 baseline already absorbed.
4. **Test coverage**: lib 425 → 429 (+4). R22-M2 hand-off — extend R20-C1 tests to cover terminal→terminal. Cold-boot ↔ restore field-list parity test (R21-API2) is the bigger ask.
5. **Performance**: no change. R20-M3 (60s ch.sock probe ceiling) is still the top WAKE lever per perf r21 R21-P1; not code-quality's lane to resolve.
6. **Security**: R22-M3 cross-references security r21 R21-S1. Code-quality lens recommendation: lift the validator at the restore entry point; pair with security-r21 IMPORTANT.

## Carried-finding status

| Finding | Source | r22 state |
| --- | --- | --- |
| r17-Q1 (doc inflation in `from_host_fence_timeout`) | r17 → r20-I1 closed | CLOSED at `ed30f5d0`. |
| r17-Q3 (silent `WakeJobState::Failed` fallback at `db.rs:1621`) | r17 → r18-M4 → r19-M4 → r20 → r21-M1 carry | **CLOSED at `17d65f83`**. Now `Err(DataIntegrity(...))`. |
| R19-M1 (sanitizer CIDR-table refactor) | r17-S1 → r19-M1 → r20-M5 → r21-M4 | OPEN — breakeven not crossed. |
| R19-M5 (`insert_wake_job_fresh` helper extraction) | r18 → r19 → r20-M6 → r21-M4 | OPEN — breakeven not crossed. |
| R20-I1 (ADR extract) | r20 IMPORTANT → r21 closed | CLOSED. |
| R20-M1 (breadcrumb const hoist) | r20 → r21-M2 | OPEN; carry to R22-M4. |
| R20-M2 (`RETURNING wake_id` discards rows) | r20 → r21 | OPEN; cosmetic carry. |
| R20-M3 (Phase 2 residual 500ms ureq ceiling) | r20 → r21 | OPEN; perf-lens now active. |
| R20-M4 (R19-I4 retry contract doc) | r20 → r21 | OPEN; cosmetic carry. |
| R20-C1 (terminal-overwrite SQL guard) | r20 concurrency CRITICAL | **CLOSED on data plane at `afa5da96`**; observability gap surfaces as R22-I1. |
| R21-M3 (`closure_ref` field naming) | r21 | OPEN; cosmetic carry. |
