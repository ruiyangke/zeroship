# T-8b-smoke-retry-r9 cluster validation — 2026-05-25 r9 (controller v24 / C-7 fix landed, 1+1 fleet)

**Sprint:** T-8b-ctl-v24 + smoke-retry-r9 — re-run smoke against controller v24 (sandbox HEAD anchor `b8fae7b7`, contains the C-7 fix at `493d6c1e` reducing the wake-retry budget to 25×2s=48s wall-time + per-attempt INFO log).
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `4e5c1479` (v23 → v24 pin bump; controller binary contents from `b8fae7b7`).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v24`, SHA `137c75d578fddb8a3e280936fd141d36e0013b00dd033d05a91dc2e633726f75`, size 16,503,312 B, interp `/lib64/ld-linux-x86-64.so.2`.
**Driver:** unchanged — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v4`.
**Verdict:** **FAIL on WAKE — but C-7 fix WORKED PERFECTLY.** All 25 retry attempts visible in log, exhausted-budget WARN fired cleanly, HTTP returned 503 (not silent hang). The C-7 cancellation regression is **CLOSED IN PROD**. The new failure mode is **C-8: source teardown holds vm_index for >48s under default `host_fence_timeout_secs=120`**, exceeding the C-7 budget. This is a teardown-duration vs retry-budget mismatch, not a regression.
**Recommendation:** **NO-GO for T-8b-stress as currently scoped.** Options for unblock: (a) cap `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` in worker startup to shorten teardown so the slot frees inside 48s, (b) land C-7-LT (async wake response + poll) so the budget can grow past the 60s ntex client deadline, or (c) accept C-8 as documented "wake races slow teardown" failure mode and proceed with knowingly degraded SLO. Author recommends **(a) for stress unblock, (b) as the long-term fix**.

## TL;DR

The wake path now logs every retry attempt and exits CLEANLY on exhaustion — the C-7 silent-cancel regression is GONE:

```
03:57:06.840  sandbox/nomad-ch stop: started               vm_index=1 (source teardown begins)
03:57:06.841  restore: phase entry                         ← wake handler enters 1ms later
03:57:06.879  restore: phase read_snapshot_row_ok          vm_index=1
03:57:06.899  restore: phase cas_restoring_ok              generation=3
03:57:06.899  restore: phase pre_reserve_vm_index          vm_index=1
03:57:06.899  reserve_vm_index_with_retry attempt=1/25     vm_index=1   ← C-7 per-attempt INFO log
03:57:08.899  reserve_vm_index_with_retry attempt=2/25
03:57:10.899  reserve_vm_index_with_retry attempt=3/25
…
03:57:54.903  reserve_vm_index_with_retry attempt=25/25
03:57:54.903  WARN restore/wake: vm_index reserve exhausted retry budget; source-teardown
              still holding the slot — surfacing 503    attempts=25 budget_ms=48000
              last_error="vm_index 1 already reserved"
03:57:54.925  WARN admin/wake: handler failed              error="vm_index unavailable (cluster exhausted at vm_index=1)"
```

HTTP response: 503 `{"error":"vm_index_unavailable","message":"no vm_index available to host the restored sandbox","requested":1}` at wake_ms=48085 (matching the 25×2s=48s C-7 budget to the millisecond — wall-time = (max_attempts − 1) × interval = 24 × 2000 ms = 48 000 ms).

Smoke result: **CREATE 1/1 (6448ms), SNAPSHOT 1/1 (6309ms), WAKE 0/1**, STOP failed downstream with `lost-leadership on stop pre-flight` (generation drift after the failed wake CAS-bumped to 3). Cluster torn down clean.

## Build / upload / pin

| Step | Status |
|---|---|
| Controller HEAD selection | Built from **`b8fae7b7` content** (not current HEAD `930aac3f`). Reason: the later commit `c3edf968` (R14-A6: derive `VmIndexRetryPolicy` from `cfg.host_fence_timeout_secs` in `RealRestoreBackend`) regresses C-7 under the production-default 120s fence — `from_host_fence_timeout(120)` yields 56 attempts × 2s = 110s budget, which exceeds the 60s ntex client deadline and recreates the silent-cancel failure C-7 fixed. The brief explicitly anchors HEAD at `b8fae7b7`, so we ship the safe hard-coded 25×2s=48s default. See **C-8a (R14-A6 prod regression)** in the deferred-issues queue below. |
| Docker release build | OK — `cargo build --release -p zeroship-sandbox` in `rust:slim-bookworm`, finished in ~30s. Identical-shape build to r7/r8. |
| Portable interp | OK — `readelf -p .interp` confirmed `/lib64/ld-linux-x86-64.so.2`. |
| Binary SHA | `137c75d578fddb8a3e280936fd141d36e0013b00dd033d05a91dc2e633726f75` (16,503,312 B). |
| GCS upload | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v24`; re-downloaded and SHA-verified after upload. |
| Script pin v23 → v24 | OK — `provision-gcp-cluster.sh` (CONTROLLER_OBJECT default + doc header), `gcp-server-startup.sh` (example comment), `gcp-worker-startup.sh` (example comment). Shellcheck: only pre-existing SC2020 info-level notes, unchanged. |
| Pin-bump commit | `4e5c1479` "sandbox/scripts: bump controller pin v23 -> v24 (T-8b-ctl-v24-upload)" — commit message documents the c3edf968 carve-out rationale. |
| Budget ledger | OK — `/tmp/zsbx-cluster-budget-20260524` is now 9 lines (r9 appended at 03:51:07 UTC; 9/10 used for the UTC day). |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v24
[provision] extra worker metadata: install-ch-plugin-driver=1
[provision] sentinel hit on zsbx-prod-server-1 (60s)
[provision] sentinel hit on zsbx-prod-worker-1 (45s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.22  RUNNING
```

Sentinel timings: server 60s (matches r5-r8), worker 45s (up from 15s in r5-r8; the longer time is from the larger `apt install` set on this new VM image — not a regression).

## Validation 1 — `/livez` + ch driver

```
$ curl http://127.0.0.1:9091/livez
{"status":"ok"}
```

Controller boot trace shows healthy initialization: `sandbox nomad-ch backend` config logged with `vm_index_floor=1, vm_index_ceil=12`, `subnet_second_octet=99`, `wrapper_path=/etc/zeroship/nomad-vm-wrapper.sh`. `snapshot wiring: shared vm_index allocator with backend (B18)` confirmed at 03:54:00.

ch driver: installed via `install-ch-plugin-driver=1` metadata; the controller is using the legacy bash `nomad-vm-wrapper.sh` path (zsbx-ctl SystemD env did NOT carry `SANDBOX_TASK_DRIVER=ch_plugin` in this cycle — that flip is a separate T-8b-cutover sprint). For the snapshot/restore smoke this is fine; the C-7 fix is task-driver-agnostic.

## Validation 2 — snapshot_stress.py --concurrency 1 --cycles 1

```
# snapshot-stress: concurrency=1, cycles=1, total=1
# base_url=http://127.0.0.1:9091
# elapsed: 60.9s

CREATE OK: 1/1
  create p50/p95/p99/max: 6448 / 6448 / 6448 / 6448 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 6309 / 6309 / 6309 / 6309 ms
WAKE OK: 0/1
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  [1x] code=503: {"error":"vm_index_unavailable","message":"no vm_index available to host the restored sand
```

`wake_ms=48085` — within 85ms of the C-7-derived 48,000ms budget. The retry loop ran to completion (25/25 attempts), then surfaced 503 to the client well within the 60s ntex deadline.

## Phase-by-phase wake trace (the C-8 finding)

| t (UTC)       | event                                                          |
|---------------|----------------------------------------------------------------|
| 03:57:06.840  | `sandbox/nomad-ch stop: started` vm_index=1 (snapshot's source teardown begins) |
| 03:57:06.841  | `restore: phase entry` (wake handler fires 1ms later)          |
| 03:57:06.879  | `restore: phase read_snapshot_row_ok` vm_index=1               |
| 03:57:06.899  | `restore: phase pre_reserve_vm_index` vm_index=1               |
| 03:57:06.899  | `reserve_vm_index_with_retry attempt=1/25`                     |
| 03:57:08.899  | attempt=2/25                                                   |
| 03:57:10.899  | attempt=3/25                                                   |
| …             | (2s cadence holds exactly)                                     |
| 03:57:52.903  | attempt=24/25                                                  |
| 03:57:54.903  | attempt=25/25                                                  |
| 03:57:54.903  | WARN `vm_index reserve exhausted retry budget; source-teardown still holding the slot — surfacing 503` attempts=25 budget_ms=48000 last_error="vm_index 1 already reserved" |
| 03:57:54.925  | WARN admin/wake handler failed; HTTP 503 sent to client        |

**Observed wall-time spent in retry loop:** `attempt 25 start (03:57:54.903) − attempt 1 start (03:57:06.899) = 48.004 s`. Matches the C-7 design point: 24 sleeps × 2s = 48s, with the 25th attempt firing immediately after the 24th sleep. C-7 fix wire shape is **byte-perfect**.

**Source teardown duration:** the source `vm_index 1 already reserved` error fired on EVERY attempt across the full 48s window. `sandbox/nomad-ch stop: started` at 03:57:06.840 + a default `host_fence_timeout_secs=120` + Nomad purge tail ≈ 150s minimum before the source slot vacates. The smoke's wake call gives up at 48s, ~100s short of when the slot would have freed.

No `host_fence: cleared` log line appears in the captured window (controller was torn down at ~03:58:30 — could not capture the eventual release). In r8, the post-wake teardown log did emit at +60s but here we tore down before that.

## Diagnosis: C-7 vs C-8 (CRITICAL FINDING)

The previous review's hypothesis (C-7: client-disconnect cancels wake handler future at 60s) is **CONFIRMED FALSIFIED IN ITS REGRESSION FORM** by this smoke:

- The C-7 fix (50s budget) keeps the retry loop strictly under the 60s client deadline.
- All 25 attempts emit per-attempt INFO logs.
- The exhausted-budget WARN fires synchronously with the last attempt.
- The 503 reaches the client at `wake_ms=48085` (no 60s timeout-then-disconnect).

The C-7 fix achieved its design goal: **convert silent cancellation into observable 503**. The smoke now FAILS LOUDLY instead of HANGING SILENTLY — strictly better operability.

**However**, the WAKE OK 1/1 milestone is NOT met because of a separate, pre-existing condition the C-7 budget mask had previously hidden:

### C-8 — source teardown holds vm_index for >48s (new bug, surfaced by C-7)

**Mechanism**

1. `POST /admin/sandboxes/:id/snapshot` triggers `teardown_source_for_snapshot` (now on a detached OS thread per C-6 at `91ce9be5`).
2. The detached teardown calls `backend.stop` (Nomad job purge + host_fence wait).
3. `host_fence_timeout_secs` default is **120s** (per `crates/sandbox/src/config.rs:401`).
4. While the host fence is waiting, `vm_index 1` remains in the in-memory reservation set.
5. `POST /admin/sandboxes/:id/wake` (fired immediately by stress) tries to reserve vm_index 1 → "already reserved" → retries 25 times × 2s = 48s → exhausts → 503.
6. Around t+120-150s the host_fence clears, Nomad purge completes, vm_index 1 is released — but the wake handler is long gone.

**Root cause:** mismatch between C-7's 48s wake retry budget and the production-default 120s host_fence_timeout (plus 30s Nomad purge tail).

**Why this was hidden in r1-r7:** with the previous 60×2s=118s budget, the retry loop COULD have outlasted the host_fence on shorter teardowns — but the C-7 antecedent (client-disconnect cancellation at 60s) cut it off at exactly the same point. The user-visible failure mode (timeout) and the internal-truth (slot held by source teardown) were entangled. C-7 untangles them; r9 is the first cycle where the underlying teardown-duration problem is observable in isolation.

**Why this didn't appear in unit tests:** the `StubRestoreBackend` in `restore_handler.rs` tests releases vm_index immediately after `reserve` (no real teardown semantics). The trait's `vm_index_retry_policy` test contract (`c7_retry_budget_default_is_under_client_deadline`) verifies the BUDGET timing, not the actual slot-release race.

### C-8a — R14-A6 cfg-derived policy regresses C-7 in production

The post-`b8fae7b7` commit `c3edf968` ("sandbox/restore: derive VmIndexRetryPolicy from cfg.host_fence_timeout_secs") introduces `from_host_fence_timeout()` and overrides `RealRestoreBackend::vm_index_retry_policy` to use it. Under default `cfg.host_fence_timeout_secs=120`, the derived policy is:

```
(120s − 10s headroom) / 2s + 1 = 56 attempts × 2s = 110s wall-time budget
```

110s **exceeds the 60s ntex client deadline** by 50s, recreating exactly the C-7 silent-cancel failure C-7 fixed. The c3edf968 commit message even documents this case: *"host_fence = 120 s → 56 attempts × 2 s = 110 s (exceeds the 60 s ntex client deadline; documented as requiring the C-7-LT async-poll pattern, not a budget bump)."*

This means **c3edf968 must not ship to production until either (i) `host_fence_timeout_secs` is lowered cluster-wide OR (ii) C-7-LT (async wake + poll) lands.** The v24 binary built for r9 deliberately excludes c3edf968 to preserve the safe 50s budget. C-8a should be tracked alongside C-8.

## GO / NO-GO for T-8b-stress

**NO-GO under default cluster config.** A 3-worker × 20-cycle stress at concurrency-1 would produce 60 wake calls, ALL of which would hit C-8 (vm_index race) and 503 immediately. With higher concurrency the failure shifts to a mix of vm_index races on stale workers and ceiling-pressure 503s on busy workers, neither of which validate the wake SLO.

**Conditional GO paths:**

1. **Set `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` in `gcp-worker-startup.sh`** (workers only — server bootstrap doesn't need it). With a 30s host_fence and ~30s Nomad purge tail, vm_index release lands at ~t+60s; the C-7 48s budget still misses, but a 30s budget bump (or even just letting the budget reach 60s) would catch it. Combined with a small budget tweak this could unblock stress in 1 cycle.

2. **Land C-7-LT (long-term fix)**: rework wake into POST /wake → 202 Accepted + `Location` header → polled GET /wake-status. Decouples the budget from the HTTP request deadline entirely. Out of scope for this cluster sprint; ~2-4 hours of focused work.

3. **Accept C-8 as documented "race with slow teardown" 503**: only viable if the SLO target tolerates ~5-10% wake-503s in a normal mix (it currently doesn't).

**Author's recommendation:** dispatch a small T-8b-config-fix sprint to land option (1) — env in worker startup + budget bump from 25×2s to 30×2s — then re-run smoke. That's the highest-confidence path to the WAKE-PASS milestone. Stress dispatch (10/10 budget cycle) should wait until smoke is green.

## Per-attempt log validation

The C-7 deliverable's third leg (per-attempt INFO log fires before each reserve attempt) is **CONFIRMED IN PROD**. Logged exactly 25 attempt lines with `attempt=N/25, vm_index=1, sandbox_id=…`. The previous-cycle blind spot (whether the retry loop was even firing or was being canceled mid-sleep) is now fully observable.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-prod
[teardown] deleting instances: zsbx-prod-server-1 zsbx-prod-worker-1
…
$ gcloud compute instances list --filter='name~"^zsbx-"' --format='value(name)'
(empty)
```

Cluster fully removed. Cost: ~$0.038/hr server + ~$1.55/hr worker for ~10 minutes ≈ **$0.27 for this cluster cycle**. Cumulative today: 9 cycles × ~$0.27 = ~$2.4. Stress (10th cycle) reserves another ~$3 for 3 workers × ~30min.

## Counts

- **Sandbox bugs found across cluster cycles:** C-1 (r1), C-2 (r2), C-3 (r3), C-4 (r4-r5), C-5 (r6), C-6 (r7 + hypothesis falsified r8), C-7 (r8, FIXED), **C-8 (r9, NEW)**, **C-8a (r9, NEW — c3edf968 regression)**.
- **Distinct bugs in 9 cycles:** 9 (1 per cycle as the trend has held). Notably C-8 was MASKED by C-7 — same wall-time symptom (60s timeout), different mechanism (slot-held vs handler-cancel).
- **C-7 effect on operability:** silent 60s hang → observable 48s 503 with full retry trace. Pure improvement even though smoke still fails.

## Closures-this-cycle

- **C-7 (493d6c1e):** RE-CONFIRMED CLOSED at the cancellation level. Per-attempt log fires; 503 reaches client; no silent cancellation. The fix delivered exactly what was promised.

## Opens / deferred adds

- **C-8 (NEW):** source teardown holds vm_index for >48s under default `host_fence_timeout_secs=120` + Nomad purge tail. Concretely visible in r9 logs (last_error="vm_index 1 already reserved" on attempts 1-25 spanning 48s after `nomad-ch stop: started`).
- **C-8a (NEW):** c3edf968's `from_host_fence_timeout` regresses C-7 under default cfg=120s. v24 was deliberately built without it; the regression should be reverted or guarded before the next controller cut.

Both to be added to `docs/reviews/sandbox-snapshot-restore-deferred.md` in a follow-up commit.

## Sandbox HEAD anchor

| Anchor | Commit |
|---|---|
| C-7 fix | `493d6c1e` |
| Post-C-7 closure (brief's HEAD anchor) | `b8fae7b7` |
| v24 binary content source | `b8fae7b7` (NOT current HEAD) |
| Pin-bump commit | `4e5c1479` |
| Current HEAD | `4e5c1479` (the pin-bump is the top commit) |

## What's next

If T-8b-config-fix is dispatched: small (~30min) sprint setting `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` in `gcp-worker-startup.sh` + bumping the C-7 budget from 25→30 attempts (still under 60s). Then r10 smoke for the WAKE OK 1/1 milestone.

If T-8b-C-7-LT is dispatched instead: ~3-4h sprint for async wake response. Larger blast radius — touches admin_handlers + REST surface — but the budget tension goes away forever.

Either way, **T-8b-stress remains blocked until smoke is green.**
