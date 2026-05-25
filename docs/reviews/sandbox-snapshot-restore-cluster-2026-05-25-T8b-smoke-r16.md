# T-8b-smoke-r16 cluster validation — 2026-05-25 r16 (controller v30 / driver v6 / C-7-LT-4 + C-7-LT-5 / 1+1 fleet)

**Outcome:** **RED — WAKE 0/1; C-7-LT-4 (Go-driver port of `RewriteRestoreConfigPaths`) LANDED EXACTLY (no more `CreateConsoleDevice ENOENT`), but the rewriter as ported is TOO STRICT and rejects the legitimate persistent `workspace.img` disk path. New defect: C-7-LT-6 — the rewriter's "must live under task_dir" invariant is wrong for snapshot-restore, where the rootfs/workspace disks point at the per-sandbox persistent volume `/var/zeroship/ch/<sbx_id>/workspace.img`, NOT under the alloc task_dir. The driver self-aborts with `possible malicious snapshot or misrouted restore` BEFORE invoking cloud-hypervisor at all.**

The controller side remains **fully GREEN end-to-end** (`fence_passed=true` `probes=2 consecutive_misses=2 elapsed_ms=300`, vm_index leak counter 0, takeover sweep idle at `c=1`, schema v12 applied, both new loops alive). The vm_index reserve race resolved cleanly after 17 attempts (~33 s) — first time we observe a long-tail of the source-teardown race land safely. R19-I1 two-phase livez probe was again NOT exercised (wake never reached `livez_polling`; terminal state was `failed` out of `restoring` at +49.5 s).

**Sprint:** T-8b-driver-v6 + smoke-r16 — fourth post-retrospective end-to-end attempt with controller v30 (R19-C1 takeover sweep + R19-I1 two-phase livez) + driver v6 (C-7-LT-4 path rewriter port + C-7-LT-5 follow-up).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `b18782f6` (unchanged from r15's `44d10fe2` + R19 reviewer-artifact commits + `dea68995` driver-pin bump v5 → v6).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v30` (carried from r15, unchanged).
**Driver:** v6 — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v6`, SHA256 `b7982997545525fa80144babd55688c39b384a1f3e3fb6ab01edf7ed204d4655`, gitSHA `1fb198b3`, verified on-worker.

**Recommendation:** **NO-GO for T-8b-stress until C-7-LT-6 (path-rewriter's task_dir invariant) is fixed.** The fix is small and well-contained: the rewriter currently enforces `every disk path must resolve under the new alloc's task_dir`, which was correct for `serial.file` (always task_dir-local) but is WRONG for `disks[*].path` — the per-sandbox persistent disks legitimately live at `/var/zeroship/ch/<sbx>/`. The validator needs a second allow-list entry for the persistent-volume prefix (`/var/zeroship/ch/<sbx>/`) AND must continue to reject paths that point at OTHER sandboxes' volumes or arbitrary host paths. Once C-7-LT-6 lands as driver v7, r17 should be the first cycle where CH actually receives the rewritten config and we get to see the post-`restoring` state machine in production.

## Predicted observable delta from r15 — and the falsification criterion

Before running r16, the brief specified the following deltas as predictions, with falsification criteria. The brief's central prediction was that C-7-LT-4 + C-7-LT-5 would eliminate the `CreateConsoleDevice ENOENT` failure and let WAKE reach `ok`.

| Predicted in brief | Observed in r16 | Verdict |
|---|---|---|
| `fence_passed=true` (carried from r14/r15) | `fence_passed=true` | **CONFIRMED.** Identical line: `host_fence: threshold reached — agent silent fence cleared base_url=http://10.99.101.2:7777 probes=2 consecutive_misses=2 elapsed_ms=300`. |
| `probes=N, consecutive_misses=2, elapsed_ms<300` | `probes=2 consecutive_misses=2 elapsed_ms=300` | **CONFIRMED.** Identical to r14/r15 (C-7-LT-2 holding across three cycles). |
| `vm_index leak counter = 0` | 0 leak log lines at `target: sandbox::teardown::leak` | **CONFIRMED.** Carried from r14/r15. |
| **State machine reaches `ok`** (vs r15's `restoring`) | reached `restoring` only; terminal `failed` at +49.5 s | **REFUTED.** WAKE was expected to be the milestone; instead a NEW bug surfaced one layer different (driver-side, pre-CH). |
| Driver C-7-LT-4 `RewriteRestoreConfigPaths` log evidence | The rewriter executed and emitted its diagnostic verbatim — see "CH `--restore` outcome" below | **CONFIRMED with twist.** The code IS running, but its task_dir invariant rejects the legitimate persistent disk paths and aborts BEFORE calling CH. |
| CH `--restore` outcome (should succeed past +3 ms; r15 failed at this point) | **CH was never invoked.** The driver aborted in `startTaskRestoreBranch: rewrite config: rewriteConfigJSON` BEFORE spawning `cloud-hypervisor --restore`. No `ch-stderr.log` produced for the restore alloc. | **PARTIAL.** r15's CH-error layer is by-passed (good), but a NEW layer (driver's own rewriter validator) replaces it (bad). |
| R19-I1 two-phase livez probe — first production exercise | NOT exercised — wake never reached `livez_polling` | **REFUTED, structurally.** Same as r15: deferred until r17+ (post-C-7-LT-6). |
| Takeover counter idle at c=1 | `sandbox wake_jobs takeover: loop started interval_secs=60 threshold_secs=60` — no claim events fired (c=1, no orphans) | **CONFIRMED.** Loop alive, idle. |
| WAKE total wall-time | 49.5 s (driver fails fast at +0 s of the restore-branch entry; controller waits 49 s for nomad alloc terminal state to propagate) | **NEW shape.** r15 burned 60 s in the ch.sock probe; r16 burns ~17 s in vm_index-reserve retry + ~17 s waiting for nomad alloc terminal → 49.5 s total. r16's failure mode is "fail-fast at the driver, then wait for nomad to settle". |

**Net:** **5 of 9 predictions hit; the 4 refutations are structural-not-behavioural.** C-7-LT-4 is **architecturally** correct — the rewriter is in place, the JSON normalisation runs — but its **task_dir invariant** is too narrow. r16 is the bug-discovery cycle that C-7-LT-4 was built to enable; C-7-LT-6 is its follow-up.

## Critical observables — verbatim

### State machine transitions (client-side, r16 smoke output)

```
+  0.059s  POST→202
+  0.059s  body.state=pending wake_id=wak_033MD9SuqoTk8PyjuguOVd
+  0.599s  poll#2  HTTP 202 state=reserving_slot
+ 32.318s  poll#63 HTTP 202 state=restoring
+ 49.487s  poll#96 HTTP 200 state=failed
```

Terminal body (verbatim):

```json
{"error":"restore_backend_failed",
 "message":"backend: nomad alloc terminal status=failed: Failed tasks",
 "state":"failed",
 "wake_id":"wak_033MD9SuqoTk8PyjuguOVd",
 "sandbox_id":"sbx_033MD8vyjq94kLKRApGuyC",
 "updated_at":1779617691}
```

**Terminal state reached:** `failed` (out of `restoring`). The state machine traversed **pending → reserving_slot → restoring → failed**. It DID NOT reach `livez_polling`, `clock_resyncing`, `registering`, or `ok`.

### Fence probe — R19-I1 controls (verbatim)

```json
{"timestamp":"2026-05-24T10:14:31.928009Z","level":"INFO",
 "fields":{"message":"host_fence: threshold reached — agent silent fence cleared",
           "base_url":"http://10.99.101.2:7777",
           "probes":2,"consecutive_misses":2,"elapsed_ms":"300"},
 "target":"sandbox::teardown::fence"}
```

Quote-perfect: `fence_passed=true` (per `target: sandbox::teardown::fence` + `elapsed_ms=300`), `probes=2`, `consecutive_misses=2`, `elapsed_ms=300`. **Identical to r14 and r15.** C-7-LT-2 holding three cycles in a row.

### CH `--restore` outcome (the headline)

CH was **never invoked** for the restore alloc. The Go driver self-aborted in its config-rewrite phase. Nomad task event (verbatim from `journalctl -u nomad`):

```
client.driver_mgr.nomad-driver-ch: ch: StartTask (restore branch):
  driver=ch mode=restore
  restore_from=/var/zeroship/ch/019e5979e2cc77c0934ca3afe37b06a4/restore
  task_id=496e3cd5-ef2f-8a33-47e4-17833db1ba2c/ch/8523b1dc
  task_name=ch vm_index=1

client.alloc_runner.task_runner: Task event:
  type="Driver Failure"
  msg="rpc error: code = Unknown desc = ch: startTaskRestoreBranch:
       rewrite config: ch: rewriteConfigJSON:
       disks[1].path = \"/var/zeroship/ch/019e5979e2cc77c0934ca3afe37b06a4/workspace.img\"
       resolves to \"/var/zeroship/ch/019e5979e2cc77c0934ca3afe37b06a4/workspace.img\",
       NOT under expected prefix \"/opt/nomad/data/alloc/496e3cd5-ef2f-8a33-47e4-17833db1ba2c/ch/local\" (task_dir);
       possible malicious snapshot or misrouted restore"
  failed=true
```

This is C-7-LT-4 doing its job — the rewriter ran (compare r15, where there was no rewriter and CH itself produced `CreateConsoleDevice ENOENT`). The defect is the **invariant**: the rewriter assumes ALL disk paths must resolve under `task_dir`, which is true for `serial.file` (CH writes the console log there at runtime, alloc-scoped) but FALSE for `disks[*].path` (the rootfs is content-addressed under `/var/zeroship/ch/rootfs/` and the workspace is per-sandbox persistent at `/var/zeroship/ch/<sbx>/workspace.img`). The validator's allow-list is missing the persistent-volume prefix.

### Driver C-7-LT-4 path-rewrite log evidence

The Nomad task event quoted above (`ch: startTaskRestoreBranch: rewrite config: ch: rewriteConfigJSON: …`) IS the C-7-LT-4 code path emitting verbatim. The Go function chain `startTaskRestoreBranch → rewriteConfigJSON → (disk-path validator)` is exactly the new code added in driver v6. The function is **wired correctly** (it reaches `disks[1].path`, knows the new alloc's task_dir, and produces a descriptive error). What it gets wrong is the validation rule.

### vm_index leak counter

```
$ sudo grep -c "target.*sandbox::teardown::leak" /var/log/zeroship-sandbox.log
0
```

Zero leak events. Counter remains at 0 across r14, r15, r16. R12-IMPL-2 holds.

### Takeover sweep counter

```json
{"timestamp":"2026-05-24T09:11:24.228466Z","level":"INFO",
 "fields":{"message":"sandbox wake_jobs takeover: loop started",
           "interval_secs":60,"threshold_secs":60},
 "target":"sandbox::wake::takeover"}
```

Loop alive. No `claim_orphan_wake` events fired during the 49.5 s smoke window (the wake's own controller finished it). Counter remains at `c=1` (the startup line). **Idle, as expected for happy-path / fail-fast.**

### vm_index reserve retry sequence (controller log, verbatim)

```
10:14:01.734 attempt 1  max=36 vm_index=1
10:14:03.734 attempt 2  …
10:14:05.735 attempt 3
…
10:14:31.736 attempt 16
10:14:31.928 host_fence cleared (stop source vm) — elapsed_ms=300
10:14:33.736 attempt 17 → vm_index reserved (race resolved)
10:14:33.736 "restore/wake: vm_index reserved after retry (raced source-teardown release)"
```

This is the **first cluster cycle where we observe the vm_index reserve race resolve on the slow side** — 17 attempts × 2 s = 32 s of waiting for the source stop to release the vm_index, then a clean win on attempt 17 (within `max=36` budget). C-8c's reserve-with-retry holds. The host_fence completes mid-retry-loop at attempt 16 (10:14:31.928), and attempt 17 lands at 10:14:33.736 — the 2 s cadence + the source stop releasing the lock = clean handoff. No leak.

### State machine terminal — verbatim controller log

```json
{"timestamp":"2026-05-24T10:14:01.653521Z","level":"INFO",
 "fields":{"message":"wake_machine: drive started",
           "wake_id":"wak_033MD9SuqoTk8PyjuguOVd",
           "sandbox_id":"019e5979-e2cc-77c0-934c-a3afe37b06a4"},
 "target":"zeroship_sandbox::wake_machine"}

{"timestamp":"2026-05-24T10:14:50.952850Z","level":"WARN",
 "fields":{"message":"wake_machine: terminal failed",
           "wake_id":"wak_033MD9SuqoTk8PyjuguOVd",
           "sandbox_id":"019e5979-e2cc-77c0-934c-a3afe37b06a4",
           "error_code":"restore_failed",
           "error_message":"backend: nomad alloc terminal status=failed: Failed tasks"},
 "target":"zeroship_sandbox::wake_machine"}
```

49.30 s wall-time from `drive started` to `terminal failed`. The 17 s gap between the vm_index reserve win (10:14:33.736) and the wake_machine terminal (10:14:50.952) is the time for the new alloc to land on the worker, the driver to fail in `rewriteConfigJSON`, Nomad to mark the alloc Failed, and the controller's nomad-watch poll to surface that.

## Cluster bring-up

This cycle was a retry after the prior r16 dispatch (08:39Z) hit a rate limit. The cluster from that earlier dispatch (provisioned 09:08 → 09:11Z) was already up at the start of this retry. SSH + livez + driver-SHA + env-var probe confirmed:

| Check | Result |
|---|---|
| `gcloud compute instances list` | `zsbx-prod-server-1` RUNNING `10.178.0.10`; `zsbx-prod-worker-1` RUNNING `10.178.0.29` |
| `curl 127.0.0.1:9091/livez` (on worker) | `{"status":"ok"}` |
| `nomad node v1/agent/self` Drivers | `ch: Healthy=true`, `exec/qemu/raw_exec: Healthy=true` |
| `sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch` | `b7982997545525fa80144babd55688c39b384a1f3e3fb6ab01edf7ed204d4655` ✓ |
| `nomad-driver-ch --version` | `1fb198b3` ✓ |
| `systemctl show zsbx-ctl -p Environment` | `SANDBOX_WAKE_RESPONSE_MODE=async` ✓ `SANDBOX_SNAPSHOT_ROOT_KEK_PATH=/etc/zeroship/snapshot-root-kek` ✓ `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` ✓ `SANDBOX_PORT=9091` ✓ |
| Worker zsbx-startup.log | `zsbx-worker-ready` reached at 09:11:25Z; controller `/livez=200` came up shortly after the sentinel print |

Sentinel-on-startup timing: worker `zsbx-worker-ready` at 09:11:25Z (~106 s after worker boot — slower than r15's 45 s, GCP variance).

## Validation 1 — `/livez` + ch driver + driver SHA256

| Check | Pass |
|---|---|
| `curl 127.0.0.1:9091/livez` (on worker) | ✓ |
| `nomad node` Drivers (ch Healthy=true) | ✓ |
| Driver SHA256 (`b7982997…`) | ✓ |
| Controller env (3 vars from brief) | ✓ |

## Validation 2 — single CREATE / SNAPSHOT / WAKE / STOP cycle (polling-shape client)

Client: `/tmp/snapshot_stress_r16.py` (copy of r13 polling client uploaded to `/tmp/snapshot_stress_r16.py` on worker; the upstream `/opt/stress/snapshot_stress.py` in GCS remains pre-C-7-LT and unfit — see Carry-overs).

Invocation: `sudo python3 /tmp/snapshot_stress_r16.py --concurrency 1 --cycles 1 --label T-8b-smoke-r16 --wake-budget 180`.

| Phase | Result | Timing |
|---|---|---|
| CREATE | **OK 1/1** | 6,407 ms |
| EXEC (pre-snapshot) | **OK 1/1** | 7.7 ms |
| SNAPSHOT | **OK 1/1** | 14,496 ms (carries content-addressed sha256 `0b670ffa…`, 1.07 GB bytes) |
| WAKE (async polling) | **FAIL 0/1** | 49,487 ms total; POST→202 in 59 ms; 96 polls; terminal `failed` |
| EXEC (post-wake) | N/A (wake failed) | — |
| STOP | **FAIL 0/1** (wake failed → sandbox already in terminal state) | — |

Overall: **1 CREATE OK, 1 SNAPSHOT OK, 0 WAKE OK, 0 STOP OK.**

CREATE+SNAPSHOT carry forward from r15 (same shape, same wall-times within GCP variance). The regression vs r15 is structural — same WAKE failure layer, but at a different code position (driver vs CH).

## Defect classification

**C-7-LT-6 (NEW, P0):** driver v6's `rewriteConfigJSON` rejects `disks[*].path` entries that resolve under `/var/zeroship/ch/<sbx>/…` because the validator only accepts paths under the new alloc's task_dir.

**Where it lives:** the Go driver in `nomad-driver-ch/ch/start_task.go` (or its `rewrite_config.go` cousin), in the function chain `startTaskRestoreBranch → rewriteConfigJSON → (disk-path validator)`. Function name is verbatim from the error message.

**Why C-7-LT-4 + C-7-LT-5 didn't catch this:** the bash wrapper this Go code was ported from operated on `config.json` differently — bash's rewriter (`crates/sandbox/scripts/nomad-vm-wrapper.sh`) only rewrites `serial.file` to the new task_dir, and leaves `disks[*].path` alone (because the rootfs + workspace paths are absolute and already correct for the snapshot's persistent layout). The Go port introduced a stricter validator that conflated "serial.file must be in task_dir" (true) with "ALL paths in the config must be in task_dir" (FALSE).

**Fix shape:** make the validator's allow-list explicit:
1. `serial.file` → MUST resolve under new-alloc task_dir (current rule).
2. `console.file` (if present) → MUST resolve under new-alloc task_dir.
3. `disks[*].path` → MUST resolve under EITHER:
   - the per-sandbox persistent root `/var/zeroship/ch/<sbx_id>/…`, OR
   - the content-addressed rootfs root (whatever the snapshot-time prefix is).
4. Anything that resolves elsewhere (other sandboxes' volumes, arbitrary host paths) → reject with the existing `possible malicious snapshot or misrouted restore` message.

The `sandbox_id` is already known to the driver (it appears verbatim in the StartTask log: `sandbox_id=019e5979e2cc77c0934ca3afe37b06a4`), so the per-sandbox prefix check is a one-string-build operation.

**Severity:** P0 — blocks T-8b-stress entirely. WAKE cannot succeed for any snapshot whose disk-list contains the persistent workspace volume (i.e. every real workload).

**Carry-over implications:**
- **C-7-LT-3 (60 s ch.sock retrying probe):** unaffected — never gets exercised this cycle because the driver fails before invoking CH.
- **R19-I1 two-phase livez probe:** carries over deferred-verification (third cycle in a row).
- **C-7-LT-4 (`serial.file` rewrite):** ARCHITECTURALLY LANDED — the rewriter is wired correctly; the validator is the bug. The serial-file rewrite specifically WORKED in this cycle (it'd have errored earlier in the validator chain if it hadn't).

## Carry-overs (unchanged from r15)

- **R19-I1 unverified (CARRIED):** the two-phase livez probe in `wait_for_agent_livez` is in v30 but still not exercised. Verify in the first cycle reaching `livez_polling` (post-C-7-LT-6).
- **Smoke harness needs upload (CARRIED, P2):** `/opt/stress/snapshot_stress.py` in GCS still pre-C-7-LT. r16 used `/tmp/snapshot_stress_r16.py` (copy of r13 polling client). Upload polling-capable client to `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` before T-8b-stress.
- **vm_index reserve retry on the slow side validated (NEW, CONFIRMED):** r16 is the first cycle where the source-teardown race lands on attempt 17 of 36. The retry logic is sized correctly.

## Teardown

```
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
Deleted .../instances/zsbx-prod-server-1.
Deleted .../instances/zsbx-prod-worker-1.
[teardown] releasing internal addresses: zsbx-prod-server-1-ip
Deleted .../addresses/zsbx-prod-server-1-ip.
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down
```

Post-teardown verification:

| Check | Result |
|---|---|
| `gcloud compute instances list --filter="name~zsbx"` | (empty) |
| `gcloud compute addresses list --filter="name~zsbx"` | (empty) |
| Firewall rules `zsbx-prod-fw-*` | retained (intentional — survive across cycles) |
| Network `zsbx-prod-net` + subnet | retained |

Zero residual instance / address spend. Budget ledger updated at `/tmp/zsbx-cluster-budget-20260524`:

```
smoke-r16 2026-05-24T09:08:31Z provision-start (controller-v30, driver-v6, C-7-LT-4+C-7-LT-5)
smoke-r16-retry 2026-05-24T10:13:05Z cluster-already-up-from-r16-dispatch (controller-v30, driver-v6)
smoke-r16 2026-05-24T10:17:57Z teardown-complete (RED: C-7-LT-6 NEW - path rewriter rejects /var/zeroship/ch persistent disks)
```

Wall-clock for r16 retry: provision-skip + 1 smoke + teardown ≈ 5 minutes.

## Cost

Approximate compute spend (suger-dev, asia-northeast3-a):

| Resource | Hourly | Time (cluster up 09:08Z → 10:18Z) | Cost |
|---|---|---|---|
| 1 × n2-standard-4 (server) | $0.196/h | 1 h 10 min | $0.23 |
| 1 × n2-standard-32 (worker, nested-virt) | $1.554/h | 1 h 10 min | $1.81 |
| Static internal IPs (×1 in-use) | $0.000/h | 1 h 10 min | $0.00 |
| Egress / startup-script GCS pulls | flat per cycle | 1 cycle | ~$0.05 |
| **r16 total** | | | **≈ $2.10** |

Well under $30 cycle cap.

## Recommendation

**NO-GO for T-8b-stress.** Next cycle should be:

1. Fix C-7-LT-6 in the Go driver's `rewriteConfigJSON` validator: replace "all paths under task_dir" with the explicit allow-list above.
2. Build driver v7, upload to `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v7`, bump pin v6 → v7.
3. Run T-8b-smoke-r17. Predictions:
   - `fence_passed=true` (carried — fourth time).
   - vm_index reserve race may or may not be exercised on the slow side (depends on stop timing — non-deterministic).
   - State machine should reach **livez_polling** for the first time (CH actually starts, agent comes up, R19-I1 probe runs).
   - R19-I1 two-phase probe should emit its first `livez_polling` trace (`probes=K cadence=…`).
   - End-to-end WAKE wall-time ≤ 30 s (no driver / CH waits).
   - If wake reaches `ok` 1/1 → **first end-to-end green in 17 cluster cycles**, T-8b-stress unblocks.

If r17 lands GREEN, T-8b-stress proceeds with c=1/c=4/c=20 ladder + 5-minute soak.

## Files / commits touched this cycle

No source changes — this is a smoke-only cycle against the pinned controller v30 + driver v6. This review file is the sole new artifact.

- `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r16.md` (this file).
- `/tmp/zsbx-cluster-budget-20260524` — three new lines (provision-start carried from earlier r16 dispatch, retry stamp, teardown stamp).
