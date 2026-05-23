# Test-coverage review — 2026-05-24 round 4

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: a9e568a2
**Lens**: test-coverage
**Last reviewed (this lens)**: 2026-05-24 r3

## Summary

8 findings (2 CRITICAL, 4 IMPORTANT, 2 MINOR). r3 #1 (T8) + r3 #2 (R3-T3 wrapper) + r3 #5 (decode tail) still **open**. r3 #3 (T7 flake) + #4 (A5/A6 fuzz) unchanged. R3-Q1 fix landed a clean *timer-free* test (sweep.rs:729-816). `shellcheck` against the wrapper produces 5 INFO-only diagnostics, **zero error/warning** — `shellcheck --severity=error` lands today, no fixes. Lib tests = 238 passed.

## CRITICAL

### 1. r3 #2 still open — wrapper zero in-repo cov; shellcheck clean
`crates/sandbox/scripts/nomad-vm-wrapper.sh` (437 LOC post-B17). Ran shellcheck via fresh nix-store binary: 5 INFO-level only (SC2329 unused-func at 97+278, SC2012 `ls`-not-`find` at 317, SC2086 word-splitting at 434-435). **Zero ERROR, zero WARNING.** A drop-in `tests/wrapper_lint.rs` shelling `bash -n` + `shellcheck --severity=error crates/sandbox/scripts/*.sh` lands GREEN today and catches regressions for $0. B17 fix (lines 366-419 `ch-remote resume`) and the W1 sed sink at 359-363 stay unguarded; with B18 now blocking B-SLO each new wrapper bug is $0.50+ per cluster discovery.

### 2. r3 #1 still open — `ControllerIdleSnapshotter` zero non-pg cov (T8)
`crates/sandbox/src/sweep.rs:296-387`. R3-Q1 fixer (a11ccb2d) split out `snapshot_rows_chunked` (lines 470-524) — that helper now has solid T7 + R3-Q1 coverage. **But the prod `snapshot_one` body at 296-387 stays untested**: AppState wiring-guard at 317-324, `lookup_source_vm_ops` Err-mapping at 329-332, `StateMismatch→debug` swallow at 372-381, teardown-fail warn at 357-369. A regression flipping the Ok/Err arms of the match at 329 still passes every test. r3's fix stands: extract a pure `classify_snapshot_outcome` helper, table-drive it.

## IMPORTANT

### 3. NEW — local wake-latency canary for A3 is straightforward (9.5s p50 measured)
`crates/sandbox/src/restore_handler.rs:286-412`. `do_restore_inner` takes `&dyn SnapshotStore` + `&dyn RestoreBackend` (286-293); `StubRestoreBackend` exists at line 521. Add a `SlowGetSnapshotStore` whose sync `get` blocks 100 ms, drive **two concurrent `do_restore_inner` calls** in `#[compio::test]`, assert wall-time ≈ 100 ms (concurrent) vs ≥ 200 ms (serialized). Pre-A3 the test fails (sync `get` blocks the single compio thread on the second future); post-A3 passes. Local-only canary for the cluster's 9.5s wake p50 — no GCS, no CH. Same fixture pins A3's `std::thread::sleep` in `wait_for_alloc_running_blocking` / `wait_for_livez_blocking` (lines 1088,1121).

### 4. NEW — B18 (slot reuse pubkey) controller-side coverage gap
`crates/sandbox/src/backend/nomad_ch.rs:589-596` derives `pubkey = signing_key.verifying_key()` and the wrapper injects `zsbx_pubkey=<hex>`. The actual B18 bug lives in rootfs `init.sh` (bash), but a Rust-side test that asserts **two `create_sandbox` calls emit distinct `zsbx_pubkey=` cmdline values** would pin half the contract. `grep zsbx_pubkey` in `nomad_ch.rs` returns 5 matches — all doc-comment, none in `#[test]` bodies. Prevents a controller-side regression (e.g., key cached at boot) that would manifest identically to the bash bug.

### 5. r3 #5 still open — `read_snapshot_row` decode tail no non-pg test
`crates/sandbox/src/restore_handler.rs:242-280`. r1→r4 unchanged. No `decode_snapshot_row` extraction, no unit cases for missing-vm_index / wrong-sha-length / well-formed. 60 `#[ignore]`d cases in `tests/sandbox_pg_e2e.rs` (3168 LOC, 120 `#[test]` callsites) cover happy path only.

### 6. NEW — test-fixture API is consistently used across 6 test files (positive)
21 `new_fixture`/`for_setter_test_only` hits across 8 files. All 5 e2e suites (`sandbox_admin_e2e.rs`, `sandbox_pg_e2e.rs:2604/2619`, `sandbox_preview_*`, `sandbox_typed_id_e2e.rs`) plus 4 in-tree unit fixtures go through them. No rogue `AppState { … }` literals. `Database::for_setter_test_only` (lib.rs:1713,1716) used in exactly one test — name is intentionally awkward. If A1/A3 fixers need a wider mock surface, promote to a `MockDatabase` trait. Flagging for tracking, no action.

## MINOR

### 7. Migration coverage is solid; pg-gated suite should NOT be split
`tests/sandbox_pg_e2e.rs:52-204` already covers `migration_up_creates_full_schema`, `migration_apply_is_idempotent`, `migration_concurrent_migrators_race`. All 8 migrations (`0001-0008*.sql`) flow through `db.run_pending_migrations()` (db.rs:631-771). **No gap.** Re: splitting 3168-LOC suite — at 120 `#[test]` callsites the monolith is fine; the 60 `#[ignore]`d are gated on `SANDBOX_PG_TEST_DSN`, splitting would only fragment fixture code that `sandbox_admin_e2e.rs` already imports.

### 8. R3-Q1 test is timer-free (CI-flake-safe)
`crates/sandbox/src/sweep.rs:729-816`. No `sleep`, `Duration`, `Instant`, `elapsed` — uses an `AtomicBool` flipped by the snapshotter itself after `cap` invocations. Deterministic. Contrast T7's `sweep_concurrency_is_actually_concurrent` (sweep.rs:685-705) which still asserts `elapsed < 500ms` (R3-T1 open). Proptest absence (r3 #7) unchanged: `Cargo.toml` zero `proptest`/`quickcheck`. A2 helper at `snapshot_store_gcs.rs:621-664` still lacks zero-byte / chunk-boundary / overflow / trailing-bytes coverage (r3 #6 four corner cases unchanged).

---

## What's pinned vs. open from r3

| r3 finding | Status |
| --- | --- |
| #1 T6/T8 `ControllerIdleSnapshotter` non-pg cov | **Open** (this round #2). |
| #2 R3-T3 wrapper zero in-repo cov | **Open** (this round #1). |
| #3 R3-T1 T7 wall-time flaky | **Open**, not re-checked. |
| #4 R3-T2 A5/A6 fuzz | **Open**, not re-checked. |
| #5 r1-C2 decode tail | **Open** (this round #5). |
| #6 A2 verify corner cases | **Open**. |
| #7 No proptest crate-wide | **Open**. |
