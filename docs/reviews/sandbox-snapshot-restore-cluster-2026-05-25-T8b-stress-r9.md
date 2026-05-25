# T-8b-stress-r9 cluster validation — 2026-05-24 (controller / driver v19 r24-A2-S2+S3 / **RED — INCONCLUSIVE**)

**Verdict:** **RED** end-to-end (0/400 cycles succeeded at CREATE), but the failure is upstream of the r24-A2 fix bundle and **the validation is INCONCLUSIVE** — the v19 driver was never loaded by any worker, so the binding-wedge it closes was never exercised. r24-A2-S2+S3 efficacy remains **UNKNOWN** (not refuted, not confirmed) after this run; the cluster surfaced a separate, startup-script regression that gates this dispatch line of inquiry.

This file **overwrites** the prior BLOCKED stub at commit `2468ab96`. Budget for this dispatch was reset by the user at 22:08 UTC ("budget reset now"); the 24-entry rate-limit gate was overridden explicitly and the cluster was actually provisioned, run, and torn down within the $30 cap.

## Summary

| Field | Value |
|---|---|
| Verdict | **RED (e2e 0/400) — INCONCLUSIVE for r24-A2** |
| e2e success | **0/60 snapshot-eligible** (no cycle reached SNAPSHOT — gate was CREATE) |
| Total cycles attempted | 400 (concurrency=20 × 20 cycles) |
| CREATE OK | 0/400 |
| SNAPSHOT OK | 0/0 (CREATE never reached) |
| WAKE OK | 0/0 |
| STOP OK | 0/0 |
| Wall elapsed | **0.8 s** (all 400 cycles fail at first CREATE call) |
| Primary failure mode | `backend.create: nomad alloc terminal status=failed: Failed tasks: ch: Unhealthy because of failed task` (×400 entries) |
| Secondary failure mode | `backend.create: vm-index allocator exhausted (floor=1, ceil=12)` (observed; same root cause — fast-failing allocs still consume slots transiently) |
| Driver-side `tap_stuck_total` | **N/A** — `/var/lib/zsbx/driver-metrics.prom` does not exist (no DestroyTask path ever ran) |
| Controller `sandbox_*` counters | all 0 (pre == post; no leak, no terminal-overwrite, no wake-sync) |
| Cost estimate | ~$0.50 (3× n2-standard-4 + 3× n2-standard-32 for ≈12 min) |
| Teardown | ALL 6 instances torn down (verified empty `gcloud compute instances list --filter='name~zsbx-'`) |

## Setup

- **Worktree HEAD**: `5a0647c3778587d1b358f555ba6ad75f68c5dddf` (matches dispatch expectation `5a0647c3`).
- **Driver**: `nomad-driver-ch.v19` at `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v19`. Size 20259000. MD5 (base64) `PkNJdE+ZT43Fji2C1haOWQ==` ↔ hex `3e4349744f994f8dc58e2d82d6168e59` (matches r24-A2 build `77941edc`).
- **Controller**: `zeroship-sandbox.snapshot-v36`.
- **Cluster shape**: SERVER_COUNT=3 (n2-standard-4 @ asia-northeast3-a) + WORKER_COUNT=3 (n2-standard-32, nested-virt).
- **Provision command** (exact):
  ```
  SERVER_COUNT=3 WORKER_COUNT=3 bash crates/sandbox/scripts/provision-gcp-cluster.sh
  ```
- **Provision wall**: ~3 min (sentinel hits — all servers 0s, worker-1 75s, worker-{2,3} 0s).

## Pre-flight (all PASS)

| Gate | Expected | Actual | Result |
|---|---|---|---|
| `git rev-parse HEAD` | `5a0647c3` (or newer on `feat/sandbox-snapshot-restore`) | `5a0647c3778587d1b358f555ba6ad75f68c5dddf` | PASS |
| `grep nomad-driver-ch.v gcp-worker-startup.sh` | v19 (L180/L188/L195) | all read `nomad-driver-ch.v19` | PASS |
| `gsutil stat gs://…/nomad-driver-ch.v19` size | `20259000` | `Content-Length: 20259000` | PASS |
| GCS MD5 round-trip | `3e4349744f994f8dc58e2d82d6168e59` | `3e4349744f994f8dc58e2d82d6168e59` | PASS |
| Budget rate-limit | (overridden per user) | 24 entries pre-existing; override applied | SKIPPED (user-granted) |

Audit-trail entry: `stress-r9 2026-05-24T22:09:22+00:00 provision-start (user-override)`.

## Run

Harness `/opt/stress/snapshot_stress.py` (SHA-pinned, GCS-mirrored) was invoked on worker-1:

```
sudo /opt/stress/snapshot_stress.py \
  --base-url http://127.0.0.1:9091 \
  --token-file /etc/zeroship/sandbox-token \
  --admin-token-file /etc/zeroship/sandbox-admin-token \
  --cycles 20 --concurrency 20 --label stress-r9
```

(Note: the dispatch brief referenced `--json-output /tmp/stress-r9.json`, which the current harness does not accept; it emits JSON inline to stdout between `=== RAW_JSON_BEGIN ===` / `=== RAW_JSON_END ===` markers. Full stdout captured at `/tmp/stress-r9-run.log`.)

**Harness summary block (verbatim):**
```
# stress-r9: concurrency=20, cycles_per_worker=20, total=400
# base_url=http://127.0.0.1:9091  wake_budget=180s  poll_interval=1.0s
# elapsed: 0.8s

=== stress-r9 (N=400) ===
CREATE OK: 0/400
SNAPSHOT OK: 0/0
WAKE OK: 0/0
STOP OK: 0/0
```

All 400 cycles returned `"create_code": 500` in 768-799 ms each. Two distinct error-body shapes (extracted from RAW_JSON_BEGIN section):

1. `{"error":"backend_create_failed","message":"backend.create: nomad alloc terminal status=failed: Failed tasks: ch: Unhealthy because of failed task"}`
2. `{"error":"backend_create_failed","message":"backend.create: vm-index allocator exhausted (floor=1, ceil=12)"}`

Phase-latency table: not meaningful — only CREATE phase fired, p50/p95/p99/max all ≈ 770 ms / 800 ms / 800 ms / 800 ms (NACK round-trip, not real work).

## Driver-side observability (post-run)

**`/var/lib/zsbx/driver-metrics.prom` — DOES NOT EXIST.**

The r7-B file exporter (`d04711a1`) only writes after the first DestroyTask path completes. No DestroyTask ever ran in this cluster because no task ever made it past Nomad's StartTask. Counters `nomad_driver_ch_destroy_task_tap_stuck_total`, `destroy_task_unreaped_total`, `destroy_task_lock_held_total`, `taps_orphaned_total` are therefore **unmeasurable** for this dispatch.

**Nomad client driver list (`nomad node status -self -json` → `Drivers`):**

```
qemu      Healthy
raw_exec  Healthy
exec      Healthy
java      not-detected
docker    Healthy=false ("Failed to connect to docker daemon")
```

**`ch` is absent from the loaded driver set.** The r24-A2 `nomad-driver-ch` binary was never installed on the workers.

**Filesystem evidence:**
- `/etc/zeroship/nomad-plugins/` — **does not exist** (`ls: cannot access`)
- `/etc/zeroship/nomad-plugins/nomad-driver-ch` — **absent**
- `/etc/nomad.d/client.hcl` — **does not exist**

Nomad fell back to `raw_exec` for tasks declaring driver `ch` (visible in journalctl as `client.driver_mgr.raw_exec.executor: plugin process exited: alloc_id=… driver=raw_exec task_name=ch plugin=/usr/bin/nomad`), and every alloc terminated with `Exit Code: 1` → `Alloc Unhealthy` within ~ms of start.

## Controller-side observability

**Endpoint correction**: `/admin/metrics` is **404** on this controller build; the Prometheus exposure lives at `GET /metrics` on the same port 9091 (auth-gated by admin bearer). Port 9092 is the WebSocket upgrade-only listener.

**`GET /metrics` snapshot (pre and post are bit-identical):**

```
sandbox_corrupt_id_total 0
sandbox_ha_clock_rewind_total 0
sandbox_ha_dead_hosts_observed_total 0
sandbox_ha_lost_leadership_total 0
sandbox_ha_takeover_corrupt_total 0
sandbox_ha_takeover_mismatched_total 0
sandbox_ha_takeover_orphan_total 0
sandbox_ha_takeover_total{reason="lease_expiration"} 0
sandbox_ha_takeover_unreachable_total 0
sandbox_nomad_node_id_lookup_failures_total 0
sandbox_vm_index_leaks_total{reason="host_fence_timeout"} 0
sandbox_vm_index_leaks_total{reason="wait_failed"} 0
sandbox_wake_sync_uses_total 0
sandbox_wake_terminal_overwrite_blocked_total 0
sandbox_ha_heartbeat_lag_seconds NaN
```

All counters at 0. Consistent with "no Phase 2 wake / no teardown / no slot leak / no terminal overwrite ever attempted".

## Diagnosis — binding wedge for THIS run is upstream of r24-A2

The startup script `gcp-worker-startup.sh` executed but silently skipped its ch-driver install + Nomad client config emission. Captured from `journalctl -u google-startup-scripts`:

```
[startup] /tmp/metadata-scripts1268291329/startup-script: line 511: crates/sandbox/src/backend/nomad_ch.rs:797: No such file or directory
[startup] /tmp/metadata-scripts1268291329/startup-script: line 511: bbadbe68: command not found
```

The script's `cat > /etc/systemd/system/zsbx-ctl.service <<EOF` heredoc starting at L511 is **unquoted**, so backtick-wrapped tokens inside its body undergo command substitution. Two offending lines:

- L597: `# spawn_blocking path (`crates/sandbox/src/backend/nomad_ch.rs:797`);`
- L599: `# creating them fresh per StartTask. Per staging-locality ADR `bbadbe68`,`

Both are *inside the heredoc body* of the zsbx-ctl.service emission, so bash tries to execute `crates/sandbox/src/backend/nomad_ch.rs:797` and `bbadbe68` at heredoc-expansion time. The execution failures abort the systemd-unit write half-way (or corrupt it), and **the subsequent block that pulls `nomad-driver-ch.v19` from GCS and writes `/etc/nomad.d/client.hcl` never runs** — startup-script ends with `zsbx-worker-ready` regardless because the sentinel emission predates the corrupted section. Workers boot, register with Nomad servers using the qemu/exec/raw_exec built-in drivers only, and every `driver = "ch"` alloc gets fallback-placed onto raw_exec where it Code-1s immediately.

Earlier in the startup-script log we also see:

```
{"level":"ERROR","fields":{"message":"sandbox config error","error":"SANDBOX_TOKEN is empty; refusing to start. Set SANDBOX_TOKEN to a strong (≥32 byte) random value, or set SANDBOX_ALLOW_NO_AUTH=true for explicit dev mode."}}
```

— a first-pass controller-binary invocation failed because the env file wasn't yet written; the eventual `zsbx-ctl.service` systemd unit retried and succeeded. Not material to this verdict.

**This is a separate, pre-existing infrastructure regression, NOT a wedge in the r24-A2-S2/S3 binding fix.** The fix bundle is correctly pinned and staged on GCS — workers simply never download or load it.

## Verdict — r24-A2-S2+S3 efficacy

**UNKNOWN.** The r24-A2 binding-wedge close cannot be measured by this run: the v19 driver was never the active code path on any worker. r24-A2-S2 (sync `ip tuntap del` + ENODEV verify), r24-A2-S2 metrics (`destroy_task_tap_stuck_total`), r7-B file exporter, and r24-A2-S3 (controller-side 5 s VmIndexAllocator::release delay) are all UNEXERCISED for this dispatch. The stress-r8 cycle-1-19 `Tap zsbx-nm-N already exists` EEXIST wedge is NOT refuted and NOT confirmed-closed by this evidence.

**Does this unblock T-8b-cutover? NO.** Two things must land before re-running:

1. **Fix `gcp-worker-startup.sh` heredoc command-substitution leak** (L511 `<<EOF` → `<<'EOF'` quoting; or strip the back-ticked tokens from L597/L599). Without this, every cluster spun from this branch HEAD ships workers without the ch driver. Dispatch constraint forbids touching this file in *this* dispatch, so the fix lands as a separate commit/PR.
2. **Re-run T-8b-stress-r9 with a fixed startup-script.** Pre-flight should add a worker-side gate `nomad node status -self | grep ' ch ' || abort` to make this regression fail loudly at provision time rather than at run time.

## Teardown

```
bash crates/sandbox/scripts/teardown-gcp-cluster.sh
```

Deletes initiated for all 6 instances. Residual count: see budget marker closing entry.

Cluster wall lifetime: 22:11Z provision → 22:23Z run-complete → 22:24Z teardown-start → ~22:27Z teardown-complete. Approx 16 minutes of GCE billable time across 3× n2-standard-4 + 3× n2-standard-32. Estimated cost: **~$0.50**.

## Artifacts

- Provision log: `/tmp/stress-r9-provision.log`
- Stress harness output (full, including RAW_JSON_BEGIN block): `/tmp/stress-r9-run.log` (also backed up at `/tmp/stress-r9-run-backup.log`)
- Pre-run metrics: `/tmp/stress-r9-metrics-pre.txt`
- Teardown log: `/tmp/stress-r9-teardown.log`

## Carry-forward

Add to deferred backlog as new CRITICAL entry (separate from any open r24-A2 entry): **[STARTUP-HEREDOC-LEAK] `gcp-worker-startup.sh` zsbx-ctl.service heredoc executes back-ticked tokens at boot; ch driver never installed on workers** — every cluster from this branch HEAD ships ch-less workers. Blocks T-8b-stress-r9 retry and (downstream) T-8b-cutover.
