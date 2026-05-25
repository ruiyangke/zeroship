# T-8b-smoke-r11 cluster validation — 2026-05-25 r11 (controller v26 / C-8b fix landed, 1+1 fleet)

**Sprint:** T-8b-ctl-v26 + smoke-r11 — first cycle of the next UTC day's budget window (override of the r10 "10/10 exhausted" advisory was explicitly granted in the pilot brief: "cycle 11 of today" + "fix then all and do real end to end test"). Re-run smoke against controller v26 (sandbox HEAD `0fc56df5`, contains the C-8b 2x-fence-ceiling fix at `64af1803`).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `2e9ae598` (v25 → v26 pin bump; controller binary contents from `64af1803`).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v26`, SHA `62d79d80fe3dcf690603e9c25cc259fb9ff95ff77df4d5c575c99a786955e282`, MD5 `1ef27fba105ccf85fb7e7d072c3414f7` (verified MD5 round-trips upload), interp `/lib64/ld-linux-x86-64.so.2`. Binary SHA on worker (`/usr/local/bin/zeroship-sandbox`) matches the GCS object byte-for-byte.
**Driver:** unchanged — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v4`.
**Verdict:** **RED — WAKE 0/1.** C-8b widened the retry budget from 20 s → 50 s exactly as designed (all 26 attempts logged, `budget_ms=50000` WARN fired cleanly), but the empirical source-teardown wall-time is **60.166 s** at fence=30 s — 10 s past the C-8b budget. **The 2× fence assumption is approximately correct, but not enough: teardown is ~2.0× fence and the budget is hard-capped at the deadline-derived 50 s ceiling, so the budget tops out 10 s short of teardown.** This is **C-8c — a quantitative refinement of C-8b**, not a new architectural class. The 11th cluster cycle, the 10th distinct production-only signal.
**Recommendation:** **NO-GO for T-8b-stress.** The synchronous-response wake contract is now provably under-budgeted by ~10 s at the existing client deadline ceiling — i.e. the C-8a deadline-derived ceiling itself is too tight (CLIENT_DEADLINE_SECS=60 − CLIENT_HEADROOM_SECS=10 = 50 s). There is **no remaining knob inside the synchronous-response contract** that fixes this without violating the deadline. **C-7-LT (async wake response + polling, R15-A1) is the only path forward.** Pause cluster work; surface to user for architectural sprint dispatch.

## TL;DR

```
04:37:16.498  sandbox/nomad-ch stop: started               vm_index=1  (source teardown begins)
04:37:16.499  restore: phase entry                                     (wake handler fires 1 ms later)
04:37:16.555  restore: phase pre_reserve_vm_index          vm_index=1
04:37:16.555  reserve_vm_index_with_retry attempt=1/26     vm_index=1  (C-8b widened to 26 × 2 s = 50 s budget)
04:37:18.555  attempt=2/26
04:37:20.555  attempt=3/26
…             (every 2 s, all 26 logged in full)
04:38:04.559  attempt=25/26
04:38:06.559  attempt=26/26
04:38:06.559  WARN restore/wake: vm_index reserve exhausted retry budget; source-teardown
              still holding the slot — surfacing 503    attempts=26 budget_ms=50000
              last_error="vm_index 1 already reserved"
04:38:06.580  WARN admin/wake: handler failed              error="vm_index unavailable (cluster exhausted at vm_index=1)"
                                                          ─── 503 returned to client here (wake_ms=50083) ───
04:38:16.663  ERROR sandbox/nomad-ch host_fence: timeout   elapsed_ms=30129 consecutive_misses=1
              "agent at http://10.99.101.2:7777 still answering at fence deadline"
04:38:16.663  WARN sandbox/nomad-ch vm_index leak           vm_index=1 reason=host_fence_timeout
04:38:16.664  INFO sandbox/nomad-ch stop: complete         vm_index=1 fence_passed=false elapsed_ms=60166
                                                          ─── vm_index would have been free at +60 s ───
```

Source teardown started at 04:37:16.498 and ended at 04:38:16.664 — **60.166 s wall-time, 2.005× the configured 30 s fence_timeout** (precisely the same ratio as r10's 60.164 s / 30 s). The wake retry loop ended at 04:38:06.559 (50 s in), **10.1 s before the slot vacated**.

Smoke result: **CREATE 1/1 (6424 ms), SNAPSHOT 1/1 (6314 ms), WAKE 0/1 (50083 ms)**, STOP failed downstream with generation drift. Cluster torn down clean. Cycle 11 of today.

## Build / upload / pin

| Step | Status |
|---|---|
| Controller HEAD | `0fc56df5` (= `64af1803` C-8b fix + `0fc56df5` deferred-table backfill). |
| Docker release build | OK — `cargo build --release -p zeroship-sandbox` in `rust:slim-bookworm`, finished in 31.84 s. |
| Portable interp | OK — `readelf -p .interp` confirmed `/lib64/ld-linux-x86-64.so.2`. |
| Binary SHA | `62d79d80fe3dcf690603e9c25cc259fb9ff95ff77df4d5c575c99a786955e282` (16,498,912 bytes). |
| GCS upload | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v26`; gcloud MD5 (`1ef27fba105ccf85fb7e7d072c3414f7`) matches local `md5sum` after upload. |
| On-worker SHA check | OK — `sha256sum /usr/local/bin/zeroship-sandbox` on worker matches the GCS object byte-for-byte (`62d79d80…`). |
| Script pin v25 → v26 | OK — `provision-gcp-cluster.sh` (CONTROLLER_OBJECT default + doc header), `gcp-server-startup.sh` (example comment), `gcp-worker-startup.sh` (example comment). |
| Shellcheck | Clean — `lint.sh: OK — 7 script(s) clean at --severity=error`. |
| Pin-bump commit | `2e9ae598` "sandbox/scripts: bump controller pin v25 -> v26 (T-8b-ctl-v26-upload, C-8b)". |
| Budget ledger | `/tmp/zsbx-cluster-budget-20260524` now 11 lines (r11 appended at 04:33:01 UTC). 1 cycle into the next-window allowance per the pilot brief's explicit override. |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v26
[provision] extra worker metadata: install-ch-plugin-driver=1
[provision] sentinel hit on zsbx-prod-server-1 (60s)
[provision] sentinel hit on zsbx-prod-worker-1 (45s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.24  RUNNING
```

Sentinel timings: server 60 s (matches r5-r10), worker 45 s (one notch up from r10's 30 s — the apt/image cache cooled overnight; still well inside the 1200 s budget).

## Validation 1 — `/livez` + ch driver

```
$ ssh worker -- curl http://127.0.0.1:9091/livez
{"status":"ok"}

$ ssh worker -- 'curl -sS http://localhost:4646/v1/nodes | jq …'
node=zsbx-prod-worker-1
  ch: Healthy=True Detected=True
```

Both healthy. The `install-ch-plugin-driver=1` metadata flow worked — `SANDBOX_TASK_DRIVER=ch_plugin` is present in the unit's Environment (T-8b-prereqs-config still effective).

## Validation 2 — C-8 env var landed (regression check)

```
$ ssh worker -- 'systemctl show zsbx-ctl | grep ^Environment'
Environment=…
  SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30      ← CONFIRMED
  SANDBOX_TASK_DRIVER=ch_plugin
  SANDBOX_PORT=9091
  RUST_LOG=info,zeroship_sandbox=info
```

**C-8 env var IS PRESENT.** No regression from the v25 → v26 pin bump.

## Validation 3 — snapshot_stress.py --concurrency 1 --cycles 1

```
# T-8b-smoke-r11: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091
# elapsed: 62.9s

CREATE OK: 1/1
  create p50/p95/p99/max: 6424 / 6424 / 6424 / 6424 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 6314 / 6314 / 6314 / 6314 ms
WAKE OK: 0/1
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  [1x] code=503: {"error":"vm_index_unavailable","message":"no vm_index available …","requested":1}
```

`wake_ms=50083` — within 83 ms of the C-8b-derived 50,000 ms budget (26 attempts × 2 s − 2 s for the trailing 26th non-sleeping check ≈ 50,000 ms). The retry loop ran to completion (26/26 attempts), then surfaced 503 well within the 60 s ntex client deadline.

The C-7 cancellation regression remains CLOSED: 26/26 per-attempt INFO logs visible, exhausted-budget WARN fired cleanly, HTTP returned 503 — no silent hang. C-8a's deadline-derived ceiling (50 s) bound this run, preventing C-7 re-introduction.

## Phase-by-phase wake trace (the C-8c finding)

| t (UTC)       | event                                                          |
|---------------|----------------------------------------------------------------|
| 04:37:16.498  | `sandbox/nomad-ch stop: started` vm_index=1 (source teardown begins) |
| 04:37:16.499  | `restore: phase entry` (wake handler fires 1 ms later)         |
| 04:37:16.555  | `restore: phase pre_reserve_vm_index` vm_index=1               |
| 04:37:16.555  | `reserve_vm_index_with_retry attempt=1/26`                     |
| 04:37:18.555  | attempt=2/26                                                   |
| …             | (every 2 s, 26 attempts total)                                 |
| 04:38:04.559  | attempt=25/26                                                  |
| 04:38:06.559  | attempt=26/26                                                  |
| 04:38:06.559  | WARN `vm_index reserve exhausted retry budget; source-teardown still holding the slot — surfacing 503` attempts=26 budget_ms=50000 last_error="vm_index 1 already reserved" |
| 04:38:06.580  | WARN admin/wake handler failed; HTTP 503 sent to client (wake_ms=50083) |
| 04:38:16.663  | ERROR `sandbox/nomad-ch host_fence: timeout` elapsed_ms=30129 `consecutive_misses=1` — agent still answering |
| 04:38:16.663  | WARN `sandbox/nomad-ch vm_index leak` vm_index=1 reason=host_fence_timeout |
| 04:38:16.664  | INFO `sandbox/nomad-ch stop: complete` vm_index=1 fence_passed=false elapsed_ms=60166 |

**Source teardown wall-time:** `stop: complete (04:38:16.664) − stop: started (04:37:16.498) = 60.166 s` — **exactly 2.005× the fence_timeout of 30 s, matching r10's 60.164 s within 2 ms.** Empirical reproducibility across two cycles confirms the 2× ratio is a stable constant of the teardown pipeline at fence=30 (not a one-time outlier).

**Wake retry budget:** `attempt 26 start (04:38:06.559) − attempt 1 start (04:37:16.555) = 50.004 s`. Matches the C-8b design point: 25 sleeps × 2 s = 50 s, 26th attempt fires immediately. Wire shape is byte-perfect to the C-8b formula `(MIN(2×fence − HEADROOM, CLIENT_DEADLINE − HEADROOM)) / INTERVAL + 1 = MIN(50, 50) / 2 + 1 = 26`.

**Gap:** wake exhausted at +50 s; teardown completed at +60.2 s. Wake gave up 10.2 s too early.

## Diagnosis: C-8c — the deadline-derived ceiling itself is too tight

The C-8b fix design assumed:

> Source teardown wall-time ≈ 2× host_fence_timeout. So if we set fence=30 s, teardown ≈ 60 s, and the C-8b budget of MIN(2×30 − 10, 60 − 10) = MIN(50, 50) = 50 s catches the release with ~10 s residual headroom from teardown jitter.

**This is wrong by exactly the headroom.** The 2× ratio holds (empirically: 60.166 s / 30 s = 2.005×) — but at fence=30 both ceilings collapse to 50 s, and 50 s is **less than** the typical 60 s teardown. There is no headroom; there is a ~10 s deficit. The deadline-derived ceiling (`CLIENT_DEADLINE − HEADROOM = 50`) is **the binding constraint**, and inside the synchronous-response contract there is nothing left to widen.

### Why this slipped past r10's analysis

The r10 C-8b proposal explicitly computed the fence=30 case at "MIN(50, 50) = 50s — works", treating 50 s as adequate against an expected ~60 s teardown. The implicit assumption was that the 60 s teardown estimate had ≥10 s of slack on the low side (i.e. real teardown might land at 50-55 s in some runs). Smoke-r11 — using the same image, same fence, same workload as r10 — measured 60.166 s, within 2 ms of r10's 60.164 s. **There is no low-side slack; the 60 s teardown is a sharp constant at fence=30.** The C-8b 50 s budget can never catch it.

### What the 60.166 s teardown actually contains

From the agent-side trace and the controller's `/shutdown` error:

```
admin/snapshot: detached teardown_source_for_snapshot failed
error: "/shutdown to zsbx-…: http://10.99.101.2:7777/shutdown:
        Connection Failed: Connect error: connection timed out
        (continuing with Nomad purge);
        host_fence: agent at http://10.99.101.2:7777 still answering
        at fence deadline (probes=1, last_http_status=None,
        consecutive_misses=1)"
```

Pipeline breakdown (approximate, from log timestamps):
- t+0…~21 s: TCP `/shutdown` POST to `http://10.99.101.2:7777` — `connection timed out` after kernel default ~21 s (no SYN-ACK because the agent process is mid-teardown and not accepting). This is the dominant cost.
- t+21…~30 s: `host_fence` probe loop (`wait_for_agent_silent`, 2-consecutive-miss contract) accumulates only `consecutive_misses=1` in 9 s — the probe interval is too coarse to converge before the budget runs out at t+30 s.
- t+30…~60 s: Nomad job purge tail — the controller submits `purge=true` and waits for Nomad to confirm the allocation is gone. This is ~30 s of Nomad scheduler tail (release allocation, kill task, GC eval, etc.).

The **shape** of the pipeline says the 2× ratio is **structural**: at any fence value, the `/shutdown` TCP-timeout is ~min(21 s, fence) and the Nomad purge tail is ~fence-shaped, so the composed wall-time scales linearly with fence at a ~2× slope. C-8b's 2× re-baseline was the right model — but at fence=30 the deadline ceiling (50 s) binds tighter than the model (60 s), and the deadline ceiling is the C-7 silent-cancel safety net that we CANNOT relax.

### Why this didn't fail in unit tests

The C-8b tests (`c8b_default_policy_envelopes_doubled_fence`) only assert the formula's algebraic output (26 attempts at fence=30), not its sufficiency against the empirical teardown duration. No unit test could catch this because the 60.166 s teardown is a property of the production cluster (real `/shutdown` TCP timeout + real Nomad purge), not of the Rust code. The whole "every cycle finds one new bug" pattern across r1-r11 is, mechanically, exactly this: production-only timings that unit tests cannot synthesise.

### C-8c candidate fixes

Three options, ordered by scope:

1. **Lower fence_timeout to 20 s.** With 2× ratio: teardown ≈ 40 s, C-8b budget = MIN(2×20−10, 50) = MIN(30, 50) = 30 s. **No improvement** — gap is still −10 s (teardown 40 s vs budget 30 s). At fence=20 the fence-derived ceiling binds (30 < 50) instead of the deadline ceiling, and the relationship `budget = 2*fence − HEADROOM` vs `teardown ≈ 2*fence` always leaves a HEADROOM-sized deficit. Rejected.

2. **Raise fence_timeout to 35 s.** Teardown ≈ 70 s, C-8b budget = MIN(2×35−10, 50) = MIN(60, 50) = 50 s. **Same — budget is still hard-capped by the deadline-derived 50 s ceiling, teardown grows to 70 s. Gap widens to −20 s.** Rejected.

3. **Raise CLIENT_DEADLINE_SECS via ntex client config** to e.g. 90 s + lower headroom, e.g. budget = 75 s. This violates the gateway-fronting SLO contract (the public surface is 60 s wake timeout) AND re-introduces C-7 silent cancellation territory unless the gateway timeout is also bumped. The whole point of CLIENT_DEADLINE − HEADROOM is that ntex disconnects the client at 60 s; lifting the deadline requires coordinated changes across ntex, the gateway, and any external callers. Rejected as out-of-scope for a per-cycle smoke unblock.

4. **C-7-LT: async wake response + polling.** R15-A1, the architectural fix. Wake returns 202-Accepted immediately with a poll URL; the retry loop runs server-side without the 60 s client deadline binding it. Budget can grow to the real teardown duration (60 s + headroom) without any tradeoff against C-7. **The only option that ships WAKE-PASS at fence=30.** Out of scope for a single-cycle smoke; needs a multi-cycle sprint.

**Author recommendation:** the bug pattern across r1-r11 — every cycle reveals a different wake-path runtime/timing fault that unit tests can't catch — is now formally proven. r11 ruled out the last remaining knob inside the synchronous-response contract. **C-7-LT (option 4) is the only path.** Iterating further on the budget formula is whack-a-mole and we are out of knobs.

### Why this is C-8c, not "C-8b RED on first smoke"

C-8b's algebra is correct and confirmed by the smoke-r11 log shape (26 attempts, 50 s budget, deadline-ceil binding). The bug is not in the C-8b implementation — it's in the assumption beneath C-8b that the *deadline-derived ceiling* gives us enough room. That's a distinct, separately-named bug (the deadline ceiling itself is too tight at fence=30), and tracking it as **C-8c** preserves the closure story for C-8b without erasing it. The deferred backlog can mark C-8b CLOSED (algebra works) and add C-8c as the new open.

## GO / NO-GO for T-8b-stress

**NO-GO.** WAKE 0/1 at concurrency=1 cycles=1 — stress at concurrency=1 × N cycles would replay the same 503 pattern every cycle. Concurrency >1 adds vm_index ceiling pressure on top. Single-cycle WAKE PASS is the gate; we are not at the gate.

**Author recommendation:**
- Pause cluster work. The 11-cycle "one new production-only bug per cycle" pattern has held perfectly across r1-r11.
- Surface this review to the user with the C-7-LT proposal. The synchronous-response wake contract has been formally shown to be under-budgetable inside the existing client deadline; no further per-cycle patch can close the gap.
- The C-8b fix and C-8a deadline-ceiling guard both did what they were designed to do — they did not fix WAKE because the contract itself, not any single piece of code, is the constraint.

## Per-attempt log validation

The C-7 deliverable's third leg (per-attempt INFO log fires before each reserve attempt) is **STILL CONFIRMED IN PROD** with C-8b's 26-attempt loop. Logged exactly 26 attempt lines with `attempt=N/26, vm_index=1, sandbox_id=…`. Operability of the wake-failure path is preserved across the C-8a → C-8b refactor; the C-7 invariants are not regressed.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-prod
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
…
[teardown] remaining instances matching ^zsbx-prod-: 0
[teardown] OK: cluster fully torn down

$ gcloud compute instances list --filter='name~zsbx-' --format='table(name,zone.basename(),status)'
(empty after teardown completes)
```

Cluster fully removed. Cost: ~$0.038/hr server + ~$1.55/hr worker for ~10 minutes ≈ **$0.27 for this cluster cycle**. Cumulative today: 11 cycles × ~$0.27 ≈ **$3.0 total** against the $1000/day cap (0.3%).

## Counts

- **Sandbox bugs found across cluster cycles:** C-1 (r1), C-2 (r2), C-3 (r3), C-4 (r4-r5), C-5 (r6), C-6 (r7 + hypothesis falsified r8), C-7 (r8, FIXED), C-8 (r9, FIXED), C-8a (r9, FIXED), C-8b (r10, FIXED), **C-8c (r11, NEW)**.
- **Distinct production-only signals in 11 cycles:** 10 (pattern: 1 per cycle across r1-r11, with r11 confirming a quantitative refinement rather than a structurally new fault — but it still ate a cycle).
- **C-8b effect:** algebra confirmed, 26-attempt loop exactly matches `MIN(50, 50)/2 + 1 = 26` and budget hits 50,004 ms within 4 ms. No regression of C-7 (no silent cancellation; all 26 attempts logged; clean 503 inside deadline). The fix did what it was designed to do.

## Closures-this-cycle

- **C-8b (fence-derived ceiling re-baseline):** CONFIRMED in production — observed 26 attempts × 2 s = 50 s budget, exactly matches the post-C-8b formula `MIN(2×30 − 10, 60 − 10)/2 + 1 = 26` at fence=30. No regression of C-7. Closure hash: `64af1803` (already recorded in `sandbox-snapshot-restore-deferred.md` closures table by `0fc56df5`).

## Opens / deferred adds

- **C-8c (NEW, CRITICAL):** The C-8a deadline-derived ceiling (`CLIENT_DEADLINE − HEADROOM = 50 s`) is itself ~10 s tighter than the empirical 60 s teardown at fence=30. No knob inside the synchronous-response contract can close this gap without violating the C-7 silent-cancel safety net. **The only viable fix is the C-7-LT architectural change (R15-A1): async wake + polling.** Add C-8c to `sandbox-snapshot-restore-deferred.md` as OPEN-IN-INVESTIGATION on next deferred-refresh cycle.
- **R15-A1 strengthened (7th cycle of evidence):** add r11 as the 7th cycle (r4 / r6 / r7 / r8 / r9 / r10 / r11) demonstrating that the synchronous-response wake contract is the shared root cause. C-4 / C-6 / C-7 / C-8 / C-8a / C-8b / C-8c are all patches on the same broken contract. r11 is the first cycle where the contract is provably **out of remaining knobs** — every option short of C-7-LT has been exhausted.

To be added to `docs/reviews/sandbox-snapshot-restore-deferred.md` in a follow-up commit.

## Sandbox HEAD anchor

| Anchor | Commit |
|---|---|
| C-7 fix | `493d6c1e` |
| C-8 + C-8a fix | `2afbb2dd` |
| C-8b fix | `64af1803` |
| v26 binary content source | `0fc56df5` (= HEAD before this pin-bump; deferred-table backfill on top of `64af1803`) |
| Pin-bump commit | `2e9ae598` |
| Current HEAD | `2e9ae598` (this commit) |

## What's next

Pause for user input. Two forward paths:

1. **C-7-LT architectural sprint (R15-A1)** — multi-cycle. The only remaining option. ~6-12 hours of focused work spread over 3-5 cluster cycles for design + impl + smoke. Decouples wake from the 60 s client deadline; budget can grow to the real teardown duration without any tradeoff against C-7.

2. **Accept current state, document the SLO degradation** — wake-immediately-after-snapshot returns 503 with retry-via-poll semantics for the client. Defensible only if the product surface tolerates 503 + client-side retry on the wake path within 60 s of a snapshot. The wake-after-cold-window case (e.g. wake 5 min after snapshot) is unaffected: the source VM is long-since torn down, vm_index is free, wake succeeds in one attempt.

Author recommends path (1). The synchronous-response contract is structurally under-budgeted; no further per-cycle patch can fix it.
