# ADR — Staging Locality: Driver-Side Staging Replaces the Controller-Stages-Locally Invariant

- **Date:** 2026-05-25
- **Status:** Accepted
- **References:**
  - Architecture reviews `r26-A2` (post-r3-A topology cascade), `r27-A1` (CRITICAL — five-layer ladder as a structural diagnostic), `r27-A4` (Option-3 break-even argument) under
    `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r{26,27}.md`
  - Cluster reviews `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r{1,2,3,4,5}.md`
  - Sibling ADRs:
    - `docs/decisions/2026-05-25-restore-debug-playbook.md` (observability-before-architecture, stress-as-gate)
    - `docs/decisions/2026-05-25-kernel-state-surface-inventory.md` (per-surface sweeper pattern; closure gate)
  - Closing commits the layer-peel landed at:
    - `3d03cb90` — driver v13, init-time tap pre-delete on EEXIST (r1)
    - `d638b10f` + `e82bffd7` — controller v34, leak host_dir on stop + GC sweeper (r2)
    - `177ff165` — driver v14, defensive vm_index-keyed tap cleanup in DestroyTask (r2 follow-up)
    - `883df7fe` + `9b623f44` + `d71f1a8c` — r3-A node-affinity Constraints (r3)
    - `05440498` — driver v15, netdev release poll between `ip link del` and tuntap-add (r3-B)
    - `e7ce7f1f` + `9af429c7` + `1e8fa7e8` — driver v16, DestroyTask wait-for-CH-reap + `nomad_driver_ch_destroy_task_unreaped_total` counter (r4-A)
  - Prerequisite already landed: `022f778a` (typed `StagingPathMissing` from R25-I1 / R25-I2 / R25-S1 — the wire-schema half this ADR builds on)

## Status

**ACCEPTED.** Decision: **Option C** (driver-side staging — move
`create_ext4_image_if_missing` and `materialize_rootfs` into the driver's
`StartTask`). Migration sequenced over five phases below; phase 1
(this ADR) is the gate that decisions phases 2–5 reference.

## Context

Between 2026-05-23 and 2026-05-25 the T-8b cluster stress harness ran
five 60-cycle stress cycles. All five went RED at the same noise-floor
end-to-end success rate, and each successive cycle peeled exactly one
defect layer beneath the prior cycle's fix:

| Cycle | E2E OK | Verbatim layer-N observable | Defect peeled | Fix that landed | Commits |
| ---:| ---:| --- | --- | --- | --- |
| r1 | 2/60 (3.3%) | `Tap zsbx-nm-<idx> already exists` | Driver tap re-entry on EEXIST | Driver v13 pre-delete-on-collision | `3d03cb90` |
| r2 | 2/60 (3.3%) | `workspace.img does not exist (controller must stage before spawn)` plus a 33% retry-win rate (96/144) | Controller `CreateGuard::drop` rm -rf raced concurrent retry's `StartTask` | Controller v34: leak host_dir on stop, GC sweeper, verbatim driver-msg propagation | `d638b10f` + `e82bffd7` (+ driver v14 defensive `177ff165`) |
| r3 | 1/60 (1.7%) | 22% CREATE OK at WORKER_COUNT=3 (≈ 1/N=3 random-scheduler hit-rate) | Cross-worker placement: controller staged on local fs, alloc landed on a different worker | r3-A node-affinity `Constraints` block (cold-boot + restore-path parity) | `883df7fe` + `9b623f44` + `d71f1a8c` |
| r3-B | (within r3) | `TUNSETIFF EBUSY` on retry post-`ip link del` | Kernel netdev release lag between delete and re-create | Driver v15: poll loop between `ip link del` and `ip tuntap add` | `05440498` |
| r4 | 3/60 (5.0%) | `AlreadyLocked, lock_type: Write, path: "<alloc_dir>/ch/local/rootfs.img"` | Same-host CH process not reaped before next alloc opens `rootfs.img` ExclusiveWrite | r4-A driver v16: `DestroyTask` waits on supervisor `exitDone` channel + `nomad_driver_ch_destroy_task_unreaped_total` counter | `e7ce7f1f` + `9af429c7` (+ pin `1e8fa7e8`) |
| r5 | 3/60 (5.0%) | Same `rootfs.img AlreadyLocked, Write` after r4-A landed; smoke 1/1 GREEN | r4-A's `exitDone`-channel-close predicate does NOT wait for kernel to release the OFD lock (delayed `__fput`, OFD lock persists past `wait4`) | (none yet — this ADR is the decision artifact) | — |

**The pattern: each of these five fixes defends the same architectural
assumption.** The assumption is:

> **The controller stages disk images on its LOCAL filesystem; the driver
> consumes from the SAME LOCAL filesystem.**

When the assumption holds (single-worker smoke), the patches work; smoke
went 1/1 GREEN at every cycle from r3-A onward. When the assumption
breaks (multi-worker stress: cross-node placement, kernel deferred work,
cleanup-vs-retry races, OFD-lock lifecycle outliving `wait4`), patches
are required and each new failure mode requires a new patch at a deeper
layer.

The architecture review at `r27-A1` makes the diagnostic explicit:
**five rounds of layer-peel are now themselves the evidence that the
patched invariant is wrong.** The pyramid:

- `host_dir` (controller-staged): `create_ext4_image_if_missing` at
  `crates/sandbox/src/backend/nomad_ch.rs:736-760`. Driver assumes
  presence (controller v33 added the `assert_disk_image_present`
  parity preflight at `30960451`). Patches r1 + r2 + r3 each defend
  this resource.
- `rootfs.img` (driver-staged via a controller-emitted source path):
  `restore_handler.rs:2348-2351` emits `rootfs_source`; driver
  hardlinks (`50fb987d`). The hardlink shares an inode → shares an OFD
  `flock`. Patches r4 + (predicted r5 follow-on) defend this resource.
- `taps` (driver-staged from an index allocated controller-side):
  patches r1 + r3-B defend this resource.

Each row is a shared mutable resource crossing two trust boundaries
(controller-fs → Nomad-fs, driver-process → CH-process, kernel-netdev
→ process-namespace). **Each patch defends the boundary instead of
removing it.** None of the five fixes changes the underlying
architectural premise; cumulatively they have added ~290 LOC of defense
at progressively deeper layers without simplifying anything.

## Decision

**Adopt Option C — driver-side staging.** The controller emits a typed
`StagingManifest` describing the disk images and their content
addresses; the driver materializes `workspace.img`, `home.img`, and
`rootfs.img` inside `StartTask`, runs on the same node the alloc lands
on, and owns the lifecycle of every disk-image surface for that alloc.

### Three options considered

#### Option A — Stay with locality + patches (current trajectory)

Keep the controller-stages-locally / driver-consumes-locally invariant.
Address each new failure mode with another defensive patch at the layer
the failure surfaces.

- **Pros.** Smallest per-cycle diff; preserves the "VM has fast local
  disk" performance story; no changes to wire schema or trust boundaries.
- **Cons.** Five cycles in; ~290 LOC of layer-peel already landed; no
  architectural simplification; each new failure mode requires a new
  patch at a deeper layer (r27-A2 predicts at least three more candidate
  surfaces: SNAPSHOT-side reap race, workspace.img lock-mode collision,
  cross-sandbox concurrent SNAPSHOT/STOP race).
- **Recommended ONLY if** stress-r6 (with r5-A `F_OFD_SETLK` probe)
  hits ≥95% e2e AND no 7th-layer observable surfaces in the next 60-cycle
  run.

#### Option B — Shared filesystem (NFS / GCS-FUSE / Ceph for `/var/zeroship/`)

Mount a single shared filesystem on every worker; the controller stages
to it, the driver consumes from it, cross-node placement is irrelevant
because every worker sees the same view.

- **Pros.** Eliminates node-locality issues entirely; r3-A's
  node-affinity Constraints become unnecessary; the controller can stage
  from any node and the alloc can land anywhere.
- **Cons.** Kills the "VM has fast local disk" performance story —
  NFS round-trips add ~10-50 ms per disk operation, `mkfs.ext4` over NFS
  is unbounded under back-pressure; introduces a new infra dependency
  (NFS server or GCS-FUSE per-worker mount); the host_dir GC sweeper
  needs cross-worker coordination (multiple workers can race the same
  inode); failure-mode discovery is hidden behind NFS stalls and mount
  timeouts (not in the existing structured-error pipeline).
- **Cost estimate.** Likely ~100 LOC of controller wiring PLUS new
  infrastructure (NFS server provisioning, mount lifecycle, cross-worker
  coordination of the sweeper). NOT TRIVIAL. The infra layer is the
  expensive part, not the code.

#### Option C — Driver-side staging (CHOSEN)

Move `create_ext4_image_if_missing` and `materialize_rootfs` into the
driver's `StartTask`. The controller emits a typed `StagingManifest`
declaring which images the task needs (paths, sizes, content-addresses,
mode flags); the driver materializes them on the worker that will run
the VM.

- **Pros — by failure class.**
  - **TOCTOU collapse.** The driver stages and consumes on the SAME node
    in the SAME process; no host-fs / alloc-fs boundary crossing.
  - **Cross-node placement irrelevant.** The driver runs where the alloc
    lands; r3-A's node-affinity Constraints become defense-in-depth, not
    a correctness requirement.
  - **Cleanup-vs-retry race irrelevant.** Each `StartTask` creates fresh
    images in the per-alloc `runDir`; `CreateGuard::drop` no longer
    rm -rf's a host_dir that another alloc is mid-reading. The r2
    sweeper retains its role for failed-create leaks but no longer races
    successful retries.
  - **OFD lock collisions irrelevant.** `rootfs.img` is freshly
    hardlinked per alloc; the source inode is shared across allocs but
    each alloc's `flock` is on a fresh dirent; no cross-alloc lock
    contention. r4-A's reap-wait + r5-A's `F_OFD_SETLK` probe become
    kernel-hygiene observability rather than correctness gates.

- **Cons — explicitly named.**
  - **Cross-cuts `CreateGuard` rollback.** The controller no longer
    fails fast on disk-full / quota-exceeded — those errors surface as
    `StartTask` failures (task-failed-after-place), not job-rejected-at-
    submit. Operator-facing implication: schedule retries cost more
    (alloc placed + failed → restart vs alloc never placed → resubmit);
    `wake_jobs.error_message` needs to carry the staging-error class
    typed (R25-I1's typed staging path remains the wire schema, but the
    PRODUCER moves from controller to driver). Mitigated by the
    structured-error pattern already in place at `022f778a`.
  - **host_dir GC sweeper contract changes.** The v34 sweeper
    (`e82bffd7`) was written assuming controller-created host_dirs; with
    driver-side staging the sweeper reaps DRIVER-staged dirents. The
    enumeration mechanism (`<HOST_STATE_DIR>/<sandbox_id>` typed-name
    + mtime + DB join) does NOT change — the dirents land in the same
    place — but the documentation MUST cite the driver as the creator
    rather than the controller. Per-surface sweeper pattern from the
    kernel-state-inventory ADR continues to apply unchanged.
  - **Wake/restore path changes.** The wake's `--restore` path consumes
    images staged at SNAPSHOT time, including the persistent `home.img`.
    Under driver-side staging the SNAPSHOT path must stage `home.img`
    into a controller-readable location (GCS upload or controller-fs
    write through a worker-side push); the restore-time staging is then
    driver-side again on the worker that lands the wake alloc. Snapshot
    artifact storage already lives in `TieredSnapshotStore` (L1 + L2 +
    GCS) — the integration point is the snapshot manifest, not a new
    artifact pipeline.

- **Cost estimate.** Per `r27-A4`: ~230 LOC of structural code
  (`StagingManifest` typed wire ~80 LOC + driver-side
  `stageDiskImages(taskConfig)` ~120 LOC + controller emission
  cutover ~30 LOC). The 5-cycle layer-peel has already paid the
  ~290-LOC cost defending an invariant Option C makes obsolete; the
  structural rewrite is now *smaller* than the accumulated patches.

### Why Option C, not A or B

- **A is not closed.** Stress-r5 RED at the identical 5% rate that r4
  posted means r4-A's `exitDone`-channel-close predicate did NOT
  actually close the rootfs.img lock surface. r27-A2 already predicts
  three additional candidate layers beneath r4; Option A's forward path
  is "peel ≥3 more layers and hope no 7th surfaces."
- **B trades architecture for infrastructure.** The shared-filesystem
  approach hides the failure-mode discovery loop behind NFS-class
  failure modes (mount stalls, partial write, write-after-close
  reordering) that don't compose with the existing structured-error
  pipeline. The latency tax is real and the new-infra surface (NFS
  server SPOF + mount lifecycle) is a fresh class of failure we have
  not invested observability for.
- **C collapses four failure classes at once.** Per the r27-A4
  surface-collapse table, Option C eliminates host_dir leak, cross-worker
  placement, OFD lock cross-alloc contention, and tap-EEXIST-from-prior-
  alloc as correctness concerns. Each of those was a multi-cycle peel
  under Option A.
- **The schema prerequisite has landed.** `r24-A1`'s typed staging
  schema (R25-S1) shipped at `022f778a`. The "Option 3 depends on the
  schema half" deferral from r3-A is no longer applicable.

The choice is therefore: stay on a trajectory whose 5-for-5 evidence
predicts ≥3 more cycles before convergence, OR ship a ~230-LOC
structural rewrite whose surface-collapse table eliminates four of the
candidate-future-layers in a single pass.

## Migration plan

Five phases. Each phase is a self-contained landing; phases 2 and 3 may
be split into smaller commits along the trait boundary if the diff
exceeds 400 LOC.

### Phase 1 — Design (this ADR)

This document. Immutable once landed. Sibling ADRs (playbook,
kernel-state-inventory) cross-link; the playbook ADR's r3-A and r4-A
retros (per r27-A6) cite this ADR as the structural alternative the
layer-peel surfaces.

### Phase 2 — Driver-side `stageDiskImages(taskConfig)` — pure file ops

Add a typed `StagingManifest` to the driver's task-config schema. Add
`stageDiskImages` to the driver's `StartTask` BEFORE the CH-spawn step.
NO state-machine change yet — the controller still calls
`create_ext4_image_if_missing` on the local fs; the driver's
`stageDiskImages` is a no-op when the typed manifest is absent (back-compat
flag for the migration window). This phase is observability-only — it
validates the wire schema and the driver-side staging primitives
against a live cluster without changing the correctness contract.

Exit criterion: driver-side `stageDiskImages` runs on every alloc, emits
`stage_disk_image_{success,fail}_total{kind=workspace|home|rootfs}`
counters, no failure mode introduced (smoke + stress at the existing
baseline).

### Phase 3 — Controller cutover (`RealRestoreBackend::try_create` → no-op)

Migrate `create_ext4_image_if_missing` (cold-boot path) and
`materialize_rootfs` (restore path) out of the controller. The
controller's `try_create` body becomes validation-only (schema check,
quota check, billing precondition) and emits the `StagingManifest` to
the driver. The driver's `stageDiskImages` (from phase 2) becomes the
sole materializer.

The host_dir GC sweeper stays controller-owned. Its enumeration
mechanism is unchanged (same `<HOST_STATE_DIR>/<sandbox_id>` topology,
same mtime + DB-join eligibility gates); only the rustdoc updates to
cite the driver as the creator and the controller as the reaper.

`CreateGuard` rollback becomes a no-op for disk-image cleanup
(per-alloc dirs are reaped by Nomad on alloc failure); the
controller-side guard retains DB-row rollback and metering rollback.

Exit criterion: cluster smoke 1/1 GREEN. Cluster stress 60/60 at
WORKER_COUNT=3 ≥95% e2e (this is the cutover gate from the
kernel-state-inventory ADR; phase 3 IS the cutover). Verbatim
observables from r1-r5 NO LONGER appear in any failure path.

### Phase 4 — Cluster validation (smoke + stress at WORKER_COUNT=3 × 20)

Two cluster runs back-to-back: 1×1 smoke, then 60×3 stress. Smoke
gates the structural-equivalence claim (per the stress-as-gate ADR,
smoke is necessary but not sufficient — never declare cutover on smoke
alone). Stress is the falsification step.

Phase 4 is also when the abort criterion from `r27-A3` is checked: if
stress-r-Option-C ≥95% e2e, the layer-peel is closed; if 30%-95%,
ONE more cycle is allowed with a 6th-layer verbatim already captured;
if <30%, Option C did not address the dominant mechanism either and we
re-open the design.

### Phase 5 — Deprecate the layer-peel patches

Under Option C, several of the r1-r5 patches become defense-in-depth or
unnecessary:

| Patch (commit) | Status under Option C | Disposition |
| --- | --- | --- |
| Driver v13 tap pre-delete on EEXIST (`3d03cb90`) | Still required — tap is driver-owned in both worlds | KEEP |
| Driver v14 defensive vm_index-keyed tap cleanup (`177ff165`) | Still required — tap-lifecycle is per-surface from the kernel-state inventory | KEEP |
| Controller v34 leak host_dir on stop (`d638b10f`) | Per-alloc dirs are reaped by Nomad; the cleanup-vs-retry race is gone | REMOVE; sweeper rustdoc updated |
| Controller v34 GC sweeper (`e82bffd7`) | Still required for failed-create leak hygiene; reaps driver-staged paths | KEEP, rewrite rustdoc to cite driver as creator |
| r3-A node-affinity Constraints (`883df7fe` + `9b623f44` + `d71f1a8c`) | Cross-node placement is irrelevant once driver stages locally | REMOVE OR keep as defense-in-depth (decide post-stress) |
| Driver v15 netdev release poll (`05440498`) | Still required — kernel netdev release timing is independent of staging | KEEP |
| Driver v16 reap-wait + counter (`e7ce7f1f` + `9af429c7`) | rootfs.img is per-alloc hardlinked fresh; cross-alloc OFD lock contention is gone. Reap-wait stays as kernel hygiene; the counter stays as observability | KEEP (defense-in-depth) |
| r5-A `F_OFD_SETLK` probe (proposed, not landed) | Per-alloc fresh inode means no cross-alloc lock contention to probe | DO NOT LAND |

Phase 5 is deliberately separated from phase 3 so the deprecation
commits are reviewed against the GREEN cluster, not against the
in-flight rewrite.

## Trade-offs explicitly named

This section enumerates the consequences Option C accepts so future
readers can audit them against post-cutover observability.

1. **Fail-fast on disk-full moves from submit-time to start-time.**
   Under Option A, the controller's `create_ext4_image_if_missing` ran
   at submit time and a disk-full ENOSPC rejected the wake_job before
   alloc placement. Under Option C, the alloc places, the driver's
   `stageDiskImages` returns an error, the task transitions to
   `Failed`, and the wake_job carries the typed staging error from
   `R25-I1`'s schema. The error is still typed and operator-readable;
   the scheduling cost is one alloc placement that immediately fails.
   Mitigated by per-worker disk-pressure metrics already exposed
   (`sandbox_disk_pressure_*`).

2. **Wake/restore path stages from snapshot artifacts.** Under
   Option A, the controller had the persistent `home.img` on its local
   fs and the wake just emitted the path. Under Option C, the SNAPSHOT
   path uploads `home.img` to the `TieredSnapshotStore` (L1 local +
   L2 GCS); the wake's `StagingManifest` cites the content-addressed
   blob; the driver's `stageDiskImages` fetches from the store before
   `StartTask` returns. Worst-case latency: an L1 miss + L2 fetch (GCS
   download, ~3-5 s for a typical home.img). Mitigated by the
   `TieredSnapshotStore`'s prefetch hook on `prepare_wake`.

3. **host_dir GC sweeper retains its role.** The v34 sweeper
   (`e82bffd7`) was designed for failed-CREATE leak hygiene. Under
   Option C the sweeper still applies — driver-staged dirents still
   leak under failed-stage scenarios. The enumeration mechanism is
   unchanged. The rustdoc at `sweep.rs:1249` MUST be updated in phase 3
   to cite the driver as creator.

4. **r3-A Constraints become defense-in-depth.** Once driver-side
   staging lands, cross-worker placement no longer breaks correctness.
   The Constraints block can stay (for defense-in-depth: pinning to a
   specific worker for locality / cache-warmth reasons) or be removed
   (under Option C the placement is intentionally any-worker). The
   decision is deferred to phase 5 post-cluster validation.

5. **The structured-error pipeline narrows its surface.** R22-S1's
   sanitizer (`7647cd4d`) widening for `/var/zeroship/` paths was
   motivated by driver `TaskEvent.DisplayMessage` carrying controller-
   staged path. Under Option C the path leak surface narrows (the
   driver still writes to `/var/zeroship/ch/<sid>/` but the controller
   no longer does), but the sanitizer continues to cover both producers
   — no narrowing of the redaction policy is appropriate.

6. **Snapshot manifest grows a staging-locality field.** The snapshot
   manifest already carries `home.img` content-address. It must also
   carry a `rootfs_source` content-address (today emitted as a path
   string at `restore_handler.rs:2348-2351`); the driver fetches both
   on wake. The schema evolution stays inside the existing snapshot
   manifest wire (per the pre-launch no-back-compat policy — rename
   the field, update every emitter and consumer in the same PR).

## Risks

This section names the credible failure modes for Option C so phases
3 + 4 have a concrete falsification list.

- **HIGH risk: a new bug class surfaces post-cutover.** Every
  architectural rewrite has its own bug class. The first stress run
  after Option C lands will likely surface NEW issues at NEW layers
  (driver-side blob fetch failures, content-address miss handling,
  driver-fs quota under cross-tenant contention). Estimated 1-3
  additional cycles to converge post-Option C. The abort criterion
  from `r27-A3` applies: stress < 30% → re-open the design.

- **MEDIUM risk: the ~230 LOC estimate is low.** `r27-A4`'s forecast
  was structural-code only. Actual diff likely 300-500 LOC once tests,
  schema migration, and the snapshot-manifest field rename are
  counted. Not a correctness risk — a scope risk for the migration
  window.

- **MEDIUM risk: snapshot-side latency regression.** Under Option A,
  the wake's `home.img` was an inode hardlink (zero-latency). Under
  Option C it is a blob fetch (3-5 s on L1 miss). If the L1 hit rate
  is low under stress (different sandbox per cycle, no warm cache),
  wake latency moves from "tens of ms" to "single-digit seconds." May
  require the `TieredSnapshotStore` to grow a `prepare_wake` prefetch
  primitive — already noted in the trade-offs section.

- **LOW risk: driver-side blob fetch races task-start timeout.** Nomad
  enforces a task-start timeout; if the L2 fetch is slow, `StartTask`
  could exceed it. Mitigation: drive the staging on a goroutine before
  `StartTask` returns; cap the staging budget and surface a typed
  timeout. The existing `wait_for_alloc_running_blocking` budget at
  `nomad_ch.rs:2756` is the model.

- **LOW risk: driver-fs quota under cross-tenant contention.** Per-
  alloc images live under `runDir` (Nomad-managed allocation directory
  on the worker's local disk). If the cumulative live-alloc footprint
  exceeds the worker disk, Nomad's existing disk-pressure eviction
  applies. Already observable via `nomad_client_allocated_disk` /
  `nomad_client_unallocated_disk`. No new mitigation needed.

## Cross-references

- **Cluster reviews (the five rounds of evidence).** `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r1.md` through `…-stress-r5.md`. Each carries the verbatim layer-N observable cited in the Context table.
- **Architecture reviews.**
  - `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r26.md` — `r26-A2` (post-r3-A topology cascade: per-sandbox node-pinning breaks Nomad reschedule-around-wedged-worker, motivates re-evaluating shared-storage vs driver-side staging); `r26-A3` (deferred-alternatives ADR section gap).
  - `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r27.md` — `r27-A1` CRITICAL (five-layer diagnostic as a structural diagnostic); `r27-A4` (Option-3 break-even argument with surface-collapse table); `r27-A6` (playbook ADR's r3-A + r4-A retros owed).
- **Sibling ADRs.**
  - `docs/decisions/2026-05-25-restore-debug-playbook.md` — observability-before-architecture, stress-as-gate, triage tier ladder. This ADR's phase-4 falsification step IS the stress-as-gate rule.
  - `docs/decisions/2026-05-25-kernel-state-surface-inventory.md` — per-surface sweeper pattern. This ADR's phase-5 sweeper-rustdoc rewrite preserves the per-surface pattern; the sweeper retains its role, only the creator-of-record changes.
- **Prerequisite schema.** `022f778a` (R25-I1 / R25-I2 / R25-S1 typed `StagingPathMissing`) is the wire-schema half of Option C; the PRODUCER of the typed error moves from the controller to the driver under phase 3.
- **Five-cycle commit ledger.** All commits in the References block are the layer-peel landings phase 5 disposes of.
