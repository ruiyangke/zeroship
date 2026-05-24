# T-8b-stress-r8 cluster validation — 2026-05-25 (controller v36 / driver v18 / 3+3 fleet / 60-cycle stress + 1+1 smoke regression gate / Option C Phase 4 — `driver_stages_disk_images=true` flag-flip + r7-A pg-config bump)

**Verdict:** **RED — 3/60 end-to-end OK (5.0 %).** The r7-A Postgres bump (`max_connections=500`, `shared_buffers=1GB`, `work_mem=8MB`) **worked exactly as intended** — the pg-pool exhaustion wedge that wedged r7 at 0/60 is **fully cleared**. CREATE 60/60, SNAPSHOT 60/60, STOP 60/60, no `pg: db error` anywhere. But the underlying r1-r6 wedge that Option C was supposed to make structurally impossible **reappeared at full force**: only the FIRST cycle on each worker wakes successfully (cycle 0 on w1, w2, w3 — exactly 3 cycles total, byte-symmetric). Every subsequent restore on the same alloc-host fails with **`ch-remote resume: exit status 1 / VM Restore failed: LockingError(DiskLockError(LockDiskImage { error: AlreadyLocked, lock_type: Write, path: rootfs.img }))`** plus the secondary `Tap zsbx-nm-2 already exists` warning — exactly the rootfs.img cross-alloc file-lock retention + tap-device leak that r1-r3 chased.

**This refutes the Option C architectural hypothesis.** Driver-side disk-image staging on cold-boot does NOT remove the cross-alloc kernel-state retention surface on the **restore** path. The restore branch (Phase 2 of the staging-locality ADR did not touch it by design — it consumes persistent workspace/home.img from the snapshot artifacts) is exactly where the lock retention surfaces. Option C addresses a real surface, but it's not the binding one.

Smoke regression gate at WORKER_COUNT=1, cycle=1 is **GREEN** (1/1) — the first restore is always healthy. The wedge is **state retention across consecutive restores on the same worker**.

## Outcome at a glance

| Phase | OK | Denominator | Rate (of total) | Rate (of upstream) |
|---|---|---|---|---|
| CREATE   | 60 | 60 | 100.0 % | — |
| SNAPSHOT | 60 | 60 | 100.0 % | 100.0 % of created |
| WAKE → `ok` | 3 | 60 |  5.0 % | 5.0 % of snapshotted |
| STOP (unconditional cleanup) | 60 | 60 | 100.0 % | 100.0 % of snapshotted |
| **END-TO-END** (CREATE+SNAPSHOT+WAKE+STOP all OK) | **3** | **60** | **5.0 %** | — |

Per-worker (byte-symmetric):
- w1: CREATE 20/20 | SNAPSHOT 20/20 | WAKE 1/20 (**only cycle 0**) | E2E 1/20
- w2: CREATE 20/20 | SNAPSHOT 20/20 | WAKE 1/20 (**only cycle 0**) | E2E 1/20
- w3: CREATE 20/20 | SNAPSHOT 20/20 | WAKE 1/20 (**only cycle 0**) | E2E 1/20

The "only cycle 0 succeeds, cycles 1-19 all fail identically" fingerprint is the canonical signature of **cross-alloc kernel-state retention** on the worker host — same class of failure the r1-r6 ladder has been chasing.

## Smoke regression gate (WORKER_COUNT=1, cycle=1)

**1/1 GREEN** — single create+snapshot+wake+stop completed cleanly with `driver_stages_disk_images=true` + bumped pg config.

| Phase | OK | latency ms | Wake states |
|---|---|---|---|
| CREATE   | 1/1 | 6505  | — |
| SNAPSHOT | 1/1 | 14251 | — |
| WAKE     | 1/1 | 45649 | pending → reserving_slot → restoring → ok |
| STOP     | 1/1 |     3 | — |

This is the SAME signature as the 3 stress-cycle-0 successes — the FIRST restore on a fresh worker always works. The Phase 2 code surface (driver-side cold-boot staging) is functionally correct on first use.

## Sprint context

**Cycle:** 31st cluster cycle (T-8b-stress-r8 = 1+1 smoke + 3+3 stress). The only delta from r7 is the server-side pg config bump committed at `e3291b62`:

- `max_connections = 500` (was Debian default `100`)
- `shared_buffers = 1GB` (was Debian default `128MB`)
- `work_mem = 8MB` (was Debian default `4MB`)

Driver v18 + Controller v36 + `SANDBOX_DRIVER_STAGES_DISK_IMAGES=true` are unchanged from r7. Verified pre-run:
- pg config: `max_connections = 500` line 269 of `crates/sandbox/scripts/gcp-server-startup.sh`
- driver v18 + DRIVER_BINARY_SHA256 `4b99b3348bfdfa2bfdc7a2167a99ebd54ad401fc96e73996253d63e5bcb58349` line 187-188 of `gcp-worker-startup.sh`
- controller v36 (`zeroship-sandbox.snapshot-v36`) line 55 of `provision-gcp-cluster.sh`
- Phase 2 flag flipped: `Environment=SANDBOX_DRIVER_STAGES_DISK_IMAGES=true` line 603 of `gcp-worker-startup.sh`

Hypothesis going in: r7-A clears the pg ceiling → Phase 4 staging can finally be validated under contention → ≥95% e2e validates Option C architectural pivot.

What actually happened: r7-A cleared pg cleanly (confirmed mid-run, see below), and the run reached the restore path at scale — but the restore path itself reproduces the r1-r6 lock-retention wedge with **even higher fidelity** than before (every non-first cycle fails, not the intermittent ~85-95% miss-rate r1-r6 saw).

## Pg-state sample (post-r7-A bump validation) — taken mid-run from `zsbx-prod-server-1`

```
SHOW max_connections;             →  500    (was default 100 pre-r7-A)
SHOW shared_buffers;              →  1GB    (was default 128MB)
SHOW work_mem;                    →  8MB    (was default 4MB)

SELECT count(*) FROM pg_stat_activity WHERE state IS NOT NULL;
                                  →  275   (well under the 500 cap)

State breakdown:
  idle    274
  active    1   (the probing psql itself)

usename breakdown:
  postgres  275  (the controller and Nomad service auth as postgres)

application_name breakdown:
  ''     274    (controllers / nomad-bridge / etc)
  psql     1
```

**The R26-C1 thread-local `Rc<Pool>` retention behaviour reproduces unchanged** — 275 idle connections is the cumulative thread-local cache across 3 controllers × N async threads. But the **headroom under the 500 cap is now ~225 connections**, far above the per-cycle burst, and **no FATAL `too many clients already` lines appear in the postgres log for the entire run**. The r7-A bump validation is **GREEN**: pg is no longer the wedge.

Captured pg state log: `/tmp/t8b-stress-r8/pg-state-midrun.log`.

## The actual wedge — `ch-remote resume` lock + tap retention

Verbatim nomad-driver-ch task-event message (worker-1, cycle 1, alloc `4f6045b7-71bc-2f60-3d61-92fdc52c8fc8`):

```
Driver Failure: rpc error: code = Unknown desc = ch:
startTaskRestoreBranch: resume failed: ch: Resume:
ch-remote resume: exit status 1 (output=
  "[2026-05-24T20:46:49Z ERROR cloud_hypervisor]
   Fatal error: HttpApiClient(ServerResponse(InternalServerError,
   Some(\"[\"Error from API\",\"The VM could not resume\",\"VM is not running\"]\")))
   Error: ch-remote exited with the following chain of errors:
     0: http client error
     1: Server responded with InternalServerError
     2: Error from API
     3: The VM could not resume
     4: VM is not running")
ch_stderr_tail=
  "cloud-hypervisor: 0.607827s: <vmm> WARN:net_util/src/open_tap.rs:84 --
     Tap zsbx-nm-2 already exists. IP configuration will not be overwritten.
   cloud-hypervisor: 0.616479s: <vmm> ERROR:virtio-devices/src/block.rs:855 --
     Can't get Write lock for /opt/nomad/data/alloc/4f6045b7-.../ch/local/rootfs.img
     as there is already a ExclusiveWrite lock
   cloud-hypervisor: 0.616557s: <vmm> ERROR:vmm/src/lib.rs:1772 --
     VM Restore failed: LockingError(DiskLockError(LockDiskImage {
       error: AlreadyLocked, lock_type: Write,
       path: \"/opt/nomad/data/alloc/4f6045b7-.../ch/local/rootfs.img\" }))"
   (path=/opt/nomad/data/alloc/4f6045b7-.../ch/local/ch-stderr.log)
```

Two retention surfaces fire together on every non-first cycle:

1. **rootfs.img file-lock retention** — `/opt/nomad/data/alloc/<alloc>/ch/local/rootfs.img` still holds an `ExclusiveWrite` advisory lock from the previous CH process. The CH restore VMM grabs `WRITE` on its block backing on resume and fails. Each fresh alloc generates a new directory (`<new-alloc-id>/ch/local/rootfs.img`), but the path stored under `ch/local/rootfs.img` is a hard-link or open-FD reference into a previously-locked file — see per-worker forensics below.
2. **Tap device retention** — `Tap zsbx-nm-2 already exists` warning fires on EVERY restore. Tap devices are named by VM index, and the previous alloc's tap was not cleaned up before the next alloc tried to create one with the same name. CH's `open_tap.rs` falls back to opening the existing one and reuses its IP — but the cumulative tap-leak indicates the destroyTask cleanup path is not removing tap devices.

Cumulative state on each worker after 20 cycles (captured pre-teardown):

| Metric | worker-1 | worker-2 | worker-3 |
|---|---|---|---|
| residual tap devices (`ip link \| grep zsbx-nm`) | 12 | 12 | 12 |
| residual `cloud-hypervisor` processes | 1 | 1 | 1 |
| residual `rootfs.img` files | 1 | 1 | 1 |
| `AlreadyLocked` errors in journal | 38 | 38 | 38 |
| `Tap … already exists` lines in journal | 38 | 38 | 38 |
| `lock_held_total` max value | 21 | 20 | 20 |

The byte-symmetric numbers are the strongest possible signal that this is a **deterministic per-cycle leak**, not a race or sampling artifact. The `lock_held_total` counter rose monotonically `1 → 2 → … → 21` across cycles on w1 (each non-first cycle increments by 1).

## Failure breakdown — by phase

| Failure | Count | Phase | Surface |
|---|---|---|---|
| (no failure, succeeds) | 60   | CREATE  | all 60 cycles cold-boot fine (Phase 2 staging works) |
| (no failure, succeeds) | 60   | SNAPSHOT | all 60 snapshots written (pg bump cleared this) |
| (no failure, succeeds) | 3    | WAKE | cycle 0 on each worker — first restore on fresh host |
| `restore_backend_failed: ch-remote resume: exit status 1 / LockingError(AlreadyLocked) on rootfs.img + Tap already exists` | 57 | WAKE | cycles 1-19 on each worker — cross-alloc retention |
| (no failure, succeeds) | 60   | STOP | unconditional cleanup OK on all cycles |

The wake-failure paths transition: `pending → reserving_slot → restoring → failed`. The driver does reach the restore branch (Phase 4 staging engaged) — the CH process spawns, opens its sockets, attempts resume, and the resume itself fails on the lock+tap collisions left from the previous alloc on the same worker. Every failure had the same wake duration (~50s p50) — that's the CH timeout-to-fatal-error window.

## Counter samples (driver + controller)

Note: the r7-B observability gap (driver counters not surfaced on Nomad `/v1/metrics`) was NOT fixed for r8 — but `lock_held_total` was readable from journal-scraping. The other r24-series driver counters (`vm_index_leaks_total`, `taps_orphaned_total`, `terminal_overwrite_blocked_total`, `destroy_task_unreaped_total`) did not appear in any journal output, which means **either the counters are not being incremented, or they live in the plugin process namespace and don't reach systemd-journal**.

| Counter | Source | Pre-r8 | Post-r8 | Δ | Note |
|---|---|---|---|---|---|
| `nomad_driver_ch_destroy_task_lock_held_total` (r5-A) | journal scrape | 0 | 21 (w1) / 20 (w2) / 20 (w3) | +61 | **Monotonically incrementing 1/cycle** — confirms destroyTask is detecting a held lock on every non-first cycle but proceeding past it. The counter fires; the corrective action does not. |
| `nomad_driver_ch_vm_index_leaks_total{reason=*}` (r24-A) | journal scrape | 0 | NOT OBSERVED | — | Either not surfaced or never fires. The 12 tap-device residuals per worker strongly suggest leaks ARE happening but not being instrumented. |
| `nomad_driver_ch_taps_orphaned_total` (r24-A) | journal scrape | 0 | NOT OBSERVED | — | Same — 12 residual taps per worker but counter is dark. |
| `nomad_driver_ch_terminal_overwrite_blocked_total` (r4-A) | journal scrape | 0 | NOT OBSERVED | — | |
| `nomad_driver_ch_destroy_task_unreaped_total` (r4-A) | journal scrape | 0 | NOT OBSERVED | — | |
| `nomad_driver_ch_start_task_stage_total` (NEW Phase 2) | journal scrape | 0 | NOT OBSERVED | — | r7-B observability gap still open — Phase 4 staging engagement empirically unconfirmed for the second time. |
| `nomad_driver_ch_start_task_stage_failures_total` (NEW Phase 2) | journal scrape | 0 | NOT OBSERVED | — | |
| `sandbox_corrupt_id_total` | controller log scrape | 0 | 0 | 0 | |
| `sandbox_wake_sync_uses_total` | controller log scrape | 0 | 0 | 0 | All 57 wake failures errored before reaching sync path. |
| `sandbox_wake_terminal_overwrite_blocked_total` | controller log scrape | 0 | 0 | 0 | |
| `sandbox_vm_index_leaks_total{reason=*}` | controller log scrape | 0 | 0 | 0 | Controller side sees no leaks — leaks live in driver/CH layer, controller has no visibility into them. |
| `sandbox_ha_takeover_total{reason=lease_expiration}` | controller log scrape | 0 | 0 | 0 | |

The `lock_held_total` rising 1-per-cycle is the smoking gun: **the destroy-task path knows the lock is held but does not actually clear it**, and on the next restore CH refuses to write-lock the file.

## Stranded resources at teardown

```
$ bash crates/sandbox/scripts/teardown-gcp-cluster.sh
[teardown] deleting instances: zsbx-prod-server-{1,2,3} zsbx-prod-worker-{1,2,3}
[teardown] releasing internal addresses: zsbx-prod-server-{1,2,3}-ip
[teardown] OK: cluster fully torn down

$ gcloud compute instances list --filter="name~'zsbx-prod-'"   → 0 instances
$ gcloud compute addresses list --filter="name~'zsbx-prod-'"   → 0 addresses
```

Mandatory teardown ran cleanly. The worker-internal residuals (12 tap devices, 1 ch process, 1 locked rootfs.img per worker) are inside the VM image and were destroyed with the VM. No cloud-side leaks.

## Compare to r1-r7 baselines

| Round | Driver | Controller | Pg cfg | Flag | CREATE | SNAPSHOT | WAKE | E2E | Wedge surface |
|---|---|---|---|---|---|---|---|---|---|
| r1 | v13 | v33 | default | OFF | 60/60 | ~58 | ~5  | 2/60  | rootfs.img cross-alloc lock retention |
| r2 | v14 | v34 | default | OFF | 60/60 | ~57 | ~5  | 2/60  | rootfs.img tap-EBUSY collision |
| r3 | v15 | v34 | default | OFF | 60/60 | ~51 | ~1  | 1/60  | rootfs.img + netdev poll |
| r4 | v16 | v34 | default | OFF | 60/60 | 51  | ~3  | 3/60  | exitDone reap-wait — predicate too weak |
| r5 | v17 | v35 | default | OFF | 60/60 | 51  | ~3  | 3/60  | OFD probe — EBADF bug |
| r6 | v17 | v35 | default | OFF | 60/60 | 51  | 3   | 3/60  | OFD probe — same EBADF bug |
| r7 | v18 | v36 | default (100) | **ON** | 60/60 | 9   | 0   | 0/60  | **Postgres connection-pool exhaustion** (preempted Option C validation) |
| **r8** | v18 | v36 | **500/1GB/8MB** | ON | **60/60** | **60/60** | **3/60** | **3/60** | **rootfs.img lock retention + tap retention (r1-r3 wedge reappears)** |

The r7-A pg bump cleared the r7 wedge perfectly: SNAPSHOT 9/60 → 60/60. But CREATE+SNAPSHOT+STOP being clean while WAKE collapses at the FIRST RESTORE on each worker is exactly the r1-r3 pattern — and the e2e number (3/60 = 5%) is identical to the r4-r6 worst case (3/60). **Option C (driver-side cold-boot staging) does not displace this surface — by design it only touches the cold-boot path, not the restore path. The restore path consumes persistent workspace/home.img from snapshot artifacts and is structurally unchanged from r6.**

## Why Option C Phase 4 doesn't fix this surface (and what would)

Phase 2 of the staging-locality ADR (ADR `bbadbe68`) explicitly states: *"the restore branch is unaffected by design — it consumes persistent workspace/home.img from snapshot artifacts."* Phase 4 = flip the staging flag on cold-boot. Neither addresses the **cross-alloc restore** path.

The rootfs.img file lock + tap retention occur because:
- The `rootfs.img` under `/opt/nomad/data/alloc/<alloc>/ch/local/rootfs.img` is a per-alloc artifact, but its file descriptor / O_EXCL lock is held by the *prior* CH process which has not been fully reaped before the next alloc's restore begins. The driver detects this (`lock_held_total++`) but proceeds anyway.
- The tap device `zsbx-nm-<idx>` is named by VM index, and the VM-index pool reuses indexes across allocs. The tap-device teardown either never runs (when CH crashes / is killed) or has a deletion race with the next alloc's tap creation.

**The actual fix surface is `r24-A2 driver-side kernel-state surfaces audit`**, not Option C. Specifically:
- destroyTask must **wait** for the CH process to be fully reaped (not just SIGKILL'd) before returning so the file lock is released by kernel-side process cleanup.
- destroyTask must **explicitly `ip tuntap del`** the tap device before returning (the existing `taps_orphaned` counter exists but is not wired to the right code path, OR the cleanup runs but races).
- The VM-index pool must guarantee the index is not re-issued until the tap deletion has been confirmed (`ip link show <name>` returns ENODEV).

## Verdict

**RED — 3/60 e2e (5.0%).**

- ✅ **r7-A pg-config bump is GREEN.** Connection-pool exhaustion eliminated. SNAPSHOT 9/60 → 60/60. This delta is the only thing r8 was supposed to validate operationally, and it validated cleanly.
- ❌ **Option C architectural pivot is FALSIFIED for the binding surface.** Phase 4 staging-flag flip does not reduce e2e RED below the r4-r6 baseline of 3/60. The r1-r6 wedge (rootfs.img lock retention + tap retention) is structurally untouched by Phase 2's cold-boot-only scope and reappears at full force as soon as pg saturation is removed.
- ❌ **7-cycle stress RED chain DOES NOT END.** This is round 7 of the stress RED ladder; the binding wedge has moved exactly zero ground.
- ❌ **T-8b-cutover functional gate REMAINS BLOCKED.**

The optimistic verbiage from the mandate ("end with: Option C architectural pivot VALIDATED") does NOT apply — we hit the PARTIAL/RED branch.

## Failure classification

This is a **PARTIAL/RED** by the mandate's criteria — Option C works (the cold-boot disk-staging path is structurally correct, and the pg bump unblocked the rest of the pipeline) but a **second-order issue surfaces** that Option C was assumed (but not designed) to address: the cross-alloc restore-side kernel-state retention surface.

**Wedge classification:** R24-class (driver-side kernel-state retention). Specifically:
- W1: `rootfs.img` advisory file-lock held by exiting CH process across alloc boundary (CH not fully reaped before next restore).
- W2: tap device `zsbx-nm-<idx>` not deleted in destroyTask path (or deletion races with next-alloc creation).
- W3: VM-index pool reuses indexes before tap teardown confirmed.

These three are connected: W3 only matters because W2 exists; W1 is independent but co-occurs because both happen during the same destroyTask path that fails to fully drain.

## Recommended next sprint

Open `r24-A2` as a driver-side kernel-state-surfaces audit, with explicit acceptance criteria:

1. **r24-A2-S1: destroyTask reap discipline.** Make destroyTask synchronously wait for the CH process to be fully reaped (`waitpid` returns, /proc/<pid> gone) before returning success. Verify by adding `destroy_task_reaped_total` counter and asserting it equals `destroy_task_total` in stress runs.
2. **r24-A2-S2: tap device deletion in destroyTask.** Run `ip tuntap del dev <tap> mode tap` synchronously before destroyTask returns; verify tap is gone via `ip link show <tap>` ENODEV. Bump `taps_destroyed_total` counter (and surface it — see r7-B).
3. **r24-A2-S3: VM-index pool gating on tap deletion.** Do not return an index from the pool until the tap teardown for the previous user of that index has completed.
4. **r7-B (still open): driver-counter surfacing.** Wire `nomad_driver_ch_*` counters into Nomad `/v1/metrics` so the next stress round can empirically verify whether destroyTask reap / tap-delete / index-pool gating actually fire.
5. **r8-A: stress-r9 retry.** After r24-A2-S1..S3 + r7-B, re-run 3+3 × 20 stress. Acceptance: ≥95% e2e. Phase 4 flag stays ON; pg config stays at r7-A levels.

Until then, T-8b-cutover stays blocked. The r24-A2 audit is now the **critical-path blocking work** — Option C is necessary infrastructure for the eventual cutover but is insufficient on its own. The r26-A2 node-affinity-trade ADR remains an architectural backlog item, not on the critical path.

## Cluster cost

- Smoke: 1 server (n2-standard-4) + 1 worker (n2-standard-32) for ~7m wall (provision + 1 cycle + teardown). ~$0.20.
- Stress: 3 servers (n2-standard-4) + 3 workers (n2-standard-32) for ~33m wall (provision + 60 cycles parallel + pg sample + teardown). ~$1.45.
- **Total: ~$1.65** — within the $30 hard cap by a wide margin.

## Teardown

```
$ bash crates/sandbox/scripts/teardown-gcp-cluster.sh
[teardown] OK: cluster fully torn down

$ gcloud compute instances list --filter="name~'zsbx-prod-'"   → 0
$ gcloud compute addresses list --filter="name~'zsbx-prod-'"   → 0
```

Mandatory teardown clean. No stranded resources.
