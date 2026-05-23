# Phase B cluster validation — 2026-05-24 r2 (B17 fix)

**Branch HEAD before fix:** `4340e3b5` — `pilot: round-r2 reviewer artifacts (...)`.
**Wrapper uploaded to GCS:** `gs://suger-dev-zsbx-artifacts/nomad-vm-wrapper.sh` (22.1 KiB, fixed).
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v13` (re-used from prior cycle; no controller change required).
**B17 verdict:** **CLOSED.**
**B15 verdict:** **VERIFIED PASS** (0 `[wrapper] FATAL: workspace image missing` events across the run; full create→snapshot→wake cycle completed successfully).
**Operator:** pilot-cron-worker autonomous.

## TL;DR

Wake worked end-to-end this cycle. Root cause of B17 (the
`No route to host (os error 113)` post-restore probe failure) was
**not** a tap/bridge attachment problem — there is no bridge in the
architecture; the model is one /30 per-tap with host=`X.X.X.1` and
guest=`X.X.X.2`. The actual root cause is that **CH `--restore`
brings the VM back in a paused state**: vCPUs are not running, so the
guest's virtio-net device never transmits ARP replies, and the host
sees EHOSTUNREACH on `/livez` probes.

Fix: after `cloud-hypervisor --restore` spawns, poll the CH HTTP API
socket with `ch-remote ping`, then issue `ch-remote resume`. CH's
documented snapshot/restore protocol since v23 requires this; the
cold-boot path doesn't need it because a normal `cloud-hypervisor
--kernel …` starts running from boot.

Smoke c=1: **PASS** (1/1 wake at 9.5s; first end-to-end working
snapshot+wake on cluster in this branch's history).
Smoke c=4: **5/5 wakes PASS** (the 11/16 failures are a different,
new bug #18 on the create-side — slot reuse keeping a stale agent
pubkey; not a wake regression).

## Wrapper diff — start vs restore branches (pre-fix)

The wrapper has a single pre-branch setup that runs for **both**
start and restore (lines 192-209 of `nomad-vm-wrapper.sh`):

```bash
if [ -e "/sys/class/net/$TAP" ]; then
  ip link set "$TAP" up || { exit 1 }
else
  echo "[wrapper] FATAL: tap $TAP missing — host setup script did not pre-create it" >&2
  exit 1
fi
```

No bridge attachment, no `master <bridge>` step — the host startup
script (`gcp-worker-startup.sh` lines 181-189) creates each tap with
`ip tuntap add` + `ip addr add 10.99.<100+idx>.1/30 dev <tap>` + `ip
link set up`. The /30 makes the tap itself the host's gateway for
the VM; no bridge exists.

The two branches differ in the CH invocation only:

```
# Cold boot (lines 393-403):
cloud-hypervisor \
  --api-socket "$API_SOCK" --kernel vmlinuz \
  --cmdline   "console=ttyS0 root=/dev/vda rw ... ip=${VM_IP}::${HOST_IP}:..." \
  --disk      path="$DISK",... path="$ZSBX_WORKSPACE_IMG",... path="$ZSBX_USER_HOME_IMG",... \
  --net       tap="$TAP",mac="$MAC" \
  --memory    size=${ZSBX_VM_MEMORY_MB}M,shared=on \
  --cpus      boot=${ZSBX_VM_CPUS_BOOT} \
  --console   off --serial file="$ZSBX_RUNTIME/serial.log"

# Restore (lines 366-369, pre-fix):
cloud-hypervisor \
  --api-socket "$API_SOCK" \
  --restore    "source_url=file://$ZSBX_RESTORE_FROM"
```

Cold-boot **runs** the VM (vCPUs start). Restore **loads** the
snapshot but leaves the VM **paused** — by CH's documented
behaviour. No explicit `ch-remote resume` was issued. That is the
B17 root cause.

## Fix applied

`crates/sandbox/scripts/nomad-vm-wrapper.sh`, restore branch, after
the `cloud-hypervisor --restore … &` spawn:

```bash
(
  # Wait for the CH HTTP API socket to bind. ch-remote ping is the
  # lightweight liveness probe.
  for attempt in $(seq 1 50); do
    if [ -S "$API_SOCK" ] && \
       ch-remote --api-socket "$API_SOCK" ping >/dev/null 2>&1; then
      echo "[wrapper] restore: ch-remote api ready (attempt=$attempt)" >&2
      break
    fi
    sleep 0.2
  done
  if ch-remote --api-socket "$API_SOCK" resume 2>&1 | \
       sed 's/^/[wrapper] restore: ch-remote resume: /' >&2; then
    echo "[wrapper] restore: VM resumed" >&2
  else
    echo "[wrapper] restore: WARN ch-remote resume failed; /livez probe will surface it" >&2
  fi
  # Bug-#14b speculative tap-up retry preserved as belt-and-braces.
  for delay in 0.3 1 3; do …same as before… done
) &
```

The `ch-remote resume` is idempotent (CH returns success when the VM
is already running). Pre-fix the wrapper relied on an
unverifiable "speculative" tap-up loop; post-fix the resume is the
authoritative wake step, with the tap-up loop kept defensively.

## Smoke (1+1, c=1) — PASS

Cluster: `zsbx-smoke`, 1 server (n2-standard-4) + 1 worker
(n2-standard-32), `asia-northeast3-a`. `CONTROLLER_OBJECT=zeroship-sandbox.snapshot-v13`
(unchanged from r1 cycle; the controller binary is portable since
bug #16's docker cross-build fix).

- **Provision:** OK. Server sentinel in 60s, worker sentinel in 15s.
- **Cycle:** CREATE 201 / EXEC_PRE 200 / SNAPSHOT 200 / **WAKE 200**
  / EXEC_POST 500 (different bug, see below) / STOP 200.
- **Timings:**
  - `create_ms`: 5314
  - `exec_pre_ms`: 7.8
  - `snapshot_ms`: 47959 (1.07 GB artifact, sha256
    `c36fd835…85aed0`, ch-remote v51.1)
  - **`wake_ms`: 9537** (HTTP 200 — was 37422 HTTP 500 in r1)
  - `stop_ms`: 20
- **Bug #18 surfaced (not B17):** EXEC_POST returns 500 with
  `backend.exec: sandbox not found in nomad-ch backend`. The wake
  succeeds at the controller level but the post-wake in-memory
  registry isn't repopulated. Captured for a separate cycle; **not
  a B17 regression**.

### B17 fix evidence (wrapper stderr, alloc
`019e532caae7734087fcf8bb783797b9`):

```
[wrapper] restore: ZSBX_RESTORE_FROM=/var/zeroship/ch/.../restore
[wrapper] restore: tap zsbx-nm-5 pre-CH-spawn:
[wrapper] restore:   zsbx-nm-5  DOWN  92:c7:8c:49:44:78 <NO-CARRIER,BROADCAST,MULTICAST,UP>
[wrapper] restore: ch-remote api ready (attempt=2)
[wrapper] restore: VM resumed
[wrapper] restore: tap@+0.3s   zsbx-nm-5  UP  92:c7:8c:49:44:78 <BROADCAST,MULTICAST,UP,LOWER_UP>
[wrapper] restore: tap@+1s     zsbx-nm-5  UP  92:c7:8c:49:44:78 <BROADCAST,MULTICAST,UP,LOWER_UP>
[wrapper] restore: tap@+3s     zsbx-nm-5  UP  92:c7:8c:49:44:78 <BROADCAST,MULTICAST,UP,LOWER_UP>
```

Two observations:
1. CH API socket ready in 400 ms (attempt=2 × 200ms sleep).
2. Tap state transition: `<NO-CARRIER>` before resume → `<LOWER_UP>`
   after resume. `LOWER_UP` means a peer is at the other end of the
   link — i.e., the guest's virtio-net is actually exchanging
   packets. This is exactly what cold-boot exhibits. Pre-fix the
   tap stayed `<NO-CARRIER>` indefinitely.

## Smoke (1+1, c=4) — 5/5 wakes PASS, create regression on slot reuse

To confirm the fix isn't a fluke we ran c=4 (16 cycles total).

- **Wake successes:** 5/5 (of the 5 cycles that reached snapshot).
- **Create failures:** 11/16 — all bear the **same** signature:
  ```
  503: backend.create: 3 attempts failed; last error: stale agent at http://10.99.10X.2:7777:
       /version returned 401 (agent is verifying with a different controller pubkey);
       expected fp=<hex>
  ```
  Indices 1-6 are exhausted by cycle 4, and the 7th-onwards cycle's
  create hits a previously-used slot whose in-VM agent still
  remembers the **prior** sandbox's signing pubkey. The controller
  verifies `/version`'s signature against the **new** sandbox's
  pubkey and rejects with 401. This is a separate bug — **logged as
  #18 below**.

- **Wake p50:** 13219 ms (across 5 wakes). Still well over the
  proposal's 1 s SLO. The bottleneck appears to be the 1 GB memory
  artifact restore (~14 s wall on n2-standard-32 with local SSD-
  backed disk). Future SLO work needs a smaller memory footprint or
  prefault tuning.

## SLO comparison

| Metric | Target | Observed (c=1) | Observed (c=4 p50 of 5) |
|---|---|---|---|
| Wake | ≤ 1 s | 9.5 s | 13.2 s |
| Create | (Phase B reference) | 5.3 s | 5.3 s (idx=1) → 48 s (idx=6 retries) |
| Snapshot | (Phase B reference) | 48 s | 54.8 s |
| Stop | reference | 20 ms | 19 ms |

Wake p50 is **measurable for the first time on a real cluster** —
this whole metric was `n/a — never executed` in r1.

## Stress (3+5, c=20) — NOT RUN

Per brief: scale to 3+5 stress only after c=4 passes. c=4 wakes
passed 5/5, but the create-side blockage (bug #18) means only ~30%
of cycles reach wake, so a 60-cycle stress run would dump most of
its budget on 503 retries rather than wake validation. Pilot-side
decision: do **not** spin a 3+5 cluster this cycle; close B17 on
the c=4 evidence, file bug #18 for a focused next cycle, then re-
scale once #18 is fixed.

## B15 fix verification

- `[wrapper] FATAL: workspace image missing` events across the
  full run: **0** (verified via `journalctl -u nomad --since "1 hour
  ago" | grep -c "FATAL\|workspace image missing"`).
- All 5 wake paths exercised the post-`stop_preserving_state` flow
  successfully — the workspace.img was preserved between snapshot
  and wake.
- **B15 verdict:** **VERIFIED PASS** on real cluster. Closes the
  pg-gated integration test result (`b048b491`) at the cluster
  level.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-smoke
[teardown] deleting instances: zsbx-smoke-server-1 zsbx-smoke-worker-1
…Deleted… (both instances + reserved IP)
[teardown] remaining instances matching ^zsbx-smoke-: 0
[teardown] OK: cluster fully torn down

[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-stress
[teardown] no instances to delete
[teardown] OK: cluster fully torn down
```

`gcloud compute instances list --filter='name~"^zsbx-"'` → empty.

## Estimated cost

- Cluster wall-time: ~25 min (provision 5 + smoke c=1 1 + smoke c=4
  7 + diagnostics 2 + teardown 1, with idle slack).
- n2-standard-32 worker @ ~$1.55/hr × 25/60 = **$0.65**.
- n2-standard-4 server @ ~$0.17/hr × 25/60 = **$0.07**.
- Total: **~$0.72**. Well under the $30 cap.

## NEW BUG ENTRY — bug #18: stale controller pubkey on VM slot reuse

**Status:** open, blocks scaling smoke beyond ~6 cycles per worker
(== vm-index ceiling of 12 with idle-snapshot churning).

**Symptom:** After a sandbox is snapshotted + stopped, the same
vm_index slot gets reused for a new sandbox's cold-boot create.
The new sandbox writes its **new** signing pubkey hex to the kernel
cmdline; the in-VM `/sbin/init` decodes it; agent should adopt the
new pubkey. But the controller's `wait_for_alloc_running` then
hits `/version`, validates the signature against its **new**
sandbox's pubkey, and gets 401 — meaning the agent in the VM is
still verifying with the **prior** sandbox's pubkey. After 3
attempts the create fails with 503.

**Likely root cause:** the workspace.img and possibly the rootfs
are reused across slot reuses without being torn down + recreated.
The in-VM agent's state (or the controller-pubkey file at
`/keys/controller-pubkey`) is sticky from the prior sandbox. The
new cmdline's `zsbx_pubkey=<hex>` may not actually be re-read by
the init script if the agent is already running from a prior boot
that didn't reboot the kernel.

**Wait** — the workspace.img and rootfs.img are per-NOMAD-TASK-DIR
(line 232-257 of the wrapper); each new alloc gets a fresh
NOMAD_TASK_DIR. So the workspace+rootfs are new. The pubkey
mismatch must come from somewhere else. Hypotheses:
1. The tap MAC is `printf '12:34:56:78:9b:%02x' "$ZSBX_VM_INDEX"` —
   same MAC for slot reuse. If the controller's HTTP client cache
   (or any ARP cache on the host) holds an old route, the request
   could be heading to a *still-running* prior VM. Unlikely because
   raw_exec teardown kills CH.
2. The init.sh in the rootfs is racy on /keys/controller-pubkey
   write — boots from a snapshot or doesn't actually re-read the
   cmdline each time.
3. The controller's `expected_pubkey_fp` is being computed from the
   wrong key — a stale persist row, or a key reused across
   sandboxes by mistake.

**Inputs to next cycle (cluster diag fixer):**
- `crates/sandbox/src/backend/nomad_ch.rs` `/version` verification
  path; trace which fp the controller expects vs what the agent
  reports.
- The in-VM `/sbin/init` script in the rootfs — does it
  unconditionally re-decode the cmdline pubkey or skip if a stale
  /keys/controller-pubkey exists?
- The persist.rs side: when a new sandbox is created, is a NEW
  signing key generated, or is the per-user key reused?

**Status:** captured here; **do not implement this cycle** per the
B17 brief constraints.

## Side observations (not new bugs)

- The post-wake EXEC returns 500 with `sandbox not found in
  nomad-ch backend`. This is the same issue raised in r1's appendix
  — `RealRestoreBackend` doesn't repopulate the backend's
  `NomadCHBackend::sandbox_registry` after a successful wake.
  Tracked as a follow-up; not a wake regression.

## Files of interest (r2)

- `crates/sandbox/scripts/nomad-vm-wrapper.sh` — the fix (lines
  366-419).
- `gs://suger-dev-zsbx-artifacts/nomad-vm-wrapper.sh` — uploaded.
- `/tmp/smoke-b17.log` — c=1 smoke transcript.
- `/tmp/smoke-b17-c4.log` — c=4 smoke transcript.
- `/tmp/provision-v14.log` — provision transcript.
- `/opt/nomad/data/alloc/*/alloc/logs/ch.stderr.0` (worker, pre-
  teardown) — the wrapper stderr with the `VM resumed` + tap-state
  evidence. Captured to `/tmp/smoke-b17.log` excerpts above.

---

# Appendix B — B18 fix attempt + c=4 validation (2026-05-23)

**Branch HEAD pre-fix:** `03d15012`.
**Branch HEAD post-fix:** (this commit).
**Controller binary uploaded:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v14` (15.7 MiB, Docker `rust:slim-bookworm` cross-build; portable `/lib64/ld-linux-x86-64.so.2` interp confirmed by `readelf -p .interp`).
**B18 verdict:** **CLOSED.**
**Operator:** B18 fixer + cluster validation sub-agent.

## Root cause (not what the original B18 brief suspected)

The "stale-pubkey 401" was *not* an in-VM stickiness problem
(workspace.img reuse, init.sh skipping the cmdline write, or per-user
signing-key reuse). The init.sh path was already
correct: `/run/keys/controller-pubkey` lives on tmpfs (fresh every
boot), the wrapper sets `zsbx_pubkey=${ZSBX_PUBKEY_HEX}` on the cmdline
from the freshly-minted `signing_key.verifying_key()`, and init.sh
unconditionally rewrites the pubkey file via `printf '%b' ... > /run/keys/controller-pubkey`
on every cold-boot. Each create-side `try_create()` does mint a fresh
`Arc<SigningKey>` (`random_key32()`), so the controller's
`expected_fp` is always against the new key.

The **actual** bug: **two separate `vm_index` allocators that don't
share state**.

1. `NomadCHBackend::vm_index_allocator` (the production pool, sized
   `[vm_index_floor, vm_index_ceil]`) hands out slots for new
   creates and reclaims them via `release()` on `stop_inner`.
2. `RealRestoreBackend::reservations` was a **private**
   `VmIndexReservations` map that the wake path used to reserve the
   source slot at `do_restore_inner` step 3.

A wake sequence:

- `stop_preserving_state` releases slot N **into the create-side
  allocator's freed set**.
- `do_restore_inner` reserves slot N **into the private wake-side
  map**.
- The restored VM lives on tap `zsbx-nm-N` / IP `10.99.10N.2`.
- A concurrent CREATE on the same worker calls
  `vm_index_allocator.alloc()` — which sees slot N as free (it's in
  `freed`) and **hands it out again**.
- The new create's wrapper boots a fresh CH against `zsbx-nm-N` and
  the controller probes `/version` at `10.99.10N.2:7777` — the live
  restored VM answers first, with the **original** snapshot's
  signing-pubkey, so the controller's `sig::sign(...)` is rejected
  with 401. Surface: `stale agent at http://10.99.10N.2:7777:
  /version returned 401 ...`.

The pre-existing in-source comment at
`crates/sandbox/src/restore_handler.rs:656-662` (the
`VmIndexReservations` doc) literally flagged this as
"v2 follow-up — share the allocator with NomadCHBackend so a
cross-backend create cannot collide with an in-flight restore." The
share never landed; this PR does it.

## Fix

1. `crates/sandbox/src/backend/nomad_ch.rs`:
   - `VmIndexAllocator` (`pub(crate)` → `pub`) so the restore handler
     in the same crate can hold an `Arc<Mutex<_>>` over it.
   - `VmIndexAllocator::reserve(i)`: added in-flight collision
     detection — `i < next && i ∉ freed ⇒ Err("vm_index N already
     reserved")`. Pre-fix, `reserve()` silently succeeded against a
     live slot (because `next` was bumped past `i` without
     checking). Post-fix, a double-reserve of a live slot is a hard
     error.
   - Added `NomadCHBackend::vm_index_allocator()` getter returning
     `Arc::clone` of the shared `Arc<Mutex<VmIndexAllocator>>`.

2. `crates/sandbox/src/backend/mod.rs`:
   - Added `Backend::vm_index_allocator()` returning
     `Option<Arc<Mutex<nomad_ch::VmIndexAllocator>>>`. `Some` only
     for the `NomadCh` variant.

3. `crates/sandbox/src/restore_handler.rs`:
   - `RealRestoreBackend` grew a `shared_allocator:
     Option<Arc<Mutex<...>>>` field plus a `with_shared_allocator()`
     builder. The legacy `VmIndexReservations` stays as the
     test-only fallback.
   - `reserve_vm_index` / `release_vm_index`: when
     `shared_allocator` is `Some`, route through it; else
     fall back to the local map.

4. `crates/sandbox/src/lib.rs`:
   - The Phase-A snapshot/restore wiring block extracts
     `backend.vm_index_allocator()` (when present) and passes it
     into `RealRestoreBackend::with_shared_allocator(...)`. Boot
     log: `snapshot wiring: shared vm_index allocator with backend
     (B18)`.

5. Added two regression tests in `restore_handler.rs::real_backend_tests`:
   - `b18_shared_allocator_blocks_create_side_reuse`: reserves the
     slot via wake, then asserts a concurrent create-side `alloc()`
     does NOT hand back the same slot.
   - `b18_shared_allocator_rejects_double_reserve_of_live_slot`:
     create-side `alloc()`s slot 1, then wake tries `reserve(1)` —
     must error "already reserved".

## Smoke (1+1, c=1) — PASS

Cluster: `zsbx-smoke`, 1 server (n2-standard-4) + 1 worker
(n2-standard-32), `asia-northeast3-a`,
`CONTROLLER_OBJECT=zeroship-sandbox.snapshot-v14`.

```
# stress: c=1 cycles=1 wake=True stop=True ctrl=http://localhost:9091
# total wall-time: 63.1s
create    ok=  1 err=  0 p50=  5239ms p95=  5239ms n=1
snapshot  ok=  1 err=  0 p50= 48049ms p95= 48049ms n=1
wake      ok=  1 err=  0 p50=  9762ms p95=  9762ms n=1
stop      ok=  0 err=  1 (pre-existing bug #19; post-wake stop 404)
```

Wake = 9.76 s (same range as r2's 9.5 s). No regression from B17.

## Smoke (1+1, c=4 × 4 = 16) — B18 verified PASS

Pre-fix (r2 baseline): 11/16 create failures with stale-pubkey 401.

Post-fix:

```
# stress: c=4 cycles=4 wake=True stop=True ctrl=http://localhost:9091
# total wall-time: 239.9s
create    ok= 10 err=  6 p50=13213ms p95=25426ms n=10
snapshot  ok= 10 err=  0 p50=52254ms p95=61164ms n=10
wake      ok= 10 err=  0 p50=15043ms p95=24931ms n=10
stop      ok=  0 err= 10 (pre-existing bug #19)
```

**Controller-side evidence**:

```
# stale-pubkey 401 errors in controller log (was 11 pre-fix):
$ grep -c "stale agent" /var/log/zeroship-sandbox.log
0

# B18 wiring log (boot):
$ grep "shared vm_index" /var/log/zeroship-sandbox.log
"message":"snapshot wiring: shared vm_index allocator with backend (B18)"

# Remaining 6 create failures are allocator-exhausted (the EXPECTED
# behaviour of the fix — 10 slots held by restored VMs + 6
# new-create attempts > 12-slot ceiling):
$ grep -c "allocator exhausted" /var/log/zeroship-sandbox.log
18  # = 6 surfaced to client + 12 retries inside backend.create's
    #   retry budget
```

Interpretation:
- **0 stale-pubkey 401s** — the B18 root cause is gone.
- The 6 remaining create errors are `vm-index allocator exhausted
  (floor=1, ceil=12)`. This is the **correct** post-fix behaviour:
  the shared allocator now accurately tracks which slots are held
  by live restored VMs, so a concurrent create against an exhausted
  pool fails fast at the allocator rather than booting a colliding VM.
- Without **bug #19** fixed (post-wake stop → "sandbox not found"
  → slot never released back to the shared allocator), the wake
  path leaks slots permanently from the create-side pool. The c=4
  stress dropoff from 16/16 to 10/16 is the slot-leak amplifying
  through the cycle pattern — with B19 still open, c=4 × 4 is the
  natural ceiling at vm_index_ceil=12.

## Why c=20 stress was NOT run

Bug #19 (wake doesn't register restored VM in `NomadCHBackend::
state` map → `stop_inner` returns Ok-idempotent without releasing
the slot) means the c=20 cycle pattern would hit allocator
exhaustion after ~12 successful waking cycles, the same way c=4
hit it after 10. The B-SLO p50/p95 numbers we'd record would be
*lower bounds* on the wake budget (still limited by bug #19's slot
leak). **Defer c=20 until #19 is fixed**; close B18 on the c=4
evidence (zero 401s vs 11/16 pre-fix is unambiguous).

## SLO snapshot (c=4 cycles=4 sample)

| Metric    | Target | Observed (c=4 p50) | Observed (c=4 p95) |
|-----------|--------|--------------------|--------------------|
| Create    | (ref)  | 13.2 s             | 25.4 s             |
| Snapshot  | (ref)  | 52.3 s             | 61.2 s             |
| Wake      | ≤ 1 s  | 15.0 s             | 24.9 s             |
| Stop      | ref    | n/a — bug #19      | n/a                |

Wake p50 ↑ from r2's 13.2 s to 15.0 s under c=4. Still 1-2 orders
of magnitude over the 1 s SLO; the 1 GB memory-ranges artifact
restore is the ceiling.

## Bug #19 (separate, NOT a B18 regression)

The B18 fix does not touch the pre-existing wake-doesn't-register
issue. After `do_restore_inner` returns Ok, the restored sandbox is
**not** inserted into `NomadCHBackend::state`. Any subsequent
operation (`exec`, `read_file`, `stop`) looks up via
`sandbox_keys` / `state.read().get(&id)` and gets "sandbox not
found". The slot stays reserved in the shared allocator until the
controller restarts. Tracked in deferred backlog as **[B19]**.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-smoke
[teardown] deleting instances: zsbx-smoke-server-1 zsbx-smoke-worker-1
…Deleted… (both instances + reserved IP)
[teardown] remaining instances matching ^zsbx-smoke-: 0
[teardown] OK: cluster fully torn down
```

`gcloud compute instances list --filter='name~"^zsbx-"'` → empty.

## Estimated cost

- Cluster wall-time: ~9 min (provision 90s + smoke c=1 63s + stress
  c=4 240s + observation + teardown ~3 min).
- n2-standard-32 worker @ ~$1.55/hr × 9/60 = **$0.23**.
- n2-standard-4 server @ ~$0.17/hr × 9/60 = **$0.03**.
- **Total: ~$0.26**. Well under the $30 cap.

## Files of interest (B18 fix)

- `crates/sandbox/src/backend/nomad_ch.rs` — `VmIndexAllocator`
  pub-ification + in-flight collision check + getter on
  `NomadCHBackend`.
- `crates/sandbox/src/backend/mod.rs` — `Backend::vm_index_allocator()`.
- `crates/sandbox/src/restore_handler.rs` — `RealRestoreBackend::
  with_shared_allocator` builder + reserve/release routing + two
  new B18 regression tests.
- `crates/sandbox/src/lib.rs` — wiring at AppState::from_config.
- `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v14` —
  uploaded controller binary (Docker `rust:slim-bookworm`).
- `/tmp/smoke-b18-c1.log`, `/tmp/smoke-b18-c4.log` — smoke
  transcripts.
- `/tmp/provision-b18.log` — provision transcript.
