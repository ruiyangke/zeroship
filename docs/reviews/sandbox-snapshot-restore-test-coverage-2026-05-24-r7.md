# Test-coverage review — 2026-05-24 round 7

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `27aa393a`
**Lens**: test-coverage
**Last reviewed (this lens)**: 2026-05-24 r6

## Summary

7 findings (3 CRITICAL, 3 IMPORTANT, 1 MINOR). Lib tests = **283** at
HEAD (verified). Trend 269 → r5 275 → **r6 280 → r7 283 (+3)**: the
+3 came from r6's B21/R5-S1 truth-table additions (`b25a4ea1`,
`7a094786`); since then, `93348b91` (R4-S1/R5-API1/R5-API2 pub→
pub(crate) restriction) shipped without adding regression tests, and
`27aa393a` is artifacts only. Neither of r6's two CRITICAL script-
validation harnesses (systemd_unit_consistency, script_artifact_
consistency) landed — a **2-cycle no-build** on the highest-yield
gap. Worse: `gcp-worker-startup.sh:150` already drifted from
`virtio-blk-v3` → `virtio-blk-v4` while the wrapper at
`nomad-vm-wrapper.sh:35,82,253` still references the unversioned
`rootfs-slim.img`; the harness would have caught any wrapper drift in
the same direction.

## CRITICAL

### 1. r6 #1 — `tests/systemd_unit_consistency.rs` still not written, B21-class re-armable

`gcp-worker-startup.sh:347-410` writes the heredoc. **Concrete
sketch** (~50 LOC, no external deps):

```rust
#[test]
fn snapshot_enabled_implies_persist_auth_block() {
    let src = include_str!("../scripts/gcp-worker-startup.sh");
    let unit = extract_heredoc(src, "/etc/systemd/system/zsbx-ctl.service");
    let envs: HashSet<&str> = unit.lines()
        .filter_map(|l| l.strip_prefix("Environment="))
        .map(|l| l.split('=').next().unwrap())
        .collect();
    if envs.contains("SANDBOX_SNAPSHOT_ENABLED") {
        for k in ["SANDBOX_PERSIST_AUTH",
                  "SANDBOX_AEAD_KEY_PATH",
                  "SANDBOX_PERSIST_DIR"] {
            assert!(envs.contains(k),
                "B21-class regression: {k} missing while SNAPSHOT_ENABLED=true");
        }
    }
}
```

**Would have caught B21**: yes — directly. The pre-fix heredoc at
`b25a4ea1`'s parent had `SANDBOX_SNAPSHOT_ENABLED=true` and no
`SANDBOX_PERSIST_AUTH=1`; this assertion fires red. Complements the
in-source assertion at `lib.rs:821-838` which only fires at boot
(reactive). Pair them — text-level + runtime — for defense in depth.

### 2. r6 #2 — `tests/script_artifact_consistency.rs` still not written; drift is already real

Today, `gcp-worker-startup.sh:150` pulls
`rootfs-slim.img.virtio-blk-v4` while r6 referenced `v3`. Nothing
flagged the bump. Wrapper at `nomad-vm-wrapper.sh:253` uses
`$ZSBX_ARTIFACT_DIR/rootfs-slim.img` (unversioned, post-rename) —
consistent, but the harness needs to lock both. **Sketch**:

```rust
#[test]
fn rootfs_gs_pull_matches_wrapper_cp() {
    let startup = include_str!("../scripts/gcp-worker-startup.sh");
    let wrapper = include_str!("../scripts/nomad-vm-wrapper.sh");
    let pulled = Regex::new(r#"gs_pull\s+(rootfs-slim\.img\S*)\s"#)
        .unwrap()
        .captures(startup).expect("startup pulls a rootfs")[1].to_string();
    // startup renames pulled→$ART/rootfs-slim.img; wrapper expects that.
    assert!(wrapper.contains("$ZSBX_ARTIFACT_DIR/rootfs-slim.img"),
        "wrapper rootfs reference drifted from startup rename");
    // Wrapper must request virtio-blk (init.sh expects /dev/vd[abc]).
    assert!(pulled.contains("virtio-blk"),
        "B20-class regression: non-virtio-blk rootfs ({pulled}) pulled");
}
```

**Would have caught B20**: yes — pre-fix `gs_pull rootfs-slim.img.fp32`
fails the `virtio-blk` assert. ~30 LOC. Also detects v3→v4 silent bump
if you pin the major-version tag.

### 3. NEW — B22 (clock skew) is NOT testable in Rust, but **is** testable at the wrapper level

Per the deferred-backlog diagnosis (R6 §B22 / 370-375), B22's root
cause is the restore branch lacking any clock-step command. A Rust
unit test cannot validate guest behavior, but a 5-LOC script assertion
**can**:

```rust
#[test]
fn restore_branch_has_clock_sync() {
    let wrapper = include_str!("../scripts/nomad-vm-wrapper.sh");
    let restore = extract_block(wrapper, "if [ -n \"${ZSBX_RESTORE_FROM:-}\" ]; then", "else");
    let has_sync = ["hwclock", "chronyd", "chronyc makestep",
                    "clock_settime", "vsock-time"]
        .iter().any(|cmd| restore.contains(cmd));
    assert!(has_sync,
        "B22-class regression: restore branch issues ch-remote resume \
         but no guest clock step; sandbox-agent sig.rs:313 will 401");
}
```

I grep'd current `nomad-vm-wrapper.sh` for `hwclock|chrony|ntpdate|date -s|adjtimex|clock_settime` → **0 matches**. The test is RED at HEAD today, which is the correct color until R6's B22 fix lands.

## IMPORTANT

### 4. r6 #3 — B19 trait-dispatch test still uncovered

`restore_handler.rs:1069-1091` `RealRestoreBackend::register_restored`
remains untested under `cargo test --lib`. All 7 `restore_sandbox`
callers in `tests/sandbox_pg_e2e.rs:2504,2545,2570,2769,2882` pass
`persist=None`, which takes the `warn-skip` arm at `:513`. Plan
unchanged from r6 — extend `StubRestoreBackend` with a `register_
called: AtomicUsize` recorder; drive `do_restore_inner` with
`Some(persist)` via `Persistence::for_test_only`; assert post-Ok
`register_called.load() == 1`.

### 5. NEW — R5-P1b regression test gap (when it lands)

`restore_handler.rs:198-199, 343-344` take `&dyn SnapshotStore` /
`&dyn RestoreBackend`. The deferred R5-P1b carve-out flips both to
`Arc<dyn …>` so `spawn_blocking(move || …)` can `Arc::clone` into
the closure. **Regression test**: at the call-site (`do_restore_
inner`), wire a `SlowGetSnapshotStore::new(200ms, 1<<30)` and a
real `compio::runtime::Runtime`; spawn TWO concurrent restores
against distinct sandbox_ids; assert `elapsed < 1.5 × 200ms`. If
the implementor accidentally leaves a `.block_on` on the
foreground task, this fails at ~400ms. Pair with the A3 wake-
latency canary from r5 #4 (still open).

### 6. r6 #4 — T8 (`ControllerIdleSnapshotter::snapshot_one`)

`sweep.rs:306-387`. **7 rounds open**. Production `IdleSnapshotter`
impl is exercised by zero tests; only `RecordingIdleSnapshotter`
(`sweep.rs:264-285`) covers the trait. The four conditional arms
(`:317-324`, `:329-332`, `:357-369`, `:372-381`) all lack coverage.
Test approach unchanged: `MockSnapshotSandbox` trait wrapping the 3
`AppState` reaches, drive 4 cases. ~120 LOC.

### 7. NEW — R6-P1 detach-teardown regression test (when it lands)

`admin_handlers.rs:1291` `teardown_source_for_snapshot` is currently
inline-awaited; R6-P1 detaches it via `compio::runtime::spawn`
post-CAS. **The regression test**: inject a `BlockingTeardown` fake
into `Backend::teardown_source_for_snapshot` that holds for 2s;
record `t0 = Instant::now()` at the snapshot POST, assert response
arrives in `<200ms`, then assert the teardown fake's
`completed.load()` is still `false` at response-time. Bonus: after
`compio::time::sleep(3s)`, assert `completed == true` to pin the
"detached but actually runs" half.

## MINOR

### 8. R3-T2 builder fuzz — still no proptest/quickcheck dep

`grep -E "proptest|quickcheck" crates/sandbox/Cargo.toml` → **empty**
at HEAD. The 6 `with_*` builders (`lib.rs:316-378`) are still hand-
covered by `field_setter_tests` (linear). A 30-LOC proptest over the
6-tuple of `Option<Arc<dyn ..>>` × `Option<Persistence>` × bool would
cost ~150 ms and pin idempotency.

---

## Numbers at HEAD

- lib tests: **283** (verified, `27aa393a`). r6→r7 delta **+3**
  (r6 said 280, r6 doc body said 275 — stale-count drift in the
  doc; my fresh count at r6's `b25a4ea1` was 280).
- script LOC: 2087 / 8 `.sh` (was 2046 — +41 in B21 / B22 territory).
- behavioral script-cov: still **0**. shellcheck gate (`scripts_lint.rs`)
  catches syntax only; nothing exercises text invariants across files.

## What's pinned vs. open from r6

| r6 finding | r7 status |
| --- | --- |
| #1 systemd unit consistency | **Open** (#1 here). |
| #2 script artifact consistency | **Open** (#2). Drift v3→v4 already happened. |
| #3 B19 trait-dispatch wire-up | **Open** (#4). |
| #4 T8 `snapshot_one` non-pg cov | **Open** (#6). **7 rounds**. |
| #5 wrapper behavioural cov | Tracking. |
| #6 A3 wake-latency canary | Tracking → see #5 (R5-P1b). |
| #7 r1-C2 decode tail | Tracking. |
| **NEW** R5-P1b regression | **#5 here**. |
| **NEW** R6-P1 detach regression | **#7 here**. |
| **NEW** B22 clock-sync grep | **#3 here**. |

## Trend

r6's recommendation was a ~70-LOC investment for two CRITICALs. Two
cycles later: 0 LOC delivered. The +3 lib-test growth was entirely
the in-source `assert_persist_required_when_snapshot_enabled` truth-
table — the *reactive* boot assertion, not the text-level *preventive*
gate. With B22 active and R6-P1 / R5-P1b queued, three more script-or-
async-bug surfaces are accumulating ahead of the test bench. The
preventive gates are now ~50% cheaper per LOC than the bugs they would
prevent (B20 cost a full cluster cycle; B21 cost two).
