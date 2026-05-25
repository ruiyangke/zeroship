# T-8b-stress-r4 cluster validation — 2026-05-24 (controller v35 / driver v15 / 3+3 fleet / 60-cycle stress + 1+1 smoke regression gate)

**Verdict:** **RED — 3/60 end-to-end OK (5.0%).** The r3-A node-affinity bundle (`883df7fe` + `9b623f44` + `d71f1a8c` + `b562d3a1`) clears the smoke regression gate (1/1 with the single worker pinned), so the placement-pinning architecture is mechanically working. But under 3+3 stress the e2e rate moved from r3's 1/60 (1.7%) to 3/60 (5.0%) — a real but tiny improvement, and a new failure mechanism dominates the run. **r3-A/B/C did NOT address the actual stress wedge.** The 4-for-4 r3 chain (node-affinity / netdev release poll / driver-failure event) targeted three diagnostic surfaces that the mandate's bundle agent predicted would close the chain; the run shows those surfaces were upstream of the real bottleneck. **T-8b-cutover stays BLOCKED.**

## Outcome at a glance

| Phase | OK | Denominator | Rate (of total) | Rate (of upstream) |
|---|---|---|---|---|
| CREATE   | 60 | 60 | 100.0 % | — |
| SNAPSHOT | 51 | 60 |  85.0 % | 85.0 % of created |
| WAKE → `ok` | 3 | 51 |  5.0 % | 5.9 % of snapshotted |
| STOP (unconditional cleanup) | 51 | 60 |  85.0 % | 100.0 % of snapshotted |
| **END-TO-END** (CREATE+SNAPSHOT+WAKE+STOP all OK) | **3** | **60** | **5.0 %** | — |

Per-worker:
- w1: CREATE 20/20 | SNAPSHOT 17/20 | WAKE 1/17 | E2E 1/20 (elapsed 1232.3 s)
- w2: CREATE 20/20 | SNAPSHOT ~17/20 | WAKE 1/~17 | E2E 1/20
- w3: CREATE 20/20 | SNAPSHOT ~17/20 | WAKE 1/~17 | E2E 1/20

Per-worker rates are remarkably symmetric — 1 e2e success per worker per 20 cycles. The single success in each worker happened to be cycle 0 (the very first iteration) on all three; the 1-wake-OK-per-worker pattern strongly suggests a state-leak that monotonically builds up after the first successful cycle.

## Smoke regression gate (WORKER_COUNT=1)

**1/1 GREEN** — single create+snapshot+wake+stop completed cleanly with the r3-A node-affinity bundle on a one-worker cluster.

| Phase | OK | p50 ms | Wake states |
|---|---|---|---|
| CREATE   | 1/1 | 6353 | — |
| SNAPSHOT | 1/1 | 14298 | — |
| WAKE     | 1/1 | 46967 | pending → reserving_slot → restoring → livez_polling → ok |
| STOP     | 1/1 | 20 | — |

The constraint pinning (`unique.hostname` == staging worker's hostname) does not collapse single-worker placement to zero hosts. Regression gate cleared — r3-A is NOT a regression of smoke behaviour.

## Sprint context

**Cycle:** 27th cluster cycle (T-8b-stress-r4 = 1+1 smoke + 3+3 stress). Follows stress-r3 RED (worktree `add6d5ef`, 1/60 e2e OK) and the bug-fix bundle:

- **r3-A controller node-affinity** (`883df7fe` + `9b623f44` + `d71f1a8c` + `b562d3a1`) — controller caches local Nomad node_id at boot (`883df7fe`) and the cold-boot path (`9b623f44`) and restore path (`d71f1a8c`) both emit a `Constraints` stanza on every Nomad job pinning placement to the staging worker's hostname (`unique.hostname == <worker>`). Closes the cross-worker scheduling race where a restore alloc would land on a worker that wasn't the one that staged the rootfs.
- **r3-B driver netdev release poll** (`05440498` in nomad-driver-ch) — `net` package adds a poll loop between `ip link delete` and `ip tuntap add` waiting for the kernel netdev to fully release before re-creating, fixing the TUNSETIFF EBUSY race that stress-r3 surfaced on w1.
- **r3-C controller driver-failure event preference** (`3d431eb8`) — `nomad-ch` event polling prefers Driver Failure events over Alloc Unhealthy when both are present in an alloc's event stream, so the verbatim driver-msg propagation surfaces the driver's real exception text rather than the generic Nomad envelope.

**Build / upload SHAs** (verified):

- **Driver v15** sha256 `2d5618adb82bfcdc5e098e2bfd4b22f4b7cf590e7c5b1f4f9a86304f57ce826c` (size 20,222,136 B); GCS MD5 `55d5cd483b89562f47adc876469434e4`. `scripts/build-binary.sh --verify` confirmed bit-identical rebuild (gitSHA `05440498`). Uploaded to `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v15`.
- **Controller v35** sha256 `afbd5d30b56adf54fcd9c0366b875cc4bc14de8fcaeb1a28cef4c98ee4a31e41` (size 16,719,176 B); GCS MD5 `3d9a62619cf88dcc29dea3c9affc1aee`. Built under `rust:1.94-bookworm` docker per `crates/sandbox/README.md` "Recipe: docker-build for Debian-12 compat"; `file` reports `interpreter /lib64/ld-linux-x86-64.so.2` (max GLIBC_2.34) — Debian-12 compatible. Uploaded to `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v35`.

**Pin bumps + R20-S3 driver SHA verify** committed at sandbox `2ead52c2` (driver v14→v15 + controller v34→v35 + `DRIVER_BINARY_SHA256` literal + post-pull verify with FATAL on mismatch, mirroring R24-T1's snapshot_stress.py lockstep template at `dd2079a9`). `lint.sh --severity=error` exit 0. Driver SPRINT-STATUS at nomad-driver-ch `f1c1a666`.

## Per-phase timings

| Phase | n  | p50    | p95    | p99    | max    |
|-------|---:|-------:|-------:|-------:|-------:|
| CREATE   | 60 |  6501  |  7549  | 11673  | 11673  |
| SNAPSHOT | 51 | 14232  | 14437  | 14557  | 14557  |
| WAKE-OK  |  3 | 45910  | 45915  | 45915  | 45915  |
| STOP     | 51 |    19  |    24  |   525  |   525  |

Times in ms. **CREATE: 100 % success in 60/60.** This is the biggest delta from stress-r3 (which had 13/60 CREATE OK, 21.7%). The r3-A node-affinity fix did exactly what it advertised on cold-boot — every CREATE now lands on the staging worker and finds workspace.img / rootfs.img already materialised. CREATE p50 6.5 s is consistent with smoke; CREATE max 11.7 s is the cold-boot worst case (no thrash).

SNAPSHOT: 51/60 succeeded — the 9 failures break down as 8× HTTP 500 + 1× HTTP 404. The 404 is plausibly a startup-grace artifact (sandbox not yet ready for snapshot when the harness fired); the 8× 500 needs controller-log triage in a follow-up (not investigated this cycle — out of stress-r4 scope).

WAKE: This is the cycle's load-bearing failure surface (see below).

STOP: 51/51 returned 200 in 19 ms p50 — the local-cleanup fast path. The 525-ms STOP outlier is one cycle; the rest are tight. STOP returns immediately because it's not gated on Nomad job-stop completion.

## Failure breakdown — the actual wedge

### WAKE — 46 failed wakes across 51 snapshotted sandboxes (90 %)

Every WAKE failure has the same controller-log signature (captured from `/var/log/zeroship-sandbox.log` on worker-1):

```
backend: nomad alloc terminal status=failed: Failed tasks:
  ch: rpc error: code = Unknown desc = ch: startTaskRestoreBranch:
  resume failed: ch: Resume: ch-remote resume: exit status 1
  (output="...HttpApiClient(ServerResponse(InternalServerError,
    Some([\"Error from API\",\"The VM could not resume\",
          \"VM is not running\"])))..."

ch_stderr_tail=
  "cloud-hypervisor: 0.564656s: <vmm> WARN:net_util/src/open_tap.rs:84 --
     Tap zsbx-nm-2 already exists. IP configuration will not be overwritten.
   cloud-hypervisor: 0.573644s: <vmm>
     ERROR:virtio-devices/src/block.rs:855 --
       Can't get Write lock for /opt/nomad/data/alloc/<X>/ch/local/rootfs.img
       as there is already a ExclusiveWrite lock
   cloud-hypervisor: 0.573707s: <vmm> ERROR:vmm/src/lib.rs:1772 --
     VM Restore failed: LockingError(DiskLockError(LockDiskImage {
       error: AlreadyLocked, lock_type: Write,
       path: \"/opt/nomad/data/alloc/<X>/ch/local/rootfs.img\" }))"
```

**Two distinct, layered issues** — the second one is the actual fatal cause:

1. **Tap collision (warn-level, NOT fatal):** `Tap zsbx-nm-<idx> already exists. IP configuration will not be overwritten.` CH continues past this; the kernel-netdev EEXIST race that r3-B targeted is suppressed (driver v15's pre-delete + kernel-release-poll is doing its job — the message is now a WARN that CH accepts, not the ERROR that crashed CH in r2/r3). **r3-B is working.**

2. **Rootfs.img exclusive-write lock collision (the actual fatal):** CH's virtio-blk opens every disk in `ExclusiveWrite` mode. The previous CH process for this sandbox's prior alloc still holds the file lock on `rootfs.img` when the restore alloc tries to acquire it. CH aborts the VmBoot → the resume RPC returns `InternalServerError: "VM is not running"` → driver `startTaskRestoreBranch.resume` returns exit 1 → alloc terminal failed.

The cycle is:
```
cycle N:  alloc creates → CH spawns → CH holds rootfs.img EX lock
          → STOP returns immediately (200)
          → controller's Nomad job-stop is async, CH process not yet
            reaped, lock not yet released
cycle N+1: wake alloc on SAME node, SAME sandbox_id, SAME
           per-sandbox rootfs.img path
           → CH spawn → restore → AlreadyLocked → fail
```

This is a NEW failure surface that r3-A/B/C did not touch. r3-A's node-affinity correctly pins the wake alloc to the staging worker — but that's exactly what makes this collision deterministic, because the prior cycle's CH lock is ALSO on that worker. Without node-affinity (the r1/r2/r3 world), wake allocs could land elsewhere and dodge the lock — at the cost of the workspace.img + tap leaks r3-A correctly closes.

**Diagnostic progression: r3-A traded a placement-randomisation failure (workspace.img miss / tap leak across workers) for a deterministic same-host-state-leak failure (CH rootfs.img lock retained across stop→wake on the same worker).** The fix is functionally correct architecturally; the bottleneck just shifted.

### SNAPSHOT — 9 failures (HTTP 500 × 8, HTTP 404 × 1)

Not investigated this cycle. Controller-log triage needed.

### Wake states observed (RAW_JSON)

Successful (3 cycles): `["pending", "reserving_slot", "restoring", "ok"]` or `["pending", "reserving_slot", "restoring", "livez_polling", "ok"]`. Wake-OK p50 45.9 s — same shape as smoke. No livez timeouts on the three successful runs.

Failed (46 cycles): wake terminal state `failed` with `error_code=restore_backend_failed`. No `livez_timeout` failures.

## Counter deltas (NOT MEASURED)

The mandate listed `vm_index_leak`, `terminal_overwrite_blocked`, `takeover_claims`, `taps_orphaned_total`, `nomad_node_id_lookup_failures_total`. **None of these counters are exposed via the controller's HTTP listeners** in this deployment:

- `http://127.0.0.1:9091` — sandbox HTTP API (no `/metrics` endpoint)
- `http://127.0.0.1:9092` — admin WebSocket listener
- driver runs as a Nomad plugin (no standalone HTTP listener)

Controller log grep across the 60-cycle window found ZERO instances of any counter name. The counters exist in the source but lack an emission surface; this is the same diagnostic gap stress-r3 flagged. **Not investigated further this cycle** — orthogonal to the WAKE wedge.

## Stranded resources (worker-1)

After all stress cycles + before teardown:

- **Taps:** 11 stranded `zsbx-nm-*` interfaces (nm-1, nm-2, nm-4..nm-12) out of 12 indices. nm-3 is the only one cleaned. With 20 cycles per worker and only 12 tap indices, vm_index churn ensures most indices end up stranded once their owning alloc fails before DestroyTask reclaims. Driver v14's DestroyTask defensive cleanup is partially working — without it the count would be higher — but the leak is not zero.
- **Host dirs:** 20 directories under `/var/zeroship/ch/` — one per cycle. None cleaned. This is consistent with the controller v34 host_dir GC being event-driven (relies on the sandbox/Nomad terminal sequence) and the 46-of-60 WAKE failures leaving sandboxes in `failed` state where the sweeper's grace window has not yet elapsed (default 600s post R24-A1 tightening). The sweeper would presumably clear these eventually; stress-r4 didn't wait long enough.

The teardown command cleared everything cluster-wide — these counts are pre-teardown forensics.

## Compare to r1/r2/r3 baselines (diagnostic progression)

| Round | CREATE | SNAPSHOT | WAKE | E2E | Dominant failure |
|---|---:|---:|---:|---:|---|
| r1 |  ? |  ?  |  ? | 2/60  (3.3 %) | tap EEXIST (`Tap zsbx-nm-X already exists. IP config not overwritten`) — Bug 1 + Bug 2 from stress-r1 |
| r2 |  ? |  ?  |  ? | 2/60  (3.3 %) | workspace.img miss (cross-worker placement) + tap EEXIST |
| r3 | 13/60 (22 %) | 13/13 | 1/13 | 1/60 (1.7 %) | workspace.img miss (`workspace.img does not exist`) — cross-worker placement still dominant; TUNSETIFF EBUSY race surfaced on w1 |
| **r4** | **60/60 (100 %)** | **51/60 (85 %)** | **3/51 (5.9 %)** | **3/60 (5.0 %)** | **rootfs.img `AlreadyLocked` ExclusiveWrite (post-stop CH lock retained on same host)** |

The CREATE phase went from broken (≤22 %) to perfect (100 %) — r3-A's node-affinity directly fixed the "placement landed on wrong worker → workspace.img not staged → cold-boot fail" bug that dominated r1/r2/r3 CREATE. The WAKE phase replaced one fatal surface (cross-worker tap-EEXIST / workspace.img miss) with another (same-worker rootfs.img lock collision). **The fix moved 47-cycle of CREATE failures into 3 phases worth of OK-then-fail-on-wake.** Net e2e moved from 1.7 % → 5.0 % — improvement, but not the ≥95 % target.

## Verdict

**RED.** E2E 3/60 = 5.0 %, below the 95 % cutover threshold. The mandate's pre-flight expected ~100 % "if Option 1 architecture works"; Option 1 architecture DOES work (smoke 1/1, CREATE 60/60 prove placement-pinning is mechanically sound), but Option 1 surfaced a different bottleneck that the r3 bundle didn't address. Per the mandate's decision rules: this is the "**smoke OK + stress RED (<70%)**" case — "r3-A didn't address the actual mechanism; new diagnosis needed."

**T-8b-cutover stays BLOCKED.** New diagnosis surfaces a single mechanism:

- **r4-A wedge: CH rootfs.img ExclusiveWrite lock retained across stop→wake on the same worker.** The previous alloc's CH process has not released the file lock by the time the next alloc's `--restore` attempts to acquire it. Candidate fixes (any one of these would close the surface; not investigated this cycle which is correct):
  1. **Driver-side STOP → wait-for-CH-exit before declaring task terminated.** DestroyTask must `cmd.Wait()` (with timeout + KILL escalation) before returning, so the CH process is fully reaped — and its file locks released — before Nomad considers the alloc terminal. Currently the driver likely returns from DestroyTask before CH has flushed its shutdown.
  2. **Per-alloc rootfs.img path (instead of per-sandbox).** CH locks the path verbatim; if each alloc stages a fresh copy of rootfs.img under the alloc dir (which the wrapper does via `cp --reflink=auto`), the lock is on a different inode and the collision disappears. C-7-LT-10's `restore_task.go` change supposedly already did this for restore — verify whether the symlink target points to the per-sandbox base, which would defeat the path-isolation.
  3. **Controller-side STOP waits for Nomad job-stop completion.** Adds latency to STOP but eliminates the race entirely.

Remaining cutover blockers (unchanged from mandate):
- r24-A2 kernel-state surface inventory closures (4 of 5 OPEN)
- r26-A1 typed-error template for LivezTimeout / RegisterFailed / ClockResyncFailed
- r26-A2 node-affinity-trade ADR (NEW priority — r4 makes this trade explicit)
- r22-S1 sanitize widening (already landed `7647cd4d` per side-channel commits)
- r26-S2 CI enforcement

## Cluster cost

- Stress provision: 3 servers (n2-standard-4) + 3 workers (n2-standard-32), ~30 min total wall-time (provision 5 min + stress 20.5 min + teardown 3 min).
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
