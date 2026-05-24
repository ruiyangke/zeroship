# Sandbox/snapshot-restore — concurrency r15 review

Date: 2026-05-25 (UTC)
HEAD at audit: `7469118e` (worktree). Prompt cites `2afbb2dd` as the C-8/C-8a closure; r15-Q1 (`7469118e`) layered on top is a cosmetic thread-name fix and touches no concurrency surface — both audited.
Round 15 of N. Read-only. Branch `feat/sandbox-snapshot-restore`.

## Summary

- 3 NEW findings (1 important — config-default drift, 1 important — fence-cap-vs-leak math, 1 verification), 1 carry-forward escalation, 1 carry-forward CLOSED.
- **C-8 + C-8a reconciliation audit (P1)**: the MIN-of-two-ceilings approach is structurally correct and the `host_fence_timeout_secs == 0` edge case is handled (yields 1 decisive attempt). **HOWEVER**: a config-default drift is now baked in — `NomadCHConfig::host_fence_timeout_secs` defaults to **120s** in `crates/sandbox/src/config.rs:401` while the cluster systemd unit overrides to **30s** via env. Two places, two numbers, doc text out of sync. Filed as **[R15-I1]**.
- **C-7 production observability (P2)**: smoke-r9 trace shows all 25 per-attempt INFO lines fired with byte-perfect 2s cadence. Observability for `reserve_vm_index_with_retry` is **COMPLETE**. **One latent gap**: `wait_for_agent_silent` (the fence loop) emits no per-poll log — only the final `host_fence: cleared` (Ok) or `host_fence: timeout` (Err). Under a tight 30s budget, an operator debugging a teardown wedge has no visibility into *which* poll-N showed agent-still-alive. Filed as **[R15-D1]** (low-priority observability).
- **30s host_fence safety implication (P3)**: the 30s fence is **at the empirically-measured edge of safety**. Config doc (`config.rs:394-400`) records `fence p95=29s, max=31.1s` under 30-way concurrent stop on n2-standard-32. Smoke-r9 was c=1 (single sandbox); c=20 stress will move p95 toward and likely past 31s. **Two consequences**:
  1. Under c=20, some teardowns will time out the 30s fence → vm_index **leak** (the fence's deliberate FM-F-safe behaviour). Leaks accumulate until orphan-prune at next controller boot. Filed as **[R15-I2]**.
  2. A wake racing a teardown whose fence-clear lands at 28s catches the slot release within the 20s retry budget (with C-8 fence=30). A wake racing one whose fence-clear lands at 31s misses (slot leaked; wake 503s; orphan-prune on next boot reclaims, but the wake never succeeds inside its 60s window). The 30s fence + 20s budget math leaves a narrow but real **fence-tail-vs-retry-budget gap**.
- **R14-C1 / R14-I2 still open**: confirmed — sweep.rs:563 idle-eviction-sweep and registry.rs:829 idle-GC are unchanged by C-8 (which is restore-handler + systemd-unit only). Both remain **admin-reachable runtime-starvation latent bugs** under T-8b-stress at c=20.
- **R14-C1 commit-message regression (carry-forward)**: the `91ce9be5` (C-6) commit message INCORRECTLY classified `sweep.rs:563` as "safe" — same error noted in r14. Untouched at HEAD. Audit-paper-trail debt.
- **do_restore_inner await count: 8** (unchanged from r14). C-7 (`493d6c1e`) reduced *inner* retry attempts 60→25 (so inner cancel boundaries dropped 60→25); C-8a (`2afbb2dd`) changed the *formula* deriving that count but not the structure. Outer-await count is the r14 number, verified.
- All r14 carry-forwards remain open. **R4-A2 LeasedVmSlot RAII** now **12+ cycles open** — would still dissolve R10-C1, R10-C2, R11-C1, R11-C2, R11-I1, R12-M1, six C3 widenings, R14-V1's narrow drop window — plus a new C-7-era window R15-V1 surfaces.

## Per-prompt-question audit

### Q1 — C-8 + R14-A6 reconciliation

`from_host_fence_timeout` at `crates/sandbox/src/restore_handler.rs:240-267`:

```rust
let max_budget_from_fence  = host_fence_timeout_secs.saturating_sub(CLIENT_HEADROOM_SECS);  // 10
let max_budget_from_deadline = CLIENT_DEADLINE_SECS.saturating_sub(CLIENT_HEADROOM_SECS);   // 60-10 = 50
let effective_budget = max_budget_from_fence.min(max_budget_from_deadline);
let attempts_from_budget = (effective_budget / INTERVAL_SECS).saturating_add(1);
let max_attempts = u32::try_from(attempts_from_budget).unwrap_or(u32::MAX).max(MIN_ATTEMPTS);
```

**Correctness**:
- `saturating_sub` on both ceilings means **fence < 10** does not panic — `max_budget_from_fence = 0` for `fence ∈ {0..=10}`. Combined with `MIN_ATTEMPTS = 1`, the policy never collapses to 0 attempts.
- `min(0, 50) = 0 → (0 / 2) + 1 = 1 → max(1, 1) = 1`. The doc says "still gets one decisive reserve attempt"; verified.
- Upper bound: for fence ≥ 60, MIN clamps to 50s budget = 26 attempts. For 60 ≤ fence < 60, fence-budget wins.
- `u32::try_from(u64).unwrap_or(u32::MAX)` — guards against pathologically large fences (e.g. `u64::MAX`); benign.

**Edge cases**:
- `fence == 0` (explicitly disabled): policy = 1 attempt. **Matches r14-A6 design intent** (recorded by `r14a6_policy_from_cfg_zero_fence_still_attempts_once` at `:1551`).
- `fence == 1..=10` (sub-headroom): policy = 1 attempt (fence-budget = 0 wins MIN). **No test pins this.** Filed as observation under **[R15-T1]** (test-coverage-r15 surface, not concurrency).
- `fence == 11..=20`: policy = 2..=6 attempts (e.g. 20 → 6 attempts × 2s = 10s wall-time, pinned by `r14a6_policy_from_cfg_short_timeout` at `:1535`).
- `fence == 60`: 26 attempts × 2s = 50s. Hits both ceilings simultaneously.
- `fence == 120` (Rust default): 26 attempts × 2s = 50s (capped). Pinned by `r14a6_from_cfg_caps_at_client_deadline` at `:1571`.
- `fence == 30` (cluster-systemd override): 11 attempts × 2s = 20s (fence-budget wins). **No dedicated test pins this number; covered transitively by `r14a6_policy_from_cfg_respects_host_fence_timeout`.**

**Verdict: STRUCTURALLY CORRECT.** No new concurrency hazard. The remaining concerns are (a) config drift between code default and systemd override (R15-I1) and (b) the math of "30s fence + 20s retry budget vs the c=20 fence-tail distribution" (R15-I2).

### Q2 — C-7 production observability validation

Smoke-r9 trace (per `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r9.md` § "Phase-by-phase wake trace"):

```
03:57:06.899  pre_reserve_vm_index
03:57:06.899  reserve_vm_index_with_retry attempt=1/25
03:57:08.899  attempt=2/25                            (2.000s cadence)
…
03:57:52.903  attempt=24/25
03:57:54.903  attempt=25/25
03:57:54.903  WARN exhausted retry budget; attempts=25 budget_ms=48000
```

**Coverage assessment**:
- Per-attempt INFO log: ✅ fires before EVERY `reserve_vm_index` call (`restore_handler.rs:413-420`). Visibility into which attempt-N is in flight at any cancellation point — was the critical gap r14-I1 / R14-D1 / C-7 diagnosed.
- Cadence proof: 25 attempts × 2s = 48s observed wall-time matches the policy (`max_attempts × interval` minus the first-attempt-zero-sleep = `(25-1) × 2 = 48s`).
- Exhausted-budget WARN: ✅ fires synchronously with the last attempt, **inside the deadline** (48s < 60s); ntex does not cancel.
- Success-after-retry log (line `:424`): not exercised in smoke-r9 (loop hit exhaustion); covered by `c7_returns_after_retry_logs_success` unit test (`:1430`).

**Observability gaps still present** (not regressions; pre-existing):
1. `wait_for_agent_silent` (the fence loop at `nomad_ch.rs:3205-3273`) emits NO per-poll log — only the terminal `host_fence: cleared` Ok or `host_fence: timeout` Err. Under a 30s budget with 100ms cadence = 300 polls, an operator debugging a wedge has no visibility into the consecutive-misses counter or last-status code at any intermediate point. Filed as **[R15-D1]**.
2. The wake handler's "future dropping" Drop-impl observability r14-D1 flagged is still not implemented. Smoke-r9 didn't need it (C-7 fix removed the silent-cancel path), but if a future regression re-opens the silent-cancel window (e.g. ntex deadline drift), there's still no Drop-side log to localize.

**Verdict: C-7 OBSERVABILITY COMPLETE for the retry loop.** Sibling gaps (R15-D1, drop-impl) are pre-existing and low-priority given C-7's fix removed the proximate need.

### Q3 — 30s host_fence safety implication

The fence is a **FM-F safety primitive** — its purpose is documented at `nomad_ch.rs:1062-1081` and `config.rs:374-401`:

> Nomad's "alloc terminal" lags the host-process tree (cloud-hypervisor + 3× virtiofsd + the bash wrapper) by 0.5–60 s under N=8 stress. Releasing the index while the previous tenant's agent is still listening on `10.99.<100+idx>.2:7777` is the exact race FM-A's fingerprint check papers over; this is the **primary** fix.

So `wait_for_agent_silent` (`nomad_ch.rs:3205-3273`) is checking that the agent has *stopped answering* on its IP — that the IP is safe to hand to a new tenant. The semantics are:

- 100ms poll cadence (`:3265`).
- Two consecutive misses (connect-refused, timeout, transport, or 5xx — but NOT 4xx, since "401 from a stale tenant" still means the socket is alive) → Ok (release vm_index).
- Deadline reached without two consecutive misses → Err (LEAK vm_index; orphan-prune on next boot reclaims).

**Empirical data from `config.rs:394-400`**:

> Was 30s pre-Phase-3 stress run; bumped to 120s after measuring 30-way concurrent stop on a single n2-standard-32 worker: **fence p95=29s, max=31.1s** — i.e., 30s is too tight when many CH processes tear down concurrently (worker IO/CPU contention during teardown, NOT the controller polling cadence). 60-way burst can push p99 well past 30s.

**Consequences of C-8's cluster-systemd 30s override at c=20**:
- The doc text was written for a **120s default**; C-8's systemd unit override is now **30s**, identical to the **30s pre-Phase-3** number that the comment ALREADY says is "too tight".
- Smoke-r9 was c=1 → fence-tail trivially below 30s (the single teardown's fence likely cleared in <5s, but smoke-r9 was torn down at `03:58:30` before the actual fence-clear could be observed — the *wake* gave up at 48s; the source teardown completed *some time after*).
- T-8b-stress at c=20 lands directly in the contention regime the 120s default was designed for. Two failure modes:
  1. **fence timeout → vm_index leak.** Bounded; orphan-prune reclaims at next controller boot. Operator-visible via `vm_index leak ... reason=host_fence_timeout` log. Net effect: vm_index pool shrinks during the stress run; recovers on controller restart. Filed as **[R15-I2]**.
  2. **wake races teardown whose fence-clear lands in the 20-30s window.** With C-8 fence=30 and C-8a's derived budget = `min(20, 50) = 20s`, a wake's last retry attempt is at t=20s (11 attempts × 2s = 20s wall-time). If the source's fence-clear (i.e., slot release) happens at t=21-30s, the wake **missed by 1-10s** — wake 503s with `vm_index_unavailable`, source teardown completes 1-10s later and the slot is reusable but already too late for THIS wake. This is **a wider hole than C-7's design point intended**.

**The two-ceiling math conflict**:

Under cluster-config-C-8 (`fence_timeout_secs=30`):
- `from_host_fence_timeout(30)` → fence-budget = 20s, deadline-budget = 50s → MIN = 20s → 11 attempts × 2s = **20s wall-time**.
- But the FENCE itself can take up to **30s** to clear (by design — that's its purpose).
- The wake therefore gives up **10s BEFORE the fence can possibly clear in the worst case**. This is mathematically guaranteed by `from_host_fence_timeout`'s `- CLIENT_HEADROOM_SECS = -10` term.

**The headroom is in the wrong place**. The 10s headroom was sized for "the client deadline is 60s, leave 10s for the response to return"; under fence-budget-wins (fence < 60), the 10s headroom instead subtracts from the fence-envelope. **A wake racing a fence-clear that lands at the configured `fence_timeout_secs` always misses.** This is a structural issue with the formula, not just an empirical edge case.

For C-8's cluster (fence=30): the wake retry should ideally budget `fence_timeout_secs + Nomad_purge_tail + small_headroom ≈ 30 + 5 + 2 = 37s`. The current formula yields 20s — **17s short of the worst-case fence clearance**.

The smoke-r9 commit message claims "12s residual headroom" against a "150s → 60s teardown reduction"; the actual budget arithmetic shows the wake retry caps at **20s** for the c=1 case, and that 20s is less than the worst-case fence wall-time (~30s). Filed as **[R15-I2]** (the formula's headroom is misaligned with its purpose under fence-budget-wins).

### Q4 — R14-C1 / R14-I2 status

Verified at HEAD (`7469118e`):
- `sweep.rs:563` (`spawn_idle_eviction_sweep` — `compio::runtime::spawn(...).detach()`) UNCHANGED. Last commit touching the file: `0e71e5c4` (sweep CAS predicate, pre-C-6). C-8 commits did not touch this file.
- `registry.rs:829` (`start_idle_gc` — `compio::runtime::spawn(...).detach()`) UNCHANGED. Last commit touching: `0b3bbca9` (lifecycle batch, way pre-C-6).
- Both still call into `teardown_source_for_snapshot` / `backend.stop(...).await` inline on the spawning ntex worker's runtime. Both still exhibit the C-6 shape — the only mitigation is the empirical observation that idle-eviction sweep + idle-GC rarely fire concurrent with admin/wake traffic in current smokes (c=1, 5-min runs).
- T-8b-stress at c=20 + an idle-eviction threshold pull-in (e.g. `SANDBOX_IDLE_SNAPSHOT_SECS=60`) will fire the sibling-C-6 wedge.
- **R14-C1 commit-message regression**: `91ce9be5`'s closing message still claims `sweep.rs:563` is "safe" (steady-state-loop framing). It is not — the loop body chains into `snapshot_one` → inline `teardown_source_for_snapshot.await`. This is the same misclassification r14 flagged. The closure text in `docs/reviews/sandbox-snapshot-restore-deferred.md` § C-6 still inherits the misclassification (line 99: "`crates/sandbox/src/sweep.rs:227,563` (... safe)"). Not fixed by the C-7 diagnosis-update text at line 100. Audit-paper-trail debt.

### Q5 — Budget reconciliation at c=20 stress: worst-case math

Three numbers in tension:

| Number | Source | Magnitude (cluster-C-8 config) |
|---|---|---|
| **A. Client deadline** | ntex/stress-client | 60 s |
| **B. Source teardown wall-time** | `stop_inner` = `/shutdown` + `wait_for_job_gone` + `wait_for_agent_silent` (+ host_dir rm + persist.delete) | `~5 + ≤30 + ≤30 + <1 + <1 ≈ 66 s` worst case (c=20 → fence-p99 likely past 30s) |
| **C. Wake retry budget** | `from_host_fence_timeout(30)` = 11 attempts × 2 s | 20 s |

**Mismatches**:
- **B vs A**: source teardown CAN exceed client deadline in the worst case (66s > 60s). The teardown happens detached on an OS thread (C-6 fix), so it doesn't share the client deadline; the *wake* (which DOES share the client deadline) is the future that must complete inside A.
- **C vs B**: wake retry (20s) is **less than half** the worst-case teardown (66s). A wake firing within milliseconds of a slow teardown will reliably 503. This is the C-8 commit's claim of "~12s residual headroom" — that headroom only exists if `B ≈ 60s` AND fence-clear lands at the **expected** time (not worst case).
- **C vs A**: wake retry (20s) is well inside client deadline (60s). The exhausted-budget 503 fires cleanly, no silent-cancel. **This is the only one of the three that's actually reconciled** (C-7's design point).

**Worst-case sequence at c=20**:

```
t=0    snapshot detaches teardown of source_A on OS thread.
t=0    wake_handler for sandbox_A fires.
t=0    wake_handler: reserve_vm_index attempt 1 → Err (source_A teardown still holds slot).
t=2-20 wake_handler: attempts 2-11 → all Err (source_A teardown still in fence).
t=20   wake_handler: exhausted retry budget → WARN log + 503 to client.
t=20+δ ntex flushes 503 response; client observes 503 at ~21s.
t=21+  source_A's wait_for_agent_silent still polling (fence-p99 at c=20 likely 25-35s).
t=~35  source_A's fence_passed → vm_index released.
t=~36  source_A's stop_inner returns; teardown OS thread exits.
       Slot is now reusable, but the wake gave up 15s ago.
```

**Operator-visible effect**:
- Failed wakes: 503 inside 60s with a clean `vm_index_unavailable` warn log. Strictly better than the pre-C-7 silent stall.
- Source teardown completes successfully ~10-20s after the wake gave up.
- The row state at `t=20` after wake-503: was set to `restoring` at `t=0`'s CAS-to-restoring. The wake's rollback closure (`restore_handler.rs:561-613`) CASes back to `snapshotted`. So the row is recoverable for a retry — but the operator/client must manually retry.

**T-8b-stress at c=20 will exhibit this pattern under any sustained snapshot-then-wake-immediately load.** The C-8 fix unblocks the silent-cancel mode (smoke-r9 confirmed); it does NOT close the **race window between teardown completion and wake retry exhaustion**. Per R15-I2.

The structural fix for B-vs-C is one of:
- C-7-LT (`202 Accepted` + polling): decouple wake from client deadline; retry budget can be 90+s. **Strongly recommended by the smoke-r9 review** as "the long-term fix". Out of scope for this round.
- Cross-worker fallback: pick ANY available vm_index across the fleet rather than only the source's slot. **Out of scope by § 5.1 "v1 forces vm_index = source vm_index"**.
- Smarter retry: increase per-cluster fence-budget bias (e.g. retry budget = `fence_secs + 20` to envelope the fence + a small Nomad purge tail). Would require lifting the 10s deadline-headroom for short fences. **Concrete proposal in R15-I2 action item.**

### Q6 — do_restore_inner await count: 8 (unchanged from r14)

Awaits in `do_restore_inner` (HEAD `7469118e`, lines 676-1045):

1. `:711` — `reserve_vm_index_with_retry(...).await?` (contains 11-26 inner sleep awaits depending on policy; 25 inner awaits for the default tested in smoke-r9).
2. `:767` — `compio::runtime::spawn_blocking(store.get).await`
3. `:875` — `compio::runtime::spawn_blocking(submit_restore_job).await`
4. `:903` — `compio::runtime::spawn_blocking(wait_for_livez).await`
5. `:956` — `p.unseal(sandbox_id).await` (inside `if let Some(p) = persist`)
6. `:978` — `clock_resync_post_restore(...).await` (inside same `if let Some(p) = persist`)
7. `:1026` — `db.update_sandbox_status(sandbox_id, Running, expected_generation, None).await?`
8. `:1033` — `db.clear_snapshot_metadata(sandbox_id, g2).await`

**Total: 8 outer awaits.** Identical to r14.

Inner-await delta (cancel/drop boundaries inside `:711`):
- r14 era: up to 60 inner `sleep(2s).await` per wake under contention (C-4's default).
- C-7 era (`493d6c1e` and after): up to 25 inner `sleep(2s).await` (default fallback) or 11 (production with cfg.fence=30 via R14-A6/C-8a's `from_host_fence_timeout`).
- Per-attempt INFO log adds NO `.await` (logging is sync); the boundary count is identical to the attempt count minus 1 (first attempt has no preceding sleep).

**Cancel-window count delta vs r14**: outer = 0 (still 8); inner = ≤60 → ≤25 (default) or ≤11 (production). **Net surface reduced** by C-7 + C-8a.

**Narrow cancel-window between `reserve_vm_index` success and `return Ok(())` from `reserve_vm_index_with_retry`**: still exists (no LeasedVmSlot RAII). r14 filed as **[R14-V1]** carry; pinned again here as part of R15-V1. C-7 + C-8a did not address this.

## R15-I1, R15-I2, R15-D1 (NEW)

### [R15-I1] Config drift: `NomadCHConfig::host_fence_timeout_secs` default = 120s vs cluster systemd override = 30s; doc text references both (IMPORTANT, concurrency-r15)

- **Files**:
  - `crates/sandbox/src/config.rs:401` — `host_fence_timeout_secs: u64` field, default 120 (set at lines 654, 870).
  - `crates/sandbox/src/config.rs:390-401` — doc comment: "`SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS` (default 120)".
  - `crates/sandbox/scripts/gcp-worker-startup.sh:466` — `Environment=SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30`.
  - `crates/sandbox/src/restore_handler.rs:201-202` — doc on `CLIENT_HEADROOM_SECS`: "leaves ≥10 s headroom under the 60 s ntex client deadline".
  - `crates/sandbox/src/restore_handler.rs:230-239` — derivation examples mention 30/60/120 cases but NOT which is the "current production default".
- **Drift**:
  - **Rust default**: 120s (config.rs line 654 in `NomadCHConfig::default`).
  - **Cluster systemd unit**: 30s (gcp-worker-startup.sh).
  - **r14a6 unit test docstring** at `:1498-1499`: "60 s host-fence (the platform default after the cad098e6 30→120 bump backed off to 60 in many configs)". This text **already** says "60 in many configs" — which doesn't match either the 120 Rust default OR the 30 cluster override.
  - **deferred file** at the C-8 closure: "Rust default (`crates/sandbox/src/config.rs`) left at the conservative 120 s — production deployments needing the longer drain window keep that default; cluster-smoke worker hosts opt down via this env override."
- **Why this is concurrency-relevant**:
  - `RealRestoreBackend::vm_index_retry_policy()` derives the wake-retry budget from `self.cfg.host_fence_timeout_secs`. If a test (or a deployment) instantiates `NomadCHConfig::default()` without setting the env var, the wake retry budget becomes **50s** (deadline-cap) under fence=120, not the 20s the cluster systemd unit produces. A wake racing a teardown in test will see different behaviour than the wake racing the same teardown in cluster.
  - Anyone who reads the formula doc (`:230-239`) sees the 120 example noted as "CAPPED — the pre-C-8a derivation gave 110 s here" — they may infer that 120 is still the "production" number, but it isn't on the cluster.
  - The fence-tail empirical data (`config.rs:394-400`: "fence p95=29s, max=31.1s ... 60-way burst can push p99 well past 30s") was the JUSTIFICATION for the 120s default. Reverting to 30s in systemd contradicts that data without amending the comment.
- **Severity rationale**:
  - Test-vs-cluster divergence is a real concurrency-observability hazard: smoke-r9 confirmed the 25-attempt × 2s = 48s default budget works under c=1; nobody has yet confirmed the 11-attempt × 2s = 20s derived budget works under c=20.
  - Operators reading the code expect "30s is too tight" (per config.rs:394-400) and see the systemd unit forcing 30s — surprise.
- **Action**:
  - **Short-term**: update the docstring on `host_fence_timeout_secs` to (a) record the 30s cluster-smoke override, (b) reference the 120s default's c=60-burst justification, (c) document the C-8 trade-off (smaller fence ⇒ higher leak rate under contention; smaller wake retry budget; smaller wake-race-loss window).
  - **Short-term**: align the unit-test docstring at `:1498-1499` to the real defaults (currently the comment claims "60 in many configs" which contradicts both the code default and the cluster override).
  - **Medium-term**: surface the **effective** `host_fence_timeout_secs` in the controller boot log so operators can see at a glance which value is live. (Today the value is buried in the controller's `NomadCHConfig` Debug print — not directly grep-able.)

### [R15-I2] Wake retry budget < fence wall-time under cluster-C-8 (fence=30 → budget=20s; wake gives up ≥10s before fence-clear's worst-case completion) (IMPORTANT, concurrency-r15)

- **File**: `crates/sandbox/src/restore_handler.rs:240-267` (`from_host_fence_timeout`).
- **Shape**: the formula bakes in `CLIENT_HEADROOM_SECS = 10` as a flat subtraction from BOTH ceilings:
  ```rust
  let max_budget_from_fence    = host_fence_timeout_secs.saturating_sub(CLIENT_HEADROOM_SECS);
  let max_budget_from_deadline = CLIENT_DEADLINE_SECS.saturating_sub(CLIENT_HEADROOM_SECS);
  let effective_budget = max_budget_from_fence.min(max_budget_from_deadline);
  ```
  The 10s headroom is sized for the *deadline* ceiling — "leave 10s for the response/log to return inside the 60s ntex deadline". When **fence < 60** and the fence-budget wins MIN, the 10s subtraction instead **shrinks the fence envelope**. For cluster-C-8 (fence=30): `20s budget < 30s fence wall-time` — the wake gives up 10s before the fence can possibly clear in the worst case.
- **Concurrency surface**: the design intent of the retry was to envelope the source teardown so a wake racing a fence-clear catches the slot release. The current math guarantees the wake **always misses** a worst-case fence-clear under cluster-C-8.
- **Empirical mitigation**: smoke-r9 was c=1, where fence-clear typically happens in <5s. The hole is dormant until c≥10-ish workloads stress the fence past p50 ≈ 15s.
- **Cluster-smoke math at c=20** (per the doc text at `config.rs:394-400`):
  - Fence p95 ≈ 29s, max=31.1s at c=30 baseline (the c=20 number is unmeasured but bounded by these — likely p95 ≈ 25s).
  - Wake retry budget = 20s.
  - **Race-loss window**: any teardown whose fence-clear lands in [20s, 30s] → wake 503s, slot frees within seconds of giving up. Empirically a 15-30% loss rate at c=20 is plausible (untested).
- **Action**:
  - **Short-term**: rework the formula to use a fence-aware headroom. Two options:
    1. **Asymmetric headroom**: keep CLIENT_HEADROOM_SECS = 10 on the deadline ceiling; use a smaller (or zero) fence-headroom — e.g. `FENCE_HEADROOM_SECS = 2` (just enough to absorb the timer-wheel jitter). This widens the envelope to `(fence + 2)` rather than `(fence - 10)`.
    2. **Envelope teardown tail**: include the Nomad purge tail in the budget — `from_host_fence_timeout(f) = f + NOMAD_PURGE_BUDGET_SECS` (where Nomad purge ≈ 5-10s), capped at `CLIENT_DEADLINE - HEADROOM`. For fence=30 this would be `30 + 5 = 35s capped at 50 = 35s`. For fence=120 it would still cap at 50.
  - **Medium-term**: surface the wake-retry budget in the controller boot log so operators see "wake retry budget = 20s (derived from fence=30)" — closes the observability hole identified in R15-I1.
  - **Long-term**: C-7-LT (`202 Accepted` + polling) decouples wake from the client deadline; the retry budget can grow to `fence + nomad_purge_tail + grace`. The 60s ntex deadline ceases to be the hard cap.
- **Why important, not critical**:
  - Wake-503 with `vm_index_unavailable` is observable (clean WARN log + clean HTTP code). The smoke-r9 verdict says "**convert silent cancellation into observable 503**" was the C-7 design goal — this finding doesn't violate that.
  - Idle-eviction recovery: the row is CAS'd back to `snapshotted` by the rollback closure, so the operator/client can retry. Retry latency budget is the operator's problem, not the controller's.
  - Sub-criticality matches the smoke-r9 reviewer's own call: "(c) accept C-8 as documented 'wake races slow teardown' failure mode and proceed with knowingly degraded SLO".
- **Related to R15-V1**: this is a budget-arithmetic finding, separate from R14-V1's narrow-cancel-window in `reserve_vm_index_with_retry`'s success path. Both involve the same retry but at different correctness layers.

### [R15-D1] `wait_for_agent_silent` (fence loop) has no per-poll observability (LOW-PRIORITY, concurrency-r15)

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3205-3273`.
- **Shape**: 100ms-cadence loop polling `/livez`, classifying each response as "miss" (transport, timeout, 5xx) or "answer" (non-5xx HTTP). Two consecutive misses → Ok. Deadline reached → Err with a summary string.
  ```rust
  loop {
      if Instant::now() >= deadline { break; }
      // ... poll, classify, update consecutive_misses ...
      compio::time::sleep(Duration::from_millis(100)).await;
  }
  ```
- **Observability**: only the terminal `host_fence: cleared` (callsite `nomad_ch.rs:1106-1111`) or `host_fence: timeout` (`:1120-1126`) log lines fire. Inside the loop, NO per-poll log.
- **Why this matters now**:
  - With C-8's 30s fence at c=20, fence timeouts will happen. Each timeout produces a vm_index leak. The leak-summary message at `:3267-3272` carries `probes=N, last_http_status=Option<u16>, consecutive_misses=N` — useful but only on Err.
  - On Ok (the fence cleared in 23s) there is NO log of WHEN consecutive_misses crossed 2. Diagnosing fence-tail distribution requires this signal — and it's missing.
  - The C-6 / C-7 diagnostic experience (per R14-D1) showed that loop-body INFO lines are essential for localizing loop-internal wedges. The fence loop is exactly this shape.
- **Severity**: LOW — the fence loop has a 100ms cadence and a known-bounded budget; it doesn't fundamentally hide a wedge. The signal is "fence-tail distribution under c=20", not a correctness issue.
- **Action**:
  - **Optional**: emit a single INFO log at fence-pass time recording `probes_to_clear`, `consecutive_miss_streak_start_at_probe_N`. One log per teardown. Volume bounded.
  - **Alternative**: a tracing `span` around `wait_for_agent_silent` with `probes` + `elapsed` as span fields, recorded at span close.

## Carry-forward audit

| Finding | Open Since | Cycles | r15 status |
|---|---|---|---|
| **R4-A2 / R5-A2** LeasedVmSlot RAII | r4 | **12+** | OPEN. C-7 + C-8a didn't address. Would dissolve R10-C1/C2, R11-C1/C2/I1, R12-M1, 6 C3 widenings, R14-V1's narrow window. 12+ findings collapse under correct RAII shape. |
| **R7-C1** detached teardown task | r7 | 8 | PARTIALLY CLOSED at `91ce9be5` (admin path). R14-C1 (sweep.rs:563) + R14-I2 (registry.rs:829) remain open. |
| **R14-C1** sweep.rs:563 sibling-C-6 (idle-eviction inline teardown) | r14 | 2 | OPEN. Not touched by C-7/C-8a. Latent until idle-eviction sweep meets c≥10 wake traffic on same worker. Critical at T-8b-stress. |
| **R14-I2** registry.rs:829 sibling-C-6 (idle-GC inline stop) | r14 | 2 | OPEN. Not touched by C-7/C-8a. Lower trigger rate than R14-C1. |
| **R14-I1** C-4 retry budget mismatch | r14 | 2 | PARTIALLY CLOSED by C-7 (`493d6c1e` lowered default to 25×2=48s) + R14-A6 (`c3edf968` derives from cfg) + C-8a (`2afbb2dd` caps at deadline). The proximate "120s > 60s deadline" mismatch is closed. R15-I2 surfaces a residual "budget < fence wall-time under fence-budget-wins" mismatch — same family, different layer. Mark CLOSED with R15-I2 as the successor. |
| **R14-V1** do_restore_inner await count | r14 | 2 | OPEN. Outer-count = 8 unchanged. Inner-count reduced from 60→25 (default) or →11 (production with fence=30). |
| **R11-C1** `unregister_restored` silent-fail-OPEN | r11 | 4 | OPEN. |
| **R11-C2** rollback closure 2-await window | r11 | 4 | OPEN. Still compounded by R13-I1. |
| **R10-Q3** registry.rs bare RwLock unwraps | r10 | 5 | OPEN. Out-of-scope, code-quality. |
| **R10-S2 / R12-M1** spawn_blocking JoinError swallow | r10/r12 | 5/3 | OPEN. C-7's `spawn_blocking` calls (none new in C-8a) still pattern-match: `unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))` — same swallow-as-string pattern. Not made worse, not made better. |
| **R11-P1 / R13-I1** pool churn | r11 | 4 | OPEN. T-8b-stress is the test gate. |
| **R10-M2** spawn_blocking panic-format `Any { .. }` | r10 | 5 | OPEN. |

## do_restore_inner await count

- HEAD count: **8** (unchanged from r14).
- Inner cancel boundaries inside `:711`:
  - Default policy fallback (test stubs without cfg): **25** sleep awaits (was 60 pre-C-7, was 60 pre-C-8a — both retained the 60 default).
  - Production (RealRestoreBackend on cluster-C-8): **11** sleep awaits (`from_host_fence_timeout(30) = 11 attempts, 10 sleeps`).
- Delta vs r14: outer = 0 (still 8); inner = ≤60 → ≤25 (default) or ≤11 (production). **Net cancel surface reduced**; correctness window for R14-V1 narrow-drop-window also reduced proportionally (fewer attempts ⇒ fewer Ok-followed-by-cancel-before-return chances).

## Pattern observation

The C-7 + C-8 + C-8a arc closed the "silent-cancel" failure mode by **reducing the retry budget to fit inside the client deadline**. Under cluster-C-8, the cluster's host_fence is reduced from 120s → 30s so the source-teardown wall-time fits inside the budget. **This works for the smoke workload (c=1) but fundamentally re-introduces the FM-F race surface** that the 120s default was sized against. The smoke-r9 review and the deferred file both call this out as an acceptable trade-off for cluster-smoke; T-8b-stress (c=20) is where the trade-off lands.

The C-7-LT (`202 Accepted` + polling) is the **right** long-term shape: it decouples wake from the client deadline, allowing the retry budget to envelope a conservative fence (120s+) **without** silent-cancellation. R15-I2 is the strongest argument yet for prioritizing C-7-LT — without it, every per-cluster tightening of `host_fence_timeout_secs` to fit the budget reopens FM-F by another notch.

**Sibling-coverage observation (carry from r14)**: the `91ce9be5` commit message AND the C-7 diagnosis-update text in the deferred file BOTH continue to misclassify `sweep.rs:563` as "safe". This is the second cycle the misclassification has propagated. R14-C1's filing as critical is justified — the audit-paper-trail debt around this misclassification is a second-order concurrency hazard (someone reads the audit, concludes the pattern is safe, then writes new code with the same shape).

## Status block (one-liner)

```
Round 15:
  NEW: R15-I1 (config drift — Rust default 120s vs cluster systemd
               override 30s vs unit-test docstring "60 in many configs";
               three numbers, three doc surfaces, drift unwatched),
       R15-I2 (wake retry budget < fence wall-time under cluster-C-8:
               fence=30 → budget=20 → wake gives up ≥10s before fence
               can clear in the worst case; CLIENT_HEADROOM=10
               subtraction was sized for deadline ceiling, instead
               shrinks fence envelope when fence-budget wins MIN;
               every per-cluster fence-tightening re-opens FM-F race),
       R15-D1 (wait_for_agent_silent fence loop has no per-poll
               observability; 100ms cadence, only terminal Ok/Err
               logs; fence-tail distribution under c=20 invisible).

  RESOLVED: R14-I1 (closed by C-7 + R14-A6 + C-8a; R15-I2 inherits
                    the residual budget-arithmetic-under-fence-budget-
                    wins concern as a fresh finding).

  C-8 / C-8a AUDIT VERDICT: STRUCTURALLY CORRECT. MIN-of-two-ceilings
                            arithmetic handles all edge cases including
                            fence=0 (1 attempt) and fence=u64::MAX
                            (caps cleanly). Closure narrative is
                            accurate. The residual R15-I2 is a DESIGN
                            trade-off the deferred file already
                            acknowledges; not a regression.

  CARRIED: R4-A2 LeasedVmSlot (12+ cycles, incident-class — 12+
                                findings dissolve under proper RAII),
           R11-C1 (4 cycles), R11-C2 (4 cycles, compounded by R13-I1),
           R10-Q3 (5 cycles, code-quality),
           R10-S2 / R12-M1 (5 / 3 cycles, swallow-as-string),
           R7-C1 (8 cycles, PARTIALLY CLOSED at 91ce9be5; sweep +
                  registry siblings remain),
           R14-C1 + R14-I2 (sibling-C-6 sites at sweep.rs:563 +
                  registry.rs:829, untouched by C-7/C-8a — admin-
                  reachable runtime-starvation latent at T-8b-stress),
           R14-V1 (do_restore_inner await count 8 unchanged; narrow
                   cancel-window between reserve_vm_index Ok and
                   function-return persists),
           R10-M2 (5 cycles),
           R11-P1 / R13-I1 (4 cycles, T-8b-stress gate).

  ASK FROM r15: prioritize C-7-LT (202 Accepted + polling) over more
                fence-tightening — every per-cluster fence reduction
                trades FM-F race surface for retry-budget fit, and
                R15-I2 shows the formula's 10s headroom is misaligned
                when fence-budget wins MIN.
```
