# T-8b-smoke-r13 cluster validation — 2026-05-25 r13 (controller v28 / C-7-LT-1 landed, 1+1 fleet)

**Outcome:** **RED — WAKE 0/1.** C-7-LT-1 took effect exactly as designed (server-side retry budget widened 50 s → 70 s; 36 attempts × 2 s wall-time observed), but the wake still failed because **the source teardown LEAKED the vm_index**: `host_fence` reported `fence_passed=false` at +30 s with `probes=1, consecutive_misses=1`, meaning only ONE probe ran across the entire 30 s budget. The controller correctly leaks the slot when fence cannot prove the agent is silent — but the slot is then permanently unavailable for this generation. C-7-LT-1 is **operating correctly**; the residual is a separate `wait_for_agent_silent` probing-loop bug (C-7-LT-2). NO-GO for T-8b-stress until C-7-LT-2 is identified and fixed.

**Sprint:** T-8b-ctl-v28 + smoke-r13 — second end-to-end attempt with the C-7-LT async wake response contract + the surgical C-7-LT-1 budget fix.
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `5442a29c` (= `f9996fcf` (C-7-LT-1 HEAD) + `370d13e6` (docs r17-r18) + `5442a29c` (v27→v28 pin)).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v28`, SHA256 `fdcb1649fd2455110d34fb2bf4a4168e4fb7efb27028b1f595edc9cd1b727be1`, MD5 `61c900126313393cca3f5db57e7c28ab` (GCS round-trip verified), interp `/lib64/ld-linux-x86-64.so.2`, size 16,571,056 bytes.
**Driver:** unchanged — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v4`.

**Recommendation:** **NO-GO for T-8b-stress until C-7-LT-2 (host-fence probe-count-of-1 pathology) is rooted and fixed.** With WAKE 0/1 at concurrency=1 due to a slot LEAK (not exhaustion), stress at any concurrency would replay this LEAK every cycle plus cumulatively starve the index. The leak-not-exhaustion failure mode invalidates the entire premise of C-7-LT-1 (widen budget so it catches the teardown release), because there IS no release.

## TL;DR — C-7-LT-1 landed exactly; a different residual surfaced

```
07:18:21.255  sandbox/nomad-ch stop: started               vm_index=1  (source teardown begins)
07:18:21.314  wake_machine: drive started                   wake_id=wak_033M8qAlT6sgqqvwppq3By
07:18:21.393  reserve_vm_index_with_retry attempt=1/36      vm_index=1  budget=70s (was 26/26 budget=50s in r12)
07:18:23.393  attempt=2/36
…             (every 2 s, 36 attempts total)
07:19:21.400  attempt=31/36 — at this exact moment:
07:19:21.419  host_fence: deadline reached                  elapsed_ms=30129
              base_url=http://10.99.101.2:7777
              probes=1                ← ★ pathological: ONE probe in 30 s @ 100 ms cadence
              consecutive_misses=1    ← but never reached threshold=2
              last_status=None
              fence_passed=false
07:19:21.419  ERROR sandbox/nomad-ch host_fence: timeout
07:19:21.419  WARN sandbox/nomad-ch vm_index leak  reason=host_fence_timeout  vm_index=1
07:19:21.419  WARN sandbox/nomad-ch stop: host_fence timeout; leaking vm_index to avoid handing
              out a live IP (orphan-prune will reclaim on next boot)
07:19:21.419  sandbox/nomad-ch stop: complete                fence_passed=false  elapsed_ms=60163
07:19:21.419  ERROR admin/snapshot: detached teardown_source_for_snapshot failed (non-fatal;
              orphan-prune will reclaim)
              error="… Connect error: connection timed out … leaking vm_index …"
07:19:23.400  attempt=32/36 — wake continues retrying a LEAKED slot
07:19:25.400  attempt=33/36
07:19:27.400  attempt=34/36
07:19:29.400  attempt=35/36
07:19:31.401  attempt=36/36
07:19:31.401  WARN restore/wake: vm_index reserve exhausted retry budget
              attempts=36 budget_ms=70000 last_error="vm_index 1 already reserved"
07:19:31.422  WARN wake_machine: terminal failed
              error_code=slot_unavailable
              error_message="vm_index unavailable (cluster exhausted at vm_index=1)"
07:19:31.???  GET /wake/wak_… → 200 OK (poll #136, +70.235 s)
              body: §10.0 envelope
                {error: "vm_index_unavailable", state: "failed", wake_id, sandbox_id, updated_at}
```

**Smoke result:** **CREATE 1/1 (6520 ms), SNAPSHOT 1/1 (14517 ms), WAKE 0/1 (70235 ms — terminal failed)**, STOP failed downstream. Cluster torn down clean. Cycle 13 of today.

## Async wake confirmation — every C-7-LT + C-7-LT-1 invariant held

| Invariant | Confirmed in production? |
|---|---|
| `SANDBOX_WAKE_RESPONSE_MODE=async` env reached the controller process | YES — `systemctl show zsbx-ctl --property=Environment` → `SANDBOX_WAKE_RESPONSE_MODE=async`. |
| `POST /admin/sandboxes/{id}/wake` returns 202 in async mode | YES — first call returned 202 in 58.5 ms. |
| Response body has typed `wake_id` with `wak_` prefix | YES — `wake_id="wak_033M8qAlT6sgqqvwppq3By"` (22-char base62). |
| Response body has `poll_url` + `state: "pending"` | YES — `poll_url="/admin/sandboxes/sbx_033M8pdcIYQo2Rtwa1BF7y/wake/wak_033M8qAlT6sgqqvwppq3By"`, `state="pending"`. |
| `GET poll_url` returns 202 for intermediate states | YES — poll #2 at +0.598 s got `state="reserving_slot"`. |
| `GET poll_url` returns 200 with §10.0 envelope on terminal-failed | YES — poll #136 at +70.235 s got 200 with `{error: "vm_index_unavailable", state: "failed", …}`. |
| State machine progresses through documented states | YES — transitions: `pending → reserving_slot → failed`. |
| **C-7-LT-1: async-mode budget = 2×fence + HEADROOM = 70 s** | YES — controller logged `attempts=36 budget_ms=70000`. r12 had `attempts=26 budget_ms=50000`; the +10-attempt / +20-second widening is exactly the C-7-LT-1 commit. |
| Per-attempt INFO logs visible in wake-machine path | YES — 36 attempt lines, every 2 s, `target=zeroship_sandbox::restore_handler`. |

C-7-LT-1 shipped exactly as the diagnosis specified. The async wake contract + the widened retry budget both work. The failure has moved to a different module.

## Build / upload / pin

| Step | Status |
|---|---|
| Controller HEAD | `f9996fcf` (= C-7-LT-1 landed; chain `93862496` → `1cfc9182` → `678ec197` → `db248cbf` → `3c3d72b4` → `f9996fcf`). |
| Docker release build | OK — `cargo build --release -p zeroship-sandbox` in `rust:slim-bookworm` (rustc 1.95.0), 47.05 s. |
| Portable interp | OK — `readelf -p .interp` → `/lib64/ld-linux-x86-64.so.2`. |
| Binary SHA256 | `fdcb1649fd2455110d34fb2bf4a4168e4fb7efb27028b1f595edc9cd1b727be1` (16,571,056 bytes). |
| GCS upload | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v28`; gcloud MD5 `61c900126313393cca3f5db57e7c28ab` matches local `md5sum` (decoded `YckAEmMTOTzKP121fnwoqw==` base64 → hex). |
| Script pin v27 → v28 | OK — `provision-gcp-cluster.sh` (CONTROLLER_OBJECT default + doc header), `gcp-server-startup.sh` (example comment), `gcp-worker-startup.sh` (example comment). |
| Shellcheck | Clean — `lint.sh: OK — 7 script(s) clean at --severity=error`. |
| Pin-bump commit | `5442a29c` "sandbox/scripts: bump controller pin v27 -> v28 (T-8b-ctl-v28; C-7-LT-1 + GATE-C2)". |
| Budget ledger | `/tmp/zsbx-cluster-budget-20260524` now 15 lines (r13 provision-start + teardown-complete appended). 4 cycles into the next-window allowance. |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v28
[provision] extra worker metadata: install-ch-plugin-driver=1
[provision] sentinel hit on zsbx-prod-server-1 (60s)
[provision] sentinel hit on zsbx-prod-worker-1 (45s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.26  RUNNING
```

Sentinel timings: server 60 s (steady), worker 45 s (down from r12's 90 s — r12 had the root-KEK fail-CLOSED boot-loop hot-patched mid-provision; r13 has the durable provisioning fix from `f9a9c5f0` so no boot-loop). Clean first-time bring-up.

## Validation 1 — `/livez` + ch driver

```
$ curl http://127.0.0.1:9091/livez                             → {"status":"ok"}
$ curl http://localhost:4646/v1/nodes/<id>  | jq … Drivers      → ch: Healthy=True Detected=True
                                                                 exec: Healthy=True Detected=True
                                                                 qemu: Healthy=True Detected=True
                                                                 raw_exec: Healthy=True Detected=True
```

Note: Nomad reports the driver as `"ch"`, while the controller env labels it `SANDBOX_TASK_DRIVER=ch_plugin` — this is by-design (R12-I1: the `ch_plugin` env-flag toggles emission of `Driver: "ch"` in the wake-path jobspec). The driver IS the plugin process (`/etc/zeroship/nomad-plugins/nomad-driver-ch`, PID 13545), Healthy=True.

## Validation 2 — async wake env CONFIRMED in controller process

```
$ systemctl show zsbx-ctl --property=Environment | grep -E "WAKE|ROOT_KEK|FENCE|TASK_DRIVER"
SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30
SANDBOX_WAKE_RESPONSE_MODE=async
SANDBOX_SNAPSHOT_ROOT_KEK_PATH=/etc/zeroship/snapshot-root-kek
SANDBOX_TASK_DRIVER=ch_plugin
```

All four required env vars resolved exactly as the brief asked. `WakeResponseMode::Async` is what the controller boots with (per `f9a9c5f0` + `7664b4b0` already-landed scripts), so C-7-LT-1's mode-dependent branch (`WakeResponseMode::Async => 2×fence + HEADROOM`) takes effect.

## Validation 3 — smoke-r13 cycle (1 CREATE + 1 SNAPSHOT + 1 WAKE-async + 1 STOP)

```
# t8b-smoke-r13: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091  wake_budget=120s (client side; server side is 70s)
# elapsed: 91.3s

CREATE OK: 1/1
  create p50/p95/p99/max: 6520 / 6520 / 6520 / 6520 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 14517 / 14517 / 14517 / 14517 ms
WAKE OK (async polling): 0/1
  wake total (any) p50/p95/p99/max: 70235 / 70235 / 70235 / 70235 ms
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  post_code=202 terminal_state=failed polls=136 total_ms=70235
  terminal_body: {"error":"vm_index_unavailable","message":"vm_index unavailable (cluster
                  exhausted at vm_index=1)","state":"failed","wake_id":"wak_033M8qAlT6sgqqvwppq3By",
                  "sandbox_id":"sbx_033M8pdcIYQo2Rtwa1BF7y","updated_at":1779607171}
  transitions:
    +  0.059s  POST→202
    +  0.059s  body.state=pending  wake_id=wak_033M8qAlT6sgqqvwppq3By
    +  0.598s  poll#2  HTTP 202  state=reserving_slot
    + 70.235s  poll#136 HTTP 200  state=failed
```

**Client-side notes:** the `/opt/stress/snapshot_stress.py` shipped by the provision script is pre-C-7-LT (May 6) — it expects synchronous 200 from `POST /wake`. For r13 I wrote a polling-capable client (`/tmp/snapshot_stress_r13.py`) that handles the C-7-LT contract: POST → 202 + wake_id + poll_url → GET poll_url every 0.5 s until terminal `state ∈ {ok, failed}`. The shipped harness should be updated for stress; this isn't blocking r13.

**SNAPSHOT was 14.5 s** — within 0.2% of r12's 14.6 s. The ~1 GB encrypted blob L2 GCS push dominates; reproducible cluster constant.

**WAKE was 70.235 s** — the wall-time from `POST /wake` (returned 202 in 58.5 ms) to the terminal 200 OK from poll #136 (70.235 s total). The breakdown:
- 58.5 ms — POST → 202 → wake_id minted.
- 539 ms (poll #2, t=+0.598 s) — state advances `pending → reserving_slot`.
- 69.637 s (polls #3–#135, every 500 ms) — server-side retry loop runs (36 attempts × 2 s = 72 s, observed 69.6 s wall after subtracting POST/first-poll lag).
- 0 ms (poll #136, t=+70.235 s) — state advances `reserving_slot → failed`, terminal 200 OK.

The contract executed cleanly; **C-7-LT-1's `attempts=36 budget_ms=70000` matches the new policy exactly** (vs r12's `attempts=26 budget_ms=50000`).

## State-machine phase-by-phase trace (from controller logs)

| t (UTC, monotonic) | event |
|---|---|
| 07:17:55.??? | client POST `/sandboxes` (create) |
| 07:18:00.996 | sandbox/nomad-ch create alloc running, elapsed_ms=785 |
| 07:18:06.686 | sandbox/nomad-ch create agent_ready, elapsed_ms=5689 (6.520 s wall create) |
| 07:18:06.??? | client POST `/admin/sandboxes/.../snapshot` (wait=true) |
| 07:18:21.??? | snapshot returns 200; client POST `/admin/sandboxes/.../wake` |
| 07:18:21.255 | sandbox/nomad-ch `stop: started` vm_index=1 (source teardown begins from snapshot side) |
| 07:18:21.314 | `wake_machine: drive started` wake_id=wak_033M8qAlT6sgqqvwppq3By |
| 07:18:21.393 | `reserve_vm_index_with_retry attempt=1/36` (C-7-LT-1's widened budget visible) |
| 07:18:23.393 | attempt=2/36 |
| 07:18:25.393 | attempt=3/36 |
| … | (every 2 s) |
| 07:19:21.400 | attempt=31/36 |
| **07:19:21.419** | **`host_fence: deadline reached` elapsed_ms=30129 probes=1 consecutive_misses=1 last_status=None fence_passed=false** |
| 07:19:21.419 | `sandbox/nomad-ch host_fence: timeout` ERROR; `vm_index leak` reason=host_fence_timeout vm_index=1 |
| 07:19:21.419 | `sandbox/nomad-ch stop: complete` fence_passed=false elapsed_ms=60163 (vm_index NOT freed; leaked) |
| 07:19:21.419 | `admin/snapshot: detached teardown_source_for_snapshot failed (non-fatal; orphan-prune will reclaim)` |
| 07:19:23.400 | attempt=32/36 — retrying a leaked slot, doomed |
| 07:19:25.400 | attempt=33/36 |
| 07:19:27.400 | attempt=34/36 |
| 07:19:29.400 | attempt=35/36 |
| 07:19:31.401 | attempt=36/36 |
| 07:19:31.401 | WARN `restore/wake: vm_index reserve exhausted retry budget` attempts=36 budget_ms=70000 last_error="vm_index 1 already reserved" |
| 07:19:31.422 | WARN `wake_machine: terminal failed` error_code=slot_unavailable error_message="vm_index unavailable (cluster exhausted at vm_index=1)" |
| 07:19:31.??? | wake_jobs pg row → state=failed; client poll #136 reads it, render_wake_poll_response returns 200 + §10.0 envelope |

**Source teardown wall-time:** `stop: complete (07:19:21.419) − stop: started (07:18:21.255) = 60.164 s` — within 1 ms of r10/r11/r12's 60.16 s. The teardown wall-time is a sharp constant of the cluster. **Critically, `fence_passed=false` in r13 — the source agent never went silent across the 30 s fence window, so the controller LEAKED the vm_index rather than releasing it.** The 60-second wall is split: 30 s fence (failed) + 30 s Nomad purge tail.

**Wake retry budget:** `attempt 36 start (07:19:31.401) − attempt 1 start (07:18:21.393) = 70.008 s` — matches the C-7-LT-1 async-mode formula `2×30 + 10 = 70 s` exactly: 36 attempts × 2 s = 72 s wall (observed 70.0 s after subtracting the last-attempt-doesn't-sleep tail).

**Gap:** wake exhausted at +70 s. Source teardown "completed" at +60.2 s — but with `fence_passed=false` and `vm_index leak`. The slot was NEVER freed during this generation. C-7-LT-1's widened budget caught the teardown wall-time, but caught a teardown that *did not release the slot*.

## Diagnosis: C-7-LT-2 — `host_fence` probe loop fires ONE probe in 30 s

`probes=1, consecutive_misses=1, last_http_status=None, elapsed_ms=30129` from `host_fence: deadline reached`. The `wait_for_agent_silent` loop in `crates/sandbox/src/backend/nomad_ch.rs:3184` is designed to fire probes at a **100 ms cadence** for up to `timeout = 30 s`, so the expected probe count is ~300. Instead, only ONE probe ran across the entire 30-second budget.

Loop structure (verbatim from the source, line 3209):
```rust
loop {
    if Instant::now() >= deadline { break; }
    let outcome = compio::runtime::spawn_blocking(move || {
        ureq::get(&probe_url).timeout(Duration::from_millis(500)).call()
    }).await;
    probe_count += 1;
    // classify is_miss…
    if is_miss {
        consecutive_misses += 1;
        if consecutive_misses >= MISS_THRESHOLD { return Ok(()); }
    } else {
        consecutive_misses = 0;
    }
    compio::time::sleep(Duration::from_millis(100)).await;
}
```

With `probes=1` after 30 s, **the single `spawn_blocking(...).await` consumed ~30 s of wall-time** — either the ureq `timeout(500ms)` was not honored (e.g. blocking on DNS or TCP connect with no timeout-honoring layer beneath ureq's blocking-call API), or `compio::runtime::spawn_blocking` itself blocked for the full 30 s, or the ureq call was wedged in a tight blocking-pool slot. `last_status=None` (vs `Some(401)` / `Some(200)`) is consistent with the probe never receiving a response.

The `last_http_status=None` strongly implies **transport-level wedge**, not stale-agent — the agent IP `10.99.101.2:7777` is the TAP-attached source VM that the snapshot teardown is closing; if the TAP route is in a half-collapsed state during teardown, a blocking ureq `get(...).call()` can hang on TCP-connect well past its `.timeout()` (because `ureq::Agent::timeout` covers per-call deadlines but doesn't preempt a stuck connect on every platform / DNS path).

### Why this didn't surface in r10/r11/r12

In r10/r11, the sync-contract retry budget exhausted at 48-50 s — **before** the teardown completed at +60 s — so the `host_fence: deadline reached` log fired, the slot was leaked, but the smoke client had already given up on the wake. The post-mortem fixated on the budget shortage (r12: "if only we had +10 s of budget the teardown would release the slot") — incorrect, because we never read the `fence_passed` flag in those reviews. The diagnosis chain (C-8a → C-8b → C-8c → C-7-LT → C-7-LT-1) assumed teardown completion = slot release. r13 is the first cycle where the wake budget *outlives* the teardown, exposing the truth: **teardown completes but FAILS to release the slot.**

Looking back at r12's log dump:
```
06:48:02.457  host_fence: deadline reached elapsed_ms=30129
06:48:02.457  sandbox/nomad-ch stop: complete elapsed_ms=60164 fence_passed=false
```
r12 had **the same fence failure**, but the wake had already exhausted at 50 s so the LEAK wasn't on the critical path. r12's "wake budget 50 s vs teardown 60 s — 10 s short" was the wrong story. The right story all along was: **the teardown leaks the slot, so no budget will ever catch it.**

### The C-7-LT-2 fix surface

Two independent fixes need to land:

1. **Root-cause: `wait_for_agent_silent` probe pacing.** Investigate why the probe loop fires ONCE in 30 s. Likely root causes (no current measurement):
   - `ureq::get(...).timeout(500ms).call()` may not honor `timeout(...)` on a blocking-mode call if the underlying connect/DNS resolver doesn't have a separate connect-timeout. Add `.timeout_connect(Duration::from_millis(500))` (ureq 2.x API) OR replace ureq with a connect-deadline-respecting client. Cross-check: the file already uses `ureq` in `wait_for_agent_livez` — does it exhibit the same pathology, or is the timeout path different there?
   - `compio::runtime::spawn_blocking` may serialize when the blocking pool is saturated. Less likely (1+1 cluster, no concurrent stress), but verify by counting active blocking tasks during a teardown.
   - The TAP route's TCP-RST or ICMP-unreach may not arrive (cloud-network behavior on TAP teardown), so the kernel keeps the connect open until its own SYN-retransmit ceiling (typically 60-90 s on Linux defaults). The 500 ms ureq timeout doesn't apply to a connect that never completes if ureq is using a blocking syscall path.

2. **Defense-in-depth: the wake-path should treat a LEAKED slot differently.** Currently, if vm_index is leaked, wake retries on the same slot until budget exhaustion — futile. The wake reservation code could detect the leak via the `sandbox_id ↔ vm_index` mapping (the source's last-known vm_index that is now in leak state) and either (a) escalate to the next vm_index (orphan-prune scope expansion) or (b) terminal-fail fast with a distinct `error_code=slot_leaked` so the client knows retry-via-new-create is the only recourse. This is a wake-machine + restore_handler change.

The (1) fix is required; (2) is a follow-on that prevents wasted budget on a doomed retry.

### Why this is C-7-LT-2, not "C-7-LT-1 RED"

C-7-LT-1 (`f9996fcf`) widened the async-mode retry budget exactly as the r12 diagnosis prescribed. It WORKED — controller logged `attempts=36 budget_ms=70000`, confirming the policy change took effect. The async polling contract, the §10.0 envelope, the per-attempt logs, the state machine transitions — every C-7-LT-PR1/PR2/FOLLOWUP/-1 invariant held. C-7-LT-1 did precisely what it was supposed to do; the residual is in `wait_for_agent_silent`'s probing loop (T-7-era code, predates C-7-LT). Naming it **C-7-LT-2** preserves the lineage: -1 widened the budget under the now-correct async contract, -2 fixes the upstream teardown that the widened budget revealed to be broken.

The pattern of "every cycle finds one new production-only signal" continues. r13 found:
- That `host_fence` only probes ONCE in 30 s (`probes=1, consecutive_misses=1`).
- That `fence_passed=false` causes the controller to LEAK the vm_index.
- That C-8a → C-8b → C-8c → C-7-LT → C-7-LT-1's entire diagnostic chain was reasoning over a faulty premise: the teardown does NOT release the slot; the smoke just couldn't see it because the wake gave up earlier.

## GO / NO-GO for T-8b-stress

**NO-GO until C-7-LT-2 lands.** Single-cycle smoke FAILED via slot leak; stress (concurrency >1, multiple cycles) would replay this LEAK every cycle and cumulatively starve the index. The leak-not-exhaustion failure mode means the C-7-LT-1 widening cannot help; we need to fix the teardown, not the wake.

**Recommended next steps (sequence):**

1. **C-7-LT-2 PR1 — instrument `wait_for_agent_silent`.** Add a `compio::time::interval` driver instead of `loop { … sleep(100ms) }`, so probe pacing is decoupled from blocking-task return time. Surface per-probe timing in a DEBUG log to identify whether the wedge is in `ureq.call()` or in `spawn_blocking()` queueing.
2. **C-7-LT-2 PR2 — fix ureq probe wedge.** Once root-caused, either set `.timeout_connect(...)` if available, replace with a compio-native probe path, OR add an outer `compio::time::timeout` wrapping the `spawn_blocking` future so a stuck blocking task can't consume the entire fence budget.
3. **C-7-LT-2 PR3 — defense-in-depth: detect leaked slot in wake reservation.** When `vm_index N` is marked leaked in DB, wake should terminal-fail fast with `slot_leaked` rather than retry to budget exhaustion.
4. **Re-smoke (r14).** Single 1+1 cycle, controller v29.
5. **T-8b-stress** at concurrency=4 × 8 cycles after r14 GREEN.

## Per-attempt log validation

The C-7-LT-1 deliverable's signature — `attempts=36 budget_ms=70000` — is CONFIRMED IN PROD. The retry policy update is operational; this row of the deferred table moves to LANDED. The smoke logs show exactly 36 attempt INFO lines, each from `target=zeroship_sandbox::restore_handler`, at 2-second cadence. Compared with r12's `attempts=26 budget_ms=50000`, the policy delta is +10 attempts / +20 seconds — exactly `2×fence_30 + HEADROOM_10 = 70 s` per the C-7-LT-1 commit.

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

Cluster fully removed. Cost: ~$0.038/hr server + ~$1.55/hr worker for ~10 minutes ≈ **$0.27 for this cluster cycle** (clean first-boot, no boot-loop). Cumulative today: 13 cycles × ~$0.28 avg ≈ **$3.6 total** against the $1000/day cap (0.36%).

## Counts

- **Sandbox bugs found across cluster cycles:** C-1 (r1), C-2 (r2), C-3 (r3), C-4 (r4-r5), C-5 (r6), C-6 (r7+r8), C-7 (r8, FIXED), C-8 (r9, FIXED), C-8a (r9, FIXED), C-8b (r10, FIXED), C-8c (r11, OBSOLETED by C-7-LT), **C-7-LT-1 (r12, FIXED in r13 — controller v28)**, **C-7-LT-2 (r13, NEW — `wait_for_agent_silent` probe wedge + slot LEAK)**.
- **Distinct production-only signals in 13 cycles:** 12 (pattern continues: each cycle exposes exactly one new signal).
- **C-7-LT-1 effect:** the async-mode retry budget widened from 50 s to 70 s (26 attempts → 36 attempts). The widening shipped exactly as the diagnosis prescribed; the policy update is LANDED.
- **Cumulative cycle:** 13 of today.

## Closures-this-cycle

- **C-7-LT-1 (async-mode retry budget):** LANDED. Controller logs `attempts=36 budget_ms=70000` in async mode at fence=30 — the `2×fence + HEADROOM = 70 s` formula is operative. Closure hash: `f9996fcf`.

## Opens / deferred adds

- **C-7-LT-2 (NEW, P0 for T-8b-stress):** `wait_for_agent_silent` probe loop fires only ONCE across the 30 s fence budget (`probes=1, consecutive_misses=1, last_http_status=None`). The likely root cause is a blocking `ureq::get(...).timeout(500ms).call()` that ignores the timeout on a stuck connect — needs instrumentation (PR1), root-cause fix (PR2), and a wake-side defense-in-depth (PR3) to terminal-fail fast on a leaked slot rather than retry to budget exhaustion. Add to `docs/reviews/sandbox-snapshot-restore-deferred.md` as OPEN-CRITICAL on next deferred-refresh cycle.
- **Pre-r13-conclusion correction (NEW, P3 retrospective):** r10/r11/r12 reviews stated "teardown completes at +60 s and releases the slot, but the wake budget gives up earlier" — r13 shows this premise was wrong; the teardown completes at +60 s but LEAKS the slot. The C-8a / C-8b / C-8c / C-7-LT-1 fix chain is still correct *as fixes* (the budget tuning matters when the agent does go silent), but the production-only signal it was diagnosing was always `host_fence` failing under the hood. Add a one-paragraph footnote to the relevant prior reviews on next docs-pass.
- **Smoke harness needs update (NEW, P2 ops-polish):** `/opt/stress/snapshot_stress.py` (from GCS `gs://suger-dev-zsbx-artifacts/stress/`, dated 2026-05-06) is pre-C-7-LT — it expects synchronous 200 from `POST /wake`. r13 used an ad-hoc polling client (`/tmp/snapshot_stress_r13.py`); upload that (or a refactored version) to `gs://…/stress/snapshot_stress.py` before T-8b-stress so the in-cluster harness handles the C-7-LT contract.

To be added to `sandbox-snapshot-restore-deferred.md` in a follow-up commit.

## Sandbox HEAD anchor

| Anchor | Commit |
|---|---|
| C-7-LT-PR2-FOLLOWUP LANDED | `93862496` |
| GATE-C2 part 1 (UNIQUE INDEX) | `1cfc9182` |
| GATE-C2 part 2 (ON CONFLICT in db) | `678ec197` |
| GATE-C2 part 3 (admin_handlers replay) | `db248cbf` |
| GATE-C2 docs/deferred LANDED | `3c3d72b4` |
| C-7-LT-1 LANDED | `f9996fcf` |
| Docs r17-r18 reviewer artifacts | `370d13e6` |
| v28 binary content source | `f9996fcf` |
| Pin-bump commit (v27 → v28) | `5442a29c` |
| Current HEAD | `5442a29c` (this review is a follow-up commit on top) |

## What's next

C-7-LT-1 closed the budget gap exactly as designed. The first end-to-end green attempt did NOT land — r13 surfaced a different bug (C-7-LT-2) one layer deeper: the host-fence probe loop is wedged, the slot leaks, and no widening of the wake retry budget can rescue a leaked slot. **The fence-probe wedge must be fixed before T-8b-stress.**

The signal-density pattern continues: 13 cycles, 12 distinct signals, each one a production-only bug that no test infrastructure caught. r13 is the first cycle where a fix landed exactly as prescribed AND a deeper bug was revealed. The chain reading is now:
- C-1..C-6: shipping bugs (early infra).
- C-7..C-8c: synchronous-contract budget exhaustion (r4-r11).
- C-7-LT: async-contract polling (r12, structural fix).
- C-7-LT-1: async-mode budget widened (r13, LANDED).
- **C-7-LT-2: `host_fence` probe-wedge → slot LEAK (r13, NEW).**

The synchronous wake contract that haunted r4-r11 is GONE. The retry-budget cap that haunted r12 is GONE. What remains is the upstream teardown bug that was hiding behind all of them. After C-7-LT-2 + a re-smoke (r14), T-8b-stress is on.
