# Sandbox snapshot-restore architecture review — 2026-05-25 r28

**Reviewer**: architecture-r28 (post-Phase-2 capability + post-r7-A/C-followup; stress-r8 IN FLIGHT)
**HEAD**: `0b8cf6c2` (round-37 reviewer artifact; tree includes `8c0b361e` start_housekeeper + `e3291b62` pg config bump + `231e66c6` driver-v18/controller-v36 pin + `6e928a25` Phase 2 controller emission). Worktree: `.worktrees/sandbox-snapshot-restore` (READ-ONLY).
**Predecessor**: r27 at `01b6a744`.
**Lens**: architecture (READ-ONLY). **NO changes proposed; in-flight stress-r8 + cluster validation pending. DO NOT touch scripts/* or in-flight ops files.**

---

## Summary

Two architectural shifts have landed between r27 and r28:

1. **Option C Phase 2 capability** (driver-side staging) shipped behind `SANDBOX_DRIVER_STAGES_DISK_IMAGES` feature flag. The staging-locality ADR (`docs/decisions/2026-05-25-staging-locality.md`) is ACCEPTED. Phase 4 cluster validation (stress-r8) is the gate for the default-flip.
2. **r7-A + r7-C-followup**: pg `max_connections=100→500` config bump (`e3291b62`) plus `Pool::start_housekeeper()` invocation in both `open_pool` and `pool_audit` (`8c0b361e`). Together these address the R26-C1 thread-local `Rc<Pool>` retention wedge that emerged in stress-r7.

The r27 review predicted the structural pivot (r27-A1 CRITICAL → staging-locality ADR → Option C). That landed. **The five-layer ladder's structural diagnostic is correctly named and the migration plan is sequenced.** What r28 must assess is *what changed* about the architectural shape, what *new* ambiguities Phase 2 introduced, and whether the defense-in-depth (pg config + housekeeper) is sufficient for stress-r8 OR whether other pg-saturation surfaces are still latent.

Five findings (1 CRITICAL, 2 IMPORTANT, 2 MINOR). The CRITICAL is about Phase 2's split-brain responsibility window — host_dir lifecycle ownership crosses the controller/driver boundary depending on a feature flag, and the flag is currently FALSE while the architectural pivot has been DECIDED. r27 carries: A1 (staging-locality ADR) CLOSED at `bbadbe68`; A2 (6th-layer prediction) PARTIALLY VALIDATED — r5 RED at same 5% e2e, predicted Candidate 2 (workspace.img-class lock collision) refuted in favor of OFD-lock-lifecycle within the same `rootfs.img` surface; A3 (abort criterion) folded into staging-locality ADR Phase 4; A4 (Option-3 break-even) USED as the decision justification; A5 (kernel-state inventory rootfs.img row) STILL NOT UPDATED in the inventory ADR; A6 (playbook ADR retros) STILL NOT WRITTEN.

---

## CRITICAL

### [r28-A1] Phase 2 introduces a flag-gated split-brain in host_dir lifecycle ownership — the responsibility split must be resolved before stress-r8 is interpretable as Phase 4 validation

The Phase 2 capability landed at `6e928a25` with the wire-schema half (`Task.Config.stage_disk_images` + `Job.Meta.zsbx_stage_disks`) and the controller-side bypass at `crates/sandbox/src/backend/nomad_ch.rs:797-821`:

```rust
let workspace_img: PathBuf = if self.cfg.driver_stages_disk_images {
    workspace_image_path(host_dir)
} else {
    // ... legacy spawn_blocking truncate + mkfs.ext4 ...
    guard.host_dir_created = true;
    staged
};
```

The branch decides which side of the controller/driver boundary owns the dirent's existence on disk. The comment block at `:774-796` documents the design intent: under flag-on the driver owns the lifecycle and `guard.host_dir_created` stays FALSE; under flag-off the controller stages and the guard rolls back on failure. **Phase 2 default is `false`**; Phase 4 cluster validation (stress-r8) flips the default; Phase 3 deletes the legacy branch.

But during the Phase 2 window — TODAY — three distinct ownership regimes exist depending on flag state and code path:

| Flag | Code path | `host_dir` creator | `workspace.img` creator | `home.img` creator | `CreateGuard::drop` rollback | Sweeper-reaper |
| --- | --- | --- | --- | --- | --- | --- |
| FALSE (default) | cold-boot | controller `spawn_blocking` | controller `mkfs.ext4` | controller `mkfs.ext4` | leaks host_dir (`d638b10f` policy) | `spawn_host_dir_gc` reaps post-grace |
| TRUE | cold-boot | driver `stageDiskImages` | driver `stageDiskImages` | driver `stageDiskImages` (?) | guard.host_dir_created=FALSE → no-op | `spawn_host_dir_gc` reaps post-grace (rustdoc says "controller created") |
| ANY | restore | controller (today, per ADR Phase 3 unchanged) | snapshot artifact via `materialize_rootfs` | snapshot artifact | guard policy unchanged | sweeper unchanged |

**Three concrete ambiguities the split-brain introduces:**

#### 1. `home.img` creator under flag-on is undefined at the typed boundary

Under flag-off, both `workspace.img` AND `home.img` are created by the `spawn_blocking` block at `nomad_ch.rs:803-815`. The block calls `create_ext4_image_if_missing(&user_home_img_owned, ...)` for `home.img` immediately after the `workspace.img` create. **Under flag-on, the controller skips the entire block — but the wire field is named `stage_disk_images` (plural).** The driver's `stageDiskImages` op (per the commit message of `b3b1fe59`) is responsible for materializing the images the typed manifest declares.

The typed manifest carried over the wire today is the `Task.Config.stage_disk_images: bool` field — a single boolean. The path information for `home.img` is still derived controller-side in `build_nomad_job_json` via `user_home_img` (the second positional arg). The driver knows the path; the driver materializes the file. **But the staging-locality ADR's Phase 2 progress note (`docs/decisions/2026-05-25-staging-locality.md:246-282`) does NOT explicitly enumerate which images the driver-side `stageDiskImages` materializes** — it says "workspace.img, home.img, and rootfs.img" in the Decision section (line 84-86) but the wire-schema half landed as a single bool, not as a typed image list with paths/sizes/modes.

If the driver-side `stageDiskImages` materializes only `workspace.img` and the controller's `user_home_img` path lands on an empty filesystem under flag-on, the first stress-r8 cycle on a fresh user will fail at CH spawn with a CH-side virtio-blk EIO on `home.img` open. The cluster review for stress-r6 (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r6.md`) ran against the flag-OFF default — it does NOT validate that stress-r8 with flag-ON exercises the home.img materialization path on first-sandbox-per-user.

**Verbatim verification required before stress-r8**: confirm the driver's `stageDiskImages` op (cross-worktree at `nomad-driver-ch/ch/stage_disks.go`) materializes BOTH `workspace.img` and `home.img` from the controller's emitted Config — or rename the wire field to `stage_workspace_image` and explicitly carve out home.img as still-controller-owned.

#### 2. `CreateGuard::drop` policy under flag-on is "no-op for host_dir" but the sweeper rustdoc still names the controller as creator

Per the ADR `Trade-offs` section item 3 (line 372-376): "host_dir GC sweeper retains its role. The v34 sweeper (`e82bffd7`) was designed for failed-CREATE leak hygiene. Under Option C the sweeper still applies — driver-staged dirents still leak under failed-stage scenarios. The enumeration mechanism is unchanged. **The rustdoc at `sweep.rs:1249` MUST be updated in phase 3 to cite the driver as creator.**"

Under flag-on Phase 2 (today, with the default false but operator-flippable in flight), the sweeper IS reaping driver-staged dirents while the rustdoc says "controller-created". This is a documentation-vs-reality divergence that the production-systems-have-correct-comments invariant violates. The closure entry [r7-C-followup] at `deferred.md:1937` notes "structural fix verified by code inspection" — but the inspection itself relies on rustdoc as the source of truth for who-creates-the-dirent. If a future engineer reads `sweep.rs:1249` to debug a sweeper bug under flag-on, they'll be misdirected.

#### 3. `CreateGuard::drop` rollback semantics differ in a way the type system does not enforce

`guard.host_dir_created = true` is set on the calling thread AFTER `spawn_blocking` returns Ok under flag-off (`nomad_ch.rs:819`). Under flag-on, the field stays FALSE. The guard's `drop` impl uses this field to gate the `rm -rf host_dir` cleanup. This means:

- Flag-off: a failed-CREATE post-spawn rollback removes the host_dir contents that the controller created.
- Flag-on: a failed-CREATE post-spawn rollback DOES NOT remove the host_dir. The driver's failed `stageDiskImages` (if it partially-materialized the dir) leaves the partial contents for the sweeper to reap on grace expiry.

This is the architecturally-correct shape under Option C (the driver owns the lifecycle, the sweeper is the safety net). **But the lifecycle invariant — "if `host_dir_created=false`, the controller is not responsible for cleaning host_dir" — is a runtime-implicit invariant, not a type-system invariant.** A future patch to `CreateGuard` that toggles `host_dir_created=true` for some other reason (e.g., recording that the controller fsync'd the parent dir) would silently break flag-on by re-introducing controller-side rollback on a driver-owned dirent.

**Severity**: CRITICAL — for stress-r8 interpretability. If stress-r8 RED at flag-on, the cluster review must distinguish between (a) Option C is itself wrong; (b) the home.img materialization path was not wired; (c) the split-brain ownership accumulates partial state that the sweeper's enumeration does not catch within its 1-hour grace; (d) a different layer entirely. Without resolving the three ambiguities above before stress-r8, the cluster review's RCA tree has a fork at every observable.

**Recommendation**: BEFORE stress-r8, land three small clarifications:

1. Update `sweep.rs:1249` rustdoc to read "Reaps host_dirs created by EITHER the controller (`spawn_blocking` block at `nomad_ch.rs:803-815`) under flag-off, OR the driver's `stageDiskImages` op under flag-on. Enumeration is creator-agnostic."
2. Verify the driver's `stageDiskImages` materializes both `workspace.img` and `home.img` from the emitted Config; if not, add a typed Config field `Task.Config.staged_images: ["workspace", "home"]` and have the driver iterate.
3. Promote `host_dir_created` from a free-form bool to a typed enum `HostDirOwnership::{ControllerCreated, DriverCreated, NotCreated}`. Make the rollback policy explicit at the boundary. ~15 LOC.

**Fix shape**: the three points above are scoped pre-stress-r8 so the cluster review's RCA tree has fewer branches. Failure to resolve them does NOT block stress-r8 mechanically; it makes RED interpretation ambiguous.

---

## IMPORTANT

### [r28-A2] Defense-in-depth (pg_max_connections=500 + start_housekeeper) is sufficient for the known wedge but other pg-saturation surfaces are still latent

r7-A and r7-C-followup together close the R26-C1 retention wedge. The fix is structural (housekeeper reaps idle conns; max-lifetime rotates them; the floor under sustained load is bounded by the per-thread `min_idle=2` not by `pool_max=16`). The pg cap bump to 500 provides headroom for bursty growth past the 384-conn steady-state floor.

But the R28-DISCIPLINE audit (round-37 artifact at `docs/reviews/sandbox-snapshot-restore-test-discipline-audit-2026-05-25-r1.md`) flagged R26-C1 as having ZERO predicate tests. The same audit lens applies architecturally to **other pg-touching surfaces that have been added or modified since R26-C1 landed**. Three candidates:

#### 1. wake-machine GC sweep tick frequency

`spawn_wake_jobs_gc` at `crates/sandbox/src/sweep.rs:331-354` polls every `WAKE_JOBS_GC_POLL_SECS` (60 s per the takeover sweep constant; the GC's interval is read separately). Each tick issues `run_wake_jobs_gc_once` which is one indexed DELETE backed by the `wake_jobs_id_idx`. The GC runs on its own `detach_isolated` OS thread + private compio runtime — so its pg conns are SEPARATE from the ntex worker's thread-local `Rc<Pool>` cache. **Each `detach_isolated` thread has its own `POOL_APP_CELL`**. Three sweepers (`wake-gc`, `wake-takeover`, `snap-idle-evict`) × 2 cached pool cells × `min_idle=2` = 12 additional steady-state conns per controller process, plus growth.

Under flag-on Phase 2 with the new Phase 4 sweeper rustdoc update pending, no NEW sweeper is added. But the existing three sweepers' connection accounting is invisible in the r7-C-followup reasoning chain (the reasoning was scoped to "32 ntex threads × 2 roles × 2 min_idle = 128 per worker"). The actual floor is 128 + 12 = 140 per worker; 3 workers = 420 steady-state. Still under 500, but the headroom is 80 not 116. Bursty growth past min_idle on the sweeper threads (the GC's pg DELETE can take several seconds under contention, opening additional conns from the pool) further narrows the headroom.

**Verbatim observability gap**: there is no per-thread or per-purpose conn count metric. `compio-postgres::Pool` does not expose `current_size` / `idle_size` over a metrics interface. The r7-A diagnostic was "psql could not connect; grep all the call sites." A stress-r8 that fails with the same wedge would require the same 8-hour diagnostic.

**Likelihood**: LOW for stress-r8 (the 500 cap absorbs the 420 floor + 80 headroom under normal stress). MEDIUM for the next-stress-after-add-a-sweeper change.

#### 2. takeover-sweep batching

`run_wake_jobs_takeover_once` (line 388-425) calls `db.claim_orphan_wake_for_recovery(threshold)` which is one indexed UPDATE per tick. Per-tick conn cost: 1 from the takeover thread's `POOL_APP_CELL`. **No batching** — the UPDATE is single-statement. So pg-saturation from this surface is bounded by the polling cadence × per-tick conn-acquisition rate, both bounded by `WAKE_JOBS_TAKEOVER_POLL_SECS=60`. Not a saturation surface in normal operation.

But under a **cluster failover** scenario where multiple controllers crash simultaneously and N takeover threads on M survivors all race for the same UPDATE batch, the pg-side row-lock contention can stall the UPDATE for tens of seconds, holding the pool conn open. This is in `compio-postgres`'s connection budget, not pg's `max_connections`. Per the `min_idle=2` config, the pool will open a second conn if the first is held by the long UPDATE. If the stall cascade hits all three sweepers simultaneously (an unhealthy pg making every sweep's transaction slow), the per-thread floor doubles transiently. Still bounded; not a wedge.

**Likelihood**: LOW. Mention here for completeness; the architectural shape is correct, the bound is just not encoded in a test.

#### 3. admin endpoint load

`admin_handlers.rs:388` issues `db.pool_app().await` per request. Under flag-on stress, the cluster review for any prior r-stress includes admin endpoint traffic (the harness `snapshot_stress.py` POSTs to `/admin/...` to drive lifecycle). Each admin handler thread reuses the ntex worker's thread-local cache; no new conns per request. But under a debugging session where an operator runs `psql` against pg server-1 from the controller node itself (a r7-A-style "even psql gets `too many clients`" reproduction), psql competes for the same cap with the controller's already-warm pools. The 500 cap absorbs psql; a 100 cap did not.

**Architectural diagnosis**: the cap bump correctly absorbs a different class of load (interactive diagnostic queries vs. application queries). The headroom calculation in the r7-A closure entry budgeted "psql / Nomad / housekeeping headroom" without quantifying. Under stress-r8 with the harness driving full load AND a parallel debugging session AND the new Phase 2 wire-field-emission verbosity (which adds tracing not pg load — so this is not a saturation surface for stress-r8 itself), the headroom is qualitatively-defined.

**Recommendation**: file a backlog item to add `pg_connections_in_use_total{purpose=ntex_worker|gc|takeover|idle_evict|admin}` as a controller metric, sampled from `pg_stat_activity` on a low-frequency tick (the R1-DISC-3 backlog item's pg_stat_activity oracle pattern composes here). ~40 LOC, defers to post-stress-r8. Not blocking.

**Severity**: IMPORTANT — the defense-in-depth is sufficient for stress-r8 in expected scenarios, but the *blast-radius* analysis of pg-saturation surfaces is incomplete. The 80-conn headroom is qualitatively narrow; an unforeseen surface (a new sweeper, an admin endpoint that holds the pool across an external API call, a misconfigured failover) could exhaust it again.

### [r28-A3] R1-DISC-3 (R26-C1 cache hit tests) — the right architectural shape uses `pg_stat_activity` as the production-state oracle; alternatives are inferior

The test-discipline audit (round-37) identifies R26-C1's predicate as untested:

> The PREDICATE: "successive calls to `pool_app()` on the SAME compio worker thread return the SAME `Rc<Pool>` (no fresh `Pool::connect_with_config` per call)."

R28-DISCIPLINE's rule mandates a fixture that reproduces production state. For this predicate, "production state" is observable in two places:

- **In the controller process**: `Rc::strong_count(&pool_a) == Rc::strong_count(&pool_b)` after both come from `pool_app()` — proves they're the same Rc.
- **In pg**: `pg_stat_activity` filtered to the controller's role-DSN, observed before and after `pool_app()` calls — proves no NEW conns were opened.

The audit recommends the pg-stat-activity approach (~80 LOC in `sandbox_pg_e2e.rs`). Below is the architectural ranking of the alternatives:

| Approach | Production fidelity | Test infra cost | False-positive rate | False-negative rate |
| --- | --- | --- | --- | --- |
| `pg_stat_activity` oracle | HIGH — pg is the source of truth for conn count | MEDIUM — needs `sandbox_pg_e2e.rs` extension + spawn N threads | LOW — pg-counted conns are not racy at the observed quanta | LOW — measures the actual production wedge |
| `Rc::ptr_eq` on cached pools | HIGH for cache predicate; LOW for retention predicate | LOW — pure Rust, no pg fixture | LOW | HIGH — does not catch the R26-C1 retention wedge that motivated r7-A (the cache CAN hit while conns simultaneously accumulate) |
| Mock `Pool::connect_with_config` with a counter | LOW — requires a test-only seam in `compio-postgres` | HIGH — cross-crate change; new API surface | MEDIUM — mock divergence | MEDIUM — connect-count != idle-conn-retention |
| Instrument `pool.get()` via metrics + assert | MEDIUM — runtime introspection; depends on a metrics tap | HIGH — requires plumbing | LOW | MEDIUM — metric scope hides the housekeeper behaviour |
| Track `pool.size()` directly | HIGH — if `compio-postgres::Pool` exposes `size()` | LOW — single API call | LOW | LOW — but the API does not exist; would require upstream change |

**The pg_stat_activity oracle is strictly dominant for the R26-C1 predicate** because:

1. It tests BOTH the cache predicate (same Rc returned) AND the retention predicate (conns are released under housekeeper) using the same fixture. The audit's example test sketch tests both as one assertion-set against `pg_stat_activity`.
2. It tests against the EXACT failure mode r7-A surfaced (pg's view of conn count exceeding cap). A pass = "this code path under THIS load shape does not exhaust pg's cap." A regression = "the load shape changed enough to exhaust." The test cost is one pg fixture spin-up.
3. It composes with future predicate additions. A second test for "DSN rotation drops the prior pool's conns within N seconds" uses the same oracle — observe pg_stat_activity at T0 with DSN-A, rotate to DSN-B at T1, assert DSN-A conns count → 0 by T2.

**Architectural shape for the backlog item**:

```rust
// crates/sandbox/tests/sandbox_pg_e2e.rs
#[ntex::test]
async fn r26_c1_cache_hit_and_retention_under_load() {
    let db = test_database_with_pool_max(16, min_idle=2);

    // Predicate 1: same compio worker → same Rc<Pool>
    let p1 = db.pool_app().await.unwrap();
    let p2 = db.pool_app().await.unwrap();
    assert!(Rc::ptr_eq(&p1, &p2), "cache hit predicate");

    // Predicate 2: N threads each call pool_app() once → N×min_idle conns
    let n = 8;
    spawn_n_compio_workers(n, |db| async move {
        db.pool_app().await.unwrap();
    });
    let conns = query_pg_stat_activity_role_count("sandbox_app", &db).await;
    assert_eq!(conns, n * 2, "N×min_idle conns held");

    // Predicate 3: housekeeper reaps idle conns past max-lifetime
    sleep(31min).await; // > max-lifetime default 30min
    let conns_after_reap = query_pg_stat_activity_role_count("sandbox_app", &db).await;
    assert!(conns_after_reap < n * 2, "housekeeper reaps past max-lifetime");
}
```

The 30-min sleep is the architectural inconvenience — predicate 3 (the housekeeper IS running) cannot be tested in a unit-test cadence without making the housekeeper interval configurable for tests. Two architectural options:

- **(a)** Add a `PoolConfig::max_lifetime_secs` knob accepting a low value for tests; production default stays 30 min. Cross-crate change but small (~5 LOC in `compio-postgres`).
- **(b)** Restrict the r1-DISC-3 backlog to predicates 1 + 2 only (cache hit + steady-state floor); test predicate 3 via an integration smoke against a real pg with `pgss_max_idle_secs` tuned low. Larger blast radius but no upstream change.

**Recommendation**: (a). The `compio-postgres` crate is workspace-internal; adding a test-tunable knob is cheap. The architectural shape "test predicate over production state via the same oracle production observes" is a workspace-wide pattern — codifying it once via the pool config sets the precedent.

**Severity**: IMPORTANT — R1-DISC-3 is the highest-leverage R28-DISCIPLINE backlog item (per the round-37 verdict). The architectural shape choice determines whether the test catches the next R26-C1-class regression cheaply or fails-open.

---

## MINOR

### [r28-A4] Phase 5 deferred items (r4-A reap-wait, r5-A OFD probe) — keep r4-A indefinitely; r5-A's deferral resolved by Option C

The staging-locality ADR Phase 5 table (`docs/decisions/2026-05-25-staging-locality.md:330-340`) enumerates layer-peel patches with KEEP/REMOVE disposition. Two entries are flagged as defense-in-depth post-Option C:

- **Driver v16 reap-wait + counter (`e7ce7f1f`+`9af429c7`)**: KEEP (defense-in-depth). Under Option C, `rootfs.img` is per-alloc hardlinked fresh; cross-alloc OFD lock contention is eliminated. The reap-wait remains as "kernel hygiene observability."
- **r5-A `F_OFD_SETLK` probe (proposed, not landed)**: DO NOT LAND. Per-alloc fresh inode means no cross-alloc lock contention to probe.

**Question r28 must answer**: are these patches *safe to leave indefinitely* given Option C's architectural shift? Or do they introduce new failure modes under flag-on stress that we don't yet observe?

**r4-A reap-wait analysis**:

The reap-wait predicate is: "DestroyTask must wait for the supervisor's `exitDone` channel before returning, OR for `kill -9` + `wait4` to release kernel-side resources." Under Option A, this gated the next alloc on the same vm_index from inheriting an unreaped CH process's OFD lock on `rootfs.img`. Under Option C, the next alloc on the same vm_index gets a fresh `rootfs.img` hardlink (per-alloc `runDir`), so the OFD lock cannot collide.

**Net effect under flag-on**: r4-A is a `wait4` for an exit that has already completed by the time `DestroyTask` returns (CH is a short-lived process; SIGKILL→reap is sub-second in healthy cases). The unreaped counter (`nomad_driver_ch_destroy_task_unreaped_total`) increments only on the pathological case where the supervisor's `exitDone` chan does not close within the timeout — this is a kernel-state observability signal independent of staging-locality.

**Is it safe to leave?** YES. The reap-wait does not introduce a new failure mode under Option C. It also does NOT consume new pg/CH/network resources (the `wait4` is in-process kernel-side). The counter is observability.

**Risk profile**: VERY LOW. The reap-wait could MASK a different bug (driver hangs on `exitDone` chan close failure indefinitely) but the timeout in the implementation bounds this. The arch r24-r27 deferred-backlog audit verified the timeout is in place.

**r5-A `F_OFD_SETLK` probe analysis**:

The r5-A patch was never landed; it was the proposed fix for stress-r5's RED (predicate "the OFD lock is acquirable via `F_OFD_SETLK` before opening as ExclusiveWrite"). Per the staging-locality ADR's "Why C, not A or B" section: "Stress-r5 RED at the identical 5% rate that r4 posted means r4-A's `exitDone`-channel-close predicate did NOT actually close the rootfs.img lock surface." The proposed r5-A probe was the 6th-layer fix; Option C makes it unnecessary by eliminating the cross-alloc lock surface.

**Is it safe to leave UNLANDED?** YES. The patch's predicate (probe OFD lock before open) only makes sense if there's a lock to compete for. Under Option C there isn't.

**Layer-peel patches that DO live under Option C** (per Phase 5 table):

- Driver v13 tap pre-delete on EEXIST: KEEP — tap is driver-owned in both worlds.
- Driver v14 defensive vm_index-keyed tap cleanup: KEEP — tap-lifecycle is per-surface from the kernel-state inventory.
- Driver v15 netdev release poll: KEEP — kernel netdev release timing is independent of staging.
- Controller v34 GC sweeper (`e82bffd7`): KEEP, rewrite rustdoc (per r28-A1 #2).

**Architectural verdict**: the Phase 5 deferral is correctly classified. The r4-A patch and the v13/v14/v15 driver patches are all KEEP-as-DiD; r5-A is correctly NOT landed; the controller v34 LEAK host_dir on stop (per `d638b10f` policy) is REMOVE-under-Option-C (Phase 3 work) because per-alloc dirs are reaped by Nomad on alloc failure.

**Severity**: MINOR — documentation. The Phase 5 dispositions are correct as stated. The follow-on architectural work (Phase 3 patch removal + sweeper rustdoc rewrite) is correctly sequenced post-stress-r8 GREEN.

### [r28-A5] r24-A2 driver-side kernel-state surface audit — order, dependencies, parallelizability

The kernel-state inventory ADR (`docs/decisions/2026-05-25-kernel-state-surface-inventory.md`) enumerates 5 OPEN surfaces beyond tap + host_dir:

1. **cgroup** (Nomad-managed)
2. **mount-ns** (overlay + virtiofs)
3. **vsock CID** (driver-injected)
4. **jailer chroot** (if used)
5. **PID files** (CH api-socket, ch.sock)

The cutover gate requires ≥4 of 5 OPEN surfaces to reach CLOSED. The closure roadmap proposes ordering by risk: cgroup → PID files → mount-ns → jailer chroot → vsock CID.

**Architectural lens: order matters, dependencies are minimal, parallelizability is high.**

#### Order analysis

The proposed order (cgroup → PID files → mount-ns → jailer chroot → vsock CID) is risk-stratified. The architectural shape of each closure (the four-part deliverable: EEXIST-safe re-entry test + orphan counter + sweeper task + grace policy) is identical, so the ordering captures *expected complexity* not *required ordering*.

The single architectural ordering constraint: **mount-ns BEFORE jailer chroot, if jailer is in use.** A jailer chroot is mounted inside its own mount-ns under typical Firecracker / CH-jailer setups; reaping the mount-ns before the chroot rmtree is required to avoid EBUSY on the rmtree. Under the v18 driver this dependency may or may not apply depending on jailer usage — the ADR's row 4 says "if used" — so the dependency is conditional.

Otherwise, the surfaces are independent:

- **cgroup** is Nomad-owned; its closure is an *audit* (verify Nomad cleans cleanly), not a new sweeper. The deliverable is a stress run with `cat /sys/fs/cgroup/.../tasks` post-cycle. **Zero new code paths.** Can run TODAY against stress-r6's bench harness with one new assertion.
- **PID files** is driver-owned; closure is a defensive `os.RemoveAll(<runDir>/*.sock)` at `StartTask` entry + counter. **Driver-side change, cross-worktree.** Independent of cgroup, mount-ns, vsock.
- **mount-ns** depends on whether driver uses a private mount-ns per VM. If yes, closure is a `/proc/*/mountinfo` post-cycle audit. If no, the row collapses to N/A. **Audit first, code second.** Independent of the others.
- **jailer chroot** depends on whether jailer is in use. If yes, sweeper. If no, N/A. **Audit first.**
- **vsock CID** is kernel-auto-cleaned; closure deliverable is a stress run that confirms zero stranded CIDs via `ss -K | grep AF_VSOCK`. **Audit only.** Zero new code paths.

#### Parallelizability

Three of the five OPEN surfaces are **audit-only** (cgroup, vsock CID, mount-ns-if-shared). These can be performed against stress-r6's existing logs OR run against any stress-r8 GREEN result — no new code, just observation. Three audits in parallel: feasible in a single sprint.

The two CODE-change surfaces (PID files driver-side, mount-ns-or-jailer driver-side if-applicable) are independent of each other and independent of the audits. Three sprints can run in parallel:

| Sprint | Surface | Type | Cross-worktree? | Estimated LOC |
| --- | --- | --- | --- | --- |
| 1 | cgroup | audit | NO — log analysis against stress-r6 + stress-r8 | 0 LOC + 1 doc-update |
| 1 | vsock CID | audit | NO — `ss -K` script + log capture | 0 LOC + 1 doc-update |
| 1 | mount-ns (shared inherit) | audit | NO — `/proc/*/mountinfo` script | 0 LOC + 1 doc-update |
| 2 | PID files | code | YES — driver `StartTask` entry sweep | ~30 LOC driver + ~10 LOC test |
| 3 | mount-ns (private) OR jailer chroot (if used) | code | YES — driver sweeper | ~80-120 LOC driver + ~40 LOC test |

Sprints 1, 2, 3 can run concurrently. Sprint 1 (audits) gates whether sprints 2 and 3 carry the right scope — if the audits show no orphans across N stress cycles, the corresponding code-change sprint can degrade to "add an empty sweeper for future-defense" or be skipped entirely.

#### Dependency on stress-r8

The audits in Sprint 1 require **a fresh stress cycle with R28-DISCIPLINE instrumentation enabled** to capture the post-cycle kernel state. Stress-r8 is that cycle. So:

- Sprint 1 audits CAN start drafting the audit script against stress-r6's existing logs.
- Sprint 1 audits CANNOT close (CLOSED status in the inventory) until stress-r8 logs are captured.
- Sprints 2 + 3 (code changes) CAN start in parallel because they don't depend on the audit outcome — they ship the defensive sweeper unconditionally; the audit informs whether the sweeper observes orphans in production.

#### Recommended sequencing post-stress-r8

```
Stress-r8 GREEN
  │
  ├─ Sprint 1a: cgroup audit (1-2 days)
  ├─ Sprint 1b: vsock CID audit (1-2 days)
  ├─ Sprint 1c: mount-ns shared-inherit audit (2-3 days)
  ├─ Sprint 2:  PID files defensive sweep + counter (3-5 days)
  └─ Sprint 3:  IF mount-ns audit shows private namespace OR jailer is in use:
                  jailer chroot sweeper + counter (5-7 days)
                ELSE:
                  collapse rows to N/A in inventory ADR
  │
  └─ Cutover gate check: ≥4 of 5 OPEN surfaces CLOSED
       (3 audit closures + 1-2 code closures = 4-5 depending on Sprint 3 outcome)
```

**Severity**: MINOR — meta-planning. The order/dependency/parallelizability question is answered: audits are independent and parallel; code changes are independent of audits and of each other; the only ordering constraint is mount-ns-before-jailer if both are private/in-use.

**Recommendation**: post-stress-r8 GREEN, draft a sprint plan with the 5 surfaces fanned out across 3 parallel sprints. The cutover gate is reachable in ~1-2 weeks of focused work, not the "1 surface per week" linear pace the inventory ADR's risk-ordering implies.

---

## Cross-lens consensus

- **r27-A1 + r27-A4 (staging-locality ADR)**: CLOSED at `bbadbe68`. r28 carries forward: Phase 4 stress-r8 gates the default flip; r28-A1 (split-brain) must be resolved BEFORE stress-r8 for RCA tree clarity.
- **r27-A2 (6th-layer prediction)**: PARTIALLY VALIDATED. Stress-r5's RED at same 5% rate matched the predicted "r4-A reap-wait does not address dominant mechanism" outcome. The specific 6th-layer mechanism was within-the-`rootfs.img`-surface (OFD lock lifecycle past `wait4`) not the predicted Candidate 1 (SNAPSHOT-side reap race). Option C addresses both classes.
- **r27-A3 (abort criterion)**: SUBSUMED into staging-locality ADR Phase 4. r28 carries no new abort criterion — Phase 4's "stress < 30% → re-open design" rule is the inherited bound.
- **r27-A5 (kernel-state inventory rootfs.img row)**: STILL NOT UPDATED. The inventory ADR table at `:73-86` still reads "2 CLOSED + 5 OPEN + 4 N/A" — the rootfs.img file-lock surface is not enumerated. **Recommendation: roll this into the post-stress-r8 closure documentation pass** (the surface is OBVIATED by Option C, not closed by a patch; the inventory row should read CLOSED-BY-OPTION-C with cross-link to the staging-locality ADR).
- **r27-A6 (playbook ADR retros)**: STILL NOT WRITTEN. r3-A and r4-A retros remain owed. Recommend writing them as a single doc commit after stress-r8 GREEN, including a 6th retro for the Option C migration itself (the playbook's "force-multiplier evidence" rule is what triggered the structural pivot).
- **R28-DISCIPLINE (test-coverage r28)**: r28 architectural lens RECEIVES the audit's R1-DISC-3 finding as r28-A3 above. The architectural shape choice (`pg_stat_activity` oracle) is documented here as the right precedent for future predicate-test sites.
- **cluster review stress-r8**: gated on r28-A1 split-brain resolution. The cluster review's RCA tree template should include: (a) e2e rate; (b) home.img materialization path exercised (yes/no); (c) `host_dir_created` field state on every failure observation; (d) sweeper-reaper count by alleged-creator (controller vs driver, from rustdoc-updated logs).
- **code-quality r28** (if it runs): r28-A1's "HostDirOwnership enum" recommendation is a code-quality finding (free-form bool used as state flag). Cross-link.

---

## Lens hand-off (priority-ordered)

1. **r28-A1 (P0)**: resolve the three Phase 2 split-brain ambiguities BEFORE stress-r8. Sweeper rustdoc update + home.img materialization confirmation + `host_dir_created` typed enum. ~30 LOC total.
2. **r28-A3 (P1)**: implement R1-DISC-3 as a pg_stat_activity-oracle test in `sandbox_pg_e2e.rs`. ~80 LOC + a `compio-postgres` test-tunable `max_lifetime_secs` knob (~5 LOC upstream). Required because R26-C1 is the highest-leverage R28-DISCIPLINE gap.
3. **r28-A2 (P2)**: add `pg_connections_in_use_total{purpose=...}` controller metric sampled from `pg_stat_activity`. ~40 LOC. Defers to post-stress-r8; not blocking.
4. **r27-A5 + r27-A6 carries (P2)**: post-stress-r8 GREEN, document the rootfs.img file-lock surface in the inventory ADR as CLOSED-BY-OPTION-C; write r3-A + r4-A + Option C retros in the playbook ADR. ~doc-only.
5. **r28-A5 (P2)**: post-stress-r8 GREEN, draft the kernel-state surface audit sprint plan (3 parallel sprints). Sprint 1 audits can start NOW against stress-r6 logs; Sprints 2 + 3 await GREEN.
6. **r28-A4 (P3)**: no action — Phase 5 dispositions are correct. Document the analysis for future readers.

---

## Carry status

| Finding | r28 status |
| --- | --- |
| r27-A1 staging-locality ADR | **CLOSED at `bbadbe68`** — accepted Option C; Phase 2 capability landed |
| r27-A2 6th-layer prediction | PARTIALLY VALIDATED — r5 RED at same 5%; Option C addresses both predicted candidates and the within-surface OFD lock lifecycle |
| r27-A3 stress-r5 abort criterion | SUBSUMED into staging-locality ADR Phase 4 abort rule |
| r27-A4 Option-3 break-even | USED as decision basis in the ADR |
| r27-A5 kernel-state inventory rootfs.img row | STILL OPEN — recommend closing as CLOSED-BY-OPTION-C post-stress-r8 |
| r27-A6 playbook ADR retros | STILL NOT WRITTEN — bundle with Option C retro post-stress-r8 |
| r26-A1 BackendFailureDetail trait | DEFERRED — landing post-stress-r8 GREEN |
| r26-A6 fsync_dir doc-comment lie | OPEN (carry from r25-A3); no change at r28 |
| r25-A4 restore-debug-playbook ADR | CLOSED at `28fa64d1`; needs r27-A6 retros appended |
| r24-A2 kernel-state surface enumeration | CLOSED as ADR at `3e853cc6`; 2 of 7 surfaces closed; r28-A5 lays out parallel-sprint plan for remaining 5 |
| r7-A pg max_connections bump | CLOSED at `e3291b62` |
| r7-C-followup start_housekeeper | CLOSED at `8c0b361e` |
| Option C Phase 2 capability | LANDED at `6e928a25` + `2226de7a`; default flag=false; Phase 4 stress-r8 gates flip |
| r28-A1 Phase 2 split-brain resolution | **NEW (P0)** |
| r28-A2 pg-saturation surface audit | **NEW (P2)** |
| r28-A3 R1-DISC-3 architectural shape | **NEW (P1)** — pg_stat_activity oracle |
| r28-A4 Phase 5 deferred items safety analysis | **NEW (P3)** — analysis-only, no action |
| r28-A5 r24-A2 audit sprint plan | **NEW (P2)** — 3-parallel-sprint post-stress-r8 |

---

## Final note

Two months ago this codebase had a single architectural assumption ("controller stages locally, driver consumes locally") that five rounds of stress evidence refuted. One ADR + Phase 2 capability later, the assumption has been replaced by a typed contract — and the new shape carries its own assumptions that r28 must NAME before they accumulate evidence the same way.

r28's CRITICAL finding (split-brain in host_dir lifecycle ownership) is the *first such assumption to emerge*. It is small, scoped, and fixable in ~30 LOC pre-stress-r8. **The pattern matters more than the LOC.** Every architectural pivot has a window where the old assumption and the new assumption both partially-hold; the window must be closed before the cluster cycle interprets results through it.

The defense-in-depth from r7-A + r7-C-followup is *sufficient for the known wedge* — the structural fix (housekeeper) addresses the retention semantics; the cap bump (500) absorbs the steady-state floor. But the R28-DISCIPLINE audit's R1-DISC-3 finding shows the test posture is *zero coverage for the predicate that matters*. r28-A3 names the right architectural shape (pg_stat_activity oracle) so the next iteration of the pool semantics has a deterministic test gate.

The post-stress-r8 work plan is now tractable: r28-A1 unblocks RCA-tree clarity; r28-A5 lays out 3 parallel sprints to close the remaining kernel-state surfaces; r28-A3 closes the highest-leverage test-discipline gap. The cutover gate (≥4 of 5 surfaces CLOSED) is reachable in ~1-2 weeks of focused work, not the linear cadence the inventory ADR's risk-ordering implies.

Three decisions on the next reviewer's desk:

1. **Resolve r28-A1 split-brain (P0).** ~30 LOC. Pre-stress-r8. Without it, RED interpretation is ambiguous.
2. **Implement r28-A3 pg_stat_activity oracle (P1).** ~80 LOC + 5 LOC upstream. Catches the next R26-C1-class regression cheaply.
3. **Draft r28-A5 sprint plan (P2).** Post-stress-r8 GREEN. Closes the kernel-state inventory in parallel rather than linearly.

The architectural pivot is decided. The post-pivot ambiguities are now the load-bearing question.
