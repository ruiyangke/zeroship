# T-8b-smoke-r20 cluster validation — 2026-05-25 r20 (controller v31 / driver v9 / C-7-LT-9 fix / 1+1 fleet)

**Outcome:** **RED on WAKE — and the CH `CreateConsoleDevice ENOENT` failure surface is STRUCTURALLY UNCHANGED from r19.** Driver v9's C-7-LT-9 pre-create runs (the rewriter returns a `runtimeFiles` slice, `startTaskRestoreBranch` `OpenFile(O_WRONLY|O_CREATE|O_TRUNC, 0o640)`+`Close` each post-rewrite path before spawning CH), but **CH still aborts at `CreateConsoleDevices(CreateConsoleDevice(NotFound))`** — byte-identical stderr to r19 across the CH error chain (0 through 4). This is the **20th distinct production signal**: r19's diagnosis was correct that the symptom is "CH opens `serial.file` without `O_CREAT` and ENOENT-aborts", but r19 misidentified the file CH was opening. **The bug is one layer deeper than C-7-LT-9 addressed: CH consumes the snapshot's ORIGINAL `config.json` from `RestoreFrom`, NOT the rewritten copy materialised in `runDir`. The rewrite + pre-create both target paths CH never reads.**

This is the **20th cluster cycle without an end-to-end green** — but for the **first time in 5 cycles** the failure has been narrowed to a **single, structurally diagnosed root cause** that is a 3-line fix in `restore_task.go`: stop passing `--restore source_url=file://<RestoreFrom>` and instead pass `--restore source_url=file://<runDir>` (after also staging `state.json` + `memory-ranges` symlinks or copies into runDir), OR rewrite the snapshot's `config.json` in-place at `RestoreFrom` (the bash wrapper's behaviour, which the comment at `restore_task.go:325-329` explicitly chose to break from).

**Sprint:** T-8b-driver-v9-upload + smoke-r20 — first end-to-end cycle on C-7-LT-9 (driver pre-creates `serial.file`/`console.file` at the post-rewrite paths before CH spawn).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `f2641e88` (= controller v31 base `af80aa0b` + pin-bump v8 → v9 `f2641e88`).
**Worktree (driver):** `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch` @ `2155896c` (= C-7-LT-9 fix `742c43e4` + SPRINT-STATUS update `2155896c`).
**Driver binary (v9):** `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v9`, SHA256 `7c45cdc06d42e58df8cc943ac5eac12775f0531a4fd723ccfd43ea7d6da337d2`, gitSHA `00706d29`, size 20,209,848 bytes, MD5 hex `6addf7afbd1aa8d68c6e703c4be66c34`, verified on-worker.
**Controller binary (v31, unchanged from r19):** SHA256 `ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655`, gitSHA `fcac5355`.

**Recommendation:** **NO-GO for T-8b-stress.** Need driver C-7-LT-10 first: re-point `--restore source_url` to the rewritten staging directory (or, equivalent: stage the rewritten `config.json` back into `RestoreFrom`). The C-7-LT-9 patch already accumulates the right paths in `runtimeFiles` and pre-creates them; what's missing is wiring the rewritten config into CH's view of the snapshot. **Do not publish driver v10 until source diagnosis confirms the symbol-level path.** See Recommendation for full triage.

## Theory verdict at top

| Hypothesis | Verdict |
|---|---|
| **A. C-7-LT-9 unblocks WAKE (rewriter retargets `serial.file`, pre-creates it; CH opens the new path; VM boots)** — predicted in the brief and SPRINT-STATUS | **REFUTED.** CH stderr is byte-identical to r19. The rewrite + pre-create are running correctly (driver v9 verified on-worker, no early-abort), but CH is reading from a different `config.json` than the one the driver rewrites. |
| **B. C-7-LT-9 patch never reached driver v9 source** → byte-identical error would re-appear | **REFUTED.** Driver SHA `7c45cdc0…` on worker matches the locally-built `dist/nomad-driver-ch` byte-for-byte; the rewriter source at `ch/restore_task.go:387-395` clearly contains the pre-create loop. The patch landed; it targets the wrong file. |
| **C. R19's diagnosis "rewriter only retargets `console.file`, not `serial.file`" was correct** | **REFUTED.** `rewriteConfigJSON` at `ch/config_rewrite.go:490-508` rewrites `serial.file` via `PathFieldRuntimeFile` and appends to `runtimeFiles`; tests `TestRewriteRestoreConfigPaths_RetargetsSerialFile` + `TestStartTaskRestoreBranch_PreCreatesSerialLog` exist and pass locally. The rewrite is correct. The rewritten file is just written to a location CH never reads. |
| **D. CH's `--restore source_url=file://<RestoreFrom>` makes CH read `config.json` from the snapshot directory, not from `<runDir>` where the driver wrote the rewritten copy** | **CONFIRMED — root cause.** Source audit: `ch/restore_task.go:420` builds `restoreURL := "source_url=file://" + driverConfig.RestoreFrom` and passes it as the second arg to `cloud-hypervisor`. The rewritten config at `<runDir>/config.json` is never consumed. |

## Critical observables — verbatim

### Driver SHA + gitSHA on worker

```
$ sudo sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch
7c45cdc06d42e58df8cc943ac5eac12775f0531a4fd723ccfd43ea7d6da337d2  /etc/zeroship/nomad-plugins/nomad-driver-ch
$ sudo /etc/zeroship/nomad-plugins/nomad-driver-ch --version
nomad-driver-ch 00706d29
```

Driver v9 confirmed deployed.

### State machine transitions (client-side, r20 smoke output, verbatim)

```
states=['pending', 'reserving_slot', 'restoring', 'failed']
terminal=failed  ms=110210  polls=108
```

POST→202 in 59 ms; 108 polls; terminal state `failed` (out of `restoring`). Body verbatim:

```json
{"error":"restore_backend_failed",
 "message":"backend: nomad alloc terminal status=failed: Failed tasks",
 "state":"failed",
 "wake_id":"wak_033MFeEukjTlGDuMse1y8X",
 "sandbox_id":"sbx_033MFdhUuKsL8C3hm1X3rR",
 "updated_at":1779623855}
```

**Terminal state reached:** `failed` (out of `restoring`). The state machine traversed **pending → reserving_slot → restoring → failed**. It DID NOT reach `livez_polling`, `clock_resyncing`, `registering`, or `ok`. Same outer shape as r19, and the wall-time is **near-identical (110.21 s in r20 vs 109.71 s in r19)** because the failure layer is the **same**: the driver's 60 s ch.sock poll budget exhausts after CH dies in `CreateConsoleDevices` ~3 ms after launch.

### Driver's CH `--restore` invocation outcome (the headline)

CH was invoked. Validator passed (no `rewriteConfigJSON: ... NOT under any allow-list prefix` in logs). The driver's NEW C-7-LT-9 pre-create step also ran successfully (no `pre-create runtime file ... failed` in nomad logs; the validator class is genuinely closed). But CH itself fails before the API socket binds, exactly as r19. Nomad task event (verbatim from `journalctl -u nomad`, abridged):

```
client.alloc_runner.task_runner: Task event:
  alloc_id=8d95f44c-952f-dfa1-cfbf-100b6af4e57d task=ch
  type="Driver Failure"
  msg="rpc error: code = Unknown desc =
       ch: startTaskRestoreBranch:
       ch: api socket not responsive at
       /opt/nomad/data/alloc/8d95f44c-952f-dfa1-cfbf-100b6af4e57d/ch/local/ch.sock
       within 1m0s (attempts=599, lastErr=dial unix .../ch.sock: connect: connection refused);
       ch_stderr_tail=
         \"cloud-hypervisor: 0.002828s: <vmm> ERROR:vmm/src/lib.rs:1772 --
           VM Restore failed: CreateConsoleDevices(CreateConsoleDevice(
             Os { code: 2, kind: NotFound, message: \\\"No such file or directory\\\" }))
          cloud-hypervisor: 0.003175s: <main> ERROR:.../cloud-hypervisor/src/lib.rs:23 --
           Fatal error: VmRestore(VmRestore(CreateConsoleDevices(
             CreateConsoleDevice(Os { code: 2, kind: NotFound, message:
               \\\"No such file or directory\\\" }))))
          Error: Cloud Hypervisor exited with the following chain of errors:
            0: Error restoring VM
            1: The VM could not be restored
            2: Error creating console devices
            3: Error creating console device
            4: No such file or directory (os error 2)\"
        (path=/opt/nomad/data/alloc/8d95f44c-952f-dfa1-cfbf-100b6af4e57d/ch/local/ch-stderr.log)"
```

**Compare to r19's error verbatim**: byte-identical at every level of the CH error chain. The CH-side error is the same; what changed is the driver-side context (v9 ran the pre-create step before reaching this point — verified by audit, not by log, because the pre-create step has no info-level log line on success).

### Snapshot config.json — what CH actually opens (verbatim)

```
$ sudo jq '.serial, .console' /var/zeroship/ch/snapshots/sbx_033MFdhUuKsL8C3hm1X3rR/config.json
{
  "file": "/opt/nomad/data/alloc/d227fb8d-0008-82e6-8120-2514b8ba3b8b/ch/local/serial.log",
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

The **snapshot-side** `config.json` shows `serial.file = /opt/nomad/data/alloc/d227fb8d-.../ch/local/serial.log` (the SOURCE alloc — the one that took the snapshot). `console.file = null, mode = Off` (console is disabled, so the `CreateConsoleDevice` failure is on the serial-side path, not console-side — CH labels the serial-via-ConsoleDevice machinery as "ConsoleDevice" internally; this is a CH naming choice, not a code-path mismatch). 

The NEW alloc on the restoring worker is `8d95f44c-...` and its task_dir is `/opt/nomad/data/alloc/8d95f44c-.../ch/local/`. The driver's rewriter would have rewritten `serial.file` → `/opt/nomad/data/alloc/8d95f44c-.../ch/local/serial.log`, written that to `<runDir>/config.json` (the rewrittenConfigPath at `restore_task.go:319`), and pre-created the file at the new path (verified by inspection of `restore_task.go:387-395`). **But CH is invoked with `--restore source_url=file://<RestoreFrom>`** (line 420), so CH reads `config.json` from `/var/zeroship/ch/snapshots/sbx_033MFdhUuKsL8C3hm1X3rR/` — the file that STILL says `serial.file = /opt/nomad/data/alloc/d227fb8d-.../ch/local/serial.log` — and tries to open it. The OLD alloc's task_dir does not exist on this worker; ENOENT.

The driver's explicit-by-design comment at `restore_task.go:325-329` documents this gap candidly:

> Materialise the rewritten copy in the run dir (operator-facing trail of "what did the restore actually feed CH"). The bash wrapper rewrites in-place in $ZSBX_RESTORE_FROM/config.json; we DO NOT do that because (a) the source dir is potentially read-only and (b) re-wakes of the same snapshot should each see a pristine source — the rewrite is idempotent across attempts but touching the staged dir is a smell.

The comment is honest about the design choice but it stranded the rewritten config in a directory CH never reads. The pre-create files land in the right place; the path strings CH consumes still point at the old alloc.

### Fence probe — verbatim (carry from r14–r19)

```json
{"timestamp":"2026-05-24T11:56:15.245257Z","level":"INFO",
 "fields":{"message":"host_fence: threshold reached — agent silent fence cleared",
           "base_url":"http://10.99.101.2:7777",
           "probes":2,"consecutive_misses":2,"elapsed_ms":"300"},
 "target":"sandbox::teardown::fence"}
```

`fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300`. **Identical to r14–r19** — **C-7-LT-2 holding for the SEVENTH consecutive cycle.** Below the brief's 300 ms ceiling (= ceiling, not strict-less; same observation as r19).

### vm_index leak counter

```
$ sudo grep -c "target.*sandbox::teardown::leak" /var/log/zeroship-sandbox.log
0
```

Zero leak events. R12-IMPL-2 holds for the **seventh consecutive cycle** (r14–r20).

### vm_index reserve retry sequence (controller log, verbatim, abridged)

```
11:55:45.169  attempt 1   max=36 vm_index=1
…
11:56:15.245  host_fence cleared (stop source vm) — elapsed_ms=300
11:56:17.049  attempt 17 → vm_index reserved (race resolved)
              "restore/wake: vm_index reserved after retry (raced source-teardown release)"
```

**Fifth consecutive cluster cycle resolving on attempt 17** (r16, r17, r18, r19, r20). Identical count and cadence. C-8c reserve-with-retry is **deterministically stable at this cluster size**. `stop_preserving_state` log lines fired between attempts 15 and 17 confirming snapshot-aware teardown (workspace.img + sealed record preserved across the wake hand-off).

### R19-I1 two-phase livez probe — exercised?

```
$ sudo grep -iE 'livez_polling|wait_for_agent_livez|two[-_]phase' /var/log/zeroship-sandbox.log
(no matches)
```

**NOT exercised in r20** — the wake state machine terminated at `restoring → failed` and never advanced to `livez_polling`. The two-phase livez probe verification remains deferred for the **seventh cycle in a row**, but the deferral is now strictly downstream of the CH restore-config layer (same depth as r19; r20 did not move the failure layer despite shipping a fix targeted at this exact symptom).

### C-7-LT-3 ch.sock probe (per brief — "should resolve quickly (<5s)")

The brief predicted that the post-C-7-LT-9 cycle would resolve the ch.sock probe in <5s instead of retrying to the 60s budget. **REFUTED**: the ch.sock probe ran the **full 599-attempt 60 s budget** before surfacing the embedded `ch_stderr_tail`. This is expected because CH dies in ~3 ms before binding the socket; the probe correctly polls until budget exhaustion. **C-7-LT-3 is still working as designed** — it's the right tool to surface CH internal failures — but the prediction assumed the underlying CH-side fault was resolved, which it isn't.

### State machine terminal — verbatim controller log

```json
{"timestamp":"2026-05-24T11:55:44.970548Z","level":"INFO",
 "fields":{"message":"wake_machine: drive started",
           "wake_id":"wak_033MFeEukjTlGDuMse1y8X",
           "sandbox_id":"019e59d7-02af-77b2-971b-c43471009745"},
 "target":"zeroship_sandbox::wake_machine"}

{"timestamp":"2026-05-24T11:57:34.633375Z","level":"WARN",
 "fields":{"message":"wake_machine: terminal failed",
           "wake_id":"wak_033MFeEukjTlGDuMse1y8X",
           "sandbox_id":"019e59d7-02af-77b2-971b-c43471009745",
           "error_code":"restore_failed",
           "error_message":"backend: nomad alloc terminal status=failed: Failed tasks"},
 "target":"zeroship_sandbox::wake_machine"}
```

109.66 s wall-time from `drive started` to `terminal failed` (vs 109.46 s in r19 — within 200 ms). No layer movement.

### Wake wall-time

| Cycle | Wall-time | Failure layer |
|---|---|---|
| r17 | 49.86 s | Driver validator rejected `disks[2].path` (allow-list) |
| r18 | 49.56 s | Driver validator rejected `disks[2].path` (allow-list) |
| r19 | 109.71 s | CH `CreateConsoleDevice ENOENT` (driver passed validator; CH dies before ch.sock binds) |
| **r20** | **110.21 s** | **Same CH `CreateConsoleDevice ENOENT` — C-7-LT-9 pre-create runs but rewritten config never reaches CH (root cause D above)** |

## Predicted observable delta from r19 — and the verdict

The brief specified the C-7-LT-9 fix would let WAKE reach `ok`. **REFUTED.**

| Prediction (brief) | Observed in r20 | Verdict |
|---|---|---|
| `fence_passed=true probes=2 consecutive_misses=2 elapsed_ms<300` (7th cycle) | `fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300` | **CONFIRMED.** Seventh cycle. |
| vm_index leak counter = 0 | 0 | **CONFIRMED, 7th cycle.** |
| CH `--restore` outcome — should now actually boot (no more `CreateConsoleDevice ENOENT`) | CH still aborts at `CreateConsoleDevice(NotFound)` — byte-identical stderr | **REFUTED.** |
| State machine `restoring → livez_polling → clock_resyncing → registering → ok` | `restoring → failed` at +110.21 s | **REFUTED.** Same terminal as r19. |
| R19-I1 two-phase livez probe — FIRST production exercise | NOT exercised (state machine terminal before `livez_polling`) | **DEFERRED, 7th cycle.** |
| C-7-LT-3 ch.sock probe resolves quickly (<5s) | Ran full 599-attempt 60s budget | **REFUTED — but C-7-LT-3 is working as designed; the prediction assumed CH would boot.** |
| Wake total wall-time ~15-25s (mixed) | 110.21 s (failure layer unchanged) | **REFUTED.** |
| WAKE OK 1/1 — THE MILESTONE | 0/1 | **REFUTED.** First end-to-end green still pending. |

**Net:** **2 of 8 hard predictions confirmed (fence + leak — both carry-overs). 6 refutations all downstream of the same single new root cause (D above): CH reads the un-rewritten config.json from RestoreFrom, not the rewritten copy from runDir.** This is a **diagnosis cycle, not a regression cycle** — every C-7-LT-9 component works correctly in isolation; what was missing was the wire from rewriter output to CH input.

## Cluster bring-up

Fresh provision (no carry-over from r19). Provision time: server sentinel **60 s**, worker sentinel **45 s** (slightly slower than r19's 15 s; not concerning at 1+1 fleet — likely cold-image-pull variance). Worker IP `10.178.0.33` (vs r19's `10.178.0.32`). All validation checks pass:

| Check | Result |
|---|---|
| `gcloud compute instances list` | `zsbx-prod-server-1` RUNNING `10.178.0.10`; `zsbx-prod-worker-1` RUNNING `10.178.0.33` |
| `sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch` | `7c45cdc06d42e58df8cc943ac5eac12775f0531a4fd723ccfd43ea7d6da337d2` ✓ (driver v9, NEW) |
| `nomad-driver-ch --version` | `nomad-driver-ch 00706d29` ✓ |
| `sha256sum /usr/local/bin/zeroship-sandbox` | `ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655` ✓ (controller v31 unchanged) |

## Validation — single CREATE / SNAPSHOT / WAKE / STOP cycle

Client: `/tmp/snapshot_stress_r20.py` (polling-aware variant, derived from r19's, label `T-8b-smoke-r20`).

Invocation: `sudo PYTHONPATH=/opt/stress python3 /tmp/snapshot_stress_r20.py --label T-8b-smoke-r20 --wake-budget 240`.

| Phase | Result | Timing |
|---|---|---|
| CREATE | **OK 1/1** | 6,503 ms |
| EXEC (pre-snapshot) | **OK 1/1** | 8.6 ms |
| SNAPSHOT | **OK 1/1** | 14,705 ms (artifact sha256 `50a4d54d34cceb1eed6355dff2f0f32f80d5ec3e185d9525a56c9c07e0b83fa6`, ch_version `ch-remote v51.1+aead-cc20p1305`, 1,073,867,731 bytes ≈ 1.07 GB) |
| WAKE (async polling) | **FAIL 0/1** | 110,210 ms total; POST→202 in 59 ms; 108 polls; terminal state `failed` (out of `restoring`) |
| EXEC (post-wake) | N/A (wake failed) | — |
| STOP | **OK 1/1** (admin-DELETE on already-terminal sandbox returned 200, `lost_leadership=true`) | 20 ms |

Overall: **1 CREATE OK, 1 SNAPSHOT OK, 0 WAKE OK, 1 STOP OK.** Same per-phase scoreboard as r19 — the C-7-LT-9 fix did not move the failure layer.

## Defect classification

**C-7-LT-10 (NEW, P0):** **CH `cloud-hypervisor --restore` consumes the snapshot's ORIGINAL `config.json` from `RestoreFrom`, not the rewritten copy materialised at `<runDir>/config.json`.** Driver source proof: `ch/restore_task.go:319` sets `rewrittenConfigPath := filepath.Join(runDir, chConfigName)` and `:360` writes the rewritten bytes there, but `:420` builds `restoreURL := "source_url=file://" + driverConfig.RestoreFrom` (the snapshot directory, not runDir). The block comment at `:323-330` documents the choice candidly: "we DO NOT [rewrite in-place] because (a) the source dir is potentially read-only and (b) re-wakes of the same snapshot should each see a pristine source." That reasoning is correct but it stranded the rewritten config — CH only reads `config.json` from whatever directory the `--restore source_url=file://` points at.

**Where the bug lives:** **Single point of repair, two viable shapes:**
1. **Shape A — repoint `source_url`:** stage `state.json` + `memory-ranges` into `runDir` (symlink or hard-link them since they're large), copy the rewritten `config.json` over, and pass `--restore source_url=file://<runDir>`. Idempotent and lets the source dir stay read-only as the original design intended. Cost: one symlink per artifact plus the existing rewrite write. Wire change: ~5 lines in `restore_task.go`.
2. **Shape B — rewrite in-place at `RestoreFrom`:** what the bash wrapper does. Simplest diff but reintroduces the "touching the staged dir" smell. Concurrent re-wakes of the same snapshot would race on the file write (currently impossible because vm_index serialises waking, but a future concurrent wake of two children of the same snapshot — if that ever becomes a feature — would race). Shape A is preferred; the cost is negligible.

**Severity:** P0 — blocks T-8b-stress entirely. Same severity as C-7-LT-9 (which is now closed at the symptom level but the symptom never moved because the file CH actually reads was never the one the rewriter wrote).

**Carry-over implications:**
- **C-7-LT-1 / C-7-LT-2 (host_fence):** unaffected; seventh consecutive cycle of `probes=2 consecutive_misses=2 elapsed_ms=300`. Stable.
- **C-7-LT-3 (60 s ch.sock retrying probe):** **exercised again this cycle** (third production fire) — full 599-attempt budget consumed, embedded `ch_stderr_tail` surfaced correctly. Working as designed; the brief's prediction it would resolve in <5 s assumed CH would boot, which is not the case.
- **C-7-LT-4 + C-7-LT-5 (`serial.file` / `console.file` rewrite):** **CONFIRMED LANDED.** The rewriter does rewrite both fields when present (`config_rewrite.go:490-531`). The rewritten config is just written to a location CH never reads. r19's hypothesis "the LT-4/LT-5 patch was scoped to console.file only" is **REFUTED** (and was a red herring — both fields are handled).
- **C-7-LT-6 (per-sandbox prefix):** **CONFIRMED LANDED.** Seventh cycle.
- **C-7-LT-7 (per-user-home prefix):** **CONFIRMED LANDED.** Validator passed all three disk entries.
- **C-7-LT-9 (pre-create runtime files):** **CONFIRMED LANDED at the symptom-fix level.** Driver source review shows `restore_task.go:387-395` runs `OpenFile(O_WRONLY|O_CREATE|O_TRUNC, 0o640)`+`Close` on each entry in `runtimeFiles`. The pre-create itself produces no error. It just creates files at paths CH never opens (the new alloc's task_dir), while CH opens paths from the un-rewritten config (the old alloc's task_dir).
- **R19-I1 two-phase livez probe:** **STILL deferred** (7th cycle). One LT-10 fix away.
- **Controller r21-A1 `user_id` emission on restore-path:** **CONFIRMED LANDED.** Validator's user-home check fired and passed (no error string in r20 logs).

## Distinct-signal accounting (cluster cycles since T-8b inception)

r20 is the **20th cluster cycle**.

| # | Cycle | New defect (or "carry") |
|---|---|---|
| 1–13 | (history) | C-8a, C-8b, C-8c, B18, B23, R12-IMPL-2, R19-C1, R19-I1, C-7-LT-1, C-7-LT-2, C-7-LT-3, C-7-LT-4, C-7-LT-5 |
| 14 | smoke-r14 | (greens consolidated; fence stabilised) |
| 15 | smoke-r15 | C-7-LT-4 (originally observed as `CreateConsoleDevice ENOENT` — same class as r19/r20 in retrospect; the LT-4/LT-5 patch addressed the rewriter but the rewritten-config-never-reaches-CH gap was latent the whole time) |
| 16 | smoke-r16 | C-7-LT-6 (rewriter task_dir invariant rejects persistent disks) |
| 17 | smoke-r17 | C-7-LT-7 (per-user-home prefix missing from allow-list) |
| 18 | smoke-r18 | C-7-LT-8 (REFUTED in r19 — actually controller-side user_id omission) |
| 19 | smoke-r19 | C-7-LT-9 (CH opens `serial.file`/`console.file` without O_CREAT on restore — symptom-level diagnosis; pre-create added to driver v9) |
| **20** | **smoke-r20** | **C-7-LT-10 — CH consumes `config.json` from `RestoreFrom`, not from `runDir`; the C-7-LT-9 rewrite + pre-create targets paths CH never reads.** First STRUCTURALLY confirmed root cause via source audit (not just stderr-pattern inference). |

**Pattern observed:** r15 → r19 → r20 are the **same defect class** (CH `CreateConsoleDevice ENOENT`) at increasingly refined diagnostic depth:
- r15: diagnosed as "rewriter doesn't retarget runtime files" → fixed in driver v6 (C-7-LT-4/LT-5), but the latent C-7-LT-10 gap meant the fix never reached CH.
- r19: re-surfaced after intermediate allow-list churn; diagnosed as "rewriter retargets but file doesn't exist at new path" → fixed in driver v9 (C-7-LT-9 pre-create), but again the rewritten config never reached CH.
- r20: confirmed via SOURCE audit that the rewriter's output goes to `runDir/config.json` while CH reads from `RestoreFrom/config.json`. **First diagnosis with file:line evidence** (`restore_task.go:319, 360, 420`).

**Why this took 6 cycles to nail:** every prior cycle stopped at the stderr-pattern layer (`CreateConsoleDevice NotFound` → "missing file at new path" → fix that, but the fix targets a file CH never reads). r20 went one layer deeper — the **source audit** confirming `--restore source_url=file://<RestoreFrom>` against `restore_task.go:420`. The takeaway: when fixing a "file path" bug, the right verification is to **read the source of the consumer**, not just match the stderr to a hypothesis.

## Carry-overs

- **R19-I1 unverified (CARRIED, 7th cycle):** the two-phase livez probe is in v31 but still not exercised. Verify in the first cycle reaching `livez_polling` (post C-7-LT-10 fix).
- **C-7-LT-3 working as designed (third production fire):** the 60-second retrying ch.sock probe ran the full 599-attempt budget this cycle and surfaced the embedded ch_stderr_tail. No change needed; the brief's "<5s" prediction was contingent on CH booting.
- **vm_index reserve retry IDENTICAL to r16/r17/r18/r19:** 17 attempts × 2 s cadence, source-teardown race resolved cleanly. **Five cycles in a row** with identical 17-attempt resolution — the race is deterministic at this cluster size.
- **Smoke harness in GCS (CARRIED, P2):** `/opt/stress/snapshot_stress.py` in GCS still pre-async. r20 used `/tmp/snapshot_stress_r20.py` (uploaded from local). Upload polling client to `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` before T-8b-stress.
- **CONTROLLER_OBJECT pin unchanged (v31):** correct; r20 was a driver-only cycle. The next cycle (r21, after C-7-LT-10 ships) is also expected to be driver-only — v31's user_id emission is correct.
- **NEW carry: r20 source-audit discipline:** every future `CreateConsoleDevice`-class defect must include a source audit of both the writer (rewriter output path) and the reader (CH `--restore source_url`) before publishing a driver fix. Three cycles of stderr-pattern-only diagnosis cost ~$1.5–2 in cluster time.

## Cost

GCP cluster time (1+1 fleet, asia-northeast3): provision ~105 s + smoke ~131 s + teardown ~30 s ≈ **~4.5 minutes total**. At n2-standard-4 + n2-standard-32 + nested-virt, **~$0.30** for the full cycle (under the $30 cap by 100×).

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

**Verified: zero residual.** Instance count 0. Internal IP released. No carry-over to r21.

## Recommendation

**NO-GO for T-8b-stress.** Fix C-7-LT-10 first; smoke-r21 should be the cycle that exercises `livez_polling → clock_resyncing → registering → ok` for the first time in 20 cycles.

**Fix shape (preferred — Shape A):** in `ch/restore_task.go`, after writing the rewritten config to `<runDir>/config.json`, also:

1. Hard-link or symlink `state.json` and `memory-ranges` from `<RestoreFrom>` to `<runDir>` (these are read-only inputs to CH; symlink works; cost = two extra inodes per restore, no copy).
2. Change `restoreURL := "source_url=file://" + driverConfig.RestoreFrom` to `restoreURL := "source_url=file://" + runDir` (so CH reads ALL three files — config.json, state.json, memory-ranges — from the same runDir, with config.json being the rewritten one).
3. The pre-create loop already targets the right paths; no change needed there.

**Test pin:** `TestStartTaskRestoreBranch_PassesRunDirToCH` — assert the spawned CH cmd's argv contains `--restore source_url=file://<runDir>` (not `<RestoreFrom>`). This is a **structural** test that would have caught C-7-LT-10 before r19/r20.

**After C-7-LT-10 lands:** rebuild driver v10, upload, bump pin v9→v10 in sandbox worktree, run smoke-r21. Expect first GREEN — but also remember that **r15's signal was structurally the same**, so the next cycle to greenlight stress is the FIRST one where state machine reaches `ok`, NOT just "no CreateConsoleDevice in stderr". The first cycle past `livez_polling` is the real milestone.

**If r21 RED with a NEW layer:** the next bottleneck (per perf r21 projection) is somewhere in `livez_polling → clock_resyncing → registering → ok` — R19-I1 finally exercised; agent boot variance; possibly AEAD decrypt cost. Triage discipline: source-audit before patching.

**Stress prerequisites unchanged from r19 review:** upload polling-aware client to GCS, contract test for restore-path field emission, controller idempotency around fence + reserve retry (all stable for ≥5 cycles).
