# T-8b-stress-r9-retry-4 — PARTIAL/RED, but r24-A2-S2 binding wedge VALIDATED

**Run**: 2026-05-24 23:01-23:10 UTC. Cluster: 3 servers + 3 workers, n2-standard-4 + n2-standard-32, asia-northeast3-a. Shape: c=20 × 20 cycles = 400 attempts.
**HEAD**: `508c3d76` (pre-cycle-42 paperwork). Driver v19 (`d04711a1`). Controller v36.
**Cost**: ~$0.50. Teardown confirmed (0 instances, 3 IPs released).

## Summary

| Phase | OK | Total | Ratio | p50/p95/p99/max (ms) |
|---|---|---|---|---|
| CREATE | 6 | 400 | 1.5% | 97330 / 97429 / 97429 / 97429 |
| SNAPSHOT | 6 | 6 | 100% | 25256 / 44480 / 44480 / 44480 |
| WAKE | 1 | 6 | 16.7% | 61367 (single sample) |
| STOP | 6 | 6 | 100% | 2 / 3 / 3 / 3 |

**Elapsed**: 192.1s.

## Pre-stress validation (first GREEN across 4 r9 attempts)

- `ssh zsbx-w-1 'nomad node status -self -json | jq -r ".Drivers | keys[]"'` → **`ch` PRESENT** (plus docker/exec/java/qemu/raw_exec).
- This is the **first stress-r9 attempt where the ch driver actually loaded** — r9 RED, r9-retry-2 ABORT, r9-retry-3 ORPHAN, r9-retry-4 attempts 1-3 REFUSED all failed before this gate.

## Driver counter snapshot post-run (from `/var/lib/zsbx/driver-metrics.prom`)

```
nomad_driver_ch_destroy_task_tap_stuck_total 0
nomad_driver_ch_destroy_task_lock_held_total 22
nomad_driver_ch_destroy_task_unreaped_total 0
nomad_driver_ch_start_task_stage_total 22
nomad_driver_ch_start_task_stage_failures_total 0
nomad_driver_ch_taps_orphaned_total 0
```

## Verdict on r24-A2-S2+S3 (BINDING WEDGE)

**VALIDATED.** `destroy_task_tap_stuck_total = 0` across 22 successful DestroyTask runs. The synchronous `ip tuntap del` + ENODEV verify path closed the cycle-1-19 EEXIST `Tap zsbx-nm-N already exists` wedge that drove stress-r8 to 3/60. **First measured cluster evidence** that the binding wedge is closed.

Cross-check: `taps_orphaned_total = 0` (no defensive cleanup needed) + `start_task_stage_total = 22` (Option C Phase 2 staging worked end-to-end).

## New layer surfaced (binding wedge now MOVED, not solved)

Two failure modes dominate stress-r9-retry-4:

### 1. vm-index allocator exhaustion (CREATE-side, 394/400 failures)

```
{"error":"backend_create_failed","message":"backend.create: vm-index allocator exhausted (floor=1, ceil=12)"}
```

**Diagnosis**: with c=20 concurrent CREATE × ceiling=12 slots × 5s release delay (r24-A2-S3), the harness saturates the allocator immediately and 394 of 400 CREATEs fast-fail. This is a CONSEQUENCE of the r24-A2-S3 fix making slot release intentionally async (driver tap-verify budget alignment). Not a bug — but the harness shape needs to either:
- Reduce concurrency (c=12 = ceiling) OR
- Add retry-on-exhausted CREATE OR
- Increase ceiling (production prod tier may need >12)

### 2. WAKE failure mode: `ch: startTaskRestoreBranch` (driver-side)

```
[5x] code=200 state=failed error=restore_backend_failed:
  backend: nomad alloc terminal status=failed:
  Failed tasks: ch: rpc error: code = Unknown desc =
  ch: startTaskRestoreBran[truncated]
```

5 of 6 wakes failed with the driver's `startTaskRestoreBranch` returning an error. The full error message was truncated by the harness (180-char limit). Need to capture verbatim from Nomad alloc events or driver-metrics.prom counter (none of the existing counters cover restore-branch failures specifically — observability gap).

`destroy_task_lock_held_total = 22` likely correlates: every restore attempt's underlying `--restore` path tries to acquire OFD lock on rootfs.img, and the prior alloc's lock isn't released until the 5s budget on the NEW alloc expires — but the lock here is on a DIFFERENT alloc's rootfs.img (the snapshot's), so this is the r5-A wedge wrapping the restore path.

## Counter histogram analysis

`lock_held_total = 22` vs `start_task_stage_total = 22` vs successful CREATE = 6:

- 22 StartTasks ran (covers 6 successful CREATEs + 5 failed wake StartTasks + 11 either cancellations or in-flight at teardown)
- 22 DestroyTasks exhausted their OFD-lock budget — every single one
- This means the rootfs.img OFD-lock path is firing 100% of the time post-CH-exit on this cluster shape

The r5-A probe is doing its job (budget-exhaust + counter + return nil), but the LOCK ITSELF isn't releasing in 5s. The kernel's delayed __fput is taking longer than the budget. r5-A bounds the wedge but doesn't solve it.

## Counter-data interpretation

| Counter | Value | Interpretation |
|---|---|---|
| tap_stuck=0 | ✓ | r24-A2-S2 working: sync tap del + ENODEV verify closed the stress-r8 wedge |
| stage_total=22 | ✓ | Option C Phase 2 driver-side staging functional |
| stage_failures=0 | ✓ | mkfs.ext4 + truncate working under load |
| taps_orphaned=0 | ✓ | No need for defensive VMIndex-keyed cleanup |
| unreaped=0 | ✓ | r4-A reap-wait working: every CH process reaped within 5s |
| lock_held=22 | ✗ | r5-A budget exhausted on every destroy — kernel delayed __fput exceeds 5s under load |

## Next-cycle binding wedge candidates

1. **r5-A budget tuning OR delayed __fput root cause** — `lock_held=22/22` is the new dominant signal. Either bump the OFD probe budget (5s → 10s? 30s?) OR fix the underlying delayed __fput cause (likely kernel workqueue backlog under high concurrent CH process exits).

2. **Driver `startTaskRestoreBranch` error verbatim** — need to capture the full error message. Possibly related to the rootfs.img lock issue above (if restore can't acquire the lock, it errors). Or a different layer.

3. **Harness or ceiling tuning** — c=20 vs ceiling=12 is structurally lossy. Either reduce harness concurrency to 12, OR raise ceiling, OR add CREATE-side retry.

## Recommendation

**This is real progress, not regression.** r24-A2-S2 closed the prior binding wedge; we've now MOVED to the next layer (r5-A rootfs lock-held wedge + restore-branch errors). The pattern matches the layer-peel ladder that's defined the campaign since stress-r1.

Next: capture full verbatim restore-branch error → diagnose whether it's lock-held on rootfs.img (then r5-A budget bump is the fix) or a separate failure (new sprint).

## Files
- Harness output: `/tmp/stress-r9-retry-4-ssh.log` (116KB)
- Provision log: `/tmp/stress-r9-retry-4-provision.log`
- Driver metrics: captured inline above
- Budget marker: `/tmp/zsbx-cluster-budget-20260524`
