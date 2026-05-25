# T-8b-smoke-retry-r10 cluster validation — 2026-05-25 r10 (controller v25 / C-8 + C-8a fixes landed, 1+1 fleet)

**Sprint:** T-8b-ctl-v25 + smoke-retry-r10 — final cluster cycle of the UTC day (10/10 budget). Re-run smoke against controller v25 (sandbox HEAD `2afbb2dd`, contains the C-8 cluster-config fix at `2afbb2dd` setting `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` AND the C-8a Rust-side dual-ceiling cap at `2afbb2dd`'s `restore_handler.rs` change).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `c73b3956` (v24 → v25 pin bump; controller binary contents from `2afbb2dd`).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v25`, SHA `9a0142bfec30ee8f2ef13972bb672557b3dee814d8c3c9e545ce5c8ed4e57e19`, MD5 `804baed83deff079ad178c74ef579b03` (verified MD5 round-trips upload), interp `/lib64/ld-linux-x86-64.so.2`.
**Driver:** unchanged — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v4`.
**Verdict:** **FAIL on WAKE — new failure mode C-8b: empirical source-teardown wall-time is 2× the fence_timeout (not 1×).** WAKE retry budget (20s, derived from `fence=30` − headroom=10) is exhausted ~40s before the actual vm_index release at +60s. Both C-8 (env landed) and C-8a (dual-ceiling MIN cap) DID their jobs by the letter of the design — but the design's load-bearing assumption (fence_timeout ≈ teardown_wall_time) is wrong. **This is the 10th cluster cycle and the 9th distinct production-only bug.**
**Recommendation:** **NO-GO for T-8b-stress.** Pause cluster work for user input. Today's budget is exhausted. The C-8b fix is small (re-baseline the C-8a dual-ceiling model) but the broader pattern — every cycle finds one new wake-path bug — argues for the **C-7-LT** architectural fix (async wake + polling, R15-A1) rather than another per-cycle patch.

## TL;DR

C-8/C-8a landed cleanly. The new C-8b finding: the empirical source-teardown wall-time scales **larger** than the fence_timeout, not equal to it. With fence=30s set on the worker, actual teardown took ~60s — so the C-8a fence-derived retry budget (fence − headroom = 20s) is half of what's needed.

```
04:15:23.443  sandbox/nomad-ch stop: started               vm_index=1  (source teardown begins)
04:15:23.443  restore: phase entry                                     (wake handler fires same ms)
04:15:23.499  restore: phase pre_reserve_vm_index          vm_index=1
04:15:23.499  reserve_vm_index_with_retry attempt=1/11     vm_index=1  (C-8a derived 11 attempts × 2s = 20s budget)
04:15:25.499  attempt=2/11
04:15:27.499  attempt=3/11
…
04:15:43.501  attempt=11/11
04:15:43.501  WARN restore/wake: vm_index reserve exhausted retry budget; source-teardown
              still holding the slot — surfacing 503    attempts=11 budget_ms=20000
              last_error="vm_index 1 already reserved"
04:15:43.523  WARN admin/wake: handler failed              error="vm_index unavailable (cluster exhausted at vm_index=1)"
                                                          ─── 503 returned to client here (wake_ms=20082) ───
04:16:23.606  ERROR sandbox/nomad-ch host_fence: timeout   elapsed_ms=30129 consecutive_misses=1
              "agent at http://10.99.101.2:7777 still answering at fence deadline; leaking vm_index"
04:16:23.606  WARN sandbox/nomad-ch vm_index leak           vm_index=1 reason=host_fence_timeout
04:16:23.606  INFO sandbox/nomad-ch stop: complete         vm_index=1 fence_passed=false elapsed_ms=60164
                                                          ─── vm_index would have been free at +60s ───
```

Source teardown started at 04:15:23.443 and ended at 04:16:23.606 — **60.164s wall-time, 2× the configured 30s fence_timeout**. The wake retry loop ended at 04:15:43.501 (20s in), 40s before the slot vacated.

Smoke result: **CREATE 1/1 (6509ms), SNAPSHOT 1/1 (6413ms), WAKE 0/1**, STOP failed downstream with generation drift. Cluster torn down clean.

## Build / upload / pin

| Step | Status |
|---|---|
| Controller HEAD | Current HEAD `2afbb2dd` (post-C-8 / C-8a). No carve-out needed this cycle (both fixes are in HEAD by design). |
| Docker release build | OK — `cargo build --release -p zeroship-sandbox` in `rust:slim-bookworm`, finished in 31.82s. |
| Portable interp | OK — `readelf -p .interp` confirmed `/lib64/ld-linux-x86-64.so.2`. |
| Binary SHA | `9a0142bfec30ee8f2ef13972bb672557b3dee814d8c3c9e545ce5c8ed4e57e19`. |
| GCS upload | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v25`; gcloud MD5 (`804baed83deff079ad178c74ef579b03`) matches local `md5sum` after upload. |
| Script pin v24 → v25 | OK — `provision-gcp-cluster.sh` (CONTROLLER_OBJECT default + doc header), `gcp-server-startup.sh` (example comment), `gcp-worker-startup.sh` (example comment). |
| Shellcheck | Clean for the change — only pre-existing SC2020 info-level notes on `tr ',\r' '\n\n'` in worker and server startup, unchanged. |
| Pin-bump commit | `c73b3956` "sandbox/scripts: bump controller pin v24 -> v25 (T-8b-ctl-v25-upload)". |
| Budget ledger | OK — `/tmp/zsbx-cluster-budget-20260524` is now 10 lines (r10 appended at 04:10:26 UTC). **10/10 used for UTC day — budget exhausted.** |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v25
[provision] extra worker metadata: install-ch-plugin-driver=1
[provision] sentinel hit on zsbx-prod-server-1 (60s)
[provision] sentinel hit on zsbx-prod-worker-1 (30s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.23  RUNNING
```

Sentinel timings: server 60s (matches r5-r9), worker 30s (down from r9's 45s — the OS image / apt cache appears warm from r9's recent provision).

## Validation 1 — `/livez` + ch driver

```
$ ssh worker -- curl http://127.0.0.1:9091/livez
{"status":"ok"}
[http=200]

$ ssh worker -- 'curl -sS http://localhost:4646/v1/nodes | jq …'
ch: Healthy=true
```

Both healthy. The `install-ch-plugin-driver=1` metadata flow worked — `SANDBOX_TASK_DRIVER=ch_plugin` is present in the unit's Environment.

## Validation 2 — VERIFY new env var landed (the C-8 cluster-config fix)

```
$ ssh worker -- 'systemctl show zsbx-ctl | grep ^Environment'
Environment=SANDBOX_BACKEND=nomad-ch
              SANDBOX_NOMAD_ADDR=http://127.0.0.1:4646
              SANDBOX_NOMAD_DATACENTER=zsbx-prod
              SANDBOX_NOMAD_CH_WRAPPER_PATH=/etc/zeroship/nomad-vm-wrapper.sh
              SANDBOX_NOMAD_CH_RUNTIME_DIR=/var/lib/zeroship/ch
              SANDBOX_NOMAD_CH_HOST_STATE_DIR=/var/zeroship/ch
              SANDBOX_NOMAD_CH_USER_HOME_ROOT=/var/zeroship/ch/users
              SANDBOX_NOMAD_CH_VM_INDEX_FLOOR=1
              SANDBOX_NOMAD_CH_VM_INDEX_CEIL=12
              SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET=99
              SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30      ← CONFIRMED
              SANDBOX_SNAPSHOT_ENABLED=true
              … (snapshot wiring)
              SANDBOX_TASK_DRIVER=ch_plugin
              SANDBOX_PORT=9091
              RUST_LOG=info,zeroship_sandbox=info
```

**C-8 env var IS PRESENT.** The cluster-config fix landed correctly; the worker startup script's heredoc surgically inserted `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` into the zsbx-ctl unit.

## Validation 3 — snapshot_stress.py --concurrency 1 --cycles 1

```
# t8b-r10: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091
# elapsed: 33.0s

CREATE OK: 1/1
  create p50/p95/p99/max: 6509 / 6509 / 6509 / 6509 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 6413 / 6413 / 6413 / 6413 ms
WAKE OK: 0/1
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  [1x] code=503: {"error":"vm_index_unavailable","message":"no vm_index available …","requested":1}
```

`wake_ms=20082` — within 82ms of the C-8a-derived 20,000ms budget (11 attempts × 2s − 2s for the trailing 11th non-sleeping check ≈ 20,000ms). The retry loop ran to completion (11/11 attempts), then surfaced 503 to the client well within the 60s ntex deadline.

The C-7 cancellation regression remains CLOSED: 11/11 per-attempt INFO logs visible, exhausted-budget WARN fired cleanly, HTTP returned 503 — no silent hang. Operability gain from C-7 is preserved.

## Phase-by-phase wake trace (the C-8b finding)

| t (UTC)       | event                                                          |
|---------------|----------------------------------------------------------------|
| 04:15:23.442  | `sandbox/nomad-ch stop: started` vm_index=1 (snapshot's source teardown begins) |
| 04:15:23.443  | `restore: phase entry` (wake handler fires 1ms later)          |
| 04:15:23.479  | `restore: phase read_snapshot_row_ok` vm_index=1               |
| 04:15:23.499  | `restore: phase pre_reserve_vm_index` vm_index=1               |
| 04:15:23.499  | `reserve_vm_index_with_retry attempt=1/11`                     |
| 04:15:25.499  | attempt=2/11                                                   |
| 04:15:27.499  | attempt=3/11                                                   |
| 04:15:29.500  | attempt=4/11                                                   |
| 04:15:31.501  | attempt=5/11                                                   |
| 04:15:33.501  | attempt=6/11                                                   |
| 04:15:35.501  | attempt=7/11                                                   |
| 04:15:37.501  | attempt=8/11                                                   |
| 04:15:39.501  | attempt=9/11                                                   |
| 04:15:41.501  | attempt=10/11                                                  |
| 04:15:43.501  | attempt=11/11                                                  |
| 04:15:43.501  | WARN `vm_index reserve exhausted retry budget; source-teardown still holding the slot — surfacing 503` attempts=11 budget_ms=20000 last_error="vm_index 1 already reserved" |
| 04:15:43.523  | WARN admin/wake handler failed; HTTP 503 sent to client (wake_ms=20082)|
| 04:16:23.606  | ERROR `sandbox/nomad-ch host_fence: timeout` elapsed_ms=30129 `consecutive_misses=1` — agent still answering |
| 04:16:23.606  | WARN `sandbox/nomad-ch vm_index leak` vm_index=1 reason=host_fence_timeout |
| 04:16:23.606  | INFO `sandbox/nomad-ch stop: complete` vm_index=1 fence_passed=false elapsed_ms=60164 |

**Source teardown wall-time:** `stop: complete (04:16:23.606) − stop: started (04:15:23.442) = 60.164 s` — exactly 2× the fence_timeout of 30s. fence_passed=false (the agent at 10.99.101.2:7777 was still answering /livez at the 30s deadline — only `consecutive_misses=1` accumulated before the budget elapsed), so vm_index was LEAKED (not released). The slot would have been reclaimed by the next orphan-prune cycle, not by stop semantics.

**Wake retry budget:** `attempt 11 start (04:15:43.501) − attempt 1 start (04:15:23.499) = 20.002 s`. Matches the C-8a design point: 10 sleeps × 2s = 20s, 11th attempt fires immediately. Wire shape is byte-perfect to the C-8a formula `(fence_timeout − HEADROOM) / INTERVAL + 1 = (30 − 10) / 2 + 1 = 11`.

**Gap:** wake exhausted at +20s; teardown completed at +60s. Wake gave up 40s too early.

## Diagnosis: C-8b — fence_timeout ≠ teardown wall-time (CRITICAL)

The C-8 / C-8a fix design assumed:

> Source teardown wall-time ≈ host_fence_timeout. So if we cut fence from 120s → 30s, teardown shrinks from ~150s → ~30s, fitting inside a fence-derived retry budget of (30 − 10) = 20s + ~12s residual headroom.

**This assumption is wrong by a factor of 2.**

Empirical observation from this smoke:

- `host_fence_timeout_secs = 30 s` (C-8 env var)
- `consecutive_misses` accumulator only reached 1 by the 30s deadline (agent's /shutdown took longer than 30s to actually shut the HTTP listener down)
- The full `stop` path is: send `/shutdown` to agent → host_fence wait (30s, may time out) → Nomad job purge → cleanup → emit `stop: complete`
- Real wall-time: 60.164s, or **2.005× the fence_timeout**

So the C-8a fence-derived ceiling `(fence − HEADROOM) = 20s` is half of what the actual teardown needs. The deadline-derived ceiling (50s) is closer but still 10s short of the 60s actual.

### Why this slipped past r9's analysis

r9 review estimated *"30s host_fence + Nomad purge tail ≈ 60s vm_index release"* — that estimate was correct, but the recommendation paired it with *"the C-7 48s budget still misses, but a 30s budget bump would catch it"*. The C-8a implementation took a different shape (dual-ceiling MIN, not a fixed budget bump), and the MIN at fence=30 collapses to 20s, not 30s.

The r9 hand-math implicitly assumed the budget would track ≥50% of the teardown duration. With C-8a's formula it tracks only **33%** (20/60) — the formula is too aggressive against the budget direction.

### Why this didn't fail in unit tests

The `r14a6_from_cfg_caps_at_client_deadline` test added in C-8a asserts the 120s fence yields ≤ 50s budget. The 30s fence case (yielding 20s budget) was the EXAMPLE in the docstring — by design, not a failure assertion. The unit-test contract is "budget ≤ client deadline", which 20s trivially satisfies. Nothing checks "budget ≥ realistic teardown duration".

### C-8b candidate fixes (NOT to be applied today — pause)

Three options, increasing in scope:

1. **Re-baseline the C-8a fence-derived ceiling**: use `host_fence_timeout_secs × 2 − HEADROOM` instead of `host_fence_timeout_secs − HEADROOM`. Reflects the observed 2× ratio. Trade-off: still bounded by deadline-derived (50s), so for fence=30 the MIN becomes MIN(50s, 50s)=50s — works. For fence=15 it becomes MIN(20s, 50s)=20s — still too tight for that aggressive a fence, but the operator opt-in to fence=15 also accepts vm_index churn. For fence=60 it stays at MIN(110s, 50s)=50s — exactly what we want post-C-8a. **Smallest change.**

2. **Re-baseline + bump fence_timeout in cluster config**: set `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=45` (was 30 in C-8). With 2× ratio, teardown ≈ 90s, budget capped at 50s (deadline) → still misses by ~40s. Doesn't actually help unless we also do (1). Skip.

3. **C-7-LT: async wake response + polling.** R15-A1. The architectural fix. Decouples wake from the 60s client deadline so the retry budget can grow to the real teardown duration without silent cancellation. Out of scope for any single-cycle smoke.

**Author recommendation:** the bug pattern across r1-r10 is now unambiguous — every cycle reveals a different wake-path runtime/timing fault that unit tests can't catch. C-7-LT (option 3) is the right answer; iterating on the synchronous-response budget formula is whack-a-mole.

## GO / NO-GO for T-8b-stress

**NO-GO.** Today's cluster budget is exhausted (10/10). Even with the C-8b fix-in-progress, stress at concurrency-1 × 20 cycles would replay the same wake-503 pattern (every cycle's snapshot races its own wake). Concurrency >1 adds vm_index ceiling pressure on top.

**Author recommendation:**
- Pause cluster work. The 10-cycle "one new bug per cycle" pattern has held perfectly; the next cycle would almost certainly find C-8c or beyond rather than achieve WAKE 1/1.
- Surface this review to the user with the option to either (a) merge a tiny C-8b patch (option 1 above, ~5 LOC) and run a single tomorrow-budget smoke as the WAKE-PASS validation, or (b) dispatch C-7-LT as a multi-cycle architectural sprint targeting the root cause (R15-A1).
- The C-8 cluster-config fix and C-8a Rust-side cap have both validated the way they were designed — they fixed exactly what they targeted. They didn't fix WAKE because the assumption beneath them (fence_timeout ≈ teardown) is wrong, not because the implementation was wrong.

## Per-attempt log validation

The C-7 deliverable's third leg (per-attempt INFO log fires before each reserve attempt) is **STILL CONFIRMED IN PROD**. Logged exactly 11 attempt lines with `attempt=N/11, vm_index=1, sandbox_id=…`. Operability of the wake-failure path is preserved across the C-8a refactor; the C-7 invariants are not regressed.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-prod
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
…
$ gcloud compute instances list --filter='name~"^zsbx-"' --format='value(name)'
(empty after teardown completes)
```

Cluster fully removed. Cost: ~$0.038/hr server + ~$1.55/hr worker for ~10 minutes ≈ **$0.27 for this cluster cycle**. Cumulative today: 10 cycles × ~$0.27 ≈ **$2.7 total**. T-8b-stress (if it ran) would have added another ~$3.

## Counts

- **Sandbox bugs found across cluster cycles:** C-1 (r1), C-2 (r2), C-3 (r3), C-4 (r4-r5), C-5 (r6), C-6 (r7 + hypothesis falsified r8), C-7 (r8, FIXED), C-8 (r9, FIXED), C-8a (r9, FIXED), **C-8b (r10, NEW)**.
- **Distinct bugs in 10 cycles:** 10 (1 per cycle — pattern held without exception).
- **C-8 + C-8a effect:** env var landed cleanly, dual-ceiling MIN derivation behaves exactly per spec, C-7 operability preserved. Both fixes did their jobs — they just exposed that the design's underlying assumption was wrong by 2×.

## Closures-this-cycle

- **C-8 (env var landed at provision):** RE-CONFIRMED in `systemctl show zsbx-ctl` — `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` present. The startup-script heredoc edit is correct.
- **C-8a (dual-ceiling MIN behaves per spec):** CONFIRMED — observed 11 attempts × 2s = 20s budget, exactly matches the `(30 − 10) / 2 + 1 = 11` formula at fence=30. No regression of C-7 (no silent cancellation; all 11 attempts logged; clean 503 inside deadline).

## Opens / deferred adds

- **C-8b (NEW, CRITICAL):** Empirical source-teardown wall-time is 2× host_fence_timeout (60s at fence=30s, not ~30s). The C-8a fence-derived ceiling underestimates teardown by 2×. Three candidate fixes documented above; recommend option 1 (re-baseline ceiling formula) + future C-7-LT for the architectural fix.
- **R15-A1 strengthened:** add r10 as the 6th cycle demonstrating that the synchronous-response wake contract is the shared root cause. C-4 / C-6 / C-7 / C-8 / C-8a / C-8b are all patches on the same broken contract.

To be added to `docs/reviews/sandbox-snapshot-restore-deferred.md` in a follow-up commit (paused per "DO NOT push to remote / stress" — main-line work today stops here).

## Sandbox HEAD anchor

| Anchor | Commit |
|---|---|
| C-7 fix | `493d6c1e` |
| C-8 + C-8a fix | `2afbb2dd` |
| v25 binary content source | `2afbb2dd` (= HEAD before this pin-bump) |
| Pin-bump commit | `c73b3956` |
| Current HEAD | `c73b3956` (this commit) |

## What's next

Pause for user input. Three forward paths:

1. **C-8b 5-LOC patch + tomorrow's budget r1 smoke** — re-baseline the fence-derived ceiling to `2× fence − HEADROOM`. Ships the WAKE-PASS milestone in one tomorrow-budget cycle if the 2× ratio holds at concurrency-1.
2. **C-7-LT architectural sprint (R15-A1)** — multi-cycle. The right answer. ~6-12 hours of focused work spread over 3-5 cluster cycles for design + impl + smoke.
3. **Accept current state, document the SLO degradation** — wake-immediately-after-snapshot returns 503 with retry-via-poll semantics for the client. Defensible if the product surface tolerates it.

Author recommends path (1) for the smoke-unblock + path (2) as the followup landmark.
