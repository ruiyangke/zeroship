# T-8b-stress-r3 cluster validation — 2026-05-24 (controller v34 / driver v14 / 3+3 fleet / 60-cycle stress)

**Verdict:** **RED — 1/60 end-to-end OK (1.7%).** **WORSE than stress-r2** (2/60 = 3.3%) and roughly tied with stress-r1's regression baseline. The driver v14 + controller v34 bundle did NOT close either of the two known r2 bugs. CREATE remains dominated by `workspace.img does not exist` (47 of 47 classifiable failures — same Bug 1 from r2, NOT fixed by controller v34's host_dir lifecycle restructure). Driver v14's `taps_orphaned_total` counter is unreachable in this deployment (no driver-side /metrics endpoint exposed) so the defensive `DestroyTask` tap cleanup remains unmeasured. **T-8b-cutover stays BLOCKED.** The bundle agent's pre-flight prediction (CREATE 20 %→≥85 %, e2e ~70 %) was wildly optimistic; the actual movement was 20 % → 22 % on CREATE and 3.3 % → 1.7 % on e2e — net negative.

## Outcome at a glance

| Phase | OK | Denominator | Rate (of total) | Rate (of upstream) |
|---|---|---|---|---|
| CREATE   | 13 | 60 | 21.7 % | — |
| SNAPSHOT | 13 | 13 | 21.7 % | 100 % of created |
| WAKE → `ok` | 1 | 13 | 1.7 % | 7.7 % of snapshotted |
| STOP (unconditional cleanup) | 13 | 13 | 21.7 % | 100 % of created |
| **END-TO-END** (CREATE+SNAPSHOT+WAKE+STOP all OK) | **1** | **60** | **1.7 %** | — |

Per-worker:
- w1: CREATE  1/20 | SNAPSHOT  1/1  | WAKE 0/1  | E2E 0/20 (elapsed 176.5 s)
- w2: CREATE 10/20 | SNAPSHOT 10/10 | WAKE 1/10 | E2E 1/20 (elapsed 770.8 s — anomalous slow run)
- w3: CREATE  2/20 | SNAPSHOT  2/2  | WAKE 0/2  | E2E 0/20 (elapsed 233.5 s)

Worker-2's e2e success (1/20) is the entire green in the run. Workers 1 and 3 hit zero. Compared to r2's per-worker (w1 0/20, w2 1/20, w3 1/20) the run is in the same noise floor.

## Sprint context

**Cycle:** 26th cluster cycle (T-8b-stress-r3). Follows stress-r2 RED (commit `e6363fce` worktree state, 2/60 e2e OK) and the bug-fix bundle:

- **Driver v14** (`177ff165`) — `stop_task.go` DestroyTask path adds defensive `vm_index`-keyed tap cleanup so a tap leaked between `setupTapForVM` and `p.tasks` insertion is freed on Destroy. New driver-side prometheus counter `taps_orphaned_total`. Driver tests 127→130.
- **Controller v34** (`92c45d26`) — three commits bundled: `d638b10f` (host_dir leak-on-stop + verbatim driver-msg propagation), `e82bffd7` (host_dir GC sweeper task), `6b240683` (docker-build runbook). Sandbox tests 449→454.

**Build / upload SHAs** (verified):
- Driver v14 sha256 `6801fe9d6faf0890193b024ee3271e8c4db44b24f092d26c03dec5596654a2df` (size 20,218,040 B); GCS MD5 `79c487bc5ce707d437074081fc90c253`. `scripts/build-binary.sh --verify` bit-identical.
- Controller v34 sha256 `07e01c666d7662aaab0bff5012045ff7c696170731449d976840bd1def14fed2` (size 16,682,856 B); GCS MD5 `fae766b95b9475508e2902b133fc5a44`. Built under `rust:1.94-bookworm` docker per `crates/sandbox/README.md` "Recipe: docker-build for Debian-12 compat"; `file` reports `interpreter /lib64/ld-linux-x86-64.so.2` (max GLIBC_2.34) — Debian-12 compatible.

Pin bumps committed at sandbox `c729c2b8` (driver v13→v14 + controller v33→v34); driver SPRINT-STATUS at nomad-driver-ch `e86b586b`.

## Per-phase timings

| Phase | n  | p50    | p95    | p99    | max    |
|-------|---:|-------:|-------:|-------:|-------:|
| CREATE   | 13 |  6411  |  8070  |  8070  |  8070  |
| SNAPSHOT | 13 | 14223  | 14711  | 14711  | 14711  |
| WAKE     |  1 | 45950  | 45950  | 45950  | 45950  |
| STOP     | 13 |    19  |    20  |    20  |    20  |

Times are ms. CREATE p50 6.4 s and SNAPSHOT p50 14.2 s are consistent with r2's smoke profile — no thrash or contention; the issue is failure rate, not latency. STOP at p50 19 ms is the local-cleanup fast path; the controller is not blocking on driver-side teardown.

## Failure breakdown

### CREATE — 47 failures, single class

```
 47  nomad ch unhealthy (no driver-msg propagation)
```

All 47 CREATE failures arrived at the harness as the generic 500 envelope:

```json
{"error":"backend_create_failed",
 "message":"backend.create: nomad alloc terminal status=failed: Failed tasks: ch: Unhealthy because of failed task"}
```

…or the trailing-state variant `Policy allows no restarts`. **This is the regression**: controller v34 commit `d638b10f` was titled "verbatim driver-msg propagation" but the actual error returned to clients is still the nomad alloc-status string, not the driver's `StartTask: ...` exception message. The driver's actual rejection reason is only recoverable from `journalctl -u nomad` on the worker.

Driver-side per-worker (from `journalctl -u nomad | grep StartTask` aggregated):

| Worker | `workspace.img does not exist` | `setup tap … (after collision-replace): TUNSETIFF EBUSY` | Other |
|---|---:|---:|---:|
| w1 | 24 | 2 | 0 |
| w2 | (truncated in tail; ≥10) | 0 | 0 |
| w3 | 24 | 0 | 0 |

Across all workers, **`workspace.img does not exist (controller must stage before spawn)`** is the dominant driver-side failure — **the SAME r2 Bug 1 message**, unchanged.

w1 also saw 2 instances of the new failure: `setup tap for vm_index=1: ip tuntap add zsbx-nm-1 (after collision-replace): exit status 1 (output="ioctl(TUNSETIFF): Device or resource busy\n")`. This is the v13 EEXIST pre-delete path racing: the delete returns, the immediate re-add hits `TUNSETIFF EBUSY` because the kernel hasn't fully released the tun device. **v13's net-package patch is partially regressed by the race**; driver v14's DestroyTask cleanup doesn't address this because the failure is happening during StartTask, not after.

### WAKE — 12 failures

```
 11  restore_backend_failed: nomad alloc terminal status=failed:
        Failed tasks: ch: Unhealthy because of failed task
  1   livez_timeout: agent at [redacted] never returned 200 on /livez
```

11 of 12 wake failures are the same restore-side incarnation of CREATE Bug 1: when the restore alloc is dispatched, the driver hits `workspace.img does not exist` (or the tap-collision EBUSY path) and fails the alloc; the controller propagates the generic nomad-alloc status. 1 livez timeout indicates a wake where the alloc started, CH came up, but the agent never responded — a different failure surface (probably correlated with the wake_total_ms=45.95 s pulse from w2's only success — the agent is slow).

## Counter deltas / stranded resources

| Resource | Worker-1 | Worker-2 | Worker-3 |
|---|---:|---:|---:|
| host_dirs in `/var/zeroship/ch/` (end of run) | 22 | 17+ | 17+ |
| `workspace.img` files | 20 | (n/a, truncated) | (n/a, truncated) |
| Taps present (`zsbx-nm-N`) | 10 (nm-3..12) | 12 (nm-1..12) | 9 (nm-4..12) |

Notes:
- **Host_dir leak confirmed**: 22 dirs on w1 against 1 successful CREATE — controller v34's GC sweeper (`e82bffd7`) is either not running or hasn't ticked through one of its intervals yet (run was ~5 min on w1; sweeper period likely longer). The 20 workspace.img files indicate the dir creation step succeeded for 20 of 20 attempts, even though the driver later reported it missing — strongly suggesting a **TOCTOU between controller staging and driver StartTask** (or between sweeper unlink and StartTask), not a controller-never-staged bug.
- **Taps**: Workers boot with `zsbx-nm-1..12` pre-created in the startup script (one per VM_INDEX_CEIL). The teardown subset shown (nm-3..12 on w1) reflects which were never re-deleted; the absent ones (nm-1, nm-2 on w1) were the active CH allocs' taps at the moment of inspection.
- **`taps_orphaned_total` (driver v14 new counter)**: **NOT MEASURED** — the driver does not expose its own `/metrics` endpoint in this deployment. Controller `/metrics` (port 9091) does not surface driver-internal counters either. Confirming this counter requires either a sidecar prom-exporter on the driver, or harvesting via the nomad-agent metrics endpoint, neither of which is wired up. **This is a gap in driver v14's observability story**: the counter exists in the binary but is not externally readable.
- **`vm_index_leak` / `terminal_overwrite_blocked` / `takeover_claims`**: same — not surfaced anywhere a `curl /metrics` can reach. The defensive counters added across recent driver/controller versions are diagnostically inert until a metrics-exposure pass lands.

## What the bundle DID NOT fix

| Bug from r2 | Bundle target | Outcome |
|---|---|---|
| Bug 1: `workspace.img does not exist` (47/60 CREATEs in r2) | Controller v34 host_dir lifecycle restructured to sweeper-owned + `e82bffd7` sweeper task | **NOT fixed.** Now 47/47 classifiable failures still hit Bug 1. The lifecycle restructure changed who owns the dir but the controller→driver staging handshake is still racing (controller releases the alloc before the workspace.img write is durable enough for the driver to see it, OR the driver opens the path before the controller's `mkfs` returns). |
| Bug 2: `Tap zsbx-nm-N already exists` (3 occurrences in r2) | Driver v14 `DestroyTask` defensive vm_index-keyed cleanup + `taps_orphaned_total` counter | **Regressed into a new failure shape.** Bug 2's literal "already exists" path is gone; in its place a new race: after v13's `net`-package pre-delete, the immediate re-add hits `ioctl(TUNSETIFF): Device or resource busy` (EBUSY, not EEXIST) because the kernel hasn't dropped the tun device's exclusive lock yet. v14's DestroyTask cleanup is the wrong layer — the failure is in StartTask, not after. |
| Generic error envelope (`Unhealthy because of failed task` vs the driver's actual reason) | Controller v34 `d638b10f` "verbatim driver-msg propagation" | **NOT working.** All 47 CREATE failures still return only the generic alloc-status envelope. Either the propagation code path isn't being entered, or it's overwritten downstream. |

## Comparison vs r1/r2 baseline

| Metric | r1 | r2 | r3 |
|---|---:|---:|---:|
| CREATE OK | 2/60 (3.3%) | 12/60 (20%) | 13/60 (21.7%) |
| SNAPSHOT OK (of created) | 2/2 (100%) | 12/12 (100%) | 13/13 (100%) |
| WAKE OK (of snapshotted) | 1/2 (50%) | 2/12 (16.7%) | 1/13 (7.7%) |
| **E2E OK** | **2/60 (3.3%)** | **2/60 (3.3%)** | **1/60 (1.7%)** |
| Dominant CREATE failure | workspace.img | workspace.img (Bug 1) | workspace.img (Bug 1) |
| Notable secondary | Tap EEXIST | Tap EEXIST (3×) | Tap TUNSETIFF EBUSY (2×, new) |

**The headline**: three consecutive stress runs at the same 3+3 × 20 shape have landed at 2/60, 2/60, 1/60 e2e OK. The bug surface has shifted (EEXIST → EBUSY for tap), the controller-side observability hasn't actually moved (verbatim-msg-propagation isn't surfacing), and Bug 1 is unchanged. **No durable progress** in three rounds.

## Cost

- Cluster on for ~25 minutes wall (provision 08:34→08:41, stress 08:48→09:01, teardown 09:04→09:08).
- 3× n2-standard-4 + 3× n2-standard-32 in asia-northeast3 for 25 min ≈ **$0.85** (well under the $1.50 expected / $30 cap).

## Teardown verification

```
$ gcloud compute instances list --filter="name~'zsbx-prod-'"
Listed 0 items.
$ gcloud compute addresses list --filter="name~'zsbx-prod-'"
Listed 0 items.
```

Clean.

## Verdict and next-step recommendation

**VERDICT: RED.** 1/60 e2e OK (1.7 %), worse than r2.

**Decision: T-8b-cutover REMAINS BLOCKED.** The functional gate (≥95 % e2e) is not even approached; it's regressed.

### New failure modes (vs r2)

1. **`ioctl(TUNSETIFF): Device or resource busy` after collision-replace** (tap re-add race) — new on w1 (2× this run). The v13 pre-delete + immediate re-add doesn't wait for the kernel to release the tun device. **Fix shape**: a bounded poll loop on `ip link show <tap>` after delete before re-add, OR switch to a fresh vm_index rather than reusing the colliding one.

2. **Controller-side "verbatim driver-msg propagation" is silently a no-op** — `d638b10f`'s test cases passed (sandbox tests 449→454) but the production path doesn't enter the propagation branch. **Fix shape**: trace the controller's `backend_create_failed` body construction; the alloc-status string is being preferred over the driver-side `task.LastEvent.DisplayMessage`.

3. **Driver `taps_orphaned_total` counter is unreachable** — v14 added the metric but there's no transport. **Fix shape**: either expose driver metrics through nomad's agent fingerprint, or ship a tiny sidecar HTTP /metrics endpoint inside the plugin process.

### Recommended next-fixer brief

The next sprint needs THREE simultaneous fixes (in priority order):

- **r3-A: Bug 1 root-cause** — `workspace.img does not exist` at StartTask is the dominant failure across all three stress runs. The controller v33 `fsync_dir` + v34 sweeper-owned-lifecycle did NOT close it. The TOCTOU candidate space: (a) controller releases the alloc before `mkfs.ext4` syncs the inode; (b) the sweeper unlinks the dir between controller stage and driver open; (c) the driver opens at a path the controller didn't actually mkdir. Pick the actual cause by adding `strace -f -e openat,unlinkat,renameat -p <nomad-pid>` on a worker and re-running stress for 5 cycles. (Pure diagnostic, no fix shipped, ~$0.20 cluster cost.)
- **r3-B: Tap collision re-add race** — switch the v13 net-package pre-delete path from `del + immediate re-add` to `del + poll-for-disappear + re-add`, or sidestep by allocating a fresh vm_index on collision. (Code-only; driver v15.)
- **r3-C: Driver-msg propagation regression** — controller v34's `d638b10f` titled this intent but doesn't deliver it. Verify the controller's error-envelope construction reads from `Task.LastEvent.DisplayMessage`, not `Alloc.ClientStatus`. (Code-only; controller v35.)

All three fixes block T-8b-cutover. R3-A is the highest-value (47/60 of the entire CREATE failure surface) and the hardest (TOCTOU diagnosis); R3-B and R3-C are mechanical.

### Architectural blockers (unchanged)

Independent of stress GREEN, T-8b-cutover also requires:
- **r24-A1**: typed `StagingManifest` (replaces the loose dir + image-file convention with a typed contract)
- **r24-A2**: kernel-state audit (whether the controller is leaving kernel-namespace residue)
- **r24-A3**: observability-before-arch ADR
- **R20-S3**: driver SHA256 verify in `gcp-worker-startup.sh`

These were already gating; stress RED doesn't change them but also doesn't move them.
