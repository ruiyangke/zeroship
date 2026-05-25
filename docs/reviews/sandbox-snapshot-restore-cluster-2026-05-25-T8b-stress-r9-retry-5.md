# T-8b-stress-r9-retry-5 — RED REGRESSION (0/400 CREATE)

**Run**: 2026-05-25 00:30 UTC. Cluster: 3+3 (asia-northeast3-a, n2-standard-4 + n2-standard-32). HEAD `e66d5efb` (post-R29-C1 class-fix + controller v37 + driver v20).

## Verdict
**REGRESSION from retry-4**. Controller v37 introduced something that causes 100% CREATE failure with `vm-index allocator exhausted (floor=1, ceil=12)` — even idx=0. Every single failure takes ~31600-32100ms wall (suspiciously close to alloc_running_timeout=30s).

| Phase | retry-4 | retry-5 | Delta |
|---|---|---|---|
| CREATE OK | 6/400 (1.5%) | **0/400** | **REGRESSION** |
| SNAPSHOT | 6/6 | 0/0 | — |
| WAKE | 1/6 | 0/0 | — |
| STOP | 6/6 | 0/0 | — |
| create_ms (failed) | 31s | 31s | same |
| create_ms (success) | 97s | n/a | — |

## What's different in v37 vs v36
Only the R29-C1 class-fix at `62b083e1`:
- `stop_inner` now `.await`s `release_vm_index_after(...)` adding +5s to its wall (was fire-and-forget `spawn_delayed_release`)
- `CreateGuard::drop` cleanup future inside `detach_isolated` uses the same helper
- `spawn_delayed_release` helper DELETED

## Hypothesis (UNVERIFIED — needs investigation)

The 31s wall + idx=0 failure suggests a CONTROLLER-INIT or pre-alloc HANG, not actual slot exhaustion. Candidates:

1. **Controller startup race**: v37's binary takes longer to ready `alloc_running_timeout` machinery. Harness hits it before init completes → some pre-check fails → returns "exhausted" as a misleading error.

2. **stop_inner +5s ripple**: at controller boot, the persistence-restore or sweeper might call stop_inner on stale allocs. Each now blocks +5s. Cascading boot delay. With ~6 phantom uses + 12 ceil = 6 free slots, then they too get drained by retried stop_inner calls.

3. **CreateGuard::drop ordering bug**: the new `release_vm_index_after(...).await` inside `detach_isolated` cleanup — maybe the `.await` interacts wrongly with the detach_isolated short-lived runtime in a way that the test fixture (`delay=100ms`) doesn't catch but `delay=5000ms` production does.

4. **Test fixture mismatch**: the new tests use small delays (100ms) and may not have caught a production-only behavior at 5s delay.

## Driver-side observability (was clean in retry-4; not captured here)
Cluster torn down before counter capture. Future investigator should:
- ssh worker-1; cat /var/lib/zsbx/driver-metrics.prom
- Compare counter ratios between retry-4 and retry-5 — if `start_task_stage_total` shows the same 22 cycles in retry-5 with 0 CREATE successes, it confirms the driver IS getting StartTask calls (the failure is purely controller-side BEFORE driver gets invoked).

## Cost
~$0.50. Teardown confirmed (`gcloud compute instances list --filter='name~zsbx-'` empty).

## CRITICAL FOLLOW-UP

**This blocks any further cluster validation until diagnosed.** Revert R29-C1 class-fix (and accept the R29-C1 snap-teardown vm_index leak temporarily) OR diagnose the regression.

Suggested next action: a focused investigator agent that:
1. Reads the R29-C1 class-fix diff line-by-line
2. Runs the controller LOCAL (docker compose or single-VM smoke) to see if CREATE works at all
3. Identifies the specific code path that broke

## File
- Harness log: `/tmp/stress-r9-retry-5-ssh.log` (112KB)
- Budget marker: `/tmp/zsbx-cluster-budget-20260524` (entries logged)

## Context
- Total session cluster cost: ~$1.30 across 5 attempts (r9 RED, retry-2 ABORT, retry-3 ORPHAN, retry-4 PARTIAL, retry-5 REGRESSION). All under $30/cycle hard cap.
- User confirmed unlimited GCP budget.
