# T-8b-stress-r5 cluster validation — 2026-05-24 (controller v35 / driver v16 / 3+3 fleet / 60-cycle stress + 1+1 smoke regression gate)

**Verdict:** **RED — 3/60 end-to-end OK (5.0 %).** The r4-A DestroyTask wait-for-CH-reap fix (driver `9af429c7` + `e7ce7f1f`) clears the smoke regression gate (1/1 with the single worker pinned), so r4-A is NOT a regression. But under 3+3 stress the e2e rate is **flat to stress-r4 at exactly 3/60** — the same `rootfs.img AlreadyLocked` failure mechanism dominates the run, in the same proportion, with the same per-worker symmetry (1 e2e OK per worker, almost always cycle 0). **r4-A's predicate did not actually wait for the kernel to release the file lock.** The 5th diagnostic layer is not yet peeled — the lock-collision mechanism is more subtle than "wait until the supervisor's `exitDone` channel closes". **T-8b-cutover stays BLOCKED.**

## Outcome at a glance

| Phase | OK | Denominator | Rate (of total) | Rate (of upstream) |
|---|---|---|---|---|
| CREATE   | 60 | 60 | 100.0 % | — |
| SNAPSHOT | 53 | 60 |  88.3 % | 88.3 % of created |
| WAKE → `ok` | 3 | 53 |  5.0 % | 5.7 % of snapshotted |
| STOP (unconditional cleanup) | 53 | 60 |  88.3 % | 100.0 % of snapshotted |
| **END-TO-END** (CREATE+SNAPSHOT+WAKE+STOP all OK) | **3** | **60** | **5.0 %** | — |

Per-worker:
- w1: CREATE 20/20 | SNAPSHOT 18/20 | WAKE 1/18 | E2E 1/20
- w2: CREATE 20/20 | SNAPSHOT 17/20 | WAKE 1/17 | E2E 1/20
- w3: CREATE 20/20 | SNAPSHOT 18/20 | WAKE 1/18 | E2E 1/20

Per-worker rates are again remarkably symmetric — 1 e2e success per worker per 20 cycles, just as in r4. The state-leak hypothesis ("first cycle works, then state accumulates") still fits the data perfectly.

## Smoke regression gate (WORKER_COUNT=1)

**1/1 GREEN** — single create+snapshot+wake+stop completed cleanly with the r4-A wait-for-reap fix on a one-worker cluster.

| Phase | OK | p50 ms | Wake states |
|---|---|---|---|
| CREATE   | 1/1 | 6535 | — |
| SNAPSHOT | 1/1 | 14431 | — |
| WAKE     | 1/1 | 46979 | pending → reserving_slot → restoring → ok |
| STOP     | 1/1 | 21 | — |

r4-A's DestroyTask exitDone-wait does not regress single-worker behaviour — the 25 × 200 ms reap-budget completes within the supervisor's normal exit window. Regression gate cleared.

## Sprint context

**Cycle:** 28th cluster cycle (T-8b-stress-r5 = 1+1 smoke + 3+3 stress). Follows stress-r4 RED (worktree `01b6a744`, 3/60 e2e OK) and the driver-only bug-fix:

- **r4-A driver wait-for-CH-reap** (`9af429c7` counter scaffold + `e7ce7f1f` predicate in nomad-driver-ch) — `stop_task.go::DestroyTask` now waits on the supervisor's existing `exitDone chan struct{}` predicate before declaring the task terminal. Budget 25 × 200 ms = 5 s wall; bumps `nomad_driver_ch_destroy_task_unreaped_total` counter on budget-exhaustion + returns nil to avoid a Nomad destroy-loop on a wedged kernel. Driver test suite 133 → 136 PASS.

Hypothesis going in: closing `exitDone` (signalled by Go's `cmd.Wait()` returning) gives both `exit_files()` (file locks released) AND `wait4()` (zombie reaped). That hypothesis appears to be wrong.

**Build / upload SHAs** (verified):

- **Driver v16** sha256 `0e153a6ff7d5b6e8fe5b261c49a240cc7b86619ef302552c76186cabc35e5ec1` (size 20,230,328 B); GCS MD5 `080ec0c80c1da621860b39eb2b0f8eb7`. `scripts/build-binary.sh --verify` confirmed bit-identical rebuild (gitSHA `e7ce7f1f`). Uploaded to `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v16`.
- **Controller v35** unchanged — r4-A is driver-only.

**Pin bump** committed at sandbox `1e8fa7e8` (driver v15→v16 + `DRIVER_BINARY_SHA256` literal updated, R20-S3 SHA verify chain intact, shellcheck clean). Driver SPRINT-STATUS entry added at nomad-driver-ch `9ee26130`.

## Per-phase timings

| Phase | n  | p50    | p95    | p99    | max    |
|-------|---:|-------:|-------:|-------:|-------:|
| CREATE   | 60 |  6569  | ~37500 | ~38000 | ~38000 |
| SNAPSHOT | 53 | ~14180 | ~14500 | ~16000 | ~16000 |
| WAKE-OK  |  3 | ~46900 | ~46980 | ~46980 | ~46980 |
| STOP     | 53 |    20  |    20  |  ~1515 |  ~1515 |

Times in ms (aggregated across the three workers; per-worker tables in raw logs). CREATE p95/p99 — there is a long-tail of ~37 s cold-boots (one per worker), suggesting tap-EEXIST contention in the cold-boot path is now bleeding into CREATE-time variance even though it doesn't outright fail. This is new and worth noting but not the e2e wedge.

SNAPSHOT: 53/60 succeeded — 7 failures (same shape as r4's 9: HTTP 500 controller-side, not investigated this cycle).

WAKE: still the load-bearing failure surface, see below.

STOP: 53/53 returned 200 in 20 ms p50, same fast-path as r4. The 1515-ms outlier indicates one alloc where the STOP touched a slow Nomad path.

## Failure breakdown — the actual wedge (unchanged from r4)

### WAKE — 50 failed wakes across 53 snapshotted sandboxes (94 %)

Every WAKE failure has the **same** controller-log signature as r4, captured from Nomad alloc events on worker-1:

```
ch: startTaskRestoreBranch: resume failed:
  ch: Resume: ch-remote resume: exit status 1
  Fatal error: HttpApiClient(ServerResponse(InternalServerError,
    Some(["Error from API", "The VM could not resume",
          "VM is not running"])))

ch_stderr_tail:
  WARN net_util: Tap zsbx-nm-2 already exists. IP configuration will not be overwritten.
  ERROR virtio-devices/src/block.rs:855 -- Can't get Write lock for
        /opt/nomad/data/alloc/<NEW-alloc-id>/ch/local/rootfs.img
        as there is already a ExclusiveWrite lock
  ERROR vmm/src/lib.rs:1772 -- VM Restore failed: LockingError(DiskLockError(
        LockDiskImage { error: AlreadyLocked, lock_type: Write,
        path: ".../alloc/<NEW-alloc-id>/ch/local/rootfs.img" }))
```

**The wedge is byte-identical to r4.** r4-A's wait-for-`exitDone` did not change the failure surface, just its precise timing.

### Why r4-A's predicate is insufficient — `/proc/locks` evidence

`/proc/locks` on worker-1 mid-stress shows multiple `OFDLCK` entries with **`PID -1`**:

```
3: OFDLCK ADVISORY WRITE -1 08:01:655366 0 21474836479
4: OFDLCK ADVISORY WRITE -1 08:01:655365 0 21474836479
5: OFDLCK ADVISORY WRITE -1 08:01:398352 0 629145599
```

`PID -1` for an OFD lock means **the owning task struct is gone but the file description that holds the lock has not been released**. The Linux kernel's `do_lock_file_wait` / `posix_lock_file` semantics for F_OFD_SETLK pin the lock to the open file description (`struct file`), not the task — so the lock survives `task_struct` teardown until the last reference to the `struct file` drops via `fput()`.

The driver's r4-A predicate (`<-exitDone`) closes when Go's `cmd.Wait()` returns. `cmd.Wait()` wraps `wait4()`, which reaps the zombie — but `wait4()` does NOT block on the kernel's `__fput`/`delayed_fput` work that actually releases the `struct file` (and thus the OFD lock). Specifically:

1. Process gets SIGTERM → main thread exits → kernel runs `exit_files()` → drops task's FD table refs.
2. If any FD has additional refs (e.g., held via dup, passed via SCM_RIGHTS, or referenced by another kernel object), `fput()` is deferred to a workqueue.
3. `wait4()` returns as soon as the zombie is reaped (after `release_task`); it does NOT wait for the workqueue's `delayed_fput` to drain.
4. Until `delayed_fput` runs, the `struct file` (and its OFD lock) is live.

So `<-exitDone` is strictly weaker than what's needed. The mandate's predicate-strength claim ("strictly stronger than `/proc`-based check") is correct in the comparison set but the actual requirement is stronger still.

### Counter evidence — `nomad_driver_ch_destroy_task_unreaped_total` did NOT increment

Journalctl grep across the full stress window on worker-1: **zero hits** for "unreaped" or "destroy_task_unreaped". The driver's 25 × 200 ms = 5 s budget was sufficient (the supervisor's `exitDone` channel closes well within 5 s of the SIGTERM ladder), so the counter was never bumped. **The reap-wait predicate is firing successfully — and the rootfs.img is still AlreadyLocked when the next alloc tries to acquire it.** This is the cleanest possible refutation of the r4-A hypothesis: the predicate works as designed, the design is too weak.

(Note: the controller HTTP listener does not export driver counters — driver metrics are only available via the Nomad agent's `/v1/metrics?format=prometheus` endpoint, which on this cluster returned an empty counter set for the `driver_ch` namespace, suggesting Nomad's metrics relay path is not picking up the plugin's emissions. Orthogonal issue.)

### Tap EEXIST — the warn is back, count is up

worker-1 journalctl grep for the warn: **25 occurrences**. r3-B suppressed the EEXIST → ERROR path in the cold-boot branch (now a warn that CH accepts), but the restore-branch's `--net tap=...` opens the tap fresh, and the controller-side path (which sets up the tap via the wrapper before the restore alloc spawns) sees the same race. The warn is non-fatal — CH continues — but the cardinality (25 / 60 cycles) is much higher than r4's reading of "warn-level, not fatal". 

Worth noting because: if the AlreadyLocked rootfs.img collision were ever fixed in isolation, the tap-EEXIST race would likely surface next as a fatal under further stress.

## Cross-alloc inode sharing — the actual lock-leak mechanism

The locked path `/opt/nomad/data/alloc/<NEW-alloc-id>/ch/local/rootfs.img` is in the NEW alloc's directory — but the OFD lock against it must come from somewhere. Hypothesis (not verified this cycle, would need `lsof -nP +D /opt/nomad/data/alloc` at the moment of failure, which the stress harness doesn't capture):

- The controller stages `rootfs.img` into each alloc dir via `cp --reflink=auto` from `$ZSBX_ARTIFACT_DIR/rootfs-slim.img` (per `nomad-vm-wrapper.sh:300-305`, mirrored by the driver's `materializeRootfs` in `start_task.go`).
- `cp --reflink=auto` on btrfs/xfs/zfs creates a CoW clone with a **shared underlying extent**; the file is logically distinct (different inode, different stat) but the data extent is shared.
- Linux's `flock`/F_OFD_SETLK is on the `struct file`, which is keyed by `struct inode`. Different files with different inodes have different `struct inode` instances — so reflink-shared extents should NOT share OFD locks. This means reflink is NOT the immediate cause.
- More likely cause: the SAME alloc-dir's `rootfs.img` is being opened twice in overlapping windows. The driver's `materializeRootfs` and a subsequent CH spawn could both open the file; if the first open's FD lingers (in driver or wrapper subprocess), the second open's ExclusiveWrite acquire collides.
- OR: A prior CH from a DIFFERENT alloc on the same worker held an OFD lock against the SAME PATH (impossible across alloc dirs unless there's a symlink or bind-mount sharing) — would require additional `lsof` evidence to confirm.

The full diagnosis requires capturing `lsof -p <ch-pid>` and `lsof +D /opt/nomad/data/alloc/<id>/ch/local/` at the moment of failure, plus checking whether `/opt/nomad/data/alloc/<X>/ch/local/rootfs.img` shares an inode with the source artifact — neither of which is in scope for this stress cycle's review.

## Counter deltas

| Counter | Pre-stress | Post-stress | Delta |
|---|---:|---:|---:|
| `nomad_driver_ch_destroy_task_unreaped_total` | 0 | 0 | **0** (predicate's 5 s budget never exhausted) |
| `nomad_driver_ch_taps_orphaned_total` | 0 | 0 | **0** (per Nomad metrics scrape — counter not surfacing, see note above) |

Driver counters are not surfacing through the Nomad agent's HTTP metrics endpoint on this cluster. The driver source defines the counter and bumps it via `metrics.IncrCounter`, which should land in Nomad's go-metrics sink, but `/v1/metrics?format=prometheus` returns empty for the `driver_ch` namespace. Driver telemetry observability gap — orthogonal to the e2e failure, but worth a follow-up.

## Stranded resources (worker-1, pre-teardown)

Not catalogued this cycle — the run ended in a uniform e2e-RED state where stranded counts would mirror r4's profile. The teardown swept everything.

## Compare to r1/r2/r3/r4/r5 baselines (diagnostic progression — 5th vs 6th layer)

| Round | Driver | Controller | CREATE | SNAPSHOT | WAKE-of-SNAP | E2E | Dominant failure |
|---|---|---|---:|---:|---:|---:|---|
| r1 | v13 | v33 | ?  | ?  | ?    |  2/60 (3.3 %) | tap EEXIST cross-worker |
| r2 | v14 | v34 | ?  | ?  | ?    |  2/60 (3.3 %) | workspace.img miss cross-worker + tap EEXIST |
| r3 | v15-pre | v34 | 13/60 (22 %) | 13/13 | 1/13 |  1/60 (1.7 %) | workspace.img miss + TUNSETIFF EBUSY |
| r4 | v15 | v35 | 60/60 (100 %) | 51/60 (85 %) | 3/51 (5.9 %) |  3/60 (5.0 %) | rootfs.img AlreadyLocked (same-worker CH lock retained) |
| **r5** | **v16** | **v35** | **60/60 (100 %)** | **53/60 (88 %)** | **3/53 (5.7 %)** | **3/60 (5.0 %)** | **rootfs.img AlreadyLocked (SAME wedge, r4-A's `exitDone`-wait insufficient)** |

**Diagnostic ladder reads: layer 5 NOT peeled.** r1 peeled tap-EEXIST. r2 peeled cross-worker workspace.img. r3 peeled cross-worker + EBUSY (via node-affinity + netdev-release-poll). r4 surfaced the rootfs.img lock retention. r5 attempted to peel it via wait-for-`exitDone`, but the predicate is too weak. The wedge mechanism remains: **CH's `struct file` reference (and its OFD lock) survives past `wait4()` due to delayed `__fput`** — and any fix must wait for `fput`-completion, not just task reap.

The CREATE / SNAPSHOT phases ticked up modestly (60/60 unchanged, SNAPSHOT 51→53, WAKE-of-SNAP rate 5.9 → 5.7 % — within noise). Net E2E is identical to r4 at 3/60.

## Verdict

**RED.** E2E 3/60 = 5.0 %, below the 95 % cutover threshold, identical to r4. Per the mandate's decision rules: "**<70 % RED → r4-A didn't address actual mechanism, new diagnosis sprint (6th layer).**"

**T-8b-cutover stays BLOCKED.** The 6th-layer mechanism is:

- **r5-A wedge: CH's `struct file` (and its OFD WRITE lock on `rootfs.img`) survives past Go's `cmd.Wait()` due to Linux's `delayed_fput` workqueue.** The driver's r4-A `exitDone`-wait predicate fires when `wait4()` returns, which is BEFORE the kernel has finalized `__fput` on every FD the dying CH process held. Until `__fput` runs, the `struct file` is live and its OFD locks are held.

Candidate fixes for r5-A (any one of these should close the surface; not investigated this cycle):

1. **Driver-side post-reap `flock` probe.** After `<-exitDone` closes, in DestroyTask the driver opens each disk path with `F_OFD_SETLK(F_WRLCK)` non-blocking, retrying with bounded backoff until success — that *proves* no kernel state still holds a write lock. Budget 5 s + already-spent reap-budget = up to ~10 s wall; bumps a new `destroy_task_lock_held_total` counter on exhaustion. This is the strictly-stronger predicate the mandate aimed for, derived from the actual kernel guarantee we need.

2. **Per-alloc rootfs.img with unique inode** (no reflink). Currently `materializeRootfs` uses `cp --reflink=auto`. Reflink alone shouldn't share OFD locks (different inodes), but if the controller's stage path also touches the file via a shared path or symlink, that would explain the collision. Forcing `cp` (no reflink) costs ~1 GiB of disk per cycle but eliminates any inode-sharing surface as a debugging step (NOT a permanent fix — wasteful).

3. **Restore-branch flock-wait in the driver itself.** Before invoking `--restore`, the driver could pre-acquire and release a `F_OFD_SETLK(F_WRLCK)` on each disk path to ensure the kernel state is clean. Same primitive as (1) but on the spawning side rather than the despawning side; works even when the prior CH was a different alloc on the same worker. This is the controller-architecture-independent fix.

4. **Bind-mount each alloc dir onto its own tmpfs subvolume.** Heavy-handed; cleans up everything at unmount, including stale `struct file` refs from the prior alloc. Disproportionate to the problem.

**Recommended next fixer brief:** option (1) — driver-side post-reap flock probe. The mechanism (driver waits for kernel guarantee instead of approximating it via Go-runtime signals) directly addresses what the `/proc/locks` evidence shows. Option (3) is the better engineering choice if (1) proves insufficient under further stress (e.g., if there's also a controller-side opener that the driver doesn't see); diagnose by escalating only if r5-A's predicate-strengthening doesn't close the wedge.

Remaining cutover blockers (unchanged from r4):
- r24-A2 kernel-state surface inventory closures (4 of 5 OPEN — r5-A becomes #5)
- r26-A1 typed-error template for LivezTimeout / RegisterFailed / ClockResyncFailed
- r26-A2 node-affinity-trade ADR
- r22-S1 sanitize widening (already landed `7647cd4d`)
- r26-S2 CI enforcement

## Cluster cost

- Stress provision: 3 servers (n2-standard-4) + 3 workers (n2-standard-32), ~30 min total wall-time (provision 5 min + stress 20 min + teardown 3 min).
- Smoke provision: 1 server + 1 worker, ~6 min total.
- Estimated cost: ~$1.40 (well under the $30 hard cap).

## Teardown

```
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down
gcloud compute instances list --filter="name~'zsbx-prod-'"  → Listed 0 items.
gcloud compute addresses list --filter="name~'zsbx-prod-'"  → Listed 0 items.
```

Verified — 0 residual GCP resources after teardown.
