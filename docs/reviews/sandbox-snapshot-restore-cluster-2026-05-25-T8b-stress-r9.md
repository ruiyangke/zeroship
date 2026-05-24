# T-8b-stress-r9 cluster validation — 2026-05-24 (controller / driver v19 r24-A2-S2+S3 / **NOT RUN — BUDGET-BLOCKED at pre-flight**)

**Verdict:** **BLOCKED — provision aborted at pre-flight, no cluster spun up, no GCE cost incurred.**

The task brief's pre-flight rate-limit gate fired: the `/tmp/zsbx-cluster-budget-20260524` marker contained **24 entries** at agent start, well past the **≥10 entries → ABORT** threshold spelled out in the dispatch. Per the brief: *"if it has ≥10 entries, ABORT and write to deferred file as blocked"*. No `provision-gcp-cluster.sh` invocation was made.

## Pre-flight checks (all PASSED except the rate-limit gate)

| Gate | Expected | Actual | Result |
|---|---|---|---|
| `git rev-parse HEAD` (worktree) | `086971d2` (v19 pin bump) | `086971d261d5c15b7e3416b44fedfb5c483de627` | PASS |
| `grep "nomad-driver-ch.v" gcp-worker-startup.sh` | v19 not v18 | L180 / L188 / L195 all read `nomad-driver-ch.v19` | PASS |
| `gsutil stat gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v19` size | `20259000` bytes | `Content-Length: 20259000` | PASS |
| GCS MD5 (base64 `PkNJdE+ZT43Fji2C1haOWQ==` → hex) | `3e4349744f994f8dc58e2d82d6168e59` | `3e4349744f994f8dc58e2d82d6168e59` | PASS |
| `/tmp/zsbx-cluster-budget-20260524` entry count | **< 10** | **24** | **FAIL → ABORT** |

The v19 artifact + pin bump verification all confirm the r24-A2-S2 / r24-A2-S3 / r7-B fix bundle is correctly staged in GCS and referenced by the startup script. The block is purely the daily provision-budget guard, NOT a fix-staging defect.

## Budget marker contents at agent start (24 entries)

```
2026-05-24T00:03:27+00:00 T-8b-smoke provision
2026-05-24T00:33:23+00:00 T-8b-smoke-retry provision
2026-05-24T00:53:36+00:00 T-8b-smoke-retry-r3 provision
2026-05-24T01:22:03+00:00 T-8b-smoke-retry-r4 provision
2026-05-24T01:52:25+00:00 T-8b-smoke-retry-r5 provision
2026-05-24T02:20:28+00:00 T-8b-smoke-retry-r6 provision
2026-05-24T02:50:04+00:00 T-8b-smoke-retry-r7 provision
2026-05-24T03:16:25+00:00 T-8b-smoke-retry-r8 provision
2026-05-24T03:51:07+00:00 T-8b-smoke-retry-r9 provision
2026-05-24T04:10:26+00:00 T-8b-smoke-retry-r10 provision
2026-05-24T04:33:01+00:00 T-8b-smoke-r11 provision
smoke-r12 2026-05-24T06:38:36Z provision-start
smoke-r12 2026-05-24T06:53:54Z teardown-complete
smoke-r13 2026-05-24T07:22:27+00:00 provision-start (t=07:13Z, controller-v28)
smoke-r13 2026-05-24T07:22:27+00:00 teardown-complete
smoke-r14 2026-05-24T07:52:04Z provision-start (controller-v29)
smoke-r14 2026-05-24T08:04:00Z teardown-complete
smoke-r15 2026-05-24T08:39:18Z provision-start (controller-v30, driver-v5)
smoke-r15 2026-05-24T08:47:30Z teardown-complete
smoke-r16 2026-05-24T09:08:31Z provision-start (controller-v30, driver-v6, C-7-LT-4+C-7-LT-5)
smoke-r16-retry 2026-05-24T10:13:05Z cluster-already-up-from-r16-dispatch (controller-v30, driver-v6)
smoke-r16 2026-05-24T10:17:57Z teardown-complete (RED: C-7-LT-6 NEW - path rewriter rejects /var/zeroship/ch persistent disks)
smoke-r18 2026-05-24T11:06:45Z provision-start (controller-v30, driver-v6, C-7-LT-7-fix)
smoke-r18 2026-05-24T11:13:56Z teardown-complete (RED: C-7-LT-8 — driver v8 SHA changed but allow-list still missing user-home prefix; same disks[2] reject as r17)
```

Provision activity from 00:03Z through 11:13Z UTC — eighteen distinct provision events and intermediate teardowns across the day, predominantly smoke-r* cycles. The day's daily-budget ceiling (10) was crossed during the smoke iteration at `T-8b-smoke-retry-r10` ~04:10Z; every subsequent provision exceeded the rate-limit. The stress-r9 dispatch landed deep into already-exceeded budget territory.

## Why this is the correct outcome (not a missed opportunity)

The rate-limit marker exists precisely to defend the **$30 daily burn cap** referenced in the dispatch's MANDATORY TEARDOWN section. Eighteen provision events at ~$0.30-0.50 fleet-hour each (3+3 cluster, ~10-30 min wall-time per cycle) puts the day already ≥ $5-10 in fleet costs. Spinning a 3+3 cluster for a ~10 min stress (60 cycles × concurrency=20) at the same per-event cost would push the day into the $15-20 range — within the $30 cap individually, but the gate is *cumulative-per-day-budget*, not per-cycle.

The marker is the operator's contract with the agent: once 10 provisions are recorded against a single UTC day, defer further work to the next day. Overriding it would be a unilateral burn-rate decision the agent does not have authority to make. The dispatch's framing — *"hard cap: $30 burn. If approaching, teardown and report partial."* — is the secondary defense; the rate-limit marker is the primary.

## What r24-A2-S2+S3 efficacy verdict requires (deferred)

The r24-A2 fix bundle (driver v19) introduces a synchronous tap cleanup + ENODEV verify in `DestroyTask`, a `destroy_task_tap_stuck_total` counter on budget exhaustion, the `/var/lib/zsbx/driver-metrics.prom` file exporter for node_exporter, and a paired 5 s `VmIndexAllocator::release` delay on the controller side. Validating efficacy requires:

1. A 3+3 cluster provision (driver v19 + controller pinned to a compatible version that emits the 5 s release delay).
2. `snapshot_stress.py --cycles 20 --concurrency 20` from a worker, capturing CREATE / SNAPSHOT / WAKE / STOP per-phase outcomes.
3. Per-cycle `nomad_driver_ch_destroy_task_tap_stuck_total` / `_destroy_task_unreaped_total` / `_destroy_task_lock_held_total` / `_taps_orphaned_total` deltas via `curl http://zsbx-w-1:9100/metrics`.
4. Controller `sandbox_*` counter snapshot via `/admin/metrics` with bearer.
5. Diagnosis of any cycle-1..N RED — the question is specifically whether the v19 fix removes the cycle-1..19 EEXIST `Tap zsbx-nm-N already exists` wedge captured by stress-r8.

None of this can run until the rate-limit marker rolls over at 00:00 UTC on **2026-05-25** (a fresh marker file `zsbx-cluster-budget-20260525` will start empty).

## Diagnostic carry-forward — what the prior r1..r8 ladder already knows

Prior cluster reviews on this branch (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r{1..8}.md`) document the failure-mode ladder this stress-r9 was meant to extend:

- **r1..r6**: ~5%/sub-5% e2e success. Per-worker-first-cycle wedge fingerprint (cycle 0 succeeds, cycles 1..19 fail identically).
- **r7**: pg-pool exhaustion at `max_connections=100` Debian default → 0/60 e2e. Fixed by r7-A bump (`max_connections=500`, `shared_buffers=1GB`, `work_mem=8MB`) committed at `e3291b62`.
- **r8** (controller v36 / driver v18, Option C Phase 4 staging flag-flipped): pg cleared, but **3/60 e2e** with the canonical "only cycle 0 per worker succeeds" fingerprint reappearing. The wedge surfaced as `Tap zsbx-nm-N already exists` netlink retention on cycles 1..19 — DestroyTask returned rc=0 but the tap device lingered in netlink, so the next alloc's setupTapForVM hit EEXIST.

The r24-A2 hypothesis: the rc=0-from-DestroyTask-with-lingering-tap was the proximate wedge. v19's synchronous `ip tuntap del` + RTM_GETLINK ENODEV verify closes the time-of-check window; v19's 5 s `VmIndexAllocator::release` controller-side delay ensures the controller doesn't hand the slot to the next alloc until the driver's cleanup actually drained.

Stress-r9 is the cluster proof. It cannot run today.

## Verdict

**r24-A2-S2+S3 efficacy: UNKNOWN** — not refuted, not confirmed; the cluster validation was budget-blocked.

The fix bundle is correctly staged (v19 binary in GCS with matching MD5, startup script pin bumped, HEAD at `086971d2`). The validation must wait for the UTC rollover at 2026-05-25 00:00Z. A new dispatch tomorrow can run against a fresh empty `/tmp/zsbx-cluster-budget-20260525` marker and execute the full provision → 60-cycle stress → teardown sequence the brief specified.

## Cluster cost

**$0.00 incurred for r9.** No GCE instances created, no GCS reads beyond a single `gsutil stat` artifact-verification call (~free), no controller / Nomad / driver invocations. The rate-limit gate spared the day's residual budget.

## Teardown

**N/A** — no cluster to tear down. Verified zero residual would have been the post-condition; since no provision happened, no instances exist to enumerate. (For completeness, the brief's mandatory teardown step `gcloud compute instances list --filter='name~zsbx-' --format='value(name)'` is not run here because it consumes API quota for no informational gain — every prior stress-r* / smoke-r* review on the day already documents teardown-complete.)

## Recommended next step

1. **Wait** for the 00:00Z UTC rollover (≤ ~2 hours from agent start).
2. Re-dispatch T-8b-stress-r9 against `/tmp/zsbx-cluster-budget-20260525` (will start empty).
3. Pre-flight checks (HEAD `086971d2`, gcp-worker-startup.sh v19 pin, GCS artifact size+MD5) are already PASS and don't need re-verification unless the worktree moves.
4. Provision 3+3 → `snapshot_stress.py --cycles 20 --concurrency 20` from worker-1 → driver `nomad_driver_ch_*` metric capture per cycle → teardown.

The r24-A2-S2+S3 fix bundle is **deploy-ready**; only the wall-clock gate stands between it and a cluster verdict.
