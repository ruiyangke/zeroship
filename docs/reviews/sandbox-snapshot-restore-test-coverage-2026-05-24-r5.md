# Test-coverage review — 2026-05-24 round 5

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `15b4f9a8`
**Lens**: test-coverage
**Last reviewed (this lens)**: 2026-05-24 r4

## Summary

8 findings (2 CRITICAL, 4 IMPORTANT, 2 MINOR). r4 #1 (R3-T3 wrapper)
**partial-closed** by R4-T1's shellcheck gate; r4 #2 (T8) + r4 #5 (decode
tail) still **open**. B19 ships a direct backend-level test
(`nomad_ch.rs:4562`) and a stop+release test (`:4624`) but **the wire-up
through `do_restore_inner` line 450-463 is uncovered** (every existing
`restore_sandbox` call passes `persist=None`, taking the warn-and-skip
branch). A4's 21 wire-shape tests genuinely assert `body["error"]` /
`body["message"]` shape, not just status. Lib tests = 269 at HEAD
(brief said 266 — slightly stale; trend now 223→230→230→237→238→240→
245→266→**269**).

## CRITICAL

### 1. B19 wire-up gap — `do_restore_inner` `register_restored` call site has zero in-repo coverage

`crates/sandbox/src/restore_handler.rs:450-463`. The new `if let Some(p)
= persist { … backend.register_restored(…) }` block is the entire
B19 contract: every prod restore MUST hit it. But all 5 in-repo callers
of `restore_sandbox` (`tests/sandbox_pg_e2e.rs:2504,2545,2570,2769,2882`)
pass `persist=None`, taking the `else` warn-skip arm at lines 464-474.
The direct backend test at `nomad_ch.rs:4562` exercises
`NomadCHBackend::register_restored` standalone, and the stop+release
test at `:4624` proves the slot-leak fix, but neither covers the
trait-dispatch wire-up: `RealRestoreBackend::register_restored`
(restore_handler.rs:1018-1047) is dead code under `cargo test --lib`.
A regression that drops the call (or swaps the i16/u16 conversion at
:1029, or rebuilds the agent_url at :1035) still passes every test.
**Fix**: add a `StubRestoreBackend.register_restored_calls` recorder
field, and a unit test that drives `do_restore_inner` with a `Some(p)`
where `p` is a minimal `Persistence` fixture (`persist.rs` already
exposes `for_test_only` helpers used at `lib.rs:1517`); assert the
recorder vector contains `(sid, vm_index, &user_id[..])` after Ok.

### 2. r4 #2 still open — `ControllerIdleSnapshotter::snapshot_one` non-pg cov (T8)

`crates/sandbox/src/sweep.rs:296-387`. Unchanged since r3. R3-Q1 fixer
(a11ccb2d) split out `snapshot_rows_chunked` (470-524) which now has
solid T7 + R3-Q1 cov, but the prod `snapshot_one` body stays untested:
AppState wiring-guard (317-324), `lookup_source_vm_ops` Err-mapping
(329-332), `StateMismatch→debug` swallow (372-381), teardown-fail warn
(357-369). r3+r4 fix stands: extract a pure `classify_snapshot_outcome`
table-drivable helper.

## IMPORTANT

### 3. r4 #1 partial-close — R4-T1 lint.sh adds gate, wrapper logic still uncovered

`crates/sandbox/tests/scripts_lint.rs:39-108` ships **two** tests:
non-ignored skip-on-missing arm + `#[ignore]` "required" arm for CI.
Validated `bash -n crates/sandbox/scripts/lint.sh` passes; on this host
shellcheck is absent so the gate is currently the skip-arm. **What's
covered**: syntax + error-level regressions (B12-class unquoted globs,
`[[ ]]` syntax bugs). **What's still NOT covered**: wrapper *logic* —
the 437-LOC `nomad-vm-wrapper.sh` still has zero behavioural test
(B17's `ch-remote resume` block at lines 366-419, W1 sed sink at
359-363, B18's `zsbx_pubkey` injection). R4-T1 closes the syntax half
of r3 #2; the behavioural half (a `bash` driver that mocks `ch-remote`
+ asserts argv ordering) is still open as R3-T3-behavioural.

### 4. r4 #3 still open — A3 local wake-latency canary not implemented

`crates/sandbox/src/restore_handler.rs:286-412`. r4 sketched a 100-ms
`SlowGetSnapshotStore` + concurrent-do_restore_inner canary that passes
post-A3 only. No code added in this cycle. Same fixture would also pin
`std::thread::sleep` regressions in `wait_for_alloc_running_blocking`
(:1088) / `wait_for_livez_blocking` (:1121).

### 5. r4 #5 still open — `read_snapshot_row` decode tail no non-pg test

`crates/sandbox/src/restore_handler.rs:242-280`. r1→r5 unchanged. 60
`#[ignore]`d pg-gated cases (`tests/sandbox_pg_e2e.rs`, 58 `#[test]`
+ 2 ignore-annotated, 60 ignore-tagged in total across the suite)
cover happy decode only; missing-vm_index / wrong-sha-length /
truncated-row corner cases unguarded.

### 6. NEW — `with_nomad_handle` + `nomad_ch_handle()` accessors zero direct cov

`crates/sandbox/src/restore_handler.rs:879` (`with_nomad_handle`) and
`crates/sandbox/src/backend/mod.rs:467` (`nomad_ch_handle()`) are the
B19 wiring entry points. Five test-cov searches yield zero `#[test]`
bodies touching either. Combined with finding #1 above, an
`AppState::from_config` regression that builds `rb_inner` *without*
chaining `.with_nomad_handle(h)` (lib.rs:653) would land green — the
`None` branch at lib.rs:655-663 emits a `tracing::warn!` and continues.
Promote that warn to a `debug_assert!` in test builds, OR add a unit
test that asserts the returned `Arc<dyn RestoreBackend>` (lib.rs:665)
delegates `register_restored` correctly.

## MINOR

### 7. R3-Q3 fixture bump landed without a test

Commit 28f60d73 bumped `alloc_running_timeout_secs 60→120` across 4
source files (config.rs, docker.rs, nomad_ch.rs, lib.rs). No
fixture-defaults regression test was added: a future revert to 60s
would only surface via a flake in cluster smoke. Acceptable given r4
#7 (no proptest crate-wide); flagging for visibility.

### 8. R3-T2 builder-fuzz still uncovered; A7 happy test landed

`crates/sandbox/src/config.rs:1010-1026` `with_token_replaces_existing`
adds the single happy test for A7 (mirrors A6b's
`with_config_replaces_existing` pattern). Builder fuzz on the other 8
builders (A5/A6/A6b + 5 minor) **still absent**: zero `proptest`
crate-wide, no Arbitrary impls. The current cov is 9 single-call happy
tests; a single-builder proptest harness (e.g. random ApiToken bytes
through `with_token` + assert byte-equality round-trip) would shake
out drop-order / zeroize edge cases the manual test misses. Tracking
only; no severity bump from r4.

---

## What's pinned vs. open from r4

| r4 finding | Status |
| --- | --- |
| #1 R3-T3 wrapper zero in-repo cov | **Partial-close** (R4-T1 syntax gate; behavioural still open). |
| #2 T8 `snapshot_one` non-pg cov | **Open** (this round #2). |
| #3 A3 local wake-latency canary | **Open** (this round #4). |
| #4 B18 controller-side `zsbx_pubkey` distinctness | Not re-checked. |
| #5 r1-C2 decode tail | **Open** (this round #5). |
| #6 test-fixture API consistency | Positive, no action. |
| #7 pg-suite split | NOT recommended, no action. |
| #8 R3-Q1 timer-free | Positive, no action. |
| **NEW** B19 trait-dispatch wire-up | **This round #1 CRITICAL**. |
| **NEW** with_nomad_handle / nomad_ch_handle accessors | **This round #6 IMPORTANT**. |

## Numbers at HEAD

- lib tests: **269** (was 266 in brief; +3 likely from A4 wire-shape
  fills + B19 regression pair counted under nomad_ch unit module).
- pg-gated `#[ignore]`d test bodies: **60** across `sandbox_pg_e2e.rs`
  (3 in `scripts_lint.rs` are R4-T1 gates, 10 in
  `sandbox_admin_e2e.rs` are admin e2e — total 73 `#[ignore]`d).
- pg-gated suite is single-file 3.2k LOC. Re-checked: still NOT
  recommended to split (r4 #7 unchanged).
