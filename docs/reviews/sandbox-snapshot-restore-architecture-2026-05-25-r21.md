# Sandbox snapshot-restore architecture review — 2026-05-25 r21

**Reviewer**: architecture-r21 (post-smoke-r17 + driver v8 cycle)
**HEAD**: `4d73a5d1` (`feat/sandbox-snapshot-restore`, driver v8 pin landed)
**Predecessor**: r20 at `8718120b`. Diff since r20: r17-Q3 closure
(`17d65f83`), r25/r26/r27 reviewer artifacts (`b18782f6` / `2fed96bc`
/ `eabf7a4a`), C-7-LT-7 controller-side `user_id` emission
(`03d2f4a8`), driver-v8 pin bump (`4d73a5d1`).
**Lens**: architecture (READ-ONLY).

## Summary

The chain is converging but **r21 surfaces a real CRITICAL**: the
C-7-LT-7 controller-side `user_id` emission only landed in the
**cold-boot** builder (`build_nomad_job_json_with`, nomad_ch.rs:2451)
— **the restore-path builder `build_restore_nomad_job_json` (which is
the ONLY path that actually runs in WAKE) does NOT emit `user_id` in
its typed ChPlugin Config block**. Driver v8 will receive a Config
with `user_id=""`/missing on the restore branch, and the per-user-home
allow-list will reject `/var/zeroship/ch/users/<user_id>/home.img`
again on smoke-r18 — byte-for-byte the C-7-LT-7 failure mode. This is
a wire-format gap, not a validator gap; **the architectural
"three-rewriter / one-namespace-at-a-time" pattern r20-A1 flagged is
now masking a wire-format coverage hole on the same axis** (the
controller has TWO emitters of the ChPlugin Config — one in nomad_ch
for cold-boot, one in restore_handler for restore — and the C-7-LT-7
fix touched the wrong one).

Five findings (1 CRITICAL, 2 IMPORTANT, 2 MINOR). r19-A1 leak ledger
+ reaper still on paper; the empirical leak rate is zero across FOUR
cycles (smoke-r14..r17), so the r20 re-rank to IMPORTANT holds.

## CRITICAL

### [r21-A1] C-7-LT-7 user_id emission landed in cold-boot builder ONLY — restore-path builder still omits the field; driver v8 will re-fail smoke-r18 with identical disks[2] rejection

- **Where**:
  - Fix landed (cold-boot): `nomad_ch.rs:2451`
    (`build_nomad_job_json_with` ChPlugin branch) — `"user_id":
    user_id` added per `03d2f4a8`.
  - Fix NOT landed (restore): `restore_handler.rs:2345-2364`
    (`build_restore_nomad_job_json` ChPlugin branch). The Config
    emits `vm_index, kernel, cpus, memory_mb, restore_from,
    sandbox_id, workspace_img, user_home_img, pubkey_hex,
    subnet_base_octet, disks, fs, net` — **no `user_id`**. Grep
    `grep -n '"user_id":' restore_handler.rs` returns ZERO hits.
- **Why this is the bug-of-record**: smoke-r17's verbatim failure
  (`disks[2].path = /var/zeroship/ch/users/usr_033MDp768TZVdYSaq2M14o
  /home.img … NOT under any allow-list prefix`) is the **restore
  branch** — Nomad event quotes `StartTask (restore branch)`. Driver
  v8's per-user-home allow-list reads `cfg.UserId` from task config;
  on restore, that field arrives missing/empty.
- **Why the cold-boot fix isn't enough**:
  `build_nomad_job_json_with` is only invoked from `create()` (cold-
  boot, nomad_ch.rs:718). The restore path's `submit_restore_job`
  (restore_handler.rs:2029-2073) calls its OWN builder
  `build_restore_nomad_job_json` exclusively — a *separate* function
  not touched by `03d2f4a8`.
- **Test contract gap that masked it**:
  `ch_plugin_restore_jobspec_populates_restore_from` (l.3563-3597)
  asserts `vm_index/kernel/cpus/memory_mb/restore_from/
  subnet_base_octet` — NOT `user_id`. The cold-boot pin test
  `ch_plugin_jobspec_includes_all_task_config_fields` (updated by
  `03d2f4a8`) is the WRONG TEST for the restore wire-format pin.
- **Fix shape (one-line mirror of `03d2f4a8`)**: add `"user_id":
  user_id,` to the ChPlugin Config in `build_restore_nomad_job_json`
  and assert it in `ch_plugin_restore_jobspec_populates_restore_from`.
  `user_id: &str` is already a function parameter (l.2230); no
  signature change needed.
- **Tie-in with r20-A1**: r20-A1 flagged THREE coexisting rewriters
  (controller / wrapper / driver). r21-A1 surfaces that the
  controller layer ITSELF has TWO emitters (cold-boot + restore) with
  no shared field-list contract. C-7-LT-7 patched one emitter; the
  other was missed. Exactly the audit-ambiguity / no-ownership-rule
  failure mode r20-A1 predicted, surfacing within 24 hours.
- **Severity**: CRITICAL — smoke-r18 is the gating cycle for T-8b
  cutover. Without this one-line fix, r18 burns another cluster
  cycle (~$30 + ~2 h) reproducing the C-7-LT-7 failure verbatim.

## IMPORTANT

### [r21-A2] user_id unvalidated on restore boundary — defense-in-depth gap on the path-isolation axis

- **Where**: `read_snapshot_row` (restore_handler.rs:639-664) pulls
  `user_id` from pg as a raw `String` with no format check. It
  flows verbatim into `cfg.user_home_dir_root.join(user_id)`
  (l.2258) AND into the driver Config (post-r21-A1 fix). The schema
  CHECK is NOT NULL only — no `usr_<base62>` shape enforcement.
- **What I see**: `validate_typed_id` exists at nomad_ch.rs:3633 but
  is invoked ONLY in cold-boot `create()` (l.472). The restore path
  has no equivalent boundary check. Driver-side mirror waives
  `isTypedID` on restore (`restore_task.go:273-278`, same shape as
  R20-S2 for SandboxId per r25 security). With C-7-LT-7's per-user-
  home allow-list now load-bearing, `user_id` becomes a *trust
  anchor for path isolation* — `filepath.Join` Cleans `..`
  components and silently widens the allow-list root. Same surface
  as R20-S2.
- **Action**: add `validate_typed_id(&user_id, "usr", "user_id")?`
  at the top of `submit_restore_job` (mirrors cold-boot l.472).
  Two-line change; pairs naturally with r21-A1.
- **Severity**: IMPORTANT — defense-in-depth, no known exploit
  vector today (controller is the only writer to
  `sandbox.sandboxes.user_id`). Becomes load-bearing once
  driver-side per-user allow-list ships to prod.

### [r21-A3] Wrapper-retirement dependency ledger — pin r20-A1 gating list before T-8b-cutover

- **Where**: wrapper path-rewrite block (`nomad-vm-wrapper.sh:486-
  624`) unchanged since `801ae449`. `TaskDriverMode::RawExec` is
  STILL the default (nomad_ch.rs:2232-2236); ChPlugin is opt-in via
  `SANDBOX_TASK_DRIVER=ch_plugin`.
- **Gating list** for r20-A1 wrapper retirement, by precedence:
  1. **R19-S1 unfixed** (per r25 security): driver-side
     `validatePathByKind` is lexical-only (no `EvalSymlinks`);
     wrapper has realpath check (R15-S2). Retiring wrapper BEFORE
     R19-S1 is a security-posture regression. Security r20 cross-
     lens already escalated; architecture endorses.
  2. **r21-A1 wire-format gap**: restore-path builder must emit
     `user_id` before driver's per-user-home allow-list works on
     restore. Until r21-A1 lands + cluster-verifies, the driver-only
     path-rewriter story is incomplete.
  3. **Cold-boot ChPlugin smoke-coverage**: full
     CREATE→SNAPSHOT→WAKE→STOP under ChPlugin-only with wrapper
     absent has ZERO cluster evidence. r20-A1 implicitly assumed
     this coverage.
  4. **Operator-rollback path**: `SANDBOX_TASK_DRIVER` is process
     env, not a registry flag. Wrapper-absent rollback is "re-
     deploy" not "flip env." Document before cutover.
- **Action**: pin items 1-4 in a `docs/decisions/2026-05-25-wrapper-
  retirement-blockers.md` ADR; each gets a "checked" criterion
  (item 3 = "two consecutive GREEN smoke cycles under
  SANDBOX_TASK_DRIVER=ch_plugin with wrapper script absent from
  worker image"). T-8b-cutover PR blocked until all four flip.
- **Severity**: IMPORTANT — process gap; no runtime risk today.

## MINOR

### [r21-A4] r19-A1 leak ledger + reaper — fourth consecutive zero-leak cycle; r20 re-rank holds, downgrade to MINOR

- **Where**: `cleanup_orphans_at_startup` (nomad_ch.rs:431-464),
  three lying-comment sites at `:1081/:1162/:1184` — all unchanged.
  `grep -rn "leak_reaper\|leak_ledger\|VmIndexLeakLedger"` returns
  zero. Counter wiring landed (metrics.rs); reaper did not.
- **What I see**: smoke-r14/r15/r16/r17 all `leak counter = 0`.
  Hidden-risk audit (per intake): the counter has two reasons
  (`wait_failed`, `host_fence_timeout`) both zero. The third class
  would be a leak THROUGH the counter — i.e. silent release on a
  bad path. Audited the three `vm_index_allocator.release` sites
  (cold-boot rollback, stop fence-passed branch, restore rollback);
  all three release on success-only or controller-confirmed-gone
  paths. No silent-release surface identified.
- **Action**: hold deferral. Ship the per-process
  `detach_isolated` reaper FIRST when bandwidth opens (r20-A2
  sketch); ledger waits for first measurable multi-process leak.
  Three lying-comment sites still need cleanup or implementation.
- **Severity**: MINOR (down from IMPORTANT) on fourth-consecutive-
  zero evidence.

### [r21-A5] r20-A3 ADR bundle still pending — wait_for_agent_silent, wait_for_job_gone, takeover thresholds

- **Where**: r20-A3 recommended bundling three timing-constant
  extractions into `docs/decisions/2026-05-25-teardown-timing-
  constants.md`. No commit since r20. Code-quality r21 (`eabf7a4a`)
  was a separate-lens review and did not pick this up; the lens
  hand-off in r20 routed it to "code-quality r21" — which fired but
  on a different scope.
- **Action**: re-route the r20-A3 ADR bundle to a code-quality r22
  hand-off; precedent (R20-I1 `ed30f5d0`) is clean and ready to
  duplicate. Low risk, value is durable-history-preservation, not
  runtime-safety.
- **Severity**: MINOR — process backlog item.

## Cross-lens consensus

- **API-surface r20**: R20-API1 flagged three-rewriter coexistence.
  r21-A1 shows the controller layer itself has TWO emitters (cold-
  boot + restore) and C-7-LT-7 only touched one. Aligned action: pin
  BOTH ChPlugin emitters via union-walk test contract.
- **Security r20**: R20-S2 (SandboxId unvalidated on restore) and
  r21-A2 (user_id unvalidated on restore) are the same shape, both
  rooted in `restore_task.go:273-278` waiver. Combined fix: add
  controller-side `validate_typed_id` re-check at
  `submit_restore_job` entry covering both.
- **Test-coverage r20**: deferred r19-A1 integration test
  (`stub wait_for_agent_silent → Err → assert leak telemetry`) is
  fifth-cycle overdue; the only remaining end-to-end check on the
  leak-counter wiring.

## Lens hand-off

1. **Implementer (immediate)**: land r21-A1 fix in
   `restore_handler.rs:2345-2364` (one-line `"user_id": user_id,`
   addition + update `ch_plugin_restore_jobspec_populates_restore_
   from` test to assert it). Cycle: <30 min.
2. **API-surface r21**: define a single-source-of-truth field list
   for the ChPlugin Config wire format (TaskConfig schema mirror).
   Both emitters reference the same constant; tests walk the union.
3. **Security r21**: pair r21-A2 + R20-S2 into a combined boundary
   re-validation patch at `submit_restore_job`.
4. **Code-quality r22**: execute r20-A3 + r21-A5 ADR bundle as
   originally scoped.
5. **Cluster-smoke r18**: gated on r21-A1 fix landing. Falsification
   criterion: state machine reaches `livez_polling`; if it again
   fails at the path-rewriter layer, that's a NEW class (e.g. a
   disks[3] surface) — but the CURRENT defect cannot recur if
   r21-A1 lands first.

## r19-A1..A5 + r20-A1..A5 carry status

| Finding | r21 status |
|---|---|
| r19-A1 (vm_index leak ledger + reaper) | **OPEN, downgraded MINOR** (r21-A4). Five-cycle zero counter. |
| r19-A2 (wake_jobs takeover sweep) | CLOSED (r20). |
| r19-A3 (per-phase wall-time `stop()` metric) | OPEN — no commits since r19. |
| r19-A4 (`from_host_fence_timeout` ADR) | CLOSED (r20). |
| r19-A5 (other ureq sites audit) | CLOSED for livez (R19-I1 in v30); Nomad-side LOW-risk, skipped. |
| **r20-A1** (three-rewriter coexistence, retire wrapper) | **OPEN**. r21-A1 surfaces it as a current bug; r21-A3 documents the retirement blockers. |
| r20-A2 (r19-A1 carry, re-ranked) | OPEN → downgraded MINOR (r21-A4). |
| r20-A3 (ADR bundle: wait_for_agent_silent + wait_for_job_gone + takeover) | OPEN (r21-A5). |
| r20-A4 (C-7-LT-2 → invariant + regression test) | OPEN — five cycles `fence_passed=true`; integration test still not landed. |
| r20-A5 (R19-I1 unverified-in-prod) | OPEN — smoke-r17 again did not reach livez_polling. Carries to smoke-r18. |
