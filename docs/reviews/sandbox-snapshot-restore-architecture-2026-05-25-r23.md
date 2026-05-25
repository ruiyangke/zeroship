# Sandbox snapshot-restore architecture review — 2026-05-25 r23

**Reviewer**: architecture-r23 (post-smoke-r22 + driver v11 + C-7-LT-12a controller-half cycle)
**HEAD**: `0e0eeffa` (`feat/sandbox-snapshot-restore`, deferred-ledger close-out for C-7-LT-12 / C-7-LT-12a)
**Predecessor**: r22 at `ef11edb3`. Diff since r22: round-30 reviewer
artifacts (`26a42e54`), driver-pin v9→v10 (`1da1e4e9`), smoke-r21
(`6e3d8c52`), driver-pin v10→v11 (`8b366b6d`), R22-I1 wake-machine
counter (`f98611fb`), round-31 reviewer artifacts (`fc4621c9`),
smoke-r22 (`6f51efe6`), controller-half C-7-LT-12a
(`7fd661c9`), deferred close (`0e0eeffa`). Driver-worktree:
`7a85ed7b` + `50fb987d` + `538ca1de`.
**Lens**: architecture (READ-ONLY).

## Summary

C-7-LT-11 (stderr capture) is the highest-leverage observability
spend of the entire chain — a 5-LOC patch reclassified a layer from
"outside our codebase" to "back in our codebase" in a single cycle
and saved a multi-cycle CH-version-sweep. r22-A2's "next layer is
CH-internal" forecast was **REFUTED** by smoke-r22's verbatim CH
stderr (`DeviceManager(Disk(NotFound))` on `disks[0].path`).
C-7-LT-12a (driver-side `stageRootfsForRestore` + controller-side
`rootfs_source` emission) is the response: 15 LOC driver + 5 LOC
controller, lands in two coordinated commits across two worktrees.

The driver's restore-input model is now structurally equivalent to
the bash wrapper's in-place model: one directory (`runDir`) carries
config.json (rewritten), state.json + memory-ranges (symlinked from
RestoreFrom), AND rootfs.img (hardlinked/copied from
`rootfs-slim.img`). CH `--restore source_url=file://<runDir>` reads
all four from the same place. The wrapper accomplished the same end
state via cp + in-place rewrite of `$ZSBX_RESTORE_FROM`; the driver
accomplishes it via runDir aggregation. r22-A1's wrapper-retirement
blocker is now **STRUCTURALLY UNBLOCKED** pending smoke-r23 GREEN
confirmation.

r19-A1 leak ledger is at 9 consecutive fence_passed=true cycles + 9
consecutive leak_counter=0; demotion to MINOR-permanent is now
justified. Five findings (1 CRITICAL, 2 IMPORTANT, 2 MINOR).

## CRITICAL

### [r23-A1] Cross-emitter drift is now a 2-strike pattern (user_id + rootfs_source) — R22-T1 field-list parity contract test MUST land before C-7-LT-13

- **Where**:
  - `crates/sandbox/src/backend/nomad_ch.rs` — cold-boot
    `build_nomad_job_json_with` ChPlugin Config emitter.
  - `crates/sandbox/src/restore_handler.rs:2345-2420` — restore-path
    `build_restore_nomad_job_json` ChPlugin Config emitter
    (post-`7fd661c9`).
  - `docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r22.md:50-95`
    — R22-T1 specification.
- **Architectural pattern**: this is the **second** cross-emitter
  drift inside 4 cycles. C-7-LT-7 (`03d2f4a8`) added `user_id` to
  cold-boot; restore-path was missed for one commit-cycle until
  smoke-r18 caught the validator rejection (r21-A1 closed at
  `fcac5355`). C-7-LT-12a (`50fb987d`) added a new wire-input
  `RootfsSource` to the driver; both emitters had to ship in
  lockstep this time because smoke-r22's RED was specifically about
  the missing artifact, not a field-list parity bug. **Without
  R22-T1, the lockstep was procedural luck, not structural
  enforcement.**
- **Why this is CRITICAL now (not at r22)**: at r22 R22-T1 was
  process debt and r22-A3 framed it as latent risk. The C-7-LT-12a
  cross-worktree change just demonstrated that the risk is no longer
  latent — every new field added to either emitter is a one-commit
  window for a field-list drift bug. The probability of a third
  C-7-LT-13 / C-7-LT-14 needing a new Config field is high (R19-I1
  livez-polling verification still pending, post-LT-12a stability
  work may need new knobs). The contract test is the only
  structural fix; everything else is "remember to update both
  sides", which has failed twice already.
- **Fix shape (~30 LOC, no driver dependency)**: in
  `crates/sandbox/src/restore_handler.rs` add a `#[test] fn
  ch_plugin_emitter_field_list_parity()` that:
  1. Builds a representative `WakeContext` and calls
     `build_nomad_job_json_with(..., DriverKind::ChPlugin, ...)`,
     extracting the `Config` JSON object's keys into a HashSet.
  2. Builds a parallel `RestoreContext` and calls
     `build_restore_nomad_job_json(...)`, extracting Config keys.
  3. Asserts `restore_keys.difference(&cold_keys).collect() ==
     {"restore_from", "rootfs_source"}` (the restore-only fields),
     and `cold_keys.difference(&restore_keys).collect() == {}` (no
     cold-only fields).
- **Severity**: **CRITICAL** (escalated from r22-A3 IMPORTANT). The
  C-7-LT-12a cross-emitter additions move this from "latent
  regression risk on any future Config-field addition" to "the next
  Config-field addition is the third strike — third strike makes
  this a pattern, not an incident". Land before any C-7-LT-13 cycle
  ships a new field. Cycle: <30 min, no driver dependency, no
  cluster cycle needed for verification.

## IMPORTANT

### [r23-A2] Driver restore-path is now ARCHITECTURALLY equivalent to the bash wrapper (modulo symlink/hardlink mechanism) — wrapper retirement r20-A1 UNBLOCKED pending smoke-r23 GREEN

- **Where**: the divergence r22-A1 flagged at `restore_task.go:319,
  360, 420` is closed across three coordinated commits:
  - `1d5aa2f9` (driver) — C-7-LT-10: `restoreURL := "source_url=file://" + runDir`, symlinks state.json + memory-ranges from `RestoreFrom` into `runDir`.
  - `50fb987d` (driver) — C-7-LT-12a: `stageRootfsForRestore` hardlinks (or copies on EXDEV) `RootfsSource` → `<runDir>/rootfs.img`.
  - `7fd661c9` (controller) — emits `rootfs_source = cfg.runtime_dir / "rootfs-slim.img"` on the wake-path Nomad job.
- **Structural comparison post-C-7-LT-12a**:

  | Input file | Bash wrapper (`nomad-vm-wrapper.sh`) | Driver (`restore_task.go`) |
  |---|---|---|
  | `config.json` (rewritten) | tempfile + atomic rename in-place at `$ZSBX_RESTORE_FROM` | written to `<runDir>/config.json` |
  | `state.json` (immutable) | read from `$ZSBX_RESTORE_FROM` | symlinked from `RestoreFrom` into `<runDir>` |
  | `memory-ranges` (immutable) | read from `$ZSBX_RESTORE_FROM` | symlinked from `RestoreFrom` into `<runDir>` |
  | `rootfs.img` (alloc-scoped) | `cp --reflink=auto $ZSBX_ARTIFACT_DIR/rootfs-slim.img $DISK` (l.301) | hardlink-or-copy `RootfsSource` → `<runDir>/rootfs.img` |
  | `--restore source_url=` | `file://$ZSBX_RESTORE_FROM` | `file://<runDir>` |
  | "single directory CH reads" | yes (in-place at RestoreFrom) | yes (aggregated at runDir) |

  **Both converge on the one-input-directory contract.** Wrapper
  does it by rewriting source; driver does it by aggregating into a
  fresh dir. The driver's choice preserves the read-only-source
  invariant (the original concern at `restore_task.go:325-329`) and
  is strictly safer — every re-wake of the same snapshot sees a
  pristine `RestoreFrom`. r22-A1 CRITICAL is **STRUCTURALLY
  CLOSED**; carry as `architecturally-resolved-pending-empirical-
  confirmation` until smoke-r23 returns GREEN (or RED with a
  failure beyond DeviceManager).
- **Wrapper retirement (r20-A1) gating**: the gating condition stated
  at r22 ("driver matches wrapper's CH-input contract") is now met
  in source. The remaining gate is one GREEN smoke cycle with
  `SANDBOX_TASK_DRIVER=ch_plugin` + wrapper script absent. Earliest
  plausible: smoke-r24 (one cycle past first GREEN at smoke-r23).
- **Severity**: IMPORTANT — architectural milestone, no immediate
  code action; serves as the closure note for r22-A1.

### [r23-A3] Observability-before-architectural-decisions deserves an ADR alongside the vm-index retry-policy retrospective

- **Where**: `docs/decisions/2026-05-25-vm-index-retry-policy.md`
  already carries a "Smoke-r13 retrospective" section as
  non-normative empirical ground truth. The C-7-LT-11 meta-lesson
  has the same shape: a small observability investment changed the
  trajectory of a multi-cycle debug chain.
- **The meta-lesson, concretely**:
  - **r21 working hypothesis**: CH-internal VM-state corruption
    (state.json deserialisation, KVM/virtio reconstruction).
    **Forecasted action**: CH-version-sweep, CH-source-audit,
    potentially pin to a different CH version.
  - **C-7-LT-11 spend**: ~5 LOC in driver's resume-failure branch
    to lift CH's `stderr.log` tail into the Nomad task event.
  - **r22 observable**: `DeviceManager(Disk(NotFound))` —
    deterministic ENOENT on `disks[0].path`, fix is 20 LOC across
    two worktrees.
  - **Counterfactual cost avoided**: one CH-version-sweep cycle
    (provision + 5–8 hours) + an indeterminate number of cycles
    chasing a wrong layer.
- **What an ADR captures (that a smoke-r22 review does not)**: the
  generalised rule. The C-8c retro is normative for the vm-index
  retry policy; an ADR for "before patching at layer N+1, capture
  layer N's verbatim error first" would be normative for the wake/
  restore debug discipline going forward. r22-A2 already prescribed
  the tier-budgeted triage playbook informally; codifying it
  alongside C-8c gives it the same canon-status.
- **Recommended ADR**: `docs/decisions/2026-05-25-restore-debug-
  playbook.md` — three sections:
  1. Triage tier ladder (validator → wire → CH input contract → CH
     internal → agent boot); 1-cycle diagnostic budget per tier.
  2. Observability-before-architecture rule: each tier transition
     MUST include a verbatim-error capture mechanism before any
     fix at that tier is designed. Captures C-7-LT-11's lesson.
  3. The r22-A2 retro: 22 cycles of "in-codebase" failures
    REFUTE any architectural decision premised on "this must be
    outside our codebase" without a verbatim observable.
- **Severity**: IMPORTANT — process codification; one ADR file, no
  code change, prevents repetition of the r21 misdiagnosis class.

## MINOR

### [r23-A4] r19-A1 leak ledger — 9 consecutive cycles fence_passed=true, 9 consecutive zero leak counter — DEMOTE to MINOR-permanent

- **Where**: `cleanup_orphans_at_startup` (nomad_ch.rs:431-464),
  three lying-comment sites at `:1081/:1162/:1184` still unchanged.
  No `leak_reaper` / `leak_ledger` symbols in tree.
- **What I see**: r14, r15, r16, r17, r18, r19, r20, r21, r22 —
  **nine** consecutive cycles of `fence_passed=true probes=2
  consecutive_misses=2 elapsed_ms=300` AND **nine** consecutive
  cycles of `vm_index leak counter = 0`. The leak-counter wiring
  has had nine full opportunities to fire (all nine reached the
  teardown phase) and surfaced zero signal. C-7-LT-2 holding is now
  more than empirical — it is the leak ledger's own falsification.
- **Demotion verdict**: **YES, demote to MINOR-permanent**. The
  carry pattern has crossed the credibility threshold the r22
  review proposed ("if r21–r25 all show zero leaks AND C-7-LT-10
  lands cleanly, fold this into the 'remove dead leak-counter
  wiring' ADR"). C-7-LT-10 landed at `1d5aa2f9` and confirmed
  empirically at smoke-r22 (CH consumed the rewritten config).
  r21–r25 partial window: r21 + r22 = 2 of 5, both zero. The
  remaining 3 cycles (r23–r25) will land as smoke-r23/24/25; if
  any of those show a non-zero counter, re-escalate. Otherwise the
  next architecture round (r24+) should write the
  "remove-dead-leak-counter-wiring" ADR, NOT the
  "implement-leak-reaper" PR.
- **Action**:
  1. Carry through r23–r25 to complete the 5-cycle window the r22
     demotion criterion specified.
  2. r26+ writes a single ADR: "leak-counter wiring is dead code;
     remove and document the empirical basis (15-cycle
     fence_passed=true + 15-cycle zero counter)."
  3. The three lying-comment sites at `:1081/:1162/:1184` should
     be cleaned up in the same PR (delete the wiring + comments
     together).
- **Severity**: MINOR (held from r22-A4, demotion criterion at 9/15
  cycles — on track for r26+ closure).

### [r23-A5] R22-I1 wake-machine terminal-overwrite counter landed (`f98611fb`); r20-A3/r19-A3/r20-A4/r20-A5 ADR bundle still open as process debt

- **Where**: `f98611fb` closed R22-I1 (wake-machine surfaces
  terminal-overwrite-blocked via tracing + counter — 3 caller sites
  in wake_machine.rs at `:128, :161, :487`). +2 lib tests. Pure
  observability, no behavior change. Counter wiring pattern mirrors
  `inc_vm_index_leak`.
- **Remaining open process debt**:
  - r19-A3: per-phase wall-time `stop()` metric (no commits since
    r19).
  - r20-A3: ADR bundle for timing constants
    (`wait_for_agent_silent` + `wait_for_job_gone` + takeover).
  - r20-A4: C-7-LT-2 invariant + regression test (9-cycle
    fence_passed=true; integration test still not landed).
  - r20-A5: R19-I1 unverified-in-prod (deferred for the 9th cycle
    by smoke-r22, gated on first `livez_polling` reach — now one
    GREEN cycle away post-C-7-LT-12a).
- **What I see**: the smoke-r22 cycle did not exercise livez_polling
  (terminal at `restoring → failed`). R19-I1 verification is
  structurally downstream of C-7-LT-12a's empirical confirmation.
  The r22-A5 carry forward of r21-A5 (re-route to code-quality r22)
  appears not to have landed; bundle r19-A3 + r20-A3 + r20-A4 for
  the next code-quality cycle's ADR pass.
- **Action**:
  - code-quality r23 (or r24): execute r20-A3 ADR bundle + r19-A3
    metric + r20-A4 regression test. Single ADR + 2 PRs.
  - r20-A5: stays carried until first smoke cycle reaches
    livez_polling (smoke-r23 the earliest plausible).
- **Severity**: MINOR — process backlog; no runtime risk.

## Cross-lens consensus

- **smoke-r22 cluster review**: C-7-LT-12 reclassified from
  CH-internal to driver-side via verbatim stderr; C-7-LT-11
  observability spend is the highest-leverage patch in the
  22-cycle arc. Architecture endorses the r22-A2 forecast
  REFUTATION as a meta-lesson worth codifying (r23-A3).
- **test-coverage r22 (R22-T1)**: field-list parity contract test
  is the structural fix for cross-emitter drift; r23-A1 escalates
  it from IMPORTANT (r22-A3) to CRITICAL on the strength of the
  C-7-LT-12a cross-emitter addition (second strike).
- **code-quality r22 (R22-I1)**: wake-machine terminal-overwrite
  counter landed at `f98611fb`; lens drained for the round. r23-A5
  notes the open r19-A3 + r20-A3 + r20-A4 backlog for r23+.
- **api-surface r21 (R21-API2)**: same finding as R22-T1 from the
  api-surface side; r23-A1 is the single canonical version.
- **security r21 (R21-S1 + R21-S2)**: user_id + SandboxId
  unvalidated on restore boundary remain open. With
  `rootfs_source` now joining the restore Config wire surface, the
  validator-symmetry contract test scope expands — the parity test
  in r23-A1 also constrains future additions.

## Lens hand-off

1. **Implementer (R22-T1, immediate)**: land the field-list parity
   contract test per r23-A1. ~30 LOC, no driver dependency, no
   cluster cycle. Single PR title:
   "field-list parity contract test (R21-API2 / R22-T1 / r23-A1)".
2. **Cluster-smoke r23**: gated on driver v12 upload + controller
   pin bump v11→v12. **Falsification criterion**: state machine
   reaches `livez_polling` (R19-I1 exercises for the first time).
   If RED with a NEW failure layer beyond DeviceManager, the
   r23-A3 ADR's triage tier ladder is the playbook.
3. **Architecture r24**: gated on smoke-r23 outcome.
   - GREEN → r24 pivots to "post-restore stability + first-WAKE-OK
     baseline" (R19-I1 verification, AEAD-decrypt cost
     characterisation, wrapper retirement r20-A1 cutover plan).
   - RED with new layer → r24 scopes the failure per the triage
     tier ladder, no source-audit before stderr capture.
4. **Code-quality r23**: bundle r19-A3 + r20-A3 + r20-A4 into a
   single ADR + 2 PRs per r23-A5.
5. **ADR landing (r23-A3)**: write
   `docs/decisions/2026-05-25-restore-debug-playbook.md` — triage
   tier ladder + observability-before-architecture rule + r22-A2
   retro. Recommend bundling with the code-quality r23 ADR pass.
6. **Wrapper retirement (r20-A1)**: STRUCTURALLY UNBLOCKED per
   r23-A2; one full GREEN smoke cycle is the cutover prerequisite.
   Earliest: smoke-r24.

## r19-A1..A5 + r20-A1..A5 + r21-A1..A5 + r22-A1..A5 carry status

| Finding | r23 status |
|---|---|
| r19-A1 (vm_index leak ledger + reaper) | **OPEN, MINOR — DEMOTION TRACK** (r23-A4). 9-cycle zero counter; 9/15 cycles to formal demotion ADR. |
| r19-A2..r19-A5 | CLOSED (r20). |
| r20-A1 (three-rewriter coexistence + wrapper retirement) | **STRUCTURALLY UNBLOCKED** (r23-A2). Pending smoke-r23 GREEN. |
| r20-A2 / r20-A3 / r20-A4 / r20-A5 | OPEN — bundle for code-quality r23 (r23-A5). |
| r21-A1..r21-A5 | r21-A1 CLOSED at `fcac5355`; remainder carried per r22 mapping. |
| r22-A1 (driver/wrapper restore-input contract divergence) | **CLOSED** (r23-A2). Driver post-C-7-LT-10 + C-7-LT-12a matches wrapper's one-input-directory model. |
| r22-A2 (next failure class will be CH-internal — outside codebase) | **SUPERSEDED / REFUTED** by smoke-r22. r23-A3 codifies the lesson as an ADR. |
| r22-A3 (R21-API2 field-list parity contract test) | **ESCALATED to CRITICAL** (r23-A1). Cross-emitter additions of `rootfs_source` make the latent risk active. |
| r22-A4 (r19-A1 carry, MINOR demotion candidate) | → r23-A4. 9/15 cycles. |
| r22-A5 (R20-C1 closed, r19-A3/r20-A3 bundle open) | → r23-A5. R22-I1 added; bundle remains open. |
