# Sandbox snapshot-restore architecture review — 2026-05-25 r27

**Reviewer**: architecture-r27 (post-T-8b-stress-r4 RED 3/60, r4-A driver v16 reap-wait IN FLIGHT for stress-r5)
**HEAD**: `01b6a744` + driver-v16 pin bump `1e8fa7e8`. Worktree: `.worktrees/sandbox-snapshot-restore` (READ-ONLY).
**Predecessor**: r26 at `326f4d4f`.
**Lens**: architecture (READ-ONLY). **NO changes proposed to `lib.rs`, `nomad_ch.rs`, `restore_handler.rs`, `wake_machine.rs` — r4-A in flight; r5 cluster validation pending.**

---

## Summary

Stress-r4 RED 3/60 (5.0% e2e). The diagnostic ladder is now **five layers deep** with one new commit landed (driver v16 reap-wait, `1e8fa7e8` controller + `e7ce7f1f`/`9af429c7` driver). All five cycles followed the exact arch r25-A4 playbook prediction: each cycle's dominant failure was reclassified by a verbatim observable from a layer the prior cycle misdiagnosed. **The playbook is 5-for-5 on force-multiplier evidence.**

But e2e moved 1.7% → 5.0% → ???. The CREATE/SNAPSHOT/WAKE/STOP rate at r4 (60/51/3/51 of 60) has all but one column at ≥85%; the wedge is concentrated on **WAKE 3/51 (5.9%)**. Per-worker rates are symmetric (1 e2e success per worker per 20 cycles, all on cycle 0) — strong evidence of a state-leak that accumulates monotonically post-first-success. The r4-A fix targets exactly that mechanism: CH process not reaped before Nomad GCs the alloc dir → next alloc inherits a Write-locked rootfs.img.

This review focuses on five questions the cycle leaves open:

1. **Playbook ADR update**: r4-A is the 5th force-multiplier; the ADR (`docs/decisions/2026-05-25-restore-debug-playbook.md` Section 5) is missing the entry. The r3-A entry from r26-A5 is also still unwritten.
2. **6th-layer prediction**: if stress-r5 also RED, what is the most likely next failure-class? Three candidates emerge from the diagnostic chain; r27-A2 ranks them.
3. **Cutover-plan reassessment**: five cycles in at $1.40 + 30 min per cycle. What is the abort criterion? r26-A4's 9-item gate splits into Tier-1 (3) + Tier-2 (6) but the Tier-1 functional gate (#1 stress GREEN) has slipped from "next cycle" to "unbounded." r27-A3 proposes a hard cycle-count bound.
4. **The structural alternative**: r3-A's Option 3 (driver-side staging) was deferred as "biggest rewrite." Five accrued patches (sweeper-owned host_dir, node-affinity Constraints, reap-wait, tap defensive cleanup, two-pass DriverFailure preference) all defend the same architectural assumption — controller stages locally, driver consumes. r27-A4 argues the patch-pyramid has reached the point where the structural rewrite is cheaper than continuing the layer-peel.
5. **Kernel-state inventory ADR update**: r4-A surfaces a new kernel-state surface (rootfs.img file lock — CH-internal `flock` via `ExclusiveWrite`) that the inventory ADR did not enumerate. r27-A5 adds the row; the count moves from 2-CLOSED + 5-OPEN to 2-CLOSED + 6-OPEN (or 3-CLOSED + 5-OPEN if r4-A lands GREEN).

Six findings (1 CRITICAL, 3 IMPORTANT, 2 MINOR). r26 carries: A1 (BackendFailureDetail trait) marked DEFERRED-below-threshold per code-quality r26; A2 (node-affinity-trade ADR) still NOT WRITTEN; A3 (kernel-state ADR deferred-alternatives section) still NOT WRITTEN; A4 (cutover gate Tier-1/Tier-2 split) NOT YET CODIFIED in the inventory ADR. Three of r26's four findings remain open; all four were P0/P1.

---

## CRITICAL

### [r27-A1] Five-layer diagnostic ladder is now a STRUCTURAL DIAGNOSTIC — the patch pyramid is defending the wrong invariant

Five rounds, five layers. Each layer was correct as a fix; the cumulative shape names a deeper problem.

| Round | Verbatim observable | Layer peeled | Fix shape | LOC | Patch class |
|---|---|---|---|---|---|
| r1 | `Tap zsbx-nm-X already exists` | Driver-side tap re-entry | Init-time tap pre-delete on EEXIST (`3d03cb90`) | ~10 | Defensive |
| r2 | "workspace.img does not exist" (49/60); 33% retry-win rate | Controller cleanup-vs-retry race | `CreateGuard::drop` no longer rm -rf host_dir; sweeper-owned (`d638b10f`+`e82bffd7`) | ~50 | Lifecycle redirect |
| r3 | 22% CREATE OK (≈1/N=3 random placement) | Cross-worker placement race | Constraints block pins alloc to staging node (`9b623f44`+`d71f1a8c`+`883df7fe`) | ~30 | Topology constraint |
| r3-B | TUNSETIFF EBUSY on retry | Kernel netdev release lag | Driver poll loop between `ip link del` and `ip tuntap add` (`05440498`) | ~20 | Defensive |
| r4 | `AlreadyLocked, lock_type: Write, path: ".../rootfs.img"` | Same-host CH process not reaped before next alloc | DestroyTask waits on supervisor `exitDone` chan (`e7ce7f1f`+`9af429c7`) | ~40 | Defensive |

**The pattern**: every fix is **either defensive** (catches a kernel-state surface another component owns) **or topology-constraining** (forces placement to maintain a controller-local assumption). None of them changes the architectural premise: **the controller stages locally, the driver consumes locally, and the alloc dir is the shared resource pool**.

The pyramid:

- `host_dir` (controller-staged) — `crates/sandbox/src/backend/nomad_ch.rs:736-760` creates `workspace.img` controller-side. Driver assumes it exists. r1-r2 peeled around the cleanup race; r3 peeled around the placement race.
- `rootfs.img` (driver-staged from a controller-emitted source path) — `restore_handler.rs:2348-2351` emits `rootfs_source`; driver hardlinks (commit `50fb987d`). The hardlink shares an inode → shares a `flock`. r4 peeled around the reap race.
- `taps` (driver-staged from an index allocated controller-side) — r1 + r3-B peeled around the kernel-netdev EEXIST.

Each row is a **shared mutable resource crossing two trust boundaries** (controller-fs → Nomad-fs, driver-process → CH-process, kernel-netdev → process-namespace). Each patch defends the boundary instead of removing it.

**The architectural diagnosis**: the **controller-stages-locally / driver-consumes-locally split** is the load-bearing assumption every patch defends. r3-A makes this explicit by pinning placement; r4-A makes it explicit by gating Nomad's alloc-GC on driver-side reap. The patches don't compose — each one adds a synchronization point between two components that were designed to be independent.

**The forward path**: this isn't a "next-cycle" fix. It's the call to choose between:

- **A) Keep peeling layers** — accept N more cycles of layer-by-layer defense. Cost: $1.40 × N + 30 min × N + technical debt at every defensive site. Estimated remaining surfaces from r27-A2: 3 (concurrent SNAPSHOT/STOP race, SNAPSHOT-side 9 failures' mechanism, controller crash mid-staging recovery).
- **B) Move staging into the driver** (the Option 3 from r26-A3) — controller emits a typed `StagingManifest`; driver materializes `workspace.img`, `home.img`, `rootfs.img`, and registers vm_index/tap in `StartTask`. Eliminates: cross-worker placement (no longer matters where the alloc lands), host_dir leak class (per-alloc not per-sandbox), node-affinity Constraints. Cost: redesign the staging contract + a controller→driver-blob-fetch path.
- **C) Move staging to shared storage** (the Option 2 from r26-A3) — `host_dir` lives on NFS/Ceph/GCS-FUSE. Eliminates cross-worker placement. Latency tax on every CREATE.

**Severity**: CRITICAL. The diagnostic is structural. Five cycles of evidence converge on the same architectural seam. Continuing to peel layers without naming the seam means cycle 6 + cycle 7 + ... all defend the same assumption, and the cutover gate stays bounded by the next-cycle-bug-discovery loop forever.

**Recommendation**: write the ADR that names the assumption AND the three options. The decision can wait for stress-r5's outcome; the ADR cannot. If r5 is GREEN, the ADR explains why the patches converged (and provides cover for future regressions to land at known boundaries); if r5 is RED, the ADR is the basis for the abort-and-restructure decision.

**Fix shape**: `docs/decisions/2026-05-25-staging-locality.md` ADR draft. Sections: Context (the five-layer ladder), Decision deferred (cite stress-r5 as the trigger), Options A/B/C with quantitative trade-offs (latency, cycle-count, surface-collapse table). Land alongside or before r5.

---

## IMPORTANT

### [r27-A2] 6th-layer prediction: three candidates from the diagnostic chain, ranked by likelihood

If stress-r5 also RED, what does the 6th-layer verbatim look like? Per the playbook's rule "the next bug will be reclassified by a verbatim observable from a layer we currently misdiagnose," we should predict the candidate set BEFORE running r5 so we know what to instrument.

Three candidates:

**Candidate 1 — SNAPSHOT-side state corruption (9 SNAPSHOT failures at r4 unexplained, P ≈ 50%)**

Stress-r4's SNAPSHOT phase saw 9/60 failures (8× HTTP 500 + 1× HTTP 404). The cluster review at `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r4.md:115` flags this as "not investigated this cycle." Per `snapshot_handler.rs:220-323` the failure surface set includes:

- `db.update_sandbox_status(... Snapshotting ...)` CAS rejection — could indicate concurrent stress harness retry or controller-side state inconsistency.
- `ch.pause()` failure — `ch-remote pause` HTTP error on the api-socket. If the CH process is mid-restart from a prior failed wake, the api-socket may be down or stale.
- `ch.snapshot()` failure (excluding the documented CH v51.1 post-snapshot-VMM-down quirk handled at `snapshot_handler.rs:545-565`) — could indicate disk-full, virtiofsd stuck, or vCPU hang during pause.
- `store.put()` failure — disk-full at temp_dir / L1 path.
- `update_snapshot_metadata` CAS — peer takeover (lease-takeover race).
- `vm_ops.teardown_source` failure — best-effort, logged not bubbled (`snapshot_handler.rs:424-430`). **But teardown_source is the mechanism that would prevent the r4-A rootfs.img lock leak from arising in the first place**. If teardown_source returns Ok but the CH process is NOT actually reaped (just `kill`'d), the SNAPSHOT phase has the SAME unreaped-CH-process problem r4-A diagnosed on WAKE. The 9 SNAPSHOT failures may be the wake-side bug surfacing on the snapshot side.

**Cross-link**: `Backend::teardown_source_for_snapshot` calls `stop_preserving_state` (`nomad_ch.rs:1024-1029`) → `stop_inner(remove_host_dir=false)`. `stop_inner` does the Nomad job purge but **does NOT wait for the driver's `DestroyTask` to return reaped state to Nomad**. So the controller-side `stop_preserving_state` returns Ok before the CH process is reaped on the worker.

**Predicted verbatim**: SNAPSHOT 500 with controller-log "registry: api_socket not found" or "ch-remote pause: connection refused" — the api-socket from the prior alloc is gone but the next snapshot tries to address it on a stale registry entry.

**Likelihood: HIGH**. Same root cause as r4-A but on the snapshot path. r4-A's reap-wait fix in DestroyTask helps the next-alloc-on-this-task case but does NOT help the controller-issues-stop-then-immediately-tries-another-API-call case.

**Candidate 2 — Wake-path file system race: rootfs.img + workspace.img conflicting lock modes (P ≈ 25%)**

`workspace.img` is **per-sandbox** under `host_dir/workspace.img` (`nomad_ch.rs:762`). `rootfs.img` is **per-alloc** under `runDir/rootfs.img` (hardlinked from `runtime_dir/rootfs-slim.img`). Both opened by CH virtio-blk.

Pre-r4-A: r4-A specifically diagnoses `rootfs.img` lock. But `workspace.img` is also opened by CH in some mode (`ReadOnly` vs `ExclusiveWrite` — depends on the snapshot's config.json). If the wake alloc opens `workspace.img` as ExclusiveWrite, and the per-sandbox `workspace.img` was opened by the snapshot-alloc as ExclusiveWrite, and that snapshot-alloc's CH process is unreaped (Candidate 1's case), then:

- r4-A's reap-wait closes the rootfs.img collision (per-alloc + per-alloc same path).
- But the workspace.img collision (per-sandbox path across snapshot-alloc → wake-alloc) is **a different shape** — different path, different alloc dir, different CH process.

The r4-A diagnostic chain assumed rootfs.img is the canary; r5 may show workspace.img is the canary AFTER rootfs.img is fixed.

**Predicted verbatim**: same `AlreadyLocked, Write` but with `path: "/var/zeroship/ch/<sid>/workspace.img"` (host_dir-prefixed) instead of `path: "<alloc_dir>/ch/local/rootfs.img"` (alloc_dir-prefixed).

**Likelihood: MEDIUM**. Depends on whether CH opens workspace.img in `ExclusiveWrite` or `ReadWrite-NoLock` mode. Worth instrumenting before r5: a verbatim CH stderr capture on workspace.img IO from the snapshot-alloc would falsify this candidate cheaply.

**Candidate 3 — Concurrent SNAPSHOT/STOP race (P ≈ 15%)**

The stress harness does sequential CREATE → SNAPSHOT → WAKE → STOP per sandbox; multiple sandboxes run concurrently per worker. STOP at cycle N for sandbox A could race SNAPSHOT at cycle M for sandbox B if they share an api-socket directory, a vm_index, or a tap interface. Per `nomad_ch.rs:1024` STOP returns 200 quickly (51/51 in 19 ms p50 at r4) — too fast to have waited for any of these.

The 1-success-per-worker-on-cycle-0 pattern suggests monotonically-building state, not random concurrent races. So this candidate is **less likely** than candidates 1 + 2 — but if r4-A fixes the dominant mechanism, this becomes the next visible layer.

**Predicted verbatim**: cross-sandbox vm_index reuse (sandbox A's STOP doesn't release index, sandbox B's CREATE grabs it but A's tap is still up), or cross-sandbox host_dir collision (sandbox A's STOP leaves workspace.img open, sandbox B's CREATE on the same vm_index inherits a non-clean host_dir).

**Likelihood: LOW**. The per-worker symmetry (each worker: 1 e2e OK at cycle 0) argues against worker-shared state being the dominant mechanism. But not zero — a vm_index leak that monotonically depletes the per-worker pool would produce exactly this shape.

**Recommendation**: BEFORE running stress-r5, **add observability for Candidate 1**. Specifically: instrument `stop_preserving_state` to log the driver-side DestroyTask completion confirmation (or its absence) and `vm_ops.teardown_source` to surface the reap-wait counter (`nomad_driver_ch_destroy_task_unreaped_total` per `1e8fa7e8`) on every SNAPSHOT cycle. If r5 SNAPSHOT failures correlate with non-zero unreaped count, Candidate 1 is confirmed without needing a 6th cluster cycle to peel it.

**Severity**: IMPORTANT. The 6th-layer prediction is testable AHEAD of the cycle. The playbook's observability-before-architecture rule (`docs/decisions/2026-05-25-restore-debug-playbook.md:62-74`) explicitly says verbatim capture comes BEFORE the fix at layer N+1. Pre-instrumenting r5 means r5 either GREENs (no 6th layer needed) or surfaces the layer with the verbatim observable already attached — saving the 30-min/$1.40 cycle that would otherwise just be "capture observability."

### [r27-A3] Cutover-plan abort criterion: a hard cycle-count bound is overdue

Five cycles in, $7 + 2.5 hr cumulative. The 9-item cutover gate from r26-A4 had #1 (stress GREEN ≥95%) as Tier-1; we are at 5%, 8.5% gap from threshold. The trajectory:

| Cycle | E2E rate | Delta | Forecast (linear) | Forecast (compound) |
|---|---|---|---|---|
| r1 | 3.3% | — | — | — |
| r2 | 3.3% | +0.0% | — | — |
| r3 | 1.7% | -1.6% | regression | regression |
| r4 | 5.0% | +3.3% | recovery | recovery |
| r5 | TBD | TBD | ~8% | ~12-25% (if r4-A multiplicative) |

Linear extrapolation has r5 at ~8% — far from 95%. Compound (if r4-A multiplicatively boosts the conditional rate) could put r5 at 25-50%. Either way, the prediction is **NOT 95%**, and the 95% Tier-1 threshold may require 2-5 more cycles even on the optimistic compound trajectory.

**The decision r26-A4 left unfinished**: what is the abort criterion? r26-A4 split the gate into Tier-1/Tier-2 but didn't bound the Tier-1 budget. Without a bound, the pilot loop can run indefinitely, peeling layers, racking up cycles, never deciding when to step back.

**Proposed abort criterion**:

| Trigger | Action |
|---|---|
| Stress-r5 ≥70% e2e | Continue layer-peel; cycle remaining surfaces budget |
| Stress-r5 ≥30% and <70% | Continue ONE more cycle ONLY if 6th-layer verbatim was captured this cycle (per r27-A2 observability-first rule). Otherwise pause + decide. |
| Stress-r5 ≥10% and <30% | PAUSE. Write the staging-locality ADR (per r27-A1). Decide A/B/C before r6. |
| Stress-r5 <10% (no improvement from r4) | **ABORT layer-peel.** r4-A's reap-wait did not address the dominant mechanism either; the 5-for-5 playbook is not enough. Direct path to Option B (driver-side staging) or Option C (shared storage) without another cycle. |

The numerical breakpoints come from the observed cycle costs and the patch convergence rate. r4 → 5% from r3's 1.7% is a 3.3 pp absolute boost. If r5 produces a similar absolute boost (r5 at ~8.3%), the residual gap is ~87 pp and N≈26 cycles at this rate. That's $36 + 13 hr; well past the structural rewrite's break-even.

**Why the bound is structural, not arbitrary**: at $1.40/cycle the dollar cost is trivial. The expensive resource is the cycle's 30-min wall + the architectural attention required to interpret each cycle's verbatim. The five cycles to date have each demanded careful diagnosis + a structural review post-mortem + an ADR check; the marginal cost is engineer-hours not cluster-time. A bound on cycles is a bound on engineer attention budget.

**Severity**: IMPORTANT. Without a bound, sunk-cost reasoning extends the pipeline indefinitely. The pilot mode's "trust-but-verify" reviewing each round (per `feedback_review_each_round.md`) means each cycle takes 1+ hr of review + ADR + commit work; five cycles = 5 hr of attention before getting to the structural decision the evidence has been pointing at since r3.

**Fix shape**: append "Abort criterion" section to either the playbook ADR or the new staging-locality ADR (per r27-A1). Codify the trigger table above. The cluster review for stress-r5 then has a deterministic decision rule, not a judgement call.

### [r27-A4] The structural alternative: Option 3 (driver-side staging) is no longer the "biggest rewrite" — five patches have already paid the cost

r3-A's Option 3 was deferred as "biggest rewrite": moving `create_ext4_image_if_missing` into the driver's `StartTask`. But the patches landed since stress-r1 — host_dir-leak-then-sweeper (r2), node-affinity Constraints (r3), tap defensive cleanup (r1+r3-B), reap-wait (r4), two-pass DriverFailure preference (r3-C) — are EACH partial steps toward Option 3 without the structural benefit.

The accumulated cost of the layer-peel:

| Patch | LOC | Surface defended | Would survive Option 3? |
|---|---|---|---|
| Host_dir leak + sweeper (v34) | ~120 | host_dir leak class | NO — under Option 3, host_dir is per-alloc, leaked-on-stop pattern unnecessary |
| Node-affinity Constraints | ~30 | Cross-worker placement | NO — under Option 3, placement doesn't matter |
| Tap defensive cleanup (v14) | ~80 | Stranded taps | YES — tap is driver-owned in both worlds |
| Reap-wait (v16) | ~40 | rootfs.img lock | PARTIAL — reap discipline matters under Option 3 too, but the lock surface itself goes away if rootfs.img is freshly hardlinked per alloc |
| Two-pass DriverFailure preference | ~20 | Verbatim msg propagation | YES — observability layer |

~290 LOC of layer-peel; ~180 LOC of which is **defending an assumption Option 3 makes obsolete**. The Option 3 redesign itself is `StagingManifest` (~80 LOC of typed-wire schema) + driver-side staging in `StartTask` (~120 LOC moving create_ext4_image_if_missing into the driver) + controller emission (~30 LOC removing the local-fs stage). Net: ~230 LOC of structural code that REPLACES the ~180 LOC of defensive code AND eliminates the four future-layer-peel candidates (Candidate 1, 2, 3 from r27-A2 + whichever shows up at layer 7).

**The break-even has already been passed.** Five cycles + 2.5 hr of review + four ADR drafts + one explicit Option 3 deferral. The break-even was somewhere around cycle 3.

**Why Option 3 was deferred**: r3-A's reasoning at the time was "biggest rewrite, depends on r24-A1 schema half completing." The r24-A1 schema half DID complete at `022f778a` (R25-S1 typed staging). The "biggest rewrite" framing was correct at cycle 3 BEFORE the schema half landed; at cycle 5 after it landed, Option 3 is the **smallest** path to closing the layer-peel.

**Comparison to Option 2 (shared storage)**:

| Dimension | Option 2 (shared storage) | Option 3 (driver-side staging) |
|---|---|---|
| Latency tax | +30-60 s on CREATE (NFS write of workspace.img) | +5-10 s on CREATE (blob fetch from controller-emitted URL) |
| Surface-collapse | host_dir + mount-ns + jailer-chroot → uniform across workers | host_dir + workspace.img + rootfs.img → per-alloc, no cross-alloc leak |
| Node-affinity | Eliminated | Eliminated |
| Controller-as-twin | Eliminated (controller pool restored) | Eliminated (controller pool restored) |
| Code surface | shared-storage backend trait + LocalFS/NFS impl | StagingManifest typed wire + driver-side materialize |
| Operational complexity | NEW: shared-storage SPOF, NFS tuning, mount-failure semantics | NEW: blob-fetch path on hot create, content-addressing for cache-hits |
| Failure mode if degraded | NFS down → CREATE blocks (latency unbounded) | Blob fetch fails → CREATE returns explicit error from staging contract |

Option 3 has a clearer failure model (the staging contract is enforced at the typed boundary; degradation surfaces as a wire-typed error). Option 2 has hidden failure modes (NFS hangs, mount stalls) that don't compose with the existing observability stack.

**Severity**: IMPORTANT. r27-A1 + r27-A4 together argue for writing the staging-locality ADR (Section A/B/C) NOW, even if the decision waits for stress-r5. The ADR's existence anchors the next-cycle decision; its absence means the next cycle's pilot loop spins on whether to peel or rewrite without a reference.

**Fix shape**: `docs/decisions/2026-05-25-staging-locality.md` with three sections: A (continue layer-peel), B (driver-side staging), C (shared storage). Quantitative comparison table. Defer the decision but write the analysis. Land before or alongside stress-r5.

---

## MINOR

### [r27-A5] Kernel-state inventory ADR needs the rootfs.img file-lock surface — 6th OPEN surface

The inventory ADR (`docs/decisions/2026-05-25-kernel-state-surface-inventory.md:73-86`) enumerates 7 surfaces (2 CLOSED + 5 OPEN + 4 N/A). The r4-A diagnosis surfaces a NEW kernel-state surface:

| Surface | Where created | Where cleared today | EEXIST-safe re-entry? | Sweeper-replicable? | Status |
| --- | --- | --- | --- | --- | --- |
| **rootfs.img file lock** (`<alloc_dir>/ch/local/rootfs.img` opened ExclusiveWrite by CH virtio-blk) | CH process opens on VM spawn (driver `StartTask`) | CH process exit (kernel `exit_files()` releases all fcntl locks) | YES (post-r4-A: DestroyTask waits for supervisor `exitDone` → kernel reap → lock release) | NO (kernel-internal lock surface; no FS enumeration) | **OPEN — r4-A fix in flight (driver v16, GCS sha `0e153a6f...`); stress-r5 validates** |

This is the surface the r4-A fix targets. Adding it to the inventory updates the count:

- If r4-A lands GREEN at stress-r5: **3 CLOSED + 5 OPEN** (rootfs.img file lock joins tap + host_dir).
- If r4-A lands RED at stress-r5: **2 CLOSED + 6 OPEN** (new surface added but not closed).

Either way the inventory ADR is no longer accurate at "2 + 5 + 4 = 11 surfaces." The actual count is 12 (with rootfs.img file lock added).

**Related**: the inventory ADR's cutover gate at section "Cutover gate" requires "≥4 of the 5 OPEN surfaces reach CLOSED." That gate IS NOW WRONG — it should read "≥5 of 6 OPEN surfaces" (assuming r4-A is the first closure). Or alternatively, "rootfs.img file lock" doesn't count toward the gate denominator if it's added AND closed in the same cycle. The math under either reading depends on whether the inventory was a static-at-r25 snapshot or a living document.

**Severity**: MINOR — documentation. The fix is a 1-row update to the inventory table + one-line gate-denominator adjustment. But it matters because the next reader needs to know the surface exists and where to look in the codebase.

**Fix shape**: edit `docs/decisions/2026-05-25-kernel-state-surface-inventory.md` table at lines 73-86 to add the rootfs.img file lock row. Update the cutover gate denominator. Cross-link to driver v16's `nomad_driver_ch_destroy_task_unreaped_total` counter as the orphan-counter analogue.

### [r27-A6] Playbook ADR is missing two force-multiplier retros — r3-A AND r4-A

r26-A5 flagged that r3-A's retro was missing from the playbook ADR. As of `01b6a744`, that ADR's section 5 ("Force-multiplier retrospectives") still has only three retros (r22-A2, r23-A2, r24-A1). r4-A's reap-wait diagnosis is now ALSO missing.

The playbook ADR's value depends on the retro section being complete. Three retros + observability-before-architecture rule + stress-as-gate rule = a checklist a future engineer can use to interpret a new failure mode. Five retros + the same rules = a corpus large enough to recognize a pattern (the layer-peel pattern itself, per r27-A1).

The r3-A retro is the 4th force-multiplier:
- Pre-fix: cross-worker placement was misclassified as "controller staging is broken" (stress-r2's 33% retry-win analysis pointed at cleanup race; the cleanup race was real but secondary).
- Decisive observable: **22% CREATE OK at WORKER_COUNT=3** (1/N=3 random scheduler hit-rate ≈ 33%, observed 22% means staging-host is NOT being preferred).
- Fix: Constraints block + node_id caching.
- Lesson: rate signals (the 22%) refuted the prior hypothesis (cleanup race) and surfaced the cross-worker scheduling assumption.

The r4-A retro is the 5th:
- Pre-fix: post-r3-A's smoke 1/1 + CREATE 60/60 at stress-r4 made it appear placement-pinning was sufficient. WAKE 3/51 (5.9%) revealed a different layer.
- Decisive observable: **CH stderr `AlreadyLocked, Write, path: ".../rootfs.img"` after the warning `Tap zsbx-nm-2 already exists`** (the tap warning is now suppressed by r3-B; the rootfs.img lock is the new fatal layer below it).
- Fix: DestroyTask `exitDone`-wait in driver v16.
- Lesson: each layer's fix surfaces the next layer's verbatim. The reap-wait wasn't needed under non-affinitized placement (cross-worker placement made rootfs.img collision unlikely); r3-A's correctness CREATED the conditions for r4-A's bug to dominate.

**Severity**: MINOR — documentation. Two retros to add. The lessons are already implicit in the cluster reviews; the ADR consolidates them.

**Fix shape**: append to `docs/decisions/2026-05-25-restore-debug-playbook.md` section 5: "r3-A retro" and "r4-A retro" subsections following the existing r22-A2 / r23-A2 / r24-A1 shape. Update section 2 (triage tier ladder) per r26-A5's note: add Tier 0 (Placement) above Validator, OR rename Tier 1 to "Submit/Placement" to cover the post-validator scheduler decision.

---

## 6th-layer prediction summary (r27-A2 distilled)

| Candidate | Mechanism | Predicted verbatim | Likelihood | Pre-r5 instrumentation |
|---|---|---|---|---|
| 1 | SNAPSHOT teardown_source returns Ok but CH not reaped | "api_socket not found" or "ch-remote pause: connection refused" on next SNAPSHOT | HIGH (~50%) | Log driver-side DestroyTask completion confirmation + unreaped counter delta per SNAPSHOT cycle |
| 2 | Workspace.img cross-alloc lock collision (analog of rootfs.img but per-sandbox path) | `AlreadyLocked, Write, path: "/var/zeroship/ch/.../workspace.img"` | MEDIUM (~25%) | CH stderr capture on workspace.img IO mode (`ReadOnly` vs `ExclusiveWrite`) |
| 3 | Cross-sandbox vm_index reuse / host_dir collision | Vm_index reuse with stale tap; or host_dir not freshly clean on second CREATE per vm_index | LOW (~15%) | Per-cycle vm_index reuse log + tap orphan count per cycle |

**Pre-r5 work to maximize information per cycle**: instrument Candidate 1 first. The instrumentation is small (~10 LOC of structured tracing) and catches the highest-likelihood candidate without needing another cycle to capture it.

---

## Patch-convergence diagram

```
ROOT ASSUMPTION (load-bearing):
  "Controller stages locally, driver consumes locally, alloc dir is shared resource pool"
  │
  ├─ r1: tap EEXIST (driver-side init-time pre-delete)             ──── defensive
  │
  ├─ r2: host_dir cleanup-vs-retry race                            ──── lifecycle redirect
  │      (controller no longer rm -rf in CreateGuard::drop)
  │
  ├─ r3-A: cross-worker placement                                   ──── topology constraint
  │      (Constraints block pins to staging worker)
  │
  ├─ r3-B: TUNSETIFF EBUSY (kernel netdev release lag)              ──── defensive
  │
  ├─ r3-C: verbatim DriverFailure msg preference                    ──── observability
  │
  ├─ r4-A: rootfs.img Write lock (CH not reaped)                    ──── defensive
  │      (DestroyTask waits for supervisor exitDone)
  │
  └─ r5: ???
        ├─ Cand 1 (HIGH): SNAPSHOT-side reap (mirror of r4-A)       ──── defensive (predicted)
        ├─ Cand 2 (MED):  workspace.img lock (analog of rootfs.img) ──── defensive (predicted)
        └─ Cand 3 (LOW):  cross-sandbox vm_index reuse              ──── defensive (predicted)
```

**Visual observation**: every branch is `defensive` or `lifecycle redirect` or `topology constraint`. **No branch attacks the root assumption.** That's the diagnostic — the patches are all leaves; the tree has no fix at the root.

Compare to the Option-3 (driver-side staging) world: the tree collapses to a single node (the staging contract is the assumption; nothing below it can leak because the assumption is enforced by the typed wire).

---

## Cross-lens consensus

- **r26-A1 (BackendFailureDetail trait)**: code-quality r26 flagged DEFERRED-below-threshold; r27 reaffirms — the trait pattern is correct but the layer-peel pyramid is the bigger structural issue. Land the trait alongside or after the staging-locality decision; don't block r5 on it.
- **r26-A2 (node-affinity-trade ADR)**: NOT WRITTEN as of `01b6a744`. r27-A1 + r27-A4 jointly propose folding this into `docs/decisions/2026-05-25-staging-locality.md` instead of a standalone "node-affinity-trade" ADR. The trade is a CONSEQUENCE of choosing Option A; the choice between A/B/C is the more fundamental decision.
- **r26-A3 (deferred-alternatives section)**: r27 absorbs this. The deferred-alternatives section IS the staging-locality ADR. Don't add a section to the kernel-state inventory; write the staging-locality ADR and have the inventory cite it.
- **r26-A4 (Tier-1/Tier-2 cutover gate split)**: r27-A3 adds the abort criterion. Tier-1's #1 (stress GREEN ≥95%) is now bounded by the abort criterion's trigger table. Tier-2 items remain as documented.
- **stress-r5 cluster review**: gated on r4-A landing. The cluster review template should include: (a) e2e rate; (b) abort-criterion trigger row applied; (c) Candidate 1/2/3 verbatim presence; (d) decision recorded (peel further / pause + write ADR / abort to Option B-C).
- **code-quality r27**: ~290 LOC of layer-peel vs ~230 LOC of structural rewrite is a code-quality finding (defending a wrong invariant is worse than the rewrite's marginal complexity). Cross-link r27-A4 from code-quality lens.
- **concurrency r27**: r27-A2 Candidate 1 (SNAPSHOT-side reap race) is concurrency-lens territory; the controller→stop_preserving_state→driver_DestroyTask coordination is the concurrency contract that's not enforced. Cross-link.

---

## Lens hand-off (priority-ordered)

1. **r27-A1 (P0)**: write the staging-locality ADR. Three sections (A/B/C). Quantitative comparison table. Defer the decision to post-r5; the ADR exists by r5. Cross-link from playbook ADR + kernel-state inventory ADR.
2. **r27-A3 (P0)**: codify the stress-r5 abort criterion in the staging-locality ADR (or as a section of the playbook ADR). Trigger table at concrete rate thresholds (≥70/≥30/≥10/<10%).
3. **r27-A2 Candidate 1 instrumentation (P1)**: BEFORE running stress-r5, add observability for the SNAPSHOT-side reap race. Log driver-side DestroyTask completion confirmation + unreaped counter delta per SNAPSHOT cycle. ~10 LOC of structured tracing in `snapshot_handler.rs:do_snapshot_inner` step 7 + `nomad_ch.rs:stop_preserving_state`.
4. **r27-A5 (P1)**: 1-row update to kernel-state inventory ADR for the rootfs.img file-lock surface. Cutover gate denominator adjustment.
5. **r27-A6 (P2)**: append r3-A + r4-A retros to playbook ADR section 5. Add Tier-0 to triage tier ladder.
6. **r26-A1 (P3, carried)**: BackendFailureDetail trait — defer until staging-locality decision lands.
7. **r26 carries (P2-P3)**: A2/A3/A4 all subsumed into r27-A1's staging-locality ADR + r27-A3's abort criterion.

---

## Carry status

| Finding | r27 status |
|---|---|
| r26-A1 BackendFailureDetail trait | DEFERRED-below-threshold (code-quality r26); r27 reaffirms defer until staging-locality lands |
| r26-A2 node-affinity-trade ADR | SUBSUMED into r27-A1 staging-locality ADR (option-A consequence) |
| r26-A3 kernel-state deferred-alternatives | SUBSUMED into r27-A1 |
| r26-A4 cutover gate Tier-1/Tier-2 split | EXTENDED by r27-A3 abort criterion |
| r26-A5 playbook ADR r3-A retro + Tier-0 | EXTENDED by r27-A6 (also add r4-A retro) |
| r26-A6 fsync_dir doc-comment lie | OPEN (carry from r25-A3); no change at r27 |
| r25-A4 restore-debug-playbook ADR | CLOSED at `28fa64d1`; needs r27-A6 retros appended |
| r25-A2 sweeper-pattern ADR | CLOSED at `3e853cc6`; needs r27-A5 row update |
| r24-A1 typed StagingManifest schema | CLOSED for preflight at `022f778a`; reusable for Option 3 per r27-A4 |
| r24-A2 kernel-state surface enumeration | CLOSED as ADR at `3e853cc6`; 2 of 7 surfaces closed; r27-A5 adds the 12th surface (rootfs.img file lock); 3 of 12 candidate-closed if r4-A holds |
| r3-A node-affinity | LANDED at `883df7fe`+`9b623f44`+`d71f1a8c`; stress-r4 validation BLOCKED — fix correct but new layer dominates |
| r3-B driver tap-poll | LANDED at `05440498`; stress-r4 confirms working (`Tap exists` is WARN not ERROR) |
| r3-C two-pass DriverFailure preference | LANDED at `3d431eb8`; stress-r4 confirms working (verbatim msg surfaces in TaskEvent) |
| r4-A driver reap-wait | IN FLIGHT (driver v16 at `1e8fa7e8`, driver commits `e7ce7f1f`+`9af429c7`); stress-r5 validation pending |
| r27-A1 staging-locality ADR | **NEW (P0)** |
| r27-A2 6th-layer prediction + instrumentation | **NEW (P1)** |
| r27-A3 stress-r5 abort criterion | **NEW (P0)** |
| r27-A4 Option-3 break-even argument | **NEW (P1)** — argument feeds A1's ADR |
| r27-A5 kernel-state inventory rootfs.img row | **NEW (P1)** |
| r27-A6 playbook ADR r3-A + r4-A retros | **NEW (P2)** |

---

## Final note

The r4-A reap-wait fix is the **fifth force-multiplier** in a continuous chain. The arch r25-A4 playbook ADR predicted "the next bug will be reclassified by a verbatim observable from a layer we currently misdiagnose." Five rounds confirmed the prediction. The playbook works.

But the playbook is now showing a SECOND-ORDER signal: **every layer-peel patch defends the same architectural assumption** (controller stages locally / driver consumes locally). Five rounds of evidence converge on one structural diagnosis. The patches no longer COMPOSE — each one adds a synchronization point between two components that were designed to be independent.

r27 leaves three decisions on the next reviewer's desk:

1. **Write the staging-locality ADR (r27-A1).** Three options (A/B/C); defer the decision to post-stress-r5. The ADR's existence anchors the next decision.
2. **Pre-instrument stress-r5 for Candidate 1 (r27-A2).** ~10 LOC of structured tracing. Maximizes information per cycle.
3. **Codify the abort criterion (r27-A3).** Trigger table at concrete rate thresholds. Bounds the engineer-attention budget on the cycle loop.

If stress-r5 GREEN at ≥95%: r4-A was the load-bearing fix, the layer-peel converged at cycle 5, and we cut over with the staging-locality ADR as the post-mortem analysis (Option A chosen by demonstration, alternatives documented for future regression cover).

If stress-r5 GREEN at 30-95%: r4-A was load-bearing but Candidate 1/2/3 dominates; apply abort criterion; decide whether one more cycle is worth it.

If stress-r5 RED <30%: structural rewrite. Option B or Option C from the staging-locality ADR. Five cycles of layer-peel were insufficient; the architectural seam needs closure.

The cycle budget is bounded. The architectural attention is finite. The next decision is the structural one, regardless of which row of the abort criterion fires.

The playbook is 5-for-5 on force-multiplier evidence. The patch pyramid is 5-for-5 on defending the same assumption. Both signals say the same thing.
