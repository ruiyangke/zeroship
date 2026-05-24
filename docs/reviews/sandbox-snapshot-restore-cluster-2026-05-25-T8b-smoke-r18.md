# T-8b-smoke-r18 cluster validation — 2026-05-25 r18 (controller v30 / driver v8 / C-7-LT-7 fix attempt / 1+1 fleet)

**Outcome:** **RED — WAKE 0/1; driver v8 SHA changed (`527bf830…`, gitSHA `f329ba14`) from v7 (`c33ace1b…`, gitSHA `f41a869a`) but the validator's allow-list is **byte-identical to v7**: still only the per-sandbox prefix + the new alloc task_dir; the per-user home prefix `/var/zeroship/ch/users/<user_id>/` (where `home.img` lives, listed as `disks[2].path`) is **still missing**. The C-7-LT-7 follow-up code did not reach the validator. Same `disks[2]` rejection as r17, same wall-time, same error string up to the sandbox_id / user_id substitution.**

The controller side remains **fully GREEN end-to-end** (`fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300`, vm_index leak counter 0, takeover sweep idle, vm_index reserve race resolved on attempt 17, schema v12 applied, both new loops alive). Controller v30 logs explicitly expose `user_home_dir_root":"/var/zeroship/ch/users"` and the controller env block now includes `SANDBOX_NOMAD_CH_USER_HOME_ROOT=/var/zeroship/ch/users` (NEW in v30 vs r17's env dump — the controller side of the C-7-LT-7 fix landed). **The Go driver simply doesn't read it.** R19-I1 two-phase livez probe was again NOT exercised (wake never reached `livez_polling`; terminal `failed` out of `restoring` at +49.56 s — identical timing to r16/r17).

**Sprint:** T-8b-driver-v8 + smoke-r18 — sixth post-retrospective end-to-end attempt with controller v30 (R19-C1 takeover sweep + R19-I1 two-phase livez + user_id emission `03d2f4a8`) + driver v8 (intended C-7-LT-7 per-user-home prefix in allow-list).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `4d73a5d1` (= r17's `93d77123` + the controller user_id emission `03d2f4a8` + the driver-pin bump v7 → v8 `4d73a5d1`).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v30` (carried unchanged from r15/r16/r17).
**Driver:** v8 — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v8`, SHA256 `527bf8301da5767cec8fb848f9122b2d330c082938a36cc41097aeb31e7e3388`, gitSHA `f329ba14`, verified on-worker.

**Recommendation:** **NO-GO for T-8b-stress.** This is the **third consecutive cluster cycle hitting the same allow-list-validator layer** (r16 disks[1] / r17 disks[2] / **r18 disks[2] AGAIN with no validator change**). The C-7-LT-7 fix shipped at the *binary* layer (gitSHA moved, SHA256 moved) but NOT at the *validator* layer (error message verbatim unchanged from v7). Per the r19 retrospective's escalation rule — **3 cycles in a row at the same structural layer signals the per-prefix whack-a-mole is unstable** — the next step MUST be **Approach A: snapshot-derived disk-list enumeration**. Stop hand-listing prefixes; let the driver read the snapshot's own `config.json` for the legitimate disk set and accept those paths verbatim. This is the only fix that closes the allow-list class of bugs in one motion.

## Predicted observable delta from r17 — and the falsification criterion

Before running r18, the brief specified the following deltas as predictions, with falsification criteria. The brief's central prediction was that C-7-LT-7 in driver v8 would unblock CH `--restore` and let WAKE reach `ok` — the milestone.

| Predicted in brief | Observed in r18 | Verdict |
|---|---|---|
| `fence_passed=true` (carried r14/r15/r16/r17) | `fence_passed=true` | **CONFIRMED.** Identical line: `host_fence: threshold reached — agent silent fence cleared base_url=http://10.99.101.2:7777 probes=2 consecutive_misses=2 elapsed_ms=300`. |
| `probes=2, consecutive_misses=2, elapsed_ms<300` | `probes=2 consecutive_misses=2 elapsed_ms=300` | **CONFIRMED.** Identical to r14/r15/r16/r17 (C-7-LT-2 holding across five cycles). |
| `vm_index leak counter = 0` | 0 leak log lines at `target: sandbox::teardown::leak` | **CONFIRMED.** Carried r14/r15/r16/r17. |
| Driver C-7-LT-7 validator accepts `/var/zeroship/ch/users/<user_id>/home.img` | The validator rejects `disks[2].path = /var/zeroship/ch/users/usr_033MEVIgxz846ckXa9dxDZ/home.img`; error message lists only `prefix "/var/zeroship/ch/019e59aca6697710bc82fcf20bf204ef/"` + `task_dir "/opt/nomad/data/alloc/.../ch/local"` — **the v7 allow-list verbatim, no user-home entry added**. | **REFUTED.** The intended v8 fix did not land in the validator. |
| **State machine reaches `ok`** (vs r17's `failed` from `restoring`) — THE MILESTONE | reached `restoring` only; terminal `failed` at +49.56 s | **REFUTED.** Third consecutive cycle stuck at the rewriter layer. |
| CH `--restore` outcome (should actually run for the first time) | **CH was again never invoked.** Same as r16/r17: driver self-aborts in `startTaskRestoreBranch: rewrite config: rewriteConfigJSON` BEFORE spawning `cloud-hypervisor --restore`. | **REFUTED.** No first production exercise. |
| R19-I1 two-phase livez probe — FIRST production exercise expected | NOT exercised — wake never reached `livez_polling` | **REFUTED, structurally.** Same as r15/r16/r17: deferred until the cycle that first reaches `livez_polling`. |
| vm_index leak counter = 0 | 0 | **CONFIRMED.** |
| Wake total wall-time ~10-20s (expect if green) | 49.56 s (driver fails fast at +0 s of the restore-branch entry; controller waits ~17 s for nomad alloc terminal status to propagate, with ~32 s prior in vm_index-reserve retry) | **REFUTED, but identical shape to r17 (49.86 s) and r16 (49.5 s).** |

**Net:** **4 of 9 predictions hit; the 5 refutations are all downstream of the same root cause.** The structural defect is that **the binary published as v8 does not contain the C-7-LT-7 code change** — the gitSHA moved (`f41a869a` → `f329ba14`, a real different build) and the SHA256 moved (`c33ace1b…` → `527bf830…`, different bytes), but the allow-list literal embedded in `rewriteConfigJSON` is the same. Either the v8 build was made from a branch that didn't include the validator-extension patch, or the patch was made to a different function and the validator was left untouched. r18 is **NOT a discovery cycle** — it's a **regression / no-op cycle on the driver side**, with the controller side correctly extended.

## Critical observables — verbatim

### State machine transitions (client-side, r18 smoke output, verbatim)

```
+ 0.577s  HTTP 202 state=reserving_slot
+32.213s  HTTP 202 state=restoring
+49.563s  HTTP 200 state=failed
```

POST→202 in 57 ms; 50 polls; terminal body (verbatim):

```json
{"error":"restore_backend_failed",
 "message":"backend: nomad alloc terminal status=failed: Failed tasks",
 "state":"failed",
 "wake_id":"wak_033MEVpidQeYTrXMzeYRZn",
 "sandbox_id":"sbx_033MEVIh4AQT9Z2j8znu2B",
 "updated_at":1779621018}
```

**Terminal state reached:** `failed` (out of `restoring`). The state machine traversed **pending → reserving_slot → restoring → failed**. It DID NOT reach `livez_polling`, `clock_resyncing`, `registering`, or `ok`. Same shape as r17.

### Fence probe — R19-I1 controls (verbatim)

```json
{"timestamp":"2026-05-24T11:09:58.864186Z","level":"INFO",
 "fields":{"message":"host_fence: threshold reached — agent silent fence cleared",
           "base_url":"http://10.99.101.2:7777",
           "probes":2,"consecutive_misses":2,"elapsed_ms":"300"},
 "target":"sandbox::teardown::fence"}
```

Quote-perfect: `fence_passed=true` (per `target: sandbox::teardown::fence` + `elapsed_ms=300`), `probes=2`, `consecutive_misses=2`, `elapsed_ms=300`. **Identical to r14, r15, r16, r17.** C-7-LT-2 holding five cycles in a row.

### CH `--restore` outcome (the headline)

CH was **never invoked** for the restore alloc. The Go driver self-aborted in its config-rewrite phase. Nomad task event (verbatim from `journalctl -u nomad`):

```
client.driver_mgr.nomad-driver-ch: ch: StartTask (restore branch):
  driver=ch mode=restore vm_index=1
  restore_from=/var/zeroship/ch/019e59aca6697710bc82fcf20bf204ef/restore
  task_id=5d39a5fc-e883-a292-811a-0be72a20d620/ch/2cc949ff
  task_name=ch

client.alloc_runner.task_runner: Task event:
  type="Driver Failure"
  msg="rpc error: code = Unknown desc = ch: startTaskRestoreBranch:
       rewrite config: ch: rewriteConfigJSON:
       disks[2].path = \"/var/zeroship/ch/users/usr_033MEVIgxz846ckXa9dxDZ/home.img\"
       resolves to \"/var/zeroship/ch/users/usr_033MEVIgxz846ckXa9dxDZ/home.img\",
       NOT under any allow-list prefix
         [sandbox=019e59aca6697710bc82fcf20bf204ef
          prefix \"/var/zeroship/ch/019e59aca6697710bc82fcf20bf204ef/\";
          task_dir \"/opt/nomad/data/alloc/5d39a5fc-e883-a292-811a-0be72a20d620/ch/local\"];
       possible malicious snapshot or misrouted restore"
  failed=true
```

**Compare to r17's error verbatim**: structurally identical — same `disks[2].path`, same `NOT under any allow-list prefix [sandbox=… prefix "..."; task_dir "..."]` shape, same two-entry allow-list, no user-home third entry. The only diff between r17 and r18 is the sandbox_id / user_id / task_id substitutions. **v8 did not extend the validator.**

### Driver C-7-LT-7 path-rewrite log evidence

The allow-list embedded in the v8 error message contains only:

1. `prefix "/var/zeroship/ch/<sandbox_id>/"` — the per-sandbox prefix (added in v7 = C-7-LT-6).
2. `task_dir "/opt/nomad/data/alloc/<alloc_id>/ch/local"` — the new-alloc task_dir (already in v6).

A third entry for the per-user home root (`/var/zeroship/ch/users/<user_id>/`) is **NOT** in the error message. The validator was not extended; the v8 binary differs from v7 in some other code path, not in `rewriteConfigJSON`'s allow-list literal.

### vm_index leak counter

```
$ sudo grep -c "target.*sandbox::teardown::leak" /var/log/zeroship-sandbox.log
0
```

Zero leak events. Counter remains at 0 across r14, r15, r16, r17, r18. R12-IMPL-2 holds.

### Takeover sweep counter

```json
{"timestamp":"2026-05-24T11:07:25.538714Z","level":"INFO",
 "fields":{"message":"sandbox wake_jobs takeover: loop started",
           "interval_secs":60,"threshold_secs":60},
 "target":"sandbox::wake::takeover"}
```

Loop alive. No `claim_orphan_wake` events fired during the 49.56 s smoke window. Counter remains at `c=1` (the startup line). **Idle, as expected for happy-path / fail-fast.**

### vm_index reserve retry sequence (controller log, verbatim, abridged for length)

```
11:09:28.659  attempt 1   max=36 vm_index=1
11:09:30.659  attempt 2
…
11:09:56.660  attempt 15
11:09:58.660  attempt 16
11:09:58.864  host_fence cleared (stop source vm) — elapsed_ms=300
11:10:00.660  attempt 17 → vm_index reserved (race resolved)
              "restore/wake: vm_index reserved after retry (raced source-teardown release)"
```

**Third consecutive cluster cycle resolving on attempt 17.** Identical count to r16/r17 — same 2 s cadence, same race resolution, same host_fence completion at attempt 16. C-8c's reserve-with-retry holds. No leak.

### State machine terminal — verbatim controller log

```json
{"timestamp":"2026-05-24T11:09:28.586143Z","level":"INFO",
 "fields":{"message":"wake_machine: drive started",
           "wake_id":"wak_033MEVpidQeYTrXMzeYRZn",
           "sandbox_id":"019e59ac-a669-7710-bc82-fcf20bf204ef"},
 "target":"zeroship_sandbox::wake_machine"}

{"timestamp":"2026-05-24T11:10:17.962319Z","level":"WARN",
 "fields":{"message":"wake_machine: terminal failed",
           "wake_id":"wak_033MEVpidQeYTrXMzeYRZn",
           "sandbox_id":"019e59ac-a669-7710-bc82-fcf20bf204ef",
           "error_code":"restore_failed",
           "error_message":"backend: nomad alloc terminal status=failed: Failed tasks"},
 "target":"zeroship_sandbox::wake_machine"}
```

49.38 s wall-time from `drive started` to `terminal failed`. Byte-for-byte same shape as r16/r17.

### Snapshot's restore `config.json` disks enumeration (verbatim, NEW)

Per the r17 retrospective recommendation, this cycle captures the full disk list from `/var/zeroship/ch/<sandbox_id>/restore/config.json` so future allow-list work doesn't whack-a-mole prefixes:

```
$ sudo jq -r ".disks[] | .path" /var/zeroship/ch/019e59aca6697710bc82fcf20bf204ef/restore/config.json
/opt/nomad/data/alloc/8379aeba-0d41-70bd-1b3a-61e34d842d35/ch/local/rootfs.img
/var/zeroship/ch/019e59aca6697710bc82fcf20bf204ef/workspace.img
/var/zeroship/ch/users/usr_033MEVIgxz846ckXa9dxDZ/home.img
```

So the canonical disk-list for a snapshot has **exactly 3 entries with 3 distinct prefix namespaces**:
1. **task_dir prefix** (rootfs.img — `/opt/nomad/data/alloc/<alloc_id>/ch/local/`). **Validator accepts (v6 rule).**
2. **per-sandbox prefix** (workspace.img — `/var/zeroship/ch/<sandbox_id>/`). **Validator accepts (v7 rule = C-7-LT-6).**
3. **per-user-home prefix** (home.img — `/var/zeroship/ch/users/<user_id>/`). **Validator REJECTS (v8 was supposed to add a rule = C-7-LT-7, did not).**

There is **no `disks[3]`**. Once the user-home prefix is whitelisted, all three disks pass and the next failure layer (if any) will be at a fundamentally different stage (CH `--restore` itself, network setup, or livez polling — i.e. layers we have not exercised in 18 cycles).

## Cluster bring-up

Fresh provision (no carry-over from r17; r17 cluster was torn down). Provision time: server sentinel 60 s, worker sentinel 15 s — identical to r17 (60 s / 15 s), with worker IP shifted to `10.178.0.31` (vs r17's `10.178.0.30`). All validation checks pass on a fresh cluster:

| Check | Result |
|---|---|
| `gcloud compute instances list` | `zsbx-prod-server-1` RUNNING `10.178.0.10`; `zsbx-prod-worker-1` RUNNING `10.178.0.31` |
| `curl 127.0.0.1:9091/livez` (on worker) | `{"status":"ok"}` |
| `nomad node ... Drivers.ch` | `Healthy=true Detected=true HealthDescription="ready"` |
| `sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch` | `527bf8301da5767cec8fb848f9122b2d330c082938a36cc41097aeb31e7e3388` ✓ |
| `nomad-driver-ch --version` | `nomad-driver-ch f329ba14` ✓ |
| `systemctl show zsbx-ctl -p Environment` | `SANDBOX_WAKE_RESPONSE_MODE=async` ✓ `SANDBOX_SNAPSHOT_ROOT_KEK_PATH=/etc/zeroship/snapshot-root-kek` ✓ `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` ✓ **`SANDBOX_NOMAD_CH_USER_HOME_ROOT=/var/zeroship/ch/users` (NEW v30 vs r17 — controller side of C-7-LT-7 LANDED)** |
| Worker zsbx-startup.log | `zsbx-worker-ready` reached at 11:07:24Z |

Sentinel-on-startup timing: 75 s total (faster than r16's 166 s, identical to r17).

## Validation 1 — `/livez` + ch driver + driver SHA256

| Check | Pass |
|---|---|
| `curl 127.0.0.1:9091/livez` (on worker) | ✓ |
| `nomad node` Drivers (ch Healthy=true) | ✓ |
| Driver SHA256 (`527bf830…`) | ✓ |
| Driver gitSHA (`f329ba14`) | ✓ |
| Controller env (3 vars from brief) | ✓ |
| Controller env (user_home_dir_root, NEW) | ✓ |

## Validation 2 — single CREATE / SNAPSHOT / WAKE / STOP cycle (polling-shape client)

Client: `/tmp/snapshot_stress_r18.py` (polling-aware variant, freshly uploaded — `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` in GCS still pre-async; see Carry-overs).

Invocation: `sudo PYTHONPATH=/opt/stress python3 /tmp/snapshot_stress_r18.py --label T-8b-smoke-r18 --wake-budget 240`.

| Phase | Result | Timing |
|---|---|---|
| CREATE | **OK 1/1** | 6,463 ms |
| EXEC (pre-snapshot) | **OK 1/1** | 8.4 ms |
| SNAPSHOT | **OK 1/1** | 14,496 ms (artifact `11e6ef94…`, ch_version `ch-remote v51.1+aead-cc20p1305`, 1.07 GB bytes) |
| WAKE (async polling) | **FAIL 0/1** | 49,563 ms total; POST→202 in 57 ms; 50 polls; terminal state `failed` (out of `restoring`) |
| EXEC (post-wake) | N/A (wake failed) | — |
| STOP | **OK 1/1** (admin-DELETE on already-terminal sandbox returned 200) | 20 ms |

Overall: **1 CREATE OK, 1 SNAPSHOT OK, 0 WAKE OK, 1 STOP OK.**

CREATE+SNAPSHOT carry forward from r15/r16/r17 (same shape, same wall-times within GCP variance). STOP succeeded this cycle (returned 200; prior cycles had reported STOP fail because of post-wake cleanup interactions — this cycle the client's STOP is best-effort after wake failure and the controller cleanly accepted it).

## Defect classification

**C-7-LT-8 (NEW, P0):** **driver v8 binary shipped but the validator's allow-list extension did NOT.** The intended C-7-LT-7 fix (add `/var/zeroship/ch/users/<user_id>/` as a third allow-list entry, gated on snapshot owner) is missing from the v8 binary even though gitSHA + SHA256 both changed. v8 differs from v7 in some code path, but **not** in `rewriteConfigJSON`'s allow-list literal — confirmed by the error message being byte-identical to r17's modulo IDs.

**Where the bug lives:** the Go driver source `nomad-driver-ch/ch/start_task.go` (or its `rewrite_config.go` cousin) in the function chain `startTaskRestoreBranch → rewriteConfigJSON → (disk-path validator)`. The validator's allow-list literal needs the third entry; whatever was changed in v8 was not the validator.

**Why the fix didn't land:** unknown without inspecting the v8 source. Three plausible hypotheses (cannot distinguish from this cluster cycle alone):
1. v8 was built from a branch that hadn't yet merged the C-7-LT-7 patch.
2. The C-7-LT-7 patch was authored against a different function (e.g. the wrapper's bash rewriter `nomad-vm-wrapper.sh`) and the Go validator was left untouched.
3. The patch added `user_id` plumbing into the task env / task config block but the validator still has the old hard-coded two-entry allow-list.

The retrospective for r18 should `git log -p` the driver repo between `f41a869a..f329ba14` to identify what *did* change in v8, then audit `rewriteConfigJSON` directly.

**Fix shape (DEFERRED to Approach A — see Recommendation):** instead of adding rule 3b inline, switch the validator to read the snapshot's own `config.json` for the canonical disk-list and accept those paths verbatim (with the existing safety check that they resolve under at least one of: the new task_dir, the per-sandbox dir, OR a controller-emitted owner-prefix list — but the controller now becomes the source of truth, not a hard-coded literal). See Recommendation.

**Severity:** P0 — blocks T-8b-stress entirely. WAKE cannot succeed for any snapshot whose disk-list contains the per-user home volume (which is every real workload).

**Carry-over implications:**
- **C-7-LT-3 (60 s ch.sock retrying probe):** unaffected — never gets exercised this cycle because the driver fails before invoking CH.
- **R19-I1 two-phase livez probe:** carries over deferred-verification (fifth cycle in a row — still waiting on the first cycle that reaches `livez_polling`).
- **C-7-LT-4 + C-7-LT-5 (`serial.file` rewrite):** still ARCHITECTURALLY LANDED — the rewriter is wired correctly; the validator's allow-list scope is the bug.
- **C-7-LT-6 (per-sandbox prefix):** LANDED in v7, carried unchanged into v8 — the error message still lists the per-sandbox prefix as the first allow-list entry, and the rewriter correctly accepts `/var/zeroship/ch/<sbx>/workspace.img`.
- **C-7-LT-7 (per-user-home prefix):** **NOT LANDED.** This is the regression — v8 was *intended* to ship the fix but didn't.
- **Controller user_id emission (`03d2f4a8`):** **LANDED.** Visible in the controller env block as `SANDBOX_NOMAD_CH_USER_HOME_ROOT=/var/zeroship/ch/users`, and the controller's startup log now exposes `user_home_dir_root":"/var/zeroship/ch/users"`. The plumbing on the controller side is correct — the driver just doesn't consume it.

## Distinct-signal accounting (cluster cycles since T-8b inception)

r18 is the **18th cluster cycle**. Each cycle has surfaced at least one new defect or carried over a known one:

| # | Cycle | New defect (or "carry") |
|---|---|---|
| 1–13 | (history) | C-8a, C-8b, C-8c, B18, B23, R12-IMPL-2, R19-C1, R19-I1, C-7-LT-1, C-7-LT-2, C-7-LT-3, C-7-LT-4, C-7-LT-5 |
| 14 | smoke-r14 | (greens consolidated; fence stabilised) |
| 15 | smoke-r15 | C-7-LT-4 (originally observed as `CreateConsoleDevice ENOENT`) |
| 16 | smoke-r16 | C-7-LT-6 (rewriter task_dir invariant rejects persistent disks) |
| 17 | smoke-r17 | C-7-LT-7 (per-user-home prefix missing from allow-list) |
| **18** | **smoke-r18** | **C-7-LT-8 (driver v8 binary shipped but C-7-LT-7 patch did NOT land in the validator)** |

This is the **third consecutive cycle at the path-rewriter validator layer.** r16 = layer first surfaced (workspace.img). r17 = layer drilled one disk forward (home.img). r18 = same disk, no validator change — **the C-7-LT-7 patch was supposed to fix it and didn't.** The trajectory is no longer "discover next layer"; it's "intended fix did not land."

**Per the r19 retrospective's escalation rule, 3 cycles in a row at the same validator layer signals that the per-prefix whack-a-mole pattern is structurally unstable** — and r18's specific failure mode (a published v8 binary that doesn't carry the fix) is the n+1 evidence that hand-listing prefixes one at a time will continue to miss. **Escalate to Approach A** in the recommendation below.

## Carry-overs (unchanged from r17 except where noted)

- **R19-I1 unverified (CARRIED, 5th cycle):** the two-phase livez probe in `wait_for_agent_livez` is in v30 but still not exercised. Verify in the first cycle reaching `livez_polling` (post-validator-fix).
- **Smoke harness needs upload (CARRIED, P2):** `/opt/stress/snapshot_stress.py` in GCS still pre-async. r18 used `/tmp/snapshot_stress_r18.py` (polling-aware variant uploaded fresh). Upload polling client to `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` before T-8b-stress.
- **vm_index reserve retry IDENTICAL to r16/r17:** 17 attempts × 2 s cadence, source-teardown race resolved cleanly. C-8c retry sizing is correct. **Three cycles in a row with identical 17-attempt resolution** — the race is deterministic at this cluster size.
- **Controller user_id emission (`SANDBOX_NOMAD_CH_USER_HOME_ROOT`) NEW IN ENV BLOCK:** the controller side of the C-7-LT-7 fix shipped. The variable is present in the controller env and visible in startup-log `user_home_dir_root` field. The driver is the bottleneck.

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

| Resource | Hourly | Time (cluster up ~11:05Z → 11:14Z, ~9 min) | Cost |
|---|---|---|---|
| 1 × n2-standard-4 (server) | $0.196/h | 9 min | $0.03 |
| 1 × n2-standard-32 (worker, nested-virt) | $1.554/h | 9 min | $0.23 |
| Static internal IPs (×1 in-use) | $0.000/h | 9 min | $0.00 |
| Egress / startup-script GCS pulls | flat per cycle | 1 cycle | ~$0.03 |
| **r18 total** | | | **≈ $0.29** |

Well under $30 cycle cap.

## Recommendation

**NO-GO for T-8b-stress. ESCALATE to Approach A.** Next cycle should be:

1. **DIAGNOSE first**: `git log -p f41a869a..f329ba14` on the nomad-driver-ch source tree to identify what *did* change in v8. The diff will show whether the C-7-LT-7 patch was authored against the wrong function, against the bash wrapper, or never authored at all. Until this is established, **do NOT publish v9**.

2. **Approach A — snapshot-derived disk-list enumeration**: switch the driver's `rewriteConfigJSON` allow-list from "hand-listed prefix literals" to "read the snapshot's own `config.json` for the canonical disk-list and accept the exact paths it lists, subject to safety prefixes":
   - **Safe prefixes** (the driver still hard-codes these as the *outer* membership test):
     - `<new task_dir>/` — for `serial.file`, `console.file`, and `rootfs.img` (matches the existing v6 rule).
     - `<host_state_dir>/<sandbox_id>/` — for the per-sandbox persistent volume (matches the existing v7 rule).
     - `<user_home_dir_root>/<user_id>/` — for the per-user home volume (the missing v8/C-7-LT-7 rule), where `user_home_dir_root` and `user_id` are **threaded through from the controller** via task env / task config. The controller already emits `SANDBOX_NOMAD_CH_USER_HOME_ROOT` and the snapshot owner's `user_id`; the driver just needs to read them from its task config block.
   - **Membership rule**: every path in the snapshot's `disks[]` MUST resolve under EXACTLY ONE of the three safe prefixes (or the new task_dir for rootfs.img). Anything else → reject with the existing `possible malicious snapshot or misrouted restore` message.
   - This closes the entire class of "next disk uncovers next prefix" bugs in one motion. The snapshot's `config.json` is the source of truth for what disks exist; the driver just validates them against the three controller-emitted prefix roots.

3. **Build driver v9** with Approach A, upload to `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v9`, bump pin v8 → v9. **Before publishing, run a unit-test against the snapshot's `config.json` fixture** that asserts the validator accepts all three of: `/opt/nomad/data/alloc/.../ch/local/rootfs.img`, `/var/zeroship/ch/<sbx>/workspace.img`, `/var/zeroship/ch/users/<usr>/home.img`. This is the missing CI step that would have caught r18's no-op patch before paying for the cluster cycle.

4. **Run T-8b-smoke-r19**. Predictions:
   - `fence_passed=true` (carried — sixth time).
   - `probes=2 consecutive_misses=2 elapsed_ms=300` (carried — sixth time).
   - vm_index leak counter = 0 (carried — sixth time).
   - vm_index reserve attempt count likely identical (17/36 with 2 s cadence, race resolves cleanly).
   - Driver rewriter accepts **all three** of `disks[0..2].path` (rootfs, workspace, home).
   - **State machine reaches `livez_polling` and then `ok`** — THE MILESTONE. (Falsifiable: if WAKE still fails, the failure surface will be at a *fundamentally different* layer — CH `--restore` itself, network setup, or the livez probe — because the validator class is now closed by construction.)

5. **Only IF r19 is green:** T-8b-stress (3 workers × 20 cycles) is the next checkpoint.

**Structural note for the retrospective**: r18's failure mode (a binary published as "the fix" that doesn't carry the fix) is a **release-pipeline gap** as much as a code gap. The fix-ship loop should require **a regression test driven by a representative snapshot fixture** so that an off-by-one allow-list patch (or, in this cycle's case, a no-op patch) cannot reach a cluster cycle. The CI step is cheap; the cluster cycle is $0.29 plus engineer time. **Three cycles in a row at the same validator layer is the cost evidence.**
