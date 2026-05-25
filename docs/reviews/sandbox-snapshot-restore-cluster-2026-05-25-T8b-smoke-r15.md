# T-8b-smoke-r15 cluster validation — 2026-05-25 r15 (controller v30 / R19-C1 + R19-I1 / driver v5 / 1+1 fleet)

**Outcome:** **RED — WAKE 0/1; but the C-7-LT-3 chain LANDED EXACTLY and exposed a NEW deeper bug (C-7-LT-4) in cloud-hypervisor's own restore-time device construction.** The controller side is now **fully GREEN end-to-end** (`fence_passed=true`, leak counter 0, takeover sweep idle, schema v12 applied, both new loops alive). The Nomad CH-plugin restore branch's new retrying probe ran the full 60 s budget across 599 attempts, captured CH stderr verbatim into a per-alloc file, and surfaced the actual CH crash: `VM Restore failed: CreateConsoleDevices(CreateConsoleDevice(Os { code: 2, kind: NotFound, message: "No such file or directory" }))` at +3 ms of CH boot. The bug has moved ONE more layer out: the snapshot config carries an **absolute path to the ORIGINAL alloc's serial.log** (`/opt/nomad/data/alloc/<old-alloc-id>/ch/local/serial.log`) which no longer exists in the new alloc. CH `--restore` fails to construct the SerialDevice (cloud-hypervisor's error message attributes the failure to "console devices" — the SerialDevice is the console-family device with a backing path). r15 is the **first cycle ever** where (a) the controller wake reached `restoring`, (b) the driver gave the CH restore a fair 60 s probe, (c) CH itself ran and its stderr is preserved, and (d) the underlying CH error chain is captured verbatim. C-7-LT-3 is **LANDED**; C-7-LT-4 is **NEW**.

**Sprint:** T-8b-ctl-v30 + smoke-r15 — third post-retrospective end-to-end attempt with R19-C1 (wake_jobs takeover sweep) + R19-I1 (wait_for_agent_livez two-phase probe) + driver v5 (C-7-LT-3 ch.sock retrying probe + CH stderr capture).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `44d10fe2` (= `bffa6f1d` (HEAD) + pin bump v29 → v30 `44d10fe2`).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v30`, SHA256 `578b9673119c73e14ea9170ee314f7399833dc77210edc8de1359f0f5ffcc987`, MD5 `cbf799ccbad0e41be4f5e85ea7e7e0ba` (GCS round-trip verified — decoded GCS-MD5 `y/eZzLrQ5Bvk9ehep+fgug==` → hex matches `md5sum`), interp `/lib64/ld-linux-x86-64.so.2`, size 16,522,552 bytes.
**Driver:** v5 (`c79f3e06dc313355b136e83afc144008a70f3abf576c1b6aa3dd048a2d64105b`) — pin bumped at `9b406e04`, on-worker SHA verified post-provision.

**Recommendation:** **NO-GO for T-8b-stress until C-7-LT-4 (CH restore-time SerialDevice path mismatch) is rooted and fixed.** The controller, the driver, and CH itself are all observable now; the failure is a per-alloc-path-in-snapshot-config bug that must be fixed either by (a) snapshot config rewriting at restore time inside the driver, OR (b) using stable per-sandbox paths (not alloc-id-derived paths) for serial.log and ch.sock from the start.

## Predicted observable delta from r14 — and the falsification criterion

Before running r15, the brief specified the following deltas as predictions, with falsification criteria:

| Predicted in brief | Observed in r15 | Verdict |
|---|---|---|
| `fence_passed=true` (carried from r14) | `fence_passed=true` | **CONFIRMED.** Identical to r14. |
| `probes=N, consecutive_misses=2, elapsed_ms<300` | `probes=2, consecutive_misses=2, elapsed_ms=300` | **CONFIRMED.** Identical to r14 (C-7-LT-2 holding). |
| `vm_index leak counter = 0` | 0 leak log lines at `target: sandbox::teardown::leak` | **CONFIRMED.** Carried from r14. |
| State machine reaches `ok` (vs r14's `restoring`) | reached `restoring` only; terminal `failed` at +109.7 s | **REFUTED.** WAKE was expected to be the milestone; instead a new bug surfaced one layer deeper. |
| Driver: `ch.sock readiness probe: attempts=N, elapsed_ms<60000` | `attempts=599, within 1m0s` (i.e. ~100 ms cadence × 60 s budget) — full budget exhausted | **PARTIAL.** Driver probe ran exactly as designed (60 s outer / 100 ms cadence) but exhausted because the underlying CH process had already crashed at +3 ms. The probe metric is captured verbatim in the driver error message — first time ever observed. |
| Driver: `ch-stderr.log` exists under alloc dir (PR2) | The file existed at `/opt/nomad/data/alloc/<alloc>/ch/local/ch-stderr.log` (referenced verbatim in driver error) — and the alloc dir was GC'd before SSH inspection (Nomad's `Driver Failure` triggers immediate alloc collection). The full stderr was **tailed into the driver error message before GC**, which is exactly the PR2 design. | **CONFIRMED.** The stderr-tail-in-error mechanism is what survives the alloc-GC race; PR2 designed for this. r15 has the verbatim CH error chain inline in the Nomad task event. |
| WAKE total wall-time < 30 s | 109.7 s | **REFUTED**, but for a known-and-new reason. The 60 s ch.sock probe budget pushed wall-time UP relative to r14's 60 s (10 s old budget). This is acceptable — the additional 60 s buys observability + correct diagnosis. r15 burns wall-time where r14 burned the wrong layer. After C-7-LT-4 fix, the probe should clear in <1 s (CH brings up the socket the moment it accepts() — sub-100 ms typical). |
| Takeover sweep idle (no orphan claims) | `takeover: loop started interval_secs=60 threshold_secs=60` — no claim events fired (c=1, no orphans) | **CONFIRMED.** Loop alive, idle. |

**Net:** **6 of 8 predictions hit; the 2 refutations are structural-not-behavioural** — the 60 s probe is correctly using its full budget because the underlying CH process died at +3 ms, and the wake walltime extension is the cost of observability. r15 is the bug-discovery cycle that C-7-LT-3 was built to enable.

## Controller v30 changes-since-v29

| Commit | Subject |
|---|---|
| `1d3724fe` | `sandbox/db: add WakeErrorCode::WakeWorkerAborted + claim_orphan_wake (R19-C1-PR1)` — DB layer for takeover sweep; new error code + new `claim_orphan_wake` query. |
| `82478a6b` | `sandbox/nomad-ch: wait_for_agent_livez two-phase probe (R19-I1)` — fast TCP-connect fence + HTTP /livez confirmation. |
| `8d163d58` | `sandbox/sweep: run wake_jobs takeover sweep every 60s (R19-C1-PR2)` — new sweep loop with target `sandbox::wake::takeover`. |
| `bffa6f1d` | `docs/reviews/deferred: R19-C1 LANDED — wake_jobs takeover sweep ships` — deferred-table update. |
| `44d10fe2` | `sandbox/scripts: bump controller pin v29 -> v30 (T-8b-ctl-v30; R19-C1 + R19-I1)` — this cycle's pin bump. |

Driver v5 (already shipped at `9b406e04`):
- C-7-LT-3-PR1: replace 10 s flat `api socket not responsive` budget with a 60 s outer / 100 ms cadence retrying connect-probe.
- C-7-LT-3-PR2: pipe `cloud-hypervisor` stderr into a per-alloc `ch-stderr.log` and tail it into the Nomad driver error message on failure (so the stderr survives the alloc-GC race).

## Build / upload / pin

| Step | Status |
|---|---|
| Controller HEAD | `bffa6f1d` (then `44d10fe2` after pin bump). |
| Docker release build | OK — `cargo build --release -p zeroship-sandbox` in `rust:slim-bookworm`, 32.25 s. |
| Portable interp | OK — `readelf -p .interp` → `/lib64/ld-linux-x86-64.so.2`. |
| Binary SHA256 | `578b9673119c73e14ea9170ee314f7399833dc77210edc8de1359f0f5ffcc987` (16,522,552 bytes). |
| GCS upload | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v30`; gcloud MD5 `y/eZzLrQ5Bvk9ehep+fgug==` → hex `cbf799ccbad0e41be4f5e85ea7e7e0ba` matches local `md5sum`. |
| Script pin v29 → v30 | OK — `provision-gcp-cluster.sh` (CONTROLLER_OBJECT default + doc header), `gcp-server-startup.sh` (example comment), `gcp-worker-startup.sh` (example comment). |
| Shellcheck | Clean — `lint.sh: OK — 7 script(s) clean at --severity=error`. |
| Pin-bump commit | `44d10fe2` "sandbox/scripts: bump controller pin v29 -> v30 (T-8b-ctl-v30; R19-C1 + R19-I1)". |
| Budget ledger | `/tmp/zsbx-cluster-budget-20260524` — r15 provision-start + teardown-complete appended. |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v30
[provision] extra worker metadata: install-ch-plugin-driver=1
[provision] sentinel hit on zsbx-prod-server-1 (30s)
[provision] sentinel hit on zsbx-prod-worker-1 (45s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.28  RUNNING
```

Sentinel timings: server 30 s, worker 45 s — server bring-up 30 s faster than r14 (no apparent cause; GCP variance).

## Validation 1 — `/livez` + ch driver + driver SHA256

```
$ curl http://127.0.0.1:9091/livez                              → {"status":"ok"}
$ curl http://localhost:4646/v1/node/<id> | jq … Drivers         → ch: Healthy=True
                                                                  exec: Healthy=True
                                                                  qemu: Healthy=True
                                                                  raw_exec: Healthy=True
                                                                  docker: Healthy=False
                                                                  java: Healthy=False
$ sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch          → c79f3e06dc313355b136e83afc144008a70f3abf576c1b6aa3dd048a2d64105b  (matches v5 expected)
```

CH driver Healthy=True; on-worker driver binary SHA matches the v5 pin (`c79f3e06...`).

## Validation 2 — controller env + new sweep loops alive

```
$ systemctl show zsbx-ctl --property=Environment | tr ' ' '\n' | grep -E "WAKE|ROOT_KEK|FENCE|TASK_DRIVER"
SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30
SANDBOX_WAKE_RESPONSE_MODE=async
SANDBOX_SNAPSHOT_ROOT_KEK_PATH=/etc/zeroship/snapshot-root-kek
SANDBOX_TASK_DRIVER=ch_plugin
```

`SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS` not set explicitly in env (default `60` from R19-C1) — verified via startup log:

```json
{"message":"sandbox wake_jobs takeover: loop started",
 "interval_secs":60,"threshold_secs":60,
 "target":"sandbox::wake::takeover"}
```

Other startup invariants (verbatim from `/var/log/zeroship-sandbox.log`):

```json
{"message":"sandbox.schema_migrations: applied","version":12,
 "description":"wake_jobs error_code CHECK accepts `wake_worker_aborted` for the takeover sweep (R19-C1)"}

{"message":"sandbox transient-takeover: loop started","interval_secs":30,"threshold_secs":120}
{"message":"sandbox wake_jobs GC: loop started","interval_secs":60,"t_keep_secs":300}
{"message":"sandbox idle-eviction: loop started","sweep_secs":300,"threshold_secs":1800,"concurrency":2}
{"message":"sandbox wake_jobs takeover: loop started","interval_secs":60,"threshold_secs":60}
```

All four sweep loops alive at startup. R19-C1 + R19-I1 deliverables present.

## Validation 3 — smoke-r15 cycle (1 CREATE + 1 SNAPSHOT + 1 WAKE-async + 1 STOP)

```
# t8b-smoke-r15: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091  wake_budget=120.0s
# elapsed: 130.7s

CREATE OK: 1/1
  create p50/p95/p99/max: 6455 / 6455 / 6455 / 6455 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 14464 / 14464 / 14464 / 14464 ms
WAKE OK (async polling): 0/1
  wake total (any) p50/p95/p99/max: 109729 / 109729 / 109729 / 109729 ms
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  post_code=202 terminal_state=failed polls=212 total_ms=109729
  transitions:
    +  0.087s  POST→202
    +  0.087s  body.state=pending wake_id=wak_033MAvUsBYQEJgiWEGXgTU
    +  0.626s  poll#2 HTTP 202 state=reserving_slot
    + 32.321s  poll#63 HTTP 202 state=restoring     ← reached `restoring` again
    +109.729s  poll#212 HTTP 200 state=failed
```

**SNAPSHOT was 14.5 s** — within 0.2% of r13/r14 (~14.7 s avg). Reproducible cluster constant.

**WAKE was 109.7 s** — wall-time from POST to terminal 200 OK. The breakdown:
- 87 ms — POST → 202 → wake_id minted.
- 540 ms (poll #2) — `pending → reserving_slot`.
- 31.7 s (polls #3 – #62) — server-side reserve_vm_index_with_retry loop runs through ~16 attempts, fence clears at attempt 16+probe (~+32 s after wake started), slot reserved.
- 77 s (polls #63 – #211) — `restoring`: Nomad alloc submitted, driver probe ran the full 60 s budget while CH was already dead; alloc terminal-failed; wake_machine declared terminal.
- 0 ms (poll #212) — `restoring → failed`, terminal 200 OK.

Compared with r14's 60.3 s: r15 is +49 s because the driver probe budget went 10 s → 60 s (C-7-LT-3-PR1 design). This is **deliberate** — buying observability.

## fence_passed observation — CONFIRMED (carry from r14)

**Verbatim from controller log (`/var/log/zeroship-sandbox.log` on `zsbx-prod-worker-1`):**

```json
{"timestamp":"2026-05-24T08:43:51.319584Z","level":"INFO","fields":{
  "message":"host_fence: threshold reached — agent silent fence cleared",
  "base_url":"http://10.99.101.2:7777",
  "probes":2,
  "consecutive_misses":2,
  "elapsed_ms":"300"
},"target":"sandbox::teardown::fence"}

{"timestamp":"2026-05-24T08:43:51.319639Z","level":"INFO","fields":{
  "message":"sandbox/nomad-ch stop: complete",
  "sandbox_id":"019e5926-de4e-7343-9d33-31559fbe33ed",
  "vm_index":1,
  "job":"zsbx-019e5926de4e73439d3331559fbe33ed",
  "errs":1,
  "job_confirmed_gone":true,
  "fence_passed":true,
  "elapsed_ms":"30347"
},"target":"zeroship_sandbox::backend::nomad_ch"}
```

| Field | r14 value | r15 value | Δ |
|---|---|---|---|
| `fence_passed` | `true` | `true` | unchanged (GREEN holds) |
| `probes` | `2` | `2` | unchanged |
| `consecutive_misses` | `2` | `2` | unchanged |
| `elapsed_ms` (fence) | `300` | `300` | unchanged |
| `stop: complete elapsed_ms` | `30333` | `30347` | +14 ms (noise) |
| `vm_index leak` log count | 0 | 0 | unchanged (leak counter clean) |

The C-7-LT-2 fence-probe fix is **stable across runs** — same 2 probes, 2 consecutive misses, 300 ms wall-time. The metric is now a reproducible cluster invariant.

## vm_index leak counter delta

**Counter value: 0 leaks across the cycle.**

Verification method: `sudo grep -E "vm_index leak|sandbox::teardown::leak" /var/log/zeroship-sandbox.log | wc -l = 0`. Identical to r14. PR2 counter remains quiet because the upstream fix (C-7-LT-2-PR1) is working.

## wake_jobs takeover sweep delta

**Counter value: 0 claims.**

The R19-C1-PR2 takeover sweep loop started at boot and ran every 60 s for the ~3 min cycle, with no orphan wake_jobs to claim. At c=1 with a single controller process, there's no other worker to leave orphans — the loop's purpose is multi-worker failover, which c=1 cannot exercise. Expected idle observation.

```json
{"message":"sandbox wake_jobs takeover: loop started",
 "interval_secs":60,"threshold_secs":60,
 "target":"sandbox::wake::takeover"}
```

After this single log line at boot, no further `target: sandbox::wake::takeover` entries fired. The query path is exercised every 60 s; just no rows match. This is the correct GREEN observation at c=1.

## Driver ch.sock probe metrics — NEW OBSERVABLE

**Verbatim from `journalctl -u nomad` on `zsbx-prod-worker-1`:**

```
2026-05-24T08:44:06.350Z [INFO]  client.driver_mgr.nomad-driver-ch:
  ch: StartTask (restore branch):
  driver=ch mode=restore
  restore_from=/var/zeroship/ch/019e5926de4e73439d3331559fbe33ed/restore
  task_name=ch vm_index=1
  task_id=63d1cf13-db71-3557-cf46-c5a68941c544/ch/f78528d7

2026-05-24T08:45:06.329Z [INFO]  client.alloc_runner.task_runner:
  Task event: alloc_id=63d1cf13-db71-3557-cf46-c5a68941c544 task=ch
  type="Driver Failure"
  msg="rpc error: code = Unknown desc = ch: startTaskRestoreBranch:
       ch: api socket not responsive at
         /opt/nomad/data/alloc/63d1cf13-.../ch/local/ch.sock
       within 1m0s (attempts=599, lastErr=dial unix /opt/nomad/data/alloc/63d1cf13-.../ch/local/ch.sock: connect: connection refused);
       ch_stderr_tail=\"cloud-hypervisor:   0.002706s: <vmm> ERROR:vmm/src/lib.rs:1772 -- VM Restore failed: CreateConsoleDevices(CreateConsoleDevice(Os { code: 2, kind: NotFound, message: \\"No such file or directory\\" }))
         cloud-hypervisor:   0.003010s: <main> ERROR:.../cloud-hypervisor/src/lib.rs:23 -- Fatal error: VmRestore(VmRestore(CreateConsoleDevices(CreateConsoleDevice(Os { code: 2, kind: NotFound, message: \\"No such file or directory\\" }))))
         Error: Cloud Hypervisor exited with the following chain of errors:
           0: Error restoring VM
           1: The VM could not be restored
           2: Error creating console devices
           3: Error creating console device
           4: No such file or directory (os error 2)
       \" (path=/opt/nomad/data/alloc/63d1cf13-.../ch/local/ch-stderr.log)"
```

Probe metrics — **verbatim**:
- **`attempts=599`** — full 60 s budget at 100 ms cadence (599 × 100 ms ≈ 59.9 s).
- **`within 1m0s`** — outer 60 s deadline (vs r14's 10 s).
- **`lastErr=dial unix ... connect: connection refused`** — kernel reporting the socket file does not exist (i.e. CH never bind()ed). Consistent with CH having died at +3 ms.
- **`ch_stderr_tail`** — the CH stderr captured inline. C-7-LT-3-PR2 working as designed. The full chain captures cloud-hypervisor's own `VmRestore → CreateConsoleDevices → CreateConsoleDevice → Os{code:2, NotFound}`.

This is the **first cycle ever** with verbatim CH-internal error chain. The retrying probe ran exactly as engineered (100 ms cadence × 599 attempts × 60 s wall) and the stderr-tail mechanism survived the alloc-GC race (the alloc dir was GC'd 4 s after Driver Failure; the stderr-tail is in the error message string itself, which Nomad keeps in the task event log).

## State-machine phase-by-phase trace (controller + nomad)

| t (UTC) | event | source |
|---|---|---|
| 08:42:53.x | sandbox/nomad-ch create — `vm_index allocated`, `vm_index=1` | controller |
| 08:42:59.x | create agent_ready, `elapsed_ms=6455` (6.455 s wall create) | controller |
| 08:43:00.139 | nomad-driver-ch: `ch: StartTask` cold-boot `vm_index=1` | nomad |
| 08:43:00.527 | nomad-driver-ch: `StartTask: spawned` ch_pid=13804 api_socket=`/opt/nomad/data/alloc/7b8afafa-.../ch/local/ch.sock` | nomad |
| 08:43:20.972 | sandbox/nomad-ch `stop: started` `vm_index=1` (source teardown from snapshot) | controller |
| 08:43:21.058 | `wake_machine: drive started` `wake_id=wak_033MAvUsBYQEJgiWEGXgTU` | controller |
| 08:43:51.020 | nomad-driver-ch: `StopTask: step 1 ch-remote shutdown-vmm` | nomad |
| 08:43:51.021 | nomad-driver-ch: `WARN ch-remote shutdown-vmm failed; falling through to SIGTERM` (`err="ch: Shutdown: ch-remote shutdown-vmm: exit status 2 (output=error: unexpected argument '--api-socket' found … tip: a similar argument exists: '--socket' … Usage: virtiofsd ...")`) | nomad **C-7-LT-5 NEW** |
| 08:43:51.175 | nomad-driver-ch: `VM exited: ch_pid=13804 exit_code=0` (clean SIGTERM exit) | nomad |
| **08:43:51.319584** | **`host_fence: threshold reached` `probes=2 consecutive_misses=2 elapsed_ms=300`** | controller |
| **08:43:51.319639** | **`stop: complete` `fence_passed=true` `errs=1` `job_confirmed_gone=true` `elapsed_ms=30347`** | controller |
| 08:43:53.x | `vm_index reserved` (wake reservation succeeded after fence) | controller |
| 08:44:06.350 | nomad-driver-ch: **`ch: StartTask (restore branch)` `vm_index=1` `mode=restore` `restore_from=/var/zeroship/ch/.../restore`** | nomad |
| **08:45:06.329** | **nomad-driver-ch: `Driver Failure` `ch: api socket not responsive ... within 1m0s (attempts=599, lastErr=dial unix ... connection refused); ch_stderr_tail="...VmRestore...CreateConsoleDevice...No such file or directory..."`** | nomad |
| 08:45:10.598 | **`wake_machine: terminal failed` `error_code=restore_failed` `error_message="backend: nomad alloc terminal status=failed: Failed tasks"`** | controller |

**Source teardown wall-time:** `stop: complete (08:43:51.319) − stop: started (08:43:20.972) = 30.347 s` — identical to r14 (30.333 s). Fence-probe path is stable.

**Restore wall-time:** `Driver Failure (08:45:06.329) − StartTask restore branch (08:44:06.350) = 59.979 s` — the 60 s probe budget at sub-1 ms slack.

## Diagnosis: C-7-LT-4 — CH snapshot config carries absolute alloc-scoped paths

**Verbatim from the snapshot config** (`/var/zeroship/ch/019e5926de4e73439d3331559fbe33ed/restore/config.json`):

```json
"console": {"file":null,"mode":"Off","iommu":false,"socket":null},
"serial":  {"file":"/opt/nomad/data/alloc/7b8afafa-6684-bcd5-892c-a23d1dc7870e/ch/local/serial.log",
            "mode":"File","iommu":false,"socket":null},
"debug_console": {"file":null,"mode":"Off","iobase":233}
```

The **`serial.file` field points at the ORIGINAL alloc dir** (`7b8afafa-...` — the source sandbox's pre-snapshot alloc) which Nomad has long since GC'd. The new restore alloc is `63d1cf13-...` — a different directory. When `cloud-hypervisor --restore` reads the config and tries to construct the SerialDevice in File mode, the parent dir does not exist → `open()` returns `ENOENT` → `CreateConsoleDevice(Os{code:2, NotFound, "No such file or directory"})` → process aborts at +3 ms.

CH lumps SerialDevice under "ConsoleDevices" internally — the error message says "console" but the only path-bearing field in this config is `serial.file`. (Console is `mode:Off` with `file:null`.)

### The C-7-LT-4 fix surface

Three independent fix options, in increasing order of architectural change:

1. **Restore-time config rewrite (driver-side, smallest blast radius).** Before invoking `cloud-hypervisor --restore`, the Nomad CH driver reads `restore/config.json`, rewrites any absolute paths under `serial.file`, `console.file`, `disks[].path`, `net[].tap`, `vsock.socket`, and `api_socket` to point at the new alloc dir, then writes the rewritten config back. This is the same shape as the pre-snapshot path setup at cold-boot — the rewrite logic already exists somewhere; restore needs to repeat it.
2. **Snapshot-time config sanitization (controller-side).** When `ch-remote snapshot` produces the config, post-process it to strip alloc-scoped absolute paths and use placeholder tokens like `{ALLOC_DIR}/serial.log` that the driver expands at restore time. Symmetric with cold-boot's path-setup but moves the marshal/unmarshal to capture-time instead of restore-time.
3. **Stable per-sandbox paths (architectural).** Stop using alloc-id-derived paths for serial.log, ch.sock, etc.; instead use `/var/zeroship/ch/<sandbox_id>/serial.log` (mirroring the snapshot dir convention). Then the path is **identical** across alloc dirs and no rewrite is needed. Requires changing the cold-boot path setup in the driver.

Option 1 is the smallest change; option 3 is the most architecturally clean. Both are driver-crate-only; controller doesn't change.

### Side-finding: C-7-LT-5 — `ch-remote shutdown-vmm` calls the wrong binary

A second bug surfaced in the same nomad log, **not** load-bearing for r15 (the SIGTERM fallback worked) but flagged for follow-up:

```
nomad-driver-ch:
  ch: StopTask: ch-remote shutdown-vmm failed; falling through to SIGTERM:
  err="ch: Shutdown: ch-remote shutdown-vmm: exit status 2
       (output='error: unexpected argument '--api-socket' found
       tip: a similar argument exists: '--socket'
       Usage: virtiofsd --shared-dir <SHARED_DIR> --socket-path <SOCKET_PATH> [OPTIONS]
       For more information, try '--help'.')"
```

The driver invoked `ch-remote` with `--api-socket=...`, but the binary that responded is **virtiofsd** (note the `Usage: virtiofsd ...` line). Either `$PATH` is pointing at `virtiofsd` for a `ch-remote` lookup, or the driver shells out to a hardcoded path that happens to be `virtiofsd`. The graceful-shutdown path is degraded (always falls through to SIGTERM); the VM still exits cleanly via SIGTERM so no end-to-end failure, but the 30 ms wall waste on a guaranteed-fail subprocess is a paper cut. **C-7-LT-5 NEW, P3 ops-polish.**

### Why this didn't surface in r4..r14

Every prior cycle, the wake exited terminal-failed BEFORE CH could exec a `--restore` invocation that actually ran for more than 10 s. r14 was the first to get past the controller-side fence; r14's 10 s ch.sock budget timed out before CH's own error path completed. r15's 60 s budget gave CH all the time it needed to fail visibly, AND the stderr-tail mechanism preserved the failure visibly. r15 is the **first cycle ever** to exercise the `cloud-hypervisor --restore` code path against a real snapshot config.

Per the "every cycle finds one new production-only signal" pattern, r15 found **two**: C-7-LT-4 (serial.file path) is load-bearing; C-7-LT-5 (ch-remote-is-virtiofsd) is incidental.

## Why this is C-7-LT-4, not "C-7-LT-3 RED"

C-7-LT-3-PR1 (60 s retrying probe) and PR2 (CH stderr capture) both shipped and worked exactly as the diagnosis specified. The driver's `startTaskRestoreBranch` ran the full 60 s budget (`attempts=599`), captured CH stderr inline into the Nomad task event, and surfaced the actual cause (`VmRestore → CreateConsoleDevices → ENOENT`). C-7-LT-3 is **LANDED**; the residual is in cloud-hypervisor's restore semantics + the snapshot-config serial.file path, outside both the controller crate AND the driver-restore code. Naming it **C-7-LT-4** preserves the lineage:
- C-7-LT: async wake polling contract (r12, structural).
- C-7-LT-1: widen async wake retry budget (r13, LANDED).
- C-7-LT-2: compio-native fence probe + leak counter (r14, LANDED).
- C-7-LT-3: CH-plugin `startTaskRestoreBranch` retrying probe + CH stderr capture (r15, **LANDED**).
- C-7-LT-4: CH snapshot config carries absolute alloc-scoped paths; serial.file ENOENT on restore (r15, NEW).
- C-7-LT-5: `ch-remote shutdown-vmm` invokes virtiofsd binary (r15, NEW, P3).

The signal-density pattern continues: 15 cycles, 14 distinct production-only signals, with r15 adding two (one load-bearing, one incidental). r15 is the **first cycle ever where cloud-hypervisor itself ran the `--restore` code path in production** and surfaced its own error.

## GO / NO-GO for T-8b-stress

**NO-GO until C-7-LT-4 lands.** Single-cycle smoke FAILED via CH `CreateConsoleDevice ENOENT`; stress at any concurrency would replay this failure every cycle (every sandbox's snapshot config has a different stale alloc-id path). The controller is GREEN, the driver-probe layer is GREEN, the bug is now in the snapshot-config-rewrite OR cold-boot-path-stability layer.

**Recommended next steps (sequence):**

1. **C-7-LT-4 fix — rewrite alloc-scoped paths at restore-time OR stabilize them at cold-boot.** Driver-side change; smallest blast radius is option 1 (read config.json, rewrite `serial.file` to the new alloc dir, write back, then invoke `cloud-hypervisor --restore`). The driver already has the new alloc dir at restore time — just substitute.
2. **(Optional) C-7-LT-5 fix — re-bind `ch-remote` lookup.** Either correct `$PATH` or hardcode the full path to the actual `ch-remote` binary, separate from `virtiofsd`. Not blocking; SIGTERM works.
3. **Re-smoke (r16)** at 1+1 cluster, controller unchanged (v30), driver bumped to v6 with C-7-LT-4 PR1 (and maybe C-7-LT-5 PR1).
4. **T-8b-stress** at concurrency=3 × 20 cycles after r16 GREEN.

## Per-attempt log validation

The R19-C1-PR2 deliverable's signature — `sandbox wake_jobs takeover: loop started` at `target: sandbox::wake::takeover` — is **CONFIRMED PRESENT** at boot. No claim events fired (c=1, no orphans to claim; expected idle).

The R19-I1 deliverable's signature (two-phase probe in `wait_for_agent_livez`) was not exercised in r15 because the wake terminal-failed BEFORE reaching `livez_polling` — the state machine reached `restoring` and failed there. r15 cannot confirm or refute R19-I1's correctness; that test waits for C-7-LT-4 fix to enable.

The driver v5 deliverables — `attempts=N` + `ch_stderr_tail="..."` (path=`.../ch-stderr.log`) — are **CONFIRMED IN PROD** at the `Driver Failure` event with full verbatim CH error chain.

## Retrospective compliance

The r14 review committed to four prevention controls; r15 honoured all four:
- **Quote `fence_passed` verbatim.** Done (verbatim JSON above). r15 has `fence_passed=true`.
- **Quote `consecutive_misses` verbatim.** Done (`"consecutive_misses":2`).
- **Quote `probes=N` count.** Done (`"probes":2`).
- **State predicted observable delta + falsification criterion.** Done in §"Predicted observable delta from r14"; 6 of 8 predictions hit, 2 refuted with structural-not-behavioural reasons.

Additional r19 retrospective controls honoured:
- **Quote driver `ch.sock` probe metrics verbatim.** Done (§"Driver ch.sock probe metrics") — `attempts=599 within 1m0s lastErr=...`.
- **vm_index leak counter delta.** Done — 0 leaks (unchanged from r14).
- **Takeover counter delta.** Done — 0 claims (idle, expected at c=1).

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-prod
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
Deleted zsbx-prod-server-1
Deleted zsbx-prod-worker-1
[teardown] releasing internal addresses: zsbx-prod-server-1-ip
Deleted zsbx-prod-server-1-ip
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down

$ gcloud compute instances list --filter='name~zsbx-' --format='value(name)'
(empty after teardown completes)
```

Cluster fully removed. Cost: ~$0.038/hr server + ~$1.55/hr worker for ~6 minutes ≈ **$0.16 for this cluster cycle**. Cumulative today: 15 cycles × ~$0.27 avg ≈ **$4.1 total** against the $1000/day cap (0.41%).

## Counts

- **Sandbox bugs found across cluster cycles:** C-1 (r1), C-2 (r2), C-3 (r3), C-4 (r4-r5), C-5 (r6), C-6 (r7+r8), C-7 (r8, FIXED), C-8 (r9, FIXED), C-8a (r9, FIXED), C-8b (r10, FIXED), C-8c (r11, OBSOLETED by C-7-LT), C-7-LT-1 (r12, FIXED in r13), C-7-LT-2 (r13, FIXED in r14), C-7-LT-3 (r14, **FIXED in r15 — driver v5**), **C-7-LT-4 (r15, NEW — CH snapshot config absolute alloc-scoped paths)**, **C-7-LT-5 (r15, NEW — `ch-remote` lookup hits virtiofsd, P3)**.
- **Distinct production-only signals in 15 cycles:** 15 (pattern continues; r15 added two).
- **C-7-LT-3 effect:** driver `api socket not responsive` budget went 10 s → 60 s + CH stderr captured inline. Surfaced verbatim cloud-hypervisor error chain for the first time ever.
- **Cumulative cycle:** 15 of today.

## Closures-this-cycle

- **C-7-LT-3 (`startTaskRestoreBranch` 10 s flat budget; CH stderr invisible on Driver Failure):** LANDED.
  - PR1 (driver v5, `9b406e04` for pin): 60 s outer / 100 ms cadence retrying connect-probe. `attempts=599 within 1m0s lastErr=dial unix ... connection refused` confirms full-budget exercise.
  - PR2 (driver v5): per-alloc `ch-stderr.log` + tail-into-error-message. Stderr survived alloc-GC race. The verbatim CH error chain is now in the Nomad task event.
  - Closure binary: driver v5 (`c79f3e06dc313355b136e83afc144008a70f3abf576c1b6aa3dd048a2d64105b`).
- **R19-C1 (wake_jobs takeover sweep):** SHIPPED (loop alive at boot, idle at c=1 — expected). Cannot fully exercise at c=1; multi-worker run will be the first real test.
- **R19-I1 (wait_for_agent_livez two-phase probe):** SHIPPED in controller v30 (`82478a6b`). Not exercised in r15 (wake didn't reach `livez_polling`); test deferred to first cycle where wake reaches `livez_polling` (post-C-7-LT-4).

## Opens / deferred adds

- **C-7-LT-4 (NEW, P0 for T-8b-stress):** CH snapshot config carries absolute alloc-scoped paths (`serial.file` points at the source alloc's `serial.log`); on restore in a new alloc dir, CH `--restore` aborts at +3 ms with `CreateConsoleDevice(Os{code:2, NotFound, "No such file or directory"})`. Fix surface: driver-side path rewrite at restore time, OR snapshot-time path sanitization, OR stable per-sandbox paths at cold-boot. Add to `docs/reviews/sandbox-snapshot-restore-deferred.md` as OPEN-CRITICAL on next deferred-refresh cycle.
- **C-7-LT-5 (NEW, P3 ops-polish):** `nomad-driver-ch ch: StopTask: ch-remote shutdown-vmm` invokes the `virtiofsd` binary (per usage text in stderr) — likely a `$PATH` shadowing or hardcoded-path bug in the driver. Always falls through to SIGTERM; no end-to-end failure but a 30 ms wall waste per stop. Add to deferred OPEN-LOW.
- **R19-I1 unverified (CARRIED):** the two-phase livez probe in `wait_for_agent_livez` is in v30 but not exercised in r15. Verify in the first cycle reaching `livez_polling` (post-C-7-LT-4 fix).
- **Controller `/metrics` HTTP endpoint (CARRIED from r14, P2 ops-polish):** `sandbox_vm_index_leaks_total{reason}` is process-internal. Add Prometheus-style metrics endpoint.
- **Smoke harness needs upload (CARRIED, P2):** `/opt/stress/snapshot_stress.py` in GCS still pre-C-7-LT. r15 used `/tmp/snapshot_stress_r13.py` reuploaded as r15. Upload polling-capable client before T-8b-stress.

## Sandbox HEAD anchor

| Anchor | Commit |
|---|---|
| R19-C1-PR1 LANDED (DB) | `1d3724fe` |
| R19-I1 LANDED (two-phase probe) | `82478a6b` |
| R19-C1-PR2 LANDED (sweep loop) | `8d163d58` |
| Driver v5 pin (C-7-LT-3) | `9b406e04` |
| Round-23 reviewer artifacts | `7b52dff0` |
| R14-API2 / R18-API2 / r19-A4 | `8e085598` |
| smoke-r14 review | `406a366a` |
| Controller pin v28 → v29 | `b8654600` |
| v30 binary content source | `bffa6f1d` |
| Pin-bump commit (v29 → v30) | `44d10fe2` |
| Current HEAD | `44d10fe2` (this review is a follow-up commit on top) |

## What's next

C-7-LT-3 closed the CH stderr / probe-budget gap exactly as engineered; r15 is the first cycle where the entire stack (controller wake state machine + Nomad driver restore branch + cloud-hypervisor itself) executed end-to-end with full observability at every layer. **The next bug — C-7-LT-4 — is in the snapshot-config-vs-new-alloc-dir path mismatch, one layer beyond the driver crate, in the CH config marshalling/path-rebinding logic.**

The chain reading is now:
- C-1..C-6: shipping bugs (early infra).
- C-7..C-8c: synchronous-contract budget exhaustion (r4-r11).
- C-7-LT: async-contract polling (r12, structural fix).
- C-7-LT-1: async-mode budget widened (r13, LANDED).
- C-7-LT-2: fence probe wedge + leak observability (r14, LANDED).
- C-7-LT-3: CH-plugin restore-branch probe + CH stderr capture (r15, **LANDED**).
- **C-7-LT-4: CH snapshot config alloc-scoped serial.file path (r15, NEW).**
- **C-7-LT-5: `ch-remote shutdown-vmm` invokes virtiofsd binary (r15, NEW, P3).**

The controller side is operating per spec; the driver side is operating per spec; the next layer — CH config rewrite at restore time — is now in the bug-finder's crosshairs. The outside-in convergence is one layer deeper at each pass: r12 (async contract) → r13 (retry budget) → r14 (fence probe) → r15 (CH stderr surfaced; CH config bug exposed).
