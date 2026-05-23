# Test-coverage review — 2026-05-24 round 6

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `1066a319`
**Lens**: test-coverage
**Last reviewed (this lens)**: 2026-05-24 r5

## Summary

7 findings (3 CRITICAL, 3 IMPORTANT, 1 MINOR). r5 #1-#5 all open.
Zero new tests this cycle (`ac6a6bf2` no-test cleanup; `1066a319`
artifacts only). Lib tests = **275** (verified). New angle: B20 + B21
— $0.50+/cluster-cycle each — were script/systemd-unit-text bugs
that **a ~70-LOC Rust harness would catch locally**. Trend
223→230→230→237→238→240→245→266→269→**275**; delta peaked at +21
(A4 wire-shape) and is trending to zero (r10→r11 effective 0).

## CRITICAL

### 1. B21-class is locally unit-testable today

`crates/sandbox/scripts/gcp-worker-startup.sh:326-360` writes the
controller systemd `Environment=` block. B21 root cause: one missing
line (`SANDBOX_PERSIST_AUTH=1`). With `snapshot_enabled=true` +
`persist=None`, `restore_handler.rs:450-474` silently takes the
warn-skip arm; every wake leaks a vm_index. **Plan**: add
`tests/systemd_unit_consistency.rs` (~40 LOC) that parses the heredoc
and asserts (a) `SNAPSHOT_ENABLED=true` ⇒ `PERSIST_AUTH=1`, (b) every
`Environment=` key matches a known consumer in `persist.rs::from_env`
/ `lib.rs::from_config`, (c) `PERSIST_KEK_PATH` dirname matches an
earlier `mkdir -p`. Saves $0.50+/misconfig.

### 2. B20-class is also unit-testable

`gcp-worker-startup.sh:147` hard-codes `rootfs-slim.img.virtio-blk-v3`;
wrapper at `nomad-vm-wrapper.sh:35-45,82-85,253` requires exactly that
filename + virtio-blk init. B20 cost a cycle because startup pulled
fp32 → wrapper expected v3 → 0/16 creates. **Plan**: parse both
scripts, extract `gs_pull rootfs-*` + `cp $ZSBX_ARTIFACT_DIR/rootfs-
slim.img`, assert filename consistency + wrapper `--disk` lists two
virtio-blk paths. 30 LOC. Same harness catches B12/B13/B17/B20.

### 3. r5 #1 still open — B19 trait-dispatch wire-up uncovered

`crates/sandbox/src/restore_handler.rs:450-463`. **No movement since
r5.** All 5 in-repo `restore_sandbox` callers in `sandbox_pg_e2e.rs`
pass `persist=None`. `RealRestoreBackend::register_restored`
(`:1018-1047`) is dead under `cargo test --lib`. Plan: extend
`StubRestoreBackend` (`:577-605`) with a recorder, drive
`do_restore_inner` with `Some(p)` via `Persistence::for_test_only`
(`lib.rs:1517`), assert post-Ok the recorder is non-empty. **Adding
this test would have caught B21 too** — it exercises the persist=Some
branch B21's misconfig disables.

## IMPORTANT

### 4. r5 #2 still open — `ControllerIdleSnapshotter::snapshot_one` (T8)

`crates/sandbox/src/sweep.rs:296-387`. Unchanged 6 rounds. Only the
`RecordingIdleSnapshotter` fake (`:248-285`) is exercised. Plan:
`MockSnapshotSandbox` trait wrapping the 3 AppState reaches; drive 4
cases — wiring-incomplete (`:317-324`), lookup Err (`:329-332`),
StateMismatch swallowed (`:372-381`), teardown-fail warn but Ok
(`:357-369`).

### 5. r5 #3 still open — wrapper behavioral cov

`nomad-vm-wrapper.sh` is **456 LOC** (r5 said 437; grew 19). Plan:
`tests/wrapper_behaviour.rs` creates a temp `$ZSBX_ARTIFACT_DIR`,
stubs `cloud-hypervisor` + `ch-remote` as bash shims recording argv,
runs the wrapper under `ZSBX_RESTORE_FROM=` unset/set, asserts argv:
cold-boot ⇒ `--kernel … --disk …`; restore ⇒ `--restore source=…` +
`ch-remote ping` + `ch-remote resume`. Catches B17 (missing resume)
pre-cluster. ~120 LOC; `scripts_lint.rs` is the template.

### 6. r5 #4 still open — A3 wake-latency canary

`crates/sandbox/src/restore_handler.rs:1254-1341`. `std::thread::sleep`
at `:1302,1335` still on hot path. Pair with deferred R5-P1 (`BufReader`
+ `spawn_blocking`); **canary lands FIRST** to pin regression. Two
concurrent `do_restore_inner` + `SlowGetSnapshotStore::new(100ms,
1_073_741_824)`; assert `elapsed < N × 100ms × 2`.

## MINOR

### 7. r5 #5 still open — `read_snapshot_row` decode tail

`restore_handler.rs:242-280`. Open 5 rounds. 60-LOC table-driven test
mocking 6 `Database::read_snapshot_row` rows (missing-vm_index, bad
sha length, truncated, valid, aead-flag-no-dek-id, inverse).

---

## What's pinned vs. open from r5

| r5 finding | Status |
| --- | --- |
| #1 B19 trait-dispatch wire-up | **Open** (#3 here). |
| #2 T8 `snapshot_one` non-pg cov | **Open** (#4). Unchanged 6 rounds. |
| #3 R3-T3 wrapper behavioral half | **Open** (#5). |
| #4 A3 wake-latency canary | **Open** (#6). |
| #5 r1-C2 decode tail | **Open** (#7). |
| #6 with_nomad_handle accessors | Subsumed by #3 (same test covers both). |
| #7/#8 fixture bump / builder fuzz | Tracking only. |
| **NEW** B21 systemd-unit consistency | **#1 CRITICAL** (local-testable). |
| **NEW** B20 script-artifact consistency | **#2 CRITICAL** (local-testable). |

## Numbers at HEAD

- lib tests: **275** (verified at `1066a319`); delta r5→r6 real 0
  (the +6 vs r5's stated 269 is stale-count drift; this cycle added
  none).
- script LOC: 2046 across 8 `*.sh`; behavioral cov **0**.

## Trend

Two consecutive cycles near-zero growth while **two bugs** (B20, B21)
were script/config-text bugs caught only at cluster smoke. Behavioral-
rust cov is plateauing as the bug surface migrates to scripts + unit
files. The 2 NEW CRITICALs are a ~70-LOC investment that closes the
gap.
