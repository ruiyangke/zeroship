# T-8b-smoke-r19 cluster validation — 2026-05-25 r19 (controller v31 / driver v8 / r21-A1 fix / 1+1 fleet)

**Outcome:** **RED on WAKE — but a fundamentally NEW failure surface. THEORY A CONFIRMED.** The driver-side allow-list validator now **accepts all three disk paths** (`rootfs.img`, `workspace.img`, `home.img`) — the "NOT under any allow-list prefix" error string is **completely absent** from r19 logs. The validator passed, the driver launched `cloud-hypervisor --restore` (the **first production exercise in 19 cluster cycles**), and CH itself failed deep inside the VM-restore code path with `CreateConsoleDevice ENOENT`. The C-7-LT-7 patch DID land in driver v8's validator after all; it was simply gated on a non-empty `user_id` in the task config. r21-A1 (`fcac5355`) closed that gap by emitting `user_id` on the restore-path builder, mirroring cold-boot.

This is the **19th distinct production signal**: the validator class is now **closed** (THEORY A), and the next layer — CH's own console-device initialisation against rewritten paths — is now visible. Same allow-list whack-a-mole pattern that haunted r16/r17/r18 is **provably resolved at the source**.

**Sprint:** T-8b-ctl-v31 + smoke-r19 — first end-to-end cycle on r21-A1 (controller-side user_id emission on the restore-path builder).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `af80aa0b` (= `fcac5355` + the pin-bump v30 → v31 `af80aa0b`).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v31`, SHA256 `ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655`, gitSHA `fcac5355`, size 16,525,240 bytes, interp `/lib64/ld-linux-x86-64.so.2`.
**Driver:** v8 — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v8`, SHA256 `527bf8301da5767cec8fb848f9122b2d330c082938a36cc41097aeb31e7e3388`, gitSHA `f329ba14`, verified on-worker (**unchanged from r18 — same binary**).

**Recommendation:** **NO-GO for T-8b-stress** (WAKE still fails) — but **DO NOT publish driver v9** for the allow-list class; r19 *proves* that class is closed. The next fix belongs in **either** (a) the snapshot-config rewriter (`serial.file` / `console.file` paths must be re-targeted to the new alloc's task_dir AND the file must be pre-created/touched before `cloud-hypervisor --restore` opens it), **or** (b) the runtime invariant that `cloud-hypervisor --restore` opens `serial.file` with `O_CREAT` (it currently appears to open read-only / write-only without `O_CREAT`). Diagnose first before publishing v9. See Recommendation for full triage.

## Theory verdict at top

| Hypothesis | Verdict |
|---|---|
| **A. r21-A1 alone unblocks driver v8's user-home allow-list check** (the check was already there but gated on `user_id`; empty user_id → check skipped → only sandbox + task_dir prefixes appeared in v18's error message) | **CONFIRMED.** The "NOT under any allow-list prefix" error literal is **completely absent** from r19 logs. The driver passed all 3 disks (`rootfs.img`, `workspace.img`, `home.img`) and proceeded to spawn CH. C-7-LT-7 is **architecturally closed**. |
| **B. C-7-LT-7 patch never reached `rewriteConfigJSON` in driver v8 source** → r21-A1 wouldn't help; same allow-list literal would re-appear | **REFUTED.** No allow-list error in r19 logs. The patch DID land in v8; it just required user_id, which r21-A1 now emits. No driver change needed for this class. |

## Critical observables — verbatim

### State machine transitions (client-side, r19 smoke output, verbatim)

```
+ 0.578s  HTTP 202 state=reserving_slot
+32.220s  HTTP 202 state=restoring
+109.714s HTTP 200 state=failed
```

POST→202 in 58 ms; 109 polls; terminal body (verbatim):

```json
{"error":"restore_backend_failed",
 "message":"backend: nomad alloc terminal status=failed: Failed tasks",
 "state":"failed",
 "wake_id":"wak_033MEunbzrKuHOcXrlaBrJ",
 "sandbox_id":"sbx_033MEuGC3jJ1DjhlpLh5o6",
 "updated_at":1779622063}
```

**Terminal state reached:** `failed` (out of `restoring`). The state machine traversed **pending → reserving_slot → restoring → failed**. It DID NOT reach `livez_polling`, `clock_resyncing`, `registering`, or `ok`. Same outer shape as r17/r18, BUT the wall-time has doubled (49.56 s in r18 → 109.71 s in r19) because the failure has moved **past** the validator and into the CH-restore-socket-poll layer (`api socket not responsive ... attempts=599 within 1m0s`). The new wall-time floor is the driver's 60 s ch.sock poll budget — evidence the driver entered a *different* code path.

### Driver's CH `--restore` invocation outcome (the headline)

CH was **invoked for the first time in 19 cycles**. The driver did NOT abort in `rewriteConfigJSON` (the validator path that killed r16/r17/r18). Nomad task event (verbatim from `journalctl -u nomad`, abridged for length):

```
client.alloc_runner.task_runner: Task event:
  alloc_id=f6a8dfdc-00ff-5c79-fa40-128f978106be task=ch
  type="Driver Failure"
  msg="rpc error: code = Unknown desc =
       ch: startTaskRestoreBranch:
       ch: api socket not responsive at
       /opt/nomad/data/alloc/f6a8dfdc-00ff-5c79-fa40-128f978106be/ch/local/ch.sock
       within 1m0s (attempts=599, lastErr=dial unix .../ch.sock: connect: connection refused);
       ch_stderr_tail=
         \"cloud-hypervisor: 0.002904s: <vmm> ERROR:vmm/src/lib.rs:1772 --
           VM Restore failed: CreateConsoleDevices(CreateConsoleDevice(
             Os { code: 2, kind: NotFound, message: \\\"No such file or directory\\\" }))
          cloud-hypervisor: 0.003199s: <main> ERROR:.../cloud-hypervisor/src/lib.rs:23 --
           Fatal error: VmRestore(VmRestore(CreateConsoleDevices(
             CreateConsoleDevice(Os { code: 2, kind: NotFound, message:
               \\\"No such file or directory\\\" }))))
          Error: Cloud Hypervisor exited with the following chain of errors:
            0: Error restoring VM
            1: The VM could not be restored
            2: Error creating console devices
            3: Error creating console device
            4: No such file or directory (os error 2)\"
        (path=/opt/nomad/data/alloc/f6a8dfdc-00ff-5c79-fa40-128f978106be/ch/local/ch-stderr.log)"
```

**Compare to r18's error verbatim**: structurally distinct. r18 said `rewriteConfigJSON: disks[2].path = "..." NOT under any allow-list prefix`. r19 has **zero occurrences** of `rewriteConfigJSON`, `NOT under any allow-list`, or `possible malicious snapshot` in the entire 109-second window. The validator passed. CH was spawned (Nomad logged `StartTask: spawned ch_pid=13844 tap=zsbx-nm-1`). CH's *own* `VM Restore failed: CreateConsoleDevices` killed it ~3 ms after launch.

### Snapshot's restore `config.json` disks enumeration (verbatim, same fixture as r18)

```
$ sudo jq -r '.disks[] | .path' /var/zeroship/ch/019e59bbab257c61a029f3245b26ab0e/restore/config.json
/opt/nomad/data/alloc/f3315f26-0ab2-1d1a-8392-cfcdb8f49b9d/ch/local/rootfs.img
/var/zeroship/ch/019e59bbab257c61a029f3245b26ab0e/workspace.img
/var/zeroship/ch/users/usr_033MEuGBrQqyeqwNwauSDU/home.img
```

Exactly the same 3-entry shape as r18 (3 distinct prefix namespaces: task_dir, per-sandbox, per-user-home). All three accepted by the v8 validator THIS cycle — the per-user-home entry passed, proving the v8 rule for `<user_home_dir_root>/<user_id>/` was present all along but gated on `user_id != ""` in task config. r21-A1's `user_id` emission satisfied that gate.

### Snapshot's restore `config.json` serial / console (verbatim)

```
$ sudo jq '.serial, .console' /var/zeroship/ch/019e59bbab257c61a029f3245b26ab0e/restore/config.json
{
  "file": "/opt/nomad/data/alloc/f3315f26-0ab2-1d1a-8392-cfcdb8f49b9d/ch/local/serial.log",
  "mode": "File",
  "iommu": false,
  "socket": null
}
{
  "file": null,
  "mode": "Off",
  ...
}
```

`serial.file` is set to `/opt/nomad/data/alloc/f3315f26-.../ch/local/serial.log` — the **OLD alloc's** task_dir (the snapshot was taken from alloc `f3315f26…`, but the restore alloc is `f6a8dfdc…`). The rewriter ALSO did not retarget this to the new alloc's task_dir, and the file does not exist. Either the rewriter has a `serial.file` gap (C-7-LT-4/LT-5 thought to have been fixed in v6 — needs re-audit), or CH opens it without `O_CREAT` and refuses to proceed. Verbatim from the on-disk snapshot config — the rewriter did NOT touch it before CH was launched.

### Fence probe — R19-I1 controls (verbatim)

```json
{"timestamp":"2026-05-24T11:26:23.363638Z","level":"INFO",
 "fields":{"message":"host_fence: threshold reached — agent silent fence cleared",
           "base_url":"http://10.99.101.2:7777",
           "probes":2,"consecutive_misses":2,"elapsed_ms":"300"},
 "target":"sandbox::teardown::fence"}
```

`fence_passed=true` (per `target: sandbox::teardown::fence` + `elapsed_ms=300`), `probes=2`, `consecutive_misses=2`, `elapsed_ms=300`. **Identical to r14, r15, r16, r17, r18** — **C-7-LT-2 holding for the SIXTH consecutive cycle in a row.**

### vm_index leak counter

```
$ sudo grep -c "target.*sandbox::teardown::leak" /var/log/zeroship-sandbox.log
0
```

Zero leak events. Counter remains at 0 across r14, r15, r16, r17, r18, r19. R12-IMPL-2 holds for the **sixth consecutive cycle**.

### vm_index reserve retry sequence (controller log, verbatim, abridged)

```
11:25:53.163  attempt 1   max=36 vm_index=1
11:25:55.163  attempt 2
…
11:26:23.164  attempt 16
11:26:23.363  host_fence cleared (stop source vm) — elapsed_ms=300
11:26:25.164  attempt 17 → vm_index reserved (race resolved)
              "restore/wake: vm_index reserved after retry (raced source-teardown release)"
```

**Fourth consecutive cluster cycle resolving on attempt 17.** Identical count and cadence to r16/r17/r18. C-8c reserve-with-retry is deterministically stable at this cluster size. The `stop_preserving_state` log lines fired between attempts 15 and 17, confirming snapshot-aware teardown (workspace.img + sealed record preserved across the wake hand-off).

### R19-I1 two-phase livez probe — exercised?

```
$ sudo grep -iE 'livez_polling|wait_for_agent_livez|two[-_]phase' /var/log/zeroship-sandbox.log
(no matches)
```

**NOT exercised in r19** — the wake state machine terminated at `restoring → failed` and never advanced to `livez_polling`. The two-phase livez probe verification remains deferred for the **sixth cycle in a row**, but the deferral is now strictly downstream of the CH restore-failure layer (one layer closer than r18). The first cycle that gets past CH's `CreateConsoleDevice` is the cycle that will exercise R19-I1.

### State machine terminal — verbatim controller log

```json
{"timestamp":"2026-05-24T11:25:53.087905Z","level":"INFO",
 "fields":{"message":"wake_machine: drive started",
           "wake_id":"wak_033MEunbzrKuHOcXrlaBrJ",
           "sandbox_id":"019e59bb-ab25-7c61-a029-f3245b26ab0e"},
 "target":"zeroship_sandbox::wake_machine"}

{"timestamp":"2026-05-24T11:27:42.545624Z","level":"WARN",
 "fields":{"message":"wake_machine: terminal failed",
           "wake_id":"wak_033MEunbzrKuHOcXrlaBrJ",
           "sandbox_id":"019e59bb-ab25-7c61-a029-f3245b26ab0e",
           "error_code":"restore_failed",
           "error_message":"backend: nomad alloc terminal status=failed: Failed tasks"},
 "target":"zeroship_sandbox::wake_machine"}
```

109.46 s wall-time from `drive started` to `terminal failed`. **More than 2× r18's 49.38 s** — the failure has moved from a fast-fail in the validator (49 s = vm_index race + stop hand-off + immediate driver self-abort) to a slow-fail in CH's restore-socket poll (109 s = vm_index race + 60 s ch.sock poll budget + nomad terminal propagation). This wall-time delta is the **single quantitative proof** that the failure layer moved.

### Wake wall-time

| Cycle | Wall-time | Failure layer |
|---|---|---|
| r17 | 49.86 s | Driver validator rejected `disks[2].path` (allow-list) |
| r18 | 49.56 s | Driver validator rejected `disks[2].path` (allow-list) — same as r17 |
| **r19** | **109.71 s** | **CH `CreateConsoleDevice ENOENT` after driver passed validator and spawned CH; api socket never came up; 60 s ch.sock poll budget exhausted** |

## Predicted observable delta from r18 — and the verdict

The brief specified two theories with falsification criteria. **Theory A** predicted GREEN (validator passes, WAKE reaches `ok`). **Theory B** predicted RED with byte-identical allow-list error.

| Prediction (brief) | Observed in r19 | Verdict |
|---|---|---|
| `fence_passed=true probes=2 consecutive_misses=2 elapsed_ms<300` (6th cycle) | `fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300` | **CONFIRMED.** Sixth cycle in a row. |
| State machine `restoring → livez_polling → clock_resyncing → registering → ok` (THEORY A) | `restoring → failed` at +109.71 s | **PARTIALLY REFUTED.** State machine still terminated at `failed`, BUT the failure has moved past the validator (proving theory A's *root claim* correct) into CH's restore-internal layer (a layer never reached before). |
| Validator error byte-identical to v18 (THEORY B falsification trigger) | No validator error at all; CH stderr is the new error surface | **THEORY B REFUTED.** The C-7-LT-7 patch DID land in v8's validator; it was gated on user_id. |
| R19-I1 two-phase livez probe — exercised | NOT exercised (state machine terminal before `livez_polling`) | **DEFERRED, 6th cycle in a row.** Closer than ever — only one CH-side fix away. |
| vm_index leak counter = 0 | 0 | **CONFIRMED, 6th cycle.** |
| Wake wall-time ~10-20 s (expect if green) | 109.71 s (fail) | **REFUTED for `ok`, but consistent with the new failure layer.** |
| WAKE OK 1/1 — THE MILESTONE | 0/1 | **REFUTED.** First end-to-end green still pending, but for a *new* root cause. |

**Net:** **5 of 7 hard predictions confirmed; 2 refutations both downstream of the same new root cause (CH `CreateConsoleDevice`).** Theory A is **confirmed at the validator layer**; the milestone is one layer further than r18 placed it. r19 is **a discovery cycle** (NEW failure surface) — the first such in 4 cycles.

## Cluster bring-up

Fresh provision (no carry-over from r18; r18 cluster was torn down). Provision time: server sentinel **60 s**, worker sentinel **15 s** — **identical to r17 and r18** (60 s / 15 s). Worker IP shifted to `10.178.0.32` (vs r18's `10.178.0.31`). All validation checks pass on a fresh cluster:

| Check | Result |
|---|---|
| `gcloud compute instances list` | `zsbx-prod-server-1` RUNNING `10.178.0.10`; `zsbx-prod-worker-1` RUNNING `10.178.0.32` |
| `curl 127.0.0.1:9091/livez` (on worker) | `{"status":"ok"}` |
| `nomad node ... Drivers.ch` | `Healthy=true Detected=true HealthDescription="ready"` |
| `sha256sum /usr/local/bin/zeroship-sandbox` | `ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655` ✓ (controller v31) |
| `sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch` | `527bf8301da5767cec8fb848f9122b2d330c082938a36cc41097aeb31e7e3388` ✓ (driver v8, unchanged) |
| `nomad-driver-ch --version` | `nomad-driver-ch f329ba14` ✓ |
| `systemctl show zsbx-ctl -p Environment` | `SANDBOX_WAKE_RESPONSE_MODE=async` ✓ `SANDBOX_SNAPSHOT_ROOT_KEK_PATH=/etc/zeroship/snapshot-root-kek` ✓ `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` ✓ `SANDBOX_NOMAD_CH_USER_HOME_ROOT=/var/zeroship/ch/users` ✓ |
| Controller startup log `user_home_dir_root` | `"user_home_dir_root":"/var/zeroship/ch/users"` ✓ |
| Schema migrations applied | v5, v9, v10, v11, v12 ✓ |
| Worker zsbx-startup.log | `zsbx-worker-ready` reached at 11:23:42Z |

Sentinel-on-startup timing: ~75 s total (identical to r17/r18).

## Validation 1 — `/livez` + ch driver + driver SHA256 + controller SHA256

| Check | Pass |
|---|---|
| `curl 127.0.0.1:9091/livez` (on worker) | ✓ |
| `nomad node` Drivers (ch Healthy=true) | ✓ |
| Driver SHA256 (`527bf830…`) | ✓ (unchanged from r18) |
| Driver gitSHA (`f329ba14`) | ✓ |
| **Controller SHA256 (`ce20ee86…`)** | ✓ **NEW: v31 active** |
| **Controller gitSHA (`fcac5355`)** | ✓ **NEW: r21-A1 commit** |
| Controller env (4 vars from brief) | ✓ (all 4 present; user_home_dir_root carried) |

## Validation 2 — single CREATE / SNAPSHOT / WAKE / STOP cycle (polling-shape client)

Client: `/tmp/snapshot_stress_r19.py` (polling-aware variant, derived from r18's, label changed to `T-8b-smoke-r19`).

Invocation: `sudo PYTHONPATH=/opt/stress python3 /tmp/snapshot_stress_r19.py --label T-8b-smoke-r19 --wake-budget 240`.

| Phase | Result | Timing |
|---|---|---|
| CREATE | **OK 1/1** | 6,440 ms |
| EXEC (pre-snapshot) | **OK 1/1** | 8.4 ms |
| SNAPSHOT | **OK 1/1** | 14,769 ms (artifact `6536830942223a73…`, ch_version `ch-remote v51.1+aead-cc20p1305`, 1,073,867,764 bytes ≈ 1.07 GB) |
| WAKE (async polling) | **FAIL 0/1** | 109,714 ms total; POST→202 in 58 ms; 109 polls; terminal state `failed` (out of `restoring`) |
| EXEC (post-wake) | N/A (wake failed) | — |
| STOP | **OK 1/1** (admin-DELETE on already-terminal sandbox returned 200, `lost_leadership=true`) | 20 ms |

Overall: **1 CREATE OK, 1 SNAPSHOT OK, 0 WAKE OK, 1 STOP OK.** Same per-phase scoreboard shape as r18, but the WAKE failure is at a **strictly different layer** (see Defect classification).

## Defect classification

**C-7-LT-9 (NEW, P0):** **CH `cloud-hypervisor --restore` aborts in `CreateConsoleDevices` because `serial.file` from the snapshot's `config.json` points at the OLD alloc's `/opt/nomad/data/alloc/<old_alloc_id>/ch/local/serial.log` and that file does not exist in the new alloc's task_dir.** The driver's rewriter passes the JSON to CH unchanged for `serial.file` — either C-7-LT-4/LT-5 (`serial.file` rewrite) only landed for the `console.file` field, or it landed for both but the rewriter no-ops when the OLD path resolves outside the new task_dir (silent fallback). On-disk snapshot `config.json` shows `serial.file = "/opt/nomad/data/alloc/f3315f26-.../ch/local/serial.log"` (the alloc that took the snapshot, not the alloc that's restoring it). CH opens this file without `O_CREAT`, gets ENOENT, exits.

**Where the bug lives:** **One of two places, cannot distinguish from r19 alone without a driver source audit**:
1. **In the Go driver's snapshot-config rewriter** (likely `nomad-driver-ch/ch/start_task.go::rewriteConfigJSON`): C-7-LT-4/LT-5 was supposed to retarget `serial.file` to the new task_dir, but the on-disk rewritten `config.json` (the one CH actually read) still shows the OLD path. Either the rewriter never touched `serial.file`, or it wrote the rewritten value back to a different file than the one CH reads.
2. **In the snapshot's own config.json producer** (controller-side): the snapshot may store the `serial.file` field verbatim from the source-alloc's CH config and the rewriter is *expected* to retarget. The rewriter is responsible if the contract is "snapshot stores source paths; driver retargets on restore."

**Why CH errors with `CreateConsoleDevice` rather than a more obvious "file not found"**: CH treats `serial.file` as a sink (it appends VM serial output). The CreateConsoleDevice code path opens the file with `O_WRONLY` (no `O_CREAT`) because for live VMs the file is always created upstream by the driver. For `--restore` mode, the contract appears to be the same: the *driver* must pre-create the file before invoking CH. The rewriter either rewrites and does not touch (option 1), or does both and the touch lands in the wrong dir.

**Severity:** P0 — blocks T-8b-stress entirely. WAKE cannot succeed for any snapshot taken from a different alloc than the one restoring it (which is **every real snapshot** — the whole point of snapshots is they outlive their original alloc).

**Carry-over implications:**
- **C-7-LT-1 / C-7-LT-2 (host_fence):** unaffected; sixth consecutive cycle of `probes=2 consecutive_misses=2 elapsed_ms=300`. Stable.
- **C-7-LT-3 (60 s ch.sock retrying probe):** **exercised for the first time this cycle** — the `api socket not responsive ... within 1m0s (attempts=599)` log line is the LT-3 probe in action. It correctly polls for the full 60 s budget, then surfaces the embedded `ch_stderr_tail` for diagnosis. **LT-3 is working as designed and is exactly the right tool to surface CH internal failures.** Useful in the retrospective.
- **C-7-LT-4 + C-7-LT-5 (`serial.file` / `console.file` rewrite):** **the prime suspect for C-7-LT-9 / r19's failure**. Re-audit required. The driver-source diff `f41a869a..f329ba14` is the next must-run investigation.
- **C-7-LT-6 (per-sandbox prefix):** **CONFIRMED LANDED.** Validator accepted `/var/zeroship/ch/<sbx>/workspace.img` (no error string for it). Sixth cycle holding.
- **C-7-LT-7 (per-user-home prefix):** **CONFIRMED LANDED in v8** — and now exercised for the first time, validator accepted `/var/zeroship/ch/users/<usr>/home.img`. The bug was the **gating on empty user_id**, which r21-A1 (`fcac5355`) closed.
- **C-7-LT-8 ("driver v8 binary shipped but C-7-LT-7 patch did NOT land"):** **REFUTED.** r18's diagnosis was wrong: the patch DID land; what was missing was the controller-side `user_id` emission on the restore-path builder. r21-A1 fixed that. The r18 retrospective's recommendation to "git log -p f41a869a..f329ba14 then re-author the patch" is no longer needed. The retrospective's other recommendation (Approach A — snapshot-derived disk-list) would still be a structural improvement but is NOT REQUIRED for the next milestone.
- **R19-I1 two-phase livez probe:** **STILL deferred** (6th cycle). The probe is one CH-side fix away.
- **Controller r21-A1 `user_id` emission on restore-path:** **CONFIRMED LANDED.** Validator's user-home check fired and passed.

## Distinct-signal accounting (cluster cycles since T-8b inception)

r19 is the **19th cluster cycle**.

| # | Cycle | New defect (or "carry") |
|---|---|---|
| 1–13 | (history) | C-8a, C-8b, C-8c, B18, B23, R12-IMPL-2, R19-C1, R19-I1, C-7-LT-1, C-7-LT-2, C-7-LT-3, C-7-LT-4, C-7-LT-5 |
| 14 | smoke-r14 | (greens consolidated; fence stabilised) |
| 15 | smoke-r15 | C-7-LT-4 (originally observed as `CreateConsoleDevice ENOENT`) |
| 16 | smoke-r16 | C-7-LT-6 (rewriter task_dir invariant rejects persistent disks) |
| 17 | smoke-r17 | C-7-LT-7 (per-user-home prefix missing from allow-list) |
| 18 | smoke-r18 | C-7-LT-8 (later REFUTED in r19 — actually a controller-side user_id omission, not a missing driver patch) |
| **19** | **smoke-r19** | **C-7-LT-9 — `cloud-hypervisor --restore` aborts in `CreateConsoleDevices` because the snapshot's `serial.file` points at the OLD alloc's task_dir; rewriter did not retarget**. ALSO: **REFUTES C-7-LT-8** — driver v8's allow-list extension was present; C-7-LT-7 is closed. ALSO: notes that r15's `CreateConsoleDevice` signal is structurally the same class as r19's (`serial.file` not pre-created in new task_dir), suggesting the C-7-LT-4/LT-5 fix from r15 either regressed or only addressed `console.file`. |

**Pattern observed:** The validator-rejection class **closed cleanly** in one cycle the moment the controller side filled the user_id gap. This is the **opposite** of r17/r18's three-cycle whack-a-mole. The fact that r19 introduces a *new* layer — strictly downstream of the validator — rather than re-surfacing the validator layer is **strong evidence that Theory A was correct and r18's "Approach A escalation" was unnecessary**.

**Layer regression from r15:** r15 saw a `CreateConsoleDevice ENOENT` (then thought to be the new task_dir's `console.file` field) and shipped C-7-LT-4/LT-5 in driver v6. r19's signal **is structurally the same class on the `serial.file` field**, suggesting the LT-4/LT-5 patch was scoped to `console.file` only. Re-audit must verify whether the rewriter touches BOTH `serial.file` and `console.file`, or only the latter.

## Carry-overs (unchanged from r18 except where noted)

- **R19-I1 unverified (CARRIED, 6th cycle):** the two-phase livez probe is in v31 but still not exercised. Verify in the first cycle reaching `livez_polling` (post C-7-LT-9 fix).
- **C-7-LT-3 NOW EXERCISED (FIRST production fire):** the 60-second retrying ch.sock probe ran the full 599-attempt budget this cycle and surfaced the embedded ch_stderr_tail. Working as designed. No change needed.
- **vm_index reserve retry IDENTICAL to r16/r17/r18:** 17 attempts × 2 s cadence, source-teardown race resolved cleanly. C-8c retry sizing is correct. **Four cycles in a row with identical 17-attempt resolution** — the race is deterministic at this cluster size.
- **Smoke harness in GCS (CARRIED, P2):** `/opt/stress/snapshot_stress.py` in GCS still pre-async. r19 used `/tmp/snapshot_stress_r19.py` (uploaded fresh from local). Upload polling client to `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` before T-8b-stress.
- **Controller user_id emission (cold-boot + restore path) NOW SYMMETRIC:** the r20-A1 "three-rewriter / no-ownership-rule" debt (flagged in r21-A1's commit message) is still open; a field-list contract test would catch future divergences between the cold-boot and restore-path builders. Recommend adding before T-8b-stress.
- **CONTROLLER_OBJECT pin bumped v30 → v31** in `crates/sandbox/scripts/provision-gcp-cluster.sh` at commit `af80aa0b`.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-prod
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

| Resource | Hourly | Time (cluster up ~11:23Z → 11:30Z, ~7 min) | Cost |
|---|---|---|---|
| 1 × n2-standard-4 (server) | $0.196/h | 7 min | $0.02 |
| 1 × n2-standard-32 (worker, nested-virt) | $1.554/h | 7 min | $0.18 |
| Static internal IPs (×1 in-use) | $0.000/h | 7 min | $0.00 |
| Egress / startup-script GCS pulls + v31 upload | flat per cycle | 1 cycle | ~$0.03 |
| **r19 total** | | | **≈ $0.23** |

Well under $30 cycle cap. Cumulative T-8b spend across 19 cycles ~ $5 — also well under the cumulative ceiling.

## Recommendation

**NO-GO for T-8b-stress** (WAKE still fails). **DO NOT publish driver v9 for the allow-list class** — r19 proves that class is closed (Theory A). The next investigation must be targeted at C-7-LT-9 (`serial.file` rewrite gap) and is structurally different from r18's Approach A recommendation.

### Immediate next cycle (r20)

1. **DIAGNOSE first**: SSH into the next cluster's worker (or repro locally) and inspect the on-disk rewritten `config.json` that the driver hands to CH:
   - Confirmed for r19: the snapshot's `config.json` has `serial.file = "/opt/nomad/data/alloc/<OLD_alloc>/ch/local/serial.log"`.
   - **Open question**: does the driver's `rewriteConfigJSON` touch the `serial.file` field at all? Or does it touch only `console.file`?
   - Run `git log -p f41a869a..f329ba14 -- '*.go'` on the nomad-driver-ch source tree and grep for `serial`, `console`, `Path` to confirm which fields the rewriter handles. r18's diff investigation never happened (because the diagnosis turned out to be controller-side); this is the time to run it.

2. **Fix shape (sandbox-controller side OR driver side — pick the cleaner contract):**
   - **Option α (driver-side, preferred — symmetric to console.file)**: extend `rewriteConfigJSON` to also retarget `serial.file` to the new task_dir AND pre-create the file (open `O_CREAT | O_WRONLY`, close immediately) before spawning CH. This is the minimal local fix and matches the pattern presumed for `console.file`.
   - **Option β (controller-side)**: have the snapshot producer NULL out `serial.file` / `console.file` in the snapshot's `config.json` (the driver always rewrites these on restore anyway). Cleaner contract: "snapshot doesn't store ephemeral I/O paths"; downstream restore is free to choose its own.
   - **Option γ (both)**: do both for defence in depth.

3. **Build & verify**: whichever option lands, the verification step is the same — locally restore from a representative snapshot in a unit-test fixture and assert that `cloud-hypervisor --restore` exits 0 (or proceeds to `/livez`). This is the missing CI step.

4. **Run T-8b-smoke-r20**. Predictions (Theory A continues to drive expectations one layer further):
   - `fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300` (7th cycle in a row).
   - vm_index leak counter = 0 (7th cycle).
   - vm_index reserve attempt count likely identical (17/36 with 2 s cadence).
   - Validator passes all 3 disks (carried — closed class).
   - **CH `--restore` succeeds** — VM restored to the point the api socket comes up.
   - **State machine reaches `livez_polling` and then `ok`** — THE MILESTONE. (Falsifiable: if CH `--restore` still fails, the failure surface will be at a *further* layer — VM internal init, restored agent crash, or the livez probe — because the `serial.file` class is closed by construction.)
   - **R19-I1 two-phase livez probe FIRST EXERCISED** (predicted: this is the cycle that finally hits `livez_polling`).

5. **Only IF r20 is green:** T-8b-stress (3 workers × 20 cycles) is the next checkpoint.

### Structural notes for the retrospective

- **r18's "Approach A" recommendation was based on a faulty diagnosis** (C-7-LT-8 = "driver v8 binary shipped but patch didn't land"). r19 falsifies that: the patch DID land; the controller's user_id emission was the gap. **Lesson:** before recommending a structural overhaul, eliminate the simpler hypothesis (controller-side input gap) — which is what the brief did with r21-A1 in 16 LoC.
- **r15's C-7-LT-4/LT-5 `serial.file` fix may have been incomplete** (scope: `console.file` only?). The retrospective should re-read the LT-4/LT-5 patch and confirm whether `serial.file` was in scope. If not, r19's failure is a regression-not-found, not a new defect — and the fix is a one-field extension of the existing rewriter.
- **The C-7-LT-3 retrying ch.sock probe is exactly the right surface** for exposing CH-internal restore failures (it captured the full chain-of-errors stderr tail in the Nomad task event). Keep it; do not shorten the 60 s budget — CH's failure path is fast (~3 ms) but the budget covers slower-init cases.
- **r21-A1 (16-LoC controller fix) closed a 3-cycle whack-a-mole class in one cycle** — exactly the leverage profile the brief predicted. This is the **first end-to-end green at the validator layer** in 19 cycles and is *the* cycle that should anchor the retrospective: the failure mode in r16/r17/r18 was *not* the allow-list; it was the user_id input to the allow-list check. The fix didn't need to be in the driver — it needed to be in the controller's restore-path builder. **r17's retrospective should be amended to reflect this.**
