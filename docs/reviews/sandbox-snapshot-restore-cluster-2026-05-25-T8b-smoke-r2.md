# T-8b-smoke-retry cluster validation — 2026-05-25 r2 (Go ch_plugin driver, 1+1 fleet, v19+v2)

**Sprint:** T-8b-smoke-retry — second 1-worker smoke after all 4 T-8b-smoke r1 blockers fixed.
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.
**Worktree (driver):** `/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch`.
**Verdict:** **FAIL — smoke failure on CREATE again. All 4 r1 blockers confirmed FIXED. New bug surfaced: Go plugin spawns `cloud-hypervisor --config <json>`, which CH v51.1 rejects with `unexpected argument '--config'`.**
**Recommendation:** **NO-GO for T-8b-stress.** Driver-side fix required (translate the JSON config into CLI flags, or drive boot via `--api-socket` REST like the bash wrapper).

## TL;DR

All four r1 blockers landed and verified:

| Blocker | r1 status | r2 status |
|---|---|---|
| Plugin loader skipped binary (missing `plugin "..." {}` stanza) | FAIL — only `plugin_dir` written | **PASS — both `plugin_dir` AND `plugin "nomad-driver-ch" { config {} }` present; `ch Detected=true Healthy=true ready`** |
| Controller pin too old to honour `SANDBOX_TASK_DRIVER` (was `snapshot-v6`) | FAIL — controller still emitted `raw_exec` jobs | **PASS — controller pin `snapshot-v19` (b4500576 + b3bf741c bake); `Environment=SANDBOX_TASK_DRIVER=ch_plugin` on `zsbx-ctl` unit; allocs route to driver `ch` (verified in Nomad journal)** |
| R12-I1 wake-path `TaskDriverMode` arg | FAIL — wake hardcoded raw_exec | **PASS — b3bf741c included in `snapshot-v19`** |
| G4 tap rollback on partial `StartTask` failure | FAIL — leak on partial failure | **PASS — driver pin `nomad-driver-ch.v2` = 2982e0b9; rollback present** |

But CREATE still fails — at a new, downstream failure mode. The Go driver IS now being invoked (`client.driver_mgr.nomad-driver-ch: ch: StartTask: spawned ch_pid=…`) but the spawned `cloud-hypervisor` process exits with **`exit_code=2`** in ≤4ms, before the VM can ever boot:

```
[INFO] client.driver_mgr.nomad-driver-ch: ch: VM exited:
       driver=ch @module=ch ch_pid=13818 exit_code=2
       task_id=…/ch/1a212de8
[INFO] client.alloc_runner.task_runner: Task event: …
       type=Terminated msg="Exit Code: 2, Exit Message:
       \"exit status 2: error: unexpected argument '--config' found

       Usage: cloud-hypervisor --api-socket <api-socket>

       For more information, try '--help'.\""
```

The controller retries 3× (same failure each time), exhausts the budget, returns 503 to the smoke client (~92.9s total wall). p50 timings: not measurable (CREATE never landed; SNAP/WAKE never attempted).

**Root cause** is a bug in the Go plugin's `StartTask`: it writes a CH JSON config to `local/config.json` and spawns `cloud-hypervisor --config <path>`, but CH v51.1 has no `--config` flag. Its `--help` enumerates only individual CLI options (`--cpus`, `--memory`, `--kernel`, `--disk`, `--net`, `--cmdline`, `--api-socket`, …). The bash wrapper `nomad-vm-wrapper.sh` boots CH via `--api-socket` + a sequence of `ch-remote` REST calls — not a config file. The Go plugin needs the same approach (or a JSON→CLI translation layer).

This bug was masked in r1 because the alloc never reached the Go driver: the plugin loader skipped the binary, the controller still emitted `raw_exec`, and the wrapper itself failed earlier on the `ZSBX_SANDBOX_ID` guard.

## Pre-req verification (all PASS)

| Check | Expected | Observed |
|---|---|---|
| `git log --oneline -8` includes all 4 commits | 9f1dfc99, 75e8a4ea, b3bf741c, b4500576 | All present (plus 2982e0b9 in driver worktree) |
| `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v19` | exists | OK |
| `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v2` | exists | OK |
| `provision-gcp-cluster.sh` default `CONTROLLER_OBJECT` | `zeroship-sandbox.snapshot-v19` | OK (line 55) |
| `gcp-worker-startup.sh` pulls `nomad-driver-ch.v2` | yes when `INSTALL_CH_PLUGIN_DRIVER=1` | OK (line 176) |
| Daily budget marker line count | < 10 | 1 → 2 (this run) |
| `EXTRA_WORKER_METADATA` env honored | yes | OK (provision-gcp-cluster.sh:60-62, 271-273); log line `[provision] extra worker metadata: install-ch-plugin-driver=1` |

## Provision (Step 1)

Invocation:

```
SERVER_COUNT=1 WORKER_COUNT=1 \
  EXTRA_WORKER_METADATA="install-ch-plugin-driver=1" \
  bash crates/sandbox/scripts/provision-gcp-cluster.sh
```

| Phase | Time |
|---|---|
| Server `zsbx-prod-server-1` create + `zsbx-server-ready` sentinel | 60s |
| Worker `zsbx-prod-worker-1` create + `zsbx-worker-ready` sentinel | 45s |
| Provision wall (network + IP reserve + both sentinels, sequential) | ~105s |

Final fleet:

```
NAME                ZONE               MACHINE_TYPE    PRIVATE_IP   STATUS
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.15  RUNNING
```

Provision log: `/tmp/t8b-smoke-retry-provision.log`.

## Cluster validation (each prior r1 blocker verified)

### Plugin install + Nomad driver health (was the #1 r1 blocker)

```
---PLUGIN-DIR.HCL:
plugin_dir = "/etc/zeroship/nomad-plugins"

plugin "nomad-driver-ch" {
  config {}
}

---PLUGIN BINARY:
-rwxr-xr-x 1 root root 20181176 May 24 00:36 nomad-driver-ch

---NODE DRIVERS (nomad node status -self -verbose):
Driver    Detected  Healthy  Message                             Time
ch        true      true     ready                               2026-05-24T00:36:16Z
docker    false     false    Failed to connect to docker daemon  2026-05-24T00:36:16Z
exec      true      true     Healthy                             2026-05-24T00:36:16Z
java      false     false    <none>                              2026-05-24T00:36:16Z
qemu      true      true     Healthy                             2026-05-24T00:36:16Z
raw_exec  true      true     Healthy                             2026-05-24T00:36:16Z
```

`ch Detected=true Healthy=true ready` — the entire purpose of the b4500576 fix, end-to-end verified.

### Controller env (was the #2 r1 blocker)

```
---ZSBX-CTL STATUS:
● zsbx-ctl.service - zeroship-sandbox controller
     Active: active (running) since Sun 2026-05-24 00:36:16 UTC
   Main PID: 13616 (zeroship-sandbo)

---ZSBX-CTL ENV (Environment=…):
SANDBOX_BACKEND=nomad-ch
SANDBOX_TASK_DRIVER=ch_plugin

---LIVEZ:
{"status":"ok"}
```

### Driver pin (was the #4 r1 blocker)

```
---NOMAD-DRIVER-CH VERSION:
nomad-driver-ch 2982e0b9        ← matches the tap-rollback fix commit
```

## Smoke result (Step 2) — FAIL

`sudo /opt/stress/snapshot_stress.py --concurrency 1 --cycles 1 --base-url http://127.0.0.1:9091 --token-file /etc/zeroship/sandbox-token --admin-token-file /etc/zeroship/sandbox-admin-token`

Verbatim output:

```
# snapshot-stress: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091
# elapsed: 92.9s

=== snapshot-stress (N=1) ===
CREATE OK: 0/1
SNAPSHOT OK: 0/0
WAKE OK: 0/0
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED CREATES: 1
  [1x] code=503: {"error":"create_retry_budget_exhausted","message":"backend.create:
       3 attempts failed; last error: agent at http://10.99.101.2:7777 never returned
       200 on /livez (expected fp=89dabe0ee98be641c4113073bef1f2db)"}

=== RAW_JSON_BEGIN ===
[{"idx": 0, "user_id": "usr_033Lz0nZpwxcnoVtUpojU7",
  "project_id": "prj_033Lz0nZpo0lWn5GsesWMb",
  "create_code": 503, "create_ms": 92865.83041099999,
  "create_body": "{\"error\":\"create_retry_budget_exhausted\",\"message\":
                  \"backend.create: 3 attempts failed; last error: agent at
                   http://10.99.101.2:7777 never returned 200 on /livez
                   (expected fp=89dabe0ee98be641c4113073bef1f2db)\"}"}]
=== RAW_JSON_END ===
```

### Single-sample timings (per cycle)

| Step | Time | Note |
|---|---|---|
| CREATE | (never landed; 92.9s wall = 3 × ~30s budgets + retry backoff) | bug C-1 below |
| SNAPSHOT | not attempted | — |
| WAKE | not attempted | — |

## Failure trace (verbatim — Nomad journal, controller log)

Three identical `StartTask → VM exited (exit_code=2) → Terminated` cycles, one per controller retry. Sample (alloc 5ef86006):

```
00:37:51.473Z [INFO] task_runner: Task event:
                      alloc_id=5ef86006-… task=ch type=Received   failed=false
00:37:51.526Z [INFO] client.driver_mgr.nomad-driver-ch:
                      ch: StartTask: driver=ch task_id=5ef86006-…/ch/1a212de8
                      @module=ch mode=cold_boot
                      sandbox_id=019e576ab4c97b838843e8cce7e2acfc vm_index=1
00:37:51.530Z [INFO] client.driver_mgr.nomad-driver-ch:
                      ch: StartTask: spawned: driver=ch @module=ch
                      ch_pid=13818 tap=zsbx-nm-1
                      api_socket=/opt/nomad/data/alloc/5ef86006-…/ch/local/ch.sock
                      config=/opt/nomad/data/alloc/5ef86006-…/ch/local/config.json
00:37:51.531Z [INFO] client.driver_mgr.nomad-driver-ch:
                      ch: VM exited: driver=ch @module=ch ch_pid=13818
                      exit_code=2   ← 1ms after spawn
00:37:51.533Z        task=ch type=Started     msg="Task started by client" failed=false
00:37:51.535Z        task=ch type=Terminated  msg="Exit Code: 2, Exit Message:
                       \"exit status 2: error: unexpected argument '--config' found

                       Usage: cloud-hypervisor --api-socket <api-socket>

                       For more information, try '--help'.\""
00:37:51.581Z [INFO] client.driver_mgr.nomad-driver-ch:
                      ch: DestroyTask: complete: driver=ch
00:37:51.581Z        task=ch type="Not Restarting" msg="Policy allows no restarts" failed=true
```

Note: `driver=ch` (not `raw_exec`). The Go plugin IS receiving the StartTask; this is exactly the wiring T-8b was meant to validate. The wiring is correct; the plugin's spawn arguments are wrong.

Two more identical alloc cycles followed (86ca6a78, aeda2e4b), each producing the same `unexpected argument '--config' found` error. Controller then returned `create_retry_budget_exhausted` to the client.

## Bug C-1 — Go plugin's `StartTask` spawns `cloud-hypervisor --config <json>`, which doesn't exist in CH v51.1

```
$ /usr/local/bin/cloud-hypervisor --version
cloud-hypervisor v51.1

$ /usr/local/bin/cloud-hypervisor --help | head
Launch a cloud-hypervisor VMM.

Usage: cloud-hypervisor [OPTIONS]

Options:
      --api-socket <api-socket> …
      --balloon …
      --cmdline …
      --console …
      --cpus …
      --disk …
      --kernel …
      --memory …
      --net …
      [no --config option]
```

The Go plugin (`crates/.../nomad-driver-ch/start_task` in worktree
`/home/ruiyang/Projects/appbase/.worktrees/nomad-driver-ch/nomad-driver-ch`) writes the
VM spec to `local/config.json` and spawns:

```
cloud-hypervisor --api-socket <…/ch.sock> --config <…/config.json>
```

But CH v51.1 doesn't accept `--config`. Two viable fixes:

- **Fix A (parity with bash wrapper):** spawn `cloud-hypervisor --api-socket <socket>` only, then `ch-remote` the `vm.create` / `vm.boot` REST calls in sequence using the JSON config payload. This matches `crates/sandbox/scripts/nomad-vm-wrapper.sh` exactly.
- **Fix B:** unmarshal the JSON and translate every field into the CH CLI flag form (`--cpus boot=N`, `--memory size=…`, `--kernel …`, `--disk path=…`, `--net …`, `--cmdline …`). Brittle (every CH version bump can drift flag spelling).

Fix A is what the bash wrapper has been doing through 8 prior cluster smokes. Recommend mirroring it.

This bug is a **driver-side fix only** — no controller change required. It belongs in the next driver bake (`nomad-driver-ch.v3`).

## Why the controller still ended up returning 503

```
controller create budget = 3 attempts × ~30s alloc-poll budget = ~90s
   alloc 1: scheduled, ch_plugin StartTask, CH spawn exit 2, ~30s poll → terminal=failed
   alloc 2: scheduled, ch_plugin StartTask, CH spawn exit 2, ~30s poll → terminal=failed
   alloc 3: scheduled, ch_plugin StartTask, CH spawn exit 2, ~30s poll → terminal=failed
   controller: 3 attempts exhausted → 503 create_retry_budget_exhausted
   smoke client: total elapsed 92.9s, returned 1 failure
```

The error message visible to the client ("agent never returned 200 on /livez") is the controller's _outer_ wait-for-agent-livez budget — the inner Nomad alloc terminal failure is what actually fired. Both messages are technically correct but the second-level one (Nomad task exit code 2) is the actionable root cause.

## Teardown (Step 3)

```
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
[teardown] releasing internal addresses: zsbx-prod-server-1-ip
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down
```

Post-teardown `gcloud compute instances list --filter='name~"^zsbx-"' --format='value(name)'`: **empty (0 residual instances)**.

Teardown log: `/tmp/t8b-smoke-retry-teardown.log`.

## Estimated cost

| Resource | Rate (asia-northeast3, list price) | Duration | Line cost |
|---|---|---|---|
| 1 × n2-standard-4 (server) | ~$0.21 / hr | ~8 min (provision to teardown) | ~$0.028 |
| 1 × n2-standard-32 (worker, nested-virt) | ~$1.86 / hr | ~8 min | ~$0.248 |
| GCS reads (binaries pulled by worker startup) | $0.01 / GB egress within region | <1 GB | <$0.01 |
| Static IP (1× reserved, released on teardown) | $0.01 / hr unused | in use whole time | ~$0 |
| Egress (SSH via IAP, smoke client locally) | small | <100 MB | <$0.01 |
| **Total** | | | **≈ $0.30** |

Under the $0.50 envelope set in the brief.

## Recommendation: NO-GO for T-8b-stress

T-8b-smoke-retry validated 4 of 5 things required for T-8b-stress:

- Worker pulls v2 driver, mode 0755, owner root.
- Nomad loads the plugin; `ch Detected=true Healthy=true`.
- Controller pin `snapshot-v19` has the `SANDBOX_TASK_DRIVER=ch_plugin` flag honored end-to-end (allocs route to `driver=ch`, not `raw_exec`).
- R12-I1 wake-path `TaskDriverMode` arg compiled in.
- G4 tap rollback in driver v2 (2982e0b9).

…but the Go plugin's `StartTask` invokes a `cloud-hypervisor` CLI argument (`--config`) that CH v51.1 does not accept. Every CREATE attempt fails at VM-spawn time in 1ms with exit code 2. No T-8b parity claim was exercised against a running VM.

Running c=20 / 3-worker stress now would spend ~$15-25 to surface this same failure 60× in parallel. Bug C-1 must land first.

**Block T-8b-stress until `nomad-driver-ch.v3` ships a fixed `StartTask` (recommend Fix A: spawn `cloud-hypervisor --api-socket <…>` and drive the boot via `ch-remote` REST, mirroring `nomad-vm-wrapper.sh`).** Re-run the same 1+1 smoke before attempting T-8b-stress.

Wrapper removal (T-8b-cutover) remains correctly gated behind T-8b-stress.

## Artefacts (this run)

- Provision log: `/tmp/t8b-smoke-retry-provision.log`
- Stress log:    `/tmp/t8b-smoke-retry-stress.log`
- Teardown log:  `/tmp/t8b-smoke-retry-teardown.log`
- Budget marker: `/tmp/zsbx-cluster-budget-20260524`
  (2 lines this UTC day: T-8b-smoke r1 + T-8b-smoke-retry r2; under the 10/day cap)
- Pins: controller `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v19`,
        driver     `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v2`
- Sandbox-side change: **none** (this sprint only ran the smoke; no source edits)
- Driver-side change: **none** (the C-1 fix belongs in a future driver-worktree PR)
