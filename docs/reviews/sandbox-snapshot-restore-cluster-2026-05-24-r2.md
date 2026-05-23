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
