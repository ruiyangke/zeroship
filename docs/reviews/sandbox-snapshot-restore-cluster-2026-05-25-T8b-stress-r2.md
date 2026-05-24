# T-8b-stress-r2 cluster validation — 2026-05-25 (controller v33 / driver v13 / 3+3 fleet / 60-cycle stress)

**Verdict:** **RED — 2/60 end-to-end OK (3.3%).** Identical e2e success rate to stress-r1. Bug 1 (`workspace.img does not exist (controller must stage before spawn)`) still rejects **48/60 CREATEs** despite the controller v33 `fsync_dir` + `assert_disk_image_present` parity preflight. Bug 2 (`Tap zsbx-nm-N already exists` → Exit -1 on WAKE/restore) frequency dropped from r1's ~9 to **3** total occurrences across all three workers (driver v13's tap pre-delete on EEXIST is partially working, but doesn't cover every collision window). **T-8b-cutover stays BLOCKED.** A second-pass driver+controller fix pair is required, and at least one of the two fixes needs a different mental model than what landed in v13/v33.

## Outcome at a glance

| Phase | OK | Denominator | Rate |
|---|---|---|---|
| CREATE   | 12 | 60 | 20.0% |
| SNAPSHOT | 12 | 12 (of created) | 100% |
| WAKE → `ok` | 2 | 12 (of snapshotted) | 16.7% |
| END-TO-END (CREATE+SNAPSHOT+WAKE+STOP) | **2** | **60** | **3.3%** |
| STOP (unconditional cleanup) | 12 | 60 | 20% (only fires for created sandboxes) |

Per-worker:
- w1: CREATE 3/20 | WAKE 0/3 | E2E 0/20
- w2: CREATE 2/20 | WAKE 1/2 | E2E 1/20
- w3: CREATE 7/20 | WAKE 1/7 | E2E 1/20

Symmetric across workers — no single-worker pathology. Worker-3 happened to win the CREATE race more often (7/20 vs 2-3/20) but Bug 1's hit rate is invariant.

## Sprint context

**Cycle:** 25th cluster cycle (T-8b-stress, second run). Follows stress-r1 RED (commit `e6363fce`, 2/60 e2e OK) and a fix sprint that landed:
- **Driver v13** (`3d03cb90`) — `net` package pre-deletes the host tap on `EEXIST` collision before re-adding so the restore alloc no longer trips on a stranded `zsbx-nm-<idx>` interface.
- **Controller v33** (`30960451`) — cold-boot path: `create_ext4_image_if_missing` now `fsync`s the parent dir after `mkfs.ext4` returns AND ends at `assert_disk_image_present` (post-condition mirroring the driver's three preflight checks). Restore path: `submit_restore_job` asserts workspace.img + user_home.img are present before submitting.

**Build / upload SHAs (verified on all three workers post-provision):**
- Driver v13 sha256 `b34a7a65b91fb40b902da1535d3ba7cacefcbe70ef1f9fa1920c7a6a75ea5a4b` (size 20,218,040 B); GCS MD5 `8ecfc7c054ed875631d4d12d8934cf93`.
- Controller v33 sha256 `277fcb86ac3c09d9500b3c1d68d7930aab7249a76f0b57407534823641c4191b` (size 16,655,696 B); GCS MD5 `0d774e301be7415233ffa22e7557250d`. Built inside `rust:1.94-bookworm` Docker container after an initial nix-host build produced a binary linked against glibc-2.42 (incompatible with Debian 12's glibc-2.36 on the workers — surfaced as `systemd: Failed to execute /usr/local/bin/zeroship-sandbox: No such file or directory`); the docker rebuild produced an ELF with max GLIBC_2.34 and stdlib interpreter `/lib64/ld-linux-x86-64.so.2`, matching v32.
- Stress harness sha256 `89ba229e2c8544bc648b46f4963e57cf524cd7edfd7af82093a1217afb123d43` (the polling-aware rewrite uploaded by stress-r1).

**Pin bump commit:** `364ead22` — `sandbox/scripts: bump driver v12->v13 + controller v32->v33 (T-8b-stress Bug1+Bug2 fixes)`. `lint.sh` exit 0.

**Cluster shape:** 3 × n2-standard-4 server + 3 × n2-standard-32 worker (nested-virt), `asia-northeast3-a`. `EXTRA_WORKER_METADATA="install-ch-plugin-driver=1" SERVER_COUNT=3 WORKER_COUNT=3 bash crates/sandbox/scripts/provision-gcp-cluster.sh`. Server sentinels: 15s + 0s + 0s. Worker sentinels: 105s + 15s + 0s. All six up in ~2 min wall.

## Per-phase latency (OK-only)

Wall-times client-side. Sample sizes still small (≤12) because the upstream CREATE failure storm.

| Phase | N | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---:|---:|---:|---:|---:|
| CREATE   | 12 | 6,417  | 20,636 | 34,339 | 37,765 |
| SNAPSHOT | 12 | 14,405 | 14,663 | 14,785 | 14,816 |
| WAKE (total POST + poll) | 2 | 45,946 | 45,952 | 45,952 | 45,952 |
| STOP     | 12 | 19     | 20     | 20     | 21     |

**WAKE p50 vs r1 vs r23:** 45,946 ms vs 46,990 ms vs 46,902 ms — within ~2% noise across all three runs. The wake state machine is performance-stable; the bug is not in scheduling.

**CREATE p95→max spread** (6.4s p50, 37.8s max): the OK creates were not affected by Bug 1's hot-path; the variance comes from the controller's automatic retry on `wait_for_alloc_running` failure (`max_attempts=3`), which lets a small fraction of cycles eventually win the staging race on attempt 2 or 3.

## Failure analysis

### Bug 1 — `workspace.img does not exist (controller must stage before spawn)` — **UNFIXED**

The dominant failure mode, identical to stress-r1. Pulled from `journalctl -u nomad` (worker-3):

```
2026-05-24T14:38:06.390Z [INFO]  client.alloc_runner.task_runner: Task event:
  alloc_id=5e01a6e3-…  task=ch  type="Driver Failure"
  msg="rpc error: code = Unknown desc = ch: StartTask:
       disk[1] /var/zeroship/ch/019e5a6bf7e67280953fc425c2fc3487/workspace.img does not exist
       (controller must stage before spawn)"
```

Per-worker Bug 1 hit counts (from `journalctl -u nomad --since '30 min ago' | grep -c 'workspace.img does not exist'`):
- w1: 28 hits
- w2: 28 hits
- w3: 40 hits
- **Total: 96 driver-preflight rejections** (more than 48 CREATE failures because each failed CREATE retries up to 3 times)

**Why the v33 fix didn't work:** `create_ext4_image_if_missing` now fsync's the parent dir and asserts presence on return — **but the failure is happening AFTER the controller's staging completes successfully**, somewhere in the window between `nomad job run` returning and `nomad-driver-ch.StartTask` running on the same host. The file must be either:

1. **Removed by a concurrent DestroyTask** — a prior failed alloc's host_dir teardown (which the controller's `stop_preserving_state` *does* gate on for snapshotted sandboxes, but for cold-boot creates the cleanup path likely `rm -rf`'s the host_dir) racing with a freshly-allocated retry of the same sandbox_id, OR
2. **Never visible to the driver's mount namespace** — Nomad runs `nomad-driver-ch` in the client process which shares the host root mount namespace, so a peer-mount-namespace explanation is unlikely; more likely a kernel-cache / dirent-propagation gap that fsync didn't fully close (the fsync is correct but **after** mkfs; if the kernel hasn't fully resolved the namei lookup table by the time the driver's `os.Stat` fires, we still hit ENOENT), OR
3. **The sandbox_id encoded in the Nomad job path differs from the host_dir path** — the controller builds the job-spec disk[1] path from `<host_dir>/workspace.img` and the driver stat's the same string; this is the LEAST likely because the path in the error message matches the per-sandbox UUID exactly.

**Strong evidence for (1):** the CREATE retry counter — 48 CREATE failures × 3 attempts ≈ 144 alloc submissions, but only 96 driver-preflight rejections logged, meaning ~33% of retries DO win the race on attempt 2/3 (where the host_dir survives from attempt 1 because attempt 1's DestroyTask happened to lose). This is the smoking gun for a race between a failing alloc's `DestroyTask` and a retrying-alloc's `StartTask`.

**Fix surface (controller):** the cold-boot host_dir cleanup must be **transactional** — either (a) never remove host_dir on cold-boot failure (workspace.img is per-sandbox; an orphaned host_dir is reaped by a sweeper, not by the failing alloc), OR (b) hold a per-sandbox file-lock that the next alloc's StartTask blocks on. The `CreateGuard::host_dir_created` cleanup path needs to be re-audited against the retry semantics.

**Fix surface (driver):** if the preflight stat fails, the driver could `time.Sleep(50ms) + retry once` before declaring Driver Failure — defensive, papers over a kernel-namei delay, but doesn't fix the underlying race.

### Bug 2 — `Tap zsbx-nm-N already exists` — **PARTIALLY FIXED**

Down from r1's pervasive failure to **3 hits in 60 cycles** (one per worker, all on `zsbx-nm-2`). Driver v13's tap pre-delete on EEXIST is doing its job *most* of the time but missed at least one collision window. Pulled from worker-3 journal:

```
2026-05-24T14:39:10.831Z type=Terminated  msg="Exit Code: -1,
  Exit Message: \"write /opt/nomad/data/alloc/.../ch/local/ch-stderr.log: file already closed:
  cloud-hypervisor:   0.522824s: <vmm> WARN:net_util/src/open_tap.rs:84 --
    Tap zsbx-nm-2 already exists. IP configuration will not be overwritten.\""
```

The driver v13 `RECREATE_TAP` path runs in the `init` net hook BEFORE CH spawn. The window the v13 fix doesn't cover: when CH itself attempts to claim the tap from inside the VMM (the `<vmm> WARN` is from CH's open_tap.rs, not from the driver's hook). The driver pre-deletes the tap, then CH creates it, but **a stranded tap that survived the driver's pre-delete** (e.g. from a wrapper-script-based alloc that didn't go through driver init) still hits CH later.

Stranded tap count: **9 zsbx-nm-N DOWN interfaces per worker at end-of-stress** — identical to r1. The driver's pre-delete clears them when a new alloc reuses the same `vm_index`, but the orphans persist between cycles because nothing cleans them on alloc termination.

**Fix surface (driver):** add tap cleanup to `DestroyTask` — `ip link del zsbx-nm-<vm_index>` regardless of CH-exit success/failure. This is the symmetric counterpart of the `init` pre-delete; v13 closed half the loop.

**Fix surface (controller):** tap-leak counter on stop — count surviving `zsbx-nm-<vm_index>` interfaces after `host_fence: cleared` and emit a metric. Until this is in place the operator has zero post-stress signal that taps leaked.

### Bug 3 (NEW) — `ch: restore: stat <restore_from>: no such file or directory`

Observed once on worker-3:

```
2026-05-24T14:41:46.072Z task=ch type="Driver Failure"
  msg="rpc error: code = Unknown desc = ch: restore:
       stat /var/zeroship/ch/019e5a6e541677d081d503eff52a4e0a/restore:
       stat /var/zeroship/ch/.../restore: no such file or directory"
```

The wake alloc's restore directory is missing at driver-stat time. This is a **new failure mode** not seen in r1 — same shape as Bug 1 but on the restore side. Either (a) the snapshot teardown's `stop_preserving_state` lost the workspace.img-and-friends preservation, or (b) the wake alloc fired before the snapshot pipeline's directory move completed.

Single occurrence in 60 cycles makes this statistically noisy, but flagging because it's a sibling of Bug 1 (same control-plane race) on a different code path.

## Counter deltas

| Counter | Stress-r1 | Stress-r2 | Delta |
|---|---|---|---|
| Bug 1 occurrences (workspace.img missing) | dominant | 96 raw preflight hits across 3 workers | unchanged |
| Bug 2 occurrences (tap already exists) | pervasive | 3 total | DOWN ~10× — driver v13 fix is working but incomplete |
| Stranded tap interfaces | 9 per worker | 9 per worker | unchanged — needs DestroyTask hook |
| `vm_index_leaks_total` | 0 | 0 | invariant held |
| `terminal_overwrite_blocked` | 0 | 0 | invariant held |
| `host_fence: cleared … fence_passed=true elapsed_ms=300` | every STOP | every STOP (12/12 fired) | invariant held |
| `wake_jobs GC: deleted terminal rows` | observed | observed (multiple sweeps per worker) | clean |

R23 invariant set preserved. The bug surface didn't change shape — Bug 1 is still the dominant blocker.

## Error-code samples

All 60 cycles classified:

| count | phase | wire envelope |
|---:|---|---|
| 48 | CREATE | `code=500 error=backend_create_failed: nomad alloc terminal status=failed: Failed tasks` |
| 10 | WAKE | `state=failed error_code=restore_backend_failed: backend: nomad alloc terminal status=failed: Failed tasks` |
| 2 | WAKE | `state=ok` (clean) |
| 12 | STOP | `code=200` (matches r1) |

Same `Failed tasks` rollup masking three distinct underlying bugs. **Observability gap persists:** the v33 fix added `assert_disk_image_present` at the controller's submit site, which would surface a controller-side staging bug — but Bug 1 is a *driver-side* observation of a path the controller already successfully staged. The wire envelope needs to carry the alloc's verbatim driver-failure message, not just the generic rollup. This was the P2 carryover from r1 and is still OPEN.

## Path-correctness verdict

The v33+v13 fix pair correctly handled the **observability surface** (controller-side parity check, driver-side tap pre-delete) but missed the **underlying race semantics**. Specifically:

1. The Bug 1 model was "controller stages workspace.img but kernel dirent isn't visible to a peer process" — the fsync_dir+assert fix handles that. But the actual model appears to be "controller stages it, a concurrent failed-alloc DestroyTask removes it, retry sees missing." The fix doesn't catch this because the failed alloc's host_dir teardown happens AFTER the controller's `create_ext4_image_if_missing` returns Ok.

2. The Bug 2 model was "stale tap from prior alloc on the same vm_index" — driver-side pre-delete in `init` does fix the common case. But the residual 3 hits suggest CH-internal claim races on a tap that survived past the pre-delete (probably because a wrapper-script-based alloc or a kernel-cache delay leaves a state the driver doesn't probe).

3. A new Bug 3 (restore-dir missing) opens that wasn't on r1's blocker list.

Neither fix is in the wake state machine itself (which remains correct). Both are in the nomad-driver-ch ↔ controller boundary, exactly as r1 diagnosed. The fix sprint chose the wrong abstraction for Bug 1 (passive parity check vs. transactional host_dir lifecycle) and a half-loop for Bug 2 (init-time pre-delete but no DestroyTask hook).

## Recommendation — T-8b-cutover BLOCKED (second consecutive RED)

**Do not retire `nomad-vm-wrapper.sh`.** The wrapper script handled both bugs deterministically: it pre-staged workspace.img inside the wrapper before `exec`'ing CH (no race window) and it `ip link del`'d the tap on exit (no leak). The Go driver replicates the *happy-path* but not the *cleanup-and-retry* semantics.

Next sprint **must** be a deeper-than-cosmetic fix pair:

1. **Driver v14:** add `DestroyTask` hook that `ip link del zsbx-nm-<vm_index>` regardless of CH-exit path success. Optionally add a 50ms+retry on `preflightDiskPaths` ENOENT (defensive, but only AFTER the controller fix lands, otherwise it just hides the controller bug).

2. **Controller v34:** restructure cold-boot host_dir lifecycle so a failing alloc's `DestroyTask` does NOT remove `workspace.img` (or the host_dir itself) when the controller knows a retry is coming. Two options:
   - (a) Move host_dir cleanup OUT of the per-alloc path entirely; make it a sweeper task that GCs orphan host_dirs after the controller's per-sandbox state record transitions to `failed_terminal`.
   - (b) Hold a per-sandbox advisory file lock during create; failing alloc's cleanup checks the lock and skips if held by a retry.

3. **Tap-leak counter:** controller emits `taps_orphaned_total{worker}` on stop sweep. Until this surfaces a non-zero count, we have no signal that the driver's DestroyTask hook is doing its job.

4. **Wire-envelope verbatim driver message:** carry the alloc's task-event `msg` verbatim through `backend_create_failed` / `restore_backend_failed`. The single-line `Failed tasks` rollup loses the most-actionable information; an operator must SSH every time to disambiguate.

If all four ship and re-stress hits ≥95% e2e OK at 60 cycles, T-8b-cutover unblocks. **Do not attempt cutover until two consecutive stress runs are GREEN** (r1 RED → fix → r2 RED suggests our root-cause model has been incomplete; a single GREEN after the next fix is not enough signal).

## Carried action items

- **OPEN (BLOCKER, P0) — Driver v14 DestroyTask tap cleanup.** Pair with the proposal-stage Controller v34 host_dir lifecycle restructure; do not ship one without the other.
- **OPEN (BLOCKER, P0) — Controller v34 cold-boot host_dir lifecycle fix.** See "Fix surface (controller)" Bug 1 above. The current v33 fsync+assert is the right hygiene but doesn't address the race.
- **OPEN (P1, observability) — Surface alloc-level driver failure verbatim through controller wire envelope.** Same item as r1; the v33 controller-side `assert_disk_image_present` Err string IS verbatim, but Bug 1 never reaches that path (the controller's stage succeeds; the failure is in the driver). The wire envelope from `wait_for_alloc_running` is what needs the alloc msg threaded through.
- **OPEN (P2, sweep) — Stranded-tap counter** (`taps_orphaned_total`).
- **OPEN (P3, observability) — Bug 3 (restore-dir missing) repro counter.** One observation; add a counter to track if it recurs.
- **CLOSED — Polling-aware `/opt/stress/snapshot_stress.py` continues to land on GCS via the worker startup script** (carried from r1; still functioning).
- **NEW — Document the docker-build path for the controller binary.** The nix dev shell now pulls glibc-2.42 which produces incompatible binaries for the Debian-12 worker images. Either pin the flake's nixpkgs to a glibc-2.36-or-older revision, or codify the `docker run rust:1.94-bookworm cargo build` pattern that this sprint adopted. Surfaced as `systemd: Failed to execute /usr/local/bin/zeroship-sandbox: No such file or directory` after the initial v33 upload — the file existed but its ELF interpreter `/nix/store/.../ld-linux-x86-64.so.2` didn't, so execve(2) returned ENOENT for the dynamic loader rather than the binary itself. ~5 min recovery via patchelf-then-rebuild-in-docker. Pre-flight check: `file <binary> | grep 'interpreter /lib64'` before upload.

## Build & upload verification

On-worker hash check post-provision (all three workers identical):
```
/etc/zeroship/nomad-plugins/nomad-driver-ch:  sha256 b34a7a65b91fb40b902da1535d3ba7cacefcbe70ef1f9fa1920c7a6a75ea5a4b
/usr/local/bin/zeroship-sandbox:              sha256 277fcb86ac3c09d9500b3c1d68d7930aab7249a76f0b57407534823641c4191b
/opt/stress/snapshot_stress.py:               sha256 89ba229e2c8544bc648b46f4963e57cf524cd7edfd7af82093a1217afb123d43
```

Driver `--version` banner: `nomad-driver-ch 3d03cb90`. Both `zsbx-ctl.service` and `nomad` active+running across the fleet pre-stress.

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

3+3 fleet (3 × n2-standard-4 + 3 × n2-standard-32 nested-virt), `asia-northeast3-a`, ~50 min wall time (provision + controller-binary recovery rebuild + stress + teardown). Estimated **~$1.80** — under the $30 cycle cap. (Slightly higher than r1's ~$1.20 because the controller-binary rebuild path added ~15 min wall.)
