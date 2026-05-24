# T-8b-smoke-retry-r8 cluster validation — 2026-05-25 r8 (controller v23 / C-6 fix landed, 1+1 fleet)

**Sprint:** T-8b-ctl-v23 + smoke-retry-r8 — re-run smoke against controller v23 (sandbox HEAD `9c9bf3fc`, contains the C-6 fix at `91ce9be5` detaching `teardown_source_for_snapshot` onto a dedicated OS thread).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `6c6a6d3a` (v22 → v23 pin bump; parent `9c9bf3fc` carries the R14-Q3/R14-P2 closures and the full C-6 fix chain `91ce9be5` + `8e7f0b53` phase tracing + `f6f6387f` C-6 deferred entry).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v23`, SHA `2fa4116507c32f02b33586d04b316e7171caddafd931896d2129968e892f01b6`, size 16,500,848 B, interp `/lib64/ld-linux-x86-64.so.2`.
**Driver:** unchanged — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v4`.
**Verdict:** **FAIL on WAKE — phase trace IDENTICAL to r7.** The detached-teardown-on-dedicated-OS-thread fix (C-6 Option C, `91ce9be5`) did **NOT** unblock the wake path. Last phase emitted is still `pre_reserve_vm_index`; no retry-success log; no retry-exhausted log; no `post_reserve_vm_index`. The C-6 root-cause hypothesis ("detached teardown on the same ntex-worker runtime starves the wake handler's retry sleeps") is **falsified**.
**Recommendation:** **NO-GO for T-8b-stress.** This is **NEW bug C-7** (not "C-6 fix incomplete" — the C-6 fix did exactly what it promised; the hypothesis was wrong). The most likely root cause is now **ntex cancels the wake handler future on client disconnect at the 60 s stress-client timeout, before the retry loop's ~90 s budget completes**. Recommend a focused C-7-fix sprint to either (a) lift the stress-client timeout to ≥150 s, (b) decouple the wake handler from its HTTP request lifetime via a detached background driver, or (c) spawn `restore_sandbox` on its own OS-thread runtime (mirroring the C-3 / C-6 pattern but for the wake side).

## TL;DR

The wake-path trace is byte-identical in shape to r7. Both show:

```
03:19:35.135  phase=entry
03:19:35.153  phase=row_read_ok           status=snapshotted, generation=2
03:19:35.170  phase=read_snapshot_row_ok  vm_index=1
03:19:35.189  phase=cas_restoring_ok      generation=3
03:19:35.189  phase=pre_reserve_vm_index  vm_index=1        ← LAST PHASE LINE (same as r7)
              [60 s of silence on the wake path]
03:21:05.326  sandbox/nomad-ch host_fence: cleared          ← detached source teardown finishes
03:21:05.326  sandbox/nomad-ch vm_index released
              [no further wake-handler log lines for this sandbox]
```

Smoke result: **CREATE 1/1, SNAPSHOT 1/1, WAKE 0/1** (`TimeoutError` at 60.06 s, identical wire shape to r6/r7). Cluster torn down clean, 0 residual `zsbx-*` instances.

Sprint Step 6 calls said "Same phase as r7 = C-6 fix incomplete." Strictly true (the wedge site didn't move), but the more important finding is that the C-6 **hypothesis** was wrong — the fix landed correctly (the detached teardown now runs on `std::thread::Builder::new().spawn(...)` + its own `compio::runtime::Runtime::new().block_on(...)`, decoupled from any ntex-worker runtime), yet the wedge persisted exactly. Therefore the wedge is not runtime-starvation by the detached teardown; it is the **wake handler future being dropped at client disconnect**.

## Pre-cluster checks

| Check | Status |
|---|---|
| Sandbox HEAD at `9c9bf3fc` (post-r14 closures) | OK — local HEAD `6c6a6d3a` (the v23 pin-bump commit); parent `9c9bf3fc` includes `91ce9be5` C-6 fix, `8e7f0b53` phase tracing, `f6f6387f` C-6 deferred entry, plus R14-Q3/R14-P2 closures. |
| Controller v23 Docker build clean | OK — `cargo build --release -p zeroship-sandbox` finished 30.94 s in `rust:slim-bookworm`. One pre-existing dead-code warning on `seal_filename_for_str` (R14-Q2 in deferred). |
| Portable interp `/lib64/ld-linux-x86-64.so.2` | OK — `readelf -p .interp` confirmed. |
| Binary SHA recorded | OK — `2fa4116507c32f02b33586d04b316e7171caddafd931896d2129968e892f01b6`, 16,500,848 B. |
| Upload to GCS | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v23` (size + md5 verified via `gcloud storage objects describe`; local md5 base64 matched). |
| Script pin bump v22 → v23 | OK — `provision-gcp-cluster.sh` (CONTROLLER_OBJECT default + doc header); `gcp-server-startup.sh` example comment; `gcp-worker-startup.sh` example comment. |
| Shellcheck | OK (pre-existing SC2020 info-level only, unchanged from v22). |
| Pin-bump commit | OK — `6c6a6d3a` "sandbox/scripts: bump controller pin v22 -> v23 (T-8b-ctl-v23-upload)". |
| Budget ledger | OK — `/tmp/zsbx-cluster-budget-20260524` is now 8 lines (r8 appended; 8/10 used for the UTC day). |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v23
[provision] sentinel hit on zsbx-prod-server-1 (60s)
[provision] sentinel hit on zsbx-prod-worker-1 (15s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.20  RUNNING
```

Sentinel timings unchanged from r5-r7 (server 60 s, worker 15 s). v23 binary fetches cleanly from GCS on both nodes.

## Validation 1 — `/livez` + Nomad driver health

```
$ curl -sf -o - -w 'http=%{http_code}\n' http://127.0.0.1:9091/livez   (on worker)
{"status":"ok"}http=200

$ sudo nomad node status -self | sed -n '/Driver Status/p'
Driver Status   = ch,exec,qemu,raw_exec
```

`ch` plugin healthy on the worker. Controller systemd unit `zsbx-ctl.service` running.

## Validation 2 — single create/snap/wake

```bash
sudo python3 /opt/stress/snapshot_stress.py --concurrency 1 --cycles 1
```

```
# elapsed: 72.9s
=== snapshot-stress (N=1) ===
CREATE OK: 1/1
  create p50/p95/p99/max: 6474 / 6474 / 6474 / 6474 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 6303 / 6303 / 6303 / 6303 ms
WAKE OK: 0/1
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  [1x] code=0: TimeoutError: timed out
```

| Stage | p50 | Notes |
|---|---|---|
| CREATE | 6474 ms | Healthy; matches r5/r6/r7 ~6.5 s create p50. |
| SNAPSHOT | 6303 ms | Healthy. Artifact `f5cad5c99c426d70cf6a4888147b7470e3a7a068e5c4b220fa08096925759c9f`, 1,073,846,963 B. |
| WAKE | timeout (60060 ms) | **FAIL.** Same wire shape as r6/r7. |
| POST-WAKE EXEC | n/a | Cascades. |
| STOP | n/a | Cascades. |

## C-7 ROOT-CAUSE HYPOTHESIS (the headline finding of this sprint)

### Wake sandbox under inspection

```
sandbox_id     = sbx_033M2yYDfJUQwVMUR5rQCI (uuid 019e57fe-93ee-7b93-8836-126a99dba72a)
vm_index       = 1
user_id        = usr_033M2yYDTAFVsFm30qVFOm
generation     = 2 (snapshotted) → 3 (restoring; wedged)
```

### Timeline (UTC, extracted via `grep 'restore: phase'` against `/var/log/zeroship-sandbox.log`)

```
03:19:22.350  sandbox/nomad-ch create — vm_index 1 allocated
03:19:23.135  create alloc running    (elapsed 784 ms)
03:19:28.782  create agent_ready      (elapsed 5646 ms) — CREATED
03:19:35.133  sandbox/nomad-ch stop: started (snapshot teardown begins; now detached on dedicated OS thread per 91ce9be5)
03:19:35.135  WAKE handler entered.   phase=entry
03:19:35.153  phase=row_read_ok           status=snapshotted, generation=2
03:19:35.170  phase=read_snapshot_row_ok  vm_index=1
03:19:35.189  phase=cas_restoring_ok      generation=3 (pg confirmed)
03:19:35.189  phase=pre_reserve_vm_index  vm_index=1
              ── reserve_vm_index returns Err("vm_index 1 already reserved")
              ── retry loop: sleep 2 s, repeat
              ── EXPECTED: attempt ~46 (90 s in) succeeds when slot frees at 03:21:05
~03:20:35     stress client 60 s timeout fires → ntex closes the connection
              → wake handler future is DROPPED mid-retry-loop (attempt ~30)
03:21:05.326  sandbox/nomad-ch host_fence: cleared          (detached teardown finishes; elapsed_ms=60158)
03:21:05.326  sandbox/nomad-ch vm_index released  vm_index=1
03:21:05.326  stop_preserving_state: skip host_dir rm + skip persist.delete (correct)
03:21:05.326  stop: complete (elapsed_ms=90192)
03:21:05.326  ERROR admin/snapshot: detached teardown_source_for_snapshot failed
              (/shutdown 60 s timeout — non-fatal, orphan-prune reclaims)
              [end of relevant log; the wake handler future was already dropped 30 s earlier]
```

### The LAST `restore: phase=*` line before the 60 s client timeout

```json
{"timestamp":"2026-05-24T03:19:35.189371Z","level":"INFO",
 "fields":{"message":"restore: phase",
           "sandbox_id":"019e57fe-93ee-7b93-8836-126a99dba72a",
           "phase":"pre_reserve_vm_index","vm_index":1},
 "target":"zeroship_sandbox::restore_handler"}
```

**`last=pre_reserve_vm_index`** — same as r7. Identical wedge site.

### Why C-6 (Option C) didn't fix it

C-6's Option-C fix moved the detached teardown's compio runtime to a dedicated OS thread (`std::thread::Builder::new().spawn(... compio::runtime::Runtime::new().block_on(...) ...)`, mirroring C-3's `snapshot_store_gcs.rs::Tiered::put` pattern). This is correctly landed at `91ce9be5` and binary-verified in v23.

If the wedge were runtime-starvation by the detached teardown competing with the wake handler's `compio::time::sleep(2s)` continuations on the same ntex-worker compio runtime, the v23 fix would have unblocked the retry loop — the wake handler would have logged `restore/wake: vm_index reserved after retry` at attempt ~46 (~90 s in) and then continued to `phase=post_reserve_vm_index` and downstream phases.

It did neither. The wedge stayed bit-identical: same last phase, same 60 s timeout, same ~90 s teardown completion, same wake-handler silence after the 60 s mark.

This **falsifies** the C-6 runtime-starvation hypothesis. The wedge is not caused by the detached teardown's residency on the worker runtime.

### Why "wake handler future is dropped by ntex on client disconnect" is the new working hypothesis

1. **The retry loop is unconditional on `attempts ≤ 60`**, with `compio::time::sleep(retry.interval=2 s).await` between attempts (`crates/sandbox/src/restore_handler.rs:273-294`). The total budget is ~120 s if every reserve fails. The slot freed at +90 s, so attempt ~46 would have succeeded. We see neither the success-after-retry log nor the exhaustion-budget warn — both possible loop exits are silent.

2. **The wake handler is awaited inline on the ntex worker** (`admin_handlers.rs:1439-1447`):
   ```rust
   let outcome = restore_handler::restore_sandbox(...).await;
   ```
   When ntex's per-connection task receives a client TCP close (the stress client closes at its 60 s timeout), ntex drops the handler future per ntex's request-cancellation semantics. The retry loop's `compio::time::sleep` is in a pollable state at that moment — it gets cancelled cleanly, the future is dropped, and NO logging fires (neither success nor exhaustion — those are after-the-loop branches).

3. **The detached-teardown 60 s `/shutdown` collision is a red herring.** Both teardown duration (60 s connection-timeout) and stress-client wake timeout (60 s) are coincidentally equal at 60 s, which made the C-6 hypothesis plausible (teardown blocks for 60 s → wake retries for 60 s without progress → starvation). v23 isolated the teardown to its own OS thread, eliminating any runtime contention, yet the wake retry STILL produces zero progress logs in 60 s. The teardown side is fine; the wake side is being aborted at exactly 60 s by ntex's connection-drop handling.

4. **One more falsification**: in r7's review, the working hypothesis was that `reserve_vm_index_with_retry`'s `compio::time::sleep(2s).await` doesn't yield to the runtime under contention with the detached teardown. v23 removes that contention entirely. The retry loop's sleep continuations now have an idle runtime to run on for the full 60 s window. They still don't fire — because the entire future is no longer being polled (it was dropped at client disconnect).

### Mapping to the deferred file's C-6 candidate-root-cause table — updated

| Candidate root cause | Last-phase expectation | r7 result | r8 result |
|---|---|---|---|
| GCS hang / compio runtime inside `spawn_blocking` for `store.get` | `pre_store_get` | falsified | falsified |
| pg pool exhaustion (R11-P1) | `pre_cas_running` | falsified | falsified |
| Agent `/livez` never answers | `pre_wait_for_livez` | falsified | falsified |
| Nomad submit hangs | `pre_submit_restore_job` | falsified | falsified |
| `Persistence::unseal` hangs | `pre_unseal` | falsified | falsified |
| ureq POST to `/clock_resync` hangs | `pre_clock_resync` | falsified | falsified |
| Retry-loop wedge inside `reserve_vm_index_with_retry` (C-6 starvation hypothesis) | `pre_reserve_vm_index` | matched | **NOT runtime starvation — v23 isolated the teardown but wedge persists identically** |
| **NEW (this sprint, r8): wake handler future dropped by ntex on client disconnect** | `pre_reserve_vm_index` (frozen because future no longer polled after 60 s) | n/a | **MATCHES — the C-7 working hypothesis** |

### How to verify the C-7 hypothesis next sprint

Three independent verification gates, any one of which would settle it:

1. **Cheap signal — bump the stress client timeout to 150 s.** If WAKE succeeds (or at least progresses past `pre_reserve_vm_index`), C-7 is confirmed and the only question is which fix shape to land. The stress script is at `/opt/stress/snapshot_stress.py` on the worker; the timeout is hardcoded around the `requests.post(...)` call.

2. **Cleaner signal — add per-attempt tracing inside `reserve_vm_index_with_retry`.** A one-line `tracing::debug!` (or info) at the top of the `for attempt in 1..=attempts { ... }` loop body. If we see attempts 1..N in the log and they stop firing at N ≈ 30 (60 s / 2 s) without the success or exhaustion log, the future was dropped — confirms C-7. If we see 1..N where N stops at some other value, it's a different bug.

3. **Definitive signal — replace the inline `.await` with a detached background driver.** Spawn `restore_sandbox` via `compio::runtime::spawn(...).detach()` immediately, return a 202 with a status URL, and let the client poll. This entirely decouples the work from the request lifetime and matches the typical "long restore" UX. The slot-vacate race is still there but it now has its full 120 s budget regardless of client behaviour.

## Recommendation for the C-7-fix sprint

**Land #2 first as a diagnostic-only change** (cheap, behaviour-preserving, gives definitive evidence on next smoke), then choose between #1 (operationally trivial but doesn't fix the underlying coupling) and #3 (the architecturally clean fix; matches how createSandbox handles the 5-6 s create budget vs. a long-running restore).

**Validation gate for the next fix:** smoke-r9 should show either:
- `restore/wake: vm_index reserved after retry` at attempt ~46 (~90 s in) followed by the full downstream phase chain (`post_reserve_vm_index`, `alloc_dir_ready`, `pre_store_get`, …), OR
- if #3 is taken, the wake response is 202 within ms and subsequent polling reaches `phase=post_cas_running` within ~25 s of slot release.

**Do NOT escalate to T-8b-stress (3-worker / c=20)** until smoke-r9 emits a green WAKE 1/1 with all phase lines present.

## Cluster review: pass/fail per pre-req

| Pre-req | Result |
|---|---|
| HEAD includes `91ce9be5` (C-6 fix), `8e7f0b53` (phase tracing), all r12-r14 closure parents | OK |
| v23 binary built portable | OK (`/lib64/ld-linux-x86-64.so.2`) |
| v23 binary uploaded to GCS | OK |
| Pin bumped + shellcheck clean + committed | OK (`6c6a6d3a`) |
| Budget ledger has r8 line | OK (8/10/day) |
| Cluster provisions cleanly | OK (server 60 s, worker 15 s sentinel) |
| /livez 200 | OK (`:9091/livez` on worker) |
| ch driver healthy on worker | OK |
| CREATE 1/1 | **OK** |
| SNAPSHOT 1/1 | **OK** |
| WAKE 1/1 | **FAIL** (C-7 NEW — see hypothesis above; not "C-6 fix incomplete" — C-6 fix landed correctly but the hypothesis was wrong) |
| Teardown clean | OK (0 instances remaining) |

## Bug tally (T-8b smoke series — updated)

| Cycle | Driver/Controller fixed | New bug surfaced |
|---|---|---|
| r1 | bash wrapper, install gate, plugin handshake | C-1 (driver `--config` arg form) |
| r2 | C-1 | C-2 (CH disk path not on disk) |
| r3 | (re-test of C-2) | — (re-confirmed C-2) |
| r4 | C-2 (rootfs materialization + pre-flight stat) | C-3 (snapshot-store `spawn_blocking` panic) |
| r5 | C-3 (`std::thread::Builder` for L2 detach) | C-4 (wake/teardown vm_index race) + C-5 (worker missing devstorage.read_write) |
| r6 | C-4 (60×2 s reserve_vm_index retry) + C-5 (worker IAM scope) | C-6 (wake handler silent stall past vm_index reserve; row wedged at restoring) |
| r7 | C-6 phase tracing (investigation-only; no behaviour change) | C-6 LOCALIZED to `reserve_vm_index_with_retry` retry-loop wedge / runtime starvation hypothesis |
| r8 | C-6 Option-C fix (detach teardown onto dedicated OS thread, `91ce9be5`) | **C-7** — C-6 hypothesis falsified; new hypothesis is **ntex cancels wake handler future on client disconnect at 60 s**, before the ~90 s retry budget completes |

r8 did NOT produce a SUCCESS milestone as hoped. It did produce a falsification of the C-6 hypothesis and a sharper, testable new hypothesis (C-7) with three cheap verification paths. The diagnostic discipline established in r7 (last-phase localization) continues to pay off — without that we would still be guessing at "wake silently stalls."

## Cost (best-effort, this run)

- 1× n2-standard-4 (server) + 1× n2-standard-32 (worker, nested-virt)
- Up-time: ~9 minutes (provision ~2.5 min + smoke ~1.5 min + log capture ~3 min + teardown ~1 min)
- Approx GCE on-demand asia-northeast3 hourly: n2-standard-4 ≈ \$0.22/h, n2-standard-32 ≈ \$1.76/h ⇒ ~9 min ≈ \$0.30 on-demand
- Plus GCS list/get for v23 controller pull (~16.5 MB × 2 nodes = negligible), 2 ephemeral IPs (negligible)
- **Estimated total: <\$0.50** for this iteration. In line with r1–r7.

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

## GO/NO-GO for T-8b-stress

**NO-GO.** Single-cycle WAKE still 0/1. Running 3-worker × 20-cycle stress against a controller that fails the 1/1 baseline burns budget for no information. The next sprint must be a C-7-fix cycle (recommend the diagnostic-only per-attempt tracing first; one-line change in `restore_handler.rs::reserve_vm_index_with_retry` retry-loop body), then re-smoke at 1/1, then re-evaluate stress.

## Artifacts

- Controller binary: `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v23` (SHA `2fa41165…01b6`, 16,500,848 B)
- Build log: `/tmp/t8b-r8-build.log`
- Provision log: `/tmp/t8b-smoke-r8-provision.log`
- Stress log: `/tmp/t8b-smoke-r8-stress.log`
- Phase trace: `/tmp/t8b-smoke-r8-phase.log` (10 lines; final phase line is `pre_reserve_vm_index` — same as r7)
- Teardown log: `/tmp/t8b-smoke-r8-teardown.log`
- Pin-bump commit: `6c6a6d3a`
- Wedged row (post-smoke, before teardown): `status=restoring, generation=3, vm_index=1, sandbox_id=sbx_033M2yYDfJUQwVMUR5rQCI`
