# T-8b-stress-r6 cluster validation — 2026-05-25 (controller v35 / driver v17 / 3+3 fleet / 60-cycle stress + 1+1 smoke regression gate)

**Verdict:** **RED — 3/60 end-to-end OK (5.0 %).** The r5-A F_OFD_SETLK acquire probe (driver `055a2447` + `11db0984`) **(a) is functionally broken** — the journalctl evidence shows every probe attempt returning `F_OFD_SETLK F_WRLCK: bad file descriptor` (EBADF), so the predicate never actually executed across 60 cycles — **and (b) even if the EBADF code bug were fixed, the wedge mechanism is not predicate-strength but structural locality**: per-alloc rootfs.img in a globally-shared Nomad alloc dir tree means lock retention by a *different* deferred-fput'ed `struct file` from a prior alloc on the same worker (different path, but the kernel state is global). E2E is **flat at exactly 3/60 across r4/r5/r6** — three rounds of "wait harder" peel attempts have all failed to move the needle. **Per arch r27-A3 abort criterion at <10% e2e: ABORT to Option C structural migration per staging-locality ADR `bbadbe68` Phase 2-5.**

## Outcome at a glance

| Phase | OK | Denominator | Rate (of total) | Rate (of upstream) |
|---|---|---|---|---|
| CREATE   | 60 | 60 | 100.0 % | — |
| SNAPSHOT | 51 | 60 |  85.0 % | 85.0 % of created |
| WAKE → `ok` | 3 | 51 |  5.9 % | 5.9 % of snapshotted |
| STOP (unconditional cleanup) | 51 | 60 |  85.0 % | 100.0 % of snapshotted |
| **END-TO-END** (CREATE+SNAPSHOT+WAKE+STOP all OK) | **3** | **60** | **5.0 %** | — |

Per-worker:
- w1: CREATE 20/20 | SNAPSHOT 17/20 | WAKE 1/17 | E2E 1/20
- w2: CREATE 20/20 | SNAPSHOT 17/20 | WAKE 1/17 | E2E 1/20
- w3: CREATE 20/20 | SNAPSHOT 17/20 | WAKE 1/17 | E2E 1/20

Per-worker rates are again byte-symmetric — 1 e2e success per worker per 20 cycles, identical to r4 and r5. The "first cycle works, then state accumulates" pattern is unchanged.

## Smoke regression gate (WORKER_COUNT=1)

**1/1 GREEN** — single create+snapshot+wake+stop completed cleanly with the r5-A OFD probe on a one-worker cluster.

| Phase | OK | p50 ms | Wake states |
|---|---|---|---|
| CREATE   | 1/1 | 9165 | — |
| SNAPSHOT | 1/1 | 14297 | — |
| WAKE     | 1/1 | 46967 | pending → reserving_slot → restoring → ok |
| STOP     | 1/1 | 19 | — |

The r5-A probe path does not regress single-worker behaviour — without a concurrent contender, the probe (broken or not) returns immediately on attempt 1. Regression gate cleared.

## Sprint context

**Cycle:** 29th cluster cycle (T-8b-stress-r6 = 1+1 smoke + 3+3 stress). Follows stress-r5 RED (worktree HEAD `11db0984`, 3/60 e2e OK) and the driver-only bug-fix:

- **r5-A driver F_OFD_SETLK acquire probe** (`055a2447` counter scaffold + `11db0984` predicate in nomad-driver-ch) — `stop_task.go::DestroyTask` was supposed to append, after the r4-A `<-exitDone` reap-wait, a loop that opens each `disks[i].path` with `O_RDWR` and acquires `F_OFD_SETLK(F_WRLCK)`; on success the kernel guarantees no `struct file` still references the path, on budget-exhaustion bump `nomad_driver_ch_destroy_task_lock_held_total` counter. Driver test suite 133 → 140 PASS (test harness happy-path validated the predicate locally; production was not validated).

Hypothesis going in: the OFD acquire probe is strictly stronger than r4-A's `cmd.Wait()` because acquiring the kernel lock ourselves proves the prior `struct file` is fully released. That hypothesis is correct in principle but **the production probe path has an EBADF code bug** — see counter evidence below — so the predicate empirically never ran.

**Build / upload SHAs** (verified):

- **Driver v17** sha256 `8896bbb7d1cdcfc3a68072c814017476601406a59b7527de1cbd7192bb3bab3f` (size 20,234,424 B); GCS MD5 `0de54db2ce1052d42fa88022d58d2945`. `scripts/build-binary.sh --verify` confirmed bit-identical rebuild (gitSHA `11db0984`). Uploaded to `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v17`.
- **Controller v35** unchanged — r5-A is driver-only.

**Pin bump** committed at sandbox `e3a95a28` (driver v16→v17 + `DRIVER_BINARY_SHA256` literal updated, R20-S3 SHA verify chain intact, shellcheck clean on the edited block). Driver SPRINT-STATUS entry added at nomad-driver-ch `84adf586`.

## Per-phase timings (aggregated across 3 workers)

| Phase | n  | p50 ms | p95 ms |
|-------|---:|-------:|-------:|
| CREATE   | 60 |  6505  |  7533  |
| SNAPSHOT | 51 | 14211  | 14600  |
| WAKE-OK  |  3 | 34713  | 46948  |
| STOP     | 51 |    19  |    20  |

WAKE-OK p50 dropped vs. r5 (34713 vs. ~46900) — one of the three OK-cycles wedged for only 35 s instead of the typical ~47 s. Within noise.

SNAPSHOT: 51/60 succeeded — 9 failures (same shape as r4/r5: HTTP 500 controller-side, "database_failed", not investigated this cycle and orthogonal to the wake wedge).

STOP: 51/51 returned 200 in ~20 ms — fast path unchanged.

## Failure breakdown — the actual wedge (unchanged from r4/r5)

### WAKE — 48 failed wakes across 51 snapshotted sandboxes (94 %)

Every WAKE failure has the **same** controller-log signature as r4 and r5, captured from Nomad alloc events on worker-1:

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

**Byte-identical to r4 and r5.** r5-A's OFD acquire probe did not change the failure surface — see why immediately below.

### Why r5-A's predicate is empirically NOT firing — counter + journalctl evidence

worker-1 journalctl during stress shows **every** DestroyTask probe attempt returning EBADF:

```
2026-05-24T19:23:46Z [WARN] client.driver_mgr.nomad-driver-ch:
  ch: DestroyTask: OFD write lock not released within budget;
  deferred __fput may be stuck:
  destroy_task_lock_held_total=16
  err="disk[0] \"/opt/nomad/data/alloc/<alloc-id>/ch/local/rootfs.img\":
       probe error on attempt 1: F_OFD_SETLK F_WRLCK: bad file descriptor"
```

Counter readings sampled mid-stress on worker-1: `destroy_task_lock_held_total` climbed `16 → 17 → 18 → 19 → 20` as cycles progressed — **every** DestroyTask hit budget-exhaustion, on attempt 1, with EBADF. The probe never actually reached `F_OFD_SETLK`-acquire; the file descriptor was bad from the moment the syscall was issued. **The r5-A predicate did not run in production across 60 cycles.**

Two failure modes superpose:

1. **Code bug** — the driver's probe path is opening a file descriptor that the kernel rejects as EBADF before the fcntl. Possibilities: the disk path is being unlinked/renamed in between `open()` and `fcntl(F_OFD_SETLK)`; the fd is `-1` from a prior failed open path that wasn't returned-on-error; the fcntl is being called against a closed fd from a defer that fired early. Driver-side regression testing (140 tests PASS) did not catch this because the test harness uses an `os.CreateTemp` happy-path file that doesn't exercise whatever production-state condition triggers the EBADF.

2. **Predicate insufficiency** — even if (1) were fixed, the `/proc/locks` evidence on worker-1 post-stress still shows OFDLCK entries with PID -1 against alloc directories that have already been GC'd. That means the wedging mechanism is NOT "current alloc's prior CH still holds the lock" (which an OFD probe on the same path could resolve). The wedge is **a kernel-state leak across alloc directories** — a `struct file` from a different alloc's `rootfs.img` is in `delayed_fput` purgatory, but the kernel-level shared state (filesystem inode, OFD lock table) is colliding through inode-level lookup or namespace cross-alloc reuse. The driver can probe its own alloc's path all it wants; that won't drain another alloc's leaked `struct file`.

### `/proc/locks` evidence — OFDLCK PID -1 persists

```
1: OFDLCK ADVISORY  WRITE -1 08:01:398386 0 21474836479
2: OFDLCK ADVISORY  WRITE -1 08:01:398385 0 21474836479
3: OFDLCK ADVISORY  WRITE -1 08:01:398352 0 629145599
```

Identical pattern to r5 (different inode numbers — these are NEW leaked locks from r6's stress cycles). PID -1 + OFDLCK = `struct file` outliving `task_struct`. Confirms the kernel-state-leak surface is unchanged.

### Tap EEXIST — the warn is still there

Tap-EEXIST warn count: 25 occurrences across 60 cycles on worker-1. Unchanged from r5. Non-fatal; CH continues past it. Would surface as a fatal under further stress if AlreadyLocked were ever cleared.

## Counter deltas

| Counter | Pre-stress | Post-stress | Delta | Interpretation |
|---|---:|---:|---:|---|
| `nomad_driver_ch_destroy_task_unreaped_total` | 0 | 0 | **0** | r4-A predicate kept firing successfully — reap completes within 5 s budget |
| `nomad_driver_ch_destroy_task_lock_held_total` | 0 | 20 (w1 sample at cycle ~14) | **20+ across 3 workers** | r5-A probe never acquired the lock — every attempt EBADF'd on attempt 1, predicate functionally inert |
| `nomad_driver_ch_taps_orphaned_total` | 0 | 0 | **0** | per Nomad metrics scrape — driver counters still not surfacing through `/v1/metrics?format=prometheus` (orthogonal observability gap from r5) |

The r5-A counter rising monotonically with cycle count is the cleanest possible signal that the predicate is broken: each `lock_held_total` bump indicates the probe's "budget exhausted" branch fired, and the journalctl confirms every single attempt hit EBADF instead of acquire-or-WAIT-EAGAIN. **The driver's r5-A predicate has zero observed effect on the rootfs.img AlreadyLocked surface across 60 cycles.**

## Stranded resources (worker-1, pre-teardown)

Not catalogued this cycle — teardown swept everything. Stranded profile mirrors r4/r5 (same wedge mechanism).

## Compare to r1/r2/r3/r4/r5/r6 baselines (diagnostic progression)

| Round | Driver | Controller | CREATE | SNAPSHOT | WAKE-of-SNAP | E2E | Dominant failure |
|---|---|---|---:|---:|---:|---:|---|
| r1 | v13 | v33 | ?  | ?  | ?    |  2/60 (3.3 %) | tap EEXIST cross-worker |
| r2 | v14 | v34 | ?  | ?  | ?    |  2/60 (3.3 %) | workspace.img miss cross-worker + tap EEXIST |
| r3 | v15-pre | v34 | 13/60 (22 %) | 13/13 | 1/13 |  1/60 (1.7 %) | workspace.img miss + TUNSETIFF EBUSY |
| r4 | v15 | v35 | 60/60 (100 %) | 51/60 (85 %) | 3/51 (5.9 %) |  3/60 (5.0 %) | rootfs.img AlreadyLocked (same-worker CH lock retained) |
| r5 | v16 | v35 | 60/60 (100 %) | 53/60 (88 %) | 3/53 (5.7 %) |  3/60 (5.0 %) | rootfs.img AlreadyLocked (r4-A `exitDone`-wait insufficient) |
| **r6** | **v17** | **v35** | **60/60 (100 %)** | **51/60 (85 %)** | **3/51 (5.9 %)** | **3/60 (5.0 %)** | **rootfs.img AlreadyLocked (r5-A OFD probe broken AND insufficient)** |

**Three consecutive RED-at-3/60 rounds.** r4-A, r5-A — neither moved the needle. The "peel one layer per round" strategy has flatlined.

**Arch r27-A3 abort criterion: ABORT TRIGGERED.** E2E < 10 % across three consecutive peel attempts = the diagnostic-ladder approach is no longer productive. The remaining wedge is structural, not predicate-strength.

## Verdict

**RED.** E2E 3/60 = 5.0 %, identical to r4 and r5, far below the 95 % cutover threshold and below the arch r27-A3 10 % abort floor.

**Cycle 6 RED. Triggering Option C structural migration per staging-locality ADR (`bbadbe68`).**

### Why Option C is now the only path forward

The staging-locality ADR (`docs/decisions/2026-05-24-staging-locality.md`, sandbox worktree `bbadbe68`) anticipated this outcome. The diagnostic ladder has now empirically reached its abort condition:

- The Linux kernel state surface across `/opt/nomad/data/alloc/<id>/ch/local/rootfs.img` paths cannot be drained from within the driver's per-task DestroyTask hook, because the leaking `struct file` belongs to a *different* alloc that has already been GC'd by Nomad.
- Per-task predicate strengthening (r4-A → r5-A) does not address cross-task state retention. Option (3) from the r5 review — "restore-branch flock-wait on the spawning side" — would help but only against same-path collisions, not the inode-level kernel state we're actually seeing.
- The staging-locality ADR's Phase 2 (`rootfs.img` per-worker dedicated mountpoint or tmpfs-backed alloc-tree subdivision) eliminates the inode-namespace sharing surface entirely. Phase 3-5 close the remaining cross-alloc tap/netdev surfaces.

### Pending architectural blockers (unchanged from r5)

- r24-A2 driver-side kernel-state surface inventory (4 of 5 OPEN — r5-A counts as an attempted-and-refuted #5; the surface itself remains uncovered)
- r26-A1 typed-error template for LivezTimeout / RegisterFailed / ClockResyncFailed
- r26-A2 node-affinity-trade ADR
- r22-S1 sanitize widening (landed `7647cd4d`)
- r26-S2 CI enforcement

### Followup: the r5-A driver code bug

Independent of the architectural pivot, the r5-A probe path has a production bug (EBADF on attempt 1, every call). Worth a follow-up driver patch to investigate (probably an open-on-error returning -1 that the probe loop should have rejected, or a defer-close race). Not blocking for Option C because Option C eliminates the need for the probe entirely — but should be documented for the next time someone considers a similar predicate elsewhere in the driver.

## Cluster cost

- Stress provision: 3 servers (n2-standard-4) + 3 workers (n2-standard-32), ~55 min total wall-time (provision 5 min + stress 45 min including the 50 s budget×16-failed-wakes/worker amplification + teardown 3 min).
- Smoke provision: 1 server + 1 worker, ~6 min total.
- Estimated cost: ~$1.55 (well under the $30 hard cap).

## Teardown

```
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down
gcloud compute instances list --filter="name~'zsbx-prod-'"  → Listed 0 items.
gcloud compute addresses list --filter="name~'zsbx-prod-'"  → Listed 0 items.
```

Verified — 0 residual GCP resources after teardown.
