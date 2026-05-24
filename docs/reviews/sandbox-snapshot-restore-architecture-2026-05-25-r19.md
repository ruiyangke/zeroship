# Sandbox snapshot-restore architecture review — 2026-05-25 r19

**Reviewer**: architecture-r19 (post-smoke-r13 retrospective lens)
**HEAD**: `87f40229` (`feat/sandbox-snapshot-restore`)
**Uncommitted on top**: `crates/sandbox/src/backend/nomad_ch.rs` (C-7-LT-2-PR1 compio-TCP probe), `crates/sandbox/src/metrics.rs` (PR2 leak counter).
**Lens**: architecture (READ-ONLY).

## Summary

Smoke-r13 (`docs/reviews/…T8b-smoke-r13.md`) revealed that 12 cluster
cycles of "tune the wake retry budget" (C-4 → C-7-LT-1) were reasoning
over a misread of the 60 s teardown wall-time. The constant was never
"agent dies + Nomad purge"; it was **`wait_for_job_gone(30 s) +
wait_for_agent_silent(30 s)` both timing out**, with `fence_passed=false`
**leaking the vm_index** on every cycle. r18-A1 (wake_jobs takeover
sweep) is still missing and now also load-bearing for the next layer of
the problem: once C-7-LT-2-PR1 lands the probe-wedge fix, healthy
teardowns will release slots — but pathological teardowns (genuinely
hung agent, kernel hangs SYN) will still leak, and we have **no
reclaim path for a leaked vm_index in a single controller run**.

C-7-LT-2-PR1 (compio-native TCP probe) is staged uncommitted at
`nomad_ch.rs:3220-3438`; PR2 (per-reason leak metrics) likewise. Neither
ships GREEN until r14 cluster validation. The deeper architectural gap
this review surfaces: **the platform documents a "leak then orphan-prune
will reclaim" recovery contract, but `cleanup_orphans_at_startup` only
purges Nomad jobs; it does NOT reclaim leaked vm_index slots in the
in-memory allocator**. Single-process leak persists until controller
restart, and even then is implicitly "reclaimed" by losing the
allocator state — which races a still-alive source agent.

6 findings (2 CRITICAL, 3 IMPORTANT, 1 MINOR) + 1 retrospective.

## CRITICAL

### [r19-A1] Leaked-vm_index recovery contract is a comment, not code — "orphan-prune will reclaim" reclaims nothing

- **Location**: `crates/sandbox/src/backend/nomad_ch.rs:1155` ("orphan-prune
  will reclaim on next boot"), `:1170` (same), `:3779` (same),
  `:1081` (long doc-comment promising the contract). The "reclaim"
  surface is `NomadCHBackend::cleanup_orphans_at_startup`
  (`nomad_ch.rs:431-464`), which only enumerates Nomad jobs by
  `prefix=zsbx-` and DELETE-purges them. **It never touches the
  `VmIndexAllocator`.**
- **What I see**: Two control paths interact pathologically:
  1. **Single-process leak**: a `host_fence_timeout` leaks slot N
     in-memory. `VmIndexAllocator::release(N)` is never called.
     Slot N is permanently unusable until process restart. A
     subsequent wake on the same sandbox replays the C-7-LT-1
     widened retry budget (70 s @ fence=30) against a leak — exactly
     the smoke-r13 WAKE 0/1 shape.
  2. **Restart "reclaim"**: on boot, `VmIndexAllocator::new(floor,
     ceil)` (`nomad_ch.rs:359-362`) considers all slots free.
     `restore_at_startup` (`restore.rs:97`) only `register_restored`s
     `running`/`unreachable` rows, calling `allocator.reserve(N)`
     for live VMs. A `Snapshotted` sandbox whose source-teardown
     leaked the index has **no row in the boot scan**, so slot N
     is implicitly "reclaimed" — but the underlying source VM
     (the one whose agent was still ACKing at fence deadline) may
     still be alive on the worker host. A fresh `create` handing
     slot N to a new tenant collides with that live agent on
     the same tap/IP. The FM-F race the host_fence was designed
     to prevent is re-opened by the supposed recovery path.
- **Why it matters**: the architectural model assumes "leak is
  bounded" — slot loss is fine because next-boot reclaims. But
  reclaim is (a) only on full restart and (b) doesn't actually verify
  the previous tenant is dead, only that no `running` pg row claims it.
  Smoke-r13 is the first cluster smoke that triggered a leak on the
  critical path; once C-7-LT-2-PR1 fixes the probe wedge for the
  common case, leaks will be rarer but the recovery contract will be
  exercised under truly pathological conditions where the assumption
  ("dead agent") is exactly the false premise.
- **Fix sketch**:
  1. `VmIndexLeakLedger` — a pg table or persisted file recording
     `(host_id, vm_index, leak_reason, leaked_at)`. Boot-time:
     `cleanup_orphans_at_startup` consumes the ledger, runs the same
     `wait_for_agent_silent` fence against the listed slot's
     `derive_agent_url`, and either reclaims or re-leaks-with-aging.
     The leak transitions from in-memory-only to durable + verifiable.
  2. Periodic in-process **leak-reaper**: a `detach_isolated` loop
     that walks `vm_index_allocator`'s leaked set every N seconds,
     fences each slot's derived agent URL, and releases on success.
     Restores liveness without controller restart.
  3. **Stop** documenting "orphan-prune will reclaim" in code
     comments and `tracing::warn!` messages until one of (1) or (2)
     actually ships. Three call sites currently lie:
     `nomad_ch.rs:1081`, `:1155`, `:1170`, plus `:3779` and
     `snapshot_handler.rs:428`, `admin_handlers.rs:1365`,
     `sweep.rs:485`.

### [r19-A2] No wake_jobs takeover sweep — CARRY-FORWARD from r18-A1; smoke-r13 makes it strictly worse

- **Location**: still missing. `sweep.rs:283-354` (GC sweep, terminal
  rows only); `db.rs:3115-3117` documents the gap by name. r18-A1's
  fix sketch (`claim_orphan_wake_for_recovery` + `transient_state_lease_expired_wake_jobs`)
  has not landed since r18 closed.
- **What changed in r19**: r18-A1's harm was "rolling deploys leak
  wake_jobs rows; the sandbox row recovers but client sees stuck
  intermediate forever." Smoke-r13 adds a second harm: when the wake
  itself terminal-fails with `slot_unavailable` because of a leaked
  vm_index (r19-A1), the wake_jobs row terminal-writes correctly —
  but the **next** wake attempt on that sandbox enters
  `find_pending_wake_for_sandbox` (`admin_handlers.rs:1556`), MISSES
  the terminal `failed` row (state filter excludes terminal), inserts
  a NEW wake_jobs row, and replays the doomed retry budget against
  the same leaked slot. The system has no concept of "this sandbox's
  slot is poisoned; reject wake until the slot is reclaimed."
- **Why it matters**: post-PR1-probe-fix, smoke-r14 will likely pass
  WAKE 1/1 because healthy teardown clears the fence. But a single
  pathological case in stress (any fence-failed teardown) cascades:
  one wake leaks the slot, every subsequent wake on the same
  sandbox burns 70 s × N retries against a permanent dead-end. Stress
  at c=20 with even a 5 % fence-fail rate amplifies into a tens-of-cycles
  starvation event.
- **Fix sketch**: r18-A1 unchanged, plus add a **slot-poisoned wake
  short-circuit**: when `reserve_vm_index_with_retry` returns
  `slot_unavailable` AND the slot is in the leaked set (r19-A1's
  ledger), terminal-fail the wake fast with a new
  `WakeErrorCode::SlotLeaked` distinct from `SlotUnavailable`. The
  client distinguishes "race with teardown" from "permanent slot
  loss; recreate sandbox."

## IMPORTANT

### [r19-A3] No per-phase wall-time metric for `stop()` — every diagnosis was inferred from a wide log span

- **Location**: `nomad_ch.rs:1257-1266` (final `stop: complete`
  emits `elapsed_ms` for the whole pipeline + `fence_passed`/`job_confirmed_gone`).
  Phase-level wall-time only exists for the host-fence step
  (`:1109/:1123`). `wait_for_job_gone` emits none.
- **What I see**: smoke-r13's diagnosis worked only because R16-A2
  (`417cd6cd`) added phase tracing inside `wait_for_agent_silent`. If
  `wait_for_job_gone` ALSO wedges (Nomad rate-limit), the same
  pathology can hide there.
- **Fix sketch**: (1) `tracing::info!` "phase_wall_time" at every
  phase boundary in `stop()` under target `sandbox::teardown::phase`;
  (2) histogram `sandbox_teardown_phase_seconds{phase=…}` with an
  alert on `_p99 > 25`; (3) document the premise-check pattern
  ("if knob X is tuned and metric Y doesn't move, X doesn't control Y").

### [r19-A4] `from_host_fence_timeout` doc comment codifies the wrong teardown-composition story

- **Location**: `restore_handler.rs:231-246`, `:297-304`.
- **What I see**: doc claims pipeline is "agent /shutdown → host-fence
  wait → Nomad job purge tail (~fence-shaped) → cleanup." Wrong on
  two counts: (a) actual order in `nomad_ch.rs:1037-1132` is
  *Nomad-purge first, then fence*; (b) "Nomad purge tail" is
  `wait_for_job_gone` hard-coded at `:1051` to `Duration::from_secs(30)`,
  not fence-shaped. C-8b's "2× factor" coincidence is just
  `30 + 30 = 60` in the failure case, NOT a model of teardown.
- **Why it matters**: future tuners of `host_fence_timeout_secs`
  will think they're moving a `teardown_wall_time` knob; the constant
  30 s `wait_for_job_gone` floor means effective teardown stays ≥30 s
  regardless. The 2× factor breaks at fence≤15 or fence≥60.
- **Fix sketch**: rewrite the doc block to describe the real two-knob
  composition; rename `teardown_estimate` (`:304`) to
  `worst_case_failed_teardown_at_fence_30s`, or split into the two
  real components.

### [r19-A5] Other ureq sites susceptible to the same wedge — audit

- **Location**: `nomad_ch.rs:2868/2881/2895` (`http_*_unsigned`),
  `:2906` (`send_ureq`), `:2989-3007` (`signed_blocking_call`),
  `:3082-3089` (livez probe in `wait_for_agent_livez`).
- **What I see**: every call is `spawn_blocking(|| ureq::…timeout(N).call())`.
  Smoke-r13's wedge was ureq's `.timeout()` being a *request-deadline*,
  not a *connect-deadline*, on a half-collapsed TAP route. Risk by site:
  - `wait_for_agent_livez:3082` — probes a *booting* TAP not a
    collapsing one; lower wedge risk but non-zero (ARP storms /
    SYN-retransmit). Same compio-TCP fix applies.
  - `signed_blocking_call:2989` — 60 s timeout to in-VM agent;
    failure surfaces as request latency, not slot leak. Lower urgency.
  - `wait_for_job_gone:2780/2805` — Nomad API (server-side
    process, kernel-RST on failure). SYN-blackhole pathology
    doesn't apply. Risk: LOW.
- **Fix sketch**: extract `probe_agent_reachable_tcp` +
  `parse_agent_probe_addr` (`:3428-3438`) into `agent_probe.rs`;
  re-use from `wait_for_agent_livez` (replace the ureq probe).
  Nomad-side ureq calls can stay.

## MINOR

### [r19-A6] r18 carry-forward status — three open findings, mixed progress

- **r18-A1 (wake_jobs takeover sweep)**: still OPEN. Strictly more
  important post-smoke-r13 (r19-A2). No commits since r18.
- **r18-A2 (WakeMachine shutdown observance)**: still OPEN. No
  `Arc<AppState>` or shutdown flag on `WakeMachine` (verified —
  no matches in `wake_machine.rs`). Rolling-deploy leaks unchanged.
- **r18-A3 (`metrics.wake_response_mode` gauge + R17-A4 admin
  kill-switch)**: still OPEN. `wake_response_mode` is stored as
  `WakeResponseMode` (`lib.rs:176`), not `Arc<ArcSwap<…>>`. Boot-only.
- **r18-A4 (dual-track terminal-write ordering)**: still OPEN. The
  `wake_machine.rs:447-479` ordering (sandboxes-CAS-then-wake_jobs-terminal)
  is unchanged; same crash race surface.
- **r18-A5 (WakeMachine vs do_restore_inner divergence)**: still
  applies; no MIRROR comments landed.

PR1 (compio-TCP probe) is uncommitted but staged. PR2 (leak
metrics) is uncommitted but staged. r18-A1 → A5 are independent of
those PRs.

## Cross-lens consensus

- **Concurrency lens (r18)**: r19-A1's leak-reaper is concurrency-shaped
  — a periodic reclaim loop that fences-then-releases. r19-A2's
  short-circuit is a state-machine pre-flight.
- **Code-quality lens**: r19-A4 (the wrong teardown-composition story
  in a load-bearing doc comment) is a code-quality + architecture
  hybrid; should be flagged for the next code-quality lens.
- **Perf lens**: r19-A3's per-phase metric is performance instrumentation
  with architectural intent.
- **Test-cov lens**: r19-A1 needs a pg-gated test that drives
  `stop()` with a forced `fence_passed=false`, asserts the leak is
  ledger-recorded, then asserts the leak-reaper loop reclaims on a
  subsequent successful fence.

## Lens hand-off

1. **Code-quality lens**: audit every `tracing::warn!` and code
   comment that says "orphan-prune will reclaim" — collect them, lump
   into one TODO commit that either deletes the lie or implements
   the reclaim path (r19-A1 fix sketch 3).
2. **Concurrency lens**: design the leak-reaper loop interaction
   with `register_restored` and `reserve_vm_index_with_retry`. Race
   surface: leak-reaper releases slot N → fresh `create` allocs N →
   leak-reaper's "I just released this" log lies about a different
   slot. Needs a CAS-style fence.
3. **Test-cov lens**: replay smoke-r13's `fence_passed=false` shape
   as a controller-level integration test (no cluster needed); stub
   `wait_for_agent_silent` to return `Err`, drive `stop()`, assert
   leak telemetry fires.
4. **API-surface lens**: define `WakeErrorCode::SlotLeaked` wire
   contract for r19-A2's slot-poisoned short-circuit.

## C-4..C-7-LT-1 retrospective — cycle-budget waste analysis

**What was learned**: the entire 12-cycle diagnostic chain
(C-4 r4 → C-6 r7 → C-7 r8 → C-8 r9 → C-8a → C-8b → C-8c r11 →
C-7-LT r12 → C-7-LT-1 r13) was tuning a parameter that, at fence=30,
could not affect the wedge it was being tuned against. The 60.16 s
constant was always **two independent 30 s timeouts in series**
(`wait_for_job_gone` + `wait_for_agent_silent`), not a coupled
teardown waterfall. Every cycle's diagnosis attributed the constant
to the wrong cause and so every fix moved the wrong knob.

**Reasons the chain ran 12 cycles**:

1. **`fence_passed` was logged but ignored**: `nomad_ch.rs:1262`
   has emitted `fence_passed` in the `stop: complete` line since
   the field was introduced; the cluster reviews quoted the wall-time
   but never read the boolean. Smoke-r13 is the first review to
   include `fence_passed=false` in the diagnosis. A simple check —
   "what does `fence_passed` say?" — would have flagged the leak in
   r10.
2. **`wait_for_agent_silent` had no probe-count instrumentation
   until r10/r11 → R16-A2 landed at `417cd6cd` (r17 sprint).**
   Before that, the "1 probe in 30 s" pathology was invisible; the
   smoke logs only said "host_fence: deadline reached." That's
   indistinguishable from "agent was probed 300 times and ACKed
   every one" without the new fields.
3. **The C-8b doc comment** (`restore_handler.rs:231-246`) **codified
   a wrong model** that future readers extended in good faith. The
   "Nomad purge tail is ~fence_timeout-shaped" claim is wrong; both
   are hard-coded 30 s. Once written, the model became
   self-reinforcing across reviews.
4. **No metric tracked "knob X moved, Y did not":** smoke-r10 set
   fence=30, observed teardown=60.16. Smoke-r11 set fence=30 with
   C-8b's 2×, observed teardown=60.16. Smoke-r12 same. Smoke-r13
   same. The wall-time was a pinned constant across four cluster
   cycles with three different knob settings — that's a first-class
   red flag that the knob doesn't drive the variable.

**How to prevent future cycle-budget waste** (architectural controls):

- **Premise-check invariant**: each cluster smoke must state a
  predicted *observable delta* and *falsification criterion*.
  Smoke-r12 should have said "if budget widening works, the
  60.16 s teardown wall-time MUST move with the fence delta —
  else the premise is wrong." That sentence would have stopped
  the chain at r11.
- **Phase-level wall-time metrics** (r19-A3): every blocking I/O
  loop emits `phase_wall_time_ms`; promote to histogram with
  alerts (probe-count-of-1 in the dashboard, not cycle-13 forensics).
- **Refuse doc comments that describe behaviour the code does not
  enforce** (r19-A4). C-8b's doc-as-model wasted cycles. Doc
  comments must cite the assertion that enforces them or be
  marked NON-NORMATIVE.
- **Read the booleans, not just the wall-times**: `fence_passed`,
  `job_confirmed_gone`, `consecutive_misses` — cluster review
  templates must quote these verbatim. Smoke-r10–r12 quoted only
  `elapsed_ms`.

Net: smoke-r14 should treat C-7-LT-2-PR1 (probe wedge) as the fix
for the happy case; r19-A1's leak ledger + reaper is the
architectural follow-up for the pathological case. Without (A1),
stress at c=20 will replay smoke-r13's WAKE 0/1 every time a
fence legitimately fails — by design, but designs depending on
"won't happen often" should not gate Phase B cutover.
