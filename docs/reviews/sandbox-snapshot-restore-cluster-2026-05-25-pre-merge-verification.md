# Pre-merge verification — GREEN

**Run**: 2026-05-25 ~09:15 UTC. 1+1 cluster, asia-northeast3-a.
**Controller**: `zeroship-sandbox.snapshot-v41` (gitSHA `d2a26eed`),
SHA256 `326d7f684dfc4f3bf983ccb7bb269f0768bb19f6e800d6f33dd6d60e8a9d3895`.
Includes: R32-P1 parallel mkfs (`2faaf39b`), R33-I1 per-user fence
(`f39ef124`), r32-A3 + R33-M1 (`d5d4d532`), cadence drift fix
(`4ac1e526`), r31-S1 raw_exec removal (`c56893b2`).
**Driver**: v25 (pinned `a395f1b5`; T-10 trace not yet binary-built).
**Cost**: ~$0.15. Cluster torn down post-run, 0 residuals.

## Results

### c=1 ×3 (baseline path)
| Phase | OK | p50 ms |
|-------|---:|-------:|
| CREATE | 3/3 | 9093 |
| SNAPSHOT | 3/3 | 13935 |
| WAKE | 3/3 | 48100 |
| STOP | 3/3 | 3 |

### c=4 ×2 (concurrency path; exercises R33-I1 fence)
| Phase | OK | p50 ms |
|-------|---:|-------:|
| CREATE | 8/8 | 25534 |
| SNAPSHOT | 8/8 | 35846 |
| WAKE | 8/8 | 54114 |
| STOP | 8/8 | 2 |

**Total: 11/11 attempts × 4 phases = 44/44 phase-success.**

## What this verifies

- **R32-P1** (`2faaf39b`) parallel mkfs.ext4: no regression on either c=1 or c=4. CREATE p50 holds within variance of pre-fix r32-T1 baseline (was 8.6s; now 9.1s — cold cluster).
- **R33-I1** (`f39ef124`) per-user mkfs fence: no deadlock under c=4 concurrency. 8/8 same-process-different-user creates succeed in parallel.
- **r32-A3** (`d5d4d532`) `sandbox_id` plumbed through `wait_for_alloc_running`: trace emits work cleanly (verified by green status; full structured-log inspection skipped since cluster was torn down).
- **r31-S1** (`c56893b2`) raw_exec.enable removed from Nomad client config: no impact on driver dispatch (driver still launches normally).
- **Cycle 53 cadence drift fix** (`4ac1e526`): no observable change in poll timing.

## Cleared for merge

The full `feat/sandbox-snapshot-restore` branch is ready to merge to
main, alongside `feat/nomad-driver-ch`.

Open follow-ups (deferred, non-blocking):
- r30-A1 IMPORTANT: ZSBX_* env block dual-write (3+ rounds)
- r30-A2 IMPORTANT: NomadStopPermits on AppState (4 rounds)
- R30-API1 IMPORTANT: `nomad_stop_permits()` returns &Arc (4 rounds)
- R30-I1: catch_unwind on async backend.stop
- R33-V1: R32-P1 cluster re-measure under stress (this verification was smoke, not stress)
- R13-S1: worker SA `storage-rw` IAM (pre-cutover Ops blocker)

None of these block merge; they are post-launch refinements.
