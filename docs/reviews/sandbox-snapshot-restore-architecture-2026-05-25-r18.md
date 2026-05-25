# Sandbox snapshot-restore architecture review — 2026-05-25 r18

**Reviewer**: architecture-r18 (post C-7-LT sprint COMPLETE review)
**HEAD**: `ee57e23f` (`feat/sandbox-snapshot-restore`)
**Scope**: C-7-LT-PR1 + PR2 + R17-A5 + PR2-FOLLOWUP (19 commits, +49 lib tests).
**Lens**: architecture (read-only).

## Summary

C-7-LT-PR2 lands the full async-wake contract: `wake_machine.rs` (~1100 LOC),
dual-mode `wake_sandbox`, `poll_wake`, and the wake_jobs GC sweep on its own
`detach_isolated` thread. R17 CRITICALs closed cleanly:
`update_wake_job_state` now bumps `lessee_updated_at`
(`db.rs:3055-3056`) with symmetric COALESCE; `CreateGuard::drop` migrated;
0010 lands the takeover-supporting partial index, revokes the audit SELECT,
and CHECKs `agent_url`. The `detach_isolated` helper is now the canonical
pattern (5 production sites).

The CRITICAL for r18 is the inverse of R17-A1: the lessee mechanism is
**wired but not consumed**. The proposal (§5–6) calls for a `wake_jobs`
takeover sweep that marks abandoned rows `failed/wake_worker_aborted` —
only the terminal-row GC landed. Crash mid-wake → row stuck non-terminal
forever; clients poll an answer that contradicts the (recovered) sandbox
row. Three architectural concerns beyond that: (a) WakeMachine has no
shutdown linkage; (b) `wake_jobs` + `sandboxes` are two write tracks with
no atomic relationship; (c) R17-A4's admin kill-switch + the proposed
`metrics.wake_response_mode` gauge are both still missing.

5 findings (1 CRITICAL, 3 IMPORTANT, 1 MINOR) + 1 pattern note.

## CRITICAL

### [r18-A1] No wake_jobs takeover sweep — crash mid-wake strands the row forever; fresh POSTs replay the orphan

- **Location**: missing. `sweep.rs:285-355` is the GC sweep (terminal rows
  only); no analogue to `run_transient_takeover_once` (`sweep.rs:151-241`)
  for `wake_jobs`. `db.rs:3115-3117` documents the gap:
  *"Non-terminal rows are NEVER deleted by this sweep — those are handled
  by PR2's takeover scan (lessee_updated_at-based, like
  sandboxes.lessee_updated_at)."* That scan does not exist.

- **What I see**: `wake_machine.rs:108-180` writes terminal pg state from
  the detach-isolated thread only. Process crash / SIGKILL / OOM /
  unwinding panic before the terminal write → row stuck in any of
  `reserving_slot` … `registering` with `lessee_updated_at` frozen. The
  GC only deletes `state IN ('ok','failed')` (`db.rs:3132-3133`). The
  sandbox row recovers via `run_transient_takeover_once` →
  `Snapshotted` (`sweep.rs:135`), but the wake_jobs row contradicts that
  recovery.

  The wedge is permanent for the sandbox: the next `POST /wake` enters
  `find_pending_wake_for_sandbox` (`admin_handlers.rs:1556`), finds the
  orphan, and **returns replay=true** (`:1558-1568`) — so even fresh
  POSTs cannot break out, despite the sandbox being legitimately wake-
  able. The proposal (§5: "re-leases rows whose lessee expired"; §6:
  "orphan sweep marks the row failed with
  `error_code=wake_worker_aborted`") and the schema (0009 comment, 0010
  index) are wired for this — only the sweep code is missing.

- **Why it matters**: R17-A1 closed "the bump is missing"; r18-A1 is
  "the sweep that consumes the bump is missing." Same shape, opposite
  side. The `wake_jobs_lessee_idx` (0010) is currently dead weight
  supporting a nonexistent query.

- **Fix sketch**:
  1. `Database::claim_orphan_wake_for_recovery(wake_id, current_lessee,
     threshold_secs)` mirroring `claim_orphan_transient_for_recovery` —
     CAS-fence on `(lessee, lessee_updated_at < now() - threshold)`, on
     success flip `state='failed', error_code='wake_worker_aborted',
     lessee=self.host_id(), lessee_updated_at=now()`.
  2. `transient_state_lease_expired_wake_jobs(threshold_secs)` query
     (the `wake_jobs_lessee_idx` already supports it) +
     `run_wake_takeover_once` + `spawn_wake_takeover` in `sweep.rs`.
  3. `WakeErrorCode::WakeWorkerAborted` + 0011 migration to extend
     the `wake_jobs_error_code_check` CHECK.
  4. `lib.rs::AppState::from_config` wires the spawn alongside the GC.

## IMPORTANT

### [r18-A2] WakeMachine has no shutdown linkage — clean controller drain leaks wakes the same way a crash does

- **Location**: `wake_machine.rs:67-81` (struct has no `Arc<AppState>`
  or shutdown flag). Compare every other background loop —
  `sweep.rs:343-353`, `sweep.rs:266-282` — all observe
  `state.shutdown_requested()`.

- **What I see**: `WakeMachine::drive` (admin_handlers.rs:1675) runs
  fire-and-forget on a detach thread. Once `trigger_shutdown` flips
  the flag (`lib.rs:200`), GC / takeover / heartbeat loops break out
  next iteration; the wake machine continues through its worst-case
  ~150s phase ladder unobserved. If the process actually winds down,
  the thread is killed mid-phase (r18-A1 wedge shape) or blocks
  shutdown progress.

- **Why it matters**: rolling deploys are routine. Today each deploy
  leaks "stuck forever" wake_jobs rows on every controller that was
  mid-wake. Once r18-A1 sweep lands, this becomes a correctness
  non-issue but still a noisy-failures issue.

- **Fix sketch**:
  1. Pass `Arc<AppState>` (or just the shutdown Arc) into WakeMachine.
     Check at every phase boundary; on shutdown observed, write
     `state='failed', error_code='wake_worker_aborted'` and return.
  2. Track in-flight wake count in AppState so `trigger_shutdown`
     can wait-with-deadline for them to terminate (config knob,
     default ~10s).

### [r18-A3] No `metrics.wake_response_mode` gauge — operators have no live signal of which contract is active

- **Location**: `metrics.rs:212-218` adds the
  `inc_wake_sync_deprecated()` counter (Phase 5 gate). The gauge R17-A4
  recommended ("`metrics.wake_response_mode` so operators can read the
  live value") never landed. Combined with the still-open R17-A4
  (boot-time-only flag), rollback requires a fleet restart.

- **What I see**: counter tells you the sync path was *used* — not
  which mode any given controller is currently in. In a phased rollout
  with mixed-mode fleets, the counter is ambiguous.

- **Why it matters**: PR2's whole purpose is to carry a feature flag
  through Phase 4 for fast rollback. The flag is rollback-shaped but
  not rollback-capable: cost of a fleet restart for an Async regression
  caught in canary is minutes-of-degradation × N nodes. The
  `Arc<ArcSwap<WakeResponseMode>>` swap is ~10 LOC.

- **Fix sketch**:
  1. `Arc<ArcSwap<WakeResponseMode>>` on AppState.
  2. `POST /admin/wake-response-mode {mode}` (admin-bearer-gated).
  3. `metrics.set_wake_response_mode(mode)` gauge updated on boot + flip.

### [r18-A4] Dual write track — `wake_jobs` row and `sandboxes` row have no transactional relationship; ordering races leak observable states

- **Location**: `wake_machine.rs:244-259` (wake_jobs → sandboxes order
  on enter); `wake_machine.rs:447-479` + the terminal-ok write in
  `drive` (`:128`) on the success path.

- **What I see**: two concrete races.
  - Success race: sandboxes flips `Running` at `:448`. If
    `clear_snapshot_metadata` succeeds and the process dies before
    `drive`'s terminal `update_wake_job_state(Ok, …)` at `:128`, the
    sandbox is happily Running but the wake_jobs row says
    `registering` forever (until r18-A1 sweep, which doesn't exist).
    Client polling sees `registering` permanently — but a fresh GET
    on the sandbox shows `Running`. Proposal §4's "treat 404 as wake
    lost — read sandbox directly" doesn't apply because the client
    never sees 404, it sees stuck-intermediate.
  - Rollback race: `rollback_and_classify` flips sandboxes back to
    `Snapshotted` at `:524`. If the process dies before `drive`'s
    terminal `Failed` write at `:161`, the next POST replays an
    orphan row whose underlying wake already concluded (r18-A1
    interaction).

- **Why it matters**: in Sync mode the wake_jobs row never existed and
  the sandboxes row was the only truth. In Async, the wake_jobs row
  IS the client-facing truth, and divergence from the sandboxes row
  is now observable. The architectural intent ("sandboxes row is the
  lease anchor") needs an explicit "wake_jobs is a derived view"
  invariant that the code doesn't yet enforce.

- **Fix sketch**:
  1. Reorder the terminal sequence in `wake_machine.rs:447-479` so
     `update_wake_job_state(Ok, …)` is the LAST write, not in the
     post-script. Symmetrically, terminal `Failed` writes BEFORE the
     rollback CAS.
  2. Document the contract: "wake_jobs terminal ⇒ sandboxes reflects
     it; wake_jobs intermediate + stale lessee ⇒ wake presumed
     aborted; the sandbox row may have been swept."

## MINOR

### [r18-A5] WakeMachine duplicates ~150 LOC of `do_restore_inner`; divergence is already starting

- **Location**: `wake_machine.rs:186-480` vs.
  `restore_handler::do_restore_inner`. Module docs (`:29-43`) explicitly
  justify the duplication as a Phase 4 safety choice.

- **What I see**: three divergences already exist —
  (1) wake_machine has its own `read_snapshot_row` (`:639-674`)
  bypassing `pub(super) SnapshotRowMeta`;
  (2) the R16-S2 sanitiser only exists on the wake path;
  (3) phase-boundary `spawn_blocking` sites are duplicated.

- **Why it matters**: not a defect today. When Phase 5 deletes the
  sync path, any sync-path-only fix landed between PR2 and Phase 5
  will need a wake_machine companion patch. Cross-reference comments
  (`// MIRROR: do_restore_inner :NNN`) would make the dedup
  auditable.

## Cross-lens consensus

- **Concurrency lens**: r18-A1 + r18-A2 are concurrency-shaped (the
  lease is wired but unread). The reviewer should confirm every
  `wake_jobs` field maps to a code path that reads it — today
  `lessee` is write-only.
- **Perf lens**: `wake_jobs_lessee_idx` (0010) supports a sweep query
  that doesn't exist. Not wrong, just currently dead.
- **API surface lens**: r18-A3 + R17-A4 are operability gaps;
  r18-A4 is a wire-contract documentation gap (no documented
  client behavior for "intermediate forever").

## Lens hand-off

1. Concurrency lens: verify r18-A1's sweep design uses
   `claim_orphan_wake_for_recovery` (CAS on crashed lessee), NOT
   `update_wake_job_state` shape.
2. Operability lens: r18-A3 + R17-A4 are one sprint of plumbing.
3. API-surface lens: define the wire shape for
   `error_code='wake_worker_aborted'`.
4. Test-coverage lens: pg-gated test that drives a wake to
   `registering`, kills the thread, asserts the sweep marks it
   failed within threshold_secs.

## Pattern note: bolt-on sustainability

PR2-FOLLOWUP closed 7 S/A findings cleanly with +49 tests. But the
gates that DIDN'T land (r18-A1 wake_takeover, r18-A2 shutdown, r18-A3
gauge) are *contract-level* — they were named in the proposal
(`docs/proposals/c7-lt-async-wake.md` §5–6) and never made it into a
PR. Tactical findings → reviewer-flag-then-fix works. Contract
findings → must land at proposal review. The wake_takeover sweep is
the canonical example: it's specified, the schema is wired for it,
the comment in `db.rs` references it — and it's absent.

## Next-sprint architectural priorities

1. **r18-A1** wake_jobs takeover sweep (highest-impact gap).
2. **r18-A2** WakeMachine shutdown observance + drain wait.
3. **r18-A3 + R17-A4** runtime kill-switch + gauge — required
   before Phase 5 cutover.
4. **r18-A4** reorder terminal writes; document dual-track contract.
5. **Phase 5 prep** — collect r18-A5 divergences into a pre-cleanup
   ticket so sync-path deletion doesn't take wake-path with it.
