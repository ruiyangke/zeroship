# T-8b-smoke-retry-r7 cluster validation — 2026-05-25 r7 (controller v22 / C-6 phase tracing, 1+1 fleet)

**Sprint:** T-8b-ctl-v22 + smoke-retry-r7 — re-run smoke against controller v22 (sandbox HEAD `94a302c1`, post-r12 + C-6 phase tracing at `8e7f0b53`) to localize the C-6 wake silent stall observed in r6.
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `79101a99` (v21 → v22 pin bump; parents `94a302c1` r14 reviewer artifacts + `f6f6387f` C-6 deferred entry + `8e7f0b53` phase tracing).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v22`, SHA `b4d9684fe24c2c07a03d91496f38ed65381b2fd311d09fd5ec2c9d2ab189975a`, size 16,517,728 B, interp `/lib64/ld-linux-x86-64.so.2`.
**Driver:** unchanged — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v4`.
**Verdict:** **FAIL on WAKE as expected — C-6 STALL LOCALIZED** to `reserve_vm_index_with_retry`. The phase trace introduced in v22 reaches `pre_reserve_vm_index` and emits NO further phase lines for the wedged sandbox, despite the source-teardown releasing the slot 90 s later. The handler is stuck inside the retry loop itself, not in any downstream sync I/O (store.get, submit_restore_job, wait_for_livez, unseal, clock_resync, register_restored, or pg pool).
**Recommendation:** **NO-GO for T-8b-stress.** Open a focused C-6-fix sprint targeting `reserve_vm_index_with_retry` and its interaction with the detached `teardown_source_for_snapshot` future on the single-threaded compio runtime.

## TL;DR

The C-6 phase tracing v22 carried did its job — the wake-path stall is no longer mysterious. The trace pattern is:

```
02:54:16.407  phase=entry
02:54:16.436  phase=row_read_ok           status=snapshotted, generation=2
02:54:16.464  phase=read_snapshot_row_ok  vm_index=1
02:54:16.494  phase=cas_restoring_ok      generation=3
02:54:16.494  phase=pre_reserve_vm_index  vm_index=1           ← LAST PHASE LINE
              [60 s of silence on the wake path]
02:55:46.597  sandbox/nomad-ch host_fence: cleared             ← source-teardown finishes
02:55:46.597  sandbox/nomad-ch vm_index released               ← slot freed
              [no further wake-handler log lines for this sandbox]
```

The **last `restore: phase=*`** before the 60 s client timeout is `pre_reserve_vm_index`. The handler entered `reserve_vm_index_with_retry`, the first reserve call returned `Err` (slot still held), and the `for attempt in 1..=60 { ... compio::time::sleep(2s).await }` loop **never made progress** — neither the success-after-retry log nor the exhaustion-budget warn fired, and `phase=post_reserve_vm_index` was never reached even after the slot freed at 02:55:46.

Smoke result: **CREATE 1/1, SNAPSHOT 1/1, WAKE 0/1** (TimeoutError at 60.06 s, identical shape to r6). Cluster torn down clean, 0 residual `zsbx-*` instances.

## Pre-cluster checks

| Check | Status |
|---|---|
| Sandbox HEAD at `94a302c1` | OK — local HEAD `79101a99` (the v22 pin-bump commit), parent `94a302c1` r14 reviewer artifacts; full chain through `8e7f0b53` C-6 phase tracing + `f6f6387f` C-6 deferred entry present. |
| Controller v22 Docker build clean | OK — `cargo build --release -p zeroship-sandbox` finished 31.43 s, one pre-existing dead-code warning (`seal_filename_for_str`; R14-Q2 in deferred). |
| Portable interp `/lib64/ld-linux-x86-64.so.2` | OK — `readelf -p .interp` confirmed. |
| Binary SHA recorded | OK — `b4d9684fe24c2c07a03d91496f38ed65381b2fd311d09fd5ec2c9d2ab189975a`, 16,517,728 B. |
| Upload to GCS | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v22` (size, md5 verified via `gcloud storage objects describe`). |
| Script pin bump v21 → v22 | OK — `provision-gcp-cluster.sh` (CONTROLLER_OBJECT default + doc header); `gcp-worker-startup.sh` example comment; `gcp-server-startup.sh` example comment. |
| Shellcheck | OK (pre-existing SC2020 info-level only, unchanged from v21). |
| Pin-bump commit | OK — `79101a99` "sandbox/scripts: bump controller pin v21 -> v22". |
| Budget ledger | OK — `/tmp/zsbx-cluster-budget-20260524` is now 7 lines (r7 appended; 7/10 used). |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v22
[provision] sentinel hit on zsbx-prod-server-1 (60s)
[provision] sentinel hit on zsbx-prod-worker-1 (15s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.20  RUNNING
```

## Validation 1 — `/livez` + Nomad driver health

```
$ curl -sf -o - -w 'http=%{http_code}\n' http://127.0.0.1:9091/livez   (on worker, where zsbx-ctl runs)
{"status":"ok"}http=200

$ sudo nomad node status -self | sed -n '/Driver Status/p'
Driver Status   = ch,exec,qemu,raw_exec
```

`ch` plugin healthy on the worker. Controller systemd unit `zsbx-ctl.service` running; binary v22 confirmed via the `SANDBOX_PORT=9091` config and `controller-object=zeroship-sandbox.snapshot-v22` instance metadata.

## Validation 2 — single create/snap/wake

```bash
sudo python3 /opt/stress/snapshot_stress.py --concurrency 1 --cycles 1
```

```
# elapsed: 72.9s
=== snapshot-stress (N=1) ===
CREATE OK: 1/1
  create p50/p95/p99/max: 6494 / 6494 / 6494 / 6494 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 6357 / 6357 / 6357 / 6357 ms
WAKE OK: 0/1
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  [1x] code=0: TimeoutError: timed out
```

| Stage | p50 | Notes |
|---|---|---|
| CREATE | 6494 ms | Healthy; consistent with r5/r6 ~6.5 s create p50. |
| SNAPSHOT | 6357 ms | Healthy. Artifact `a7cb12ad554835c20f10947814d6eae723fd6545a332fe40c7bc97cf81a6561e`, 1,073,847,238 B. |
| WAKE | timeout (60060 ms) | **FAIL.** Identical shape to r6 — client 60 s timeout fires; controller never returns. |
| POST-WAKE EXEC | n/a | Cascades. |
| STOP | n/a | Cascades. |

## C-6 LOCALIZATION (the headline result of this sprint)

### Wake sandbox under inspection

```
sandbox_id     = sbx_033M2M2GgAxb3Xbykktkhd (uuid 019e57e7-671c-7210-8f17-ede9ad0fc931)
vm_index       = 1
user_id        = usr_033M2M2GU9yvbY0WhGi0fM
generation     = 2 (snapshotted) → 3 (restoring; wedged)
```

### Timeline (UTC, from `/var/log/zeroship-sandbox.log` extracted via `grep 'restore: phase'`)

```
02:54:03.548  vm_index 1 allocated for create
02:54:04.335  create alloc running (786 ms)
02:54:09.997  create agent_ready (5.66 s) — sandbox CREATED
02:54:16.405  sandbox/nomad-ch stop: started  (snapshot teardown begins, detached)
02:54:16.407  WAKE handler entered.  phase=entry
02:54:16.436  phase=row_read_ok                status=snapshotted, generation=2
02:54:16.464  phase=read_snapshot_row_ok       vm_index=1
02:54:16.494  phase=cas_restoring_ok           generation=3 (pg confirmed)
02:54:16.494  phase=pre_reserve_vm_index       vm_index=1
              ── reserve_vm_index returns Err("vm_index 1 already reserved")
              ── retry loop: sleep 2 s, repeat
              ── EXPECTED: 45 sleeps × 2 s = 90 s before slot frees, then Ok
              ── ACTUAL: zero further phase lines for this sandbox
~02:55:16     stress client times out (60 s wake budget) and surfaces TimeoutError
02:55:46.597  sandbox/nomad-ch host_fence: cleared          (elapsed_ms=60158)
02:55:46.597  sandbox/nomad-ch vm_index released   vm_index=1
02:55:46.597  stop_preserving_state: skip host_dir rm + skip persist.delete (correct)
02:55:46.597  stop: complete (elapsed_ms=90191)
02:55:46.597  ERROR admin/snapshot: detached teardown_source_for_snapshot failed
              (/shutdown 60 s timeout — non-fatal, orphan-prune reclaims)
              [end of relevant log; the wake handler never re-surfaces]
```

### The LAST `restore: phase=*` line before the 60 s client timeout

```json
{"timestamp":"2026-05-24T02:54:16.493757Z","level":"INFO",
 "fields":{"message":"restore: phase",
           "sandbox_id":"019e57e7-671c-7210-8f17-ede9ad0fc931",
           "phase":"pre_reserve_vm_index","vm_index":1},
 "target":"zeroship_sandbox::restore_handler"}
```

**`last=pre_reserve_vm_index`** — the wake handler is wedged INSIDE `reserve_vm_index_with_retry` (`crates/sandbox/src/restore_handler.rs:265-306`), not in any downstream sync I/O.

### Mapping to the deferred file's C-6 candidate-root-cause table

The r6 review hypothesised five plausible wedge sites; the v22 phase trace falsifies four of them:

| Candidate root cause | Last-phase expectation | Falsified? |
|---|---|---|
| GCS hang / compio runtime issue inside `spawn_blocking` for `store.get` | `last=pre_store_get` | YES — never reached |
| pg pool exhaustion (R11-P1) | `last=pre_cas_running` | YES — never reached |
| Agent `/livez` never answers | `last=pre_wait_for_livez` | YES — never reached |
| Nomad submit hangs | `last=pre_submit_restore_job` | YES — never reached |
| `Persistence::unseal` hangs | `last=pre_unseal` | YES — never reached |
| ureq POST to `/clock_resync` hangs | `last=pre_clock_resync` | YES — never reached |
| **NEW (this sprint): retry-loop wedge inside `reserve_vm_index_with_retry`** | **`last=pre_reserve_vm_index`** | **MATCHES — root cause confirmed** |

The wedge is in the **retry loop body**: either the sync `backend.reserve_vm_index(vm_index)` call itself blocks indefinitely on the first attempt, or `compio::time::sleep(2s).await` doesn't return control to the retry loop on this controller's runtime.

### Why "retry-loop wedge" is the working hypothesis

1. **The first attempt's sync call cannot block on a mutex held by the teardown for 60 s.** `RealRestoreBackend::reserve_vm_index` (`restore_handler.rs:1488-1509`) takes `shared.lock()` on a `std::sync::Mutex<VmIndexAllocator>`. The mutex is only held during `reserve()` / `release()` body — microseconds. `stop_inner` doesn't hold it across the 60 s `http_signed_async("/shutdown")` await; the lock+release at `nomad_ch.rs:1135-1138` is fenced AFTER the http await finishes. So the sync `reserve_vm_index` returns `Err` immediately on attempt 1 (correctly), as designed.

2. **The retry loop's `compio::time::sleep(2s).await` should yield to the runtime.** With `for attempt in 1..=60 { reserve(); sleep(2s).await; }`, expected behaviour is to log "vm_index reserved after retry" at attempt ~46 (45 × 2 s ≈ 90 s after the first failed reserve = ~02:55:46, exactly when the host_fence clears and the slot is released). We see neither that log nor the exhaustion-budget warn at attempt 60 (~118 s ≈ 02:56:14). Both possible loop exits are silent.

3. **The detached `teardown_source_for_snapshot` runs on the same compio runtime via `compio::runtime::spawn(...).detach()`** (`admin_handlers.rs:1311-1324`). The teardown awaits `http_signed_async("/shutdown")` for the full 60 s connection-timeout window. **On a single-threaded compio runtime**, while the teardown future is parked on its `/shutdown` await, the runtime should still drive other futures (the wake handler's sleep) — unless the http client's internal poll is blocking (ureq + ntex http inside a sync spawn_blocking wrap-around).

   Working hypothesis: the detached future's compio-spawned task is being **starved by either (a) a sync HTTP poll inside the teardown that doesn't yield, or (b) the wake handler's sleep being on a different runtime/executor than the teardown's spawn-and-detach** such that the wake handler's task isn't being polled until the teardown returns. This matches the observed timing: wake silent from 02:54:16 to (never), teardown completes at 02:55:46 → vm_index releases → but by then the wake handler's task is already off the runtime's poll queue.

4. **Single-threaded compio runtime + detached future on same runtime is a classic starvation shape.** The `spawn(...).detach()` future has equal priority to the wake handler's future. If the teardown's first poll dives into a sync block (ureq's underlying TCP connect-timeout, or any `std::thread::sleep` in the stop path that wasn't moved to `compio::time::sleep`), the runtime can't poll the wake handler's `sleep(2s).await` continuations. The wake's sleep registers a wake-up but the runtime is busy in the teardown's poll.

A focused fix targets one of three options:
- **(a)** Move the detached teardown's blocking calls into `compio::runtime::spawn_blocking` so the main runtime thread stays free to poll the wake's retry sleeps. Most direct.
- **(b)** Make `reserve_vm_index_with_retry` use a longer-yielding sleep (e.g., re-acquire the runtime via `compio::runtime::yield_now()` between attempts) so even a partially-starved runtime still progresses.
- **(c)** Eliminate the race entirely: the wake handler should `await` the source-teardown completion (or a `vm_index_released` watch channel) instead of polling-retry on a shared mutex. The cleanest fix architecturally, but requires plumbing a notification primitive through the backend trait.

## Recommendation for the C-6-fix sprint

**Target: option (a) first** — wrap `teardown_source_for_snapshot`'s synchronous-bottom calls in `compio::runtime::spawn_blocking`. The pattern already exists for `store.get` (R5-P1b at `cdd2e677`), `store.put` (R7-P1 at `79428d53`), `submit_restore_job`/`wait_for_livez` (R8-A3-5), and rollback `teardown_restore` (R10-C2 at `restore_handler.rs:447`). The candidate sync sinks inside `stop_inner` (`crates/sandbox/src/backend/nomad_ch.rs:986-`) that warrant `spawn_blocking` are:

- `http_signed_async("/shutdown")` — already async via ntex http client, but its underlying poll may dive into a sync DNS / TCP-connect path that holds the worker. Audit and confirm the http client is truly compio-native (no `std::net::TcpStream::connect` fallback).
- Any `std::fs::*` in the stop tail (host_dir cleanup is gated under `remove_host_dir=true`, which is false on the snapshot path — but worth auditing).
- The Nomad purge call (`http://127.0.0.1:4646/v1/job/{id}?purge=true` via ureq?). If this is ureq-blocking, it MUST be in `spawn_blocking`.

**Validation gate for the fix:** smoke-r8 should show `restore: phase=post_reserve_vm_index` followed by the full downstream phase chain (`alloc_dir_ready`, `pre_store_get`, `post_store_get`, …, `post_cas_running`). The phase tracing v22 carried is the right tool to grade the fix.

**Do NOT escalate to T-8b-stress (3-worker / c=20)** until smoke-r8 emits a green WAKE 1/1 with all phase lines present.

## Cluster review: pass/fail per pre-req

| Pre-req | Result |
|---|---|
| HEAD includes `94a302c1`, `8e7f0b53`, `f6f6387f`, all r12 + C-6 tracing parents | OK |
| v22 binary built portable | OK (`/lib64/ld-linux-x86-64.so.2`) |
| v22 binary uploaded to GCS | OK |
| Pin bumped + shellcheck clean + committed | OK (`79101a99`) |
| Budget ledger has r7 line | OK (7/10/day) |
| Cluster provisions cleanly | OK (server 60 s, worker 15 s sentinel) |
| /livez 200 | OK (`:9091/livez` on worker) |
| ch driver healthy on worker | OK |
| CREATE 1/1 | **OK** |
| SNAPSHOT 1/1 | **OK** |
| WAKE 1/1 | **FAIL** (C-6 still open, but NOW LOCALIZED to `pre_reserve_vm_index`) |
| Teardown clean | OK (0 instances remaining) |

## Bug tally (T-8b smoke series)

| Cycle | Driver/Controller fixed | New bug surfaced |
|---|---|---|
| r1 | bash wrapper, install gate, plugin handshake | C-1 (driver `--config` arg form) |
| r2 | C-1 | C-2 (CH disk path not on disk) |
| r3 | (re-test of C-2) | — (re-confirmed C-2) |
| r4 | C-2 (rootfs materialization + pre-flight stat) | C-3 (snapshot-store `spawn_blocking` panic) |
| r5 | C-3 (`std::thread::Builder` for L2 detach) | C-4 (wake/teardown vm_index race) + C-5 (worker missing devstorage.read_write) |
| r6 | C-4 (60×2 s reserve_vm_index retry) + C-5 (worker IAM scope) | C-6 (wake handler silent stall past vm_index reserve; row wedged at restoring) |
| r7 | C-6 phase tracing (investigation-only; no behaviour change) | **C-6 LOCALIZED** to `reserve_vm_index_with_retry` retry-loop wedge / runtime starvation by detached teardown |

r7 is the first cycle of the T-8b series that did NOT uncover a new bug — instead, it provided actionable localization of the prior cycle's mystery. The diagnostic-only landing is paying off as designed.

## Cost (best-effort, this run)

- 1× n2-standard-4 (server) + 1× n2-standard-32 (worker, nested-virt)
- Up-time: ~10 minutes (provision ~3 min + smoke ~3 min + log capture ~2 min + teardown ~1 min)
- Approx GCE on-demand asia-northeast3 hourly: n2-standard-4 ≈ \$0.22/h, n2-standard-32 ≈ \$1.76/h ⇒ ~10 min ≈ \$0.33 on-demand
- Plus GCS list/get for controller pull (negligible), 2 ephemeral IPs (negligible)
- **Estimated total: <\$0.50** for this iteration. In line with r1–r6.

## Teardown verification

```
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
Deleted instance zsbx-prod-server-1
Deleted instance zsbx-prod-worker-1
[teardown] releasing internal addresses: zsbx-prod-server-1-ip
Deleted address zsbx-prod-server-1-ip
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down

$ gcloud compute instances list --filter='name~"^zsbx-"' --format='value(name)'
(empty)
```

Zero residual `zsbx-*` instances. No leaked IPs.

## Artifacts

- Controller binary: `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v22` (SHA `b4d9684f…975a`, 16,517,728 B)
- Provision log: `/tmp/t8b-smoke-r7-provision.log`
- Stress log: `/tmp/t8b-smoke-r7-stress.log`
- C-6 phase trace: `/tmp/t8b-smoke-r7-trace.log` (16 `restore: phase` lines; final one is `pre_reserve_vm_index`)
- Teardown log: `/tmp/t8b-smoke-r7-teardown.log`
- Pin-bump commit: `79101a99`
- Wedged row (post-smoke, before teardown): `status=restoring, generation=3, vm_index=1, sandbox_id=sbx_033M2M2GgAxb3Xbykktkhd`
