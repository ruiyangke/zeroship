# Sandbox snapshot-restore architecture review — 2026-05-25 r25

**Reviewer**: architecture-r25 (post-T-8b-stress-r2 RED 2/60, v34 bundle staged for r3)
**HEAD**: `92c45d26` reviewer-artifacts; bundle = `d638b10f` (host_dir leak + verbatim driver-msg) + `e82bffd7` (sweeper) + `6b240683` (README) + `177ff165` (driver v14)
**Predecessor**: r24 at `30960451`. Diff since r24: bundle commits above, R24-I1 wake_machine WARN context (`1c255a00`), R24-T1 stress harness in-repo (`61492e54`/`dd2079a9`/`1fb69840`), T1 sandbox_admin_ro role (`7b5d84f5`/`97fcbcda`/`038ff3c7`), `10d4200b` stress-r2 RED retro, `03d3470f` round-24 reviewer artifacts.
**Lens**: architecture (READ-ONLY).

## Summary

The v34 bundle is **architecturally sound for the bug it actually closes** (host_dir cleanup-vs-retry race) and **architecturally regressive for one it accidentally introduces** (path-bearing driver msgs now flow into a column that's readable by the new `AdminRole::ReadOnly` bearer that landed in the same review window). r24-A1's "fsync-then-pray was fixing for a mechanism we have not proven" prediction is **vindicated verbatim** — the d638b10f commit message confesses the v33 fsync_dir hygiene was correct but redundant, and the *actual* cause was cleanup-vs-retry (`CreateGuard::drop` step 3 `rm -rf`'d host_dir while a concurrent retry's StartTask was still reading workspace.img). The 33% race-win rate (96/144 alloc submissions) was the data point that excluded "deterministic visibility" and pointed at "cleanup window."

r24-A1 (typed StagingManifest) is **NOT IMPLEMENTED**. The bundle's two halves close half of r24-A1:
- **Message-passing half** (`extract_failed_task_event_msgs` → composed wire string): partially done, as an unstructured `format!` concatenation. The schema half (typed `BackendError::StagingPathMissing { path, expected_size, sandbox_id }` variant carrying path-bearing fields out of the message field) is **missing**.
- **Schema half** (typed `StagingManifest` with compile-time parity between controller-staging-set and driver-preflight-set): not started.

r24-A2 (kernel-state surface enumeration) is **NOT IMPLEMENTED**. The bundle closes ONE surface (host_dir) via the sweeper-owned pattern, which is the *right* architectural shape — but the pattern generalizes to at most 2-3 surfaces before per-sweeper-per-surface explodes.

The **driver-msg leak through RO is structurally net-new with this bundle**. T1 (`AdminRole::ReadOnly` for `poll_wake`) landed at `7b5d84f5`/`97fcbcda`/`038ff3c7`. d638b10f added verbatim path-bearing driver msgs to `wake_jobs.error_message`. The two commits compose into a regression that neither commit alone would create.

Six findings (2 CRITICAL, 2 IMPORTANT, 2 MINOR). r24-A1 **REMAINS OPEN**, r24-A2 **REMAINS OPEN**, r24-A3 **STILL DID NOT LAND** — third escalation cycle.

---

## CRITICAL

### [r25-A1] Verbatim driver-msg propagation + new `AdminRole::ReadOnly` bearer compose into a path-leak through `wake_jobs.error_message` — r24-A1 schema half MUST close before T-8b-stress-r3

The leak in 5 hops:
1. Nomad alloc fails. Driver emits `disk[1] /var/zeroship/ch/<uuid>/workspace.img does not exist` in `TaskEvent.DisplayMessage`.
2. Controller's `wait_for_alloc_running_blocking` calls `extract_failed_task_event_msgs(a)`, composes verbatim path-bearing string.
3. `RestoreHandlerError::Backend(composed)` wraps it. `wake_machine::rollback_and_classify` produces `Phase::Failed { code: WakeErrorCode::RestoreFailed, message: composed }`.
4. `Phase::Failed`'s persist runs `sanitize_error_message(composed)`. The function strips IPs only — paths + typed_ids survive.
5. Operator holding RO bearer issues `GET /admin/sandboxes/<id>/wake/<wake_id>`. `render_wake_poll_response` plumbs `row.error_message` directly into the wire envelope's `message` field.

Sites:
- `crates/sandbox/src/backend/nomad_ch.rs:2756` — `extract_failed_task_event_msgs`
- `crates/sandbox/src/backend/nomad_ch.rs:2645` — cold-boot composition
- `crates/sandbox/src/restore_handler.rs:2577` — wake-path composition
- `crates/sandbox/src/wake_machine.rs:165, 592` — sanitize + persist
- `crates/sandbox/src/wake_machine.rs:769-1015` — `sanitize_error_message` (IPs only)
- `crates/sandbox/src/admin_handlers.rs:1977-1988` — `render_wake_poll_response` (does NOT funnel through `err_safe`)

Two commits in the same review window composed into a regression: T1 widened the read surface (RO bearer); d638b10f widened the data on that surface (verbatim driver text). Neither commit's threat model considered the other.

**Fix shape (~80-120 LOC)**: typed `DriverFailureMsg { task, kind: DriverFailureKind, message }` with `DriverFailureKind::StagingPathMissing { path, sandbox_id }`, `Phase::Failed.structured` field, role-aware `render_wake_poll_response` that elides path-bearing fields for ReadOnly.

**Severity**: CRITICAL. Block T-8b-stress-r3 declaring "verbatim driver-msg propagation closed" until schema half lands.

### [r25-A2] Sweeper-owned cleanup is the right shape for host_dir but does NOT generalize — wrapper retirement still blocked

Sites:
- `crates/sandbox/src/sweep.rs:909-1131` — `run_host_dir_gc_once` is purpose-built for host_dir filesystem shape
- Remaining surfaces (per r24-A2 table): cgroup, mount-ns, vsock CID, jailer chroot, PID files, systemd transient units. **None has a sweeper-owned reaper.**

Each surface needs its own eligibility predicate + enumeration mechanism + reap mechanism + grace policy. Total estimated 1500-2000 LOC across 6-8 surfaces × ~250 LOC/surface. Alternative shape (top-level unified reaper) has cross-surface dependency risks.

**The bundle made a choice (per-surface sweeper, host_dir as the prototype) without an ADR codifying that choice.** Future surfaces inherit the per-surface explosion path by mimicry.

**Recommendations**:
1. Produce r24-A2 enumeration table as `docs/decisions/2026-05-25-kernel-state-surface-inventory.md` ADR
2. Decide per-surface-sweeper vs unified-reaper ONCE
3. Block wrapper retirement on the table, NOT on the bundle landing

**Severity**: CRITICAL. Three rounds of "wrapper retirement structurally unblocked" claims (r19-A1, r20-A1, r23-A2) all REFUTED by next stress cycle.

---

## IMPORTANT

### [r25-A3] r24-A1 schema half still not implemented — observability half done, contract half pending

Bundle's `extract_failed_task_event_msgs` closes observability. Contract still drifts (controller still stages by-path, driver preflights by hard-coded `disks[*].path`). Adding a new disk image still requires 3 coordinated edits across 2 binaries with no compile-time enforcement.

**The fsync_dir doc-comment at `nomad_ch.rs:3698-3706` is now actively misleading** — cites Bug 1's 49/60 motivation but d638b10f's commit confirms that motivation was wrong. Code is harmless (fsync_dir correct-if-redundant), but doc lies about its own reason.

**Three mis-diagnoses in two cycles**: rootfs-only (r23) → fsync visibility (r24) → cleanup-vs-retry (v34). None of three restructured the staging contract; each added defense-in-depth at a different layer.

**Fix shape**: unchanged from r24-A1 Phase 1 (~40-60 LOC). r25-A1 provides a second reason to land it; structured fields ARE the typed manifest in different costume.

### [r25-A4] r24-A3 restore-debug-playbook ADR still did NOT land — third escalation cycle

Three force-multiplier evidence points:
1. smoke-r22: `DeviceManager(Disk(NotFound))` → reclassified from "CH-internal" to "driver staging" in 1 cycle
2. stress-r1: `disk[1] /...workspace.img does not exist` → reclassified from "Failed tasks" to "controller-staging" via verbatim observable
3. **stress-r2: 33% race-win-rate observation** → reclassified from "staging visibility" to "cleanup-vs-retry race" (THIS cycle, d638b10f)

All three are layer-N verbatim observables resolving multi-cycle uncertainty.

**Fix shape**: 1 markdown file, ~130 lines, no code. Six sections: triage tier ladder + observability-before-arch rule + r22-A2 retro + r23-A2 retro + **r24-A1 retro (fsync_dir REFUTED by 33% race-win observation) — new in r25** + stress-as-gate rule.

**Block T-8b-stress-r3 declaring ≥95% GREEN on this AND r25-A1 schema closure.**

---

## MINOR

### [r25-A5] r19-A1 leak ledger — 11/15 cycles zero counter; on track for r26 ADR closure

Stress-r2 + driver v14's `taps_orphaned_total` join the falsification window. Demotion track: 4 more cycles to r25-A5 closure. r26 should bundle leak-ledger ADR removal + kernel-state-surface inventory ADR (r25-A2 rec #1) — siblings: one removes over-instrumented surface, other under-instruments six new ones.

### [r25-A6] Sweeper-owned host_dir GC — three minor concerns

1. **Hard-coded 1-hour grace, no per-tenant override**. Possible: `SANDBOX_HOST_DIR_GC_GRACE_BY_ERROR_CODE_SECS_JSON` envvar.
2. **TOCTOU between gate A (`get_sandbox_row`) and gate B (`find_pending_wake_for_sandbox`)**. Practical window narrows to zero given handler invariant ("refuse wake for terminal sandbox BEFORE wake_jobs insert"). Invariant load-bearing but not tested.
3. **No `host_dir_leaks_total` counter**. The bundle moves host_dir cleanup to sweeper-owned; per-alloc paths now LEAK BY DESIGN. Symmetry argues for killing `vm_index_leaks_total` (per r24-A5 demotion track) AND adding `host_dir_leaked_total` + `host_dir_reaped_total` counter pair.

**Severity**: MINOR — all observability/policy, not correctness.

---

## Cross-lens consensus

- **security r25 (R25-S1)**: r25-A1's path-leak vector IS R25-S1 from security lens. Converge on typed-variant fix (r24-A1 schema half), NOT on regex-expansion of `sanitize_error_message`.
- **code-quality r25**: bundle r25-A1 typed-variant + r24-A4 SnapshotRowMeta + r19-A3/r20-A3/r20-A4 ADR pass + r24-A3 ADR. Five-item bundle.
- **api-surface r23**: WakeErrorCode wire codes expand; coordinate before typed-variant PR.
- **cluster T-8b-stress-r3**: gated on r25-A1 schema + r25-A4 ADR + bundle (LANDED).
- **test-coverage r24/r25**: r25-A1 adds two new contract dimensions: render-by-role test + typed-variant exhaustiveness.
- **concurrency r23**: r25-A6 concern 2 (TOCTOU) is concurrency territory; pin invariant in test.

---

## Lens hand-off

1. **r25-A1 (P0)**: typed-variant schema half, ~80-120 LOC. Block T-8b-stress-r3 verbatim-msg closure on this.
2. **r25-A2 (P0)**: kernel-state-surface inventory ADR. No code; one document.
3. **r25-A4 (P0)**: restore-debug-playbook ADR. Three cycles of escalation.
4. **r25-A6 #3 (P2)**: counter pair (kill vm_index_leaks_total, add host_dir_leaked/reaped).
5. **r25-A6 #1 (P3)**: per-error-code grace override.
6. **T-8b-stress-r3 (in flight)**: falsification criteria: ≥95% e2e, zero stranded host_dir post-1h, zero stranded taps, verbatim driver-msg WITHOUT path-bearing fields visible to RO bearer.

---

## Carry status

| Finding | r25 status |
|---|---|
| r19-A1 leak ledger | OPEN MINOR — DEMOTION TRACK 11/15 |
| r20-A1 wrapper retirement | **STILL RE-BLOCKED** — gate is r24-A2 table |
| r22-A1 driver/wrapper contract divergence | r25-A1 opens THIRD dimension (path-bearing wire fields) |
| r23-A2 wrapper retirement structurally unblocked | **STILL RE-BLOCKED** |
| r23-A3 / r24-A3 ADR | **OPEN — third escalation** |
| r24-A1 typed StagingManifest | **HALF-CLOSED** (observability half by bundle; schema half pending) |
| r24-A2 kernel-state surface enumeration | **OPEN, mostly unmoved** (1 of 7 closed) |
| r24-A4 SnapshotRowMeta DRY | OPEN, bundle into r25 code-quality 5-item PR |
| T1 sandbox_admin_ro role | CLOSED at 038ff3c7. **Composes with d638b10f into r25-A1 path-leak.** |

---

**Final note**: the v34 bundle's *correctness* is high. d638b10f's diagnosis ("33% race-win rate excludes deterministic visibility") is the kind of empirical rigor r24-A3's ADR is designed to capture. The bundle ships exactly what it claims to ship. What it does NOT ship is the structural-debt reduction r24-A1 and r24-A2 asked for; **the bundle is a fix, not a restructure**. The cluster will likely go GREEN at stress-r3 on this bundle. Whether the next stress cycle finds the next per-surface or per-emitter contract drift is the open question r25 cannot answer; r26 (after stress-r3) gets to test the hypothesis empirically.
