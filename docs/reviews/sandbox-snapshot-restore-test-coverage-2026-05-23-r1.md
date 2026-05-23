# Test-coverage review — 2026-05-23 round 1

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: 8ad3cf3f
**Lens**: test-coverage

## Summary
- 11 findings (3 critical, 5 important, 3 minor)
- Test count baseline: 222 lib `#[test]/#[compio::test]` attributes across `src/**`; **58** pg-gated `#[ignore]` tests in `tests/sandbox_pg_e2e.rs` + **10** in `sandbox_admin_e2e.rs`; **0** lines of shell test coverage (`nomad-vm-wrapper.sh` 421 LOC + `init.sh` 141 LOC, both `bash -n` clean but otherwise unexercised).

## CRITICAL

**[crates/sandbox/src/backend/mod.rs:412] — `teardown_source_for_snapshot` still calls `stop()` (destructive), not `stop_preserving_state()`. Bug #15 fix is dead code.**
  Why: `crates/sandbox/src/backend/nomad_ch.rs:905-910` introduces the snapshot-aware `stop_preserving_state` and `stop_inner(_, false)` branch at lines 1119-1125 specifically to fix bug #15, but **nothing in the tree ever calls it** (grep across `crates/sandbox/` returns only the definition + doc comments + the inner `tracing::info!`). The admin path at `admin_handlers.rs:1196-1200 → mod.rs:412` still routes through the destructive variant. Bug #15 would reproduce on the next cluster run. No test pinned the wiring.
  Fix: pure-Rust unit test in `crates/sandbox/src/backend/nomad_ch.rs::tests` that asserts (a) `stop_preserving_state` is invoked by `Backend::teardown_source_for_snapshot` (use a feature-gated `#[cfg(test)] fn last_stop_kind() -> StopKind` counter on the backend), and (b) after the call, the `host_dir` (a `tempfile::tempdir()`-managed `PathBuf`) still exists with a sentinel `workspace.img` file. Both halves can run without postgres or nomad.

**[crates/sandbox/src/restore_handler.rs:242] — `read_snapshot_row` has no non-pg test.**
  Why: this is the wake-path's first failure point and the only place that decodes the (artifact_path, sha, vm_index, user_id) tuple. The "missing required columns" branch (line 270-275), the sha-len-32 guard (line 276-280), and the `NotFound` mapping (line 265) are each pg-gated. Bug #15 wasn't here, but a malformed sha column from a half-rolled-back snapshotting CAS would reach this surface and a unit test could pin the error envelope shape.
  Fix: extract the row-decode tail (after `query_opt`) into a pure `fn decode_snapshot_row(row: &Row) -> Result<SnapshotRowMeta, RestoreHandlerError>` and add three tests: missing-vm_index → `Internal`; wrong-sha-length → `Internal`; well-formed → `Ok`.

**[crates/sandbox/scripts/nomad-vm-wrapper.sh:222-225 + 308-385] — wrapper restore branch has zero in-repo test coverage.**
  Why: every cluster bug in the 13+-bug chain (specifically #11 virtio-fs pivot, #14a stage-empty, #14b NO-CARRIER, #15 workspace.img missing) bottoms out in the wrapper. The file is 421 LOC and `bash -n` clean, but no `bats` / `shellcheck` / `bash -c` smoke runs in CI. The `[ ! -f $ZSBX_WORKSPACE_IMG ]` gate at line 222, the staged-file presence triple-check at 324-328, and the sed path-rewrite at 359 are all single-shot failure surfaces.
  Fix: see "What would a wrapper test suite look like?" below.

## IMPORTANT

**[crates/sandbox/src/admin_handlers.rs:179-192, 1061-1064, 1086-1093] — `err(500, ...)` violates the § 10.0 wire envelope.**
  Why: the proposal specifies `{"error": "<kind>", "message": "<human>"}` (machine-readable kind in slot 1). `err()` emits `{"error": "ch_remote: oops"}` — concatenated kind+message into the kind slot. `map_snapshot_error`'s `ChRemote`, `Store`, `Database`, `Internal` arms and `map_restore_error`'s `Store`, `Backend`, `ConfigRewrite`, `Database`, `Internal`, `SnapshotCorrupt` arms all funnel through this. The `state_mismatch`, `vm_index_unavailable`, `feature_disabled`, `not_found` arms ARE correct — proving the inconsistency is real, not a spec-vs-impl misread.
  Fix: rewrite `err()` to take `(status, kind: &'static str, message: impl Into<String>)` and emit `{"error": kind, "message": msg}`; add a single table-driven test that POSTs `/admin/.../snapshot` against a mock backend forced into each `SnapshotHandlerError`/`RestoreHandlerError` variant and asserts both fields are present and `error` matches the documented kind slug.

**[crates/sandbox/src/sweep.rs:120-337] — `run_transient_takeover_once`, `run_idle_eviction_once`, `spawn_transient_state_takeover`, `spawn_idle_eviction_sweep` have ZERO src-side tests.**
  Why: `sweep.rs::unit_tests` contains exactly one test (`recovery_target_pins_proposal_table`, line 419). The four pub async sweep entry points are exercised only pg-gated (`sandbox_pg_e2e.rs:2675/2728/2769`). The `Snapshotting → SnapshottingAborted` recovery path and the `RestoringCold → SnapshottedSuspect` transition both have important retry-loop branches that could be unit-tested via an in-memory mock `Database` trait. The "feature disabled → no-op" path is also pg-gated when it shouldn't need to be.
  Fix: factor `sweep_once_using<T: SweepDb>(...)` over a trait abstracting the 3 queries the sweep issues; add three non-pg tests (happy, feature-off no-op, partial-failure-continues).

**[crates/sandbox/tests/sandbox_pg_e2e.rs:2473, 2895] — fixtures still carry `fs[].socket` (virtio-fs legacy).**
  Why: documented in deferred-Q5. The "rewrite doesn't touch fs[]" invariant still holds, but using virtio-fs-shaped configs as wake-path fixtures means a regression that breaks virtio-blk-shaped restores (the only shape that ships) wouldn't necessarily light up these tests. Plus the per-pivot intent of the new `[ ! -f $ZSBX_WORKSPACE_IMG ]` defensive gate isn't exercised.
  Fix: refresh both fixtures to a virtio-blk-shaped `disks: [rootfs, workspace, userhome]` triple matching `init.sh:14-19` and the wrapper's `--disk` line at `nomad-vm-wrapper.sh:397`. Add an explicit assertion that the rewritten config carries 3 disk entries.

**[crates/sandbox/src/restore_handler.rs:286-407] — `do_restore_inner` rollback (`restoring → snapshotted`) is not unit-tested.**
  Why: the rollback branch at the bottom of the function (CAS on backend failure, lines ~390-410 region) is the safety net for every wake failure. The test module at line 570 covers `derive_mac/tap/rewrite_config_json` only. The `real_backend_tests` at line 1129 cover Nomad-submit/timeout but not the handler-level rollback orchestration. The `RestoreBackend` trait already exists explicitly so this is testable.
  Fix: add a `MockRestoreBackend` that returns `Err` from `submit_and_wait_running`, drive `restore_sandbox` against a tempdir-backed `LocalDiskSnapshotStore` + an in-memory pg double, assert the row's final status is `Snapshotted` (not `Restoring`) and the vm_index reservation is released.

**[crates/sandbox/src/snapshot_handler.rs:220-690] — `snapshot_sandbox` rollback (`snapshotting → running`) and the `update_snapshot_metadata` CAS-loss arm are not unit-tested.**
  Why: same shape as the restore rollback. `snapshot_handler.rs:691+` test module has 5 tests but they cover `MockChRemoteClient` mechanics and `snap_stage_dir`, not the orchestrator rollback. CAS-loss is pg-gated (`sandbox_pg_e2e.rs:2042`). A peer-races-us collision is the exact race the proposal worries about; non-pg coverage would catch a missing rollback.
  Fix: in-memory `Database` trait (already alluded to in the snapshot module for testability) + force CAS to fail; assert status reverts to `Running`.

## MINOR

**[crates/sandbox/src/restore_handler.rs:421-427] — `derive_mac` / `derive_tap` constants are tested but not pinned against the wrapper.**
  Why: tests at lines 575-585 pin the values to `12:34:56:78:9b:<index>` / `zsbx-nm-<index>`. The wrapper at `nomad-vm-wrapper.sh:~180` (search for `TAP=zsbx-nm-`) also encodes the format. Bug #6/#10-style drift could decouple controller and wrapper without any test failure.
  Fix: a Rust test that reads `nomad-vm-wrapper.sh` via `include_str!` and asserts the pattern literal matches `derive_tap(i)` for a few i. Cheap insurance.

**[crates/sandbox/src/snapshot_store_gcs.rs:283 / src/snapshot_aead.rs:682] — GCS retry-with-backoff and AEAD time_of_put_unix_secs paths are partially covered.**
  Why: Q1 + Q2 from deferred. The single-shot `put` and the DEK derivation path are documented divergences from §4. Both have small test modules that exercise the happy path but neither pins the 3× retry budget or the snapshot_taken_at vs time_of_put divergence as a deliberate test fixture.
  Fix: add one test per concern that documents the current behaviour with a comment pointing at the deferred entry; promotes the divergence from "tribal knowledge" to "test name".

**[crates/sandbox/src/restore_handler.rs:1129] — `real_backend_tests` only cover the Nomad-submit/timeout/reserve trio; livez polling not tested.**
  Why: the `wait_for_agent_livez` integration with `RealRestoreBackend` is exercised only by the `nomad_ch.rs:3850+ wait_for_agent_livez_returns_ok_when_fp_matches` test, which tests the helper in isolation. The restore-handler-level "Nomad says running, livez never 200s" timeout path has no test.
  Fix: extend `spawn_fake_nomad` with a sibling `spawn_fake_agent` that 404s on `/livez`; assert the handler rolls back to `snapshotted` after the timeout.

## What would a wrapper test suite look like?

A `crates/sandbox/scripts/tests/` directory using **bats-core** (already widely-deployed; one apt/brew package) with **shellcheck** in CI as the static gate. Each `*.bats` case sources the wrapper with `ZSBX_*` env vars set to point at a `tempdir`-staged restore tree, then asserts the specific exit code and `[wrapper] FATAL: …` stderr line for each failure mode (no `workspace.img`, no `memory-ranges`, no `config.json`, sed-rewrite failure). For the cold-boot path, mock `cloud-hypervisor` with a 1-line shell stub that just exits 0 — the test asserts the constructed `--disk` argv string and the `zsbx_pubkey=` cmdline literal. The full suite would run in under 2 seconds and would have caught bugs #6, #10, #11, #14a, and #15 locally (5 of 15) without any cluster smoke. As a minimum bar before bats, drop `shellcheck` and `bash -n` into the per-crate test runner script (`cargo test -p zeroship-sandbox` could shell out via a `tests/wrapper_lint.rs` driver) — `bash -n` is already clean today, so this is free regression coverage.
