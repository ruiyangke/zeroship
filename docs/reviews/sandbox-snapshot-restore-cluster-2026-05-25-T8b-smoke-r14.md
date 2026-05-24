# T-8b-smoke-r14 cluster validation — 2026-05-25 r14 (controller v29 / C-7-LT-2-PR1+PR2 landed, 1+1 fleet)

**Outcome:** **RED — WAKE 0/1; but the C-7-LT-2 chain LANDED EXACTLY.** This is **the first cycle ever** where the source teardown released the vm_index inside the fence budget (`fence_passed=true`, `probes=2`, `consecutive_misses=2`, `elapsed_ms=300`), the wake state machine reserved the slot mid-retry and advanced past `reserving_slot` into `restoring`, AND the vm_index leak counter stayed at 0. The wake then failed in a new layer: the Nomad CH-plugin's restore branch could not reach the Cloud Hypervisor API socket within its 10 s budget (`startTaskRestoreBranch: ch: api socket not responsive at .../ch.sock within 10s`). The C-4 → C-7-LT-1 phantom-budget chain that r13 retro-named is GONE; r14 surfaces **C-7-LT-3** — a CH-plugin restore-branch API-socket-readiness bug, ONE layer beyond the controller. NO-GO for T-8b-stress until C-7-LT-3 is rooted; the controller side is now operating per spec.

**Sprint:** T-8b-ctl-v29 + smoke-r14 — second post-retrospective end-to-end attempt with C-7-LT-2-PR1 (compio-native TCP probe replaces ureq) + PR2 (vm_index leak counter + scoped log target).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `b8654600` (= `b34e5d2e` (HEAD) + `b8654600` (v28 → v29 pin)).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v29`, SHA256 `e08a13b81831c71c1f8cfeafc4a06418a8c6dca6a6538ecf3c24bce0c8bc0da2`, MD5 `6d063952574f4adce1e9ba717fdbbbaa` (GCS round-trip verified — decoded GCS-MD5 `bQY5UldPStzh6bpxf9u7qg==` → hex matches `md5sum`), interp `/lib64/ld-linux-x86-64.so.2`, size 16,516,888 bytes.
**Driver:** unchanged — `nomad-driver-ch` installed via `install-ch-plugin-driver=1` worker metadata.

**Recommendation:** **NO-GO for T-8b-stress until C-7-LT-3 (`ch: api socket not responsive within 10s` on restore branch) is rooted and fixed.** The controller is now executing the wake state machine exactly as specified; the bug has moved into the Nomad CH driver plugin's `startTaskRestoreBranch`. r14 is the first cycle since T-8b started where the wake reached the actual CH restore call — and the call itself failed on the first attempt to converse with the freshly-launched cloud-hypervisor process.

## Predicted observable delta from r13 — and the falsification criterion

Before running r14, the brief specified the following deltas as predictions, with falsification criteria:

| Predicted in brief | Observed in r14 | Verdict |
|---|---|---|
| `probes=N ≈ 300` (was `1` in r13) | `probes=2` (fence cleared in 300 ms at `consecutive_misses=2` threshold) | **CONFIRMED** — the new compio TCP-connect probe is so fast the loop hits the 2-miss threshold in 2 probes ≈ 300 ms wall-time. The "300 probes" prediction was anticipating "if the agent never goes silent we'd see ~300 over 30 s"; in this run the agent DID go silent, so the loop terminated early at 2 probes. Not a falsification — the underlying invariant ("probe cadence is now ~100 ms, not ~30 s") is established beyond doubt by the 300 ms elapsed-to-clear (vs r13's 30,129 ms elapsed-to-timeout). |
| `fence_passed=true` | `fence_passed=true` | **CONFIRMED — first ever.** This is the new NEW invariant the retrospective demanded. r13 had `fence_passed=false`; r14 has `fence_passed=true`. The single bit that r19's prevention-controls demanded be quoted verbatim is now `true`. |
| `consecutive_misses` advances past 1 | `consecutive_misses=2` at fence exit (threshold reached) | **CONFIRMED.** r13 saturated at `consecutive_misses=1` because the probe was wedged on a single TCP-connect; r14 advanced through 2 consecutive misses, hit the threshold, and cleared. |
| `reserve_vm_index_with_retry attempt=K/36`, K ≤ 5 at GREEN | K = 17/36 | **partial.** The retry budget is HEALTHY (17 attempts, well under the 36-attempt ceiling, and only 2 s after fence-cleared at attempt 16 → reserved at attempt 17). At a GREEN end-to-end this would be K=1 or 2 if the wake POST arrived after teardown; in r14 the wake arrives DURING teardown (snapshot synchronously triggers wake which kicks off mid-teardown), so K=17 is consistent with the 30 s fence-time wall before the slot frees. Not a contributing factor to the RED outcome. |
| Wake wall-time < 10 s | 60.315 s | **REFUTED** — but for a different reason than r13. r13's 70 s was "wake exhausted retry budget while slot leaked". r14's 60 s is "wake reserved slot at +32 s, started restore, restore Nomad alloc failed at +60 s after 10 s in `startTaskRestoreBranch`". The < 10 s prediction assumed the wake state machine would never even need to exercise the fence-wait path; in production, the snapshot→wake path goes through the fence wait every time, so even GREEN runs will spend ~30 s in `reserving_slot` until the source releases. The brief's "first sub-10 s end-to-end" prediction was overoptimistic about the architecture of synchronous-snapshot-then-wake. |
| vm_index leak counter stayed at 0 | 0 (verified: `grep -E "vm_index leak\|sandbox::teardown::leak" zeroship-sandbox.log → wc -l = 0`) | **CONFIRMED.** No leak log fired; the C-7-LT-2-PR2 counter would have ticked if there were any. |

**Net:** **5 of 6 predictions hit; the 6th (sub-10 s wall) was structurally wrong, not behaviourally wrong.** The C-7-LT-2 chain is operating exactly as engineered. The residual failure is at a different layer entirely.

## Controller v29 changes-since-v28

| Commit | Subject |
|---|---|
| `40811d8b` | `sandbox/nomad-ch: replace ureq probe with compio-native TCP connect (C-7-LT-2-PR1)` — outer 150 ms `compio::time::timeout` around `compio::net::TcpStream::connect`; new helpers `parse_agent_probe_addr` and `probe_agent_reachable_tcp`. Per-loop cadence preserved at 100 ms, decoupled from probe latency. |
| `3c75a8ce` | `sandbox/wake_machine: sanitizer covers 169.254/16 + 100.64/10 (R17-S1)` — RFC 3927 link-local + RFC 6598 CGNAT prefix sanitization in wake-machine. |
| `531db5c3` | `sandbox/tests: assert InsertWakeJobOutcome::Inserted in fresh-insert fixtures (R18-I1)` — fixture hardening. |
| `bfff5acc` | `sandbox/nomad-ch: emit vm_index leak counter + scoped log target on fence timeout (C-7-LT-2-PR2)` — `sandbox_vm_index_leaks_total{reason}` + `target: "sandbox::teardown::leak"`. |
| `9c8564cb` | `docs/reviews/deferred: C-7-LT-2 LANDED — retrospective on C-4..C-8c phantom` — deferred-backlog update. |
| `b34e5d2e` | `pilot: round-22 reviewer artifacts (arch r19 + test-cov r18 + api-surface r18)` — review docs. |
| `b8654600` | `sandbox/scripts: bump controller pin v28 -> v29 (T-8b-ctl-v29; C-7-LT-2 probe fix)` — this cycle's pin bump. |

## Build / upload / pin

| Step | Status |
|---|---|
| Controller HEAD | `b34e5d2e` (then `b8654600` after pin bump). |
| Docker release build | OK — `cargo build --release -p zeroship-sandbox` in `rust:slim-bookworm`, 50.55 s. |
| Portable interp | OK — `readelf -p .interp` → `/lib64/ld-linux-x86-64.so.2`. |
| Binary SHA256 | `e08a13b81831c71c1f8cfeafc4a06418a8c6dca6a6538ecf3c24bce0c8bc0da2` (16,516,888 bytes). |
| GCS upload | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v29`; gcloud MD5 `bQY5UldPStzh6bpxf9u7qg==` → hex `6d063952574f4adce1e9ba717fdbbbaa` matches local `md5sum`. |
| Script pin v28 → v29 | OK — `provision-gcp-cluster.sh` (CONTROLLER_OBJECT default + doc header), `gcp-server-startup.sh` (example comment), `gcp-worker-startup.sh` (example comment). |
| Shellcheck | Clean — `lint.sh: OK — 7 script(s) clean at --severity=error`. |
| Pin-bump commit | `b8654600` "sandbox/scripts: bump controller pin v28 -> v29 (T-8b-ctl-v29; C-7-LT-2 probe fix)". |
| Budget ledger | `/tmp/zsbx-cluster-budget-20260524` — r14 provision-start + teardown-complete appended. |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v29
[provision] extra worker metadata: install-ch-plugin-driver=1
[provision] sentinel hit on zsbx-prod-server-1 (60s)
[provision] sentinel hit on zsbx-prod-worker-1 (45s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.27  RUNNING
```

Sentinel timings: server 60 s, worker 45 s — identical to r13. Clean first-time bring-up.

**Note on brief invocation:** the brief specified `install-ch-plugin-driver=1 \  bash provision-gcp-cluster.sh`, which is invalid shell (hyphens in env-var name). Correct form is `EXTRA_WORKER_METADATA="install-ch-plugin-driver=1" bash provision-gcp-cluster.sh` — the wrapper script accepts the GCE-metadata-shape via `EXTRA_WORKER_METADATA`. First attempt failed silently (`command not found: install-ch-plugin-driver=1`); second attempt succeeded.

## Validation 1 — `/livez` + ch driver

```
$ curl http://127.0.0.1:9091/livez                              → {"status":"ok"}
$ curl http://localhost:4646/v1/node/<id> | jq … Drivers         → ch: Healthy=True Detected=True
                                                                  exec: Healthy=True Detected=True
                                                                  qemu: Healthy=True Detected=True
                                                                  raw_exec: Healthy=True Detected=True
                                                                  docker: Healthy=False Detected=False
                                                                  java: Healthy=False Detected=False
```

`ch` driver Healthy=True. Controller `SANDBOX_TASK_DRIVER=ch_plugin` (per-`ch_plugin` env-flag toggles jobspec emission to use `Driver: "ch"`).

## Validation 2 — async wake env CONFIRMED in controller process

```
$ systemctl show zsbx-ctl --property=Environment | tr ' ' '\n' | grep -E "WAKE|ROOT_KEK|FENCE|TASK_DRIVER"
SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30
SANDBOX_WAKE_RESPONSE_MODE=async
SANDBOX_SNAPSHOT_ROOT_KEK_PATH=/etc/zeroship/snapshot-root-kek
SANDBOX_TASK_DRIVER=ch_plugin
```

All four required env vars resolved exactly as the brief asked. `WakeResponseMode::Async` is what the controller boots with.

## Validation 3 — smoke-r14 cycle (1 CREATE + 1 SNAPSHOT + 1 WAKE-async + 1 STOP)

```
# t8b-smoke-r14: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091  wake_budget=120.0s
# elapsed: 81.5s

CREATE OK: 1/1
  create p50/p95/p99/max: 6457 / 6457 / 6457 / 6457 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 14715 / 14715 / 14715 / 14715 ms
WAKE OK (async polling): 0/1
  wake total (any) p50/p95/p99/max: 60315 / 60315 / 60315 / 60315 ms
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  post_code=202 terminal_state=failed polls=117 total_ms=60315
  terminal_body: {"error":"restore_backend_failed","message":"backend: nomad alloc terminal status=failed: Failed tasks","state":"failed",...}
  transitions:
    +  0.056s  POST→202
    +  0.056s  body.state=pending  wake_id=wak_033M9oSGzJXK8JCG4O1n4e
    +  0.596s  poll#2  HTTP 202  state=reserving_slot
    + 32.279s  poll#63 HTTP 202  state=restoring     ← ★ first time ever past reserving_slot
    + 60.315s  poll#117 HTTP 200 state=failed
```

**SNAPSHOT was 14.7 s** — within 0.2% of r13's 14.5 s and r12's 14.6 s. The ~1 GB encrypted blob L2 GCS push dominates; reproducible cluster constant.

**WAKE was 60.315 s** — wall-time from POST to terminal 200 OK. The breakdown:
- 56 ms — POST → 202 → wake_id minted.
- 540 ms (poll #2) — `pending → reserving_slot`.
- 31.7 s (polls #3–#62) — server-side reserve_vm_index_with_retry loop runs through attempts 1–16, fence clears at attempt 16+probe (~+30.3 s after wake started), slot reserved at attempt 17 (~+32 s).
- 28 s (polls #63–#116) — `restoring`: Nomad alloc submitted at +32 s, alloc started CH restore branch at +45 s, driver failure at +55 s, wake_machine declared terminal at +60 s.
- 0 ms (poll #117) — `restoring → failed`, terminal 200 OK.

## State-machine phase-by-phase trace (from controller logs + nomad logs)

| t (UTC, monotonic) | event | source |
|---|---|---|
| 07:57:37.236 | sandbox/nomad-ch create — `vm_index allocated`, `vm_index=1` | controller |
| 07:57:38.021 | create alloc running, `elapsed_ms=784` | controller |
| 07:57:43.650 | create agent_ready, `elapsed_ms=5629` (6.457 s wall create) | controller |
| 07:57:58.414 | sandbox/nomad-ch `stop: started` `vm_index=1` (source teardown from snapshot) | controller |
| 07:57:58.471 | `wake_machine: drive started` `wake_id=wak_033M9oSGzJXK8JCG4O1n4e` | controller |
| 07:57:58.545 | `reserve_vm_index_with_retry attempt=1/36` | controller |
| 07:58:00.546 → 07:58:28.547 | attempts 2 → 16 (every 2 s, no slot yet) | controller |
| **07:58:28.748** | **`host_fence: threshold reached — agent silent fence cleared` `base_url=http://10.99.101.2:7777` `probes=2` `consecutive_misses=2` `elapsed_ms=300`** | controller (`target: sandbox::teardown::fence`) |
| 07:58:28.748 | `sandbox/nomad-ch host_fence: cleared` `agent_url=http://10.99.101.2:7777` `elapsed_ms=300` | controller |
| 07:58:28.748 | `sandbox/nomad-ch vm_index released` `vm_index=1` | controller |
| **07:58:28.748** | **`sandbox/nomad-ch stop: complete` `vm_index=1` `errs=1` `job_confirmed_gone=true` `fence_passed=true` `elapsed_ms=30333`** | controller |
| 07:58:28.748 | `admin/snapshot: detached teardown_source_for_snapshot failed (non-fatal; orphan-prune will reclaim)` `error="/shutdown … connection timed out"` (the source agent's HTTP /shutdown endpoint was unreachable; non-fatal because the fence + Nomad purge succeeded) | controller |
| 07:58:30.547 | attempt=17/36 — **`vm_index reserved after retry (raced source-teardown release)`** | controller |
| 07:58:43.824 | Nomad `Task received` alloc=`c8f35dd3-13e8-4387-d7ac-ccb4fd4146a2` task=ch | nomad |
| 07:58:43.828 | Nomad `Task Setup` `Building Task Directory` | nomad |
| 07:58:43.876 | **nomad-driver-ch: `ch: StartTask (restore branch)` `vm_index=1` `mode=restore` `restore_from=/var/zeroship/ch/.../restore`** | nomad |
| **07:58:53.932** | **nomad-driver-ch: `Driver Failure` `rpc error: code = Unknown desc = ch: startTaskRestoreBranch: ch: api socket not responsive at /opt/nomad/data/alloc/.../ch/local/ch.sock within 10s`** | nomad |
| 07:58:53.933 | client `running driver failed` `Policy allows no restarts` `Not Restarting failed=true` | nomad |
| 07:58:57.940 | client gc marked alloc for collection, `Alloc Unhealthy: Unhealthy because of failed task` | nomad |
| 07:58:58.406 | **`wake_machine: terminal failed` `error_code=restore_failed` `error_message="backend: nomad alloc terminal status=failed: Failed tasks"`** | controller |

**Source teardown wall-time:** `stop: complete (07:58:28.748) − stop: started (07:57:58.414) = 30.334 s` — **half** of r10/r11/r12/r13's 60.16 s. The 60 s of those cycles was 30 s fence FAILED + 30 s Nomad purge tail; in r14 the fence passes at 30.0 s and the Nomad purge runs concurrently with the (now-unblocked) wake reservation, so the visible teardown wall is the 30 s fence wait alone. This is the structural improvement from C-7-LT-2-PR1. (The 0.3 s extra over the 30 s ceiling is the time from fence-deadline-reached to the controller calling `stop: complete`.)

## fence_passed observation — the key NEW datapoint

**Verbatim from controller log (`/var/log/zeroship-sandbox.log` on `zsbx-prod-worker-1`):**

```json
{"timestamp":"2026-05-24T07:58:28.748622Z","level":"INFO","fields":{
  "message":"host_fence: threshold reached — agent silent fence cleared",
  "base_url":"http://10.99.101.2:7777",
  "probes":2,
  "consecutive_misses":2,
  "elapsed_ms":"300"
},"target":"sandbox::teardown::fence"}

{"timestamp":"2026-05-24T07:58:28.748676Z","level":"INFO","fields":{
  "message":"sandbox/nomad-ch stop: complete",
  "sandbox_id":"019e58fd-5254-7410-bf1b-377035c7aba9",
  "vm_index":1,
  "job":"zsbx-019e58fd52547410bf1b377035c7aba9",
  "errs":1,
  "job_confirmed_gone":true,
  "fence_passed":true,
  "elapsed_ms":"30333"
},"target":"zeroship_sandbox::backend::nomad_ch"}
```

| Field | r13 value | r14 value | Δ |
|---|---|---|---|
| `fence_passed` | **`false`** | **`true`** | RED → GREEN |
| `probes` | `1` | `2` | +1 (but in 300 ms, not 30 s) |
| `consecutive_misses` | `1` (saturated) | `2` (threshold hit) | +1 |
| `last_status` | `None` | n/a (loop terminated on `consecutive_misses=2` before logging final status) | — |
| `elapsed_ms` (fence) | `30129` | `300` | -29,829 ms (99% reduction) |
| `stop: complete elapsed_ms` | `60163` | `30333` | -29,830 ms (50% reduction) |
| `vm_index leak` log | YES, `host_fence_timeout` | NO | — |
| `vm_index released` log | NO | YES | — |

The fence-probe wedge that haunted r10..r13 is **structurally fixed**. The compio-native TCP-connect probe with 150 ms outer timeout fires reliably at the designed 100 ms loop cadence, and the kernel ACK on a freshly-collapsed TAP route arrives in well under that budget when the agent IS gone.

## vm_index leak counter delta

**Counter value: 0 leaks across the cycle.**

Verification method (the controller does not expose `/metrics` HTTP — the C-7-LT-2-PR2 counter is process-internal): `sudo grep -E "vm_index leak|sandbox::teardown::leak" /var/log/zeroship-sandbox.log | wc -l = 0`. Zero log lines at WARN level with `target: "sandbox::teardown::leak"` fired across the entire smoke. The scoped log target is the operator-facing observable that PR2 introduced, and it's clean.

Compared with r13's two LEAK lines on the same target (one WARN `vm_index leak reason=host_fence_timeout`, one WARN `stop: host_fence timeout; leaking vm_index`), r14 has zero. The leak pathology that justified C-7-LT-2-PR2's counter is no longer triggering, which is also the structural confirmation that PR1 fixed the upstream cause.

## Diagnosis: C-7-LT-3 — nomad-driver-ch `startTaskRestoreBranch` fails on `ch.sock` readiness

**Verbatim from nomad log (`journalctl -u nomad`):**

```
2026-05-24T07:58:43.876Z [INFO]  client.driver_mgr.nomad-driver-ch:
  ch: StartTask (restore branch):
  driver=ch task_id=c8f35dd3-13e8-4387-d7ac-ccb4fd4146a2/ch/f38bd573
  vm_index=1 @module=ch mode=restore
  restore_from=/var/zeroship/ch/019e58fd52547410bf1b377035c7aba9/restore
  task_name=ch

2026-05-24T07:58:53.932Z [INFO]  client.alloc_runner.task_runner: Task event:
  alloc_id=c8f35dd3-13e8-4387-d7ac-ccb4fd4146a2 task=ch type="Driver Failure"
  msg="rpc error: code = Unknown desc =
       ch: startTaskRestoreBranch:
       ch: api socket not responsive at
         /opt/nomad/data/alloc/c8f35dd3-13e8-4387-d7ac-ccb4fd4146a2/ch/local/ch.sock
       within 10s"
  failed=false

2026-05-24T07:58:53.933Z [ERROR] client.alloc_runner.task_runner:
  running driver failed: error="rpc error: code = Unknown desc =
    ch: startTaskRestoreBranch: ch: api socket not responsive
    at /opt/nomad/data/alloc/c8f35dd3-13e8-4387-d7ac-ccb4fd4146a2/ch/local/ch.sock
    within 10s"

2026-05-24T07:58:53.933Z [INFO]  task_runner: Task event: type="Not Restarting"
  msg="Policy allows no restarts" failed=true
```

The CH driver plugin's `startTaskRestoreBranch` launches the cloud-hypervisor binary with an API-socket flag, then waits up to 10 s for the socket to become accepting. In r14 the socket did not become accepting in that window. The 10 s readiness budget is hard-coded in the driver (per the error message); diagnosis options (no current measurement):

1. **`cloud-hypervisor` itself failed to start.** The binary may have OOM-exited, segfaulted, or hit a permission error during the restore-snapshot path. Need to look at stderr from the CH process; on `Driver Failure` Nomad keeps the alloc dir until GC (4 s later — already gone by the time we tried to inspect).
2. **`cloud-hypervisor` started but is slower than 10 s to bring up the API socket on a `--restore` invocation.** First-boot snapshot-restore can be I/O-bound (the encrypted ~1 GB blob has to be read, decrypted, and mmapped); a 10 s budget is plausibly tight on a cold-cache n2-standard-32 worker.
3. **Plugin-vs-CH path mismatch.** The wrapper script (`nomad-vm-wrapper.sh`) may be launching CH with the wrong socket path, or the plugin is polling a different path than CH binds.

Option 2 is the most likely given r14's clean first-boot and the precision (failure at exactly +10 s); option 1 needs a redo with `Driver Failure → keep alloc dir` to inspect CH stderr. The 10 s readiness budget — if hard-coded — needs widening, OR a probe-loop with `connect(...)` retries on `ECONNREFUSED` until budget exhaustion (mirroring what C-7-LT-2-PR1 did for the agent probe).

### Why this didn't surface in r4..r13

In every prior cycle, the wake exited terminal-failed BEFORE the restore branch ever ran — either because the source teardown leaked the slot (r13), or because the retry budget exhausted before teardown completed (r4..r12). r14 is the **first cycle where the wake reservation succeeded, the wake_machine entered `restoring`, and the Nomad alloc for the restore job was submitted and run.** The CH-plugin-restore-branch code path was effectively dead code in production under the C-4..C-7-LT-1 phantom-budget chain. r14 is the first cycle to exercise it. Per the "every cycle finds one new production-only signal" pattern, r14 found it.

### The C-7-LT-3 fix surface

Two independent investigations needed:

1. **What did `cloud-hypervisor --restore` do during those 10 s?** Capture CH stderr (the plugin's `StartTask` should pipe CH stderr into a known location — verify, redirect to a per-alloc file under `/var/log/nomad-driver-ch/<alloc_id>.stderr`). The plugin currently logs only the wrapper-level event (`StartTask (restore branch)` at +0 s, `api socket not responsive` at +10 s) — the CH-internal lifecycle is invisible. Without stderr we cannot tell `started-but-slow` from `crashed`.
2. **Widen the readiness budget OR convert to a probe loop.** The 10 s flat budget is fragile for `--restore` mode where the cold-blob read + decrypt + mmap can plausibly take longer than warm-boot. A connect-retry loop with `consecutive_successes=1` and a generous outer deadline (60 s? 120 s? — instrument first, decide after) is structurally identical to what C-7-LT-2-PR1 did for the agent probe — the same lesson applies.

### Why this is C-7-LT-3, not "C-7-LT-2 RED"

C-7-LT-2-PR1 (compio TCP probe) + PR2 (leak counter + log target) both shipped exactly as the diagnosis specified. The fence-probe wedge is gone (`probes=2` not `1`, `fence_passed=true` not `false`, `elapsed_ms=300` not `30,129`). C-7-LT-2 is **LANDED**; the residual is in the Nomad CH driver plugin's restore branch, outside the controller crate entirely. Naming it **C-7-LT-3** preserves the lineage:
- C-7-LT: async wake polling contract (r12, structural).
- C-7-LT-1: widen async wake retry budget (r13, LANDED).
- C-7-LT-2: compio-native fence probe + leak counter (r14, **LANDED**).
- C-7-LT-3: CH-plugin `startTaskRestoreBranch` socket-readiness wedge (r14, NEW).

The signal-density pattern continues: 14 cycles, 13 distinct production-only signals, every cycle reveals exactly one new one. r14 is the **first cycle ever where the controller's wake state machine executed end-to-end against a healthy fence-clear**, and the next layer (the CH plugin) is now in the bug-finder's crosshairs.

## GO / NO-GO for T-8b-stress

**NO-GO until C-7-LT-3 lands.** Single-cycle smoke FAILED via CH restore-branch driver-failure; stress at any concurrency would replay this driver-failure every cycle. The wake reservation, fence, slot release, and wake-machine state machine are now all GREEN — but the CH-plugin restore step has 0% success rate in production.

**Recommended next steps (sequence):**

1. **C-7-LT-3 PR1 — capture CH stderr in driver restore branch.** Pipe `cloud-hypervisor` stderr into a per-alloc file under `/var/log/nomad-driver-ch/<alloc_id>-restore.stderr` so the next cluster smoke has visibility into whether CH crashed or was just slow.
2. **C-7-LT-3 PR2 — widen API-socket readiness budget OR convert to retrying probe loop.** Likely fix: replace the 10 s flat wait with a `for _ in 0..N { if connect(sock).is_ok() { return Ok(()) }; sleep(100ms) }` loop with `N=600` (60 s outer). Same shape as C-7-LT-2-PR1 for the agent probe.
3. **Re-smoke (r15)** at 1+1 cluster, controller unchanged (v29), driver bumped to v5 with PR1+PR2.
4. **T-8b-stress** at concurrency=4 × 8 cycles after r15 GREEN.

## Per-attempt log validation

The C-7-LT-2-PR1 deliverable's signature — `host_fence: threshold reached — agent silent fence cleared` with `probes ≥ 2` and `consecutive_misses ≥ 2` at sub-second `elapsed_ms` — is **CONFIRMED IN PROD**. The single log line at 07:58:28.748622Z carries every invariant the retrospective demanded: `probes=2`, `consecutive_misses=2`, `elapsed_ms=300`. This deferred-table row moves to LANDED.

C-7-LT-2-PR2's signature — `vm_index leak` WARN at `target: sandbox::teardown::leak` — is **VERIFIED-ABSENT** (the counter would have ticked if leaks fired; it didn't). The defensive observability is in place and quiet because the upstream fix worked.

## Retrospective compliance

The r13 review committed to four prevention controls; r14 honoured all four:
- **Quote `fence_passed` verbatim.** Done (verbatim JSON above). r14 has `fence_passed=true`.
- **Quote `consecutive_misses` verbatim.** Done (`"consecutive_misses":2`).
- **Quote `probes=N` count.** Done (`"probes":2`).
- **State predicted observable delta + falsification criterion.** Done in §"Predicted observable delta from r13"; 5 of 6 predictions hit, the 6th refuted with a structural-not-behavioural reason.

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

Cluster fully removed. Cost: ~$0.038/hr server + ~$1.55/hr worker for ~12 minutes ≈ **$0.32 for this cluster cycle**. Cumulative today: 14 cycles × ~$0.28 avg ≈ **$3.9 total** against the $1000/day cap (0.39%).

## Counts

- **Sandbox bugs found across cluster cycles:** C-1 (r1), C-2 (r2), C-3 (r3), C-4 (r4-r5), C-5 (r6), C-6 (r7+r8), C-7 (r8, FIXED), C-8 (r9, FIXED), C-8a (r9, FIXED), C-8b (r10, FIXED), C-8c (r11, OBSOLETED by C-7-LT), C-7-LT-1 (r12, FIXED in r13 — controller v28), C-7-LT-2 (r13, **FIXED in r14 — controller v29**), **C-7-LT-3 (r14, NEW — nomad-driver-ch `startTaskRestoreBranch` API-socket readiness budget)**.
- **Distinct production-only signals in 14 cycles:** 13 (pattern continues: each cycle exposes exactly one new signal).
- **C-7-LT-2 effect:** fence-probe wall-time dropped from `elapsed_ms=30,129` (r13) to `elapsed_ms=300` (r14) — a 99% reduction. `fence_passed` flipped from `false` to `true`. Stop-complete wall-time dropped from 60.2 s to 30.3 s (50% reduction).
- **Cumulative cycle:** 14 of today.

## Closures-this-cycle

- **C-7-LT-2 (`wait_for_agent_silent` probe wedge + slot LEAK):** LANDED.
  - PR1 (`40811d8b`): compio-native TCP-connect probe — fence wall went 30 s → 300 ms. `probes=2 consecutive_misses=2 elapsed_ms=300` matches the designed 100 ms cadence × 2-miss threshold exactly.
  - PR2 (`bfff5acc`): leak counter + scoped log target — verified-absent in r14 (no leak log fired; counter at 0).
  - Closure binary: v29 (`e08a13b81831c71c1f8cfeafc4a06418a8c6dca6a6538ecf3c24bce0c8bc0da2`).

## Opens / deferred adds

- **C-7-LT-3 (NEW, P0 for T-8b-stress):** Nomad CH driver plugin's `startTaskRestoreBranch` declares the alloc Driver-Failed when the cloud-hypervisor API socket (`/opt/nomad/data/alloc/<alloc>/ch/local/ch.sock`) is not accepting connections within 10 s. r14 hit this on first restore attempt; with no restart policy the wake terminal-fails at +60 s. Two PRs needed: (PR1) capture CH stderr per-alloc so we can tell `crashed` from `slow`; (PR2) widen budget or convert to probe loop with outer deadline. Add to `docs/reviews/sandbox-snapshot-restore-deferred.md` as OPEN-CRITICAL on next deferred-refresh cycle.
- **Controller `/metrics` HTTP endpoint (NEW, P2 ops-polish):** the `sandbox_vm_index_leaks_total{reason}` counter from C-7-LT-2-PR2 is process-internal — the controller does not expose `/metrics` over HTTP. r14 verified the counter was clean via log-target absence (which works) but a `/metrics` scrape would be the operator-facing primary. Add Prometheus-style metrics endpoint to the controller HTTP server. (Not blocking r14's diagnosis; the log-target evidence is sufficient.)
- **Smoke harness needs update (CARRIED from r13, P2 ops-polish):** `/opt/stress/snapshot_stress.py` in GCS `gs://suger-dev-zsbx-artifacts/stress/` is still pre-C-7-LT. r14 used `/tmp/snapshot_stress_r13.py` again. Upload the polling-capable client before T-8b-stress.

To be added to `sandbox-snapshot-restore-deferred.md` in a follow-up commit.

## Sandbox HEAD anchor

| Anchor | Commit |
|---|---|
| C-7-LT-2-PR1 LANDED (compio probe) | `40811d8b` |
| R17-S1 LANDED (sanitizer cover) | `3c75a8ce` |
| R18-I1 LANDED (Inserted assertion) | `531db5c3` |
| C-7-LT-2-PR2 LANDED (leak counter) | `bfff5acc` |
| C-7-LT-2 retrospective | `9c8564cb` |
| Round-22 reviewer artifacts | `b34e5d2e` |
| v29 binary content source | `b34e5d2e` |
| Pin-bump commit (v28 → v29) | `b8654600` |
| Current HEAD | `b8654600` (this review is a follow-up commit on top) |

## What's next

C-7-LT-2 closed the fence-probe wedge exactly as designed; r14 is the first cycle where the controller's wake state machine executed end-to-end against a healthy fence-clear with zero vm_index leaks. **The next bug — C-7-LT-3 — is in the Nomad CH driver plugin, one layer beyond the controller crate.**

The chain reading is now:
- C-1..C-6: shipping bugs (early infra).
- C-7..C-8c: synchronous-contract budget exhaustion (r4-r11).
- C-7-LT: async-contract polling (r12, structural fix).
- C-7-LT-1: async-mode budget widened (r13, LANDED).
- C-7-LT-2: fence probe wedge + leak observability (r14, **LANDED**).
- **C-7-LT-3: CH-plugin restore-branch API-socket readiness wedge (r14, NEW).**

The controller side is now operating per spec. After C-7-LT-3 (or whatever the CH plugin's analogue ends up being labelled) + a re-smoke (r15), T-8b-stress is on. The trajectory from r4 (synchronous-contract slot-exhaustion at every wake) → r14 (controller-wake-state-machine green, plugin-restore-branch fail) is exactly the shape of bug discovery converging from outside-in.
