# T-8b-smoke-retry-r6 cluster validation — 2026-05-25 r6 (controller v21 / C-4 fix, 1+1 fleet)

**Sprint:** T-8b-smoke-retry-r6 — sixth 1-worker smoke after C-4 (wake-vs-source-teardown vm_index race, bounded retry) landed in controller v21.
**Worktree (sandbox):** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` @ `1a838882` (v20 → v21 pin bump; parents `468200ca` (C-4 closure doc) + `b2892368` (C-4 code) + `0b05a4c9` + `af4678ac` + `0053e8b6` + `d7740b03` (C-5 worker scope) + `5399b6b7`).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v21`, SHA `06f3f53b9e63eb8b605dd67344acae29537cb820fd295ab4f78eefd7f443fb3d`, size 16486864 B, interp `/lib64/ld-linux-x86-64.so.2`.
**Driver:** unchanged — `gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v4` (C-4 + C-5 are controller/script-side).
**Verdict:** **FAIL on WAKE — C-4 retry budget shape is correct (no immediate 503) but WAKE never returns to the client.** The wake handler advances the row to `restoring` and then goes silent. No `restore: post-store.get staged files`, no `admin/wake: handler failed`, no rollback CAS — handler hangs somewhere past the vm_index reserve, no Nomad job ever submitted, row left wedged at `status=restoring, generation=3` after the 60 s client-side timeout.
**Recommendation:** **NO-GO for T-8b-stress. PAUSE smoke loop and ESCALATE to user.** This is the 6th iteration / 5th distinct new bug across the T-8b series. Per sprint brief: "if NEW bug surfaces, document + recommend pause for user input (this would be 5 sequential new bugs across 6 smoke cycles)" — we have hit that trigger.

## TL;DR

C-4 (the immediate 503 from `reserve_vm_index` racing the detached source-teardown) is **no longer observable** in v21. The wake call does not return `vm_index_unavailable` within 76 ms of the snapshot response. Instead a new, deeper failure mode surfaces:

- Wake CAS to `restoring` succeeds (gen 2 → 3 confirmed in pg).
- `reserve_vm_index_with_retry` enters its 60 × 2 s polling loop (no log emitted on first attempt — only emits on success-after-retry-N or full exhaustion).
- The detached source-teardown completes at `02:26:16` (host_fence cleared, vm_index released after 90.2 s — exactly the budget the retry policy was sized for).
- After teardown release, **no further wake-handler log lines appear in `/var/log/zeroship-sandbox.log` for the test's sandbox**. No `restore: post-store.get staged files`. No CAS to `running`. No rollback. No `admin/wake: handler failed`. No Nomad job submission (Nomad `job status` reports `No running jobs`).
- The stress client times out at 60 s (its hardcoded wake timeout). 3+ minutes later, the controller is still up and serving but the sandbox row is wedged at `status=restoring, generation=3, vm_index=1`. A subsequent `POST /admin/sandboxes/.../wake` returns `409 state_mismatch` ("wake requires snapshotted; current=restoring") — confirming the row is stuck, not lost.

Smoke result: **CREATE 1/1, SNAPSHOT 1/1, WAKE 0/1** (and POST-WAKE EXEC 0/0, STOP 0/1 — failures cascade from wake).

This is the 5th distinct bug in 6 cycles. Per sprint guard, halt the loop.

## Pre-cluster checks

| Check | Status |
|---|---|
| Sandbox HEAD at `468200ca` (C-4 doc closure) | OK — local HEAD `468200ca`, all required parents present (`b2892368` C-4 code, `0b05a4c9` R13-API1/R10-API2 doc, `af4678ac` ExecBody pub(crate), `0053e8b6` C-5 doc, `d7740b03` worker storage-rw, plus 5+ earlier in chain). |
| Controller v21 Docker build clean | OK — `cargo build --release -p zeroship-sandbox` finished 31.46 s, one warning (`seal_filename_for_str` unused — pre-existing dead-code, not introduced this sprint). |
| Portable interp `/lib64/ld-linux-x86-64.so.2` | OK — `readelf -p .interp` confirmed. |
| Binary SHA recorded | OK — `06f3f53b9e63eb8b605dd67344acae29537cb820fd295ab4f78eefd7f443fb3d`, 16486864 bytes. |
| Upload to GCS | OK — `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v21` confirmed via `gcloud storage ls -L`. |
| Script pin bump v20 → v21 | OK — `provision-gcp-cluster.sh` CONTROLLER_OBJECT default + doc header; `gcp-worker-startup.sh` example comment; `gcp-server-startup.sh` example comment. |
| Lint clean | OK — `lint.sh: OK — 7 script(s) clean at --severity=error`. |
| Pin-bump commit | OK — `1a838882` "sandbox/scripts: bump controller v20 -> v21 (T-8b-ctl-v21-upload)". |
| Budget ledger | OK — `/tmp/zsbx-cluster-budget-20260524` is now 6 lines (provision r6 appended). |

## Cluster provision

```
[provision] project=suger-dev region=asia-northeast3 zone=asia-northeast3-a prefix=zsbx-prod
[provision] servers=1 × n2-standard-4 | workers=1 × n2-standard-32
[provision] artifact-bucket=suger-dev-zsbx-artifacts controller=zeroship-sandbox.snapshot-v21
[provision] sentinel hit on zsbx-prod-server-1 (60s)
[provision] sentinel hit on zsbx-prod-worker-1 (15s)
[provision] cluster up.
zsbx-prod-server-1  asia-northeast3-a  n2-standard-4   10.178.0.10  RUNNING
zsbx-prod-worker-1  asia-northeast3-a  n2-standard-32  10.178.0.19  RUNNING
```

## Validation 1 — `/livez` + Nomad driver health

```
livez_status=200
Driver    Detected  Healthy  Message
ch        true      true     ready                               2026-05-24T02:22:50Z
exec      true      true     Healthy                             2026-05-24T02:22:50Z
qemu      true      true     Healthy                             2026-05-24T02:22:50Z
raw_exec  true      true     Healthy                             2026-05-24T02:22:50Z
```

`ch  true  true` confirmed — driver v4 picked up by the worker (C-5 worker scope grant
verified on the wire: the worker fetched the controller binary from GCS without
permission errors, and the `ch` plugin loaded cleanly).

## Validation 2 — single create/snap/wake

```bash
sudo python3 /opt/stress/snapshot_stress.py --concurrency 1 --cycles 1
```

```
# elapsed: 72.9s
=== snapshot-stress (N=1) ===
CREATE OK: 1/1
  create p50/p95/p99/max: 6456 / 6456 / 6456 / 6456 ms
SNAPSHOT OK: 1/1
  snapshot p50/p95/p99/max: 6356 / 6356 / 6356 / 6356 ms
WAKE OK: 0/1
POST-WAKE EXEC OK: 0/0
STOP OK: 0/1
FAILED WAKES: 1
  [1x] code=0: TimeoutError: timed out
```

| Stage | p50 | Notes |
|---|---|---|
| CREATE | 6456 ms | Healthy; consistent with r5's 4.6 s create p50. |
| SNAPSHOT | 6356 ms | Healthy. C-3 still fixed (no panic), C-4 retry didn't fire on snapshot path. Artifact `30beb8cbe4ce8616785e2ae55b99e27125779ef660e74ad585cbc0591b6126c0`, 1073847240 B. |
| WAKE | timeout (60060 ms) | **FAIL.** Client-side 60 s timeout; controller never returned. |
| POST-WAKE EXEC | n/a | Cascades from wake failure. |
| STOP | n/a | Cascades; stop hit `lost-leadership` because row was already advanced past gen 0. |

## Root cause walk

### Timeline (UTC, from `/var/log/zeroship-sandbox.log` + controller logs)

```
02:24:33  vm_index 1 allocated for create
02:24:34  create alloc running (1.0 s)
02:24:39  create agent_ready (5.4 s) — sandbox CREATED
02:24:46  sandbox/nomad-ch stop: started — snapshot-detached teardown begins
            (sandbox_id 019e57cc-63d3-72e2-9176-42cb5c1dc1f2, vm_index=1)
~02:24:53 stress client issues POST /admin/.../wake (immediately after snapshot resp)
            ── controller-side: wake handler CAS to restoring (gen 2→3 in pg)
            ── controller-side: reserve_vm_index_with_retry enters loop;
               first attempt fails (vm_index 1 held by source teardown),
               sleeps 2 s, retries (NO LOG — only logs on success-after-N or exhaustion)
02:25:46  stop pre-flight WARN (sandbox_id sbx_033M1d8sNTwlK7rxS5syZe):
            expected_generation=0 observed_generation=3 — this is the stress STOP call
            arriving 60 s after wake started, after the client's wake-timeout fired
02:26:16  host_fence cleared (60.158 s)
02:26:16  sandbox/nomad-ch vm_index released  vm_index=1
02:26:16  stop_preserving_state cleanup (host_dir + persist preserved for wake — correct)
02:26:16  stop: complete  elapsed_ms=90191
02:26:16  ERROR admin/snapshot: detached teardown_source_for_snapshot failed
            (/shutdown 60 s timeout — non-fatal, orphan-prune reclaims)
   ←──── from here on, NO further log lines for the wake handler ────→
02:29:52  (admin-API query for state — controller still responsive on /livez and /admin)
```

### What the wake handler did vs. what it did NOT do

**DID:**
- Entered `wake_sandbox` in `crates/sandbox/src/admin_handlers.rs:1351`.
- Passed `admin_check`, feature gate, sandbox_id parse, wiring check.
- Called `restore_handler::restore_sandbox` (`crates/sandbox/src/restore_handler.rs:317`).
- Inside: read row, status check passed, captured `g0=2`.
- Read snapshot row metadata.
- CAS `Snapshotted → Restoring` succeeded (gen 2→3 — confirmed by pg row).
- Entered `do_restore_inner` (`restore_handler.rs:494`).
- Called `reserve_vm_index_with_retry(backend.as_ref(), sandbox_id, snap.vm_index)`
  at `restore_handler.rs:517`.

**DID NOT:**
- Emit any log line during the retry loop's intermediate failures
  (`reserve_vm_index_with_retry` only logs `"vm_index reserved after retry"`
  on success when `attempt > 1`, or `"vm_index reserve exhausted retry budget"`
  on full failure — never logs per-attempt). **Neither log fired.**
- Emit the post-`store.get` log at `restore_handler.rs:579-584`
  (`"restore: post-store.get staged files"`).
- Submit a Nomad job (Nomad `job status` reports `No running jobs` 3 min after).
- CAS back to `Snapshotted` on failure (rollback path, `restore_handler.rs:417`).
- CAS to `Running` on success (`restore_handler.rs:752`).
- Emit `admin/wake: handler failed` (only fires if `restore_sandbox` returns `Err`).

### Hypotheses for the silent stall

The wake handler is between "entered reserve_vm_index_with_retry" and "emitted any
of its downstream tracing", with the row stuck at `restoring`. Three plausible
mechanisms (all unverified — call sites need instrumentation):

1. **C-6a (most likely): reserve_vm_index_with_retry succeeded silently after the
   teardown released (02:26:16), but the next sync step blocked indefinitely.**
   Candidate blocking calls in order after line 517:
   - `std::fs::remove_dir_all` / `std::fs::create_dir_all` on `alloc_dir`
     (lines 519-528). Sync, runs on the ntex worker — would block the runtime
     if the path is on a slow/hung filesystem, but `/var/zeroship/ch/...` is local.
   - `store.get(...)` (the L1/GCS-backed `SnapshotStore::get`). This is the
     prime suspect: a sync trait method called from async context. If the L1
     hit path is broken (e.g., file present but checksum-verification spinning,
     or AEAD-disabled tier-shape pulling from GCS with a hung HTTP client), the
     ntex worker would park. The pre-existing
     `snapshot_store: AEAD DISABLED — guest RAM plaintext on disk + GCS` ERROR
     at controller startup says we are running without AEAD in this cluster
     (kek env unset), so the read path is the un-AEAD L1 fast-path; if L1
     filesystem is OK this should not hang, but the path is exercised here for
     the first time on cluster.

2. **C-6b: reserve_vm_index_with_retry exhausted, the bounded retry log fired
   but did not flush before journald rotation/buffering** — would require
   log loss in a 200-line file with no rotation pressure; very unlikely.

3. **C-6c: a CAS-to-Restoring side-effect locked something the retry then
   awaited indefinitely** — speculative; no obvious candidate in the code path.

Hypothesis 1a (sync-from-async on `store.get`) is the strongest because (a)
that's the next bounded-IO operation after vm_index reserve, (b) the missing
log is literally on the line immediately after store.get returns, and (c) the
nomad job is never submitted because submit_restore_job lives downstream of
store.get. This pattern matches R10-C2's earlier finding (rollback's
`teardown_restore` blocking ntex workers), suggesting another spot where a
sync `SnapshotStore`/`RestoreBackend` call needs `spawn_blocking`.

**Action item for next sprint:** instrument the wake handler. Add INFO log
on `wake_sandbox` entry, on each phase boundary in `do_restore_inner`
(post-reserve, post-store.get-call-issued, post-store.get-returned, post-submit,
post-livez), and verify what stage is blocking. Optionally wrap `store.get`
and any other sync I/O in `compio::runtime::spawn_blocking`.

## Cluster review: pass/fail per pre-req

| Pre-req | Result |
|---|---|
| HEAD includes `468200ca`, `b2892368`, `0b05a4c9`, `af4678ac`, `0053e8b6`, `d7740b03` | OK |
| v21 binary built portable | OK (`/lib64/ld-linux-x86-64.so.2`) |
| v21 binary uploaded to GCS | OK |
| Pin bumped + lint clean + committed | OK (`1a838882`) |
| Budget ledger has r6 line | OK (6/day) |
| Cluster provisions cleanly | OK |
| /livez 200 | OK |
| ch driver healthy on worker | OK |
| CREATE 1/1 | **OK** |
| SNAPSHOT 1/1 | **OK** (C-3 still fixed) |
| WAKE 1/1 | **FAIL** (new bug C-6: handler silent past vm_index reserve) |
| Teardown clean | OK (0 instances remaining) |

## Bug tally (T-8b smoke series)

| Cycle | Driver/Controller fixed | New bug surfaced |
|---|---|---|
| r1 | bash wrapper, install gate, plugin handshake | C-1 (driver `--config` arg form) |
| r2 | C-1 | C-2 (CH disk path not on disk) |
| r3 | (re-test of C-2) | — (re-confirmed C-2) |
| r4 | C-2 (rootfs materialization + pre-flight stat) | C-3 (snapshot-store `spawn_blocking` panic) |
| r5 | C-3 (`std::thread::Builder` for L2 detach) | C-4 (wake/teardown vm_index race) + C-5 (worker missing devstorage.read_write) |
| r6 | C-4 (60×2 s reserve_vm_index retry) + C-5 (worker IAM scope) | **C-6 (wake handler silent stall past vm_index reserve; row wedged at restoring)** |

**5 distinct new bugs across 6 smoke cycles. Sprint brief trigger reached
("5 sequential new bugs across 6 smoke cycles"). Halt smoke loop and escalate.**

## Cost (best-effort, this run)

- 1× n2-standard-4 (server) + 1× n2-standard-32 (worker, nested-virt)
- Up-time: ~10 minutes (provision ~3 min + smoke ~3 min + investigation ~3 min + teardown ~1 min)
- Approx GCE on-demand asia-northeast3 hourly: n2-standard-4 ≈ \$0.22/h, n2-standard-32 ≈ \$1.76/h ⇒ ~10 min ≈ \$0.33 on-demand
- Plus GCS list/get for controller pull (negligible), 2 ephemeral IPs (negligible)
- **Estimated total: <\$0.50** for this iteration. Cumulative T-8b series cost is the operative number; this run is in line with r1–r5.

## Teardown verification

```
[teardown] OK: cluster fully torn down
gcloud compute instances list --filter='name~"^zsbx-"' --format='value(name)' → (empty)
```

Zero residual `zsbx-*` instances. No leaked IPs (server-1-ip released).

## Recommendation

**NO-GO for T-8b-stress.** Do NOT escalate to 3-worker / concurrency-N stress
until C-6 is rooted out. The 1-worker smoke is the gate, and it is failing
on the wake stage in a new way.

**ESCALATE to user.** Per sprint brief: "if NEW bug surfaces, document +
recommend pause for user input (this would be 5 sequential new bugs across
6 smoke cycles)" — that condition is met. Recommend the user:

1. Decide whether to authorize a focused C-6 spike (add tracing on each wake-
   handler phase, run a single locally-reproducible cluster smoke with the
   tracing in place to identify exactly which sync call blocks), or
2. Step back to root-cause review of the snapshot/restore handler design
   (the recurring pattern across C-3 / C-4 / C-6 is sync-from-async I/O
   in handler hot paths — a more systemic fix may be cheaper than another
   spot patch).

The cluster review chain (r1 → r6) is consistent: each iteration uncovers
exactly one new failure mode at the next-deeper layer of the wake/restore
flow. The unknowns are getting smaller per cycle, but the cycle cost is
real.

## Artifacts

- Controller binary: `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v21` (SHA `06f3f53b…fb3d`)
- Local controller log capture: `/tmp/zsbx-ctl-r6.log` (214 lines)
- Provision log: `/tmp/t8b-smoke-r6-provision.log`
- Stress log: `/tmp/t8b-smoke-r6-stress.log`
- Teardown log: `/tmp/t8b-smoke-r6-teardown.log`
- Wedged row (post-smoke, in pg before teardown):
  `status=restoring, generation=3, vm_index=1, host_id=hst_033M1aYUvLm0R4zMPXOApm`
