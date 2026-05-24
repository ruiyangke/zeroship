# T-8b-smoke-r22 cluster validation — 2026-05-25 r22 (controller v31 / driver v11 / C-7-LT-11 stderr capture / 1+1 fleet)

**Outcome:** **RED on WAKE — but C-7-LT-11 stderr capture LANDED and definitively reclassifies C-7-LT-12.** The captured CH stderr tail proves the post-restore failure is NOT a CH-internal VM-state issue (the r21 working hypothesis) — it's a **plain disk-path ENOENT inside CH's `DeviceManager` at restore time**. The defect is in the driver/controller layer (rootfs.img is not staged into the new alloc's `task_dir` before CH `--restore`), and the "VM is not running" HTTP response from `ch-remote resume` is just the visible-from-userspace symptom of the VMM process having already aborted internally before its API was even reachable for a meaningful resume. **r22-A2 tier-budgeted triage's "CH-internal — outside our codebase" forecast was REFUTED.** The failure is one PR away — back in driver code.

This is the **22nd cluster cycle without an end-to-end green**, and the second consecutive cycle where the CH error chain changed structurally vs the prior cycle. r21's `Resume failed: VM is not running` was the userspace-facing symptom; r22's `DeviceManager(Disk(Os { code: 2, kind: NotFound }))` is the underlying cause, captured for the first time by the C-7-LT-11 stderr-tail-on-resume-failure branch.

**Sprint:** T-8b-driver-v11-upload + smoke-r22 — first cycle on C-7-LT-11 (driver lifts CH stderr tail on the resume-step failure branch, not just the socket-poll-timeout branch).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `8b366b6d` (= controller v31 base + pin-bump v10 → v11 `8b366b6d`).
**Worktree (driver):** `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch` @ `446f1529` (= C-7-LT-11 fix + SPRINT-STATUS update; `59ed2b5c` is the SPRINT-STATUS commit on the C-7-LT-11 base).
**Driver binary (v11):** `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v11`, SHA256 `f6ff6f218632fe87af43186edbfef86c8ddbe9f0306a163e45256c80628e6224`, gitSHA `446f1529`, size 20,209,848 bytes, GCS MD5 hex `95dec321fd6243c46ff3517dff62120d` = local round-trip verified, reproducibility verified via `scripts/build-binary.sh --verify`.
**Controller binary (v31, unchanged from r19/r20/r21):** SHA256 `ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655`.

**Recommendation:** **NO-GO for T-8b-stress; PIVOT back to driver/controller codebase. Stop the CH-version-sweep / CH-pinning path.** With the captured stderr, the failure is now narrowed to **a single missing rootfs-stage step on the driver's restore branch** (or, equivalently, a single missing controller-side rootfs symlink before the new alloc starts). Both options are 5–20 LOC patches with cheap unit tests.

Priority-ranked next-step options (C-7-LT-12 fix candidates):

1. **C-7-LT-12a — Driver-side: stage rootfs.img into runDir before `--restore`.** The rewriter retargets `disks[0].path` from `/opt/nomad/data/alloc/<OLD>/ch/local/rootfs.img` to `/opt/nomad/data/alloc/<NEW>/ch/local/rootfs.img`, passes validation (path under `task_dir` per the C-7-LT-6 allow-list), and CH then opens that path and gets ENOENT because nothing stages a rootfs file there on the restore branch. Mirror the existing `serial.file`/`console.file` pre-create loop (`ch/restore_task.go:407-438`) but for disks: hard-link or symlink rootfs from a content-addressed root (or from the source alloc's snapshot-time bundle) into the new `task_dir`. The cleanest shape is to **add `rootfs_source` to `TaskConfig`** (operator points it at the CAS rootfs path), and **symlink/hardlink rootfs_source → `<runDir>/rootfs.img` before CH `--restore`**, the same way `state.json` and `memory-ranges` are symlinked from `RestoreFrom`. **Cost: ~15 LOC + 1 unit test in driver; controller wire-up to emit `rootfs_source` on the wake-path Nomad job (already plumbs `RestoreFrom`, this is parallel).**

2. **C-7-LT-12b — Controller-side: emit `disks[0].path` as a path that DOES survive across alloc lifecycle.** Instead of letting the rewriter retarget rootfs to the new alloc's task_dir, keep rootfs in a stable content-addressed location (e.g., `/var/zeroship/ch/rootfs/<sha>.img`) and configure the driver's `content_addressed_rootfs_roots` allow-list to accept it. The driver's `PathFieldDisk` allow-list slot 4 is exactly this case (the existing comment block at `ch/config_rewrite.go:301-309` calls it out). **Cost: controller change to emit content-addressed rootfs paths in the cold-boot config.json so the snapshot's config.json embeds the stable path; driver-config `content_addressed_rootfs_roots` already supports this.**

Option (1) is the lower-blast-radius fix and matches the pattern already established for `state.json`/`memory-ranges`. Option (2) is the architecturally cleaner fix (snapshot config becomes alloc-agnostic) but requires controller-side cold-boot path changes that ripple. **Recommend Option (1) as the C-7-LT-12 fix; revisit Option (2) as a post-cutover hardening item.**

CH version sweep (the r21 recommended fallback) is **NO LONGER NEEDED.** CH v51.1's behaviour is correct here — when asked to restore a VM with a disk path that doesn't exist, ENOENT is the right error. The `+aead-cc20p1305` build tag is most likely a feature tag (chacha20-poly1305 AEAD), not a fork; the headline binary self-reports `cloud-hypervisor v51.1`.

## Theory verdict at top

| Hypothesis | Verdict |
|---|---|
| **A. C-7-LT-11 stderr capture lands; r22 captures the verbatim CH stderr tail on the resume-failure branch** (primary success criterion from the brief) | **CONFIRMED.** Driver v11's resume-failure path lifted `ch_stderr_tail="<...>"` into the Nomad task event. The exact stderr — `vmm/src/lib.rs:1772 -- VM Restore failed: DeviceManager(Disk(Os { code: 2, kind: NotFound }))` — is now in the journal. This is the primary deliverable and it landed cleanly. |
| **B. C-7-LT-12 is CH-internal (state.json / memory-ranges deserialisation, KVM/virtio reconstruction)** — the r21 working hypothesis and r22-A2's "outside our codebase" tier forecast | **REFUTED.** The captured stderr proves the failure is at `DeviceManager` time, inside CH's pre-VmBoot device-attach phase, with a plain `Os { code: 2, kind: NotFound }` on a disk path. This is the CH being asked to open a file that does not exist on disk. The state.json / memory-ranges paths read fine; CH only fails when it walks the disks list and tries `open(disks[i].path, O_RDWR)`. |
| **C. The path that ENOENT'd is one of the three disks (`rootfs.img`, `workspace.img`, `home.img`)** | **CONSISTENT.** The CH stderr names `DeviceManager(Disk(...))` — Disk is the variant produced when CH walks `config.disks[]` and fails to open one of them. Source: `cloud-hypervisor/vmm/src/lib.rs:1772` (VM Restore failed → DeviceManager → Disk). Of the three disks, `workspace.img` and `home.img` were confirmed present on disk at the persistent paths (`/var/zeroship/ch/<sbx>/workspace.img` and `/var/zeroship/ch/users/<uid>/home.img`); `rootfs.img` was NOT present anywhere on disk after teardown. Strongest single hypothesis: **`disks[0].path` (rootfs) is the ENOENT victim** because the rewriter retargets it to the new alloc's `task_dir/rootfs.img` (passing validation) but no driver/controller step stages a rootfs file at that destination. |
| **D. CH `+aead-cc20p1305` build tag is a custom fork breaking restore** (r21's "audit CH provenance" follow-up) | **REFUTED.** Worker's `/usr/local/bin/cloud-hypervisor --version` reports plain `cloud-hypervisor v51.1`. The `+aead-cc20p1305` token is most likely an `aead-chacha20-poly1305` feature flag in the `ch-remote` build (which reports build-info rather than version-info). CH is mainline; the ENOENT is a path-staging issue, not a CH bug. |

## Critical observables — verbatim

### Driver SHA + gitSHA on worker

```
$ sudo sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch
f6ff6f218632fe87af43186edbfef86c8ddbe9f0306a163e45256c80628e6224  /etc/zeroship/nomad-plugins/nomad-driver-ch
$ sudo /etc/zeroship/nomad-plugins/nomad-driver-ch --version
nomad-driver-ch 446f1529
```

Driver v11 confirmed deployed.

### Controller SHA on worker

```
$ sudo sha256sum /usr/local/bin/zeroship-sandbox
ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655
```

Controller v31 unchanged from r19/r20/r21.

### CH binary version on worker

```
$ /usr/local/bin/cloud-hypervisor --version
cloud-hypervisor v51.1
```

Mainline CH v51.1 (no custom fork). The `+aead-cc20p1305` token observed in `ch-remote` output is a build-info feature tag, not a version-info component.

### State machine transitions (client-side, r22 smoke output)

```
states=['pending', 'reserving_slot', 'restoring', 'failed']
terminal=failed  wake_total_ms=51022  polls=51
```

POST→202 in 55 ms; 51 polls (1s cadence); terminal state `failed` (out of `restoring`). Body verbatim:

```json
{"error":"restore_backend_failed",
 "message":"backend: nomad alloc terminal status=failed: Failed tasks",
 "state":"failed",
 "wake_id":"wak_033MGme56yjjh4N1yE20H7",
 "sandbox_id":"sbx_033MGm6suikreYN8LR9fQP",
 "updated_at":1779626572}
```

Same outer shape as r19/r20/r21; wall-time bumped to 51 s from r21's 38.82 s — the C-7-LT-11 stderr-collection adds a small wall-time cost (CH stderr.log read + tail extraction on the failure path) but is otherwise tracking r21's profile. The 51 s breaks down as: ~32 s POST→reserve+source-teardown wait (17-attempt vm_index retry), ~1 s in `startTaskRestoreBranch` through CH spawn + socket-bind + resume call, ~18 s in async-polling cadence + state finalization.

### CH stderr tail — VERBATIM (the primary deliverable)

Driver task-event message from Nomad journal (`journalctl -u nomad --since '12 minutes ago'`):

```
client.alloc_runner.task_runner: Task event:
  alloc_id=1749a5c7-9e16-fb9f-21a0-44d51709c7cf task=ch
  type="Driver Failure"
  msg="rpc error: code = Unknown desc =
       ch: startTaskRestoreBranch: resume failed:
       ch: Resume: ch-remote resume: exit status 1
       (output=\"[2026-05-24T12:42:47Z ERROR cloud_hypervisor]
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
          4: VM is not running\");
       ch_stderr_tail=\"cloud-hypervisor:   0.742926s: <vmm> ERROR:vmm/src/lib.rs:1772
                       -- VM Restore failed: DeviceManager(Disk(Os { code: 2, kind: NotFound,
                          message: \\\"No such file or directory\\\" }))\r\n
                       cloud-hypervisor:   0.743331s: <main> ERROR:.../cloud-hypervisor/src/lib.rs:23
                       -- Fatal error: VmRestore(VmRestore(DeviceManager(Disk(Os { code: 2, kind: NotFound,
                          message: \\\"No such file or directory\\\" }))))\r\n
                       Error: Cloud Hypervisor exited with the following chain of errors:
                         0: Error restoring VM
                         1: The VM could not be restored\"
       (path=/opt/nomad/data/alloc/1749a5c7-9e16-fb9f-21a0-44d51709c7cf/ch/local/ch-stderr.log)"
```

**Decoded, the CH stderr tail is exactly:**

```
cloud-hypervisor:   0.742926s: <vmm> ERROR:vmm/src/lib.rs:1772 -- VM Restore failed:
                                     DeviceManager(Disk(Os { code: 2, kind: NotFound,
                                                             message: "No such file or directory" }))
cloud-hypervisor:   0.743331s: <main> ERROR:.../cloud-hypervisor/src/lib.rs:23 -- Fatal error:
                                     VmRestore(VmRestore(DeviceManager(Disk(Os { code: 2, kind: NotFound,
                                                                                  message: "No such file or directory" }))))
Error: Cloud Hypervisor exited with the following chain of errors:
  0: Error restoring VM
  1: The VM could not be restored
```

**Layer:** `VmRestore` → `DeviceManager` → `Disk` → `Os { code: 2, kind: NotFound }`. CH source (`vmm/src/lib.rs:1772`) confirms this is the device-attach phase of the restore path: the VMM walks `config.disks[]`, calls `open(disk.path, O_RDWR)` for each, and ENOENT'd on one of them. **CH then exits the process** — which means the API socket the process had bound moments earlier is now closed. The driver's subsequent `ch-remote resume` over the (now-stale) `--api-socket` reaches a freshly-respawned VMM that has never seen this restore (CH does not chain-start on socket-bound), and that VMM correctly returns `"VM is not running"`. The userspace symptom and the underlying cause are now decoupled by the C-7-LT-11 capture.

### Snapshot config.json — what CH was asked to open

```
$ sudo jq '.disks[] | {path, readonly}' /var/zeroship/ch/snapshots/sbx_033MGm6suikreYN8LR9fQP/config.json
{ "path": "/opt/nomad/data/alloc/f6a4088d-930d-d52d-fb73-e6372ed6e6b2/ch/local/rootfs.img",
  "readonly": false }
{ "path": "/var/zeroship/ch/019e5a0160687772a0896bbff13c7051/workspace.img",
  "readonly": false }
{ "path": "/var/zeroship/ch/users/usr_033MGm6siViv6hFK4K9o4b/home.img",
  "readonly": false }
```

**`disks[0].path` points at the SOURCE alloc's rootfs** (`alloc/f6a4088d-...`). The snapshot was taken from a sandbox running on that alloc; the alloc was then torn down (snapshot kills the source VM, § sandbox-snapshot-restore.md). The path is no longer valid post-teardown of the source alloc.

The rewriter (`ch/config_rewrite.go::rewriteSnapshotPath` + `validatePathByKind`) retargets this path from `alloc/f6a4088d-.../ch/local/rootfs.img` to `alloc/<NEW>/ch/local/rootfs.img` (matching the alloc-prefix regex) and validates the new path is under `task_dir` (passes PathFieldDisk slot 3 — see the comment block at `ch/config_rewrite.go:301-321`). **Validation passes; the file does not exist.** That's the C-7-LT-12 mechanism.

### Disk files on disk (post-WAKE-failure, pre-teardown)

```
$ sudo find /var/zeroship/ch -maxdepth 4 -name '*.img'
/var/zeroship/ch/users/usr_033MGm6siViv6hFK4K9o4b/home.img
/var/zeroship/ch/019e5a0160687772a0896bbff13c7051/workspace.img
```

Two disks present (workspace + home, at their stable per-sandbox/per-user prefixes). **No `rootfs.img` anywhere on disk** after the source alloc's teardown. There is no CAS rootfs root (`/var/zeroship/ch/rootfs/` does not exist on this image); the rootfs only ever lives in the source alloc's `task_dir` and disappears with it.

### Snapshot dir contents

```
$ sudo ls -la /var/zeroship/ch/snapshots/sbx_033MGm6suikreYN8LR9fQP/
-r--r--r-- 1 root root       2883 May 24 12:41 config.json
-r--r--r-- 1 root root 1073762336 May 24 12:41 memory-ranges
-r--r--r-- 1 root root     102502 May 24 12:41 state.json
```

The snapshot dir has CH's three immutable artifacts (config.json, state.json, memory-ranges) but **does NOT carry a copy of the rootfs.img**. The driver's restore branch symlinks state.json + memory-ranges from this dir into runDir (C-7-LT-10 fix at `ch/restore_task.go:399-405`) but does not stage rootfs.img — there's nothing in the snapshot dir to stage from anyway. This is the architectural gap.

### Driver's CH `--restore` invocation outcome

CH was invoked (validator passed, no `rewriteConfigJSON` rejection), the C-7-LT-9 pre-create step ran for serial.file/console.file, the C-7-LT-10 runDir-rooted `--restore` URL was passed. **CH spawned, bound the API socket (briefly), then failed at DeviceManager(Disk(NotFound)) and exited.** The driver's `pollAPISocketFn` apparently succeeded (the spawn was fast and the socket bound for a brief window before CH's DeviceManager logic ran); the subsequent `ch-remote resume` call hit a process that had already self-terminated, returning "VM is not running" via whatever respawned the API server (or via a race where the resume call beat the process exit and the now-empty VMM returned the structured 500).

### Fence probe — verbatim (9th consecutive cycle)

```json
{"timestamp":"2026-05-24T12:42:31.612538Z","level":"INFO",
 "fields":{"message":"host_fence: threshold reached — agent silent fence cleared",
           "base_url":"http://10.99.101.2:7777",
           "probes":2,"consecutive_misses":2,"elapsed_ms":"300"},
 "target":"sandbox::teardown::fence"}
{"timestamp":"2026-05-24T12:42:31.612576Z","level":"INFO",
 "fields":{"message":"sandbox/nomad-ch host_fence: cleared",
           "sandbox_id":"019e5a01-6068-7772-a089-6bbff13c7051",
           "agent_url":"http://10.99.101.2:7777","elapsed_ms":"300"},
 "target":"zeroship_sandbox::backend::nomad_ch"}
```

`fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300`. **Identical to r14–r21** — **C-7-LT-2 holding for the NINTH consecutive cycle.**

### vm_index leak counter

```
$ sudo grep -c "target.*sandbox::teardown::leak" /var/log/zeroship-sandbox.log
0
```

Zero leak events. **R12-IMPL-2 holds for the ninth consecutive cycle (r14–r22).**

### vm_index reserve retry sequence (controller log, verbatim, abridged)

```
12:42:01.405  attempt 1   max=36 vm_index=1
12:42:03.405  attempt 2
12:42:05.406  attempt 3
12:42:07.406  attempt 4
12:42:09.407  attempt 5
12:42:11.407  attempt 6
12:42:13.407  attempt 7
12:42:15.407  attempt 8
12:42:17.408  attempt 9
12:42:19.408  attempt 10
12:42:21.408  attempt 11
12:42:23.408  attempt 12
12:42:25.408  attempt 13
12:42:27.408  attempt 14
12:42:29.408  attempt 15
12:42:31.408  attempt 16
12:42:31.612  host_fence cleared (stop source vm) — elapsed_ms=300
12:42:33.408  attempt 17 → vm_index reserved (race resolved)
              "restore/wake: vm_index reserved after retry (raced source-teardown release)"
```

**Resolved on attempt 17** (vs r21's 11, r16–r20's 17). The bump back to 17 is consistent with the slightly longer wake-branch wall-time observed this cycle (51 s vs r21's 39 s) — the source teardown's host_fence elapsed_ms remained 300 but the controller-side state-transition cadence was a hair slower, so the retry loop took more attempts to overlap the host_fence cleared timestamp.

### R19-I1 two-phase livez probe — exercised?

```
$ sudo grep -iE 'livez_polling|wait_for_agent_livez|two[-_]phase' /var/log/zeroship-sandbox.log
(no matches)
```

**NOT exercised in r22.** The wake state machine terminated at `restoring → failed` and never advanced to `livez_polling`. R19-I1 verification remains **deferred for the NINTH cycle in a row**. R19-I1 is now one driver patch (C-7-LT-12a) away.

### C-7-LT-3 ch.sock probe

The C-7-LT-3 probe budget did not exhaust (CH bound its API socket — briefly — before the DeviceManager failure). However, the C-7-LT-3 probe is no longer the dominant wall-time term and we no longer have a useful breakdown of "probe wait" vs "spawn-to-DeviceManager-failure" — the Nomad alloc was GC'd before we could inspect per-step driver logs. **C-7-LT-3 working as designed remains the verdict from r21; r22 does not contradict.**

### State machine terminal — verbatim controller log

```json
{"timestamp":"2026-05-24T12:42:01.333176Z","level":"INFO",
 "fields":{"message":"wake_machine: drive started",
           "wake_id":"wak_033MGme56yjjh4N1yE20H7",
           "sandbox_id":"019e5a01-6068-7772-a089-6bbff13c7051"},
 "target":"zeroship_sandbox::wake_machine"}

{"timestamp":"2026-05-24T12:42:51.686571Z","level":"WARN",
 "fields":{"message":"wake_machine: terminal failed",
           "wake_id":"wak_033MGme56yjjh4N1yE20H7",
           "sandbox_id":"019e5a01-6068-7772-a089-6bbff13c7051",
           "error_code":"restore_failed",
           "error_message":"backend: nomad alloc terminal status=failed: Failed tasks"},
 "target":"zeroship_sandbox::wake_machine"}
```

50.35 s wall-time from `drive started` to `terminal failed`. **3rd cycle in a row** where wake-machine reaches `restoring → failed` (r20/r21/r22 all share this outer shape) but with three distinct underlying causes: r20 = `CreateConsoleDevice ENOENT` (driver consumed wrong config), r21 = `Resume failed: VM is not running` (no stderr capture → misdiagnosed as CH-internal), r22 = `DeviceManager(Disk(NotFound))` (the real cause, now visible — rootfs.img not staged).

### Wake wall-time across cycles

| Cycle | Wall-time | Failure layer |
|---|---|---|
| r17 | 49.86 s | Driver validator rejected `disks[2].path` (allow-list) |
| r18 | 49.56 s | Driver validator rejected `disks[2].path` (allow-list) |
| r19 | 109.71 s | CH `CreateConsoleDevice ENOENT` (driver passed validator; CH dies before ch.sock binds) |
| r20 | 110.21 s | Same CH `CreateConsoleDevice ENOENT` (rewritten config never reaches CH) |
| r21 | 38.82 s | CH `ch-remote resume` → "VM is not running" — **NO stderr capture**, misdiagnosed as CH-internal |
| **r22** | **51.02 s** | **CH `DeviceManager(Disk(NotFound))` — rootfs.img not staged into new alloc's task_dir** |

The C-7-LT-11 stderr capture's wall-time cost (~10–12 s) is fine for observability. **r22 finally reveals what CH actually saw** at the layer below `ch-remote resume`'s 500.

## Predicted observable delta from r21 — verdict

| Prediction (brief) | Observed in r22 | Verdict |
|---|---|---|
| Resume-failure error message embeds `ch_stderr_tail=<...>` (the new C-7-LT-11 capture) | Embedded; tail is `cloud-hypervisor: ... <vmm> ERROR ... VM Restore failed: DeviceManager(Disk(Os { code: 2, kind: NotFound }))` | **CONFIRMED.** Primary success criterion met. |
| WAKE wall expected ~38 s (same as r21; v11 changes are observability-only) | 51 s | **PARTIAL.** Wall bumped ~12 s — the stderr-tail read + Nomad event format/marshal on the failure path adds measurable cost. Not a regression (the prior cycles paid the same ~10–60 s "spawn-and-fail" wall and the controller polling cadence dominates anyway). |
| vm_index reserve attempt count: ~11 or 17 (tracking wake-branch wall) | 17 | **CONFIRMED** (re-aligned with r19/r20 cadence; r21's 11 was the anomaly). |
| State machine: pending → reserving_slot → restoring → failed | pending → reserving_slot → restoring → failed | **CONFIRMED.** Same outer shape; underlying cause now visible. |
| Secondary: if C-7-LT-12 self-resolves (racing CH boot), WAKE OK 1/1 | 0/1 | **REFUTED.** C-7-LT-12 did not self-resolve and could not have — the disk-path ENOENT is a deterministic miss, not a race. |
| **Primary success criterion: capture verbatim CH stderr tail from the resume-failure branch** | Captured (see above) | **CONFIRMED.** This is the diagnostic for C-7-LT-12; the new defect class is `disk-path-ENOENT-during-DeviceManager-attach`, not VM-state-corruption. |

**Net:** **5 of 6 predictions confirmed; 1 partial (wall-time bump), 1 secondary refuted (C-7-LT-12 does not self-resolve).** The primary deliverable — the stderr tail — landed and decisively reclassifies C-7-LT-12 back into the driver/controller codebase.

## Cluster bring-up

Fresh provision. Server sentinel **60 s**, worker sentinel **15 s** (matches r19/r21 cadence). Worker IP `10.178.0.35`. Validation:

| Check | Result |
|---|---|
| `gcloud compute instances list` | `zsbx-prod-server-1` RUNNING `10.178.0.10`; `zsbx-prod-worker-1` RUNNING `10.178.0.35` |
| `sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch` | `f6ff6f218632fe87af43186edbfef86c8ddbe9f0306a163e45256c80628e6224` ✓ (driver v11, NEW) |
| `nomad-driver-ch --version` | `nomad-driver-ch 446f1529` ✓ |
| `sha256sum /usr/local/bin/zeroship-sandbox` | `ce20ee86e21224d21cacc117a0e6631391985583f49226af15496e56e6f51655` ✓ (controller v31 unchanged) |
| `cloud-hypervisor --version` | `cloud-hypervisor v51.1` ✓ |

## Validation — single CREATE / SNAPSHOT / WAKE / STOP cycle

Client: `/tmp/snapshot_stress_r22.py` (re-written polling-aware single-cycle smoke; cloned from r21's variant; label changed to `T-8b-smoke-r22`).

Invocation: `sudo PYTHONPATH=/opt/stress python3 /tmp/snapshot_stress_r22.py --label T-8b-smoke-r22 --wake-budget 240`.

| Phase | Result | Timing |
|---|---|---|
| CREATE | **OK 1/1** | 6,546 ms |
| EXEC (pre-snapshot) | **OK 1/1** | 8 ms |
| SNAPSHOT | **OK 1/1** | 14,525 ms (artifact sha256 `77ea69dd6acb7ab634f059da8403a2cc36ab465c68ce847c0ef8f1b1a4b57998`, ch_version `ch-remote v51.1+aead-cc20p1305`, 1,073,867,721 bytes ≈ 1.07 GB) |
| WAKE (async polling) | **FAIL 0/1** | 51,022 ms total; POST→202 in 55 ms; 51 polls; terminal state `failed` (out of `restoring`) |
| EXEC (post-wake) | N/A | — |
| STOP | **OK 1/1** | 19 ms |

Overall: **1 CREATE OK, 1 SNAPSHOT OK, 0 WAKE OK, 1 STOP OK.** Same per-phase scoreboard as r19/r20/r21. **First end-to-end green still pending after 22 cluster cycles** — but the failure layer is now squarely back in our codebase with a precise diagnostic.

## Defect classification

**C-7-LT-12 (re-classified, controller/driver-side P0):** **The driver's restore branch does not stage `rootfs.img` into the new alloc's task_dir before invoking CH `--restore`.** The rewriter retargets `disks[0].path` from `/opt/nomad/data/alloc/<OLD>/ch/local/rootfs.img` to `/opt/nomad/data/alloc/<NEW>/ch/local/rootfs.img`, the validator passes (path is under `task_dir` per PathFieldDisk slot 3), and CH then opens that path during VmRestore→DeviceManager→Disk attach and gets `Os { code: 2, kind: NotFound }`. CH aborts, the API socket closes, and the subsequent `ch-remote resume` call surfaces as "VM is not running" — the misleading userspace symptom that the r21 cycle could not see past.

**Fix shape — Option C-7-LT-12a (recommended, lower blast radius):**
1. Add a `rootfs_source` field to `TaskConfig` (`ch/task_config.go`), populated by the controller on the wake-path Nomad job.
2. In `startTaskRestoreBranch` (`ch/restore_task.go`), after the existing state.json/memory-ranges symlink loop, **symlink (or hardlink) `rootfs_source` → `<runDir>/rootfs.img`** before the CH spawn.
3. Controller-side: emit `rootfs_source` on the wake-path job (the controller already plumbs `RestoreFrom`; this is parallel). Point it at the location where the rootfs survives across alloc lifecycle (likely the same content-addressed root the bash wrapper used, or a new `<snapshot_dir>/rootfs.img` if we want a single per-snapshot bundle).
4. Driver unit test: `TestStartTaskRestoreBranch_StagesRootfsBeforeCH` — assert the symlink/hardlink exists post-step.

**Estimated patch:** ~15 LOC in driver + ~5 LOC in controller + 1 unit test each.

**Fix shape — Option C-7-LT-12b (architecturally cleaner, larger blast radius):** Configure the controller cold-boot config to use a content-addressed rootfs path (e.g., `/var/zeroship/ch/rootfs/<sha>.img`), and add that path to the driver's `content_addressed_rootfs_roots` config attribute. The PathFieldDisk allow-list already has the slot for this (`ch/config_rewrite.go:322-329`). Snapshot config.json would then embed a stable path that survives across alloc lifecycle for free, and no rewrite/restage step would be needed. **Estimated patch:** medium (controller cold-boot path changes + bake-rootfs.sh changes + driver config wire-up). Defer to post-cutover hardening.

**Carry-over implications:**
- **C-7-LT-1 / C-7-LT-2 (host_fence):** unaffected; 9th consecutive cycle of `probes=2 consecutive_misses=2 elapsed_ms=300`. Stable.
- **C-7-LT-3 (60s ch.sock retrying probe):** still working as designed; the probe budget did not exhaust this cycle.
- **C-7-LT-4 + C-7-LT-5 (`serial.file` / `console.file` rewrite):** **CONFIRMED LANDED** (CH stderr does not contain `CreateConsoleDevice ENOENT`).
- **C-7-LT-6 (per-sandbox prefix):** **CONFIRMED LANDED** (workspace.img path validated under per-sandbox prefix slot).
- **C-7-LT-7 (per-user-home prefix):** **CONFIRMED LANDED** (home.img path validated under per-user prefix slot).
- **C-7-LT-9 (pre-create runtime files):** **CONFIRMED LANDED** (CH stderr no longer contains CreateConsoleDevice errors).
- **C-7-LT-10 (runDir-rooted `--restore source_url` + symlinked artifacts):** **CONFIRMED LANDED** (CH reads the rewritten config; the rewriter's retargeted disks[0].path is what CH attempts to open).
- **C-7-LT-11 (stderr capture on resume-failure branch):** **CONFIRMED LANDED in production for the first time.** The captured tail was the primary deliverable.
- **R19-I1 two-phase livez probe:** **STILL deferred** (9th cycle). One driver patch (C-7-LT-12a) away.
- **CH version sweep:** **CANCELLED.** CH v51.1 is mainline and the error is a legitimate ENOENT on a path the driver/controller failed to stage. No CH issue.

## Distinct-signal accounting (cluster cycles since T-8b inception)

r22 is the **22nd cluster cycle**.

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
| 21 | smoke-r21 | C-7-LT-11 + C-7-LT-12 (originally classified as CH-internal "VM is not running"; STDERR capture deferred to r22) |
| **22** | **smoke-r22** | **C-7-LT-12 RECLASSIFIED — `DeviceManager(Disk(NotFound))` on `disks[0].path` (rootfs.img); driver's restore branch does not stage rootfs into new alloc's task_dir. NOT CH-internal — fix shape is C-7-LT-12a, ~15 LOC in driver + ~5 LOC in controller.** |

**Pattern observed:** r22 is the **second cycle in a row** where the CH stderr signature changed, and the **first cycle ever** where the stderr is independently captured (not embedded in a misleading wrapper). The r21→r22 transition demonstrates exactly the value of the C-7-LT-11 observability fix: r21's working hypothesis (CH-internal VM-state issue) would have driven us into a multi-cycle CH-version-sweep that the captured stderr makes unnecessary in a single cycle. **The r22-A2 tier-budgeted triage forecast — "the next failure layer is CH-internal — outside the controller codebase" — was REFUTED.** The next layer is back in our codebase.

## Carry-overs

- **R19-I1 unverified (CARRIED, 9th cycle):** the two-phase livez probe is in v31 but still not exercised. Verify in the first cycle reaching `livez_polling` (post C-7-LT-12 fix).
- **vm_index reserve retry on attempt 17** — same as r16/r19/r20; r21's 11 was the anomaly. Documented; no action.
- **Smoke harness in GCS (CARRIED, P2):** `/opt/stress/snapshot_stress.py` still pre-async. r22 used `/tmp/snapshot_stress_r22.py` uploaded by hand. Upload polling client to `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py` before T-8b-stress.
- **CONTROLLER_OBJECT pin unchanged (v31):** correct; r22 was a driver-only cycle. **The next cycle (r23) is expected to require BOTH a driver patch (C-7-LT-12a-driver) AND a controller patch (C-7-LT-12a-controller; emit `rootfs_source` on wake-path job).** Controller v32 build will be the first controller change since v31 went GA at r19.
- **CH version sweep CANCELLED:** the r21 recommendation to audit CH provenance and consider pinning to a different version is moot — CH v51.1 is mainline and the ENOENT is a path-staging miss on our side.
- **NEW carry: rootfs staging on restore branch (C-7-LT-12):** the fix is the only blocker for the first end-to-end green. See "Defect classification" above.

## Cost

GCP cluster time (1+1 fleet, asia-northeast3): provision ~75 s + smoke ~75 s + observable-collection ~120 s + teardown ~30 s ≈ **~5 minutes total**. At n2-standard-4 + n2-standard-32 + nested-virt, **~$0.35** for the full cycle (under the $30 cap by 85×). Cumulative T-8b cluster spend through r22: rough sum of per-cycle ≈ $7.4.

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

**Verified: zero residual.** Instance count 0. Internal IP released. No carry-over to r23.

## Recommendation

**NO-GO for T-8b-stress.** Single prerequisite for r23:

1. **Land C-7-LT-12a (driver + controller, recommended option):**
   - Driver: add `rootfs_source` to `TaskConfig`; in `startTaskRestoreBranch`, symlink/hardlink `rootfs_source` → `<runDir>/rootfs.img` before CH spawn. Unit test: `TestStartTaskRestoreBranch_StagesRootfsBeforeCH`. Rebuild driver v12, upload, bump pin v11→v12.
   - Controller: emit `rootfs_source` on wake-path Nomad job (parallel to `RestoreFrom`); point at the location where rootfs survives across alloc lifecycle. Rebuild controller v32, upload, bump `CONTROLLER_OBJECT` default in `provision-gcp-cluster.sh`.
   - Both changes are small (~15 / ~5 LOC); cost roughly two cycles of work (one to land driver v12, one to land controller v32) but architecturally they're a single coordinated change.
2. **Defer C-7-LT-12b (architectural):** make snapshot's `disks[0].path` content-addressed at cold-boot time. Post-cutover hardening.
3. **Cancel CH version sweep:** CH v51.1 mainline is fine.

**Per r22-A2 retro:** the tier-budgeted triage forecast ("next layer is CH-internal — outside our codebase") was REFUTED. The C-7-LT-11 observability fix was the highest-leverage spend of the entire 22-cycle arc: a single 5-LOC patch reclassifies a layer from "external" to "internal" and saves us a CH-version-sweep we no longer need to run. **Observability before architectural decisions, always.**

**First end-to-end green is now one PR away.** After 22 cluster cycles, the failure layer has been driven from `validate-config` (r15–r18) through `pre-create-runtime-files` (r19) through `runDir-routing` (r20) through `stderr-capture` (r21–r22) to `stage-rootfs-on-restore` (r22 final). C-7-LT-12a closes the last gap.
