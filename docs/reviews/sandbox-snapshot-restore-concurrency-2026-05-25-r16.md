# Sandbox/snapshot-restore — concurrency r16 review

Date: 2026-05-25 (UTC)
HEAD at audit: `2e9ae598` (worktree, branch `feat/sandbox-snapshot-restore`).
Round 16 of N. READ-ONLY.

Prior round: `docs/reviews/sandbox-snapshot-restore-concurrency-2026-05-25-r15.md`
(authored at `d392a308`).

## Summary

- 6 NEW findings (2 IMPORTANT, 2 MINOR, 1 observability, 1 informational
  confirmation), 1 carry-forward CLOSED (R15-I2 → C-8b), 9 still open.
- **Sibling detach audit (prompt §4): two of the six sites in deferred-C-6's
  "Sibling sites audited" list at `docs/reviews/sandbox-snapshot-restore-deferred.md:99`
  are STILL misclassified as "safe" — `sweep.rs:611` (idle-eviction) and
  `registry.rs:870` (idle-GC). Both `.await` on `backend.{stop,
  teardown_source_for_snapshot}` inline on the same compio runtime that
  hosts wake handlers. Same C-6 wedge shape. Filed as [R16-I1]** (escalates
  carry-forward R14-C1 + R14-I2).
- **R15-I2 CLOSED by C-8b**: the 2× teardown-estimate factor envelopes the
  OK-case teardown inside the 50 s deadline-bounded budget. **But the LEAK
  case is unaddressed** — `wait_for_agent_silent`'s 2-consecutive-misses
  contract leaks vm_index when the agent's death-rattle answers every
  other probe (smoke-r10 logged `consecutive_misses=1` at the 30 s
  deadline). Filed as [R16-I2].
- **AEAD KEK lifetime verified safe**; the unsealed per-sandbox signing
  key bytes, however, live on the future heap across `clock_resync.await`
  and `SealedAuth` has no Zeroize impl. Filed as [R16-M1].
- **No continuation-state leak in `reserve_vm_index_with_retry`** under
  ntex cancellation — pg client released by `read_snapshot_row` before
  the sleep loop begins; no mutex held across `.await`. Cancellation is
  invisible from logs (no Drop instrumentation) — R16-D1, re-pin of R14-D1.
- **Lessee CAS recovery safe under partition + restart** — pg row-locking
  serializes; vm_index is host-local so no cross-host double-claim possible.
- **OS-thread teardown re-audit (prompt §3) CONFIRMED SAFE** — all locks
  (std::sync) held briefly, never across `.await`.

## Per-prompt-question audit

### Q1 — `reserve_vm_index_with_retry` cancellation + continuation state

`restore_handler.rs:429-487`. Loop frame holds: `attempt: u32`, `last_err:
Option<String>`, `retry: VmIndexRetryPolicy`, `backend: &dyn RestoreBackend`,
`sandbox_id: Uuid`. No Drop instrumentation on the future.

**Continuation-state audit**: the wake call path is `wake_sandbox`
(`admin_handlers.rs:1419`) → `restore_sandbox` (`restore_handler.rs:498`) →
`read_snapshot_row` (`:666-707`) → `do_restore_inner` (`:710`) →
`reserve_vm_index_with_retry` (`:745`). `read_snapshot_row` acquires
`pool.get().await` at `:673` and DROPS the client at function return —
NOT threaded into `do_restore_inner`. **No pg connection held across
the sleep.** `backend.reserve_vm_index` is sync and takes
`vm_index_allocator.lock()` briefly without crossing `.await`. **No
mutex held at cancellation point.**

Cancellation behavior: ntex drops the future at the 60 s client deadline
mid-`compio::time::sleep(2s).await`. Last visible log line is the
per-attempt INFO from `:447-454` for attempt N; attempt N+1's log never
appears. The breadcrumb is presence-side (the C-7 fix at `493d6c1e`).

Filed as **[R16-D1]** (re-pin of R14-D1/R15-D1; LOW).

### Q2 — `wait_for_agent_silent` 100 ms × 2-misses × C-8b 2× factor (`nomad_ch.rs:3205-3273`)

C-8b widened the wake retry envelope from `(fence - HEADROOM)` to
`(2*fence - HEADROOM)`, capped at `DEADLINE - HEADROOM = 50`. Helps when
teardown wall-time is ~2× fence (the OK case). **Does NOT change the
fence itself** at `nomad_ch.rs:1100` — the fence still uses
`fence_timeout_secs` directly (30 s at cluster-C-8).

The pathology C-8b doesn't address: an agent whose death-rattle answers
EVERY OTHER probe keeps `consecutive_misses` at ≤1 for the entire fence
budget. Smoke-r10's `consecutive_misses=1 at the 30 s deadline → leak`
log line is the empirical signal. Under this case the fence times out →
`release()` NOT called (`:1138`) → vm_index leaked until next-boot
orphan-prune. The wake's 50 s retry budget runs against a slot that
will never free.

Filed as **[R16-I2]** (IMPORTANT; structurally distinct from R15-I2 —
R15-I2 was budget math, R16-I2 is the leak-case the 2× doesn't help).

### Q3 — OS-thread teardown shared-state re-audit (`admin_handlers.rs:1339-1385`)

`state_for_teardown` holds: `NomadCHBackend.state` (`Arc<RwLock<HashMap>>`,
write at `nomad_ch.rs:991-998` then dropped), `vm_index_allocator`
(`Arc<Mutex<>>`, briefly at `:1134-1138`), `creating_users` (untouched by
teardown), `persist` (ref only). pg pool NOT referenced in `stop_inner`.
All locks are std::sync, never crossing `.await`. The thread mints its
own `compio::runtime::Runtime::new()` via `block_on` — independent of
the ntex-worker runtime. No deadlock potential.

Filed as informational **[R16-V1]** (CONFIRMED SAFE; no actionable finding).

### Q4 — All `compio::runtime::spawn(...).detach()` sites

Grepped `crates/sandbox/src/` and `crates/sandbox-agent/src/` — 11 sites:

| File:line | Purpose | Risk |
|---|---|---|
| `sandbox/src/main.rs:120` | preview_ws server accept | LOW |
| `sandbox/src/sweep.rs:253` | transient-takeover loop | LOW (DB-only) |
| `sandbox/src/sweep.rs:611` | idle-eviction loop | **MEDIUM** — inline `teardown_source_for_snapshot.await` at `:378`; same C-6 shape |
| `sandbox/src/registry.rs:870` | idle-GC loop | **MEDIUM** — inline `backend.stop(id).await` at `:861`; same C-6 shape |
| `sandbox/src/preview_ws.rs:105` | per-connection WS handler | LOW (I/O-bound, per-conn) |
| `sandbox/src/lib.rs:1008` | health re-probe loop | LOW (single HTTP per tick) |
| `sandbox/src/lib.rs:1108` | HA heartbeat loop | LOW (single SQL per tick) |
| `sandbox/src/lib.rs:1422` | HA takeover scan | **MEDIUM** — `rehydrate_after_takeover.await` is multi-await burst on take-over events |
| `sandbox/src/backend/nomad_ch.rs:2127` | create-failure Drop guard | LOW (only on create panic) |
| `sandbox-agent/src/main.rs:127` | proxy_ws server accept | LOW |
| `sandbox-agent/src/proxy_ws.rs:157` | per-connection proxy handler | LOW |

Three MEDIUM: sweep.rs:611 + registry.rs:870 → **[R16-I1]** (re-pins
R14-C1/R14-I2). The third (`lib.rs:1422` takeover scan) → **[R16-M2]**.

### Q5 — Lessee CAS double-claim under partition + restart

Recovery SQL at `db.rs:2621-2647` predicates on `host_id = $expected AND
generation = $expected AND lessee_updated_at < now() - threshold`. Two
replicas racing: pg row-locking serializes; second sees `generation =
N+1` and predicate fails. vm_index is host-local (allocator per-process)
— no cross-host collision possible. Partitioned host A's restart filters
its own rows via `host_id <> my_host` at `db.rs:2499`. **No double-claim.
No finding.**

### Q6 — AEAD KEK + per-sandbox signing-key lifetime across `.await`

`Persistence::unseal` (`persist.rs:698-707`) clones the KEK `Arc` into
the `spawn_blocking` closure; the KEK material is referenced ONLY inside
the blocking-thread stack. The closure returns `SealedAuth` (no KEK).
**KEK lifetime is bounded to spawn_blocking.** ✓

**But**: `SealedAuth.signing_key_bytes` (32 bytes, `[u8; 32]`) lives on
the suspended-future heap from `restore_handler.rs:990` (unseal) through
`:1024+` (register_restored MOVE), crossing `clock_resync_post_restore.
await` at `:1007-1017` (HTTP signed RPC, 100 ms - 10 s under contention).
`SealedAuth` at `persist.rs:128-156` derives only `Clone, Debug, Serialize,
Deserialize` — **no Zeroize, no Drop impl**. Heap residual after Drop.

The doc comment at `persist.rs:136-139` says callers "must wrap in
`Arc<SigningKey>` promptly … and avoid copying these bytes around" — the
current restore flow violates the comment's intent.

Filed as **[R16-M1]** (MINOR; concurrency surface = the cross-await
lifetime; security surface = the heap residual).

## Findings

### [R16-I1] Sibling-C-6 sites (`sweep.rs:611`, `registry.rs:870`) STILL misclassified as "safe" in deferred audit (IMPORTANT)

- **Files**: `crates/sandbox/src/sweep.rs:611` + `:378` (inline teardown
  await); `crates/sandbox/src/registry.rs:870` + `:861` (inline stop
  await).
- **Misclassification at**: `docs/reviews/sandbox-snapshot-restore-deferred.md:99`,
  the "Sibling sites audited" list under the C-6 closure entry, parenthetically
  noted as "(same shape — safe)" / "(idle-GC loop — safe)".
- **Why wrong**: both loop bodies chain to an `.await` on a teardown-class
  operation (`/shutdown` + fence + Nomad purge — up to ~60 s) running on
  the spawning ntex worker's compio runtime. Exact C-6 wedge fingerprint.
- **Why now**: under T-8b-stress at c=20, if (i) idle-eviction OR idle-GC
  fires for sandbox A on worker W, AND (ii) a wake for sandbox B lands on
  the same worker, the wake's `reserve_vm_index_with_retry` sleep gets
  starved — reproduces the original C-6 wedge. The misclassification has
  propagated 3 cycles (r14 → r15 → r16) without correction.
- **Action**: apply C-6's fix shape (`std::thread::Builder::spawn` +
  `compio::runtime::Runtime::new().block_on(...)`) at both call sites,
  mirroring `admin_handlers.rs:1358-1385`. Single PR, ~60 LOC.
  Update deferred-line-99 to mark these as PENDING.

### [R16-I2] `wait_for_agent_silent` fence-LEAK case unaddressed by C-8b's 2× factor (IMPORTANT)

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3205-3273` +
  `nomad_ch.rs:1098-1131` (caller).
- **Shape**: C-8b's 2× teardown-estimate envelopes the OK-case wake retry
  inside the 50 s deadline-bounded budget. But the FENCE budget itself
  (`fence_timeout_secs`, 30 s at cluster-C-8) is unchanged. An agent that
  answers every other probe keeps `consecutive_misses ≤ 1` for the full
  fence duration → fence times out → vm_index LEAKED until next-boot
  orphan-prune.
- **Empirical signal**: smoke-r10 logged `consecutive_misses=1 at the
  30 s deadline → leak` — evidence the alternating-answer pathology is
  happening at c=1. Rate likely climbs at c=20 with contended IO/CPU
  during simultaneous teardowns.
- **Action**:
  - Short-term: detect the alternating-answer pattern explicitly (sliding
    window of last 3-5 probe outcomes). WARNING log if pattern detected.
  - Medium-term: change the fence pass criterion to "3 of last 5 misses"
    instead of "2 consecutive" — tolerates one intermittent answer.
    Requires re-audit of FM-F's failure modes before adoption.
  - Long-term: explicit shutdown handshake from `/shutdown` (agent ACK
    after which agent commits to not answering) replaces the polling
    fence entirely.

### [R16-M1] `SealedAuth.signing_key_bytes` lives on future heap across `clock_resync.await`; no Zeroize (MINOR; concurrency × security)

- **File**: `crates/sandbox/src/persist.rs:128-156` (`SealedAuth` struct);
  use site at `crates/sandbox/src/restore_handler.rs:984-1029`.
- **Concurrency framing**: the unsealed 32-byte secret key is on the
  suspended-future heap for the duration of `clock_resync_post_restore.
  await` (100 ms - 10 s). On ntex cancel-mid-await the bytes are freed
  but not scrubbed.
- **Security framing**: heap residual after Drop. Post-process compromise
  (core dump, `/proc/<pid>/mem`) recovers the signing key in cleartext.
- **Action**: wrap `signing_key_bytes: [u8; 32]` in
  `zeroize::Zeroizing<[u8; 32]>`. Trivial — `Zeroizing` is `Deref<Target
  = [u8; 32]>`, existing borrows compile unchanged. Alternative: change
  to `ed25519_dalek::SigningKey` directly (already `ZeroizeOnDrop`).

### [R16-M2] HA takeover scan (`lib.rs:1422`) calls multi-await `rehydrate_after_takeover` inline on ntex worker runtime (MINOR)

- **File**: `crates/sandbox/src/lib.rs:1377-1422` (`spawn_takeover_task`);
  inline `.await` at `:1407`.
- **Shape**: per-30-s tick, if there are dead peers, the task loops over
  taken sandboxes and runs `probe_and_register_one` (multi-await HTTP +
  DB) INLINE on the ntex worker's compio runtime. Real takeover events
  could batch 10+ sandboxes × 5-30 s probe each = multi-minute burst.
- **Severity**: MINOR — low trigger rate (only fires on peer-death),
  but same C-6 shape. Worth treating during a fault-injection cycle that
  simulates peer-death + wake concurrent.
- **Action**: same as R16-I1, apply C-6 OS-thread shape. Lower priority.

### [R16-D1] `reserve_vm_index_with_retry`'s future has no Drop instrumentation; ntex cancellation invisible (LOW, observability)

- **File**: `crates/sandbox/src/restore_handler.rs:429-487`. Re-pin of
  R14-D1/R15-D1.
- **Shape**: when ntex drops the future at 60 s, the last log line is
  attempt N's INFO at `:447-454`. Attempt N+1 never appears. Operator
  infers cancellation only by absence.
- **Action**: wrap the loop in a guard struct with `impl Drop` that
  WARN-logs at cancel time, capturing `attempt`, `sandbox_id`, `vm_index`.
  Out-of-scope if C-7-LT (R15-A1) lands first.

### [R16-V1] OS-thread teardown shared-resource re-audit (informational, CONFIRMED SAFE)

- See Q3 above. No mutex / RwLock crosses `.await`. No pg pool reference
  in the thread. Independent compio runtime via `block_on`. No deadlock
  or starvation potential.

## Carry-forward audit

| Finding | Cycles | r16 status |
|---|---|---|
| **R4-A2** LeasedVmSlot RAII | 13 | OPEN. 13+ findings dissolve under correct RAII. |
| **R14-C1** sweep.rs:611 sibling-C-6 | 3 | OPEN — escalated as R16-I1 (sweep). |
| **R14-I2** registry.rs:870 sibling-C-6 | 3 | OPEN — escalated as R16-I1 (registry). |
| **R14-V1** do_restore_inner await count | 3 | OPEN. Outer=8 unchanged. Inner ≤26 (prod fence=30, C-8b). |
| **R15-I1** Config drift 120/30/60 | 1 | OPEN. C-8b didn't update. |
| **R15-I2** Wake budget < fence wall-time | 1 | **RESOLVED** by C-8b `64af1803`. R16-I2 successor. |
| **R15-D1** wait_for_agent_silent observability | 1 | OPEN. R16-I2 raises urgency. |
| **R11-C1/C2** unregister silent-fail-OPEN / rollback 2-await | 5 | OPEN. |
| **R10-Q3/S2/M2, R12-M1** registry RwLock / swallow / panic-format | 4-6 | OPEN (code-quality). |
| **R11-P1 / R13-I1** pool churn | 5 | OPEN, T-8b-stress gate. |

## Cross-lens consensus

- **C-7-LT (R15-A1)** remains the structural fix dissolving the 6-bug
  cluster (C-4 → C-8b). r16 endorses.
- **R16-I1** is the next-most-actionable concurrency item (~60 LOC PR).
- **R16-I2** is FRESH, not in C-7-LT scope — fence redesign needs its
  own thread (sliding-window pass OR explicit shutdown handshake).
- **R4-A2** continues to be highest-leverage (13 cycles, 13+ findings).

## Lens hand-off

- **Architecture r16**: re-audit deferred-line-99; mark sweep+registry
  as PENDING (3-cycle propagated misclassification).
- **Performance r16**: measure fence-leak rate under T-8b-stress c=20.
- **Test-coverage r16**: add unit test injecting an HTTP stub `OK,
  refused, OK, refused, ...` against `wait_for_agent_silent` — deterministic
  R16-I2 reproducer.
- **Security r16**: own R16-M1 SealedAuth Zeroize gap.

## Status block

```
Round 16:
  NEW: R16-I1 (sweep.rs:611 + registry.rs:870 sibling-C-6 STILL "safe"
       in deferred-line-99; same C-6 wedge shape; 3-cycle propagated
       misclassification; escalates R14-C1+R14-I2),
   R16-I2 (wait_for_agent_silent fence-LEAK case unaddressed by C-8b's
       2x factor; alternating-answer keeps consecutive_misses<2 to
       deadline → leak; smoke-r10 logged consecutive_misses=1 at 30s),
   R16-M1 (SealedAuth.signing_key_bytes lives across clock_resync.await,
       no Zeroize; concurrency-x-security),
   R16-M2 (HA takeover scan multi-await burst on ntex runtime),
   R16-D1 (re-pin R14-D1/R15-D1 — reserve_vm_index_with_retry cancel
       invisible),
   R16-V1 (informational — OS-thread re-audit CONFIRMED SAFE per §3).

  RESOLVED: R15-I2 (CLOSED by C-8b; R16-I2 successor for LEAK case).

  C-8b VERDICT: STRUCTURALLY CORRECT for OK case (2x grounded in
       smoke-r10 60.164s @ fence=30s; deadline ceiling binds for
       conservative fences). LEAK case (R16-I2) is orthogonal.

  ASK: (1) C-6 shape to sweep.rs:611 + registry.rs:870 (R16-I1).
       (2) Prioritize C-7-LT (R15-A1) — 6-bug cluster one redesign away.
       (3) R16-I2 fence-leak — observability + sliding-window pass.
       (4) R16-M1 SealedAuth Zeroize — trivial wrap.
```
