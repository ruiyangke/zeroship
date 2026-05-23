# Phase B cluster validation — 2026-05-25 r1 (v18 + rootfs v5 / R8 stack)

**Branch HEAD:** `dfd1a43d` — `pilot: r9 reviewers + critical-fix sweep closure (8 fixes landed, 1 no-op)`.
**Worktree:** `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.
**Operator:** end-to-end cluster smoke (post critical-fix sweep).
**Verdict:** **HARD FAIL on Phase 1 c=4: 0/16 CREATE succeeded.** Phase 2 c=20 NOT REACHED.
**New bug discovered:** **#24** — controller's Nomad task `Env` block in `crates/sandbox/src/backend/nomad_ch.rs:2224-2264` is missing `ZSBX_SANDBOX_ID`; the wrapper's R8-DEPLOY1 cold-boot guard (`/etc/zeroship/nomad-vm-wrapper.sh:153`) fails-closed with `ZSBX_SANDBOX_ID: missing ZSBX_SANDBOX_ID (R8-DEPLOY1)`. **Captured, NOT fixed** per task constraints.

## TL;DR

The R8-DEPLOY1 fix landed in two places — the wrapper (which validates `ZSBX_SANDBOX_ID`) and the comment-only doc in `nomad_ch.rs:2240` — but the **actual env injection in the controller's `Tasks[].Env` JSON never landed**. Result: every Nomad alloc the controller submits gets killed by the wrapper before CH spawn, with the failure surface "Exit Code: 1" at ~50ms task lifetime and **zero bytes written to ch.stderr** (the wrapper dies on the bash `:?` parameter expansion before any explicit logging fires; bash *does* print "ZSBX_SANDBOX_ID: missing ZSBX_SANDBOX_ID (R8-DEPLOY1)" to stderr, but Nomad's raw_exec fifo is closed by the time it lands — captured stderr file is 0 bytes).

## Build / rebake outcome

| Artifact | Status | GCS object |
|---|---|---|
| Controller v18 | **OK** (Docker `rust:slim-bookworm`, 30.99s, debian-portable PT_INTERP) | `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v18` (16,356,920 bytes, MD5 fresh 20:04:44Z) |
| Rootfs v5 | **OK** (bake-rootfs.sh + patched debian-built sandbox-agent + init.sh) | `gs://suger-dev-zsbx-artifacts/rootfs-slim.img.virtio-blk-v5` (629,145,600 bytes, uploaded 20:05:04Z) |
| Wrapper | unchanged (`f0ebf783` build from 19:45:55Z) | `gs://suger-dev-zsbx-artifacts/nomad-vm-wrapper.sh` (33,070 bytes) |
| `gcp-worker-startup.sh` | bumped `virtio-blk-v4 → virtio-blk-v5` | source edit only (lines 11, 154) |

Controller PT_INTERP: `/lib64/ld-linux-x86-64.so.2` (debian-portable, no patchelf needed because built in debian-trixie Docker).
Sandbox-agent: built same path, patched + baked into rootfs by `bake-rootfs.sh`.

## Phase 1 c=4 outcome (1+1 cluster)

Cluster bringup: **OK in 105s** (server sentinel @60s, worker sentinel @45s).

Smoke (`snapshot_stress.py --concurrency 4 --cycles 4`):

| Op | Result |
|---|---|
| CREATE OK | **0/16** |
| SNAPSHOT OK | 0/0 (gated on CREATE) |
| WAKE OK | 0/0 (gated on SNAPSHOT) |
| POST-WAKE EXEC | 0/0 (gated on WAKE) |
| STOP OK | 0/16 (no sandboxes to stop) |
| FAILED CREATES | **16/16** with identical error: `backend_create_failed: nomad alloc terminal status=failed: Failed tasks` |
| Wall time | 2.0s (every alloc fails in ~50ms) |

No latency stats collected — every cycle short-circuited at CREATE.

## Phase 2 c=20 outcome (3+5 cluster)

**NOT REACHED.** Phase 1 hard-failed; per the gate rule we did not provision the stress cluster.

## Root cause analysis — new bug #24

### Symptom

Every Nomad alloc surfaces `Exit Code: 1` ~50ms after task start; `ch.stderr.0` is empty in the alloc dir; only the executor.out preamble lines are produced. Controller log shows:

```
sandbox/nomad-ch create error step=wait_for_alloc_running
error=nomad alloc terminal status=failed: Failed tasks
```

### Investigation

1. Captured a live alloc's working directory mid-flight (`/tmp/zsbx-diag-alloc`) — stderr/stdout files exist but are 0 bytes.
2. Ran the wrapper manually as `nobody` on the worker host, with the exact env block the controller injects (verified against `crates/sandbox/src/backend/nomad_ch.rs:2224-2264`):

   - Without `ZSBX_SANDBOX_ID`: `line 153: ZSBX_SANDBOX_ID: missing ZSBX_SANDBOX_ID (R8-DEPLOY1)` → exit 1.
   - With `ZSBX_SANDBOX_ID` added: proceeds past env validation, dies further down on `tap zsbx-nm-99 missing` (expected — we used a sentinel index outside the provisioned tap range).

3. Grepped controller source: `grep -rnE "ZSBX_SANDBOX_ID|SANDBOX_AGENT_SANDBOX_ID|R8-DEPLOY1" crates/sandbox/src/` → **zero matches**. The R8-DEPLOY1 fix never landed on the controller side.

### Mechanism

`nomad_ch.rs` lines 2224-2264 enumerate the env keys the controller bin-puts into the Nomad jobspec:

```
ZSBX_VM_INDEX, ZSBX_ARTIFACT_DIR, ZSBX_RUNTIME,
ZSBX_WORKSPACE_IMG, ZSBX_USER_HOME_IMG, ZSBX_PUBKEY_HEX,
ZSBX_VM_MEMORY_MB, ZSBX_VM_CPUS_BOOT, ZSBX_SUBNET_BASE_OCTET
```

The wrapper's R8-DEPLOY1 guard at line 153 demands `ZSBX_SANDBOX_ID` on every cold boot (skipped only when `ZSBX_RESTORE_FROM` is set — i.e. restore branch). Since the controller never injects it, the very first cold boot dies in env validation.

### Why ch.stderr is empty

bash's `${VAR:?msg}` writes to **stderr of the script**, which Nomad raw_exec wires to a fifo (`/tmp/zsbx-diag-alloc/alloc/logs/.ch.stderr.fifo`). On a sub-50ms exit the fifo reader (Nomad logmon) is still starting; the bytes are dropped on the floor. This is a Nomad ergonomic gotcha, not a wrapper bug — but it made the failure surface look like "task started and immediately died with no diagnostic" instead of "task printed an env error".

### Files implicated

- `crates/sandbox/src/backend/nomad_ch.rs:2224-2264` — Tasks[].Env block missing `"ZSBX_SANDBOX_ID": sandbox_id.to_string()` (or equivalent).
- `crates/sandbox/scripts/nomad-vm-wrapper.sh:153` — the guard that fires (R8-DEPLOY1).
- `crates/sandbox-agent/src/main.rs:97-102` — the downstream agent-side hard-error this guard exists to prevent.

The proposed fix is one line in the controller's jobspec env block. Not implemented in this cycle per the "don't fix new bugs" task constraint.

## B19 + B-SLO verdicts

| Item | Verdict | Reason |
|---|---|---|
| **B19** | **UNVERIFIED at cluster** (in-code: was CLOSED in r7 B22-fixer). | Phase 1 never reached the wake path; the wake-side `register_restored` chain was not exercised. Prior r7 cycle confirmed FULLY CLOSED at cluster c=4 (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r2.md` Appendix F). The r9 critical-fix sweep landed atop that, did not regress B19 in code, but **could not be re-verified at cluster** due to #24. |
| **B-SLO** | **UNMEASURED.** | Wake/snapshot p50/p95/p99 require successful end-to-end cycles; none occurred. No numbers to report against §10.2 targets. |

## Teardown status

| Cluster | Status |
|---|---|
| zsbx-smoke (1+1) | Fully torn down. `[teardown] OK: cluster fully torn down`. |
| zsbx-stress (3+5) | Never provisioned (Phase 2 not reached); teardown returned `no instances to delete`. |
| Survivors | 0 compute instances. Firewall rules + diag/prod legacy networks retained (operator-managed, no per-cluster cost). |

```
$ gcloud compute instances list --filter='name~"^zsbx-"'
(empty)
```

## Cost

- 1 × n2-standard-4 server + 1 × n2-standard-32 worker, asia-northeast3, ~14 minutes provisioned wall time.
- Estimated burn: well under $1 (n2-standard-32 ≈ $1.95/hr; 14min ≈ $0.45 + server overhead). Stress cluster not provisioned — no additional cost.
- Total cycle cost: **~$0.50** (well under the $30 cap).

## Recommended next cycle

1. Bug #24 fix: one-line env injection in `nomad_ch.rs` cold-boot branch. Trivial scope.
2. Re-run Phase 1 c=4 to confirm CREATE recovers and full create→snapshot→wake→exec→stop chain works.
3. Only then escalate to Phase 2 c=20 for B-SLO measurement.
4. The R8-DEPLOY1 fix should also have a unit test asserting `task["Env"]["ZSBX_SANDBOX_ID"]` is populated — `nomad_ch.rs:3667` already has the test shape (`assert_eq!(task["Env"]["ZSBX_VM_INDEX"], "7")`); add a sibling assertion.

## Appendix A — diag commands (for reproduction)

Captured live alloc dir:
```
$ sudo find /opt/nomad/data/alloc -maxdepth 1 -mindepth 1 -type d | head -1
/opt/nomad/data/alloc/bf315fae-f277-a65b-3585-116348c00799
$ sudo stat .../alloc/logs/ch.stderr.0
  Size: 0   (regular empty file)
```

Manual wrapper reproduction (without ZSBX_SANDBOX_ID):
```
$ sudo -E -u nobody env ZSBX_VM_INDEX=99 ZSBX_ARTIFACT_DIR=/var/lib/zeroship/ch \
    ZSBX_RUNTIME=/tmp/test ZSBX_VM_MEMORY_MB=512 ZSBX_VM_CPUS_BOOT=1 \
    ZSBX_WORKSPACE_IMG=/tmp/ws.img ZSBX_USER_HOME_IMG=/tmp/uh.img \
    ZSBX_PUBKEY_HEX=<64-hex> ZSBX_SUBNET_BASE_OCTET=99 \
    /etc/zeroship/nomad-vm-wrapper.sh
/etc/zeroship/nomad-vm-wrapper.sh: line 153: ZSBX_SANDBOX_ID: missing ZSBX_SANDBOX_ID (R8-DEPLOY1)
```

Manual wrapper reproduction (with ZSBX_SANDBOX_ID):
```
$ sudo -E -u nobody env ... ZSBX_SANDBOX_ID=01ASDF... /etc/zeroship/nomad-vm-wrapper.sh
[wrapper] FATAL: tap zsbx-nm-99 missing — host setup script did not pre-create it
```
(Proceeds past env validation, dies on a different gate — confirms #24 is the only blocker on cold-boot env.)

## Appendix B — controller log sample (truncated)

```
{"level":"WARN","sandbox/nomad-ch create error","step":"wait_for_alloc_running",
 "error":"nomad alloc terminal status=failed: Failed tasks"}
{"level":"INFO","sandbox/nomad-ch vm_index released",
 "reason":"create-failure-cleanup"}
```

All 16 stress cycles produced this exact pair; no other failure mode observed.
