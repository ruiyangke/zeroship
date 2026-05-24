# Sandbox snapshot-restore architecture review — 2026-05-25 r26

**Reviewer**: architecture-r26 (post-T-8b-stress-r3 RED 1/60, r3-A node-affinity IN FLIGHT)
**HEAD**: `326f4d4f` or later. Landed since r25: `79871194` (R23-API1 `WakeErrorCode::StagingPathMissing`), `022f778a` (R25-I1/I2/S1 typed staging), `6476d18b` (R25-S1 backlog close), `d0a744ce` (stress-r3 cluster review), `28fa64d1` (restore-debug-playbook ADR), `3e853cc6` (kernel-state surface inventory ADR), `2acedc35` (deferred backlog close r24-A2/A3 + r25-A2/A4), `05440498` (driver v15 tap-poll on r3-B), `3d431eb8` (controller r3-C two-pass DriverFailure preference). IN FLIGHT: `a27a3b3fee12bd12f` (r3-A node-affinity constraint).
**Predecessor**: r25 at `92c45d26`.
**Lens**: architecture (READ-ONLY). **NO changes proposed to `lib.rs`, `nomad_ch.rs`, `restore_handler.rs` per brief** (r3-A in flight on those).

---

## Summary

Three landmark commits closed three of r25's open findings:

- **r25-A1 (CRITICAL — typed StagingManifest schema half)** → **CLOSED** at `022f778a` via `SubmitRestoreError::Preflight { which, path, source }` + `RestoreHandlerError::StagingPreflight { which, sandbox_id_typed }` + `WakeErrorCode::StagingPathMissing` + wire code `staging_image_missing`. The path-free `Display` impl (`restore_handler.rs:178-190`) + `log_detail` tracing-only side channel (`restore_handler.rs:212-223`) is the **reference shape for every other backend-failure → wire-message edge in the system**. It should be promoted to a pattern, not held as a one-off preflight fix.
- **r25-A2 (CRITICAL — kernel-state surface enumeration ADR)** → **CLOSED** at `3e853cc6`. ADR enumerates 11 surfaces (2 CLOSED + 5 OPEN + 4 N/A), per-surface closure-roadmap (re-entry test + counter + sweeper + grace policy), commits to per-surface-sweeper pattern over unified-reaper.
- **r25-A4 (IMPORTANT — restore-debug-playbook ADR)** → **CLOSED** at `28fa64d1`. Codifies tier ladder + observability-before-architecture + stress-as-gate rules. Captures r22-A2 / r23-A2 / r24-A1 retros.

But stress-r3 went **RED 1/60 (worse than r2)** the same cycle three ADRs/fixes landed. Root cause diagnosed at the r3-A/B/C bundle: **the controller stages on its local fs but Nomad picks any worker** (78% cross-node failure at WORKER_COUNT=3). Three of the four landed fixes (R25-I1/I2/S1 typed staging + r3-B driver tap-poll + r3-C two-pass DriverFailure preference) close edges *adjacent* to the bug but not the bug itself. **r3-A node-affinity is the load-bearing fix**, still in flight as of `326f4d4f` reviewer artifacts.

That makes r3-A **the 4th instance** of the playbook's "next bug will be reclassified by a verbatim observable from a layer we currently misdiagnose" pattern (smoke-r22 stderr-tail → stress-r1 verbatim driver-msg → stress-r2 33% race-win rate → stress-r3 78% cross-node-placement rate). **The playbook is now 4-for-4 on force-multiplier evidence.** ADR Section 5 ("Force-multiplier retrospectives") is missing an r3-A retro and the table at Section 2 ("Triage tier ladder") arguably needs a new Tier 0 (placement) above Tier 1 (Validator).

Six findings (1 CRITICAL, 3 IMPORTANT, 2 MINOR). r25-A3 (fsync_dir doc-comment lie) **REMAINS OPEN**. r19-A1 leak ledger demotion now bundled into kernel-state ADR (r25-A2 closed → leak-ledger demotion track absorbed into ADR section 8). New: r26-A1 (structured-error pattern reuse beyond preflight). r26-A2 (post-r3-A architectural assumption breakage). r26-A3 (Option 2/3 deferred-alternatives gap in ADRs). r26-A4 (cutover gate stack re-assessment — 5 items, not 3).

---

## CRITICAL

### [r26-A1] R25-S1 typed-staging pattern is a reusable structured-error template — every other `rollback_with(_, _, _, e: String)` site is the next regression

Sites that take a String error as the wire-message field (NOT typed):

- `wake_machine.rs:447-453` — `WakeErrorCode::LivezTimeout` with `e: String` from `wait_for_livez`
- `wake_machine.rs:484-486` — `WakeErrorCode::ClockResyncFailed` with `e: String` from `clock_resync_post_restore`
- `wake_machine.rs:497-499` — `WakeErrorCode::RegisterFailed` with `e: String` from `register_restored`
- `wake_machine.rs:462-472` — `RestoreHandlerError::Internal(format!(...))` for unseal failures (path-bearing: `"post-wake unseal sandbox {}: {e}"` includes sandbox typed_id, but the inner `{e}` from `persist.unseal` is the keystore implementation's `Display` — KMS errors historically carry connection strings, account IDs, or HSM serial numbers depending on backend).

Each is the SAME shape as the pre-`022f778a` preflight call: a free-text string from a layer below crosses the wire-message field and reaches an `AdminRole::ReadOnly` bearer through `GET /admin/sandboxes/{id}/wake/{wake_id}`.

The `SubmitRestoreError::Preflight { which, path, source }` + `log_detail` pattern is the reference shape: structured fields carry semantic content (`which` is operator-facing, `source` is tracing-only), `Display` is path/secret-free. Same shape should apply to:

- `LivezTimeoutDetail { agent_url_redacted_host: String, elapsed_ms: u64, last_status: Option<u16> }` — agent URL host can leak internal IPs (IPv4 strip lives in `sanitize_error_message:769-1015` but the URL embedding bypasses it because the wire-message field isn't routed through sanitize for `LivezTimeout` today — the sanitize call at `wake_machine.rs:165, 592` runs on the `RestoreHandlerError`'s `Display`, but `WakeErrorCode::LivezTimeout` doesn't route through `RestoreHandlerError` — it short-circuits via `rollback_with(_, _, code, e)`).
- `RegisterFailedDetail { phase: RegisterPhase, cause: RegisterCause }` — `register_restored` failure today carries pg connection strings if the registry DB is briefly unreachable.
- `ClockResyncFailedDetail { agent_url_redacted_host: String, vm_delta_secs: i64 }` — agent URL leak same as LivezTimeout.
- `InternalUnsealDetail { kms_op: &'static str }` — keystore implementations carry account IDs, HSM serials, AWS region strings.

**Architectural lift**: the four arms above share a closure shape — operator-actionable phase name + tracing-only structured detail. Rather than four ad-hoc enum variants on `RestoreHandlerError`, the right shape is **one trait `BackendFailureDetail` with `fn log_detail(&self, sandbox_id_typed: &str)` + `fn wire_summary(&self) -> String`**, and each variant a small struct implementing it. That generalizes the preflight pattern without forcing every site to mint its own enum.

**Severity**: CRITICAL. R25-S1 closed ONE leak vector; the same vector is structurally present on three other phases. The Nomad agent /livez timeout is high-frequency (1/12 stress-r3 wakes hit it); register_failed will fire any time the registry pg is degraded. Both are operator-visible-via-RO TODAY and carry the same path/IP/connection-string leak risk preflight had.

**Fix shape**: `BackendFailureDetail` trait, four impls, ~150-200 LOC. Each wake-phase rollback edge moves from `rollback_with(_, _, code, e: String)` to `rollback_with_detail(_, _, code, detail: Box<dyn BackendFailureDetail>)`. The wire-message string falls out of `detail.wire_summary()`; tracing.warn falls out of `detail.log_detail(typed_id)`.

---

## IMPORTANT

### [r26-A2] r3-A node-affinity KILLS "Nomad reschedules around a wedged worker" — and other architectural assumptions

The r3-A `Constraints` block (in flight at `a27a3b3fee12bd12f`) pins every alloc to the controller's local Nomad node ID. **This is correct** for the cross-node-staging race. But it has cascade effects that need acknowledgement:

**Affordances broken by per-sandbox node-pinning**:

1. **Nomad-driven failover dies.** Pre-r3-A: if worker-N goes wedged mid-CREATE (kernel hang, network partition, disk failure), Nomad's rescheduler could pick worker-M and the alloc would land somewhere. Post-r3-A: the constraint pins worker-N; rescheduler has nowhere to go; the alloc fails until the controller restarts and re-probes /v1/agent/self against a different worker.
2. **Horizontal scaling on a single sandbox.** Pre-r3-A: a hot sandbox could (theoretically) be migrated by re-submitting against a different controller. Post-r3-A: every alloc is pinned to the controller-that-staged-the-image. Sandbox migration requires either (a) bytes-level transfer of `workspace.img` to the new controller before re-submit, or (b) shared storage.
3. **Bin-packing efficiency.** Pre-r3-A: Nomad's bin-packer could place an alloc on the least-loaded worker. Post-r3-A: workers are bin-packed independently by their local controller; cross-controller load imbalance is invisible to the Nomad scheduler. Workers running heavy CREATE workload starve their controller of bandwidth; idle workers under a quiet controller can't be borrowed.
4. **Worker-evacuation procedures.** Pre-r3-A: drain a worker by setting Nomad node `eligibility=ineligible`; allocs reschedule away. Post-r3-A: setting eligibility=ineligible **strands every sandbox whose controller is on a different node**. Evacuation requires controller-level coordination (stop staging on that node) — outside Nomad's scope.

**Architectural lift**: the controller-stages-locally architecture is now **coupled** to the placement decision. Pre-r3-A the coupling was implicit (the bug surfaced the dependency); post-r3-A it's explicit (the Constraints block names it). The follow-on question: **does the controller-per-worker topology survive at scale?**

- At WORKER_COUNT=3: every controller is a SPOF for its workers' sandboxes. A controller crash strands every running sandbox until the controller restarts (workers are healthy; their allocs are still running; control-plane reachability for state transitions is lost).
- At WORKER_COUNT=100: 100 controllers each managing 1 worker. Operator burden scales linearly. Spelling it differently: **node-affinity reduces the controller from "one of N coordinators" to "the worker's twin"**. The architectural shape has shifted from "centralized controller dispatching to a worker pool" to "worker-pair-with-controller as the deployment unit".

**Decision needed (post-stress-r3)**: do we accept worker-pair-with-controller as the topology, or do we move to shared storage (Option 2 — restores the controller-pool model) or driver-side staging (Option 3 — eliminates the staging-locality coupling entirely)?

**Severity**: IMPORTANT. r3-A unblocks stress GREEN; the topology question doesn't block cutover. But the cutover commit message and the ADR-to-be at `docs/decisions/2026-05-25-node-affinity-placement.md` (DOES NOT YET EXIST — should land with r3-A) need to name the trade explicitly so the post-cutover scaling thread inherits a known position rather than a surprise.

### [r26-A3] kernel-state inventory ADR is missing the "deferred alternatives" section — Option 2 (shared storage) and Option 3 (driver-side staging) need codification

The `2026-05-25-kernel-state-surface-inventory.md` ADR commits to per-surface-sweeper as the pattern. Reasonable choice. But it **does not name the alternatives it rejects**, and r3-A introduces TWO new architectural alternatives that should be added as deferred sections:

**Option 2 — Shared storage backing for `workspace.img`**:
- Move `host_dir` from controller-local fs to a shared volume (NFS, S3FS, Ceph, GCS-via-FUSE).
- Eliminates the staging-locality coupling entirely.
- Eliminates the host_dir leak class (sweeper still useful but on shared storage, not per-worker).
- Eliminates 4 of 5 OPEN kernel-state surfaces' driver-side participation (mount-ns becomes uniform across workers; jailer chroot becomes uniform; PID files… still per-worker).
- Cost: shared-storage latency tax on every CREATE (snapshot write is ~14 s today over local fs; over NFS likely 30-60 s; over S3FS substantially more).
- Status: **deferred — needs a separate ADR proposal**.

**Option 3 — Driver-side staging**:
- Controller emits a typed `StagingManifest` (the r24-A1 schema half) describing what disks should exist; driver materializes them in `StartTask` from a shared blob store.
- Eliminates the controller-stages-locally premise entirely (controller is stateless w.r.t. workspace.img).
- Eliminates node-affinity (Constraints block goes away — driver materializes on whichever worker is picked).
- Five surfaces (cgroup, mount-ns, vsock CID, jailer chroot, PID files) stay driver-owned, which is where they already are.
- Cost: redesign the staging contract; the bootstrap path on driver runs first-time-CREATE-blob-fetch instead of first-time-CREATE-stat.
- Status: **deferred — depends on r24-A1 schema half completing AND a typed `StagingManifest`-on-the-wire ADR**.

The inventory ADR's Decision section is precise about per-surface vs unified-reaper. It should be **similarly precise about per-worker-controller vs shared-storage vs driver-side-staging**. Three surfaces (host_dir, mount-ns, jailer chroot) flip from "OPEN" to "N/A" under Option 2 or Option 3 — the inventory's status table is contingent on the staging-locality decision the ADR doesn't name.

**Severity**: IMPORTANT. r3-A locks in per-worker-controller without an architectural-alternatives ADR section. Future readers will infer "per-surface sweeper is forever" rather than "per-surface sweeper because we chose per-worker-controller; under shared-storage or driver-side-staging the surface list shrinks." The reasoning chain needs to be on the record.

**Fix shape**: add a `## Deferred alternatives` section to `2026-05-25-kernel-state-surface-inventory.md` (or a sibling ADR `2026-05-25-staging-locality-options.md`) enumerating Option 2 + Option 3 with the surface-collapse table per option.

### [r26-A4] Cutover gate is 5 items, not 3 — r24's count was optimistic

R24 cutover gate was: (1) wrapper retirement on r24-A2 table closure; (2) stress GREEN ≥95%; (3) playbook ADR landed. R25 added: (4) typed staging schema half (r25-A1). After stress-r3 + r3-A: the gate now has 5+ items, several of which compound rather than parallel.

**Current cutover gate (post-r3-A)**:

| # | Gate | Source | Status |
|---|------|--------|--------|
| 1 | ≥95% stress GREEN over 60 cycles | playbook ADR (`2026-05-25-restore-debug-playbook.md`) stress-as-gate rule | RED 1/60 |
| 2 | Zero stranded host_dir post-1h grace | inventory ADR Section 4 | NOT MEASURED (sweeper exists at `e82bffd7`, exposure missing) |
| 3 | Zero stranded taps post-cycle | inventory ADR Section 3 | NOT MEASURED (`taps_orphaned_total` driver-internal, no /metrics) |
| 4 | ≥4 of 5 OPEN kernel surfaces CLOSED | inventory ADR cutover-gate section | 0 of 5 closed (cgroup, mount-ns, vsock CID, jailer chroot, PID files all OPEN) |
| 5 | r3-A node-affinity landed | this review | IN FLIGHT |
| 6 | r3-A architectural-trade ADR (per r26-A2) | this review | NOT WRITTEN |
| 7 | Wire-pattern reuse for livez/register/clock_resync (r26-A1) | this review | NOT STARTED |
| 8 | R20-S3 driver SHA256 verify in `gcp-worker-startup.sh` | r20 security carry | NOT VERIFIED |
| 9 | fsync_dir doc-comment fix (r25-A3) | r25 minor | OPEN |

**That's 9 items**, with #1 and #4 each a meta-gate (compound). r24's "3 items" framing was three rounds of optimism ago.

**Restructuring proposal**: split the gate into **Tier-1 (functional)** and **Tier-2 (hygiene)**:

- **Tier-1 (cutover-blockers)**: 1, 4, 5. These are non-negotiable.
- **Tier-2 (post-cutover-must-close)**: 2, 3, 6, 7, 8, 9. Land separately; cutover commit cites the backlog tickets.

A 9-item AND gate is statistically unlikely to close in any near-term horizon. Splitting it admits which items genuinely block the one-way migration and which are dev-debt that should not.

**Severity**: IMPORTANT — meta-architectural (gate-design). r24's gate framing assumed three independent items would close in parallel; reality has been three serial cycles where each fix surfaces the next bug. The Tier-1/Tier-2 split makes the gate testable.

---

## MINOR

### [r26-A5] Three-for-three-for-four pattern: playbook ADR needs r3-A retro + Tier-0 (Placement)

The restore-debug-playbook ADR (`28fa64d1`) captures three force-multiplier retros: r22-A2 (CH-internal → driver staging via stderr-tail), r23-A2 (smoke GREEN → stress REGRESSION on two new bugs), r24-A1 (fsync_dir → 33% race-win rate). Stress-r3's 78%-cross-node-placement-rate observation IS the fourth instance, and it has the same shape:

> A verbatim layer-N observable resolves a multi-cycle misclassification when captured BEFORE adding defense-in-depth at layer N+1.

The r3-A diagnostic chain: stress-r3 CREATE failures (47/60) → controller's `assert_disk_image_present` was running on a worker without `workspace.img` → the controller had staged on a DIFFERENT worker → 78% mismatch rate at WORKER_COUNT=3 (a uniformly-random scheduler would pick the staging worker 33% of the time; 22% land-rate ≈ 67% miss-rate ≈ predicted by N=3 random).

**The rate datum (22% CREATE OK) was the diagnostic, not any verbatim string.** This parallels r24-A1's 33% race-win observation — the rate signal contained more information than the verbatim error string. The playbook's "verbatim observable" rule should extend to "verbatim observable OR rate signal that the hypothesis can falsify". The r24-A1 retro already says this implicitly (`"33% retry-wins is incompatible with 'the file is missing'"`); r3-A makes the same shape explicit again.

**Additional ADR Section 2 update**: the triage tier ladder has 5 tiers starting at Validator (controller refused before submit). r3-A is **upstream of Validator** — a placement decision Nomad makes after the controller submitted-and-accepted. Either Tier 1 should be renamed "Submit/Placement" or a Tier 0 "Placement" should be added above Validator. The 5-tier ladder doesn't cover the case where the validator passes but the scheduler hands the alloc to a different node from the one staging was done on.

**Severity**: MINOR — documentation, no code. Add r3-A retro to Section 5; consider Tier-0 in Section 2.

### [r26-A6] r25-A3 fsync_dir doc-comment lie — STILL OPEN, now compounding with r3-A diagnosis

`nomad_ch.rs:3698-3706` still cites stress-r1 Bug 1's 49/60 motivation for the fsync_dir hygiene. r25 flagged that motivation was refuted at stress-r2 (cleanup-vs-retry race, not visibility). Stress-r3 now adds a third refutation: it was actually cross-node-placement (78% of "missing workspace.img" cases were a different worker stat'ing a path that legitimately doesn't exist there). The fsync_dir code is harmless-but-quintuple-redundant; the doc-comment is now misleading on three orthogonal axes.

**Severity**: MINOR — readable code in misleading doc-comment. Pre-launch no-back-compat policy says rewrite, not deprecate.

---

## Cross-lens consensus

- **security r26**: r26-A1 trait pattern + livez/register/clock_resync detail-types is security-lens territory; converge on `BackendFailureDetail` trait reuse not regex-expansion of `sanitize_error_message`. The fact that `WakeErrorCode::LivezTimeout` short-circuits past `sanitize_error_message` is a secondary fix (route LivezTimeout's error through the sanitize path even before the trait pattern lands).
- **code-quality r26**: r26-A1 trait + four impls bundle. ~150-200 LOC. Sibling to r25-A1's preflight closure.
- **api-surface r26**: wire-codes already extended at `79871194`; no new codes needed for r26-A1 (LivezTimeout / RegisterFailed / ClockResyncFailed already have wire codes, the change is wire-MESSAGE-shape not wire-CODE-shape).
- **cluster T-8b-stress-r4**: gated on r3-A landing. Stress-r4 will test 78%-cross-node prediction's falsifiability (single-worker-fleet run should hit ≥95% by construction; 3-worker should hit ≥95% once Constraints block lands; cross-node-placement rate metric should read 0% post-r3-A).
- **test-coverage r26**: r26-A1 trait pattern adds a new contract dimension — `BackendFailureDetail::wire_summary` must be path/IP/secret-free for every impl, pinned by trait-level property test.
- **concurrency r26**: r26-A2 topology decision (per-worker-controller vs shared-storage) is concurrency-lens territory; the controller-as-twin shape changes the lock-contention model.

---

## Lens hand-off (priority-ordered)

1. **r26-A1 (P0)**: `BackendFailureDetail` trait + four impls. Closes the wire-message structured-error gap on livez/register/clock_resync/internal-unseal. ~150-200 LOC.
2. **r26-A4 (P0)**: split cutover gate into Tier-1 (3 items, hard blockers) + Tier-2 (6 items, post-cutover backlog). Update both ADRs.
3. **r26-A3 (P1)**: deferred-alternatives section in kernel-state-inventory ADR. Names Option 2 + Option 3.
4. **r26-A2 (P1)**: r3-A architectural-trade ADR `2026-05-25-node-affinity-placement.md`. Should land with the r3-A code commit.
5. **r26-A5 (P2)**: r3-A retro in playbook ADR Section 5. Tier-0 (Placement) in Section 2.
6. **r26-A6 (P3)**: fsync_dir doc-comment rewrite.
7. **T-8b-stress-r4 (next cluster)**: falsification criteria: ≥95% e2e at 3-worker, 0% cross-node-placement-rate metric, zero stranded host_dir post-1h, zero stranded taps, R25-S1 stays closed.

---

## Carry status

| Finding | r26 status |
|---|---|
| r19-A1 leak ledger | **ABSORBED into kernel-state inventory ADR section 8** — demotion + rebalance committed in ADR, not as separate counter-removal PR |
| r20-A1 wrapper retirement | **STILL RE-BLOCKED** — gate is now inventory-ADR's 4-of-5-OPEN-CLOSED |
| r22-A1 driver/wrapper contract divergence | r26-A1 opens FOURTH dimension (livez/register/clock_resync detail-types) |
| r23-A2 wrapper retirement structurally unblocked | **STILL RE-BLOCKED** — stress-r3 RED confirms |
| r23-A3 / r24-A3 ADR | **CLOSED at `28fa64d1`** |
| r24-A1 typed StagingManifest | **CLOSED for preflight at `022f778a`**; SCHEMA HALF still partial (preflight covers workspace.img + user_home.img only; cold-boot disks[] + restore disks[] still loose) |
| r24-A2 kernel-state surface enumeration | **CLOSED as ADR at `3e853cc6`**; 2 of 7 SURFACES closed; 5 OPEN |
| r24-A4 SnapshotRowMeta DRY | OPEN, bundle into next code-quality PR |
| r25-A1 typed-variant schema half | **CLOSED at `022f778a`**. Pattern reusable per r26-A1. |
| r25-A2 sweeper-pattern ADR | **CLOSED at `3e853cc6`** |
| r25-A3 fsync_dir doc-comment lie | OPEN (r26-A6) |
| r25-A4 restore-debug-playbook ADR | **CLOSED at `28fa64d1`** |
| r25-A5 leak ledger demotion | absorbed into ADR section 8 |
| r25-A6 host_dir counter symmetry | ADR section 8 names the pair; counter implementation deferred |
| r26-A1 wire-message structured-error pattern | **NEW** (P0) |
| r26-A2 r3-A topology trade | **NEW** (P1) |
| r26-A3 deferred-alternatives ADR section | **NEW** (P1) |
| r26-A4 cutover gate Tier-1/Tier-2 split | **NEW** (P0) |
| r26-A5 playbook ADR r3-A retro + Tier-0 | **NEW** (P2) |
| r26-A6 fsync_dir doc-comment | **NEW** (was r25-A3 minor) (P3) |
| T1 sandbox_admin_ro role | CLOSED. The r25-A1-predicted compose-leak is now CLOSED at `022f778a`. |
| r3-A node-affinity constraint | IN FLIGHT (`a27a3b3fee12bd12f`) |
| r3-B driver tap-poll | LANDED at `05440498` |
| r3-C two-pass DriverFailure preference | LANDED at `3d431eb8` |

---

## Final note

The r25-r26 window landed three ADRs (playbook + kernel-state inventory + retired-ledger absorption) and one critical schema fix (R25-S1 typed staging). That's substantial structural-debt repayment in one cycle — more than the prior 4 cycles combined.

But stress-r3 went RED 1/60 because **none of the four landed fixes addresses the dominant failure (78% cross-node-placement rate)**. The r3-A node-affinity constraint is the load-bearing fix; it's still in flight. Stress-r4 is the falsification step.

**The playbook is 4-for-4 on force-multiplier evidence.** Every stress cycle since adopting "verbatim observability before architecture" has reclassified its dominant failure mode in a single cycle once the right observable was captured (stderr-tail → driver staging; verbatim driver-msg → controller staging; 33% retry-win rate → cleanup race; 22% CREATE OK rate → cross-node placement). The ADR shipped at `28fa64d1` is now the playbook of record; r3-A is its fourth force-multiplier; r26-A5 codifies the entry.

The decision items r26 leaves on the next reviewer's desk:

1. Does the controller-per-worker topology survive at scale? (r26-A2 + r26-A3)
2. Is the 9-item cutover gate the actual gate, or do we admit a Tier-1/Tier-2 split? (r26-A4)
3. Does `BackendFailureDetail` close the wire-message-structured-error class once and for all, or is preflight a special case? (r26-A1)

Answers wait for stress-r4 empirical data + the r3-A architectural-trade ADR draft.
