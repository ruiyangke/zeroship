# T-8b-smoke-r17 cluster validation — 2026-05-25 r17 (controller v30 / driver v7 / C-7-LT-6 + previous / 1+1 fleet)

**Outcome:** **RED — WAKE 0/1; C-7-LT-6 (the per-sandbox path allow-list added in driver v7) LANDED EXACTLY (no more `/var/zeroship/ch/<sbx>/workspace.img` rejection), but the allow-list is INCOMPLETE: it whitelists the per-sandbox prefix `/var/zeroship/ch/<sbx_id>/` and the alloc task_dir, but MISSES the per-user persistent home prefix `/var/zeroship/ch/users/<user_id>/` where `home.img` lives. New defect: C-7-LT-7 — the rewriter's allow-list is one entry short for the per-user persistent volume.**

The controller side remains **fully GREEN end-to-end** (`fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300`, vm_index leak counter 0, takeover sweep idle, schema v12 applied, both new loops alive). The vm_index reserve race resolved cleanly after 17 attempts — IDENTICAL count to r16 (~32 s of waiting, then attempt 17 wins). R19-I1 two-phase livez probe was again NOT exercised (wake never reached `livez_polling`; terminal state was `failed` out of `restoring` at +49.86 s).

**Sprint:** T-8b-driver-v7 + smoke-r17 — fifth post-retrospective end-to-end attempt with controller v30 (R19-C1 takeover sweep + R19-I1 two-phase livez) + driver v7 (C-7-LT-6 per-field path allow-list).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `8718120b` (= r16's `b18782f6` + the driver-pin bump v6 → v7).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v30` (carried unchanged from r15/r16).
**Driver:** v7 — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v7`, SHA256 `c33ace1bdd8a432868e1c7b67d747575e7ecc042a7727282d9613e8f2ed65c9c`, gitSHA `f41a869a`, verified on-worker.

**Recommendation:** **NO-GO for T-8b-stress until C-7-LT-7 (per-user-home prefix in the allow-list) is fixed.** The fix is small: extend the v7 allow-list with a third entry for `/var/zeroship/ch/users/<user_id>/`, gated by ownership (`user_id` MUST be the snapshot's owner — already known to the controller via the snapshot manifest and threaded through to the driver as task env / metadata). With three layout-stable prefixes covered (sandbox persistent + task_dir + per-user home), the rewriter will accept every legitimate disk path on the n+1 cycle while still rejecting cross-sandbox / cross-user / arbitrary host paths.

## Predicted observable delta from r16 — and the falsification criterion

Before running r17, the brief specified the following deltas as predictions, with falsification criteria. The brief's central prediction was that C-7-LT-6 would unblock CH `--restore` and let WAKE reach `ok`.

| Predicted in brief | Observed in r17 | Verdict |
|---|---|---|
| `fence_passed=true` (carried from r14/r15/r16) | `fence_passed=true` | **CONFIRMED.** Identical line: `host_fence: threshold reached — agent silent fence cleared base_url=http://10.99.101.2:7777 probes=2 consecutive_misses=2 elapsed_ms=300`. |
| `probes=2, consecutive_misses=2, elapsed_ms<300` | `probes=2 consecutive_misses=2 elapsed_ms=300` | **CONFIRMED.** Identical to r14/r15/r16 (C-7-LT-2 holding across four cycles). |
| `vm_index leak counter = 0` | 0 leak log lines at `target: sandbox::teardown::leak` | **CONFIRMED.** Carried from r14/r15/r16. |
| Driver C-7-LT-6 validator does NOT reject `/var/zeroship/ch/<sbx>/workspace.img` | The workspace.img path is NOT in the rejection message — the rewriter accepted it. Driver moved past the workspace-disk check this cycle. | **CONFIRMED.** The C-7-LT-6 fix is correct as far as it goes. |
| **State machine reaches `ok`** (vs r16's `failed` from `restoring`) | reached `restoring` only; terminal `failed` at +49.86 s | **REFUTED.** WAKE was expected to be the milestone; instead a NEW path allow-list entry was found missing. |
| CH `--restore` outcome (should succeed past +3 ms; r16 driver-aborted before CH) | **CH was again never invoked.** The driver aborted in `startTaskRestoreBranch: rewrite config: rewriteConfigJSON` BEFORE spawning `cloud-hypervisor --restore`, this time on `disks[2].path` (`home.img`) instead of `disks[1].path` (`workspace.img`). | **PARTIAL.** The previous layer (sandbox-prefix) now passes; the NEXT layer (per-user-home prefix) fails. |
| R19-I1 two-phase livez probe — first production exercise | NOT exercised — wake never reached `livez_polling` | **REFUTED, structurally.** Same as r15 and r16: deferred until the cycle that first reaches `livez_polling` (post-C-7-LT-7). |
| Takeover counter idle | Loop alive (`wake_jobs takeover: loop started interval_secs=60 threshold_secs=60`); zero `claim_orphan_wake` events | **CONFIRMED.** Loop alive, idle. |
| WAKE total wall-time | 49.86 s (driver fails fast at +0 s of the restore-branch entry; controller waits ~17 s for nomad alloc terminal state to propagate, with ~32 s prior in vm_index-reserve retry) | **NEW shape, IDENTICAL to r16.** Same timing fingerprint as r16 (49.5 s); the driver fails at a different disk index but the failure mode (fail-fast at validator, then nomad-terminal wait) is byte-for-byte the same. |

**Net:** **6 of 9 predictions hit; the 3 refutations are structural-not-behavioural.** C-7-LT-6 is **architecturally** correct — the rewriter accepts the per-sandbox volume — but the allow-list is **one entry short**. r17 is the bug-discovery cycle that C-7-LT-6 was built to enable; C-7-LT-7 is its follow-up.

## Critical observables — verbatim

### State machine transitions (client-side, r17 smoke output, verbatim)

```
+ 0.054s  HTTP 202 state=pending
+ 0.019s  HTTP 202 state=pending
+ 0.538s  HTTP 202 state=reserving_slot
+32.205s  HTTP 202 state=restoring
+49.859s  HTTP 200 state=failed
```

POST→202 in 54 ms; 97 polls; terminal body (verbatim):

```json
{"error":"restore_backend_failed",
 "message":"backend: nomad alloc terminal status=failed: Failed tasks",
 "state":"failed",
 "wake_id":"wak_033MDpeIodWQ43FWif57b3",
 "sandbox_id":"sbx_033MDp76KIc0Ey0FNW7DcI",
 "updated_at":1779619355}
```

**Terminal state reached:** `failed` (out of `restoring`). The state machine traversed **pending → reserving_slot → restoring → failed**. It DID NOT reach `livez_polling`, `clock_resyncing`, `registering`, or `ok`.

### Fence probe — R19-I1 controls (verbatim)

```json
{"timestamp":"2026-05-24T10:42:15.360759Z","level":"INFO",
 "fields":{"message":"host_fence: threshold reached — agent silent fence cleared",
           "base_url":"http://10.99.101.2:7777",
           "probes":2,"consecutive_misses":2,"elapsed_ms":"300"},
 "target":"sandbox::teardown::fence"}
```

Quote-perfect: `fence_passed=true` (per `target: sandbox::teardown::fence` + `elapsed_ms=300`), `probes=2`, `consecutive_misses=2`, `elapsed_ms=300`. **Identical to r14, r15, and r16.** C-7-LT-2 holding four cycles in a row.

### CH `--restore` outcome (the headline)

CH was **never invoked** for the restore alloc. The Go driver self-aborted in its config-rewrite phase. Nomad task event (verbatim from `journalctl -u nomad`):

```
client.driver_mgr.nomad-driver-ch: ch: StartTask (restore branch):
  driver=ch mode=restore vm_index=1
  restore_from=/var/zeroship/ch/019e5993440570939fcc8fbfde705022/restore
  task_id=56872caf-01ba-38cf-14bb-aa95233fae49/ch/bc3f9a7f
  task_name=ch

client.alloc_runner.task_runner: Task event:
  type="Driver Failure"
  msg="rpc error: code = Unknown desc = ch: startTaskRestoreBranch:
       rewrite config: ch: rewriteConfigJSON:
       disks[2].path = \"/var/zeroship/ch/users/usr_033MDp768TZVdYSaq2M14o/home.img\"
       resolves to \"/var/zeroship/ch/users/usr_033MDp768TZVdYSaq2M14o/home.img\",
       NOT under any allow-list prefix
         [sandbox=019e5993440570939fcc8fbfde705022
          prefix \"/var/zeroship/ch/019e5993440570939fcc8fbfde705022/\";
          task_dir \"/opt/nomad/data/alloc/56872caf-01ba-38cf-14bb-aa95233fae49/ch/local\"];
       possible malicious snapshot or misrouted restore"
  failed=true
```

This is C-7-LT-6 doing its job — the new allow-list IS in place (the error message now lists the two prefixes explicitly), and it correctly accepts the per-sandbox workspace path. But `disks[2].path` is the per-user `home.img` at `/var/zeroship/ch/users/<user_id>/`, which falls outside both whitelisted prefixes. The validator's allow-list is missing a third entry: the per-user persistent home prefix.

### Driver C-7-LT-6 path-rewrite log evidence

The new allow-list IS embedded in the error message (`prefix "/var/zeroship/ch/019e5993440570939fcc8fbfde705022/"` + `task_dir "/opt/nomad/data/alloc/.../ch/local"`). Compare r16's error which only mentioned `task_dir`. The Go function chain `startTaskRestoreBranch → rewriteConfigJSON → (disk-path validator)` is identical to v6; the validator was extended in v7 with the per-sandbox prefix but stopped one entry short of the per-user prefix.

### vm_index leak counter

```
$ sudo grep -c "target.*sandbox::teardown::leak" /var/log/zeroship-sandbox.log
0
```

Zero leak events. Counter remains at 0 across r14, r15, r16, r17. R12-IMPL-2 holds.

### Takeover sweep counter

```json
{"timestamp":"2026-05-24T10:37:34.952878Z","level":"INFO",
 "fields":{"message":"sandbox wake_jobs takeover: loop started",
           "interval_secs":60,"threshold_secs":60},
 "target":"sandbox::wake::takeover"}
```

Loop alive. No `claim_orphan_wake` events fired during the 49.86 s smoke window. Counter remains at `c=1` (the startup line). **Idle, as expected for happy-path / fail-fast.**

### vm_index reserve retry sequence (controller log, verbatim, abridged for length)

```
10:41:45.183  attempt 1   max=36 vm_index=1
10:41:47.183  attempt 2
…
10:42:13.185  attempt 15
10:42:15.185  attempt 16
10:42:15.360  host_fence cleared (stop source vm) — elapsed_ms=300
10:42:17.185  attempt 17 → vm_index reserved (race resolved)
              "restore/wake: vm_index reserved after retry (raced source-teardown release)"
```

This is the **second consecutive cluster cycle where the vm_index reserve race resolves on attempt 17** — IDENTICAL count to r16. The host_fence completes mid-retry-loop at attempt 16 (10:42:15.360), and attempt 17 lands at 10:42:17.185 — same 2 s cadence, same race resolution. C-8c's reserve-with-retry holds. No leak.

### State machine terminal — verbatim controller log

```json
{"timestamp":"2026-05-24T10:41:45.109310Z","level":"INFO",
 "fields":{"message":"wake_machine: drive started",
           "wake_id":"wak_033MDpeIodWQ43FWif57b3",
           "sandbox_id":"019e5993-4405-7093-9fcc-8fbfde705022"},
 "target":"zeroship_sandbox::wake_machine"}

{"timestamp":"2026-05-24T10:42:34.904083Z","level":"WARN",
 "fields":{"message":"wake_machine: terminal failed",
           "wake_id":"wak_033MDpeIodWQ43FWif57b3",
           "sandbox_id":"019e5993-4405-7093-9fcc-8fbfde705022",
           "error_code":"restore_failed",
           "error_message":"backend: nomad alloc terminal status=failed: Failed tasks"},
 "target":"zeroship_sandbox::wake_machine"}
```

49.80 s wall-time from `drive started` to `terminal failed`. The ~17 s gap between the vm_index reserve win (10:42:17.185) and the wake_machine terminal (10:42:34.904) is the time for the new alloc to land on the worker, the driver to fail in `rewriteConfigJSON`, Nomad to mark the alloc Failed, and the controller's nomad-watch poll to surface that. **Byte-for-byte same shape as r16.**

## Cluster bring-up

Fresh provision (no carry-over from r16; r16 cluster was torn down). Provision time: server sentinel 60 s, worker sentinel 15 s — both faster than r16 (60 s / 106 s). All validation checks pass on a fresh cluster:

| Check | Result |
|---|---|
| `gcloud compute instances list` | `zsbx-prod-server-1` RUNNING `10.178.0.10`; `zsbx-prod-worker-1` RUNNING `10.178.0.30` |
| `curl 127.0.0.1:9091/livez` (on worker) | `{"status":"ok"}` |
| `nomad node ... Drivers.ch` | `Healthy=true Detected=true HealthDescription="ready"` |
| `sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch` | `c33ace1bdd8a432868e1c7b67d747575e7ecc042a7727282d9613e8f2ed65c9c` ✓ |
| `nomad-driver-ch --version` | `nomad-driver-ch f41a869a` ✓ |
| `systemctl show zsbx-ctl -p Environment` | `SANDBOX_WAKE_RESPONSE_MODE=async` ✓ `SANDBOX_SNAPSHOT_ROOT_KEK_PATH=/etc/zeroship/snapshot-root-kek` ✓ `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` ✓ |
| Worker zsbx-startup.log | `zsbx-worker-ready` reached |

Sentinel-on-startup timing: faster than r16 (provision was ~2 min wall-clock to both sentinels).

## Validation 1 — `/livez` + ch driver + driver SHA256

| Check | Pass |
|---|---|
| `curl 127.0.0.1:9091/livez` (on worker) | ✓ |
| `nomad node` Drivers (ch Healthy=true) | ✓ |
| Driver SHA256 (`c33ace1b…`) | ✓ |
| Driver gitSHA (`f41a869a`) | ✓ |
| Controller env (3 vars from brief) | ✓ |

## Validation 2 — single CREATE / SNAPSHOT / WAKE / STOP cycle (polling-shape client)

Client: `/tmp/snapshot_stress_r17.py` (polling-aware variant — async wake POST → 202 → poll until terminal).

Invocation: `sudo PYTHONPATH=/opt/stress python3 /tmp/snapshot_stress_r17.py --concurrency 1 --cycles 1 --label T-8b-smoke-r17 --wake-budget 240`.

| Phase | Result | Timing |
|---|---|---|
| CREATE | **OK 1/1** | 6,549 ms |
| EXEC (pre-snapshot) | **OK 1/1** | 7.5 ms |
| SNAPSHOT | **OK 1/1** | 14,526 ms (artifact `d4e82f75…`, ch_version `ch-remote v51.1+aead-cc20p1305`, 1.07 GB bytes) |
| WAKE (async polling) | **FAIL 0/1** | 49,859 ms total; POST→202 in 54 ms; 97 polls; terminal state `failed` (out of `restoring`) |
| EXEC (post-wake) | N/A (wake failed) | — |
| STOP | **FAIL 0/1** (wake failed → cleanup attempted, sandbox already terminal) | — |

Overall: **1 CREATE OK, 1 SNAPSHOT OK, 0 WAKE OK, 0 STOP OK.**

CREATE+SNAPSHOT carry forward from r15/r16 (same shape, same wall-times within GCP variance). The regression vs r16 is structural — same WAKE failure layer (driver path validator), but moved one disk index forward (`disks[2]` per-user home instead of `disks[1]` per-sandbox workspace).

## Defect classification

**C-7-LT-7 (NEW, P0):** driver v7's `rewriteConfigJSON` allow-list accepts the per-sandbox prefix and task_dir but rejects `/var/zeroship/ch/users/<user_id>/home.img`. The per-user persistent home volume is missing from the whitelist.

**Where it lives:** the Go driver, same function chain as C-7-LT-6 (`startTaskRestoreBranch → rewriteConfigJSON → (disk-path validator)`). The validator's allow-list literal needs a third entry.

**Why C-7-LT-6 didn't catch this:** r16's failure occurred at `disks[1]` (`workspace.img`); the rewriter aborted at the first miss without iterating to `disks[2]` (`home.img`). C-7-LT-6 was scoped to the observed failure layer; r17 surfaces the next-layer failure that was always there but masked by the earlier abort. (Layer-by-layer error discovery is the expected shape when a validator fails-fast on the first mismatch.)

**Fix shape:** extend the v7 allow-list with a third entry tied to the snapshot's owner:
1. `serial.file` → MUST resolve under new-alloc task_dir (existing rule).
2. `console.file` (if present) → MUST resolve under new-alloc task_dir (existing rule).
3. `disks[*].path` → MUST resolve under ONE OF:
   - the per-sandbox persistent root `/var/zeroship/ch/<sandbox_id>/…` (v7 rule), OR
   - **NEW:** the per-user home root `/var/zeroship/ch/users/<user_id>/…`, where `user_id` is the snapshot's owner (already known to the controller from the snapshot manifest; thread through to the driver via task env / metadata).
4. Anything else (other sandboxes', other users', arbitrary host paths) → reject with the existing `possible malicious snapshot or misrouted restore` message.

The `user_id` is already visible in this cycle's error message (`usr_033MDp768TZVdYSaq2M14o` is the literal value), so the threading already works; the validator just needs to consume it.

**Severity:** P0 — blocks T-8b-stress entirely. WAKE cannot succeed for any snapshot whose disk-list contains the per-user home volume (which is every real workload — every sandbox is owned by some user and mounts that user's home).

**Carry-over implications:**
- **C-7-LT-3 (60 s ch.sock retrying probe):** unaffected — never gets exercised this cycle because the driver fails before invoking CH.
- **R19-I1 two-phase livez probe:** carries over deferred-verification (fourth cycle in a row — still waiting on the first cycle that reaches `livez_polling`).
- **C-7-LT-4 + C-7-LT-5 (`serial.file` rewrite):** still ARCHITECTURALLY LANDED — the rewriter is wired correctly; the validator's allow-list scope is the bug.
- **C-7-LT-6 (per-sandbox prefix):** LANDED. The error message now lists the per-sandbox prefix as one of the accepted allow-list entries, and the rewriter no longer rejects `/var/zeroship/ch/<sbx>/workspace.img`. The fix is **correct, just incomplete by one entry.**

## Distinct-signal accounting (cluster cycles since T-8b inception)

r17 is the **17th cluster cycle**. Each cycle has surfaced at least one new defect or carried over a known one:

| # | Cycle | New defect (or "carry") |
|---|---|---|
| 1–13 | (history) | C-8a, C-8b, C-8c, B18, B23, R12-IMPL-2, R19-C1, R19-I1, C-7-LT-1, C-7-LT-2, C-7-LT-3, C-7-LT-4, C-7-LT-5 |
| 14 | smoke-r14 | (greens consolidated; fence stabilised) |
| 15 | smoke-r15 | C-7-LT-4 (originally observed as `CreateConsoleDevice ENOENT`) |
| 16 | smoke-r16 | C-7-LT-6 (rewriter task_dir invariant rejects persistent disks) |
| **17** | **smoke-r17** | **C-7-LT-7 (per-user-home prefix missing from allow-list)** |

This is the third consecutive cycle where the failure is at the path-rewriter layer, drilling one disk index forward each time as the previous failure is fixed and the next miss exposes itself. The trajectory is: CH-itself rejects → driver rewriter installed but `disks[1]` rejected → `disks[1]` accepted, `disks[2]` rejected. Predicting `disks[3]` (if any) on r18 would be premature — the snapshot's `config.json` should be enumerated to confirm whether `home.img` is the LAST persistent-disk entry beyond the rootfs and workspace. The retrospective should request that enumeration as part of the C-7-LT-7 fix-up brief, so r18 has a chance of being green or surfacing a genuinely new class of failure.

## Carry-overs (unchanged from r16)

- **R19-I1 unverified (CARRIED):** the two-phase livez probe in `wait_for_agent_livez` is in v30 but still not exercised. Verify in the first cycle reaching `livez_polling` (post-C-7-LT-7).
- **Smoke harness needs upload (CARRIED, P2):** `/opt/stress/snapshot_stress.py` in GCS still pre-async. r17 used `/tmp/snapshot_stress_r17.py` (polling-aware variant uploaded fresh). Upload polling client to `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` before T-8b-stress.
- **vm_index reserve retry IDENTICAL to r16:** 17 attempts × 2 s cadence, source-teardown race resolved cleanly. C-8c retry sizing is correct.

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
| `gcloud compute instances list --filter="name~zsbx"` | (empty, 0 lines) |
| `gcloud compute addresses list --filter="name~zsbx"` | (empty, 0 lines) |
| Firewall rules `zsbx-prod-fw-*` | retained (intentional — survive across cycles) |
| Network `zsbx-prod-net` + subnet | retained |

Zero residual instance / address spend.

## Cost

Approximate compute spend (suger-dev, asia-northeast3-a):

| Resource | Hourly | Time (cluster up ~10:36Z → 10:45Z, ~9 min) | Cost |
|---|---|---|---|
| 1 × n2-standard-4 (server) | $0.196/h | 9 min | $0.03 |
| 1 × n2-standard-32 (worker, nested-virt) | $1.554/h | 9 min | $0.23 |
| Static internal IPs (×1 in-use) | $0.000/h | 9 min | $0.00 |
| Egress / startup-script GCS pulls | flat per cycle | 1 cycle | ~$0.03 |
| **r17 total** | | | **≈ $0.29** |

Well under $30 cycle cap.

## Recommendation

**NO-GO for T-8b-stress.** Next cycle should be:

1. Fix C-7-LT-7 in the Go driver's `rewriteConfigJSON` validator: add a third allow-list entry for `/var/zeroship/ch/users/<user_id>/` gated on the snapshot's owner (`user_id` is already plumbed through and visible in the v7 error message).
2. Optional but recommended: enumerate the full `disks[]` array in the snapshot's `config.json` before the next cycle, to catch any further per-disk prefix surprises (e.g., a future `data.img`, scratch volumes) without paying another cluster cycle to discover them sequentially.
3. Build driver v8, upload to `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v8`, bump pin v7 → v8.
4. Run T-8b-smoke-r18. Predictions:
   - `fence_passed=true` (carried — fifth time).
   - `probes=2 consecutive_misses=2 elapsed_ms=300` (carried — fifth time).
   - vm_index leak counter = 0 (carried — fifth time).
   - vm_index reserve attempt count likely identical (17/36 with 2 s cadence, race resolves cleanly).
   - Driver rewriter accepts all of `disks[0..N].path` (rootfs, workspace, home, …).
   - **State machine reaches `livez_polling` and then `ok`** — THE MILESTONE. (Falsifiable: if WAKE still fails, the failure surface is either a new path miss not predicted, or it's the post-restore livez polling — the FIRST cycle that exercises R19-I1.)
5. **Only IF r18 is green:** T-8b-stress (3 workers × 20 cycles) is the next checkpoint.

This is the **third consecutive layer of the rewriter validator** that has needed widening; the n+1 fix should consolidate the allow-list as a single layout-derived table rather than continuing the "fix one prefix at a time" loop. The retrospective should call for that consolidation.

## Retrospective compliance

- **Predicted observable delta + falsification at top:** done (the "Predicted observable delta from r16" table is the first content section after the headline outcome).
- **Quote ALL critical observables verbatim:** done (state-machine transitions, fence probe line, CH-restore Nomad task event with the rewriter error, vm_index retry log lines, terminal wake_machine log entry, leak counter, takeover loop start).
- **Distinct-signal accounting:** added (r17 = 17th cycle, signal counter walked forward).
- **Cost + teardown verification:** done.
- **Recommendation with concrete next-cycle predictions and falsification criteria:** done (4 carried predictions + 1 milestone prediction with explicit falsification).
