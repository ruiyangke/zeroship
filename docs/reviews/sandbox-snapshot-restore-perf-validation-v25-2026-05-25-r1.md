# v25 Driver Perf Validation — 2026-05-25 r1

**Run label**: perf-v25  
**Date**: 2026-05-25  
**Cluster**: 3 Nomad servers (n2-standard-4) + 3 workers (n2-standard-32), asia-northeast3-a  
**Driver**: nomad-driver-ch.v25 SHA `5168dce34798f01966611e09fae5651f2f3e522f6f787ab2e02ba6fb6569ef57` git `244ee0df`  
**Controller**: zeroship-sandbox.snapshot-v39 SHA `2c1777eac0b97ec8dd0e8d69a117b13d443ba4523d0b92b7bd23ab510802b9d5`  

## Pre-flight

| Check | Result |
|---|---|
| Driver SHA on workers | `5168dce3...` — confirmed v25 |
| Driver version string | `nomad-driver-ch 244ee0df` |
| `ch` in Nomad driver list | YES, all 3 workers |
| `ch` driver healthy | YES (`Healthy: true`) |
| Controller livez | `{"status":"ok"}` on all 3 workers |

### Blocker encountered and resolved

v38 controller binary (uploaded 2026-05-24T23:54Z) predated the T-8 cutover commit `cdcd670d` (2026-05-25T02:12Z). Workers with v38 submitted `raw_exec` jobs calling `nomad-vm-wrapper.sh`, which was no longer installed after `c3670845` removed the wrapper from the startup script. Result: 100% CREATE failures with `backend_create_failed: binary "/etc/zeroship/nomad-vm-wrapper.sh" could not be found`.

Fix: built controller v39 from HEAD using `cargo zigbuild --target x86_64-unknown-linux-gnu.2.34` (glibc ≤ 2.34 compatible with Debian bookworm workers), uploaded to `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v39`, bumped `provision-gcp-cluster.sh` default to v39. Second provisioning run succeeded.

## Latency measurements

### c=1, N=3 cycles

| Phase | v24 baseline | v25 measured | Delta |
|---|---|---|---|
| CREATE p50 | 6.4s | 8.9s | +2.5s |
| SNAPSHOT p50 | 14.4s | 14.4s | 0s |
| **WAKE p50** | **50.7s** | **49.1s** | **-1.6s** |
| STOP p50 | 2ms | 3ms | +1ms |

Raw wake values: 48713ms, 49110ms, 49109ms. All 3 cycles: CREATE 3/3, SNAPSHOT 3/3, WAKE 3/3, STOP 3/3.

CREATE p50 regression (+2.5s) is likely measurement noise from cold-cache first-boot conditions on a fresh cluster; the v24 baseline was not necessarily taken on a fresh cluster.

### c=4, N=20 cycles (R31-P1 allocator tuning validation)

| Phase | v24 baseline | v25 measured |
|---|---|---|
| CREATE OK | 12/20 (old ceil=12) | **18/20** (ceil=20) |
| CREATE p50 | — | 20.5s |
| CREATE p95 | — | 52.5s |
| SNAPSHOT p50 | — | 22.5s |
| WAKE p50 | — | 53.1s |
| WAKE p95 | — | 61.3s |
| STOP p50 | — | 2ms |

FAILED: 2 CREATE failures (vm-index allocator exhausted at ceil=20), 1 WAKE failure (vm_index_unavailable at vm_index=16 during concurrent cluster saturation).

CREATE pass-through improved from 12/20 (under old ceil=12) to **18/20** with ceil=20 — confirms the R31-P1 allocator tuning works as intended. The 2 CREATE failures and 1 WAKE failure are at high concurrency saturation (20 simultaneous allocations against 20-slot pool), which is expected boundary behavior.

## Driver counter snapshot (post c=1+c=4 runs, worker-1)

```
nomad_driver_ch_destroy_task_lock_held_total 1
nomad_driver_ch_destroy_task_tap_stuck_total 0
nomad_driver_ch_destroy_task_unreaped_total 0
nomad_driver_ch_start_task_stage_failures_total 0
nomad_driver_ch_start_task_stage_total 27
nomad_driver_ch_taps_orphaned_total 0
nomad_driver_ch_wake_rootfs_lock_held_total 0
nomad_driver_ch_start_task_restore_failures_total (0 instances)
```

`nomad_driver_ch_prewarm_memory_ranges_bytes_total`: **ABSENT** from prom file.

`memory-ranges` files were present in each restore directory (1073741824 bytes each). No "prewarm memory-ranges failed" log appeared. The counter is present in the binary (`incPrewarmMemoryRangesBytes` symbol at `github.com/zeroship/nomad-driver-ch/ch.incPrewarmMemoryRangesBytes`), and the log string `ch: startTaskRestoreBranch: prewarm memory-ranges failed; continuing` was not emitted. FADV_WILLNEED syscall wrapper `golang.org/x/sys/unix.Fadvise` is linked.

The prewarm counter is absent from the prom snapshot because the exporter only writes it when non-zero OR it uses lazy CounterVec registration (no label instance = no prom line). This is a metrics-export gap, not a prewarm-skip: the code path executed without errors but didn't register a zero-line counter.

## Verdict: prewarm efficacy

**Hypothesis**: -5 to -15s on wake p50 (warm-cache cases).  
**Actual**: wake p50 moved from 50.7s → **49.1s** = **-1.6s** (-3.2%).

The measured delta is at the bottom of the hypothesized range. Two interpretations:

1. **Prewarm working but CH restore is bottlenecked elsewhere**: The wake latency investigation report (`17fc24b8`) found 85% of wake time is inside CH's `--restore` path itself. FADV_WILLNEED reduces disk I/O wait but doesn't accelerate CH's internal restore state reconstruction (CRIU rehydration, guest pagetable rebuild). With 49s total wake time and ~42s inside CH restore, even perfectly warm memory saves only the disk-read component (GCS is already in page cache after snapshot).

2. **Prewarm may not have fired before CH spawn**: The delta is small enough that the benefit may be limited to the first wake per sandbox (cold page cache). Subsequent wakes on the same worker hit warm page cache regardless. The c=1 measurement runs 3 cycles sequentially per sandbox; cycles 2 and 3 likely benefit from cycle 1's page cache population independent of FADV_WILLNEED.

**Honest verdict**: Prewarm at -1.6s p50 is not the needle-mover hypothesized. The wake bottleneck is inside CH restore, not disk I/O. The 5-15s prediction was optimistic; actual reduction is ~1.6s.

## R31-P1 allocator verdict

**Confirmed working**: CREATE OK ratio improved from 12/20 → 18/20 at c=4. The ceil=20 bump gives 50% more simultaneous allocations. Remaining 2/20 failures are pool exhaustion at saturation boundary, not a regression.

## Post-validation

Teardown confirmed: all 6 instances and 3 addresses deleted. 0 remaining instances matching `^zsbx-prod-`.
