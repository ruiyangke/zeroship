# ADR — Restore-Debug Playbook: Observability-Before-Architecture, Tiered Triage, Stress-as-Gate

- **Date:** 2026-05-25
- **Status:** Accepted
- **References:**
  - Architecture reviews r22, r23, r24 (`r24-A3`), r25 (`r25-A4`) under `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r{22..25}.md`
  - Cluster reviews `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r{15..23}.md`,
    `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-{r1,r2}.md`
  - Force-multiplier commits: `6efdc42e` (C-7-LT-11 stderr-tail on resume failure), `8b366b6d` (driver v11 pin),
    `30960451` (v33 controller-side disk preflight + fsync_dir), `d638b10f` / `e82bffd7` (v34 host_dir leak +
    sweeper), `177ff165` (driver v14 defensive tap cleanup), `7fd661c9` / `50fb987d` / `7a85ed7b` (C-7-LT-12a
    rootfs-staging path)

## Context

Between smoke-r15 and stress-r2, the sandbox snapshot-restore branch ran eight cluster cycles under a
"patch first, observe later" pattern. Each cycle hypothesised a failure layer and added defensive logic
there; the next cycle's verbatim observable refuted multiple of the prior hypotheses. The pattern was
expensive: every refutation cost one cluster slot (~$0.20 + driver/controller rebuild + image upload +
30-60 min wall-clock) and left technical debt at the wrong layer.

Three closures broke the pattern and now stand as evidence that the alternative discipline works:

1. **smoke-r22 / C-7-LT-11**: `DeviceManager(Disk(NotFound))` was classified as a "CH-internal" failure
   class for three cycles. The fix at `6efdc42e` added ~5 LOC of stderr-tail capture on resume-failure in
   `nomad-driver-ch/restore_task`. One cycle later, smoke-r22 (`docs/reviews/...T8b-smoke-r22.md`)
   reclassified the failure from "CH-internal" to "driver staging" — the verbatim stderr revealed that
   the restore alloc never received `rootfs.img` in its runDir. Fix landed as the C-7-LT-12a chain
   (`7a85ed7b` + `50fb987d` + `7fd661c9`).
2. **stress-r1**: The driver emitted `disk[1] /var/zeroship/ch/<id>/workspace.img does not exist
   (controller must stage before spawn)` verbatim in `TaskEvent.DisplayMessage`. That string
   reclassified 49/60 cold-boot failures from a generic "Failed tasks" rollup to a controller-staging
   gap in one cycle. Fix landed at v33 (`30960451`).
3. **stress-r2**: After v33 shipped fsync_dir + `assert_disk_image_present` defense-in-depth, stress-r2
   stayed RED at the same 2/60 OK rate. The decisive datum was the **33% retry-win observation**: of
   144 alloc submissions (48 CREATEs × 3 retries), 96 driver-preflight rejections matched 33% race-wins,
   not the 100% rejection a deterministic visibility bug would produce. d638b10f's commit message
   diagnoses the actual mechanism: `CreateGuard::drop` step 3 was `rm -rf`ing host_dir while a
   concurrent retry's `StartTask` was still reading `workspace.img`. The reclassification from "staging
   visibility" to "cleanup-vs-retry race" happened in one cycle once the rate-data was treated as the
   layer-N observable.

Each case shows the same shape: a verbatim layer-N observable resolves multi-cycle misclassification
when captured **before** adding defense-in-depth at layer N+1.

This ADR codifies the discipline so future cycles do not relearn the lesson.

## Decision

Before patching at layer N+1, capture layer N's verbatim error first. Capture is cheap (~5 LOC per
layer of stderr-tail / TaskEvent extraction / rate-counter exposure); the cycles saved per refuted
hypothesis are not.

### Triage tier ladder (1 diagnostic cycle per tier max)

| Tier | Name | Layer | Verbatim observable |
| --- | --- | --- | --- |
| 1 | **Validator** | Controller refused before submit | jobspec build error, auth refusal, capability check |
| 2 | **Wire** | Alloc reached driver, driver rejected before CH spawn | preflight stat result, config-rewrite reject, allow-list refusal |
| 3 | **CH input contract** | CH started but failed at config parse / disk validate | CH stderr at config-load / device-init |
| 4 | **CH internal** | CH started + parsed but failed at runtime | KVM init error, vCPU boot, DeviceManager runtime errors |
| 5 | **Agent boot** | CH OK but in-VM agent `/livez` never 200 | agent stdout, in-VM dmesg, `/livez` body |

**Rules:**

1. At each tier, capture the verbatim observable BEFORE proposing a fix at the tier above.
2. If the verbatim is unavailable at the tier, the only allowed change in that cycle is an
   observability hook for that tier — do not also speculate on the layer above.
3. One diagnostic cycle is the budget per tier. If the verbatim is captured and the layer is still
   unclassified after one cycle, escalate to the next tier with the captured evidence in hand.
4. Verbatim observables flow into operator-readable surfaces (TaskEvent DisplayMessage, structured
   tracing fields, `wake_jobs.error_message`, controller logs). They do not stop at "the operator can
   SSH in and find it" — that's an unreachable escalation under the 60-cycle stress harness.

### Observability-before-architecture rule

A rejected hypothesis costs $0.20 + a cluster slot per refutation cycle. Defense-in-depth at the wrong
layer costs $0.20 × N cycles + technical debt at the wrong layer plus a misleading doc-comment that
points future readers at the wrong story (see fsync_dir's stale rationale in `nomad_ch.rs:3698-3706`
after stress-r2 refuted v33's diagnosis). Verbatim observability costs ~5 LOC (stderr-capture pattern,
TaskEvent walker, rate counter) and one cycle to capture.

The rule: a fix proposal at layer N+1 must cite the verbatim layer-N observable that motivates it.
"The symptom is consistent with X" is not a citation; the verbatim string or the rate datum is.

### Stress-as-gate rule

Smoke (1 cycle) validates state-space traversal. Stress (60 cycles, vm_index reuse, sequential CREATE
+ SNAPSHOT + WAKE + STOP) validates state-space transitions under contention and resource reuse. NEVER
declare a structural unblock on smoke alone.

The r23-A2 claim "wrapper retirement structurally unblocked pending smoke-r23 GREEN" was retired the
cycle after it landed: smoke-r23 went GREEN at N=1, stress-r1 went RED at 2/60 with two new bug
classes (workspace.img staging window, tap teardown leak). Both regressions were invisible at N=1
because they only exercise under retry / vm_index reuse / concurrent CREATE.

Cutover-class decisions (wrapper retirement, `SANDBOX_TASK_DRIVER=ch_plugin` default flip,
sweeper-owned cleanup) require **stress GREEN, not smoke GREEN**. Stress is the falsification step.

## Force-multiplier retrospectives

### r22-A2 retro — "CH-internal" classification REFUTED by stderr capture (C-7-LT-11)

Pre-fix: smoke-r19 through smoke-r21 classified `DeviceManager(Disk(NotFound))` as a CH-internal
failure. The hypothesis chain accreted: bad rootfs build, wrong virtio-blk feature flags, snapshot
format incompatibility. None were verifiable because the driver swallowed CH stderr.

Fix at `6efdc42e` (driver v11, ~5 LOC): capture CH stderr on resume-failure and surface it via the
TaskEvent DisplayMessage. One cycle later, smoke-r22 saw the verbatim CH error and reclassified the
failure to driver-side rootfs-staging gap. The fix (C-7-LT-12a) landed as a driver-side hardlink stager
plus a controller-side `rootfs_source` Config field, NOT as a CH rebuild.

Lesson: 5 LOC of stderr capture closed three cycles of CH-internal speculation.

### r23-A2 retro — "wrapper retirement structurally unblocked" REFUTED by stress

Pre-stress: r23-A2 forecast that smoke-r23 GREEN would unblock wrapper retirement (the r20-A1 cutover
gate). Smoke-r23 went GREEN at N=1 (first end-to-end OK in 23 cycles). r23-A2 declared structural
equivalence between wrapper-mode and ch_plugin-mode.

Stress-r1 (the cycle after r23-A2 landed) went RED at 2/60 OK rate with **two new bug classes**:

- Bug 1: `workspace.img` staging-window race, 49/60 CREATE failures.
- Bug 2: tap interfaces leak across cycles — 9 stranded `zsbx-nm-<idx>` interfaces per worker post-run,
  because driver `DestroyTask` either races or misses the cleanup when `h.tap` is empty.

Neither bug exercised at N=1 because neither requires retry / vm_index reuse / concurrent CREATE.

Lesson: smoke validates the happy-path traversal. Stress validates the contract under reuse + retry.
"Structurally unblocked on smoke" is a category error — the contract being validated only ships
behaviour at N>1.

### r24-A1 retro — "fsync_dir visibility" REFUTED by 33% race-win rate

Pre-fix (v33): r24-A1 forecast that 30960451's fsync_dir + `assert_disk_image_present` would close
Bug 1. The hypothesis was visibility — the dirent was created by the controller but invisible to the
driver's stat call across the staging/submit window. r24-A1 explicitly flagged this as
"fsync-then-pray" — fixing for a mechanism not yet proven.

Stress-r2 ran on v33 and stayed RED at the same 2/60 rate. The decisive observable was a rate datum,
not a string: 96 driver-preflight rejections out of 144 alloc submissions = 33%. A deterministic
visibility bug would reject 100% of retries; 33% means 2/3 of retries DID win the race. The mechanism
isn't visibility — it's a cleanup window.

d638b10f's commit traces the actual race: `CreateGuard::drop` step 3 called `spawn_blocking(remove_dir_all(host_dir))`
on alloc failure; a concurrent retry's `StartTask` would observe a half-emptied host_dir mid-preflight.
The structural fix in v34 (`d638b10f` + `e82bffd7`) leaks host_dir on stop and reaps it from a 5-minute
GC sweeper with a 1-hour mtime grace.

Lesson: when an additive fix doesn't move the metric, look for a rate signal that the hypothesis can
falsify. 33% retry-wins is incompatible with "the file is missing"; it is compatible with "the file
disappears partway through preflight."

### Stress-as-gate vindication

All three retros converge on the same meta-rule. Single-cycle smoke is necessary but not sufficient.
The cluster cycle budget per stress run is high; that's the cost of buying the contract validation
that smoke cannot provide. Roll-forward decisions wait for stress.

## Consequences

- **Verbatim observability is the first investment, not the last.** When a new failure layer is
  suspected, the cycle's deliverable is the observability hook for that layer, not a speculative fix
  above it. The `inc_*` counter pattern in `crates/sandbox/src/metrics.rs` and the stderr-tail pattern
  in `nomad-driver-ch/restore_task` are the templates.
- **Wire envelope carries the verbatim cause.** `backend_create_failed` / `restore_backend_failed` MUST
  thread the driver's verbatim failure string to `wake_jobs.error_message` (see `extract_failed_task_event_msgs`
  at `nomad_ch.rs:2756`, landed in `d638b10f`). Operators see the same string the verbatim observable
  carries.
- **Doc-comments that cite a refuted diagnosis are bugs.** The fsync_dir doc at `nomad_ch.rs:3698-3706`
  still cites Bug 1's 49/60 motivation after stress-r2 refuted that motivation; the code is
  harmless-if-redundant but the doc lies. Code review must catch this; pre-launch no-back-compat policy
  means the fix is "rewrite the doc to cite the actual closed mechanism" not "add a deprecation note."
- **Stress GREEN is the cutover gate, not smoke GREEN.** Wrapper retirement (r20-A1),
  `SANDBOX_TASK_DRIVER=ch_plugin` default flip (T-8b-cutover), and any other one-way migration require
  stress GREEN at ≥95% end-to-end OK over a 60-cycle run AND zero stranded kernel-state surfaces post-run.
- **Sibling ADR**: kernel-state surface inventory at
  `docs/decisions/2026-05-25-kernel-state-surface-inventory.md` captures the per-surface closure gate
  this ADR's stress-as-gate rule depends on. The two ADRs ship together.

## Cross-references

- `r22-A2` retro: deferred backlog entry under "C-7-LT-11" / "C-7-LT-12 / C-7-LT-12a" (CLOSED rows at
  `6efdc42e`, `7a85ed7b`, `50fb987d`, `7fd661c9`).
- `r23-A2` retro: r24-A2 carry in `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r24.md`
  (wrapper retirement RE-BLOCKED).
- `r24-A1` retro: r25-A1 in `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r25.md`
  (typed-variant schema half open; observability half landed at d638b10f).
- Stress-as-gate rule: r25-A4 in the same review file; `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r2.md`.
- Commits cited verbatim: `6efdc42e`, `8b366b6d` (driver v11 pin), `30960451` (v33), `364ead22` (v33
  pin bump), `d638b10f` (v34 leak + verbatim msg), `e82bffd7` (v34 sweeper), `c729c2b8` (v34 pin
  bump), `177ff165` (driver v14 defensive tap cleanup), `7fd661c9` / `50fb987d` / `7a85ed7b`
  (C-7-LT-12a chain).
