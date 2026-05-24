# Sandbox/snapshot-restore — concurrency r23 review

Date: 2026-05-25 (UTC).
HEAD at audit: `0e0eeffa` (branch `feat/sandbox-snapshot-restore`).
Round 23 of N. READ-ONLY.

Scope since r22:

- **R22-I1 LANDED** at `f98611fb` — terminal-overwrite tracing+counter
  at the 3 `update_wake_job_state` call sites in `wake_machine.rs`.
- **C-7-LT-12a LANDED** at `7fd661c9` — sandbox-side rootfs_source
  emission in ChPlugin restore-path Config (controller-side half of
  the driver's hardlink-stage fix). Concurrency-neutral (1-line
  field add, same pattern as r21-A1 `user_id`).
- No commits since r22 touch `wake_machine.rs:273-365` (Restoring
  watchdog), `db.rs:3358-3389` (sweep), or
  `tests/sandbox_pg_e2e.rs:4934-5066` (R20-C1 tests).
- Smoke-r22 (HEAD `8b366b6d`, pre R22-I1): WAKE wall 51.02 s; state
  machine `pending → reserving_slot → restoring → failed` (3rd
  cycle in a row at this outer shape; R19-I1 9th-cycle deferred).

Prior: `…concurrency-2026-05-25-r22.md`.

## Summary

- **4 findings** (0 new CRITICAL, 1 new IMPORTANT, 3 MINOR).
  **R22-I1 CLOSED on all 3 sites** — all match `Ok(0)` + WARN + counter
  exactly as the r22 fix prescribed; observability gap eliminated.
- **R22-I2 STILL OPEN.** No commits since r22 touch
  `tests/sandbox_pg_e2e.rs`; the terminal→terminal direction the R20-C1
  rustdoc cites remains untested. R22-I1's WARN+counter is now the
  primary live signal for that race shape — but the counter itself has
  not been exercised end-to-end in any pg-gated test (only the lib
  monotonic micro-test). **Effectiveness of the R22-I1 closure is
  test-coverage-blind.**
- **R21-I1 watchdog STILL UNIMPLEMENTED.** Smoke-r22 Restoring was
  bounded by the wake's own failure path (~50 s, below threshold 60 s);
  no pressure on the watchdog this cycle. Still acceptable to defer
  until C-7-LT-12a unblocks `livez_polling` and Restoring can stretch
  on cold-cache restores.
- **R19-I1 production-unexercised, 9th cycle.** State machine still
  terminal at `restoring → failed`. Smoke-r23 (post-C-7-LT-12a + driver
  v11+, post rootfs-staging) is the first cycle where R19-I1 has a
  realistic chance of being exercised in production.
- **R19-C1 takeover sweep cannot be assessed from smoke-r22.** Worker
  uptime / sweep invocation count not surfaced in the cluster review;
  the new `sandbox_wake_terminal_overwrite_blocked_total` counter
  introduced by R22-I1 will be the canonical observability handle once
  smoke-r23 collects it.

## Findings

### [R23-I1] R22-I2 carry — R20-C1 terminal→terminal pg-gated test gap still open; R22-I1 counter is the only live signal but is itself untested end-to-end

- **Files**: `tests/sandbox_pg_e2e.rs:4934-5066` (R20-C1 tests, unchanged
  since r22); `wake_machine.rs:128-156, 173-201, 515-540` (R22-I1
  guard-fire sites — landed); `metrics.rs:281-303` (counter accessor —
  landed); `db.rs:3231-3232` (predicate, unchanged).
- **Shape**: R22-I1 surfaces guard-fires via WARN + counter. The lib
  test (`inc_wake_terminal_overwrite_blocked_monotonic`) confirms the
  accessor increments. **No pg-gated test exercises the full path** —
  set row terminal, call `update_wake_job_state(other_terminal)`,
  observe (a) `Ok(0)` return, (b) WARN at
  `target: sandbox::wake::terminal_overwrite_blocked`, (c) counter
  bump. The lib-test counter contract and the SQL predicate are
  verified independently but never together against pg.
- **Race shape** (re-stated from r22 R22-I1, now with observability):

  ```
  t=0      wake-machine in Restoring (lessee bumped at entry)
  t=T_th   takeover sweep claims row → state=failed
  t=T_th+ε wake-machine reaches Phase::Ok → R20-C1 guard fires
           → update_wake_job_state returns Ok(0)
           → wake_machine.rs:139 matches Ok(0) → WARN +
             sandbox_wake_terminal_overwrite_blocked_total++
           → row stays state='failed' (sweep's breadcrumb survives)
  ```

- **Why now a finding**: R22-I1 changes the observability story but
  not the production-exercise story. The counter starts at zero; if
  it remains at zero through smoke-r23 we have **two indistinguishable
  hypotheses**: (a) the race never fires under current production
  load (good), (b) the WARN/counter wiring is misplumbed (bad). A
  pg-gated test that asserts the counter bumps once on a known
  terminal→terminal transition disambiguates without smoke pressure.
- **Severity**: IMPORTANT (carry from R22-I2; promoted because R22-I1's
  closure makes the missing test the only remaining gap).
- **Fix**: add two scenarios — `update_wake_job_state_after_failed_to_ok_is_noop_and_bumps_counter`
  and `update_wake_job_state_after_ok_to_failed_is_noop_and_bumps_counter`
  (each ~20 LOC). Use `wake_terminal_overwrite_blocked_value()` pre/post
  to assert `++`. Cross-lens with code-quality r23 (likely converges).
- **Owner**: test-coverage r23.

## MINOR

### [R23-M1] R22-I1 closure verified at all 3 sites; rustdoc on `update_wake_job_state` still tells callers no-op needs no special handling

- **Files**: `wake_machine.rs:139, 184, 521` (3 Ok(0) match arms —
  landed correctly); `db.rs:3202-3206` (`update_wake_job_state` rustdoc
  — unchanged since R20-C1).
- **Shape**: closure audit:

  | Site | Line | Pattern | Counter | Target |
  |---|---|---|---|---|
  | `Phase::Ok` terminal | :139 | `Ok(rows) if rows == 0 =>` | yes | `sandbox::wake::terminal_overwrite_blocked` |
  | `Phase::Failed` terminal | :184 | `Ok(rows) if rows == 0 =>` | yes | same |
  | `set_state` intermediate | :521 | `Ok(rows) if rows == 0 =>` | yes | same |

  All 3 emit `tracing::warn!` with consistent target string and bump
  the same counter. **Closure verdict: CLEAN.** The R22-I1
  `attempted_state = ?WakeJobState` field uses Debug formatting (not
  the more grep-friendly `as_str()`); minor consistency nit, not a
  defect.
- **Latent issue**: the R20-C1 rustdoc at `db.rs:3202-3206` still says
  callers "do not need to handle the no-op specially." That sentence is
  now demonstrably wrong — all 3 callers now do handle it specially,
  for the observability reason R22-I1 fixes. Rustdoc drift, not a
  concurrency bug.
- **Severity**: MINOR. **R22-I1 CLOSED.**
- **Fix**: 1-line rustdoc update at `db.rs:3204` — "callers SHOULD
  match `Ok(0)` to surface guard-fires for operator visibility (see
  R22-I1)." Optional; the call sites are already conformant.

### [R23-M2] r21-I1 Restoring watchdog still unimplemented; smoke-r22 did not exercise the gap

- **Files**: `wake_machine.rs:273-365` (Restoring phase without
  intermediate `set_state`); `sweep.rs:367` (cadence 60 s);
  `config.rs:967, 972` (threshold default 60 s, floor 30 s).
- **Smoke-r22**: WAKE wall 51 s; state machine `restoring → failed`
  at ~50.35 s wall-time. **Restoring strictly bounded** by the wake's
  own failure path; never approached threshold 60 s. R22's "still
  acceptable to defer" verdict holds for r23 — the gap has had no
  production exposure for 3 cycles (r20 ~100 s near-miss; r21 22 s;
  r22 ~50 s self-bounded).
- **Forward risk**: smoke-r23 (post-C-7-LT-12a + driver v11+) is the
  first cycle expected to reach `livez_polling`. Cold-cache restores
  on first-touch can stretch Restoring past 60 s; that's exactly the
  case the watchdog defends. **r21-I1 should land before T-8b-stress**
  or any non-trivial steady-state stretch where the wake doesn't
  self-bound at sub-60 s.
- **Severity**: MINOR (still acceptable to defer; same reasoning as
  r22 R22-M2). Will likely promote to IMPORTANT after smoke-r23 if
  Restoring stretches past threshold.
- **Carry from r22 R22-M2**.

### [R23-M3] R19-I1 production-unexercised 9th cycle; smoke-r23 is the first realistic chance to exercise

- **Files**: `wake_machine.rs:373-465` (`drive_livez_polling`, two-phase
  livez probing, R19-I1 fix region); `worker_pool.rs:170-220`
  (livez_polling phase entry).
- **Shape**: 9 smoke cycles since R19-I1 landed (`ed30f5d0`+earlier)
  and the wake state machine has terminated at `restoring → failed`
  every time. R19-I1's two-phase livez fix has never run against a
  successful Restoring exit in production. **Concurrency objection:
  the longer the code path remains unexercised, the more likely an
  unrelated refactor regresses it silently.** Pg-gated tests pass
  but the production code path is dead.
- **Smoke-r23 forecast**: with C-7-LT-12a (controller-side rootfs_source
  emission, this round) + driver v11+ (cross-worktree, hardlink-stage
  the rootfs), the rootfs.img NotFound failure should resolve and
  Restoring should reach `livez_polling`. R19-I1 should finally run.
- **Severity**: MINOR (test-coverage owns the underlying carry; this
  lens just flags the freshness budget). 9th cycle on the carry table
  is the practical ceiling.
- **Carry from r19 R19-I1**.

### [R23-M4] R19-C1 takeover sweep — smoke-r22 review did not surface sweep invocation count or counter values

- **Files**: `sweep.rs:367` (60 s cadence); `db.rs:3358-3389`
  (`claim_orphan_wake_for_recovery`); `metrics.rs:138-145`
  (`SANDBOX_TAKEOVER_ORPHANS_RECOVERED` and related).
- **Shape**: r22 ASK item (5) asked whether R19-C1 sweep exposed
  anything during smoke-r22's ~7 min worker uptime (~7 sweep
  invocations expected). The smoke-r22 cluster review
  (`…T8b-smoke-r22.md`) does NOT enumerate sweep invocation count,
  `sandbox_takeover_orphans_recovered_total`, or
  `sandbox_wake_terminal_overwrite_blocked_total` values. **Without
  that data, R19-C1 health cannot be assessed from r22.**
- **Why MINOR not IMPORTANT**: the wake state machine self-terminated
  at `failed` in well under threshold (51 s wall vs 60 s threshold);
  the sweep had no orphans to claim and (correctly) should have done
  nothing. Absence of evidence aligns with the expected behaviour;
  it just isn't *confirmed*.
- **Severity**: MINOR. Cross-lens with test-coverage r23 (smoke-r23
  observability checklist should include both counters).
- **Fix**: the next cluster smoke review template should curl
  `/_zs/metrics` from controller + worker pre- and post-WAKE and dump
  the deltas for `sandbox_wake_terminal_overwrite_blocked_total`,
  `sandbox_takeover_orphans_recovered_total`,
  `sandbox_takeover_sweep_invocations_total` (if exists). Operational
  ask, not a code change.

## Cross-lens consensus

- **R22-I1 CLOSED — all 3 wake_machine sites conformant.** The fix
  landed exactly as r22 prescribed: `Ok(rows) if rows == 0` arm,
  `tracing::warn!(target: "sandbox::wake::terminal_overwrite_blocked", …)`,
  `crate::metrics::inc_wake_terminal_overwrite_blocked()` counter bump.
  Lib tests confirm the counter monotonic. Closure clean.
- **R22-I2 STILL OPEN as R23-I1.** No pg-gated terminal→terminal test
  has landed; the counter exists but its plumbing is untested
  end-to-end. Test-coverage r23 owns.
- **r21-I1 watchdog STILL DEFERRED.** Smoke-r22's self-bounded
  Restoring (~50 s vs threshold 60 s) didn't pressure the gap.
  Smoke-r23 (first cycle with C-7-LT-12a) may finally stretch
  Restoring on cold-cache; promote to IMPORTANT if it does.
- **R19-I1 9th-cycle deferral acceptable** because smoke-r23 is the
  first realistic exercise window (rootfs-stage finally complete).
- **R19-C1 takeover-sweep observability blind for smoke-r22**; smoke-r23
  cluster review template should include counter deltas as a standing
  field.

## Lens hand-off

- **Test-coverage r23**: R23-I1 (terminal→terminal pg-gated test with
  counter assertion — ~40 LOC); R19-I1 production-unexercised 9th
  cycle (smoke-r23 forecast: first realistic exercise);
  R23-M4 (sweep counter deltas in cluster smoke template).
- **Architecture r23**: R20-I2 cross-controller claim semantics
  (carry from r20; pairs with r21-I1); R22-M1 user_id write-once
  trigger/ADR (carry).
- **Code-quality r23**: R23-M1 rustdoc drift at `db.rs:3204`
  (1-line nit, optional); confirm R22-I1 closure (we agree CLOSED).
- **Performance r23**: no concurrency objection; C-7-LT-12a is a
  one-shot path emission, no hot-path impact.
- **Security r23**: R22-M1 cross-references security-r21 R21-S1
  (user_id write-once invariant behind per-user-home allow-list —
  unchanged this round; C-7-LT-12a's rootfs_source is a fixed
  controller-managed path, not a user-supplied value).

## Carry table

| Finding | Source | r23 state |
|---|---|---|
| R19-C1 wedge-half | r19 → CLOSED r20 | CLOSED carry; r22 sweep activity unobserved (R23-M4) |
| R20-C1 terminal-overwrite | r20 NEW-CRIT → CLOSED data plane r22 | CLOSED carry; observability now also CLOSED via R22-I1 |
| **R22-I1** observability-gap | r22 NEW-IMP | **CLOSED at `f98611fb`** (all 3 sites conformant) |
| R19-I1 livez two-phase | r19 → CLOSED r20 | CLOSED carry — **PRODUCTION-UNEXERCISED 9th cycle** (R23-M3) |
| R19-I4 insert retry | r19 → CLOSED r20 | CLOSED carry |
| R20-I1 STATIC_NAMES | r20 NEW-IMP → CLOSED `ed30f5d0` | CLOSED carry |
| **R20-I2** sweep host-scoping | r20 NEW-IMP | **STILL OPEN** |
| **R20-I3 / R21-I1** Restoring watchdog | r20 → r21 → r22 → r23 | **STILL OPEN — R23-M2** |
| **R22-I2** terminal→terminal pg test | r22 NEW-IMP | **STILL OPEN — promoted to R23-I1** |
| R22-M1 user_id write-once trigger | r22 minor | carry |
| R22-M3 cross-controller claim carry | r22 minor | carry |
| R19-I2, R19-I3, R20-M1..M5, R21-M1..M3 | older minor | carry |

## Status block

```
Round 23 (R22-I1 LANDED + C-7-LT-12a LANDED):
  CLOSED:
    R22-I1 (f98611fb; 3 sites conformant; counter wired).
  STILL OPEN:
    R20-I2 (sweep host-scoping — IMPORTANT, arch),
    R20-I3 / R21-I1 (Restoring watchdog — MINOR for r23, deferred;
      promote to IMPORTANT if smoke-r23 stretches Restoring past 60 s),
    R23-I1 (R22-I2 carry; promoted — terminal→terminal pg-gated test
      + counter assertion now the canonical effectiveness signal).
  NEW (r23):
    R23-I1 (R22-I2 promoted),
    R23-M1 (R22-I1 closure verified; rustdoc nit at db.rs:3204),
    R23-M2 (r21-I1 watchdog smoke-r22 self-bounded so no pressure),
    R23-M3 (R19-I1 9th-cycle; smoke-r23 first realistic exercise),
    R23-M4 (sweep counter deltas not surfaced in smoke-r22 review).
  CARRY:
    R19-I2, R19-I3, R20-M1..M5, R21-M1..M3, R22-M1, R22-M3,
    R19-I1 (production-unexercised 9th cycle).

  ASK: (1) ship R23-I1 pg-gated terminal→terminal test with counter
       assertion (~40 LOC; test-coverage r23);
       (2) smoke-r23 cluster review template must dump counter deltas
       for sandbox_wake_terminal_overwrite_blocked_total +
       sandbox_takeover_orphans_recovered_total (R23-M4);
       (3) GATE-I3 r21-I1 watchdog: re-evaluate after smoke-r23 —
       MINOR today, promote to IMPORTANT if Restoring stretches past 60 s;
       (4) arch-r23 decide R20-I2 cross-controller claim (5th cycle
       on the carry table).
```
