# T-8b-stress cutover-readiness validation — GREEN

**Run**: 2026-05-25 ~03:30 UTC. Fresh cluster (post-v24 fix). c=4 × 5 cycles = 20 attempts.
**HEAD sandbox**: `712c96fb` (driver pin v23→v24). **HEAD driver**: `427cf39d` (v24 upload).
**Cost**: ~$0.50.

## The binding question

Does the v24 driver fix (the O_RDONLY → O_RDWR bug present since v17, masked by EBADF false positives) close the wake-path wedge that drove stress-r1 through stress-r9 RED?

**Answer: YES. WAKE 12/12 = 100% of cycles that reached snapshot.**

## Results

| Phase | OK / Total | Pass-through ratio |
|---|---|---|
| CREATE | 12 / 20 | 60% (8 fast-fail allocator-exhausted; expected on slot turnover) |
| SNAPSHOT | 12 / 12 | 100% of CREATEd |
| **WAKE** | **12 / 12** | **100% of SNAPSHOTed** ✅ |
| STOP | 12 / 12 | 100% of WOKEN |

Elapsed: 358.5s. Per-cycle p50: ~30s.

## Comparison with prior session

| Run | Wake / N | Binding wedge |
|---|---|---|
| stress-r1..r8 | 1-3 / 60 each | (varied; the actual root cause was v17 OFD-probe O_RDONLY bug — all prior "binding wedge" diagnoses were chasing EBADF false positives) |
| stress-r9 | 0 / 60 | heredoc backtick leak (fixed) |
| retry-2 | 0 / 60 | install-ch-plugin-driver default (fixed) |
| retry-4 | 1 / 6 | r24-A2-S2 tap-del verified close (counter showed tap_stuck=0) |
| retry-5 | 0 / 400 | r29-A1 GC backlog symptom (R29-P1 fix) |
| retry-6 | 0 / 400 | still seeing EBADF "lock_held" |
| c=4 v22+v23 | 0 / 8 | COW fixes architecturally correct but PROBE WAS BROKEN |
| **c=4 v24** | **5 / 5** ✅ | OFD-probe fix |
| **c=4×5 fresh** | **12 / 12** ✅ | confirmed at higher N |

## Counter snapshot (driver v24)

```
nomad_driver_ch_destroy_task_tap_stuck_total       0    (r24-A2-S2 stays validated)
nomad_driver_ch_destroy_task_lock_held_total       0    (v24 fix: probe now actually works)
nomad_driver_ch_destroy_task_unreaped_total        0    (r4-A clean)
nomad_driver_ch_wake_rootfs_lock_held_total        0    (probe acquires immediately on COW inode)
nomad_driver_ch_start_task_stage_total            12    (Option C Phase 2 working)
nomad_driver_ch_start_task_stage_failures_total    0
nomad_driver_ch_start_task_restore_failures_total  0    (no stage failures)
nomad_driver_ch_taps_orphaned_total                0
```

**Zero false-positive counter bumps for the first time in the campaign.**

## Verdict

**Plugin is production-stable.** The wake path that gated cutover for 9+ stress cycles is unblocked.

**CREATE 60% rate**: allocator-exhausted under slot turnover at ceil=12 + 5s release delay + c=4 × 5 sequential cycles. NOT a correctness bug. Tunable via:
- Bump ceil to 16-20
- Shrink vm_index_release_delay_secs 5 → 2-3
- Throttle harness concurrency to match released-slot rate

## Cleared for T-7 + T-8 cutover

The bash wrapper `nomad-vm-wrapper.sh` can now be removed. Controller's `SANDBOX_TASK_DRIVER=ch_plugin` should be made the unconditional default (currently env-gated).
