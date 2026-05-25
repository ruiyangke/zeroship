# Sandbox/snapshot-restore — concurrency r22 review

Date: 2026-05-25 (UTC).
HEAD at audit: `8b366b6d` (branch `feat/sandbox-snapshot-restore`).
Round 22 of N. READ-ONLY.

Scope since r21:

- **R20-C1 SQL guard LANDED** at `ccb2abc8` + `afa5da96`. One-line
  `AND state NOT IN ('ok','failed')` on `update_wake_job_state`.
- **r21-A1** at `fcac5355` — `user_id` in restore-path Config.
- **R10-API4** at `00a00d01` — readyz envelope (concurrency-neutral).
- **Smoke-r20** (HEAD `ef11edb3`, pre R20-C1, WAKE wall 110.21 s) and
  **smoke-r21** (HEAD `6e3d8c52`, post R20-C1, WAKE wall 38.05 s).

Prior: `…concurrency-2026-05-25-r21.md`.

## Summary

- **5 findings** (0 new CRITICAL, 2 new IMPORTANT, 3 MINOR).
  **R20-C1 data plane CLOSED**; **R22-I1 caller-side gap** is the
  dominant new finding — correctness preserved, observability
  silently degraded.
- **r21-A1 write-once audit**: `user_id` is read at
  `restore_handler.rs:639` from `sandbox.sandboxes`. Schema CHECKs
  format; **no UPDATE in tree ever rewrites it** (grep
  `SET.*user_id` zero hits). The wake_jobs table has no `user_id`
  column. **r21-A1 is concurrency-neutral.**
- **r21-I1 watchdog still unimplemented.** No commit since r21
  touches `wake_machine.rs:273-365`. Smoke-r20 was the strongest
  near-miss yet (Restoring ~100 s vs threshold 60 s); smoke-r21
  collapsed it to 22 s by happenstance, not by mechanism.
- **R20-C1 has not been production-exercised** despite pg-gated
  tests. Tests cover `Ok→Restoring` / `Failed→Restoring` (stale to
  non-terminal), not the terminal→terminal race the rustdoc
  motivates. Cross-lens consensus with code-quality r22 R22-I1.

## Findings

### [R22-I1] R20-C1 guard correct on data plane; 3 callers discard `rows_affected` → terminal-overwrite silently no-ops with no operator breadcrumb

- **Files**: `wake_machine.rs:128-144` (`Phase::Ok` terminal write),
  `:161-177` (`Phase::Failed` terminal write), `:487-500` (`set_state`
  intermediate). Contract at `db.rs:3207-3247`.
- **Trace** (all 3 sites):

  | Site | Pattern | rows_affected handling |
  |---|---|---|
  | `Phase::Ok` :128 | `if let Err(e) = … { log }` | `Ok(0)`/`Ok(1)` indistinguishable |
  | `Phase::Failed` :161 | `if let Err(e) = … { log }` | same |
  | `set_state` :487 | `if let Err(e) = … { log }` | doesn't even bind `n` |

- **Race shape** (data plane safe, observability silent):

  ```
  t=0      wake-machine in Restoring (lessee bumped at entry)
  t=T_th   takeover sweep claims row → state=failed,
           error_code=wake_worker_aborted (atomic UPDATE in
           claim_orphan_wake_for_recovery)
  t=T_th+ε wake-machine reaches Phase::Ok, calls
           update_wake_job_state(Ok) → R20-C1 predicate sees
           state='failed', UPDATE returns Ok(0)
           → wake-machine logs "terminal ok" (line 126)
           → row stays state='failed' with takeover's error_message
           → poll client sees failed; controller logs say ok
  ```

- **Data plane**: SAFE. R20-C1's authoritative-failure semantics
  hold; the sweep's row survives. **Observability**: degraded.
  Three log lines per round lie about what hit the row. The R20-C1
  rustdoc (`db.rs:3202-3206`) claims callers do not need to handle
  the no-op "specially" — true on data plane, **misleading on
  observability**.
- **Side-effect-race posture**: audited — `drive()` returns
  immediately after the terminal write; no out-of-pg side-effects
  fire on terminal-ok independent of row state. **R22-I1 is
  observability-only today**, but any future commit that adds a
  side-effect (gateway route push, metric emit, event) after the
  terminal write converts it into a live data-plane race.
- **Fix**: branch on `Ok(0)` at the 3 sites, emit
  `tracing::warn!(target: "sandbox::wake::terminal_overwrite_blocked",
  wake_id, attempted_state, …)`. ~5 lines each. Optional counter
  `sandbox_wake_terminal_overwrite_blocked_total{attempted=…}`.
- **Cross-lens consensus with code-quality r22 R22-I1** (same label,
  same severity — both lenses converged independently).

### [R22-I2] R20-C1 not production-exercised; pg-gated tests miss the terminal→terminal direction

- **Files**: `tests/sandbox_pg_e2e.rs:4934-5066` (R20-C1 tests);
  `db.rs:3231-3232` (predicate).
- **Shape**: existing tests cover `Ok→Restoring` and `Failed→Restoring`
  (stale-to-non-terminal). The race the R20-C1 rustdoc cites — sweep
  writes `Failed`, wake-machine writes `Ok` — is **terminal→terminal**
  and is NOT pinned in test. Smoke-r21 post-dates R20-C1 but Restoring
  (22 s) is well under threshold (60 s), so the race window never
  armed. Smoke-r20 (Restoring ~100 s) would have armed it, but pre-
  dates R20-C1 and the review doesn't capture pg state of `wake_jobs`.
- **Risk**: a regression flipping the predicate to `AND state != 'ok'`
  (mis-typing the IN-list, dropping `'failed'`) slips through CI and
  current smoke cycles. Effectiveness on the canonical race the
  rustdoc motivates is test-coverage-blind.
- **Fix**: add two scenarios — `update_wake_job_state_after_failed_to_ok_is_noop`
  and `update_wake_job_state_after_ok_to_failed_is_noop` (each ~15 LOC).
  Cross-lens with code-quality r22 R22-M2. Owner: test-coverage r22.

## MINOR

### [R22-M1] user_id write-once invariant — currently safe, no schema trigger pin

- **Files**: `restore_handler.rs:639-664` (read);
  `migrations/0001_initial.sql:67-68` (CHECK format constraint).
- **Shape**: r21-A1 closure rests on `user_id` being write-once on
  `sandbox.sandboxes`. Schema CHECKs format only; the write-once
  property is code-discipline (no `SET user_id =` in tree). A
  future migration adding an UPDATE path (account merge, user
  rename) would silently expose TOCTOU between `read_snapshot_row`
  and `submit_restore_job`. **No current exposure.**
- **Fix**: `BEFORE UPDATE` trigger raising on
  `OLD.user_id IS DISTINCT FROM NEW.user_id`, or an ADR pinning
  the invariant. Symmetric to r21-M2 (CHECK invariant ADR).

### [R22-M2] r21-I1 watchdog still unimplemented; smoke-r20 was the strongest near-miss yet

- **Files**: `wake_machine.rs:273-365` (Restoring without
  intermediate `set_state`); `sweep.rs:367` (cadence 60 s);
  `config.rs:967, 972` (threshold default 60 s, floor 30 s).
- **Smoke-r20** (HEAD `ef11edb3`, pre R20-C1): WAKE wall 110.21 s;
  Restoring persisted **~100 s** vs threshold 60 s. Review records
  `restoring → failed` from the wake-machine's own rollback path
  but does NOT capture whether `claim_orphan_wake_for_recovery`
  also fired. Two possibilities:
  - **(a)** Takeover sweep DID fire but pg state was not surfaced
    in the review. Pre-R20-C1, the wake-machine's terminal-failed
    write would have unconditionally clobbered the takeover's
    `wake_worker_aborted` breadcrumb (the exact data-loss bug
    R20-C1 fixes).
  - **(b)** Sweep tick did not align. With cadence == threshold
    and uniform offset, the effective claim window is 60–120 s of
    frozen lessee; Restoring at 100 s slips through ~33 % of
    offsets. **Unstable equilibrium** (R21-M3 carry).
- **Smoke-r21** (post R20-C1): Restoring 22 s, gap did not fire by
  happenstance.
- **Fix**: r21-I1 watchdog (20–30 s tick inside Restoring calling
  `update_wake_job_state(Restoring, None, None, None)` to bump
  `lessee_updated_at`). Cheap, single indexed UPDATE/tick. Pairs
  with R21-M3 (cadence vs threshold asymmetry).

### [R22-M3] R20-I2 sweep host-scoping still OPEN — cross-controller claim semantics unspecified

- **File**: `db.rs:3358-3389` (`claim_orphan_wake_for_recovery`).
- **Shape**: UPDATE has no `AND lessee = $host` clause. Single-
  controller smoke (r20/r21) cannot observe. Multi-controller:
  controller A's slow-but-alive Restoring (e.g., cold-cache 4 GB
  restore) gets claimed by controller B's sweep. Pairs with r21-I1
  watchdog — if A periodically bumps lessee, B's threshold doesn't
  match.
- **Severity**: MINOR here; architecture r22 owns. Carry from r20.

## Cross-lens consensus

- **R20-C1 data plane CLOSED; caller-side observability gap is the
  new dominant concurrency finding.** R22-I1 matches code-quality
  r22 R22-I1 by label and shape; both lenses converged independently.
- **r21-A1 is concurrency-neutral.** `user_id` is write-once on
  `sandbox.sandboxes`; no mid-wake race shape exists. The invariant
  is code-discipline, not schema-trigger — fragile but currently safe.
- **r21-I1 watchdog remains dominant operator-UX risk.** Smoke-r20
  Restoring 100 s was the strongest near-miss yet; smoke-r21
  collapsed by happenstance. Once C-7-LT-12 + C-7-LT-11 close and
  WAKE reaches `livez_polling`, Restoring may stretch again on
  cold-cache restores.
- **R20-C1 has not been production-exercised** despite pg-gated
  tests. R22-I1 and R22-I2 will quietly survive until a real
  production race fires.
- **R19-I1 8th-cycle deferral.** Two-phase livez code exists,
  tests pass, no production data point.

## Lens hand-off

- **Architecture r22**: R20-I2 (cross-controller claim semantics);
  R22-M1 (user_id write-once trigger/ADR); R21-M3 (cadence vs
  threshold symmetry — pairs with r21-I1).
- **Test-coverage r22**: R22-I2 (terminal→terminal pg-gated tests);
  R19-I1 8th-cycle still unexercised in production.
- **Code-quality r22**: R22-I1 cross-lens-consensus agreed;
  warn-on-zero-rows-affected at 3 wake_machine sites.
- **Performance r22**: source-teardown cadence shortening (22 s in
  r21) noted; no concurrency objection.
- **Security r22**: R22-M1 cross-references security-r21 R21-S1;
  the user_id write-once assumption is the integrity invariant
  behind the per-user-home allow-list.

## Carry table

| Finding | Source | r22 state |
|---|---|---|
| R19-C1 wedge-half | r19 → CLOSED r20 | CLOSED carry |
| **R20-C1** terminal-overwrite | r20 NEW-CRITICAL | **CLOSED data plane** at `ccb2abc8` + `afa5da96`; caller-discard gap = R22-I1 |
| R19-I1 livez two-phase | r19 → CLOSED r20 | CLOSED carry — **PRODUCTION-UNEXERCISED 8th cycle** |
| R19-I4 insert retry | r19 → CLOSED r20 | CLOSED carry |
| R20-I1 STATIC_NAMES | r20 NEW-IMP | CLOSED at `ed30f5d0` |
| **R20-I2** sweep host-scoping | r20 NEW-IMP | **STILL OPEN** |
| **R20-I3 / R21-I1** Restoring watchdog | r20 → r21 | **STILL OPEN** — see R22-M2 |
| R20-M1..M5, R19-I2/I3, R21-M1..M3 | r19/r20/r21 minor | carry |

## Status block

```
Round 22 (R20-C1 LANDED + r21-A1 + smoke-r20/r21):
  CLOSED on data plane:
    R20-C1 (ccb2abc8 + afa5da96; observability gap = R22-I1).
  STILL OPEN:
    R20-I2 (sweep host-scoping — IMPORTANT, arch),
    R20-I3 / R21-I1 (Restoring watchdog — IMPORTANT,
      smoke-r20 100s Restoring strongest-near-miss).
  NEW (r22):
    R22-I1 (R20-C1 caller-discard observability gap),
    R22-I2 (R20-C1 production-unexercised + tests miss
      terminal→terminal),
    R22-M1 (user_id write-once schema-trigger pin),
    R22-M2 (r21-I1 watchdog smoke-r20 restated),
    R22-M3 (R20-I2 cross-controller claim carry).
  CARRY:
    R19-I2, R19-I3, R20-M1..M5, R21-M1..M3,
    R19-I1 (production-unexercised 8th cycle).

  ASK: (1) ship R22-I1 observability warn at 3 wake_machine sites
       (~15 LOC, no SQL change);
       (2) extend R20-C1 pg-gated tests with terminal→terminal
       (R22-I2, ~30 LOC);
       (3) GATE-I3 r21-I1 watchdog still gating;
       (4) arch-r22 decide R20-I2 cross-controller claim.
```
