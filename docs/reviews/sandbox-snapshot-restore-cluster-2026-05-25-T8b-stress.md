# T-8b-stress cluster validation — 2026-05-25 (controller v32 / driver v12 / 3+3 fleet / 60-cycle stress)

**Verdict:** **RED — 2/60 end-to-end OK (3.3%).** A driver-v12 cold-boot pre-flight check (`disk[1] workspace.img does not exist`) was never exercised by the single-cycle smoke and rejects **49/60 CREATEs**. Of the 11 sandboxes that did get created and snapshotted, only **2 wakes** reached terminal `ok` — the other 9 failed on the restore alloc, all with the same wire `nomad alloc terminal status=failed: Failed tasks` envelope, masking at least two distinct underlying bugs (workspace.img missing; tap-already-exists Exit-1). **T-8b-cutover is BLOCKED.** Wrapper retirement stays deferred. Two driver/controller fixes are needed before re-stress.

## Outcome at a glance

| Phase | OK | Denominator | Rate |
|---|---|---|---|
| CREATE   | 11 | 60 | 18.3% |
| SNAPSHOT | 11 | 11 (of created) | 100% |
| WAKE → `ok` | 2 | 11 (of snapshotted) | 18.2% |
| END-TO-END (CREATE+SNAPSHOT+WAKE+STOP) | **2** | **60** | **3.3%** |
| STOP (unconditional cleanup) | 11 | 60 attempts of created sandboxes | 100% (all created ones cleaned) |

## Sprint context

**Cycle:** 24th cluster cycle (T-8b-stress, first run). Follows smoke-r23 GREEN (commit `082e6ddb`, first end-to-end success in 23 cycles).

**Goal:** validate driver v12 + controller v32 under load — 3 workers × 20 sequential cycles = 60 total cycles. Sequential per worker (concurrency=1), parallel across workers. Each cycle: CREATE → SNAPSHOT → WAKE (POST + poll) → STOP.

**Worktrees:**
- Sandbox: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `082e6ddb`
- Driver: (separate worktree, gitSHA `538ca1de` baked into v12 artifact)

**Cluster shape:** 3 × n2-standard-4 server + 3 × n2-standard-32 worker (nested-virt), `asia-northeast3-a`. Provisioned by `EXTRA_WORKER_METADATA="install-ch-plugin-driver=1" SERVER_COUNT=3 WORKER_COUNT=3 bash crates/sandbox/scripts/provision-gcp-cluster.sh`. All six sentinels hit within 60 s. Driver SHA `ed96e30d…` (v12, `538ca1de`) and controller SHA `bd505bc2…` (v32) verified post-boot on all three workers.

**Polling harness path used:** **Option B** — wrote a polling-aware `snapshot_stress.py` rewrite (POST `/admin/sandboxes/{id}/wake` → 202 with `wake_id`; GET `/admin/sandboxes/{id}/wake/{wake_id}` every 1 s until terminal) and uploaded to `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` BEFORE provisioning. Worker startup script's existing `gsutil cp` line ran on boot and put it at `/opt/stress/snapshot_stress.py` (verified SHA `89ba229e…` across all three workers). Carried action item from smoke-r23 is now CLOSED. Backup of the pre-async version: `/tmp/t8b-stress/snapshot_stress.py.preasync.bak`.

## Per-phase latency (OK-only)

Wall-times measured client-side. p50/p95/p99/max across all OK cycles for each phase. Sample sizes are small (≤11) because of the upstream CREATE failure storm; numbers should be read as profile-confirming, not statistically significant.

| Phase | N | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---:|---:|---:|---:|---:|
| CREATE   | 11 | 6,465  | 6,708  | 6,708  | 6,708  |
| SNAPSHOT | 11 | 14,653 | 15,084 | 15,084 | 15,084 |
| WAKE (total POST + poll) | 2 | 46,990 | 46,994 | 46,994 | 46,994 |
| STOP     | 11 | 20     | 21     | 21     | 21     |

**WAKE p50 vs smoke-r23:** 46,990 ms vs 46,902 ms — within 0.2% noise. The wake state machine itself, when underlying nomad cooperates, behaves identically under sequential load. This means the perf concern from smoke-r23 ("the 33-s reserving_slot phase will surface queueing under stress") is **not** what failed; the failure is a different layer entirely.

**Per-worker CREATE distribution:**
- w1: 5/20 CREATE OK (2 WAKE OK)
- w2: 3/20 CREATE OK (0 WAKE OK)
- w3: 3/20 CREATE OK (0 WAKE OK)

Symmetric across workers — no single-worker pathology, this is a contract-level bug.

## Failure analysis

All 49 CREATE failures and all 9 WAKE failures surface the same wire envelope to the client:

```
code=500   error=backend_create_failed: nomad alloc terminal status=failed: Failed tasks   (CREATE)
state=failed   error_code=restore_backend_failed: nomad alloc terminal status=failed: Failed tasks   (WAKE)
```

That single error envelope **masks at least two distinct underlying driver-level bugs**, both surfaced via `nomad alloc status` on the worker:

### Bug 1 — `workspace.img does not exist (controller must stage before spawn)` [CREATE path]

The dominant failure mode. Pulled from `journalctl -u nomad` on worker-1:

```
2026-05-24T13:36:20.952Z  Task event: type="Driver Failure"
  msg="rpc error: code = Unknown desc = ch: StartTask:
       disk[1] /var/zeroship/ch/019e5a336d8a7e20a5099f7e9caff397/workspace.img does not exist
       (controller must stage before spawn)"
```

The driver-v12 cold-boot `StartTask` path now requires `workspace.img` to be staged in the per-sandbox host dir BEFORE the driver spawns CH. The controller-v32 cold-boot path **does not** stage it (or stages it AFTER the alloc starts, racing the driver). Same alloc dies with `Policy allows no restarts` → `Alloc Unhealthy`, which bubbles up to the controller as `nomad alloc terminal status=failed: Failed tasks`.

This is a **new pre-flight contract** introduced in driver v12 that smoke-r23 didn't exercise — smoke-r23 ran exactly one CREATE on a fresh host where the contract was satisfied incidentally (the controller's snapshot path stages workspace.img *via the snapshot pipeline*, so by the time the smoke's single SNAPSHOT ran, the file existed; but the smoke's CREATE itself was apparently the *first* CREATE on that host, and we still need to audit whether smoke-r23's CREATE genuinely went through this driver path or whether the v11 wrapper was still in play on the cold path while only the restore path consumed v12). The 11/60 CREATEs that did succeed in stress likely got lucky on file-system timing — the staging completes before the driver pre-flight on a not-yet-determined subset of cycles.

**Fix surface (driver):** either drop the pre-flight check on cold-boot (where workspace.img is being staged via a different path), OR document that the controller MUST `fsync` workspace.img into `/var/zeroship/ch/<id>/` before the driver task is submitted.

**Fix surface (controller):** in `crates/sandbox/src/backend/nomad_ch.rs` create path, stage workspace.img to the canonical host dir BEFORE calling `nomad job run`, and verify it `stat`s clean.

### Bug 2 — `Tap zsbx-nm-N already exists` → Exit -1 [WAKE/restore path]

The dominant WAKE-failure mode. The restore alloc runs through `startTaskRestoreBranch` (C-7-LT-12a), the rootfs gets staged correctly (no `DeviceManager(Disk(NotFound))` like r22), CH spawns and resumes — and then exits 18 s later with `Exit Code: -1`. The captured stderr is one line:

```
cloud-hypervisor:   0.532167s: <vmm> WARN:net_util/src/open_tap.rs:84 --
    Tap zsbx-nm-2 already exists. IP configuration will not be overwritten.
```

`WARN` not `ERROR` — but the alloc exits anyway, with `Exit Message` carrying that single WARN line followed by `file already closed` from the driver's stderr-tail logmon hook closing its pipe. Two interpretations:

1. CH actually exits cleanly soon after for an unrelated reason that the stderr capture truncated (logmon pipe closed before the real error line was written).
2. The driver's `init` net hook interprets the "tap already exists" path as fatal (since the tap IS up from a stranded earlier alloc and IP setup is silently skipped, leaving the VM with a half-configured net stack that fails downstream).

Network state at teardown corroborates: 9 stranded tap interfaces (`zsbx-nm-4` through `zsbx-nm-12`, all DOWN/NO-CARRIER) on worker-1 alone. `zsbx-nm-1`/`zsbx-nm-2`/`zsbx-nm-3` were repeatedly bound and re-bound during the stress run by both CREATE and WAKE allocs.

**Fix surface (driver):** make `open_tap` deterministic — either `ip link del` any stranded `zsbx-nm-<idx>` before re-creating, or treat "already exists" as an error and abort early with a clear message instead of silently continuing with WARN-and-exit.

**Fix surface (controller):** the host_fence/stop hook is supposed to tear down the tap. From the worker-1 log it does emit `fence_passed=true elapsed_ms=300`, but the kernel-level `ip link` survives the alloc's `DestroyTask`. Either the wrapper teardown step that drops the tap isn't running, or `install-ch-plugin-driver=1` bypasses the wrapper's `ip link del` and the Go driver doesn't replicate that step.

### Wake state-machine — not the bug

For the 9 WAKE failures the state journey is identical and clean:

```
pending → reserving_slot → restoring → failed
```

i.e., the wake machine correctly walked through the public substates AND correctly surfaced `error_code=restore_backend_failed`. The bug is at the nomad/driver layer, not in the machine. The 2 OK wakes followed `pending → reserving_slot → restoring → ok` with poll counts of 46 — matching smoke-r23 exactly.

## Counter deltas

| Counter | Smoke-r23 baseline | T-8b-stress observed | Delta |
|---|---|---|---|
| vm_index leak counter | 0 | 0 | 0 (no `vm_index_leaks_total` increments; `inc_vm_index_leak` not emitted in any of the three worker logs) |
| Takeover sweep deltas | 0 | 0 | 0 (takeover loop emitted "loop started" at boot but no `claim_taken` rows surfaced) |
| `terminal_overwrite_blocked` | 0 | 0 | 0 (no such log line emitted on any worker — the wake_jobs `ON CONFLICT … DO NOTHING` guard had nothing to overwrite at this rate) |
| `wake_jobs GC: deleted terminal rows` | n/a | 11 deletions across the stress window on worker-1 (5 sweeps × 1-2 rows each), similar on w2/w3 | clean — GC kept the table bounded |
| `host_fence: cleared … fence_passed=true elapsed_ms=300` | 1 occurrence | observed on every successful STOP (11 cycles), all `elapsed_ms=300` | invariant held |

The R23 invariant set (`fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300`) is preserved verbatim on every STOP that fired. The bug is upstream of the stop path.

**Stranded tap interfaces** (NEW observable, not in r23 ledger): 9 `zsbx-nm-N` interfaces left DOWN/NO-CARRIER on worker-1 at end-of-stress. This is the second smoking gun; should be added to the standard post-stress sweep going forward.

## Error-code samples

All 60 cycles classified:

| count | phase | wire envelope |
|---:|---|---|
| 49 | CREATE | `code=500 error=backend_create_failed: backend.create: nomad alloc terminal status=failed: Failed tasks` |
| 9 | WAKE | `state=failed error_code=restore_backend_failed: backend: nomad alloc terminal status=failed: Failed tasks` |
| 2 | WAKE | `state=ok` (clean) |
| 11 | STOP | `code=200 stopped=true lost_leadership=true` (matches r23) |

A single wire envelope (`Failed tasks`) hides two distinct underlying bugs. **Observability gap:** `backend_create_failed` and `restore_backend_failed` should carry the alloc's actual driver-failure message verbatim (the `disk[1] workspace.img does not exist` and `Tap … already exists` strings), not the generic `nomad alloc terminal status=failed: Failed tasks` rollup. Without that, the controller's wire body alone doesn't tell an operator which bug they hit — they have to SSH into a worker and `nomad alloc status` to disambiguate. File this as a follow-up.

## Path-correctness verdict

Smoke-r23 reached a single GREEN end-to-end cycle that proved the **wake state machine + restore-branch rootfs staging (C-7-LT-12a)** works on the happy path. T-8b-stress confirms:

- The wake state machine, polling endpoint, and §10.0 error envelope are all correct (states/codes are clean on both OK and FAIL).
- The cold-boot path in driver v12 + controller v32 has a NEW unmet pre-flight contract (Bug 1).
- The teardown path in driver v12 + controller v32 leaks tap interfaces (Bug 2).

Neither bug is in the wake machine. Both are in the nomad-driver-ch ↔ controller boundary. The smoke missed them because:
1. A single CREATE on a fresh worker happens to win the workspace.img staging race (or hit a stale-file path).
2. A single SNAPSHOT+WAKE cycle never re-uses a vm_index, so the tap-leak doesn't compound.

## Recommendation — T-8b-cutover BLOCKED

**Do not retire `nomad-vm-wrapper.sh` yet.** The wrapper handled tap cleanup deterministically on stop and pre-staged the workspace.img through the bash script's `cp` step — both behaviours the Go driver appears to have dropped or never implemented. The cutover gate (smoke-green → cutover-green) was tripped one step too early; we needed the stress gate in between.

Next sprint **must** be a driver+controller fix pair followed by a re-run of T-8b-stress at the same 3+3 × 20 shape:

1. **Driver v13 (or v12.1):** `startTaskColdBranch` must accept workspace.img-absent and either stage it itself, or error with a structured `error_code=workspace_img_missing` so the controller can retry deterministically; `DestroyTask` must `ip link del zsbx-nm-<vm_index>` regardless of CH-exit-path success.
2. **Controller v33 (or v32.1):** cold-boot path stages workspace.img + `fsync` BEFORE `nomad job run`. Stop path emits a counter increment on every successful tap teardown so the leak counter has signal.
3. **Stress harness:** keep the polling-aware `/opt/stress/snapshot_stress.py` as the canonical version on GCS. The pre-async version is preserved at `/tmp/t8b-stress/snapshot_stress.py.preasync.bak` for rollback.

If both fixes ship and re-stress hits ≥95% end-to-end OK at 60 cycles, T-8b-cutover unblocks.

## Carried action items

- **OPEN (BLOCKER for cutover) — Driver v13 cold-boot pre-flight + tap cleanup.** See "Fix surface (driver)" above. Coordinate with `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch` worktree.
- **OPEN (BLOCKER for cutover) — Controller v33 cold-boot workspace.img staging.** `crates/sandbox/src/backend/nomad_ch.rs` create path.
- **OPEN (P2, observability) — Surface the alloc-level driver failure verbatim through the controller wire envelope.** `backend_create_failed` / `restore_backend_failed` currently strip the most-actionable line; preserve it under a new field (e.g., `cause: "<driver msg>"`).
- **OPEN (P3, sweep) — Add stranded-tap counter.** On stop, count surviving `zsbx-nm-<vm_index>` interfaces after `host_fence: cleared` and emit a metric. Stress would have shouted at us earlier with this.
- **CLOSED — Polling-aware `/opt/stress/snapshot_stress.py` promoted to GCS** (smoke-r23 P2 carryover; T-8b-stress dependency satisfied via Option B).

## Build & upload verification

No new artifacts uploaded in this sprint (driver v12 + controller v32 unchanged from r23). On-worker hash check post-provision:

```
/etc/zeroship/nomad-plugins/nomad-driver-ch:   sha256 ed96e30d88844015b83d373e3d09dfa90c0e0a926c36e33c0db6156fee7b50b6
/usr/local/bin/zeroship-sandbox:               sha256 bd505bc2070cb81e577c864086580967a6ee732c8c85c2964584c68f2b563b5d
/opt/stress/snapshot_stress.py:                sha256 89ba229e2c8544bc648b46f4963e57cf524cd7edfd7af82093a1217afb123d43
```

Identical on all three workers. Driver `--version` banner: `nomad-driver-ch 538ca1de`. Both `zsbx-ctl.service` and `nomad` active+running across the fleet.

## Teardown verification

`bash crates/sandbox/scripts/teardown-gcp-cluster.sh` exit 0. Post-teardown:

```
$ gcloud compute instances list --filter="name~'zsbx-prod-'"
Listed 0 items.
$ gcloud compute addresses list --filter="name~'zsbx-prod-'"
Listed 0 items.
```

Zero residual. Internal IP reservations released.

## Cost

3+3 fleet (3 × n2-standard-4 + 3 × n2-standard-32 nested-virt), `asia-northeast3-a`, ~25 minutes wall-time (5 min worker-2/3 stress + 7 min worker-1 stress + provision + teardown). Estimated **~$1.20** — well under the $30 cycle cap.
