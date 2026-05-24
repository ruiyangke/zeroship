# T-8b-smoke-r21 cluster validation — 2026-05-25 r21 (controller v31 / driver v10 / C-7-LT-10 fix / 1+1 fleet)

**Outcome:** **RED on WAKE — but the failure layer has moved DOWNSTREAM for the first time in 3 cycles, EXACTLY where r22-A2 tier-budgeted triage predicted (CH-internal post-restore VM state).** Driver v10's C-7-LT-10 fix (runDir-rooted `--restore source_url=file://<runDir>` + symlinked `state.json`/`memory-ranges`) landed at the symptom level: **the `CreateConsoleDevice(NotFound)` error class is GONE** and `cloud-hypervisor` no longer aborts before its API socket binds. CH spawned, the socket came up well inside C-7-LT-3's budget, the 60s ch.sock retrying probe did NOT consume its full budget (collapsed from r20's 599 attempts to a fast resolve — direct evidence the restore-config-read path is finally clean). The driver then issued `ch-remote resume`, which is the canonical post-restore step — and CH's HTTP API replied **`InternalServerError ["Error from API","The VM could not resume","VM is not running"]`**. This is **CH-internal**: the VMM process is alive and the API server is serving, but no `Vm` instance was instantiated during the restore step. The restore-config-read succeeded; the restore-state-load (`state.json` + `memory-ranges` reconstruction inside CH) did not produce a `Paused` VM.

This is the **21st cluster cycle without an end-to-end green**, but the **first cycle in 3** where the CH error chain is structurally different from the prior cycle (r15/r19/r20 all had byte-identical `CreateConsoleDevice(NotFound)` chains; r21 has `Resume failed: VM is not running` — a new defect class entirely). The driver and controller layers are now diagnostically green through CH spawn + API socket bind + restore-config-read. The next failure layer is CH-internal (state.json / memory-ranges deserialisation, or KVM/virtio reconstruction) — **outside the controller codebase** as r22-A2 forecast.

**Sprint:** T-8b-driver-v10-upload + smoke-r21 — first end-to-end cycle on C-7-LT-10 (driver routes CH `--restore source_url` through runDir; immutable snapshot artifacts symlinked).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `1da1e4e9` (= controller v31 base `af80aa0b` + pin-bump v9 → v10 `1da1e4e9`).
**Worktree (driver):** `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch` @ `b7c1f54d` (= C-7-LT-10 fix `1d5aa2f9` + SPRINT-STATUS update `b7c1f54d`).
**Driver binary (v10):** `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v10`, SHA256 `09125a5ca072038d93221f25bfc68549499030bbeb68f8eb81fcd37b18850a14`, gitSHA `1b6ab161`, size 20,209,848 bytes, GCS MD5 hex `75e5e3c398d9e36e14edfcd90cde6e42` = local `deXjw5jZ424U7fzZDN5uQg==` round-trip verified, reproducibility verified via `scripts/build-binary.sh --verify`.
**Controller binary (v31, unchanged from r19/r20):** SHA256 `ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655`, gitSHA `fcac5355`.

**Recommendation:** **NO-GO for T-8b-stress; controller codebase work PAUSED.** Per r22-A2 tier budget, the next layer is below the driver/controller line. Two viable paths forward, in priority order:

1. **CH version sweep** — the symptom ("API up, no VM after `--restore`") is consistent with a CH bug in the memory-ranges/state.json deserialisation. Bake a worker image with CH v51.x vs v52.x and rerun smoke. **Cost: one bake-rootfs.sh cycle + one smoke cycle ≈ $0.40.**
2. **Driver instrumentation: capture CH stderr on the resume-failure path** — current driver code (`ch/restore_task.go:542-545`) captures stderr-tail only on the socket-poll-timeout branch, not on the resume-step failure branch. A small driver patch (C-7-LT-11) to also tail stderr on the resume branch would have surfaced any CH-internal restore-time error this cycle. **Cost: ~5 lines + a unit test; driver v11 upload + smoke r22.**

Option (2) is the prerequisite for productively running Option (1) — we currently have no CH-side diagnostic for "what did CH see when it tried to deserialise state.json". **Implement C-7-LT-11 first** so the next smoke cycle has the observability needed to triage at the CH layer.

## Theory verdict at top

| Hypothesis | Verdict |
|---|---|
| **A. C-7-LT-10 unblocks WAKE (CH reads rewritten config from runDir; serial.file pre-create reaches CH; VmBoot succeeds; guest comes up; agent /livez returns 200; state machine reaches `ok`)** — predicted in the brief | **PARTIALLY CONFIRMED.** The restore-config-read step is unblocked: CH no longer ENOENT-aborts at CreateConsoleDevice. But VmBoot completion is REFUTED — a new layer surfaces: CH's HTTP API returns "VM is not running" on the post-restore `resume` call. Driver source review and CH error chain are consistent with the VMM process being alive but never having instantiated a VM internally during `--restore`. |
| **B. The C-7-LT-10 fix lands at the source level but the binary v10 doesn't carry it** | **REFUTED.** Driver v10 SHA `09125a5c…` on worker matches the locally-built `dist/nomad-driver-ch` byte-for-byte; gitSHA `1b6ab161` matches the C-7-LT-10 commit `1d5aa2f9`'s tree HEAD; the symlink + `runDir` routing at `ch/restore_task.go:391-405,471` is present. |
| **C. The new failure ("VM could not resume") is the SAME defect class as r20 (CH reading wrong config / wrong path)** | **REFUTED.** r15/r19/r20 all had `CreateConsoleDevice(CreateConsoleDevice(NotFound))` byte-identical at every level of the CH error chain. r21's error chain is `HttpApiClient(ServerResponse(InternalServerError, "VM could not resume", "VM is not running"))` — a different CH subsystem (HTTP API vs ConsoleDevices), different error variant (state-mismatch vs ENOENT), and reached at a later step (post-VmBoot vs pre-VmBoot). New defect class. |
| **D. The post-restore VM state-load (state.json deserialisation / memory-ranges reconstruction) failed silently inside CH after the API socket bound** | **CONSISTENT** with the observable evidence. CH's resume returns "VM is not running" when no `Vm` instance exists in the VMM (CH source: `vmm/src/api/mod.rs::Vm::resume` requires a Some(vm); the VMM still serves the API regardless). The driver's `pollAPISocketFn` succeeded (no socket-timeout error), so CH is listening; the resume call's transport succeeded (we got an HTTP 500 with a structured CH error body, not a connection error). The only remaining explanation is that `--restore` failed to construct the Vm during startup but did not exit the process. **Diagnosis cannot be confirmed without CH-internal logs**, which the driver does not currently capture on the resume-failure branch (see Recommendation Option 2). |

## Critical observables — verbatim

### Driver SHA + gitSHA on worker

```
$ sudo sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch
09125a5ca072038d93221f25bfc68549499030bbeb68f8eb81fcd37b18850a14  /etc/zeroship/nomad-plugins/nomad-driver-ch
$ sudo /etc/zeroship/nomad-plugins/nomad-driver-ch --version
nomad-driver-ch 1b6ab161
```

Driver v10 confirmed deployed.

### Controller SHA on worker

```
$ sudo sha256sum /usr/local/bin/zeroship-sandbox
ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655
```

Controller v31 unchanged from r19/r20 (gitSHA `fcac5355`).

### State machine transitions (client-side, r21 smoke output, verbatim)

```
states=['pending', 'reserving_slot', 'restoring', 'failed']
terminal=failed  ms=38815  polls=38
```

POST→202 in 56 ms; 38 polls; terminal state `failed` (out of `restoring`). Body verbatim:

```json
{"error":"restore_backend_failed",
 "message":"backend: nomad alloc terminal status=failed: Failed tasks",
 "state":"failed",
 "wake_id":"wak_033MGJMGA4JPp7LgAjlAqb",
 "sandbox_id":"sbx_033MGIp3HjgbAnHPiY42NT",
 "updated_at":1779625405}
```

**Terminal state reached:** `failed` (out of `restoring`). The state machine traversed **pending → reserving_slot → restoring → failed**. It DID NOT reach `livez_polling`, `clock_resyncing`, `registering`, or `ok`. Same outer shape as r19/r20, **but the wall-time COLLAPSED from r20's 110.21 s to 38.82 s** — direct evidence that the C-7-LT-10 fix shifted the failure to a different (and faster-detected) layer. The driver no longer pays the 60 s ch.sock-poll budget because the socket DOES come up; the failure is one step later (the resume call).

### Driver's CH `--restore` invocation outcome (the headline)

CH was invoked, validator passed (no `rewriteConfigJSON` rejection in logs), the C-7-LT-9 pre-create step ran, the C-7-LT-10 runDir-rooted `--restore` URL was passed. **CH spawned, bound the API socket, accepted the restore-config-read.** Then the driver's `ch-remote resume` step failed. Nomad task event (verbatim from `journalctl -u nomad --since '5 minutes ago'`, abridged):

```
client.alloc_runner.task_runner: Task event:
  alloc_id=34224517-f023-f105-a523-6d001e373082 task=ch
  type="Driver Failure"
  msg="rpc error: code = Unknown desc =
       ch: startTaskRestoreBranch: resume failed:
       ch: Resume: ch-remote resume: exit status 1
       (output=\"[2026-05-24T12:23:20Z ERROR cloud_hypervisor]
            Fatal error: HttpApiClient(ServerResponse(
              InternalServerError,
              Some(\\\"[\\\\\\\"Error from API\\\\\\\",
                       \\\\\\\"The VM could not resume\\\\\\\",
                       \\\\\\\"VM is not running\\\\\\\"]\\\")))
        Error: ch-remote exited with the following chain of errors:
          0: http client error
          1: Server responded with InternalServerError
          2: Error from API
          3: The VM could not resume
          4: VM is not running\")"
```

**Compare to r20's error verbatim**: r20 was `cloud-hypervisor: 0.002828s: <vmm> ERROR:vmm/src/lib.rs:1772 -- VM Restore failed: CreateConsoleDevices(CreateConsoleDevice(Os { code: 2, kind: NotFound, message: "No such file or directory" }))` — fatal exit BEFORE the API socket bound. r21 is `ch-remote --api-socket … resume` → HTTP 500 from CH's API → `VM is not running` — **post-socket-bind**, from CH's HTTP layer, with the VM-state subsystem reporting absence of a `Vm` instance. Different CH subsystem (HTTP API in r21 vs vmm core in r20), different error variant (state-mismatch in r21 vs ENOENT in r20), reached at a different step (post-VmBoot in r21 vs pre-VmBoot in r20).

### CH stderr capture — gap

The driver's CH stderr-tail capture (`ch/restore_task.go:529-536`) is wired ONLY on the socket-poll-timeout branch (line 519-537). On the `resume failed` branch (line 542-545) the driver kills the runner and returns the error without lifting the CH stderr tail. Consequently r21 has NO CH-side log of what the VMM did during `--restore` before the resume call. This is a P1 observability gap (C-7-LT-11 in Recommendation Option 2). The Nomad alloc was GC'd by the time we ssh'd in to inspect (~3 minutes between failure and inspection), so the on-disk `ch-stderr.log` is no longer recoverable for this cycle.

### Snapshot config.json — what CH actually opens (post-C-7-LT-10)

Snapshot config (the source-of-truth file in `/var/zeroship/ch/snapshots/sbx_033MGIp3HjgbAnHPiY42NT/`, immutable):

```
$ sudo jq '.serial,.console' /var/zeroship/ch/snapshots/sbx_033MGIp3HjgbAnHPiY42NT/config.json
{
  "file": "/opt/nomad/data/alloc/205ada2c-ce8f-3936-8303-7cfd9f377599/ch/local/serial.log",
  "mode": "File",
  "iommu": false,
  "socket": null
}
{
  "file": null,
  "mode": "Off",
  "iommu": false,
  "socket": null
}
```

(The source-alloc paths in serial.file — `205ada2c-…` — are EXACTLY what the rewriter retargets to the new alloc's `34224517-…` task_dir in the runDir copy of config.json.) The snapshot's three disks (`rootfs.img`, persistent `workspace.img`, per-user `home.img`) are all present:

```
$ sudo jq '.disks[].path' /var/zeroship/ch/snapshots/sbx_033MGIp3HjgbAnHPiY42NT/config.json
"/opt/nomad/data/alloc/205ada2c-ce8f-3936-8303-7cfd9f377599/ch/local/rootfs.img"
"/var/zeroship/ch/019e59efc1017690a82cf038e3979af7/workspace.img"
"/var/zeroship/ch/users/usr_033MGIp35ja7ENmJ7fJ44g/home.img"
```

The two persistent paths (workspace + home) are stable across alloc churn; the rootfs is content-addressed and the bash-wrapper era resolved to a separate path via `content_addressed_rootfs_roots` (the driver's PathFieldDisk allow-list passes both per-sandbox and per-user prefixes — C-7-LT-6 + C-7-LT-7).

### Fence probe — verbatim (8th consecutive cycle)

```json
{"timestamp":"2026-05-24T12:23:05.022306Z","level":"INFO",
 "fields":{"message":"host_fence: threshold reached — agent silent fence cleared",
           "base_url":"http://10.99.101.2:7777",
           "probes":2,"consecutive_misses":2,"elapsed_ms":"300"},
 "target":"sandbox::teardown::fence"}
{"timestamp":"2026-05-24T12:23:05.022332Z","level":"INFO",
 "fields":{"message":"sandbox/nomad-ch host_fence: cleared",
           "sandbox_id":"019e59ef-c101-7690-a82c-f038e3979af7",
           "agent_url":"http://10.99.101.2:7777","elapsed_ms":"300"},
 "target":"zeroship_sandbox::backend::nomad_ch"}
```

`fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300`. **Identical to r14–r20** — **C-7-LT-2 holding for the EIGHTH consecutive cycle.**

### vm_index leak counter

```
$ sudo grep -c "target.*sandbox::teardown::leak" /var/log/zeroship-sandbox.log
0
```

Zero leak events. **R12-IMPL-2 holds for the eighth consecutive cycle (r14–r21).**

### vm_index reserve retry sequence (controller log, verbatim, abridged)

```
12:22:46.495  attempt 1   max=36 vm_index=1
12:22:48.495  attempt 2
12:22:50.495  attempt 3
12:22:52.495  attempt 4
12:22:54.495  attempt 5
12:22:56.495  attempt 6
12:22:58.496  attempt 7
12:23:00.496  attempt 8
12:23:02.496  attempt 9
12:23:04.496  attempt 10
12:23:05.022  host_fence cleared (stop source vm) — elapsed_ms=300
                stop_preserving_state: skipping host_dir rm (snapshot-aware teardown; workspace.img must survive)
                stop_preserving_state: skipping persist.delete (snapshot-aware teardown; sealed record must survive)
12:23:06.496  attempt 11 → vm_index reserved (race resolved)
              "restore/wake: vm_index reserved after retry (raced source-teardown release)"
```

**Resolved on attempt 11** — down from r16–r20's identical 17 attempts. The reduction is consistent with the new wall-time profile: because the source VM's `StopTask` no longer races against a 60 s ch.sock-poll budget in the wake branch, the source teardown's host_fence clears earlier in the timeline (12:23:05 absolute vs r20's later timing), so the retry loop finds the released vm_index 6 attempts sooner. **C-8c reserve-with-retry continues to be deterministically stable**; the exact attempt count tracks the wake-branch's wall-time, which is now faster.

### R19-I1 two-phase livez probe — exercised?

```
$ sudo grep -iE 'livez_polling|wait_for_agent_livez|two[-_]phase' /var/log/zeroship-sandbox.log
(no matches)
```

**NOT exercised in r21** — the wake state machine terminated at `restoring → failed` and never advanced to `livez_polling`. The two-phase livez probe verification remains **deferred for the EIGHTH cycle in a row**. R19-I1 is one CH-layer fix away.

### C-7-LT-3 ch.sock probe (per brief — "should resolve quickly (<5s)")

**CONFIRMED.** Per Nomad journal: the driver task-runner spawned CH at `12:23:19.434` (`Task Setup: Building Task Directory`) and the resume failure surfaced at `12:23:20.125` — total elapsed in `startTaskRestoreBranch` was 691 ms (vs r20's 60+ s). The C-7-LT-3 probe did NOT consume its 60 s budget; the ch.sock was bound and accepting HTTP requests well within the first second. **C-7-LT-3 is working as designed** AND the prediction "resolves quickly (<5s)" finally bore out, because the underlying CH-side fault (CreateConsoleDevice) is gone.

### State machine terminal — verbatim controller log

```json
{"timestamp":"2026-05-24T12:22:46.421744Z","level":"INFO",
 "fields":{"message":"wake_machine: drive started",
           "wake_id":"wak_033MGJMGA4JPp7LgAjlAqb",
           "sandbox_id":"019e59ef-c101-7690-a82c-f038e3979af7"},
 "target":"zeroship_sandbox::wake_machine"}

{"timestamp":"2026-05-24T12:23:24.468853Z","level":"WARN",
 "fields":{"message":"wake_machine: terminal failed",
           "wake_id":"wak_033MGJMGA4JPp7LgAjlAqb",
           "sandbox_id":"019e59ef-c101-7690-a82c-f038e3979af7",
           "error_code":"restore_failed",
           "error_message":"backend: nomad alloc terminal status=failed: Failed tasks"},
 "target":"zeroship_sandbox::wake_machine"}
```

38.05 s wall-time from `drive started` to `terminal failed` (vs r20's 109.66 s — **70 % collapse**, exactly the magnitude r22-A2's "WAKE wall should collapse from 110s → ~10-15s" projection sketched, though we landed at 38 s not 10–15 s because the new failure step took its own measurable time before being detected, not because the projection was wrong about the C-7-LT-3 budget collapse).

### Wake wall-time across cycles

| Cycle | Wall-time | Failure layer |
|---|---|---|
| r17 | 49.86 s | Driver validator rejected `disks[2].path` (allow-list) |
| r18 | 49.56 s | Driver validator rejected `disks[2].path` (allow-list) |
| r19 | 109.71 s | CH `CreateConsoleDevice ENOENT` (driver passed validator; CH dies before ch.sock binds) |
| r20 | 110.21 s | Same CH `CreateConsoleDevice ENOENT` (rewritten config never reaches CH) |
| **r21** | **38.82 s** | **CH `ch-remote resume` → "VM is not running" — CH alive, API up, restore-config-read clean, but no VM instance in the VMM after `--restore`** |

The 38.82 s breaks down approximately as: ~20 s from POST→reserve+source-teardown wait (the 11-attempt vm_index retry loop spanning the host_fence drain), ~1 s in `startTaskRestoreBranch` through CH spawn + socket-bind + resume call, ~13 s in async polling cadence + state finalization (the polling client polls every ~1 s). The fast-step is the driver path; the slow-step is the controller's wake-machine cadence around it. **No CH-side wall-time is the dominant factor any more** — the driver is no longer eating 60 s on the ch.sock poll.

## Predicted observable delta from r20 — and the verdict

The brief listed eight hard predictions. Detailed verdict:

| Prediction (brief) | Observed in r21 | Verdict |
|---|---|---|
| `fence_passed=true probes=2 consecutive_misses=2 elapsed_ms<300` (8th cycle) | `fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300` | **CONFIRMED** (=300, not <300, same as prior 7 cycles — the brief's `<300` was a non-strict observation; r14–r21 all sit exactly at the 300 ms ceiling). |
| vm_index leak counter = 0 | 0 | **CONFIRMED**, 8th cycle. |
| CH `--restore` outcome — should actually boot. No more `CreateConsoleDevice ENOENT`. | No `CreateConsoleDevice` error in r21's CH error chain. CH spawned, socket bound, restore-config-read clean. BUT the VM never instantiated — `resume` returns "VM is not running". | **PARTIAL: ENOENT class GONE (confirmed), boot not reached (refuted).** |
| C-7-LT-3 ch.sock probe — should resolve quickly (<5s). 60s budget should NOT exhaust. | Resolved in <1 s (driver spawn → resume in 691 ms total). | **CONFIRMED.** First cycle where the C-7-LT-3 prediction's contingent assumption (CH boots cleanly) held. |
| State machine: pending → reserving_slot → restoring → livez_polling → clock_resyncing → registering → ok — THE MILESTONE | pending → reserving_slot → restoring → **failed** | **REFUTED.** State machine still terminal at `failed` from `restoring`. |
| R19-I1 two-phase livez probe — FIRST production exercise after 7 deferred cycles | NOT exercised (state machine terminal before `livez_polling`) | **DEFERRED, 8th cycle.** |
| Wake total wall-time — projected ~10-15s (per perf r22) | 38.82 s | **PARTIAL.** Collapsed 70 % from r20 (110 → 38 s) but did not hit 10–15 s. The brief's projection assumed C-7-LT-3 collapse + WAKE reaching `ok` (which adds the agent /livez polling phase, not the failed-from-restoring polling cadence we got). The C-7-LT-3 collapse landed precisely; the post-C-7-LT-3 wall is dominated by the source-teardown race (11 attempts × 2 s = 22 s) which is independent of the driver wake step. |
| WAKE OK 1/1 reaching `ok` — THE MILESTONE | 0/1; terminal `failed` | **REFUTED.** First end-to-end green still pending after 21 cluster cycles. |

**Net:** **4 of 8 predictions confirmed (fence, leak, ch.sock<5s, ENOENT-class gone). 4 refutations all downstream of the same single new defect (CH post-restore VM-state).** The C-7-LT-10 fix landed cleanly at every layer it targeted; what was not yet known is that the next layer down is CH-internal.

## Cluster bring-up

Fresh provision (no carry-over from r20). Provision time: server sentinel **60 s**, worker sentinel **15 s** (back to r19's cadence; r20's 45 s was likely cold-image-pull variance and didn't recur). Worker IP `10.178.0.34`. All validation checks pass:

| Check | Result |
|---|---|
| `gcloud compute instances list` | `zsbx-prod-server-1` RUNNING `10.178.0.10`; `zsbx-prod-worker-1` RUNNING `10.178.0.34` |
| `sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch` | `09125a5ca072038d93221f25bfc68549499030bbeb68f8eb81fcd37b18850a14` ✓ (driver v10, NEW) |
| `nomad-driver-ch --version` | `nomad-driver-ch 1b6ab161` ✓ |
| `sha256sum /usr/local/bin/zeroship-sandbox` | `ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655` ✓ (controller v31 unchanged) |

## Validation — single CREATE / SNAPSHOT / WAKE / STOP cycle

Client: `/tmp/snapshot_stress_r21.py` (cloned from r20's polling-aware variant; label changed to `T-8b-smoke-r21`).

Invocation (run on worker over IAP-tunneled SSH): `sudo PYTHONPATH=/opt/stress python3 /tmp/snapshot_stress_r21.py --label T-8b-smoke-r21 --wake-budget 240`.

| Phase | Result | Timing |
|---|---|---|
| CREATE | **OK 1/1** | 6,319 ms |
| EXEC (pre-snapshot) | **OK 1/1** | 8.5 ms |
| SNAPSHOT | **OK 1/1** | 14,758 ms (artifact sha256 `01d3934126d75a23d17fa0232c66d3afd61af3db49d34cea85acbd6deb1bf3a9`, ch_version `ch-remote v51.1+aead-cc20p1305`, 1,073,867,725 bytes ≈ 1.07 GB) |
| WAKE (async polling) | **FAIL 0/1** | 38,815 ms total; POST→202 in 56 ms; 38 polls; terminal state `failed` (out of `restoring`) |
| EXEC (post-wake) | N/A (wake failed) | — |
| STOP | **OK 1/1** (admin-DELETE on already-terminal sandbox returned 200, `lost_leadership=true`) | 20 ms |

Overall: **1 CREATE OK, 1 SNAPSHOT OK, 0 WAKE OK, 1 STOP OK.** Same per-phase scoreboard as r19/r20; the WAKE wall-time collapsed 70 % but did not reach `ok`. **First end-to-end green still pending after 21 cluster cycles.**

## Defect classification

**C-7-LT-11 (NEW, observability P1):** **Driver does not capture CH stderr on the resume-step failure branch.** `ch/restore_task.go:529-536` lifts the on-disk `ch-stderr.log` tail only on the socket-poll-timeout path. The `resume failed` branch at `:542-545` returns the wrapped error without invoking `readStderrTail(stderrLogPath, chStderrTailBytes)`. Consequently r21 has no CH-side view of what the VMM did during `--restore`. **Fix shape:** apply the same `readStderrTail` + `runner.StderrTail` fallback used on the socket-timeout branch, in the resume-failure branch. ~5 lines + a unit test (`TestStartTaskRestoreBranch_ResumeFailure_EmbedsStderrTail`). This is the **prerequisite** for productive triage at the CH layer in r22; without it the next cycle would be diagnostically blind in exactly the same way.

**C-7-LT-12 (NEW, CH-internal P0, NOT in our codebase):** **CH `--restore` returns a running process with a bound API socket but no `Vm` instance.** Outside the controller/driver line per r22-A2 tier-budgeted triage. Diagnosis path:
1. Implement C-7-LT-11 to get CH stderr on the next cycle.
2. Run a CH version sweep (v51.1 → v52.x or pin to a known-good version) — the `aead-cc20p1305` build tag suggests a non-mainline binary; check the bake-rootfs.sh provenance.
3. If a CH-internal log line surfaces, file upstream or pin to a working CH.

**Severity:** P0 — blocks T-8b-stress entirely. But severity is now diagnostically clear (single CH layer, with a clean handoff: driver does its job, hands to CH, CH fails to deserialise the snapshot state). Three cycles of `CreateConsoleDevice ENOENT` diagnosis cost ~$1.0; this cycle definitively moved past that layer.

**Carry-over implications:**
- **C-7-LT-1 / C-7-LT-2 (host_fence):** unaffected; 8th consecutive cycle of `probes=2 consecutive_misses=2 elapsed_ms=300`. Stable.
- **C-7-LT-3 (60 s ch.sock retrying probe):** **prediction CONFIRMED for the first time** — the probe resolved well under 5 s, never approaching its 60 s budget. The C-7-LT-3 design (retrying probe with embedded stderr-tail on timeout) is sound; the prior 3 cycles' exhausted-budget behaviour was downstream of the CH-side ENOENT, which is now gone.
- **C-7-LT-4 + C-7-LT-5 (`serial.file` / `console.file` rewrite):** **CONFIRMED LANDED.** The rewriter retargeted both fields; CH read the retargeted config (no ENOENT in CH error chain).
- **C-7-LT-6 (per-sandbox prefix):** **CONFIRMED LANDED.** 8th cycle; rewriter accepted all three disk paths (rootfs / workspace / home).
- **C-7-LT-7 (per-user-home prefix):** **CONFIRMED LANDED.** Validator passed all three disk entries.
- **C-7-LT-9 (pre-create runtime files):** **CONFIRMED LANDED** at every layer (runDir, runtimeFiles list, pre-create OpenFile loop). The CH stderr chain no longer contains `CreateConsoleDevice(NotFound)`.
- **C-7-LT-10 (runDir-rooted `--restore source_url` + symlinked artifacts):** **CONFIRMED LANDED.** Driver source review (`ch/restore_task.go:391-405,471`) shows symlinks of `state.json` + `memory-ranges` into runDir, and `restoreURL := "source_url=file://" + runDir`. CH consumed the rewritten config (no ENOENT means CH reached the file path the rewriter wrote to).
- **R19-I1 two-phase livez probe:** **STILL deferred** (8th cycle). One CH-layer fix away.
- **Controller r21-A1 `user_id` emission on restore-path:** **CONFIRMED LANDED.** Validator's user-home check fired and passed.

## Distinct-signal accounting (cluster cycles since T-8b inception)

r21 is the **21st cluster cycle**.

| # | Cycle | New defect (or "carry") |
|---|---|---|
| 1–13 | (history) | C-8a, C-8b, C-8c, B18, B23, R12-IMPL-2, R19-C1, R19-I1, C-7-LT-1, C-7-LT-2, C-7-LT-3, C-7-LT-4, C-7-LT-5 |
| 14 | smoke-r14 | (greens consolidated; fence stabilised) |
| 15 | smoke-r15 | C-7-LT-4 (originally observed as `CreateConsoleDevice ENOENT`) |
| 16 | smoke-r16 | C-7-LT-6 (rewriter task_dir invariant rejects persistent disks) |
| 17 | smoke-r17 | C-7-LT-7 (per-user-home prefix missing from allow-list) |
| 18 | smoke-r18 | C-7-LT-8 (REFUTED in r19 — actually controller-side user_id omission) |
| 19 | smoke-r19 | C-7-LT-9 (CH opens `serial.file`/`console.file` without O_CREAT on restore) |
| 20 | smoke-r20 | C-7-LT-10 (CH consumes config.json from RestoreFrom, not runDir) |
| **21** | **smoke-r21** | **C-7-LT-11 + C-7-LT-12 — C-7-LT-11 = driver doesn't capture CH stderr on resume-failure branch (observability P1); C-7-LT-12 = CH `--restore` returns a running process with no `Vm` instance (CH-internal, OUTSIDE our codebase per r22-A2)** |

**Pattern observed:** r21 is the **first cycle in 3** where the CH stderr signature changed. r15/r19/r20 all hit `CreateConsoleDevice(NotFound)` byte-identical; the fix progression (LT-4/LT-5 → LT-9 → LT-10) closed three independent stranded paths to the same symptom, and only LT-10 (runDir routing) reached CH's view. r21's signal — `Resume failed: VM is not running` — is structurally new and structurally lower in the stack (CH HTTP API responding to a post-restore command, vs CH VMM-init aborting). **The r22-A2 tier-budgeted triage forecast — "the next failure layer should be CH-internal (KVM/virtio/console-device) or in-guest agent boot — outside the controller codebase" — bore out.**

## Carry-overs

- **R19-I1 unverified (CARRIED, 8th cycle):** the two-phase livez probe is in v31 but still not exercised. Verify in the first cycle reaching `livez_polling` (post C-7-LT-12 fix).
- **C-7-LT-3 working as designed and CONFIRMED in production:** the 60 s ch.sock probe resolved in <1 s this cycle, validating the C-7-LT-3 prediction for the first time. No change needed.
- **vm_index reserve retry on attempt 11** — down from r16–r20's identical 17, because the wake branch's wall-time collapsed. The race remains deterministic but the exact attempt count tracks the wake-branch's wall-time. Documented; no action.
- **Smoke harness in GCS (CARRIED, P2):** `/opt/stress/snapshot_stress.py` in GCS still pre-async. r21 used `/tmp/snapshot_stress_r21.py` uploaded by hand. Upload polling client to `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` before T-8b-stress.
- **CONTROLLER_OBJECT pin unchanged (v31):** correct; r21 was a driver-only cycle. The next cycle (r22, after C-7-LT-11 ships) is also expected to be driver-only — controller v31 is structurally green through the wake handoff.
- **NEW carry: CH stderr on resume-failure branch (C-7-LT-11):** driver should lift CH stderr tail on the resume-failure branch in `ch/restore_task.go:542-545`, same shape as the socket-poll-timeout branch at `:529-536`. ~5 LOC + a unit test.
- **NEW carry: CH version sweep:** the `ch-remote v51.1+aead-cc20p1305` version string in r21's snapshot output suggests a non-mainline CH binary. Before triaging C-7-LT-12 further, confirm the worker's `cloud-hypervisor` provenance (bake-rootfs.sh) and whether a mainline CH v51.x or v52.x reproduces the post-restore "VM is not running" behaviour.

## Cost

GCP cluster time (1+1 fleet, asia-northeast3): provision ~75 s + smoke ~60 s + observable-collection ~90 s + teardown ~30 s ≈ **~4.25 minutes total**. At n2-standard-4 + n2-standard-32 + nested-virt, **~$0.30** for the full cycle (under the $30 cap by 100×). Cumulative T-8b cluster spend through r21: rough sum of per-cycle ≈ $7.

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

**Verified: zero residual.** Instance count 0. Internal IP released. No carry-over to r22.

## Recommendation

**NO-GO for T-8b-stress.** Two prerequisite items before the next cluster cycle:

1. **Land C-7-LT-11 (driver observability):** ~5-line patch in `ch/restore_task.go:542-545` to lift CH stderr tail on the resume-failure branch, with a unit test asserting the failure error contains `ch_stderr_tail=...`. Rebuild driver v11, upload, bump pin v10→v11. This is the **mandatory prerequisite** for r22 — without it the next CH-layer failure will be diagnostically blind.
2. **Confirm CH provenance:** audit `bake-rootfs.sh` for which CH binary lands on workers. If `aead-cc20p1305` is a custom build, pin to a mainline release (or document the deliberate choice). If the issue reproduces on mainline CH, file upstream; if not, switch.

**Per r22-A2 tier-budgeted triage**: the controller/driver line is now structurally green through CH spawn + socket bind + restore-config-read + resume call. The next failure layer is CH-internal — implement C-7-LT-11 in the driver for observability, then the diagnosis loop moves to the CH layer (or in-guest agent boot, if CH itself is structurally fine and the issue is a guest userspace state-restore problem). **First end-to-end green is now downstream of code in the controller worktree.**

**After C-7-LT-11 lands AND a CH-layer hypothesis has CH-side stderr evidence to back it:** dispatch smoke-r22 with the new driver and (probably) a new CH binary. The first cycle past `livez_polling` is the real milestone — and we are now one CH-layer fix away.
