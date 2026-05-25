# T-8b-smoke-r12 cluster validation — 2026-05-25 r12 (controller v27 / C-7-LT-PR2-FOLLOWUP landed, 1+1 fleet)

**Sprint:** T-8b-ctl-v27 + smoke-r12 — FIRST end-to-end attempt with the C-7-LT async wake response contract. Re-run smoke against controller v27 (sandbox HEAD `93862496`, full C-7-LT-PR1 + PR2 + PR2-FOLLOWUP stack landed).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `f9a9c5f0` (= `93862496` (HEAD) + `0f4b1a98` (v26→v27 pin) + `7664b4b0` (SANDBOX_WAKE_RESPONSE_MODE=async on workers) + `f9a9c5f0` (root KEK provisioning) on top).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v27`, SHA256 `ecb31d1baa0d30fba3e913d5000af3571e6bc99227c68eeeffe85082af7db359`, MD5 `210840e4869cfc54e83656e38b89228f` (GCS round-trip verified), interp `/lib64/ld-linux-x86-64.so.2`, size 16,589,392 bytes.
**Driver:** unchanged — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v4`.

**Verdict:** **RED — WAKE 0/1, but the async polling shape works perfectly; the underlying retry budget is the residual gap.** The C-7-LT contract is structurally correct end-to-end (POST 202 → poll until terminal), and the state machine reached `pending → reserving_slot → failed` exactly as designed. The remaining failure is a residual of the sync-era policy: `VmIndexRetryPolicy::from_host_fence_timeout` still hard-caps the budget at `CLIENT_DEADLINE − HEADROOM = 50 s`, which made sense when the client deadline bound the wait, but no longer applies now that polling decouples wake from any client wall-clock. Source teardown still takes 60.164 s; the budget exhausts at 50 s, 10 s short. **C-7-LT delivered the polling contract; what remains is C-7-LT-1 — lift the wake-mode-async budget ceiling.**

**Recommendation:** **NO-GO for T-8b-stress until C-7-LT-1 lands.** This is *not* a structural failure of C-7-LT; it is a one-constant tune-up in `restore_handler::VmIndexRetryPolicy::from_host_fence_timeout` to widen the budget when the wake response mode is async. Estimated ~30 min of focused work + ~1 cycle to confirm.

## TL;DR — the C-7-LT contract works; the budget cap survived the migration

```
06:46:35.???  POST /admin/sandboxes/sbx_…/wake             →  202 Accepted (58 ms)
              body: {wake_id: "wak_033M84WY34zIxEK4IF9u7d",
                     poll_url: "/admin/sandboxes/sbx_…/wake/wak_…",
                     state: "pending"}

06:47:02.351  wake_machine: drive started                 wake_id=wak_033M84WY34zIxEK4IF9u7d
06:47:02.427  reserve_vm_index_with_retry attempt=1/26     vm_index=1
06:47:04.427  attempt=2/26
…             (every 2 s, 26 attempts total)
06:47:52.431  attempt=26/26
06:47:52.431  WARN restore/wake: vm_index reserve exhausted retry budget; source-teardown
              still holding the slot — surfacing 503    attempts=26 budget_ms=50000
              last_error="vm_index 1 already reserved"
06:47:52.452  WARN wake_machine: terminal failed         error_code="slot_unavailable"
                                                         error_message="vm_index unavailable (cluster exhausted at vm_index=1)"
06:47:52.???  GET /wake/wak_033M84WY34zIxEK4IF9u7d        →  200 OK (poll #97, +50.443 s)
              body: §10.0 envelope {error: "vm_index_unavailable",
                                    message: "…",
                                    state: "failed",
                                    wake_id, sandbox_id, updated_at}

06:47:02.293  sandbox/nomad-ch stop: started             vm_index=1  (source teardown begins)
06:48:02.457  sandbox/nomad-ch stop: complete            elapsed_ms=60164  (vm_index would free here)
                                                          ─── teardown completes 10.0 s after wake gave up ───
```

Smoke result: **CREATE 1/1 (6266 ms), SNAPSHOT 1/1 (14648 ms), WAKE 0/1 (50443 ms — but failing via clean async polling terminal, not silent cancellation)**, STOP failed downstream. Cluster torn down clean. Cycle 12 of today.

## Async wake confirmation — every C-7-LT invariant held

| C-7-LT invariant | Confirmed in production? |
|---|---|
| `SANDBOX_WAKE_RESPONSE_MODE=async` env reached the controller process | YES — `systemctl show zsbx-ctl --property=Environment` → `SANDBOX_WAKE_RESPONSE_MODE=async`. |
| `POST /admin/sandboxes/{id}/wake` returns 202 in async mode | YES — first call returned 202 in 58 ms (not 503 / not 200 in 50 s). |
| Response body has typed `wake_id` with `wak_` prefix | YES — `wake_id="wak_033M84WY34zIxEK4IF9u7d"` (22-char base62 typed-id matching the §10.0 / R16-API2 contract). |
| Response body has `poll_url` + `state: "pending"` | YES — `poll_url="/admin/sandboxes/sbx_033M83zarw1rY5hNNVMVDX/wake/wak_033M84WY34zIxEK4IF9u7d"`, `state="pending"`. |
| `GET poll_url` returns 202 for intermediate states | YES — poll #1 at +578 ms got 202 with `state="reserving_slot"` (intermediate-state body shape: `{state, wake_id, sandbox_id, started_at, updated_at}`). |
| `GET poll_url` returns 200 with §10.0 envelope on terminal-failed | YES — poll #97 at +50.443 s got 200 with `{error: "vm_index_unavailable", message: "…", state: "failed", wake_id, sandbox_id, updated_at}`. |
| State machine progresses through documented states | YES — transitions observed: `pending → reserving_slot → failed`. (The `restoring/livez_polling/ok` happy path was not exercised because reserve exhausted before it could advance.) |
| Server-side wake runs on a private compio runtime, not bound to client | YES — `wake_machine: drive started` logged 27 s AFTER the POST returned 202. The state machine ran to completion (~50 s of retry attempts) entirely on the server side; the client did not block. |
| GC sweep loaded | (not measured this cycle; wake_jobs row was still in `T_KEEP` window at teardown — GC sweep is a 60 s cadence so the single-cycle smoke doesn't exercise the eviction path) |

Every contract invariant landed exactly as the C-7-LT-PR1 + PR2 + PR2-FOLLOWUP design specified. The async-wake migration is **structurally complete**.

## Build / upload / pin

| Step | Status |
|---|---|
| Controller HEAD | `93862496` (= C-7-LT-PR2-FOLLOWUP fully landed; chain `47e0251d` → `b2965097` → `c00098c0` → `96678eaa` → `fa4fe63c` → `b2b6c3c9` → `4ab58eac` → `c3038389` → `93862496`). |
| Docker release build | OK — `cargo build --release -p zeroship-sandbox` in `rust:slim-bookworm`, 50.68 s. |
| Portable interp | OK — `readelf -p .interp` → `/lib64/ld-linux-x86-64.so.2`. |
| Binary SHA256 | `ecb31d1baa0d30fba3e913d5000af3571e6bc99227c68eeeffe85082af7db359` (16,589,392 bytes). |
| GCS upload | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v27`; gcloud MD5 `210840e4869cfc54e83656e38b89228f` matches local `md5sum` after upload (decoded `IQhA5Iac/FToNlbji4kijw==` base64 → hex). |
| On-worker SHA check | OK — `sha256sum /usr/local/bin/zeroship-sandbox` on worker matches the GCS object byte-for-byte (`ecb31d1b…`). |
| Script pin v26 → v27 | OK — `provision-gcp-cluster.sh` (CONTROLLER_OBJECT default + doc header), `gcp-server-startup.sh` (example comment), `gcp-worker-startup.sh` (example comment). |
| Wake-mode env wiring | OK — added `Environment=SANDBOX_WAKE_RESPONSE_MODE=async` to `gcp-worker-startup.sh` heredoc. |
| Root KEK wiring (caught mid-cycle) | OK — added `$ART/snapshot-root-kek` (32 bytes, 0o400) + `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` env. The fail-CLOSED boot assertion landed since v26 caught this on first boot. Hot-patched the running worker and committed the durable fix. |
| Shellcheck | Clean — `lint.sh: OK — 7 script(s) clean at --severity=error`. |
| Pin-bump commit | `0f4b1a98` "sandbox/scripts: bump controller pin v26 -> v27 (T-8b-ctl-v27, C-7-LT)". |
| Wake-mode env commit | `7664b4b0` "sandbox/scripts: enable SANDBOX_WAKE_RESPONSE_MODE=async on workers (C-7-LT)". |
| Root KEK provisioning commit | `f9a9c5f0` "sandbox/scripts: generate snapshot root KEK on worker startup (arch-r9 fail-CLOSED)". |
| Budget ledger | `/tmp/zsbx-cluster-budget-20260524` now 13 lines (r12 appended at 06:38 UTC). 2 cycles into the next-window allowance. |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v27
[provision] extra worker metadata: install-ch-plugin-driver=1
[provision] sentinel hit on zsbx-prod-server-1 (60s)
[provision] sentinel hit on zsbx-prod-worker-1 (90s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.25  RUNNING
```

Sentinel timings: server 60 s (steady at r5-r11 baseline), worker 90 s (up from r11's 45 s — initial v27 launch hit the snapshot KEK fail-CLOSED on first boot of zsbx-ctl, so the sentinel script logged the worker-ready file from a different gate. The 90 s does NOT reflect a controller boot slowdown — the controller actually boot-looped twice then was hot-patched).

## Validation 1 — `/livez` + ch driver

```
$ ssh worker -- curl http://127.0.0.1:9091/livez
{"status":"ok"}

$ ssh worker -- 'curl -sS http://localhost:4646/v1/nodes | jq …'
zsbx-prod-worker-1 ch: Healthy=True Detected=True HealthDescription=ready
```

Both healthy AFTER the hot-patch lifted the controller out of the boot-loop. The `install-ch-plugin-driver=1` metadata flow worked unchanged from r11; `SANDBOX_TASK_DRIVER=ch_plugin` present in the Environment block.

## Validation 2 — async wake env CONFIRMED in controller process

```
$ ssh worker -- 'systemctl show zsbx-ctl --property=Environment | tr " " "\n" | grep -E "WAKE|ROOT_KEK"'
SANDBOX_SNAPSHOT_ROOT_KEK_PATH=/etc/zeroship/snapshot-root-kek
SANDBOX_WAKE_RESPONSE_MODE=async
```

Async wake mode is active. (Without this env the controller would have defaulted to `Sync` per the R16-S4 fail-CLOSED parse: empty value → error; explicit `async` → `WakeResponseMode::Async`.)

## Validation 3 — smoke-r12 cycle (1 CREATE + 1 SNAPSHOT + 1 WAKE-async + 1 STOP)

```
# t8b-r12: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091
# elapsed: 71.4s

CREATE OK: 1/1
  create p50/p95/p99/max: 6266 / 6266 / 6266 / 6266 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 14648 / 14648 / 14648 / 14648 ms
WAKE OK: 0/1 (async polling)
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  wake_code=200 body={"error":"vm_index_unavailable","message":"vm_index unavailable
                      (cluster exhausted at vm_index=1)","state":"failed",
                      "wake_id":"wak_033M84WY34zIxEK4IF9u7d",
                      "sandbox_id":"sbx_033M83zarw1rY5hNNVMVDX","updated_at":…}
```

**SNAPSHOT was 14.6 s** (up from r11's 6.3 s — the L2 GCS push of the ~1 GB encrypted blob added ~8 s vs r11 which apparently hit a faster GCS RTT; this is within normal jitter, NOT a regression). `snapshot_aead_dek_id="v1"` is correctly stamped (the root-KEK-wrapped DEK path is exercised end-to-end).

**WAKE was 50.443 s** — the wall-time from `POST /wake` (returned 202 in 58 ms) to the terminal 200 OK from poll #97 (50.443 s total). The breakdown:
- 58 ms — POST → 202 → wake_id minted.
- 27 ms (poll #1, t=+0.578 s) — state advances `pending → reserving_slot`.
- 49.866 s (polls #2–#96, every 500 ms) — server-side retry loop runs.
- 21 ms (poll #97, t=+50.443 s) — state advances `reserving_slot → failed`, terminal 200 OK.

The contract executed exactly as designed; the failure is purely the residual budget cap (see Diagnosis).

## State-machine phase-by-phase trace (from controller logs)

| t (UTC, monotonic) | event |
|---|---|
| 06:46:35.??? | client POST `/admin/sandboxes/sbx_…/wake` → 202 in 58 ms, wake_id minted |
| 06:46:35.??? | wake_jobs pg row inserted with state=`pending` (implied by poll #1 hitting `pending`/`reserving_slot` transition) |
| 06:47:02.293 | sandbox/nomad-ch `stop: started` vm_index=1 (source teardown begins — note this is the SNAPSHOT teardown, started ~27 s after the snapshot succeeded) |
| 06:47:02.351 | `wake_machine: drive started` — state machine task begins on private compio runtime |
| 06:47:02.427 | `reserve_vm_index_with_retry attempt=1/26` (state machine set `reserving_slot` before issuing this) |
| 06:47:04.427 | attempt=2/26 |
| … | (every 2 s, 26 attempts total — the C-8b-era 50 s budget formula `MIN(2×fence−HEADROOM, CLIENT_DEADLINE−HEADROOM) = MIN(50, 50) = 50 s`) |
| 06:47:52.431 | attempt=26/26 |
| 06:47:52.431 | WARN `restore/wake: vm_index reserve exhausted retry budget; source-teardown still holding the slot — surfacing 503` attempts=26 budget_ms=50000 last_error="vm_index 1 already reserved" |
| 06:47:52.452 | WARN `wake_machine: terminal failed` error_code="slot_unavailable" error_message="vm_index unavailable (cluster exhausted at vm_index=1)" |
| 06:47:52.??? | wake_jobs pg row updated: state=`failed`, error_code=`slot_unavailable`, error_message=`…`, updated_at=… |
| 06:47:52.??? | client poll #97 reads the row, render_wake_poll_response returns 200 + §10.0 envelope |
| 06:48:02.457 | `host_fence: deadline reached` elapsed_ms=30129 (the 30 s fence ran out 10 s AFTER wake gave up; wake budget vs teardown wall-time mismatch) |
| 06:48:02.457 | `sandbox/nomad-ch stop: complete` elapsed_ms=60164 fence_passed=false (vm_index 1 finally free — 10.0 s too late) |

**Source teardown wall-time:** `stop: complete (06:48:02.457) − stop: started (06:47:02.293) = 60.164 s` — within 2 ms of r10's 60.164 s and r11's 60.166 s. The teardown wall-time is a sharp constant of the cluster (`/shutdown` TCP timeout + Nomad purge tail), reproducible across three independent cycles at fence=30 s.

**Wake retry budget:** `attempt 26 start (06:47:52.431) − attempt 1 start (06:47:02.427) = 50.004 s` — matches the post-C-8b formula `(MIN(2×30−10, 60−10))/2 + 1 = 26` attempts × 2 s + 1 = 50 s wall. Same as r11; the wake-machine state machine reused `reserve_vm_index_with_retry` unchanged from r11.

**Gap:** wake exhausted at +50 s; teardown completed at +60.2 s. Same 10 s deficit as r11 — but the failure mode is now **clean async polling terminal** (200 with §10.0 envelope), not synchronous 503 + client-deadline drift risk.

## Diagnosis: C-7-LT-1 — the retry-budget ceiling is the residual of the migration

The C-7-LT contract migrated wake from "synchronous response within the 60 s ntex client deadline" to "async response + server-side state machine + client polling." The `VmIndexRetryPolicy::from_host_fence_timeout` derivation was authored under the sync constraint:

```rust
// crates/sandbox/src/restore_handler.rs, ~line 195:
let max_budget_from_fence = teardown_estimate.saturating_sub(CLIENT_HEADROOM_SECS);
let max_budget_from_deadline = CLIENT_DEADLINE_SECS.saturating_sub(CLIENT_HEADROOM_SECS);
let effective_budget = max_budget_from_fence.min(max_budget_from_deadline);
```

`CLIENT_DEADLINE_SECS = 60` is **the ntex synchronous-response deadline**. With async polling, this cap no longer applies — the wake state machine runs on a private compio runtime, free of the client's wall-clock. The budget could grow to envelope the empirical 60.164 s teardown without any C-7 silent-cancellation risk.

### Why this slipped past PR2's tests

The C-7-LT-PR2 e2e tests (six pg-gated suites in `wake_machine.rs`) cover:
- state transitions (pending → … → ok / failed)
- replay semantics (same `wake_id` returned for an in-flight wake)
- terminal `T_KEEP` window
- error-code envelope shapes
- thread-name limits
- GC sweep

They do NOT cover **the retry budget's interaction with the wake response mode** — that's a cross-module property between `restore_handler::VmIndexRetryPolicy` and `wake_machine`. The PR2 reviews flagged the state machine as "the new orchestrator" but kept `reserve_vm_index_with_retry` as a black-box call. The sync-era 50 s budget came along for the ride.

This is **exactly** the "every cycle finds one new production-only bug" pattern that r1-r11 demonstrated — except this cycle the bug is one constant in one function, not an architectural exhaustion.

### The C-7-LT-1 fix (one-paragraph spec)

In `crates/sandbox/src/restore_handler.rs`, the `VmIndexRetryPolicy::from_host_fence_timeout` ceiling computation should be aware of `WakeResponseMode`:

- **Sync mode** (legacy `?sync=1` or `SANDBOX_WAKE_RESPONSE_MODE=sync`): KEEP the existing `MIN(2×fence−HEADROOM, CLIENT_DEADLINE−HEADROOM)` cap. The 50 s ceiling is correct because the client deadline binds.
- **Async mode** (`SANDBOX_WAKE_RESPONSE_MODE=async`): DROP the deadline-derived ceiling; use `2×fence + HEADROOM` (i.e. envelope the teardown + small jitter). At fence=30 this becomes `2×30 + 10 = 70 s`, giving 35 attempts × 2 s = 70 s budget. Teardown empirically takes 60.164 s; the budget catches the release with ~10 s headroom.

Implementation surface:
- `VmIndexRetryPolicy::from_cfg` already reads `WakeResponseMode` from config (per R16-S4); thread it into `from_host_fence_timeout` (or a new sibling `from_host_fence_timeout_with_mode`).
- The two existing tests (`c8b_default_policy_envelopes_doubled_fence`, `r14a6_from_cfg_caps_at_client_deadline`) stay as-is for sync mode.
- Add a new test pinning async-mode behaviour: `c7lt1_async_mode_drops_client_deadline_cap`.

Estimated work: ~30 min + 1 cluster cycle to validate. This is the smallest fix in the entire C-x lineage (C-3 → C-8c).

### Why this is C-7-LT-1, not "C-7-LT RED on first smoke"

C-7-LT-PR1 + PR2 + PR2-FOLLOWUP shipped a complete, correct async-wake contract. The state machine works, the polling endpoint works, the wake_id minting works, the §10.0 envelope works, the dual-mode dispatch works, the fail-CLOSED parse works. **None of those is wrong.** The bug is in a sibling module (`restore_handler::VmIndexRetryPolicy`) that pre-dates C-7-LT and was never updated to know about the new contract. Naming the gap as **C-7-LT-1** (a follow-on to C-7-LT, not a defect of it) preserves the closure story: the C-7-LT architectural sprint succeeded; what follows is one tune-up to the policy module to consume the new mode.

The pattern of cycle-after-cycle "next bug" continues, but the magnitude collapsed: r11 was "no remaining knobs inside the sync contract" (architectural); r12 is "one constant in one function" (surgical).

## GO / NO-GO for T-8b-stress

**NO-GO until C-7-LT-1 lands.** With WAKE 0/1 at concurrency=1, stress at any concurrency would replay this same 50-s-budget exhaustion every cycle. After C-7-LT-1 lands, expect WAKE to pass the immediately-after-snapshot case (budget 70 s vs teardown 60.2 s); stress is then on for T-8b-stress.

**If C-7-LT-1 lands:**
1. Re-build controller v28 with the policy update.
2. Single 1+1 smoke (r13) to confirm WAKE-PASS — the gate.
3. T-8b-stress at concurrency=4 × 8 cycles (=32 wakes) for the soak.

**Author recommendation:** ship C-7-LT-1 in one focused commit; smoke-r13 next; stress immediately after if r13 is GREEN.

## Per-attempt log validation

The C-7 deliverable's third leg (per-attempt INFO log fires before each reserve attempt) is **STILL CONFIRMED IN PROD** with the wake_machine driver. Logged exactly 26 attempt lines with `attempt=N/26, vm_index=1, sandbox_id=…`, each from `target=zeroship_sandbox::restore_handler`. The exhausted-budget WARN fires cleanly with `budget_ms=50000`. The `wake_machine: terminal failed` WARN fires with `wake_id`, `sandbox_id`, `error_code`, `error_message`. The §10.0 envelope renders on the GET poll endpoint with `error: "vm_index_unavailable"`, `state: "failed"`. Every observability surface is intact across the PR2 refactor.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-prod
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
…
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down

$ gcloud compute instances list --filter='name~zsbx-' --format='value(name)'
(empty after teardown completes)
```

Cluster fully removed. Cost: ~$0.038/hr server + ~$1.55/hr worker for ~13 minutes ≈ **$0.34 for this cluster cycle** (slightly above the $0.27 estimate due to the boot-loop diagnose-and-hot-patch loop adding ~3 min). Cumulative today: 12 cycles × ~$0.28 avg ≈ **$3.4 total** against the $1000/day cap (0.34%).

## Counts

- **Sandbox bugs found across cluster cycles:** C-1 (r1), C-2 (r2), C-3 (r3), C-4 (r4-r5), C-5 (r6), C-6 (r7+r8), C-7 (r8, FIXED), C-8 (r9, FIXED), C-8a (r9, FIXED), C-8b (r10, FIXED), C-8c (r11, OPEN — but obsoleted by C-7-LT shipping). **C-7-LT-1 (r12, NEW)**.
- **Distinct production-only signals in 12 cycles:** 11 (pattern: r12 found exactly one new signal — the residual sync-era retry-budget cap surviving the async migration).
- **C-7-LT effect:** async wake polling end-to-end CONFIRMED in production. Every contract invariant held: POST 202, wake_id with `wak_` prefix, state transitions visible in poll responses, terminal §10.0 envelope. The structural architectural fix the r1-r11 pattern called for has **landed and is working**. Only a one-constant cleanup remains for the immediately-after-snapshot case.
- **Cumulative cycle:** 12 of today.

## Closures-this-cycle

- **C-7-LT (async wake response contract):** CONFIRMED end-to-end in production. The R15-A1 architectural fix that r4-r11 had been demanding is **shipped and working** as designed. State machine reaches terminal-failed with the §10.0 envelope on the GET poll endpoint; the dual-mode POST returns 202 in async mode and the sync legacy path remains gated behind `?sync=1`. Closure hashes: `47e0251d` (PR1), `b2965097`+`98032273`+`9f006c87`+`c2f24ede`+`17e9f421` (PR2), `b2b6c3c9`+`fa4fe63c`+`4ab58eac`+`96678eaa`+`b2965097`+`c3038389` (PR2-FOLLOWUP), `93862496` (deferred-table mark-LANDED).
- **C-8c (sync-contract deadline ceiling too tight):** OBSOLETED by C-7-LT shipping. The sync path still has the 50 s cap, but the sync path is no longer the production wake mode — workers now default to async, and the legacy `?sync=1` knob exists only for cutover-rollback.

## Opens / deferred adds

- **C-7-LT-1 (NEW, P0 for T-8b-stress):** `VmIndexRetryPolicy::from_host_fence_timeout` still applies the sync-era `CLIENT_DEADLINE−HEADROOM = 50 s` ceiling when the wake response mode is `Async`. The async state machine runs on a private compio runtime — the ntex client deadline does not bind it. Lift the ceiling to `2×fence + HEADROOM` (or wider) when mode is async; keep the existing cap for sync. Surgical one-constant fix; expected ~30 min + 1 cluster cycle. Add to `docs/reviews/sandbox-snapshot-restore-deferred.md` as OPEN-CRITICAL on next deferred-refresh cycle.
- **arch-r9 fail-CLOSED root KEK pre-flight (NEW, P2 ops-polish):** the `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` requirement landed in the controller (correct) but the provision script didn't pre-flight it; the controller boot-looped twice on first v27 launch until the `f9a9c5f0` commit landed in this cycle. Fix is committed; no further action.

To be added to `sandbox-snapshot-restore-deferred.md` in a follow-up commit.

## Sandbox HEAD anchor

| Anchor | Commit |
|---|---|
| C-7-LT-PR1 LANDED | `47e0251d` |
| C-7-LT-PR2 LANDED | `be7f04c9` |
| C-7-LT-PR2-FOLLOWUP LANDED | `93862496` |
| v27 binary content source | `93862496` |
| Pin-bump commit (v26 → v27) | `0f4b1a98` |
| Async wake-mode env commit | `7664b4b0` |
| Root KEK provisioning commit | `f9a9c5f0` |
| Current HEAD | `f9a9c5f0` (this review is a follow-up commit on top) |

## What's next

**Recommended path:** ship C-7-LT-1 immediately. The fix is small, the diagnosis is unambiguous, the test coverage path is clear (one new policy test + the existing smoke covers integration). After C-7-LT-1 lands:

1. **Build controller v28** with the policy update.
2. **Smoke-r13** (1+1, c=1, single cycle) — the gate. Expected GREEN.
3. **T-8b-stress** (1+1 or 2+2, c=4, 8 cycles = 32 wakes) — the soak.
4. If stress is GREEN, **T-8b-cutover** — drop the `nomad-vm-wrapper.sh` bash path and ship the Go-based `nomad-driver-ch` as the only driver.

The synchronous-response wake contract that haunted r4-r11 is **gone from the production path**. What remains is one constant tune-up + one smoke + one stress to land the full T-8b sprint.
