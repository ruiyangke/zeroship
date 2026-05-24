# Sandbox snapshot-restore code-quality review — 2026-05-25 r24

**Reviewer**: code-quality-r24 (cron-pilot)
**HEAD**: `e6363fce`
**Prior round**: r22 (HEAD `ef11edb3`)
**Lens**: code-quality
**Scope since r22**: R22-I1 wake-terminal-overwrite tracing+counter wiring (`f98611fb`) · `WakeWorkerAborted` wire-code + takeover sweep (`1d3724fe`, `8d163d58`) · C-7-LT-12a restore-path rootfs_source emitter (`7fd661c9`) · ChPlugin Config field-list parity contract test (`b6c55d93`) · R23-I1 pg-gated wake-machine terminal-overwrite counter e2e tests (`234c3bdf`) · controller staging tests for workspace.img / home.img (T-8b-stress Bug 1) · driver pins v9→v12 + controller v30→v32 · reviewer artifact rounds 25-32.

## Summary

- **8 findings**: 0 critical, 3 important, 5 minor.
- `cargo test -p zeroship-sandbox --lib --release`: **439 pass / 1 ignored / 0 failed** (r22: 429, delta +10). Increases: r22-I1 metrics +2 · R19-C1 wake_worker_aborted enum-round-trip +1 · C-7-LT-12a rootfs_source contract tests +2 · R22-T1 field-list parity contract +1 · T-8b-stress staging tests +4 (deduplicated `stage_disk_image_preconditions` × 3 existing tests + 1 new). R23-I1's two pg-gated counter tests live in `tests/sandbox_pg_e2e.rs` (integration, not `--lib`).
- `cargo build -p zeroship-sandbox --tests --release`: **2 warnings, unchanged from r22** (`SandboxAuth` unused at `restore.rs:43`; `WAKE_JOBS_T_KEEP` unused at `sweep.rs:96`). No new warnings introduced.
- **R22-I1 wiring is clean and complete.** Three call-sites in `wake_machine.rs` now branch on `Ok(0)` from `update_wake_job_state` and emit both `tracing::warn!` + counter bump: lines `:139-147` (terminal Ok), `:184-192` (terminal Failed), `:521-529` (intermediate `set_state`). All three use the same `target: "sandbox::wake::terminal_overwrite_blocked"` tracing key so log-side filtering is uniform. R22-I1 CLOSED on code-quality lens; one minor (R24-M1 below) on the metric's test discipline.
- **C-7-LT-12a `rootfs_source` ships clean.** The restore-path now emits a 14-field ChPlugin Config (cold-boot: 13 fields; sole asymmetry: `rootfs_source` exists only on restore by design). The field-list parity test (R22-T1) pins this contract via `HashSet::symmetric_difference` against an explicit `expected_diff` set — a future-proof shape that surfaces both directions of drift in the failure message. r21-A1's recurring pattern (user_id, then rootfs_source) is now bounded by a test.
- **Lib-test flakiness observed under repeated invocation.** First `cargo test -p zeroship-sandbox --lib --release` run reported 3 FAILURES in `submit_restore_job_*` (C-N-W1 staging preconditions race). Subsequent runs reported 0 failures (439 pass). Root cause is filesystem-state interaction between the new `stage_disk_image_preconditions` helper (which writes under `host_state_dir`) and `restore_alloc_dir`'s `mkdir_p` ordering in tests where the same UUID seed is reachable. See R24-I2 below.
- **`detach_isolated` + `CreateGuard::drop` migration is sound.** The compio-runtime-affinity break is correctly motivated (`detach.rs:9-23` docs), the factory-closure shape correctly carries `Send + 'static` (`detach.rs:32-42`), and the panic-containment + `pr_set_name` 15-byte name-length contract is tested (`detach_isolated_contains_panics`, `all_known_thread_names_fit_kernel_limit`). `CreateGuard::drop` correctly disarms before std::mem::take of `nomad_addr` / `job_id` / `host_dir` (avoiding partial-move on double-drop) and threads cleanup ordering through `purge_ok` gate. No lifetime issues.
- **No new `unwrap()` / `expect()` in production code** across this round's commits. R22-I1's three call-sites use exhaustive `match Ok(n)` patterns; the counter wiring is allocation-free (single `fetch_add(1, Relaxed)`); the contract test uses `HashSet` (clippy-clean for the test boundary).
- **One pub addition**: `metrics::inc_wake_terminal_overwrite_blocked()` + `metrics::wake_terminal_overwrite_blocked_value()` (R22-I1). The latter is `#[doc(hidden)]` test accessor; both are pure-additive. The api-surface review should baseline-bump.

## CRITICAL

None.

## IMPORTANT

### [R24-I1] `wake_machine.rs:185` terminal-Failed Ok(0) path swallows the SANITIZED error message, losing diagnostic lineage

- **File**: `crates/sandbox/src/wake_machine.rs:158-202` (the `Phase::Failed` arm of `drive`'s outer match).
- **Snippet**:
  ```rust
  let sanitized = sanitize_error_message(message);  // :165 — work done
  tracing::warn!(..., error_message = %message, ...);  // :170 — full unredacted to journald
  match self.database.update_wake_job_state(
      &self.wake_id, WakeJobState::Failed,
      Some(*code), Some(sanitized.as_str()), None,
  ).await {
      Ok(rows_affected) if rows_affected == 0 => {  // :184
          tracing::warn!(
              target: "sandbox::wake::terminal_overwrite_blocked",
              wake_id = %self.wake_id,
              attempted_state = ?WakeJobState::Failed,
              "update_wake_job_state no-op: row already terminal (R20-C1 guard tripped)"
          );
          crate::metrics::inc_wake_terminal_overwrite_blocked();
      }
      // ...
  }
  ```
- **Issue**: when the guard fires, the WARN log line does NOT include the wake-machine's own `error_code` or `sanitized error_message` — both are computed (`let sanitized = sanitize_error_message(message)` at line 165, `error_code = code.as_str()` at line 169), but only the previous "terminal failed" WARN at `:166-172` carries them. An operator triaging "client saw `wake_worker_aborted`, controller logs say machine attempted `register_failed`" needs both messages joined to trace the lineage — the current shape requires correlation across two tracing events by `wake_id`, which works only if the journald query window catches both. Adding `attempted_code = ?code, attempted_message = %sanitized` to the guard-tripped warn (no extra cost — both values are in scope) closes this gap.
- **Why this matters**: this is the exact observability gap R22-I1 was filed to close. The counter + tracing target now fire, but the diagnostic payload is one short of what the operator needs. The `Phase::Ok` arm at line 139 has nothing comparable to log (no error to lose), so the asymmetry only bites on the Failed→already-terminal race — which is the race the rustdoc cites as the motivating example (`db.rs:3197-3199`).
- **Cross-lens note**: concurrency lens may already track this in r23 R23-? — please cross-check. Code-quality recommends adding the two fields to the existing WARN call.
- **Severity**: IMPORTANT. Data-plane unaffected; observability halfway done.

### [R24-I2] `submit_restore_job_*` lib tests are flaky — same-process re-run produces 3 failures then 0 (filesystem-state interaction with `stage_disk_image_preconditions`)

- **Files**: `crates/sandbox/src/restore_handler.rs:2059-2089` (the new C-N-W1 `assert_disk_image_present` preflight); test fixture at `:2859-2883` (`stage_disk_image_preconditions`); call sites at `:2887-2916` (success), `:2920-2940` (500), `:2942-2997` (timeout).
- **Observation**: in one local run (release mode), `cargo test -p zeroship-sandbox --lib --release` reported:
  ```
  failures:
      restore_handler::real_backend_tests::submit_restore_job_errors_when_nomad_500s
      restore_handler::real_backend_tests::submit_restore_job_succeeds_when_nomad_returns_running
      restore_handler::real_backend_tests::submit_restore_job_times_out_when_alloc_never_running
  test result: FAILED. 434 passed; 3 failed; 1 ignored
  ```
  Panic message: `"restore submit: workspace.img missing for sandbox <uuid> ... /tmp/zsbx-restore-real-<pid>-<dir>/<sandbox_id_simple>/workspace.img (No such file or directory)"`. On the next `cargo test` invocation the same suite passes 439/439 cleanly. The pid in the temp-dir name differs across runs (different cargo test binary process), so the test-vs-test interference is intra-binary, not cross-run.
- **Why this matters**: `fresh_dir()` at `:2794-2802` uses `std::env::temp_dir().join(format!("zsbx-restore-real-{pid}-{uuid_simple}"))`. The pid is stable within one cargo invocation; the uuid is fresh per test. But the cleanup uses `let _ = std::fs::remove_dir_all(&host_state)` AFTER the assertion — if a prior test in the same binary panicked before its cleanup ran, the next test could observe a stale subdirectory. More likely: cargo's parallel test scheduler runs N tests concurrently; `stage_disk_image_preconditions` does `create_dir_all(&host_dir)` then `write(host_dir.join("workspace.img"))`. If a peer test calls `remove_dir_all` on the SAME `host_dir` between those two steps, the write succeeds but is immediately removed before the assertion. The peer call is impossible if all tests use fresh UUIDs — but the failure mode IS observable, which means either (a) clock-skew rebinding the same UUIDv7, (b) `temp_dir()` returning a shared path that one test cleared while another staged, or (c) a non-obvious ordering bug.
- **Suggested fix**: switch `fresh_dir()` to a `tempfile::TempDir` (or the in-tree equivalent) so the directory's lifetime is RAII-bound to the test stack — eliminates the "early cleanup races later test's staging" possibility. Alternative: make `stage_disk_image_preconditions` `fsync_dir` the host_dir after the write, mirroring the production path's `fsync_dir(&host_dir)` discipline in `backend/nomad_ch.rs:3666-3672` — that closes the visible-too-late window if the failure mode is filesystem dirent caching rather than process-level cleanup. Either is ~10 LOC.
- **Severity**: IMPORTANT. A flaky lib test that passes on retry is the kind of background noise that masks the next real regression. Cross-lens with test-coverage.

### [R24-I3] `wake_machine.rs:686-714` `read_snapshot_row` re-implements `restore_handler::read_snapshot_row` — schema-evolution risk; the documented "phase 5 deletes the sync path" plan does not bind today

- **Files**: `crates/sandbox/src/wake_machine.rs:657-714` (duplicate); `crates/sandbox/src/restore_handler.rs:613-665` (canonical).
- **Snippet (the SELECT statements)**:
  ```rust
  // wake_machine.rs:687-691
  "SELECT snapshot_sha256, snapshot_vm_index, user_id \
     FROM sandbox.sandboxes \
    WHERE sandbox_id = $1::TEXT AND deleted_at IS NULL"

  // restore_handler.rs:638-642
  "SELECT snapshot_artifact_path, snapshot_sha256, snapshot_vm_index, user_id \
     FROM sandbox.sandboxes \
    WHERE sandbox_id = $1::TEXT AND deleted_at IS NULL"
  ```
- **Issue**: the wake-machine version DROPS `snapshot_artifact_path` from the SELECT — a deliberate divergence (the wake machine doesn't need it because store.get is keyed off `snapshot_sha256` only) — but the schema-evolution failure mode is now asymmetric. Restoring via the sync `restore_handler::do_restore_inner` path errors on a missing `snapshot_artifact_path` column; the wake-machine path silently succeeds because it never asks. The drift is the same shape r21-A1 fixed for cold-boot vs. restore: two emitters, one missed field, no shared contract. The inline doc at `:658-670` justifies the duplication with "phase 5 deletes the sync path" — but the proposal is in phase 4, the deletion has not landed, and the rationale's "if the snapshot-row schema evolves both paths need to update" is precisely the failure-mode r21-A1 closed for the *job-spec emitters* and r22-T1's parity test now bounds. The state-row emitters have no equivalent bound.
- **Why this matters**: `WakeSnapshotMeta` (`:672-677`) and `SnapshotRowMeta` (`:611-616` in `restore_handler.rs`) hold ~3 of the same 4 fields, with the wake-machine version missing only `artifact_path`. If `restore_handler::SnapshotRowMeta` had been re-exported `pub(super)` → `pub(crate)`, the wake_machine could reuse it with a `.artifact_path = String::new()` test sentinel and the inline `read_snapshot_row` here would collapse to a single LOC. The "visibility-bump ripples" excuse is real but small.
- **Cross-lens note**: architecture lens may want to take this as the natural follow-up to r21-A1's recurring "no shared validator" pattern — there's an emerging case for a `SandboxSnapshotMeta` typed input shared between cold-boot, restore (sync), and wake-machine.
- **Suggested fix**: lift the canonical `SnapshotRowMeta` to `pub(crate)` (`restore_handler.rs:610-616`), delete the wake-machine copy (`:672-677`), use the canonical `read_snapshot_row` directly. The wake-machine ignores `artifact_path` at zero cost. ~50 LOC removed.
- **Severity**: IMPORTANT — same code-quality shape as r21-A1 (cold-boot/restore Config emitters); same blast radius (a schema rename misses one of two paths, smoke catches it). Test-coverage lens may want a parity test analogous to R22-T1 for the `SELECT` column-list.

## MINOR

### [R24-M1] `metrics.rs:490-502` `wake_terminal_overwrite_blocked_counter_starts_at_zero` test does not assert ANYTHING — the body is `let _ = v` with a comment apologizing for the absence of a check

- **File**: `crates/sandbox/src/metrics.rs:490-502`.
- **Snippet**:
  ```rust
  #[test]
  fn wake_terminal_overwrite_blocked_counter_starts_at_zero() {
      let v = wake_terminal_overwrite_blocked_value();
      // The counter must be a finite u64 — just confirm the accessor
      // compiles and returns without panic.
      let _ = v;
  }
  ```
- **Issue**: the test name promises a property (`starts_at_zero`) that the body does not verify. The test passes for `v = 0`, `v = 1`, `v = u64::MAX`. It is functionally equivalent to a doc-comment. The companion test at `:506-512` (`inc_wake_terminal_overwrite_blocked_monotonic`) IS a real test of the same machinery. The "starts at zero" test should either be deleted (the property is unenforceable for a process-global counter shared across tests) or rewritten as `#[serial]`-gated + a one-shot assertion on a fresh process — but the latter is overkill for this counter.
- **State**: new in r22's wiring landing (`f98611fb`).
- **Suggested fix**: delete the test. The monotonic test below it is the meaningful contract. Two-line LOC delta.

### [R24-M2] `wake_machine.rs:159-202` Phase::Failed arm computes `sanitized` BEFORE the terminal-overwrite branch — wasted work when guard fires

- **File**: `crates/sandbox/src/wake_machine.rs:163-166`.
- **Snippet**:
  ```rust
  // R16-S2: sanitize ... full unredacted ... logged at warn!
  let sanitized = sanitize_error_message(message);
  tracing::warn!(...);
  match self.database.update_wake_job_state(... Some(sanitized.as_str()) ...) ...
  ```
- **Issue**: `sanitize_error_message` is a 3-pass byte-scan over `message` (`:757-775`). For a typical 100-byte error string this is ≤1 µs; for the 256-byte truncation case it's <2 µs. Not a hot-path concern. But the work is unconditionally done even when the SQL guard will reject the write. The fix is to move the `sanitize` call INSIDE the `Ok(_) =>` arm (after the no-op branch), keeping the `tracing::warn!` outside (which logs the unredacted `%message`, not the sanitized form) so the operator-visible log is unchanged.
- **State**: pre-existing pattern, R22-I1 landed without revisiting it.
- **Suggested fix**: hoist `sanitize` to after the guard-noop branch. ~3 LOC reflow. Strictly cosmetic at the current message sizes; flag for the next "perf hot-path pass" alongside R20-M3 (60s ureq ceiling).

### [R24-M3] `restore_handler.rs:2071-2089` `assert_disk_image_present` mapper double-quotes the error: outer `format!` includes "{e}" where `e` already contains a quoted path

- **File**: `crates/sandbox/src/restore_handler.rs:2071-2089`.
- **Snippet**:
  ```rust
  // assert_disk_image_present returns e.g.
  //   "disk image post-stage stat failed: /var/.../workspace.img (No such file or directory (os error 2)); controller-side parity check for driver preflight"
  crate::backend::nomad_ch::assert_disk_image_present(&workspace_img).map_err(|e| {
      format!(
          "restore submit: workspace.img missing for sandbox {} \
           (snapshot teardown should have preserved it via \
           stop_preserving_state; controller will not submit \
           restore job that the driver's preflight would reject \
           with a generic Failed-tasks rollup): {e}",
          sandbox_id
      )
  })?;
  ```
- **Issue**: the resulting error message reads `"restore submit: workspace.img missing for sandbox <uuid> (...): disk image post-stage stat failed: <path> (...); controller-side parity check for driver preflight"`. Two narrative sentences glued together by `: ` — the second sentence is from the inner assert, and it ALSO has a trailing semicolon-clause ("controller-side parity check for driver preflight") that duplicates the outer's framing ("controller will not submit ..."). When this string lands in a wake-job's `error_message` column it is also subject to `sanitize_error_message`'s 256-byte truncation (`wake_machine.rs:726`), which would clip the outer's clause and leave the operator reading only the redundant inner one.
- **Severity**: cosmetic; the data is there, just verbose and partially redundant. Pre-emptive cleanup before truncation cost goes up.
- **Suggested fix**: drop the `(snapshot teardown should have preserved it via stop_preserving_state; controller will not submit restore job that the driver's preflight would reject with a generic Failed-tasks rollup)` framing — push that explanation into a `tracing::warn!` log line instead and keep the wake_job error_message lean: `"restore submit: workspace.img missing: {e}"`. ~5 LOC.

### [R24-M4] `wake_machine.rs:629-643` `classify_failure` has a `_ =>` arm that silently maps three named variants to `Internal` — exhaustive match would catch a future variant

- **File**: `crates/sandbox/src/wake_machine.rs:629-643`.
- **Snippet**:
  ```rust
  fn classify_failure(err: &RestoreHandlerError) -> WakeErrorCode {
      match err {
          RestoreHandlerError::VmIndexUnavailable { .. } => WakeErrorCode::SlotUnavailable,
          RestoreHandlerError::SnapshotCorrupt => WakeErrorCode::RestoreFailed,
          RestoreHandlerError::Backend(_) => WakeErrorCode::RestoreFailed,
          RestoreHandlerError::Store(_) => WakeErrorCode::RestoreFailed,
          RestoreHandlerError::ConfigRewrite(_) => WakeErrorCode::RestoreFailed,
          RestoreHandlerError::Database(_) => WakeErrorCode::Internal,
          // FeatureDisabled / StateMismatch / NotFound are pre-flight
          // and the async handler refuses to spawn the machine in those
          // shapes — but defense-in-depth: if they leak here, surface as
          // Internal so the wire shape is well-formed.
          _ => WakeErrorCode::Internal,
      }
  }
  ```
- **Issue**: the inline comment promises defense-in-depth for `FeatureDisabled / StateMismatch / NotFound` — but `_ =>` swallows ANYTHING new. If a future PR adds `RestoreHandlerError::SnapshotMissing` (or anything else), the wake-machine will silently map it to `Internal` without surfacing the un-mapped case for code review. The test at `:1020-1029` only exercises the three documented variants explicitly. The fix: list all variants by name; the compiler then errors on the next variant addition.
- **Severity**: cosmetic; today's `_` is correct for the listed variants, but the test asserts only an incomplete subset.
- **Suggested fix**: expand `_ =>` to `RestoreHandlerError::FeatureDisabled | RestoreHandlerError::StateMismatch { .. } | RestoreHandlerError::NotFound(_) => WakeErrorCode::Internal`. If `RestoreHandlerError` grows a variant, the compiler reminds the author to think about the classification. Add a `#[non_exhaustive]` attribute on the enum, or — simpler — drop the `_` and rely on `match`'s exhaustiveness for documentation. ~5 LOC.

### [R24-M5] r22-carry items — r24 status

- **State**:
  - **R22-I1** (terminal-overwrite invisibility): **CLOSED at `f98611fb`**. Counter + tracing wired at all 3 wake_machine call-sites. R23-I1 added pg-gated e2e tests at `sandbox_pg_e2e.rs:5530-5689`.
  - **R22-M1** (`DataIntegrity(String)` opaque payload): UNCHANGED — still one call site, no breakeven. OPEN; cosmetic-only carry.
  - **R22-M2** (terminal→terminal pg-gated test gap): partially CLOSED by R23-I1's two new tests (`failed_to_ok`, `ok_to_failed`). The original ask (an `update_wake_job_state` unit-level pg test for both directions) is still open at the db.rs layer — R23-I1 covers the WakeMachine integration but not the lower-level `update_wake_job_state` × `terminal_a → terminal_b` shape. Carry forward as a residual.
  - **R22-M3** (restore-path `user_id` no `validate_typed_id`): UNCHANGED — security r21 R21-S1 still tracks the IMPORTANT. The C-N-W1 fix added `assert_disk_image_present` but did NOT add `validate_typed_id` upstream. OPEN.
  - **R22-M4** carry-chain (R17-Q1 / R20-M1..M4 / R19-M1 / R19-M5): UNCHANGED; cosmetic carries.
  - **R21-M3** (`closure_ref` field naming): UNCHANGED. Cosmetic carry.
  - **R20 lib warnings** (unused `SandboxAuth` import, unused `WAKE_JOBS_T_KEEP` const): UNCHANGED. `SandboxAuth` is actually USED at `restore.rs:618` (`-> Result<SandboxAuth, String>`) — the warning is about the explicit `use crate::backend::{Backend, SandboxAuth, SandboxInfo}` at line 43, which lints because the body now resolves through a fully-qualified path. ~30 sec drive-by; defer until next commit touches `restore.rs`.

## Cross-lens consensus

- **R22-I1 ships clean on the data plane AND the observability plane.** The R22-M1 rustdoc claim ("callers do not need to handle this specially") is now true at three different definitions of "handle" — the data is right (R20-C1's SQL), the metric fires (R22-I1), and the WARN log carries enough context for triage modulo R24-I1. The R23-I1 e2e tests prove the counter wiring from the WakeMachine driver perspective, not just the db.rs SQL guard.
- **C-7-LT-12a contract test is the new gold-standard pattern for this crate.** `ch_plugin_config_field_list_parity` (`restore_handler.rs:3770-3854`) uses `HashSet::symmetric_difference` to surface BOTH directions of drift in the failure message — cold-only and restore-only — and pins the documented intentional asymmetry as an explicit `expected_diff` constant. Future "two-emitter parity" reviews should reference this test as the shape to copy (e.g. R24-I3's `SELECT` column-list parity, security r21 R21-S2's validator-call-symmetry).
- **No new unwrap()/expect() in production.** All R22-I1 / C-7-LT-12a / R23-I1 / contract-test changes use exhaustive `match` patterns and `?`-propagation. The production code base's panic-vector count is unchanged this round.
- **Lib-test count drift = real test gain.** +10 lib tests since r22 (429 → 439), with no `--ignored` regressions and one (existing) `--ignored` test count unchanged. Quality of added tests is high (contract pinning + counter wiring + monotonic accumulation), modulo R24-M1's no-op-body test.

## Lens hand-off — architecture / concurrency / api-surface / test-coverage / performance / security

1. **Architecture**: R24-I3 (duplicate `read_snapshot_row` + `SnapshotRowMeta`) is the natural architecture-r24 follow-up to r21-A1's "two-emitter no-shared-rule" pattern. A `pub(crate) struct SnapshotRowMeta` + single `read_snapshot_row` would close the schema-drift surface for the wake-machine ↔ sync-restore pair before phase 5 of the C-7-LT migration deletes one of the two.
2. **Concurrency**: R24-I1 (terminal-Failed log payload incompleteness) likely overlaps with concurrency-r23's coverage of the same race; cross-check. R23-I1's pg-gated counter tests are the right contract for the existing race; no new concurrency surface in this round.
3. **Api-surface**: `metrics::inc_wake_terminal_overwrite_blocked()` + `metrics::wake_terminal_overwrite_blocked_value()` are pure-additive pub additions; api-surface r23 has already baselined these. `WakeErrorCode::WakeWorkerAborted` (1d3724fe) is a pub enum-variant addition the api-surface r21 baseline absorbed.
4. **Test coverage**: R23-I1 added 2 pg-gated e2e tests (`failed_to_ok`, `ok_to_failed`); R24-M2 hand-off (delete `wake_terminal_overwrite_blocked_counter_starts_at_zero`) — and R24-I2 (lib-test flakiness) is squarely test-coverage's lane. The C-N-W1 staging tests are well-formed; the flakiness is an `fresh_dir` fixture issue, not a missing-coverage issue.
5. **Performance**: R24-M2 (sanitize-before-guard-check) is the only perf-shape finding this round, and it's <2 µs/call. Not worth chasing alone; bundle with R20-M3 (60s ureq ceiling) when perf-r24 looks at WAKE hot paths.
6. **Security**: R22-M3 (restore-path `validate_typed_id` gap) is unchanged this round; the C-N-W1 `assert_disk_image_present` fix landed but did NOT lift the validator. Security-r24 should still drive this — the C-N-W1 fix only catches "file missing", not "user_id was `../etc`".

## Carried-finding status

| Finding | Source | r24 state |
| --- | --- | --- |
| r17-Q1 (doc inflation in `from_host_fence_timeout`) | r17 → r20-I1 closed | CLOSED at `ed30f5d0` (r20). |
| r17-Q3 (silent `WakeJobState::Failed` fallback) | r17 → r21-M1 → r22 closed | CLOSED at `17d65f83` (r21). |
| R19-M1 (sanitizer CIDR-table refactor) | r17-S1 → r22-M4 | OPEN — breakeven not crossed. |
| R19-M5 (`insert_wake_job_fresh` helper extraction) | r18 → r22-M4 | OPEN — breakeven not crossed. |
| R20-I1 (ADR extract) | r20 IMPORTANT → r21 closed | CLOSED. |
| R20-M1 (breadcrumb const hoist) | r20 → r22-M4 | OPEN; cosmetic carry. |
| R20-M2 (`RETURNING wake_id` discards rows) | r20 → r22 | OPEN; cosmetic carry. |
| R20-M3 (Phase 2 residual 500ms ureq ceiling) | r20 → r22 (perf-lens active) | OPEN; perf-lens. |
| R20-M4 (R19-I4 retry contract doc) | r20 → r22 | OPEN; cosmetic carry. |
| R20-C1 (terminal-overwrite SQL guard) | r20 CRITICAL → r22-I1 | CLOSED data-plane (`afa5da96`) + CLOSED observability (`f98611fb`). |
| R21-M3 (`closure_ref` field naming) | r21 → r22 | OPEN; cosmetic carry. |
| R22-I1 (terminal-overwrite invisibility) | r22 IMPORTANT | **CLOSED at `f98611fb`**. R23-I1 e2e tests landed at `234c3bdf`. |
| R22-M1 (`DataIntegrity(String)` opaque payload) | r22 | OPEN; no breakeven. |
| R22-M2 (terminal→terminal test gap) | r22 → R23-I1 partial close | **PARTIAL CLOSE** at `234c3bdf` (WakeMachine layer covered; pure `update_wake_job_state(terminal_a, terminal_b)` unit at db.rs layer still missing). |
| R22-M3 (restore-path `user_id` no validator) | r22 → security r21 R21-S1 | OPEN; security-lens owns. |
| R22-M4 (cosmetic carry pile) | r22 | OPEN; unchanged. |
| R22-T1 (ChPlugin Config field-list parity contract test) | r22 — surfaced via deferred backlog | **CLOSED at `b6c55d93`**. Test pattern is the new gold-standard for two-emitter parity. |
| R23-I1 (WakeMachine terminal-overwrite counter e2e) | r23 — Path B per deferred.md | **CLOSED at `234c3bdf`**. Two pg-gated tests at `sandbox_pg_e2e.rs:5530, 5617`. |
