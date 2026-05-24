# T-8b-smoke cluster validation — 2026-05-25 r1 (Go ch_plugin driver, 1+1 fleet)

**Sprint:** T-8b-smoke — first cluster cycle for the Go-based `nomad-driver-ch` plugin.
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.
**Worktree (driver):** `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch`.
**Verdict:** **FAIL — smoke failure on CREATE, before any T-8 logic ever ran. Root cause is environmental, not in the Go plugin.**
**Recommendation:** **NO-GO for T-8b-stress.** Pre-requisites must land first (see "Required before next cluster cycle").

## TL;DR

The 1-server + 1-worker GCP cluster came up cleanly in **105s** (server sentinel @60s, worker sentinel @45s) and the new plugin install path executed exactly as designed:

- Plugin binary `nomad-driver-ch v=ce118450` deployed to `/etc/zeroship/nomad-plugins/nomad-driver-ch` (0755).
- `/etc/nomad.d/plugin-dir.hcl` written with `plugin_dir = "/etc/zeroship/nomad-plugins"`.
- `zsbx-ctl` systemd unit shows `Environment=SANDBOX_TASK_DRIVER=ch_plugin`.
- Controller `/livez` returned `{"status":"ok"}`.

But the single create-then-snap-then-wake cycle failed at step 1 (CREATE) with HTTP 500 + body `backend.create: nomad alloc terminal status=failed: Failed tasks`. Two **distinct, independent** issues were uncovered:

1. **(Latent driver-install bug)** Nomad refused to load the plugin: `[WARN] agent.plugin_loader: plugin not referenced in the agent configuration file, loading skipped: plugin_dir=/etc/zeroship/nomad-plugins plugin=nomad-driver-ch`. The worker startup writes `plugin_dir`, but Nomad 2.0.2 additionally requires a `plugin "<name>" { ... }` config stanza or the binary is *detected and skipped*. The `ch` driver therefore never appeared in the node's driver list (only `docker / exec / java / qemu / raw_exec` showed up under `nomad node status -verbose`).
2. **(Stale controller pin)** The provision script defaults `CONTROLLER_OBJECT=zeroship-sandbox.snapshot-v6`, which predates both `a4c481e1` (B24 `ZSBX_SANDBOX_ID` env injection fix, **already documented as a HARD-FAIL blocker in `…cluster-2026-05-25-r1.md`**) and `5fe36805` (T-7, the `SANDBOX_TASK_DRIVER=ch_plugin` flag). Result: the controller (a) silently ignores `SANDBOX_TASK_DRIVER` and still emits `raw_exec` jobs, and (b) does not inject `ZSBX_SANDBOX_ID`, so the wrapper's R8-DEPLOY1 guard kills the alloc in ~30ms.

**Issue #2 is the immediate blocker** — even if issue #1 had been fixed, the controller would not have routed the job to the `ch` driver. **Issue #1 is the latent driver-install bug** that will surface as the next failure after issue #2 is fixed.

Because the create never landed in the `ch_plugin` path, **the T-8b-smoke validation goal (parity of the Go plugin under a real workload) was not exercised at all**. This is a NO-GO for T-8b-stress.

## Pre-flight checks (all PASS)

| Check | Result |
|---|---|
| `/tmp/zsbx-cluster-budget-20260524` line count | 0 → 1 (this run; ≪10/24h cap) |
| `dist/nomad-driver-ch` artifact present | OK (20,181,176 bytes) |
| `gcloud auth list` | OK (active: `ruiyang@suger.io`) |
| `gcloud config get-value project` | `suger-dev` |

## Upload (Step 1)

| Field | Value |
|---|---|
| Local path | `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch/dist/nomad-driver-ch` |
| sha256 | `df2b117511e9f218ca20ccd3ce74e1c2d37c65cfb8deeeca4b9b70d9621a25f8` |
| Size | 20,181,176 bytes |
| Embedded gitSHA | `ce118450` (from `--version`) |
| Target | `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v1` |
| Upload throughput | 69.0 MiB/s |
| Post-upload `objects describe md5Hash,size` | `HeQ47ZNI0N9hskRydBOkOw==, 20181176` (matches) |

## Provision (Step 2)

Provision script change (only addition):
- `crates/sandbox/scripts/provision-gcp-cluster.sh`: added `EXTRA_WORKER_METADATA` env-var pass-through (appended to the worker `--metadata` list when set). Default empty preserves existing behaviour.

Invocation:

```
SERVER_COUNT=1 WORKER_COUNT=1 \
EXTRA_WORKER_METADATA="install-ch-plugin-driver=1" \
  bash crates/sandbox/scripts/provision-gcp-cluster.sh
```

| Phase | Time |
|---|---|
| Server `zsbx-prod-server-1` create + sentinel `zsbx-server-ready` | 60s |
| Worker `zsbx-prod-worker-1` create + sentinel `zsbx-worker-ready` | 45s |
| Provision wall (sequential, includes network creation) | ~105s |

Final fleet:

```
NAME                ZONE               MACHINE_TYPE    PRIVATE_IP   STATUS
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.14  RUNNING
```

Provision log: `/tmp/t8b-smoke-provision.log`.

## Plugin-install validation (post-provision SSH probe)

```
---nomad-plugin-dir---
plugin_dir = "/etc/zeroship/nomad-plugins"
---plugin-binary---
-rwxr-xr-x 1 root root 20181176 May 24 00:07 /etc/zeroship/nomad-plugins/nomad-driver-ch
---plugin-version---
nomad-driver-ch ce118450
---nomad-plugin-status---
Container Storage Interface
No CSI plugins                       ← `nomad plugin status` only lists CSI; not a defect
---zsbx-ctl-env---
…SANDBOX_BACKEND=nomad-ch …SANDBOX_TASK_DRIVER=ch_plugin …
---livez---
{"status":"ok"}
```

`nomad node status -verbose 631d833b…` Drivers block:

```
Driver    Detected  Healthy  Message                             Time
docker    false     false    Failed to connect to docker daemon  2026-05-24T00:07:12Z
exec      true      true     Healthy                             2026-05-24T00:07:12Z
java      false     false    <none>                              2026-05-24T00:07:12Z
qemu      true      true     Healthy                             2026-05-24T00:07:12Z
raw_exec  true      true     Healthy                             2026-05-24T00:07:12Z
```

**`ch` is missing from the node driver list.** The plugin loader explicitly skipped it (see Finding A below).

## Smoke result (Step 3) — FAIL

`/opt/stress/snapshot_stress.py --concurrency 1 --cycles 1 --base-url http://127.0.0.1:9091 --token-file /etc/zeroship/sandbox-token --admin-token-file /etc/zeroship/sandbox-admin-token`

Verbatim output:

```
# snapshot-stress: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091
# elapsed: 0.5s

=== snapshot-stress (N=1) ===
CREATE OK: 0/1
SNAPSHOT OK: 0/0
WAKE OK: 0/0
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED CREATES: 1
  [1x] code=500: {"error":"backend.create: nomad alloc terminal status=failed: Failed tasks"}

=== RAW_JSON_BEGIN ===
[{"idx": 0, "user_id": "usr_033LyJmoDbzPQk1BFUAQBG", "project_id": "prj_033LyJmoDkwAyvkatcUd3I",
  "create_code": 500, "create_ms": 512.5274940000395,
  "create_body": "{\"error\":\"backend.create: nomad alloc terminal status=failed: Failed tasks\"}"}]
=== RAW_JSON_END ===
```

p50 timings: not measurable (CREATE failed); the create call returned in **~512ms**, of which ~500ms is wrapper-spawn + alloc-poll round-trip (the wrapper itself exits in ~30ms).

## Nomad / controller failure trace (verbatim)

Nomad plugin loader at startup:

```
2026-05-24T00:07:12.702Z [WARN]  agent.plugin_loader: plugin not referenced in the
  agent configuration file, loading skipped:
  plugin_dir=/etc/zeroship/nomad-plugins plugin=nomad-driver-ch
```

Nomad task lifecycle for the failed alloc:

```
00:09:35.312Z [INFO] task_runner: Task event: alloc_id=1fc1102e-2491-9657-c666-487a3345541f
                      task=ch type=Received    msg="Task received by client"   failed=false
00:09:35.316Z        … task=ch type="Task Setup" msg="Building Task Directory" failed=false
00:09:35.365Z [INFO] client.driver_mgr.raw_exec: starting task: driver=raw_exec
                      driver_cfg="{Command:/etc/zeroship/nomad-vm-wrapper.sh Args:[] …}"
00:09:35.394Z        … task=ch type=Started     msg="Task started by client"   failed=false
00:09:35.397Z        … task=ch type=Terminated  msg="Exit Code: 1"             failed=false
00:09:35.400Z [INFO] client.driver_mgr.raw_exec.executor: plugin process exited:
                      alloc_id=1fc1102e-… task_name=ch plugin=/usr/bin/nomad id=14206
00:09:35.400Z        … task=ch type="Not Restarting" msg="Policy allows no restarts" failed=true
00:09:35.407Z        … task=ch type="Alloc Unhealthy" msg="Unhealthy because of failed task"
00:09:35.809Z [INFO] client.gc: garbage collecting allocation: alloc_id=1fc1102e-… reason="forced collection"
```

Note `driver=raw_exec` (not `ch`). The controller never routed the job to the Go plugin.

zsbx-ctl backend log:

```json
{"ts":"2026-05-24T00:09:35.296844Z","level":"INFO","fields":{
  "message":"sandbox/nomad-ch create",
  "sandbox_id":"019e5750-d340-7cd2-ae98-76e62188e7d6",
  "user_id":"usr_033LyJmoDbzPQk1BFUAQBG","project_id":"prj_033LyJmoDkwAyvkatcUd3I",
  "key_fp":"76bd1ef75f79446f5728ff03675a6070"}}
{"ts":"2026-05-24T00:09:35.807181Z","level":"WARN","fields":{
  "message":"sandbox/nomad-ch create error",
  "sandbox_id":"019e5750-d340-7cd2-ae98-76e62188e7d6",
  "step":"wait_for_alloc_running",
  "error":"nomad alloc terminal status=failed: Failed tasks"}}
{"ts":"2026-05-24T00:09:35.807222Z","level":"ERROR","fields":{
  "status":500,"error":"backend.create: nomad alloc terminal status=failed: Failed tasks"}}
{"ts":"2026-05-24T00:09:35.809628Z","level":"INFO","fields":{
  "message":"sandbox/nomad-ch vm_index released","vm_index":1,
  "reason":"create-failure-cleanup","job":"zsbx-019e5750d3407cd2ae9876e62188e7d6"}}
```

Wrapper stdout/stderr were unavailable post-mortem: the alloc was GC'd by Nomad ~400ms after task exit (`restart_attempts=0` means terminal failure → immediate GC; the alloc dir was already gone by the time we shelled in). This matches the prior `…cluster-2026-05-25-r1.md` observation that the wrapper's `:?`-style bash guard kills the task before Nomad's logmon can flush captured FDs.

## Findings

### Finding A — Latent driver-install bug: `plugin_dir` alone is insufficient

The Nomad 2.0.2 plugin loader emits:

```
[WARN] agent.plugin_loader: plugin not referenced in the agent configuration file,
       loading skipped: plugin_dir=/etc/zeroship/nomad-plugins plugin=nomad-driver-ch
```

The worker startup (`gcp-worker-startup.sh:185-188`) writes only:

```hcl
plugin_dir = "/etc/zeroship/nomad-plugins"
```

For the plugin to actually be loaded, Nomad additionally requires a matching `plugin "<name>" { … }` stanza in the agent config (the `<name>` must match the binary name, i.e. `nomad-driver-ch`). Without it, Nomad detects the binary, prints the WARN, and skips it. The result is identical to the binary not existing.

**Effect:** even with the controller already routing to driver=`ch` (which it currently does NOT — see Finding B), Nomad would reject the job because no client node has the `ch` driver healthy.

**Suggested fix** (out of scope here):

```hcl
plugin_dir = "/etc/zeroship/nomad-plugins"

plugin "nomad-driver-ch" {
  config {}
}
```

…in `/etc/nomad.d/plugin-dir.hcl` (or split into a separate file).

### Finding B — Provision-script controller pin is too old for T-8b

`crates/sandbox/scripts/provision-gcp-cluster.sh` defaults:

```
CONTROLLER_OBJECT=${CONTROLLER_OBJECT:-zeroship-sandbox.snapshot-v6}
```

`zeroship-sandbox.snapshot-v6` predates BOTH:

- `a4c481e1` — `sandbox/nomad-ch: inject ZSBX_SANDBOX_ID into Nomad task env (B24)` — the R8-DEPLOY1 wrapper kill-switch fix. **This is the same root cause documented at length in `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-r1.md`** (HARD-FAIL on Phase 1 c=4, 0/16 CREATE).
- `5fe36805` — `sandbox/nomad-ch: SANDBOX_TASK_DRIVER=ch_plugin flag switches jobspec to typed Go driver (T-7)` — the T-7 jobspec switch this entire sprint hinges on.

**Effect:** the deployed controller (a) does not inject `ZSBX_SANDBOX_ID`, so the wrapper kill-switch fires, and (b) silently ignores `SANDBOX_TASK_DRIVER=ch_plugin` and still emits `raw_exec` jobs. The Go driver was never exercised even once.

This is *the same* unresolved blocker from the prior cluster review. No new bake of the controller binary has happened between that review and this one.

### Finding C — Sentinel does not gate on plugin-install verification

The worker startup emits `zsbx-worker-ready` (line 480) without confirming Nomad actually loaded the plugin. The provision script then declares the cluster healthy. For cutover gates like T-8b, the sentinel should additionally assert that `nomad node status -self -verbose` shows `Driver  ch  Healthy=true`. Without that gate, the install bug in Finding A is invisible until a sandbox create attempt.

(Recommendation only; this report does not modify the startup script.)

## Teardown (Step 4)

```
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
[teardown] releasing internal addresses: zsbx-prod-server-1-ip
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down
```

Post-teardown `gcloud compute instances list --filter='name~"^zsbx-"' --format='value(name)'`: empty (0 residual instances).

Teardown log: `/tmp/t8b-smoke-teardown.log`.

## Estimated cost

| Resource | Rate (asia-northeast3, list price) | Duration | Line cost |
|---|---|---|---|
| 1 × n2-standard-4 (controller/server) | ~$0.21 / hr | ~7 min (provision to teardown) | ~$0.025 |
| 1 × n2-standard-32 (worker, nested-virt) | ~$1.86 / hr | ~7 min | ~$0.217 |
| GCS reads (binaries pulled by worker startup) | $0.01 / GB egress within region | <1 GB total | <$0.01 |
| GCS write (this run's `nomad-driver-ch.v1`, 20 MB) | $0.005 / 1000 ops + $0 storage <1mo | 1 op | ~$0 |
| Static IPs (1× reserved, released on teardown) | $0.01 / hr unused | n/a (in use whole time) | ~$0 |
| Egress to operator (SSH via IAP) | small | <100 MB | <$0.01 |
| **Total** | | | **≈ $0.25** |

Well under the $1 envelope for this run and the $30 hard cap for the sprint.

## Required before next cluster cycle (T-8b retry)

In priority order — all are out of scope for THIS sprint per the brief, but the next attempt cannot succeed without them:

1. **Bake + upload a fresh controller binary** (`zeroship-sandbox.snapshot-v7` or `v18`) that includes commits `a4c481e1` (B24) and `5fe36805` (T-7). Update `CONTROLLER_OBJECT` default in `provision-gcp-cluster.sh` to the new object.
2. **Add the `plugin "nomad-driver-ch" { config {} }` stanza** to `/etc/nomad.d/plugin-dir.hcl` in `gcp-worker-startup.sh:185-188`. Without this, the binary is detected-and-skipped.
3. (Nice-to-have) Add a post-startup assertion in `gcp-worker-startup.sh` that `nomad node status -self -verbose | grep -E '^ch\s+true\s+true'` succeeds when `INSTALL_CH_PLUGIN_DRIVER=1` — sentinel should fail loudly, not silently.

Once 1 + 2 land, re-run **this same** 1+1 smoke before attempting T-8b-stress.

## Recommendation: NO-GO for T-8b-stress

T-8b-smoke did not validate any of the T-8b parity claims. The Go driver binary was correctly deployed but never reached by the controller, and even if it had been, Nomad would have refused to start a task against it. Running the c=20 / 3-worker stress cycle on this fleet shape would burn ~$15-$25 to surface the same two failures at higher concurrency.

Block T-8b-stress until both the controller pin and the Nomad plugin-config stanza land. Wrapper-removal (T-8b-cutover) is correctly gated behind T-8b-stress and remains out of reach.

## Artefacts (this run)

- Provision log: `/tmp/t8b-smoke-provision.log`
- Teardown log: `/tmp/t8b-smoke-teardown.log`
- Budget marker: `/tmp/zsbx-cluster-budget-20260524` (1 line — first cluster of the day)
- Binary uploaded: `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v1`
  (sha256 `df2b117511e9f218ca20ccd3ce74e1c2d37c65cfb8deeeca4b9b70d9621a25f8`,
   embedded gitSHA `ce118450`)
- Driver-side change: none (this sprint did not touch driver code)
- Sandbox-side change: `EXTRA_WORKER_METADATA` env-var pass-through added to `provision-gcp-cluster.sh`
