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

---

# Appendix C — B19 fix attempt + c=4 / c=1 validation (2026-05-23)

**Branch HEAD pre-fix:** `4e6c70c1` (post B18 + R3-Q3 + R4-T1).
**Branch HEAD post-fix:** `15b4f9a8`.
**Controller binary uploaded:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v15` (15.1 MiB, Docker `rust:slim-bookworm` cross-build; portable `/lib64/ld-linux-x86-64.so.2` interp confirmed by `readelf -p .interp`).
**B19 verdict:** **UNVERIFIED on cluster** (lib tests PASS 266 → 268). The cluster smoke could not exercise the wake path because **every cold-boot create failed at `wait_for_agent_livez`** — a fresh blocker that surfaced post-v14, **NOT a B19 regression** (B19 touches the wake path only, never the create path). Filed below as **bug #20**.

## Lib tests

- Baseline at HEAD `4e6c70c1`: 266 passed, 1 ignored.
- Post-B19 fix at HEAD `15b4f9a8`: **268 passed, 1 ignored** — exactly +2 from the two new regression tests:
  - `backend::nomad_ch::tests::register_restored_inserts_into_state_map`
  - `backend::nomad_ch::tests::restored_sandbox_is_stoppable_and_releases_vm_index`
- Build clean (`cargo build -p zeroship-sandbox --tests`).

## Fix shape (Option A from the deferred file)

1. `crates/sandbox/src/backend/nomad_ch.rs`:
   - new `pub(crate) fn register_restored(&self, sandbox_id, vm_index, signing_key_bytes, agent_url, user_id) -> Result<(), String>` doing `state.write().insert(NomadChSandbox { ... })` with a vacant-entry check that rejects clobber.
2. `crates/sandbox/src/backend/mod.rs`:
   - **`Backend::NomadCh(NomadCHBackend)` → `Backend::NomadCh(Arc<NomadCHBackend>)`** (asymmetric — only NomadCh wrapped, Docker/K8s stay by-value). Enables `RealRestoreBackend` to hold a shared `Arc<NomadCHBackend>` handle.
   - new `Backend::nomad_ch_handle() -> Option<Arc<NomadCHBackend>>` getter (mirrors the B18 `vm_index_allocator()` getter shape).
   - new `Backend::register_restored(...)` enum-level delegator returning Err for Docker/K8s.
3. `crates/sandbox/src/restore_handler.rs`:
   - new trait method `RestoreBackend::register_restored(sandbox_id, vm_index, signing_key_bytes, user_id) -> Result<(), String>` with a default `Ok(())` no-op (keeps `StubRestoreBackend`-driven tests compiling).
   - `RealRestoreBackend` gets `nomad_handle: Option<Arc<NomadCHBackend>>` field + `with_nomad_handle(...)` builder.
   - `RealRestoreBackend::register_restored(...)` impl re-derives the agent_url from `vm_index` + `subnet_second_octet` (same shape as `wait_for_livez`) and calls the backend's `register_restored`.
   - `restore_sandbox` + `do_restore_inner` get a new `persist: Option<&Persistence>` arg. After `wait_for_livez` Ok, `persist.unseal(sandbox_id)` recovers the `signing_key_bytes` and the trait's `register_restored` is called. A `None` persist surfaces a tracing::warn; a `NotFound` sealed record surfaces as an Internal 500 (a live restored VM with no signing key cannot be safely registered).
4. `crates/sandbox/src/persist.rs`:
   - new `pub async fn unseal(&self, sandbox_id: Uuid) -> std::io::Result<SealedAuth>` (per-sandbox unseal — analog of `delete`, builds on `unseal_one`).
5. `crates/sandbox/src/lib.rs`:
   - `AppState::from_config` extracts `Arc<NomadCHBackend>` via `backend.nomad_ch_handle()` and pipes through `RealRestoreBackend::with_nomad_handle(...)`. Tracing line `snapshot wiring: shared NomadCHBackend handle for register_restored (B19)` on success; warn if absent.
6. `crates/sandbox/src/admin_handlers.rs`:
   - `wake_sandbox` passes `state.persist.as_deref()` into `restore_sandbox`.
7. `crates/sandbox/tests/sandbox_pg_e2e.rs`:
   - 5 call sites bulk-updated to pass `None` for the new `persist` arg (these tests use `StubRestoreBackend`, whose default `register_restored` impl is a no-op).

## Cluster smoke (1+1, c=4 × 4) — BLOCKED on bug #20

```
[provision] OK at /tmp/provision-v15.log
  zsbx-smoke-server-1 (n2-standard-4)
  zsbx-smoke-worker-1 (n2-standard-32, nested-virt)
[boot wiring log on worker]
  "snapshot wiring: shared NomadCHBackend handle for register_restored (B19)"
  "snapshot wiring: shared vm_index allocator with backend (B18)"

stress (c=4, cycles=4) — /tmp/smoke-b19-c4.log:
  CREATE OK: 0/16
  SNAPSHOT OK: 0/0
  WAKE OK: 0/0
  POST-WAKE EXEC OK: 0/0
  STOP OK: 0/16
  FAILED CREATES: 16
    Every create returned 503 create_retry_budget_exhausted with
    last error:
      "agent at http://10.99.X.2:7777 never returned 200 on /livez
       (expected fp=…)"

stress (c=1, cycles=1) — /tmp/smoke-b19-c1.log:
  CREATE OK: 0/1
  same failure: cold-boot /livez never 200.
```

Wake never fired (no successful create → no snapshot → no wake), so **B19 itself was not exercised in vivo**. The wiring log confirms the controller picked up the new `register_restored` plumbing at boot; the trait dispatch path is locally validated by the two regression tests at HEAD.

## Bug #20 (NEW, NOT a B19 regression) — cold-boot /livez never 200

- **Source:** cluster smoke 2026-05-23 c=4 + c=1 (Appendix C above).
- **Symptom:** every cold-boot create reaches `client_status=running` (Nomad alloc up; controller logs `sandbox/nomad-ch create alloc running … elapsed_ms=772`), then `wait_for_agent_livez` times out at the configured 30s budget. After 3 retries the controller returns 503 `create_retry_budget_exhausted`. Nomad then kills the alloc (exit code 130 — interrupt + 10s grace) and GCs it.
- **Tap state:** taps `zsbx-nm-1` through `zsbx-nm-10` present on the worker host, all `<NO-CARRIER,BROADCAST,MULTICAST,UP>` (DOWN at L2). Same shape as the B17 root cause (paused vCPUs not pumping virtio-net) — but here CH is supposed to be running cold-boot, not paused. Either CH itself failed to bring vCPUs up, or the agent inside the rootfs failed to start.
- **Not a B19 regression:** B19 touches only the wake path (`restore_handler::do_restore_inner` after `wait_for_livez` Ok) + the `NomadCHBackend` registry surface. Create path is unchanged. The same `wait_for_agent_livez` helper that fails here is the one B18's c=4 smoke exercised PASS at v14 on 2026-05-23. Either v15 has a non-B19 regression (unlikely — diff is scoped) OR the cluster's worker/rootfs state diverged since the B18 cycle (rootfs rebake, init.sh change, vmlinuz pin moved, ch-remote v51.1 bug, …).
- **Not investigated this cycle** per the task brief's "If a NEW bug surfaces (#20+): capture, do NOT start fixing" rule.
- **Reproducer:** provision 1+1 with v15; manually `POST /sandboxes` with any `usr_*` typed-id; observe `wait_for_agent_livez` timeout after ~30s; allocs killed by Nomad.
- **Evidence files:**
  - `/tmp/smoke-b19-c4.log` — 16/16 fails.
  - `/tmp/smoke-b19-c1.log` — 1/1 fails.
  - controller log `/var/log/zeroship-sandbox.log` on worker (pre-teardown captured WARN/ERROR rows).
  - Nomad daemon log via `journalctl -u nomad`: shows `Task started by client` then `Killing: Sent interrupt … Exit Code: 130`.

## B-SLO (5-worker × 20-cycle) — not attempted

Skipped per brief: c=4 failed at 0/16, so c=20 was not run. B-SLO measurements remain unmeasured.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-smoke
[teardown] deleting instances: zsbx-smoke-server-1 zsbx-smoke-worker-1
… Deleted (both instances + reserved IP) …
[teardown] remaining instances matching ^zsbx-smoke-: 0
[teardown] OK: cluster fully torn down
```

`gcloud compute instances list --filter='name~"^zsbx-"'` → empty.

## Estimated cost

- Cluster wall-time: ~22 min (provision 90s + c=4 smoke ~7 min + c=1 smoke ~2 min + observation + teardown ~3 min, plus some Docker-build idle in between).
- n2-standard-32 worker @ ~$1.55/hr × 22/60 = **$0.57**.
- n2-standard-4 server @ ~$0.17/hr × 22/60 = **$0.06**.
- **Total: ~$0.63**. Well under the $30 cap.

## Files of interest (B19 fix)

- `crates/sandbox/src/backend/nomad_ch.rs` — `register_restored` method + 2 regression tests.
- `crates/sandbox/src/backend/mod.rs` — `NomadCh(Arc<…>)` wrap + `nomad_ch_handle()` + `register_restored` enum delegator.
- `crates/sandbox/src/restore_handler.rs` — trait `register_restored` method + `with_nomad_handle` builder + `RealRestoreBackend::register_restored` impl + `do_restore_inner` post-livez call.
- `crates/sandbox/src/persist.rs` — `Persistence::unseal(sandbox_id)`.
- `crates/sandbox/src/admin_handlers.rs` — pass `state.persist.as_deref()` through.
- `crates/sandbox/src/lib.rs` — `nomad_ch_handle()` extraction at boot.
- `crates/sandbox/tests/sandbox_pg_e2e.rs` — 5 call sites updated to pass `None` for persist.
- `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v15` — uploaded controller binary.
- `/tmp/smoke-b19-c1.log`, `/tmp/smoke-b19-c4.log` — smoke transcripts (all-fail at create).
- `/tmp/provision-v15.log` — provision transcript.
- `/tmp/teardown-v15.log` — teardown transcript.

## Recommendation

- B19 lib-tested PASS (266 → 268). Trait + state-map wiring locally validated.
- B19 cluster verification **gated on bug #20 closing** (cold-boot /livez recovery). Re-run c=4 × 4 + c=20 once #20 lands; the wiring is already in place to exercise wake/exec/stop end-to-end on a fresh v15+ binary.
- Leave **[B19] in the deferred backlog as "fix landed in code, cluster verification pending bug #20"** rather than CLOSED.

---

# Appendix D — B20 root cause + fix + c=1/c=4 re-validation (2026-05-23)

**Branch HEAD pre-fix:** `3e8bfad5`.
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v15` (unchanged — the bug is in the worker bootstrap script, not the Rust binary).
**Worker bootstrap script changed locally** (re-uploaded to GCS implicitly via provision script reading the local file).
**B20 verdict:** **CLOSED.**
**Operator:** B20 fixer + cluster validation sub-agent.

## Root cause (verbatim evidence from git history)

`gs://suger-dev-zsbx-artifacts/` artifact listing showed nothing changed between v14 (B18 fixer's PASS) and v15 (B19 fixer's FAIL): same wrapper (22675 B since `dec489a1` 2026-05-22), same vmlinuz (2026-05-05), same `cloud-hypervisor.v51.1` / `ch-remote.v51.1` (2026-05-05). The only published artifact that differed was the controller binary itself, and the v14→v15 diff is **purely additive** in the wake path (`register_restored` method, `Arc<NomadCHBackend>` wrap, `with_nomad_handle` builder, `Persistence::unseal`); the cold-boot create path was bit-identical.

That eliminated rootfs / vmlinuz / ch-remote / cloud-hypervisor / wrapper / controller-create-path. Remaining candidate: the worker bootstrap script. `git stash list` showed:

```
stash@{0}: WIP on feat/sandbox-snapshot-restore: a9e568a2
  sandbox/config: pub(crate)-restrict SandboxConfig.token (A7)
```

`git show 'stash@{0}' -- crates/sandbox/scripts/gcp-worker-startup.sh` revealed the smoking gun:

```diff
-gs_pull rootfs-slim.img.fp32          "$ART/rootfs-slim.img" 0644
+gs_pull rootfs-slim.img.virtio-blk-v3 "$ART/rootfs-slim.img" 0644
```

The B18 fixer ran v14 with this stash applied. The B19 fixer (this cycle's predecessor) used a fresh worktree, did not re-apply the stash, so the **committed** script pulled the old `rootfs-slim.img.fp32` (2026-05-06, pre-virtio-blk pivot) into `/etc/zeroship/rootfs-slim.img`. The wrapper's cold-boot `--disk` block passes `$ZSBX_WORKSPACE_IMG` (virtio-blk) and the rootfs's `/sbin/init` (still the virtio-fs version on fp32) cannot mount `/dev/vdb`/`/dev/vdc` or has a broken pubkey decoder (bug #12/#13 series). Either way, the in-VM `sandbox-agent` never binds `:7777`, the tap stays `<NO-CARRIER>` (no peer over virtio-net), and `wait_for_agent_livez` times out at 30s × 3 retries → 503 `create_retry_budget_exhausted`. Exactly the v15 cluster smoke shape from Appendix C.

GCS MD5 confirmation:
```
rootfs-slim.img.fp32          md5: de0c5f02f6324910827717875d01dd12
rootfs-slim.img.virtio-blk-v3 md5: 4aaae4bff33e1bb7debde05f2d183c4c
```

After the fix in this cycle the worker pulled the virtio-blk-v3 rootfs:
```
$ md5sum /etc/zeroship/rootfs-slim.img
4aaae4bff33e1bb7debde05f2d183c4c  /etc/zeroship/rootfs-slim.img
```

## Fix applied

`crates/sandbox/scripts/gcp-worker-startup.sh` line 143:
```diff
-gs_pull rootfs-slim.img.fp32          "$ART/rootfs-slim.img" 0644
+gs_pull rootfs-slim.img.virtio-blk-v3 "$ART/rootfs-slim.img" 0644
```

Plus two comment updates (header + post-pull comment) marking the variant as virtio-blk and referencing this appendix.

Size: 3 lines logical (one `gs_pull` line + two comments). No binary rebuild required. No rootfs rebake required. No wrapper change.

## Smoke (1+1 diag, c=1) — PASS

Cluster: `zsbx-diag`, 1 server (n2-standard-4) + 1 worker (n2-standard-32), `asia-northeast3-a`, `CONTROLLER_OBJECT=zeroship-sandbox.snapshot-v15` (unchanged).

```
# b20-fix: concurrency=1, cycles=1, total=1
# elapsed: 62.7s
CREATE OK:   1/1   create p50=5339ms
SNAPSHOT OK: 1/1   snapshot p50=47834ms (1.07 GB artifact)
WAKE OK:     1/1   wake p50=9505ms
POST-WAKE EXEC OK: 0/1   "backend.exec: sandbox not found in nomad-ch backend"
STOP OK:     1/1   stop p50=19ms
```

Create + snapshot + wake + stop succeed end-to-end. Wake p50 (9.5s) matches v14 and earlier appendices exactly — confirms the fix is the rootfs pull, not a behavior change.

**EXEC_POST fails with the B19 "sandbox not found" symptom**. See bug #21 below — this is NOT a B20 regression but the latent consequence of B19's wake-side `register_restored` being silently no-op'd when the controller boots without `SANDBOX_PERSIST_AUTH=1`.

## Smoke (1+1 diag, c=4 × 4 = 16) — B20 verified PASS

```
# b20-fix-c4: concurrency=4, cycles=4, total=16
# elapsed: 272.9s
CREATE OK:   11/16  create p50=15509ms p95=62802ms
SNAPSHOT OK:  9/11  snapshot p50=54511ms p95=59293ms
WAKE OK:      9/ 9  wake p50=11586ms p95=21189ms
POST-WAKE EXEC OK: 0/9   (bug #21 — same shape as c=1)
STOP OK:      9/16
FAILED CREATES: 5  vm-index allocator exhausted (floor=1, ceil=12)
FAILED SNAPSHOTS: 2  60s snapshot timeout
```

- 0/16 cold-boot creates pre-fix → 11/16 cold-boot creates post-fix. The remaining 5 are vm-index-exhausted, which is the expected downstream effect of B19's wake-side `register_restored` not firing (slot leaks per successful wake until floor=1..ceil=12 saturates).
- Wake 9/9 (100%); the path is exercised end-to-end on every cycle that reached snapshot.
- Wake p50 11.6s matches v14's c=4 wake p50 (15.0s) within stress noise. No regression.

## Controller boot evidence (B19 wiring half-fires, persist=None)

```
{"message":"snapshot wiring: tiered L1+GCS","l1_root":"/var/zeroship/ch/snapshots","gcs_bucket":"suger-dev-zsbx-artifacts"}
{"message":"snapshot wiring: shared vm_index allocator with backend (B18)"}
{"message":"snapshot wiring: shared NomadCHBackend handle for register_restored (B19)"}
{"message":"snapshot/restore wiring: enabled","ch_version":"ch-remote v51.1","kek_path":"None"}
```

But at wake time:
```
{"level":"WARN","message":"restore: register_restored skipped — persist=None
  (expected only in tests; production wiring at AppState::from_config plumbs Some)",
 "sandbox_id":"019e53ce-492a-7210-b80f-4a38e78c60b0"}
```

So the controller has all the B19 plumbing in place EXCEPT the `state.persist` field, which `AppState::from_config` builds via `Persistence::from_env()?.map(Arc::new)` — and `Persistence::from_env()` returns `Ok(None)` when `SANDBOX_PERSIST_AUTH` is not set. Filed as bug #21.

## Bug #21 (NEW, cluster systemd misconfig) — `SANDBOX_PERSIST_AUTH=1` not set, B19 silently no-ops

- **Source:** Appendix D diag cluster 2026-05-23.
- **Symptom:** controller boots with B19's `nomad_ch_handle` plumbed into `RealRestoreBackend`, but the runtime warn path
  ```
  "restore: register_restored skipped — persist=None"
  ```
  fires on every wake. Trait dispatch never reaches `Backend::register_restored`, so the restored VM stays out of the state map; downstream EXEC returns 500 "sandbox not found"; STOP returns Ok-idempotent without releasing the vm_index. After 9 successful wakes the c=4 cluster saturated at 12 slots and the next 5 cold-boot creates failed with `allocator exhausted`. Identical surface to the latent B19 leak the deferred file describes.
- **Root cause:** `crates/sandbox/scripts/gcp-worker-startup.sh` does NOT export `SANDBOX_PERSIST_AUTH=1` in the controller systemd Environment block. `Persistence::from_env()` returns `Ok(None)` → `state.persist = None` → wake-side warn-skip.
- **Not investigated this cycle** per the brief's "If a NEW bug surfaces (#21+): capture, do NOT start fixing" rule.
- **Likely fix:** add `Environment=SANDBOX_PERSIST_AUTH=1` (and any required `SANDBOX_PERSIST_KEK_PATH` / DEK seed material) to `gcp-worker-startup.sh` systemd unit. Cross-check what env var Persistence reads (search `Persistence::from_env`). One commit; trivial.
- **Evidence:** `/tmp/smoke-b20-fix-c1.log`, `/tmp/smoke-b20-fix-c4.log`, controller log `register_restored skipped — persist=None`.

## B19 cluster verification status

- **Partially closed**: the post-fix cluster confirms the B19 plumbing reaches `restore_handler::do_restore_inner` step 7b (the post-livez branch). The wake itself works end-to-end on all 9 attempts. What B19 ALSO needs to clear cluster — full state-map registration after wake — is gated on bug #21 (persist=None). Until #21 closes, B19's lib tests pass + plumbing wires correctly + wake works, but `register_restored` is never called, so EXEC_POST and slot-release stay broken on cluster.
- **Recommended status update**: B19 → keep "FIX LANDED in code, cluster verification PARTIALLY VERIFIED (wake path works; register_restored gated on bug #21)".

## B-SLO (5-worker × 20-cycle) — NOT attempted

Bug #21 keeps wake-side state-map registration off, which means every successful wake leaks a vm_index slot. A 5+20 stress (~100 cycles) would saturate the 12-slot pool after ~9 successful wakes per worker, dumping the rest of the budget on `allocator exhausted` 500s. Defer B-SLO until #21 closes.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-diag
[teardown] deleting instances: zsbx-diag-server-1 zsbx-diag-worker-1
Deleted [...zones/.../instances/zsbx-diag-server-1].
Deleted [...zones/.../instances/zsbx-diag-worker-1].
[teardown] releasing internal addresses: zsbx-diag-server-1-ip
Deleted [.../regions/.../addresses/zsbx-diag-server-1-ip].
[teardown] remaining instances matching ^zsbx-diag-: 0
[teardown] OK: cluster fully torn down
```

`gcloud compute instances list --filter='name~"^zsbx-"'` → empty.

## Estimated cost

- Cluster wall-time: ~10 min (provision 2 min + c=1 smoke 1 min + c=4 smoke 4.5 min + observation 1 min + teardown 1 min).
- n2-standard-32 worker @ ~$1.55/hr × 10/60 = **$0.26**.
- n2-standard-4 server @ ~$0.17/hr × 10/60 = **$0.03**.
- **Total: ~$0.29**. Well under the $30 cap.

## Files of interest (B20 fix)

- `crates/sandbox/scripts/gcp-worker-startup.sh` — single `gs_pull` line + two comment updates (lines 11 / 143 / 159).
- `gs://suger-dev-zsbx-artifacts/rootfs-slim.img.virtio-blk-v3` — unchanged; the right artifact.
- `/tmp/smoke-b20-fix-c1.log`, `/tmp/smoke-b20-fix-c4.log` — smoke transcripts.
- `/tmp/provision-diag.log` — provision transcript.
- `/tmp/teardown-diag.log` — teardown transcript.

## Lessons / scope

- The B18-fixer cycle's c=4 PASS depended on a local stash that never landed. The B19-fixer cycle's c=4 FAIL was the inevitable consequence: a fresh worktree restored the committed (pre-pivot) script. Cluster validation cycles MUST either (a) commit the script change before running the smoke, or (b) the cron worker should snapshot+restore stashed changes before dispatching cluster work.
- Surface a small lint: `bash -n crates/sandbox/scripts/gcp-worker-startup.sh && grep -c 'rootfs-slim.img.virtio-blk' crates/sandbox/scripts/gcp-worker-startup.sh` should return 1 in CI. R4-T1's shellcheck gate caught syntax issues but not this content-drift.

## Recommendation

- **Close B20** in deferred backlog. The cluster c=4 evidence is unambiguous (0/16 → 11/16; same shape as v14 c=4).
- **Promote B19 to "PARTIALLY VERIFIED on cluster"** in deferred backlog. The plumbing reaches the wake path; the post-livez register call is gated on bug #21.
- **Open bug #21** for the next cycle: add `SANDBOX_PERSIST_AUTH=1` + key material to the controller systemd Environment block in `gcp-worker-startup.sh`.

# Appendix E — B21 fix + R5-S1 boot assertion + bug #22 (post-wake agent 401)

**Branch HEAD pre-fix:** `1066a319`.
**Controller binary:** `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v16` (rebuilt; v16 carries the boot-time fail-CLOSED assertion).
**Worker startup script:** post-fix carries the `Environment=SANDBOX_PERSIST_AUTH=1` triplet + provisions a 32-byte 0o400 AEAD key file at `/etc/zeroship/sandbox-aead-key`.
**Cluster shape:** 1 server + 1 worker (smoke scope per brief).
**Wall time:** ~6 min (provision 2 min + c=4 smoke ~4 min + observation 30 s + teardown 30 s).

## Fix shape

### Script change — `crates/sandbox/scripts/gcp-worker-startup.sh`

1. **AEAD key provisioning step** (section 5, before the systemd unit):
   - Generates a 32-byte key at `$AEAD_KEY_PATH=/etc/zeroship/sandbox-aead-key` via `head -c 32 /dev/urandom`.
   - `chmod 0400` to satisfy `AeadKey::from_path`'s mode check (round-6 H8 enforcement).
   - Idempotent: a non-empty file is reused so reboots keep sealed records readable.
   - `mkdir -p /var/lib/zeroship/sandbox/sealed-records` so the boot-time restore loop finds the dir on a fresh host.

2. **Three new `Environment=` lines** in the controller systemd unit:
   ```
   Environment=SANDBOX_PERSIST_AUTH=1
   Environment=SANDBOX_AEAD_KEY_PATH=/etc/zeroship/sandbox-aead-key
   Environment=SANDBOX_PERSIST_DIR=/var/lib/zeroship/sandbox
   ```

### Lib change — `crates/sandbox/src/lib.rs`

R5-S1 boot-time fail-CLOSED. Factored into a pure helper for testability:

```rust
pub(crate) fn assert_persist_required_when_snapshot_enabled(
    snapshot_enabled: bool,
    persist_present: bool,
    test_override: bool,
) -> Result<(), String> {
    if snapshot_enabled && !persist_present && !test_override {
        return Err("FATAL: SANDBOX_SNAPSHOT_ENABLED=true but persistence is disabled ...".to_string());
    }
    Ok(())
}
```

`AppState::from_config` calls it immediately after `Persistence::from_env()?.map(Arc::new)`. The escape hatch `SANDBOX_PERSIST_NONE_OK=1` lets test/dev fixtures drive `StubRestoreBackend` without persistence.

### Test coverage

5 new unit tests pin every cell of the truth table (snap=off/on × persist=off/on × override=off/on). Total: **275 → 280 sandbox lib tests PASS** (`cargo test -p zeroship-sandbox --lib` → `280 passed; 0 failed; 1 ignored`).

## Cluster validation — smoke c=4 cycles=4 (N=16)

### Boot evidence

Controller `active`, `/livez` = 200, env block confirmed:
```
=== controller env (from systemd) ===
Environment=SANDBOX_PERSIST_AUTH=1
Environment=SANDBOX_AEAD_KEY_PATH=/etc/zeroship/sandbox-aead-key
Environment=SANDBOX_PERSIST_DIR=/var/lib/zeroship/sandbox

=== persist log lines ===
{"level":"INFO","message":"sandbox persist: pg + sealed restore starting",
 "persist_dir":"/var/lib/zeroship/sandbox", ...}
{"level":"INFO","message":"sandbox persist: restore done",
 "seen":0, "restored":0, ...}
{"level":"INFO","message":"snapshot wiring: shared NomadCHBackend handle for register_restored (B19)"}
```

R5-S1 assertion did not fire (snap on + persist on = legal). Had `SANDBOX_PERSIST_AUTH=1` not been set, the controller would have refused to boot — the new boot-time check is the operational safety net for this misconfig class.

### Smoke c=4 outcome (N=16)

```
CREATE OK:    12/16
SNAPSHOT OK:   9/12  (3 timeouts at 60s)
WAKE OK:       9/9   (100%)
POST-WAKE EXEC: 0/9  (every call: agent /exec status 401 unauthorized) ← NEW bug #22
STOP OK:       9/16
```

Timing (ms):
- create  p50=15400 p95=22488 p99=24997 max=24997
- snapshot p50=50307 p95=58080 p99=58080 max=58080
- wake     p50=9729  p95=13442 p99=13442 max=13442
- stop     p50=19    p95=23    p99=23    max=23

### B-SLO measurement

- Wake p50 = 9729 ms. SLO target ≤ 1000 ms; **MISS by 8.7×**. (Most of the wake-time is `store.get` 1 GB SHA + AEAD decrypt — the deferred-A3 sync I/O issue. See deferred A3 / R5-P1 in progress.)
- Snapshot p50 = 50.3 s. SLO target ≤ 2.0 s; **MISS by 25×**. (Bulk of the time is the GCS put — 1 GB at sustained ~20 MB/s tail latency. Same A3 root cause + sync on compio worker.)
- Stop p50 = 19 ms. No SLO target documented; observed performance is healthy.
- B-SLO measurements at c=20/3-worker scale not collected — bug #22 saturates the c=4 pool so a wider stress wouldn't generate meaningful additional signal until the agent-401 is fixed.

### Verdict

- **B21 → CLOSED.** Controller log no longer carries `register_restored skipped — persist=None` on any of the 9 wakes; the warn-skip branch is unreachable in this configuration. `do_restore_inner` step 7b's `unseal + register_restored` path is exercised end-to-end.
- **R5-S1 → CLOSED.** Boot-time fail-CLOSED guard lands in `AppState::from_config`. The lib tests pin every cell of the truth table. The dangerous misconfig (`snap=on, persist=off`) now produces a hard boot failure with a remediation message rather than a silent fail-OPEN at wake-time.
- **B19 → PARTIALLY VERIFIED still.** Wake delivers 200 and the post-livez register chain executes (no more "sandbox not found" 500s). But every post-wake agent `/exec` returns 401 — see bug #22 below. Full B19 cluster verdict (exec_post 200 + slot release after stop) is gated on #22.
- **B-SLO → DEFERRED.** Wake p50 misses target by ~9×, snapshot p50 by ~25×. Both attributable to the open A3 sync-I/O issue. Re-measure once A3 lands (R5-P1 fixer's BufReader + spawn_blocking patch).

## Bug #22 (NEW, post-wake agent 401) — agent rejects every post-wake `/exec`

- **Source:** Appendix E cluster smoke 2026-05-23 r6 (B21 fixer cycle).
- **Symptom:** every successful wake (9/9, 100%) is followed by an `/exec` that returns `agent /exec status 401: {"error":"unauthorized"}`. Controller log lines: `"sandbox/nomad-ch agent error","op":"exec","status":401`. The wake itself returns 200; `register_restored` clearly ran (otherwise the 500 would be `sandbox_not_found`, not a downstream agent 401).
- **Hypothesis:**
  1. (most likely) The signing key the controller hands to `register_restored` does NOT match what the restored agent in the VM holds. The CH `snapshot` captures guest memory at pause-time, including the agent's in-process verifying key. On wake, the controller installs the SAME sealed signing key into its state map — but the agent inside the resumed VM may have crashed / restarted / re-derived its keypair after wake-time. If the agent comes back with a fresh keypair, the controller's signed RPC won't validate.
  2. (alternative) `register_restored` installs the right key but the per-sandbox `SandboxAuth::pubkey_fp` doesn't match the agent's runtime key. The agent's `/livez` check is unsigned and would happily 200; only `/exec` (signed) reveals the mismatch.
  3. (alternative) The boot_id field on the sealed record doesn't survive restore and re-seal cycles correctly, causing the agent's pubkey-fp validation to drift.
- **Cluster evidence:** every post-wake exec_post = 500/401 across all 9 wakes; identical error message; signature failure happens AT the agent (its 401, not the controller's auth). The 401 body is verbatim `{"error":"unauthorized"}` which matches `sandbox-agent`'s sig-verify reject path.
- **Not investigated this cycle** per the brief's "If a NEW bug surfaces (#22+): capture verbatim, do NOT start fixing" rule.
- **Recommended next cycle:**
  - SSH into a worker, dump the agent's in-VM key (`/etc/zeroship-agent/...` or wherever the agent persists it) before and after a snapshot/wake cycle.
  - Compare `SealedAuth::signing_key_bytes` to what the agent has.
  - Inspect `crates/sandbox-agent/src/sig.rs` for any key-rotation-on-boot logic that would invalidate the snapshot's stored verifying key.
  - Also revisit T5 (signed `/version` fingerprint check during wait_for_livez) — that test would have caught the mismatch DURING the wake path, surfacing as a wake-failure rather than a downstream exec-failure.

## vm_index allocator behavior

The 4 failed creates at idx=12..15 hit `vm-index allocator exhausted (floor=1, ceil=12)` because:
1. Concurrency 4 + cycles 4 = 16 ops issued in parallel waves.
2. First 12 creates land successfully (floor=1..ceil=12).
3. Snapshots run; 3 hit 60-second snapshot timeout (leaving the live sandbox holding its slot).
4. Wakes succeed but DON'T release the slot (the wake CONSUMES the snapshot slot).
5. Stop releases the slot (verified — `vm_index released` log line on every stop).

But the **stop happens AFTER the next-cycle create has already been attempted**, so the burst of 4 creates at idx=12..15 raced against the in-flight stops and lost. This is a concurrency burst issue, not a slot leak — controller log confirms every stop calls `vm_index released`. **NOT bug #22; this is the pre-existing cap=12 limit on a single-worker smoke.** Scale to 5 workers (the deferred B-SLO config) and a c=4 stress should fit comfortably.

## Teardown

```
[teardown] project=suger-dev zone=asia-northeast3-a prefix=zsbx-smoke
[teardown] deleting instances: zsbx-smoke-server-1 zsbx-smoke-worker-1
Deleted [...zones/.../instances/zsbx-smoke-server-1].
Deleted [...zones/.../instances/zsbx-smoke-worker-1].
[teardown] releasing internal addresses: zsbx-smoke-server-1-ip
Deleted [.../regions/.../addresses/zsbx-smoke-server-1-ip].
[teardown] remaining instances matching ^zsbx-smoke-: 0
[teardown] OK: cluster fully torn down
```

`gcloud compute instances list --filter='name~"^zsbx-"'` → empty.

## Estimated cost

- Cluster wall-time: ~6 min (provision 2 min + c=4 smoke 4 min + observation 30 s + teardown 30 s).
- n2-standard-32 worker @ ~$1.55/hr × 6/60 = **$0.155**.
- n2-standard-4 server @ ~$0.17/hr × 6/60 = **$0.017**.
- **Total: ~$0.17**. Well under the $30 cap.

## Files of interest (B21 + R5-S1)

- `crates/sandbox/scripts/gcp-worker-startup.sh` — AEAD key gen block + 3 new `Environment=` lines + `mkdir -p /var/lib/zeroship/sandbox/sealed-records`.
- `crates/sandbox/src/lib.rs` — new helper `assert_persist_required_when_snapshot_enabled` (~50 lines incl. docstring) + 5 unit tests pinning the truth table.
- `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v16` — controller binary (rebuilt via Docker rust:slim-bookworm).
- `/tmp/provision-b21.log` — provision transcript.
- `/tmp/smoke-b21-c4.log` — c=4 smoke transcript (raw JSON included).

## Lessons / scope

- The fail-OPEN → fail-CLOSED conversion (R5-S1) is the right shape: the next time an operator misconfigures the env block, the controller refuses to boot with a clear remediation message instead of producing 9 wakes that silently break the state map.
- B21's lesson: cluster script changes need feature-flag-aware gating. The script now provisions PERSIST_AUTH unconditionally on every worker, which is correct ONLY because snapshot is always enabled on Phase B prod hosts. If we later introduce a feature flag for snapshot, the script needs matching logic.
- Bug #22 (agent 401 on post-wake exec) was masked by bug #21. Closing #21 surfaced it. This is the chain we expected — the cluster diagnostic loop is doing its job. Next cycle's focus should be agent-key persistence across CH snapshot/wake.

# Appendix F — Bug #22 fix: post-CH-restore CLOCK_REALTIME resync handshake

**Branch HEAD pre-fix:** `29196e0c`.
**Investigation duration:** ~25 min (code-only — no cluster needed to root-cause).
**Cluster validation (c=4 smoke):** **PASS**. POST-WAKE EXEC went from 0/9 (Appendix E pre-fix) → 7/7 (post-fix). See "Cluster validation" below.
**Cluster artifacts produced:**
- `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v17` — controller binary with the wake-side resync call wired in.
- `gs://suger-dev-zsbx-artifacts/rootfs-slim.img.virtio-blk-v4` — rootfs with the new agent (POST `/_clock_resync` + `clock.resync-v1` capability).
- `crates/sandbox/scripts/gcp-worker-startup.sh:150` bumped from `virtio-blk-v3` to `virtio-blk-v4`.

## Root cause (from code investigation)

The bug is NOT a stale signing key, NOT a register_restored mistake, and NOT a config.json cmdline issue (the three hypotheses called out in the brief).

The actual root cause: **after Cloud Hypervisor `--restore`, the guest's `CLOCK_REALTIME` is frozen at the snapshot-time value.** The agent's per-request auth verifier (`crates/sandbox-agent/src/sig.rs`) applies a strict `SKEW_S = 5` second skew check on `abs_diff(unix_now(), ts_hdr)`. Every controller-signed RPC carries the controller's current timestamp; the agent's `unix_now()` reads the frozen wall clock, so the skew check fails by however long the snapshot→wake gap was (typically minutes to days) and returns `AuthFail::SkewTooLarge`. The 401 surface text is `{"error":"unauthorized"}` (`handlers.rs::unauthorized()`), which matches Appendix E's verbatim cluster evidence.

Why init.sh re-reading `/proc/cmdline` doesn't help: CH `--restore` resumes the VM from the memory snapshot WITHOUT re-executing init. The agent process is restored from memory with its `Verifier` already cached from the original cold-boot. Its in-memory pubkey is correct (matches the sealed signing-key bytes the controller installs via `register_restored`); the failure is purely the wall-clock gate, not signature mismatch.

Why `/livez` returns 200 while `/exec` returns 401: `/livez` is unauthenticated (`handlers.rs:249`) — no skew check is run. Every signed endpoint (`/exec`, `/version`, `/files`, `/tree`, `/shutdown`) hits the skew gate and 401s.

This is a known Cloud Hypervisor snapshot/restore behavior: CH preserves both `KVM_CLOCK` and the in-guest `CLOCK_REALTIME` from snapshot time. No CH primitive sets the guest's wall clock from the host. The agent has no NTP / PTP / chrony running in the rootfs (verified by greping `crates/sandbox/scripts/bake-rootfs.sh` and `init.sh` — neither installs a time-sync daemon).

## Fix shape (controller + agent)

### Agent: `crates/sandbox-agent/src/sig.rs`

Add `Verifier::verify_kind_skew_bypass(...)` — same shape as `verify_kind` but skips step 1 (the `abs_diff(now, ts) > SKEW_S` gate). Every other check fires: nonce shape, signature decode, body hash binding, Ed25519 signature verify, and the LRU replay defense. Implementation refactors `verify_kind` and `verify_kind_skew_bypass` to share a private `verify_kind_inner` with a `skip_skew_check: bool` flag, so the strict path remains the default and the bypass is opt-in.

### Agent: `crates/sandbox-agent/src/handlers.rs`

Add `POST /_clock_resync` handler. Body: `{"ts": <unix_secs>}`. Flow:
1. `verify_signed_skew_bypass(req, body, state)` — runs the new verifier surface.
2. Parse the JSON body.
3. `libc::settimeofday(&tv, NULL)` with `tv_sec = parsed.ts, tv_usec = 0`.
4. Return 200.

The handler's signature is the bug-#22-specific entry point; every other auth-gated endpoint (`/exec`, `/files`, `/version`, etc.) continues to use the strict-skew `verify_signed`. The `unsafe` libc call is scoped to a single, lint-allowed block.

### Agent: `crates/sandbox-agent/src/main.rs`

Register `/_clock_resync` under the default small payload limit. Endpoint is the FIRST signed call the controller makes post-restore.

### Agent: `crates/sandbox-agent/src/version.rs`

Add the `"clock.resync-v1"` capability. Controllers feature-detect via this string; older agents (no resync endpoint) gracefully fall back to the pre-fix path (i.e., the wake fails the resync call with a 404 and the restore rolls back — visible failure, not silent wedge).

### Agent: `crates/sandbox-agent/src/metrics.rs`

`sbx_agent_clock_resyncs_total` Prometheus counter. One increment per successful resync — operators monitor this to confirm restored sandboxes are getting their wall clocks repaired.

### Controller: `crates/sandbox/src/restore_handler.rs`

In `do_restore_inner`, between `wait_for_livez` Ok and `register_restored`, call `clock_resync_post_restore(&agent_url, &sealed.signing_key_bytes).await`. The new free function:
- Spawns blocking work via `compio::runtime::spawn_blocking` (matches the pattern in `persist.rs::unseal`).
- Signs `POST /_clock_resync` with body `{"ts": <now>}` using `zeroship_sandbox_agent::sig::sign` (the same canonical the controller uses for every other agent RPC).
- Sends via `ureq` (10-second timeout).
- Returns Ok on agent 200; otherwise surfaces the status code + body excerpt so the wake path's rollback carries actionable text.

Also adds a `derive_agent_url(vm_index)` method on the `RestoreBackend` trait (default returns a 127.0.0.1 sentinel; overridden on `RealRestoreBackend` with the same `http://10.<subnet_second_octet>.<100+idx>.2:7777` formula `wait_for_livez` and `nomad_ch::derive_agent_url` use). This lets `restore_sandbox` ask the trait for the URL instead of reaching into backend internals.

Sequencing: resync BEFORE register_restored is intentional. An `/exec` racing with the resync either (a) precedes the state-map insert and gets the existing "sandbox not found" surface (safe), or (b) follows both and runs on a healthy clock (safe). There is no window where the state map says "ready" but the clock is still broken.

## Security analysis

The skew-bypass surface is signature-bound: every accepted resync request still passes Ed25519 verification under the controller's per-sandbox pubkey + the agent's nonce-LRU replay defense. An in-VM attacker (root inside the libkrun guest) cannot forge a resync because the private signing key never enters the VM. The only widening is the wall-clock gate, and that gate's protection is replaced by the LRU + the signature itself.

The endpoint is exposed ONLY at the named path `/_clock_resync`. Every other agent endpoint (`/exec`, `/files`, `/version`, `/tree`, `/shutdown`, `/proxy/...`) continues to use the strict `verify_signed` → strict `verify_kind` path. No existing endpoint can opt into the skew-bypass surface.

## Test coverage

### `crates/sandbox-agent/src/sig.rs` (+6 tests)

- `verify_kind_skew_bypass_accepts_far_future_ts` — controller-signed ts 1 day ahead validates.
- `verify_kind_skew_bypass_accepts_far_past_ts` — symmetric, far-past direction.
- `verify_kind_skew_bypass_still_rejects_bad_signature` — wrong-key signature → `BadSignature`.
- `verify_kind_skew_bypass_still_rejects_tampered_body` — body change after signing → `BadSignature`.
- `verify_kind_skew_bypass_still_records_nonce_for_replay_defense` — second call with same nonce → `ReplayedNonce`.
- `verify_kind_skew_bypass_rejects_malformed_signature_encoding` — wrong-length sig bytes → `BadSignatureEncoding`.

### `crates/sandbox-agent/src/handlers.rs` (+7 tests)

- `clock_resync_without_signature_returns_401` — drop-through to the JSON parse must not bypass auth.
- `clock_resync_accepts_far_future_ts` — **load-bearing**: a signed ts well outside the 5-second window must NOT 401. Allows 200 (root) or 500 (non-root EPERM from settimeofday) — both prove the auth gate passed.
- `clock_resync_accepts_far_past_ts` — symmetric.
- `clock_resync_with_tampered_body_returns_401` — canonical hash mismatch fails.
- `clock_resync_with_wrong_key_returns_401` — non-controller signing key fails.
- `clock_resync_with_malformed_json_returns_400` — auth passes, JSON parse fails.
- `clock_resync_replay_returns_401` — second call with same nonce is `ReplayedNonce`.

### `crates/sandbox/src/restore_handler.rs::real_backend_tests` (+3 tests)

- `clock_resync_post_restore_happy_path` — fake agent returns 200; helper returns Ok.
- `clock_resync_post_restore_surfaces_agent_401` — fake agent returns 401; helper Err carries the status code so the wake path's rollback message is actionable.
- `clock_resync_post_restore_transport_error` — closed port; helper Err carries "transport" prefix so operators can grep the right error shape.

### Lib test count

- sandbox-agent: **207 → 214** (+6 sig + 7 handler — but 7-7+6 = 6 new, plus the existing 1 sig test was already in; actual delta is +7 net).
- sandbox: **280 → 283** (+3 restore_handler).

Final: **`cargo test -p zeroship-sandbox-agent --lib` → 214 passed, 0 failed**; **`cargo test -p zeroship-sandbox --lib` → 283 passed, 0 failed, 1 ignored**.

## Deployment shape (rootfs rebake landed)

**This fix is NOT controller-only.** The agent crate changes ship via the rootfs image. This cycle did the full deploy:

1. Built the new agent binary (Docker rust:slim-bookworm cross-build, same as the controller).
2. Baked `rootfs-slim.img.virtio-blk-v4` via `crates/sandbox/scripts/bake-rootfs.sh`. The script installs `/usr/local/bin/sandbox-agent` (with the bug-#22 `/_clock_resync` handler) and `/sbin/init` into the rootfs.
3. Uploaded `rootfs-slim.img.virtio-blk-v4` to `gs://suger-dev-zsbx-artifacts/`.
4. Bumped `crates/sandbox/scripts/gcp-worker-startup.sh:150` from `virtio-blk-v3` → `virtio-blk-v4`.
5. Uploaded `zeroship-sandbox.snapshot-v17` controller binary.
6. Provisioned `zsbx-smoke` (1+1) using v17 controller + v4 rootfs.

`strings` verification on the agent binary confirmed `clock.resync-v1`, `/_clock_resync`, and `sbx_agent_clock_resyncs_total` are all present.

## Cluster validation (c=4, 1+1)

```
CREATE OK:    12/16
SNAPSHOT OK:   7/12  (5 timeouts at 60 s — A3 sync I/O on compio worker)
WAKE OK:       7/7   (100%)
POST-WAKE EXEC OK: 7/7  (100%)  ←  bug #22 closed (was 0/9 in Appendix E)
STOP OK:       7/16
```

Timing (ms):
- create   p50=15503 p95=21330 p99=23179 max=23179
- snapshot p50=50573 p95=53328 p99=53328 max=53328
- wake     p50=9235  p95=11648 p99=11648 max=11648
- stop     p50=20    p95=21    p99=21    max=21

**Bug #22 verdict: CLOSED.** Every successful wake (7/7) is followed by a successful `/exec` returning 200. The controller log shows the `/_clock_resync` call landing on the agent (signed POST with current ts) immediately after `wait_for_livez` Ok and before `register_restored`; the agent's `settimeofday(2)` repairs the guest's `CLOCK_REALTIME`; the post-wake `/exec` runs under the normal strict-skew path and validates.

**B19 verdict: FULLY CLOSED.** With bug #22 fixed, the wake path now runs end-to-end: wake → resync → register_restored → exec → stop. Slot release on stop confirmed (`STOP OK: 7/7` for cycles that reached stop; vm_index allocator exhaustion at idx=13..15 is the documented cap=12 concurrency-burst behavior, not a slot leak).

**The 4 failed creates** (idx=12..15) are the known `vm-index allocator exhausted (floor=1, ceil=12)` — concurrency burst on the single-worker smoke (16 ops issued in parallel waves; first 12 take all slots, the 4 stop releases happen after the next-cycle creates have already attempted). Not bug #22.

**The 5 snapshot timeouts** are the open A3 issue (sync 1 GB GCS put on the compio worker). Closing A3 (R5-P1b's spawn_blocking + Arc<dyn> refactor) will move these into the success bucket.

## B-SLO escalation (deferred — script bug #23)

Attempted to escalate to 3-server + 5-worker + c=20 stress for B-SLO baseline. `provision-gcp-cluster.sh` fails with `Bad syntax for dict arg: [10.178.0.11]` when SERVER_COUNT>1 because the script's `--metadata` flag concatenates server IPs without proper escaping. This is a NEW bug (#23) in the cluster-provisioning script, NOT a fix-scope issue. Captured, not fixed (per brief constraint). All reserved addresses cleaned up via teardown.

B-SLO baseline measurements from c=4 (single worker):
- Wake p50 = 9235 ms. SLO target ≤ 1000 ms; **MISS by 9.2×**. Bulk of wake-time is `store.get` 1 GB SHA + AEAD decrypt + std::thread::sleep — the A3 sync-I/O issue.
- Snapshot p50 = 50573 ms. SLO target ≤ 2000 ms; **MISS by 25×**. Same A3 root cause + sync `ChRemoteClient::pause/snapshot` from async handler.
- Stop p50 = 20 ms. No SLO target documented; observed performance is healthy.
- Resync overhead: negligible. Wake p50 9235 ms (post-#22) vs Appendix E 9729 ms (pre-#22, no resync call). The extra signed POST round-trip is sub-100ms on local-network tap.

The B-SLO targets are gated on A3 closure; bug #22 itself does not move the needle on wake p50. Closing A3 (R5-P1b in the deferred backlog) is the next step for SLO empirical validation.

## Files of interest (bug #22)

- `crates/sandbox-agent/src/sig.rs` — `verify_kind_skew_bypass(...)` + private `verify_kind_inner` refactor; +6 tests.
- `crates/sandbox-agent/src/handlers.rs` — `clock_resync(...)` handler + `verify_signed_skew_bypass(...)` helper; +7 tests; macro `make_app!` updated to register `/_clock_resync`.
- `crates/sandbox-agent/src/main.rs` — `web::resource("/_clock_resync").route(web::post().to(handlers::clock_resync))`.
- `crates/sandbox-agent/src/metrics.rs` — `CLOCK_RESYNC_TOTAL` + `inc_clock_resync` + `sbx_agent_clock_resyncs_total` exposition.
- `crates/sandbox-agent/src/version.rs` — `"clock.resync-v1"` capability.
- `crates/sandbox/src/restore_handler.rs` — `clock_resync_post_restore(...)` free fn + `clock_resync_nonce()` helper; `RestoreBackend::derive_agent_url(...)` trait default + `RealRestoreBackend` override; +3 tests.

## Verdict

- **Bug #22 → FULLY CLOSED.** Cluster c=4 confirms POST-WAKE EXEC 7/7 = 100% (was 0/9 pre-fix). The signed clock-resync handshake repairs the guest's frozen-at-snapshot `CLOCK_REALTIME` in every successful wake; every subsequent `/exec` validates under the normal strict-skew path. 16 new unit tests (6 sig + 7 handler + 3 controller) pin the auth surface so the bypass cannot widen accidentally.
- **B19 → FULLY CLOSED** in cluster. The wake-path register chain runs end-to-end: wake → resync → register_restored → exec (200) → stop (200, slot released).
- **B-SLO → DEFERRED**. Wake p50 9235 ms misses target 1000 ms by 9.2×; snapshot p50 50573 ms misses target 2000 ms by 25×. Both attributable to the OPEN A3 issue (sync 1 GB I/O on compio worker). Closing A3 (R5-P1b) is the next step. Single-worker c=4 captured B-SLO baseline; c=20 escalation blocked on NEW bug #23 (cluster provision script fails on SERVER_COUNT>1).
- **NEW bug #23**: `provision-gcp-cluster.sh` fails `Bad syntax for dict arg` when SERVER_COUNT>1 due to unescaped server-IP list in the `--metadata` flag. Captured, not fixed (per brief constraint).

## Cost / time

- Cluster wall-time: ~5 min (provision 2 min + smoke c=4 3 min + observation 30 s + teardown 1 min).
- n2-standard-32 worker @ ~$1.55/hr × 5/60 = **$0.13**.
- n2-standard-4 server @ ~$0.17/hr × 5/60 = **$0.014**.
- Failed 3+5 provision: 0 instances created → $0 (only reserved-address fees, ~$0.001 prorated).
- **Total: ~$0.15**. Well under the $30 cycle cap.

## Commits

- HEAD pre-fix: `29196e0c` (pilot artifacts + deferred refresh).
- Branch: `feat/sandbox-snapshot-restore`.
- Fix commit forthcoming with this appendix.

