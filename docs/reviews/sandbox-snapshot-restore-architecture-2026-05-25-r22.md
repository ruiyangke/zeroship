# Sandbox snapshot-restore architecture review — 2026-05-25 r22

**Reviewer**: architecture-r22 (post-smoke-r20 + driver v9 cycle)
**HEAD**: `ef11edb3` (`feat/sandbox-snapshot-restore`, smoke-r20 review landed)
**Predecessor**: r21 at `4d73a5d1`. Diff since r21: r21-A1 fix
(`fcac5355`), controller-pin v30→v31 (`af80aa0b`), smoke-r19 review
(`c2e4a6e0`), round-28 reviewer artifacts (`8bc11768`), R20-C1 SQL
guard (`ccb2abc8`), round-29 reviewer artifacts (`afa5da96`),
driver-pin v8→v9 (`f2641e88`), smoke-r20 review (`ef11edb3`).
**Lens**: architecture (READ-ONLY).

## Summary

The chain has shifted decisively from "validator/staging-coordination
tuning" to **actual VM-startup pathology**. r21-A1 closed the last
wire-format gap and the validator class is empirically closed for two
cycles. The new C-7-LT-10 defect surfaced in smoke-r20 is **not a
controller-side bug at all** — it is a Go-driver design choice
(materialise rewritten config in `runDir`, pass `RestoreFrom` to CH)
that strands the rewriter output in a directory CH never reads. **The
bash wrapper got this right by accident** (rewrites in-place at
`$ZSBX_RESTORE_FROM`, line 678 then points CH at the same dir) and
the Go driver explicitly chose the opposite at `restore_task.go:325-
329` for read-only-safety reasons that turned out to be wrong about
what CH actually consumes.

The r20-A1 three-rewriter coexistence finding is now a **hard cutover
blocker** (escalated to CRITICAL): the wrapper was masking a CH
input-path contract the Go driver violates; retiring the wrapper
before C-7-LT-10 lands would leave the system with NO working restore
path. r19-A1 leak ledger holds (five cycles fence_passed=true, eight
cycles leak counter = 0) and remains deferred. Five findings
(1 CRITICAL, 2 IMPORTANT, 2 MINOR).

## CRITICAL

### [r22-A1] C-7-LT-10 reveals driver/wrapper restore-input contract divergence — wrapper retirement BLOCKED until driver passes `runDir` to CH (not `RestoreFrom`)

- **Where**:
  - **Wrapper (correct by accident)**: `nomad-vm-wrapper.sh:474, 678`
    — writes rewritten `config.json` in-place at
    `$ZSBX_RESTORE_FROM/config.json` (atomic-rename via tempfile l.630),
    then `cloud-hypervisor --restore source_url=file://$ZSBX_RESTORE_FROM`.
    Single directory: CH reads what wrapper wrote.
  - **Go driver (broken by design)**: `ch/restore_task.go:319` writes
    rewritten config to `<runDir>/config.json`; `:420` passes
    `--restore source_url=file://<RestoreFrom>` to CH. **Two
    directories**: CH reads the un-rewritten snapshot config from
    RestoreFrom and ignores the rewrite. Documented at l.325-329 with
    the explicit reasoning "we DO NOT [rewrite in-place] because (a)
    the source dir is potentially read-only and (b) re-wakes of the
    same snapshot should each see a pristine source." The reasoning
    is correct about the design constraint but wrong about CH's
    input path — CH consumes config.json from `source_url`, not from
    runDir.
- **Why this re-classifies r20-A1**: r21 had r20-A1 (three-rewriter
  coexistence) as IMPORTANT/process-debt. r20's source-level
  confirmation that the **driver and wrapper disagree on a
  load-bearing CH contract** elevates this to CRITICAL/cutover
  blocker. Retire wrapper today → restore path is dead at CH-spawn
  (driver-only path is what smoke-r20 just demonstrated). The
  three-rewriter coexistence was masking exactly this contract drift.
- **Architectural framing of C-7-LT-10**: this is the **first defect
  in the entire 20-cycle chain that is NOT a staging-coordination
  bug** — it is a CH input-contract bug. The pattern shift is real:
  r14–r17 were validator tuning; r18 was wire-format coverage; r19
  was missing-file pathology in driver-side staging; **r20 is the
  first defect rooted in "what does CH actually read on `--restore`"
  with file:line proof.** The next failure layer (if r21 RED) will
  be inside CH's restore flow proper (livez_polling onward), not in
  the driver's staging layer.
- **Fix shape (mirrors smoke-r20 Recommendation Shape A)**: in
  `ch/restore_task.go`, after writing rewritten `config.json` to
  `<runDir>`, also symlink `state.json` + `memory-ranges` from
  `RestoreFrom` into `runDir`, then change l.420 to
  `restoreURL := "source_url=file://" + runDir`. Pre-create loop
  already targets correct paths. Wire change ~5 lines + one test
  pin `TestStartTaskRestoreBranch_PassesRunDirToCH`.
- **Severity**: CRITICAL — single-line architectural blocker for
  T-8b-cutover. Wrapper retirement (r20-A1) cannot proceed until the
  driver matches the wrapper's CH-input contract.

## IMPORTANT

### [r22-A2] Pattern shift after 20 cycles — staging layer is empirically closed; next failure class will be CH-internal or livez-polling

- **Where**: synthesis across smoke-r14 through smoke-r20 (8 cycles).
  Staging-layer signals that have been stable for ≥5 cycles:
  - `fence_passed=true probes=2 consecutive_misses=2 elapsed_ms=300`
    — 7 cycles (C-7-LT-2).
  - `vm_index leak counter = 0` — 7 cycles (R12-IMPL-2).
  - `vm_index reserve retry resolves on attempt 17` — 5 cycles
    (C-8c at this cluster size).
  - `stop_preserving_state` log lines firing between attempts
    15-17 — 5 cycles.
- **What I see**: every defect in r14–r20 has been at progressively
  deeper layers along the same call chain:
  - r14: fence stabilised (staging).
  - r15: rewriter task-dir invariant (staging).
  - r16: per-sandbox allow-list prefix (staging).
  - r17: per-user-home allow-list prefix (staging).
  - r18: controller wire-format omission (staging boundary).
  - r19: pre-create runtime files (staging boundary).
  - r20: **driver/CH input-path contract** (CH boundary — NEW class).
  The depth axis is monotonic. r21 will exercise — for the first
  time — code *inside* CH's restore flow. R19-I1 two-phase livez
  probe will fire for the first time. The architectural posture has
  shifted from "controller-driver staging" to "CH-internal +
  in-guest agent boot".
- **Why this matters for cycle planning**: triage discipline now
  needs to include **CH source-audit** (configuration, KVM,
  console/serial, virtio-fs) plus **agent boot variance**. The
  smoke-r20 review correctly identifies this with the "source-audit
  before patching" rule. Architecture endorses: post-LT-10 cycles
  should pre-budget a CH-source-or-strace investigation lane.
- **Action**: add to `docs/decisions/2026-05-25-restore-debug-
  playbook.md` (NEW): triage tiers — staging (validator + wire) →
  CH input contract (config path, file paths) → CH internal
  (KVM/virtio errors) → agent boot. Each tier has a 1-cycle
  diagnostic budget; if not localised in 1 cycle, escalate to source-
  audit of the next tier's consumer.
- **Severity**: IMPORTANT — process-evolution finding; informs how
  r21+ cycles are scoped, no immediate code change.

### [r22-A3] r21-A1 fix landed cleanly but field-list contract test from API-surface r21 still on paper — close before cutover

- **Where**: `restore_handler.rs:2345-2364` post-`fcac5355` now emits
  `user_id`. r21-A1 closed at field-set-equality with cold-boot
  builder (deferred ledger l.1807-1809). But R21-API2 (api-surface
  r21, `afa5da96`) prescribes a contract test asserting field-set
  equality between `build_nomad_job_json_with` (cold-boot) and
  `build_restore_nomad_job_json` (restore) ChPlugin Config maps —
  ~30 LOC, no commit yet.
- **Why this still matters**: the C-7-LT-7 → r21-A1 root cause was
  precisely "two emitters, no shared schema, one updated, the other
  missed." Adding a third field (likely happens once C-7-LT-10
  lands — driver may want a new `RunDir` config knob) without the
  contract test repeats the exact failure. The deferred-ledger
  `r21-A1 CLOSED` entry explicitly calls this out: "A field-list
  contract test between the two builders would catch future
  divergences." The architectural learning from r20-A1 is not
  durably encoded until that test exists.
- **Action**: code-quality r22 or api-surface r22 to land the
  symmetric-set test (HashSet of field names from each builder; assert
  equal modulo `restore_from`). Cycle: <30 min.
- **Severity**: IMPORTANT — process-debt; latent regression risk on
  any future Config-field addition.

## MINOR

### [r22-A4] r19-A1 leak ledger + reaper — fifth cycle of fence_passed=true, seventh of leak counter = 0; deferral holds

- **Where**: `cleanup_orphans_at_startup` (nomad_ch.rs:431-464),
  three lying-comment sites at `:1081/:1162/:1184`, all unchanged.
  No `leak_reaper` / `leak_ledger` symbols in tree.
- **What I see**: counter has registered ZERO leak events across
  smoke-r14..r20 (8 cycles, 7 of which reached the teardown phase
  before failing on the wake-half). The deferral basis is stronger
  than at r21: fence_passed=true on r14, r15, r16, r17, r18, r19,
  r20 — and the leak-counter wiring has surfaced no signal. The
  only theoretical surface remaining (silent-release on a bad path)
  was audited at r21-A4 and remains clear.
- **Action**: hold deferral. When bandwidth opens post-cutover, ship
  per-process `detach_isolated` reaper FIRST (r20-A2 sketch); ledger
  waits for first measurable multi-process leak. Three lying-
  comment sites still need cleanup or implementation. **Demotion
  candidate**: if r21–r25 all show zero leaks AND C-7-LT-10 lands
  cleanly, fold this into the "remove dead leak-counter wiring"
  ADR rather than completing the reaper.
- **Severity**: MINOR (held from r21). Eight-cycle zero counter is
  now the leak ledger's own falsification.

### [r22-A5] R20-C1 SQL guard landed (`ccb2abc8`); r19-A3/r20-A3/r20-A4/r20-A5/r21-A5 still open as process debt

- **Where**: R20-C1 (terminal-overwrite SQL guard) closed at
  `ccb2abc8` with 2 pg-gated tests. Concurrency-lens debt drained.
  Remaining open: r19-A3 (per-phase `stop()` metric), r20-A3 (ADR
  bundle: wait_for_agent_silent + wait_for_job_gone + takeover),
  r20-A4 (C-7-LT-2 invariant + regression test), r20-A5 (R19-I1
  unverified-in-prod — deferred for the 7th cycle by smoke-r20),
  r21-A5 (re-route r20-A3 to code-quality r22).
- **What I see**: the code-quality r21 lens did NOT pick up the r20-
  A3 ADR bundle (r21-A5 confirmed). The re-route to code-quality
  r22 in r21's hand-off has been carried forward in r29 reviewer
  artifacts but not yet implemented. r20-A5 (R19-I1 verification)
  is structurally downstream of C-7-LT-10 — gated on the wake state
  machine ever reaching `livez_polling`, which has not happened in
  20 cycles.
- **Action**: when code-quality r22 fires, bundle r20-A3 + r21-A5
  into a single ADR. r20-A5 stays carried until smoke-r21+ exercises
  livez_polling (post-LT-10 fix). r19-A3 is the last metric-debt;
  recommend bundling it into the same ADR cycle.
- **Severity**: MINOR — process backlog; no runtime risk.

## Cross-lens consensus

- **smoke-r20 cluster review**: C-7-LT-10 root cause is structurally
  diagnosed via source audit (`restore_task.go:319, 360, 420`).
  Architecture endorses Shape A (repoint `source_url` to runDir +
  symlink state.json/memory-ranges) over Shape B (rewrite in-place
  at RestoreFrom). Shape A preserves the read-only-source-dir
  invariant the driver author was protecting and aligns with the
  one-input-directory contract the wrapper inadvertently established.
- **api-surface r21 (R21-API2)**: field-list contract test for the
  two ChPlugin Config emitters — r22-A3 reaffirms this as cutover-
  gating.
- **security r21 (R21-S1 + R21-S2)**: user_id and SandboxId
  unvalidated on restore boundary remain open. With C-7-LT-10 in
  flight, the driver-side `RunDir` may become another typed-id-
  derived path — same boundary, expand the validator-symmetry
  contract test scope.
- **concurrency r21**: R20-C1 closed at `ccb2abc8`. Lens drained.
- **perf r21**: WAKE wall-time projection (15-25s mixed post-fix)
  is contingent on C-7-LT-10 landing AND R19-I1 livez-polling
  exercising cleanly. Both still pending.

## Lens hand-off

1. **Implementer (driver-side, immediate)**: land C-7-LT-10 in
   `nomad-driver-ch/ch/restore_task.go` per smoke-r20 Shape A.
   Add `TestStartTaskRestoreBranch_PassesRunDirToCH` pin. Build
   driver v10. Cycle: <2 h.
2. **Implementer (controller-side, parallel)**: land R21-API2
   field-list parity contract test (r22-A3). ~30 LOC, no driver
   dependency. Cycle: <30 min.
3. **Cluster-smoke r21**: gated on C-7-LT-10 landing + driver v10
   upload + pin bump v9→v10. **Falsification criterion**: state
   machine reaches `livez_polling`. If it again fails at CH spawn,
   the next layer down is CH-internal (KVM, virtio-fs, console-
   device) — outside this codebase, requires CH source/strace.
4. **Architecture r23**: gated on smoke-r21 outcome. If GREEN, r23
   pivots to "post-restore stability + first-WAKE-OK cycle baseline"
   (R19-I1 verification, AEAD-decrypt cost characterisation). If RED
   with new CH-internal layer, r23 scopes the CH-source-audit
   playbook.
5. **Code-quality r22**: execute r20-A3 + r21-A5 ADR bundle.
6. **Wrapper retirement (r20-A1)**: blocked behind r22-A1; one full
   GREEN smoke cycle with `SANDBOX_TASK_DRIVER=ch_plugin` + wrapper
   script absent is the cutover prerequisite. Earliest plausible:
   smoke-r22 (one cycle past first GREEN).

## r19-A1..A5 + r20-A1..A5 + r21-A1..A5 carry status

| Finding | r22 status |
|---|---|
| r19-A1 (vm_index leak ledger + reaper) | **OPEN, MINOR** (r22-A4). 8-cycle zero counter; demotion candidate. |
| r19-A2 (wake_jobs takeover sweep) | CLOSED (r20). |
| r19-A3 (per-phase wall-time `stop()` metric) | OPEN — no commits since r19. |
| r19-A4 (`from_host_fence_timeout` ADR) | CLOSED (r20). |
| r19-A5 (ureq sites audit) | CLOSED for livez (R19-I1 v30); Nomad-side LOW, skipped. |
| **r20-A1** (three-rewriter coexistence) | **OPEN, ESCALATED to CRITICAL** (r22-A1). Source-confirmed contract divergence between wrapper + Go driver. |
| r20-A2 (r19-A1 carry, re-ranked) | OPEN → MINOR (r22-A4). |
| r20-A3 (ADR bundle: timing constants) | OPEN (r22-A5). |
| r20-A4 (C-7-LT-2 invariant + regression test) | OPEN — 7-cycle fence_passed=true; integration test still not landed. |
| r20-A5 (R19-I1 unverified-in-prod) | OPEN — gated on first `livez_polling` reach, 7th cycle deferred. |
| **r21-A1** (user_id emission on restore-path) | **CLOSED** (`fcac5355`); smoke-r20 confirmed validator passed (no allow-list error). |
| r21-A2 (user_id unvalidated on restore boundary) | OPEN — tracked via security r21 R21-S1. |
| r21-A3 (wrapper-retirement blocker ADR) | OPEN — r22-A1 promotes urgency. |
| r21-A4 (r19-A1 carry) | OPEN → r22-A4. |
| r21-A5 (code-quality r22 re-route for r20-A3) | OPEN → r22-A5. |
