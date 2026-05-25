# Sandbox snapshot-restore architecture review — 2026-05-25 r16

**Reviewer**: architecture-r16 (cron-pilot)
**HEAD**: 2e9ae598 (C-8b ceiling 2× fix landed at `64af1803`; controller pin v26 at `2e9ae598`)
**Prior round**: r15 (round-1 + round-2 in a single file at d392a308)
**Lens**: architecture

## Summary
- 5 findings: 1 critical, 3 important, 1 minor.
- Most pressing: the C-8b "2× fence" doc change pretends to be a constant factor, but Smoke-r10 measured 60.164 s teardown at fence=30s only because `wait_for_agent_silent` *timed out* (~30 s) and was followed by another fence-shaped Nomad-purge tail — the ratio is a coincidence of two `~fence` waits, not a stable property; **C-8b will under-estimate again whenever either component disconnects from the fence value**, and the deadline-ceiling MIN-of-two means we never observe the regression until the deadline-ceiling stops binding.

## CRITICAL

### [r16-A1] C-8b's `2 × host_fence_timeout_secs` teardown estimate is a numeric coincidence, not a model — it will under-estimate again when either component decouples from fence value
- **Location**: `crates/sandbox/src/restore_handler.rs:265-301` (`from_host_fence_timeout` body); doc-block at `:231-264` (the C-8b rationale).
- **What I see**: The C-8b doc explicitly attributes the 60.164 s measurement to **two sequential `~fence`-shaped waits** composing:
  1. `wait_for_agent_silent` (`nomad_ch.rs:3205-3273`) — bounded by `host_fence_timeout_secs` and (per the smoke-r10 commit message) "often timing out because the agent's HTTP listener takes >fence to actually close".
  2. The Nomad job purge tail — "`fence_timeout`-shaped" only because Nomad's allocation-terminal lag is on a *separate timeline* from the host fence, and r10 happened to measure them at parity.

   The formula then hard-codes `teardown_estimate = 2 * host_fence_timeout_secs`. But the two waits do not vary together with `host_fence_timeout_secs`:
  - The Nomad purge tail (`wait_for_job_gone` in `nomad_ch.rs:1048-1060`) has its **own** 30 s timeout, fully independent of `host_fence_timeout_secs`. Under a fence of 60 s the purge tail will *still* be capped at ~30 s, so teardown ≈ 60 + 30 = 90 s, not 120 s as the formula assumes.
  - Under a fence of 10 s, the Nomad purge tail is *larger* than the fence (~30 s vs 10 s); teardown is dominated by Nomad, so `2 × 10 = 20 s` under-estimates the empirical teardown of `~10 + 30 = 40 s` by a factor of 2.

   So the formula is correct *only* at the single fence value (30 s) the smoke happened to use. Two regimes are visible already:
  - **fence ∈ {0..15}**: `2 × fence < 30s Nomad tail` → formula under-estimates teardown ⇒ wake's last retry attempt lands *inside* the Nomad-purge window, will 503.
  - **fence ≥ 60**: deadline-ceiling MIN binds at 50 s regardless, so the 2× factor is **inert** — the failure mode is hidden behind C-8a's hard cap, not actually closed.

   The doc block claims "the MIN-of-two design from C-8a still structurally prevents the C-7 silent cancellation regardless of how this estimate is tuned" — which is true for the *silent-cancel* failure mode (because the deadline-ceiling is the hard cap). But for the *wake-success* failure mode, the formula's accuracy only matters when fence-ceiling binds (fence < 60), and that's exactly the regime where the 2× constant is least defensible.

- **Why it matters**: the architectural intent of `from_host_fence_timeout` is to "tie the wake budget to the operator-tuned fence so a future stress run bumping fence scales the wake budget automatically" (per the r14-A6 doc at `:185-193`). C-8b broke that property in a subtle way: the budget now scales linearly with fence under `fence < 30 s` (incorrectly: Nomad tail dominates), is exactly right at `fence == 30 s` (the measured point), and stops scaling at all under `fence ≥ 30 s` (deadline-ceiling binds). The R14-A6 architectural property "fence drives budget" is preserved only at one point. The next operator who tunes fence (per C-8's documented escape hatch) will discover this empirically — *again*.

   This is the same architectural pattern R15-A1 already named: **the synchronous-wake contract forces the budget to live under a 60 s ceiling, which forces this kind of brittle numeric tuning**. C-8b's commit message admits it: "defense-in-depth tactical patch; the structural fix is C-7-LT". But r16 sees a sharper claim: even *as defense-in-depth*, C-8b's formula is wrong for `fence < 30` and inert for `fence ≥ 60`. The single point where it is "correct" is the smoke-r10 measurement point.

- **Fix sketch**:
  1. **Land R15-A1 (C-7-LT)** — once the synchronous-response contract is gone, the entire `from_host_fence_timeout` arithmetic is moot. This is still the right structural answer.
  2. **Until R15-A1 lands**: replace the `2 × fence` constant with the actual model. The teardown is `host_fence + nomad_purge_tail + small_constant`, not `2 × host_fence`. The Nomad purge tail is bounded by a separate timeout (`wait_for_job_gone`'s 30 s at `nomad_ch.rs:1051`). Model that explicitly:
     ```rust
     // Teardown components (each bounded by its own timeout):
     //   1. wait_for_agent_silent: bounded by host_fence_timeout_secs.
     //   2. wait_for_job_gone:     bounded by 30s (nomad_ch.rs:1051).
     //   3. cleanup tail:          ~5s constant.
     const NOMAD_PURGE_TIMEOUT_SECS: u64 = 30; // from wait_for_job_gone
     const CLEANUP_TAIL_SECS: u64       = 5;
     let teardown_estimate =
         host_fence_timeout_secs + NOMAD_PURGE_TIMEOUT_SECS + CLEANUP_TAIL_SECS;
     ```
     This model is honest about the dependency: changing fence shifts ONE component; changing the Nomad timeout shifts another. The constants come from the actual code sites, not from a single measurement.
  3. **Add a property test**: for each (fence, nomad_purge_timeout) pair in {(10,30), (30,30), (60,30), (120,30)}, assert the derived budget is `≥ teardown_estimate − HEADROOM` AND `≤ CLIENT_DEADLINE − HEADROOM`. Pin the model, not the numbers — the model is the architecture.
  4. **Mark the file**: until R15-A1 lands, every constant in `from_host_fence_timeout` needs a comment pointing at its *source* (which code site, which RFC). The smoke-r10 measurement comment is a one-off; we want the constants to be derivable from the codebase.

## IMPORTANT

### [r16-A2] The `wait_for_agent_silent` 2-consecutive-misses contract is itself the source of fence-tail variance — but it lives in `nomad_ch.rs` with zero phase tracing, so the next "teardown took longer than expected" investigation will repeat r10's diagnosis cost
- **Location**: `crates/sandbox/src/backend/nomad_ch.rs:3205-3273` (`wait_for_agent_silent`); commits 64af1803 attributes the 60 s teardown specifically to this function's "2-consecutive-misses contract".
- **What I see**: The C-8b commit message names this function as the proximate cause of the 2× ratio: "wait_for_agent_silent host-fence wait, up to fence_timeout, often timing out because the agent's HTTP listener takes >fence to actually close". So the fence-tail behaviour is dominated by:
  - The 100 ms poll cadence (`:3265`).
  - The 2-consecutive-misses gate (`:3253`).
  - The 500 ms per-probe timeout (`:3225`).

  All three are constants in this function with **no instrumentation** between "fence started" (`nomad_ch.rs:1097`) and "fence cleared/timeout" (`:1110`/`:1125`). The whole 30 s window the cluster review had to diagnose by reading Nomad's audit log is silent at the controller-log layer. R15-A2 / R14-A4 flagged this; the C-8b fix shipped without addressing it.

   r15 R15-D1 also flagged this from the concurrency lens; r16 escalates from architecture: the architectural fault is that the function whose behaviour drives the empirical teardown distribution is the *least* instrumented in the stop-path. The retry-budget formula in `restore_handler.rs` cannot be modelled correctly without observability into what `wait_for_agent_silent` is doing.

- **Why it matters**: smoke-r10 paid a cluster cycle to localize teardown=60.164s to the fence wait. Smoke-r11 under c=20 will produce a *distribution* of teardown times; without per-poll instrumentation in `wait_for_agent_silent`, the team has no way to read that distribution from the controller log alone. The next surprise (smoke-r11 reveals teardown=75 s? 90 s? 120 s?) will be diagnosed the same way: by reading Nomad's audit log. That's the predicted failure mode r15 already named; C-8b proved it; r16 records that **the fix has still not landed**.

- **Fix sketch**:
  1. Add a per-poll INFO log inside `wait_for_agent_silent` (mirrors C-7's per-attempt log shape):
     ```rust
     tracing::info!(
       target: "zeroship_sandbox::backend::nomad_ch",
       agent_url = %base_url,
       poll = probe_count,
       consecutive_misses = consecutive_misses,
       last_status = ?last_status,
       "stop/fence: wait_for_agent_silent poll",
     );
     ```
     Volume at c=20 with a 30 s fence + 100 ms cadence = 6000 lines per stress run. Acceptable; the C-7 per-attempt log is the same shape.
  2. Pre-condition: extract phase tracing per R15-A2 *first*; then this becomes one more `phase = "fence_poll"` line.

### [r16-A3] `restore_handler.rs` has crossed 3500 LOC (was 3444 at r15, +90 for C-8b) — the retry-policy module split (R15-A3) is still un-landed and now buries 5 doc-block iterations on the same 35-LOC function
- **Location**: `crates/sandbox/src/restore_handler.rs` (3534 LOC at HEAD; was 3444 at r15 d392a308). `from_host_fence_timeout` (the only function changed by C-8b) is now ~35 LOC of code wrapped in ~80 LOC of doc-block enumerating R14-A6 → C-8a → C-8b iterations.
- **What I see**: the file's growth trajectory across the last 4 rounds:
  - r12 → r13: 2680 → 2662 LOC (−18, doc cleanups).
  - r13 → r14: 2662 → 3101 LOC (+439, C-6 phase tracing + C-7 retry shrink).
  - r14 → r15: 3101 → 3444 LOC (+343, R14-A6 + C-8a).
  - r15 → r16: 3444 → 3534 LOC (+90, C-8b).
  - **3 consecutive cycles of growth driven entirely by retry-policy iterations on `from_host_fence_timeout`**. The function is 35 LOC; its doc block is now 80 LOC.

   R15-A3 already proposed extracting a `retry_policy` module as a low-dependency early win. Cycle r16 confirms the carry cost: every new retry-policy iteration (R14-A6, C-8a, C-8b, presumably C-8c when smoke-r11 surfaces the next regime) lands in the largest file in the crate's hot path, and the doc-block burden grows monotonically. R10-A4 (`nomad_ch.rs` 5399 LOC un-split for 5 cycles) is being mirrored by `restore_handler.rs`.

- **Why it matters**: the architectural smell is the same one R3-A2 named four months ago: `restore_handler.rs` is a second backend implementation. C-8b's diff is 100% scoped to one function on `VmIndexRetryPolicy`. That entire concern is extractable as a sibling module *today* with zero dependency on R13-A1. Cycle r15 proposed it; cycle r16 measures the cost of one more cycle of not doing it (+90 LOC, all doc).

- **Fix sketch**: same as R15-A3 — extract `restore_handler::retry_policy` (`VmIndexRetryPolicy` + `from_host_fence_timeout` + `reserve_vm_index_with_retry` + the 5 retry-policy unit tests; ~350 LOC at HEAD). Restore_handler drops to ~3180 LOC. The retry-policy module's tests are value-object arithmetic — not coupled to R13-A1's StubRestoreBackend integration tests. Low risk, drops the file below `db.rs` again, and isolates the next C-8c iteration to one module.

### [r16-A4] R4-A2 LeasedVmSlot RAII is now in its 15th cycle of carry — but R10-C1's "remove state-map entry on rollback" interim (12 LOC) is what would have *also* caught C-8b's wake-success failure mode (different concern, same root)
- **Location**: `crates/sandbox/src/restore_handler.rs:2003-2034` (the R10-C1 interim at `teardown_restore`, comment at `:2019` "structural cure is R4-A2's `LeasedVmSlot` RAII; this 1-line interim is the cheap insurance").
- **What I see**: R4-A2 has been carried 15 consecutive cycles. The cost is no longer abstract — it's measurable in patches:
  - **R10-C1** (lines 2003-2034, ~12 LOC) — manual state-map cleanup in `teardown_restore`. Comment says "this 1-line interim".
  - **R10-C2** (the `spawn_blocking` wrap at line 628) — manual sync-on-async wrap in the rollback path.
  - **C-8a / C-8b** — manual min-of-two ceiling math, dual constant CLIENT_DEADLINE/HEADROOM in the same function body.

   Each of these is a localized "1-line interim" or "tactical patch" comment. The architectural pattern is: every cluster cycle adds another interim on a different surface (state-map, sync-on-async, retry-budget), and none of them collapses into the next-most-natural primitive (LeasedVmSlot or the C-7-LT async response).

   Concretely, **the cost of NOT landing R4-A2 today**:
  - `teardown_restore` is 30+ LOC of cleanup choreography (R10-C1 + release + nomad purge).
  - The rollback closure in `restore_sandbox` (lines 597-647) is ~50 LOC of error-translation + best-effort-teardown + CAS-on-rollback.
  - `nomad_ch.rs::stop_inner` (lines 986-1190) is 200+ LOC of similar choreography on a *different* state-map / vm_index pair.

   A LeasedVmSlot RAII type would collapse all three into roughly:
  ```rust
  struct LeasedVmSlot {
      sandbox_id: Uuid,
      vm_index: i16,
      backend: Arc<dyn RestoreBackend>,
      state_map: Arc<NomadCHBackend>,
      committed: bool,
  }
  impl Drop for LeasedVmSlot { /* release + state-map remove */ }
  impl LeasedVmSlot { fn commit(mut self) { self.committed = true; } }
  ```
   The rollback closure would be a 3-line early-return on Err. C-8b's "is the retry budget aligned with teardown?" question would still exist, but the *cleanup correctness* concern (which has produced R10-C1, R10-C2, R11-C1, R11-C2, R11-I1, R12-M1, R14-V1 over 6 cycles) would be done.

- **Why it matters**: R4-A2 is the longest-open critical finding in this branch. The cluster-cycle cost of carrying it has been compounded by every new failure mode that adds another manual cleanup site. The C-8b fix didn't *touch* the cleanup surface — but the next cluster cycle (whichever C-8c, C-9, ... lands) almost certainly will, because cleanup choreography is the highest-density code in the wake path.

- **Fix sketch**: same as R4-A2 + R5-A2 + carried — introduce LeasedVmSlot RAII with `commit()` on success. ~80 LOC of new type, ~−60 LOC at use sites (3 use sites: restore_handler::do_restore_inner success, restore_handler rollback, nomad_ch::stop_inner). Should land before R15-A1 (C-7-LT) — the async-response refactor will want to hand a LeasedVmSlot to the detached task as its single point of ownership.

## MINOR

### [r16-A5] `CLIENT_DEADLINE_SECS` and `CLIENT_HEADROOM_SECS` are now triplicated across `from_host_fence_timeout`, doc text in 3 places, and the cluster-config-30 override rationale — **the canonical ntex/stress-client deadline has no single source of truth in the codebase**
- **Location**:
  - `crates/sandbox/src/restore_handler.rs:266-267` — `const CLIENT_HEADROOM_SECS: u64 = 10; const CLIENT_DEADLINE_SECS: u64 = 60;` (local consts inside fn body).
  - `crates/sandbox/src/restore_handler.rs:203-204` (doc), `:224` (doc), `:248-262` (doc examples table) — same numbers, three doc surfaces.
  - The actual ntex client deadline (60 s) lives… nowhere we own. It's set by the stress client (`/opt/stress/snapshot_stress.py`) and by ntex defaults. The controller's code has no way to discover or enforce it.
- **What I see**: the constants `CLIENT_DEADLINE_SECS = 60` and `CLIENT_HEADROOM_SECS = 10` are local to `from_host_fence_timeout`. The same numbers appear textually in three doc surfaces in the same file, in the C-7 / C-8a / C-8b commit messages, and (implicitly, as a literal) in the stress client. If the stress client raises its timeout to 90 s, the controller has no way to know — but the C-8a cap would silently leave the budget at 50 s.

   This is R15-A5 (encode the invariant in the type), one cycle of carry, now slightly worse because C-8b added the `2 × fence` factor as another magic constant *next to* the deadline ceiling.

- **Why it matters**: the deadline-ceiling is a contract between three independent components (ntex, stress-client, controller). Pinning it as a local `const` in one function body is the architectural minimum. Pinning it as a typed constant on `VmIndexRetryPolicy` (per R15-A5) would also let the C-8b model live in code, not in a doc-block. The C-8b commit's "smoke-r10 measured 60.164 s" rationale is itself evidence that nobody can find the deadline contract today — the team had to *measure* the teardown wall-time because the deadline-ceiling is implicit.

- **Fix sketch**: same as R15-A5 — promote `CLIENT_DEADLINE_HARD_CEILING` to a `pub const` on `VmIndexRetryPolicy`, force constructors through a `try_new` that validates `(max_attempts − 1) * interval ≤ CLIENT_DEADLINE_HARD_CEILING`. Defer if R15-A1 (C-7-LT) lands — async response removes the deadline ceiling entirely.

## Cross-lens consensus tracker

- **[r4-A2 LeasedVmSlot RAII]**: cycle 15 of carry. **LIVE**. Sharper finding this round (r16-A4): the cost is measurable in patches — R10-C1 + R10-C2 + R11-C1 + R11-C2 + R11-I1 + R12-M1 + R14-V1 are all 1-line interims that would collapse into LeasedVmSlot. Concurrency-r15 also flagged this at 12+ cycles.
- **[r3-A2 restore_handler as 2nd backend]**: cycle 13+ of carry. **LIVE**. r16-A3 measures one more cycle of cost (+90 LOC, all doc). The retry-policy split (r15-A3) is the smallest piece of this that can land independently.
- **[r10-A4 nomad_ch split, 5399 LOC]**: cycle 5 of carry, UNCHANGED at HEAD. **LIVE**. r16 observes `wait_for_agent_silent` (the fence loop) is one of the highest-leverage extraction targets — its behaviour drives the empirical teardown distribution but it has zero phase instrumentation (r16-A2).
- **[r15-A1 C-7-LT async wake response]**: cycle 1 of carry. **LIVE**. r16-A1 sharpens the cost: C-8b is the 6th patch on the synchronous-response contract; its formula is wrong for `fence < 30` and inert for `fence ≥ 60`; the only measurement that validates it is the smoke-r10 point.
- **[r15-A2 phase tracing extension]**: cycle 2 of carry. **LIVE**. r16-A2 names a specific predicted failure (smoke-r11's teardown-distribution localization will repeat r10's audit-log-reading diagnostic cost).
- **[r15-A3 retry_policy module split]**: cycle 2 of carry. **LIVE**. r16-A3 measures +90 LOC of pure carry this cycle.
- **[r15-A4 R13-A1 stub-driven harness]**: cycle 2 of carry (8+ cycles open as R13-A1). **LIVE**. r16 doesn't add new evidence; r15's cost-benefit math (~4 of 9 catchable bugs / ~$3.80 paid) is still the load-bearing argument.
- **[r15-A5 type-enforced retry budget invariant]**: cycle 2 of carry. **LIVE-IF-NOT-R15-A1**. r16-A5 adds the C-8b doc-triplication and the absent-single-source-of-truth on `CLIENT_DEADLINE_SECS` as fresh evidence; deferral remains correct if R15-A1 ships.
- **[r15-I1 host_fence_timeout config drift 120/30]**: cycle 2 of carry (raised by concurrency-r15). **LIVE**. Still 120 in `config.rs:401`, 30 in `gcp-worker-startup.sh:468`. r16 doesn't re-flag.
- **[r15-I2 fence-budget headroom math]**: cycle 2 of carry. **PARTIALLY-RESOLVED by C-8b**, structurally still LIVE. C-8b's 2× factor coincidentally makes the fence-30 case work (26 attempts × 2 s = 50 s budget > 30 s fence + 30 s nomad tail), but r16-A1 shows the underlying issue (formula doesn't model components) remains.
- **[C-6 sibling detach sites]**: brief asked about audit drift. Verified at HEAD: 8 `.detach()` sites in `crates/sandbox/src/`. Three categories:
  1. **Steady-state startup loops** (sweep.rs:253, :611; lib.rs:1008, :1108, :1422; registry.rs:870): all are long-running background loops that wrap their iteration body in `catch_unwind` and call `compio::time::sleep`. Different pattern from C-6 (per-request, ureq-blocking). Not the C-6 footgun.
  2. **Per-connection on accept-loop** (preview_ws.rs:105): WS upgrade then handle_connection; longer-running and on the WS server's own runtime, not the admin worker's. Not C-6.
  3. **Per-request cleanup tasks**:
     - `admin_handlers.rs:1306-1396` — **fixed by C-6 (uses dedicated OS-thread)**.
     - `backend/nomad_ch.rs:2127` (CreateGuard::drop cleanup) — **still uses the bad pattern** (`compio::runtime::spawn(...).detach()` from the ntex runtime). R14-A1 has been open since r14; the C-6 fix at admin_handlers did not extract a helper to migrate this site.
  - **No new detach sites added since r14.** Audit drift: zero. The single open site (CreateGuard::drop) is the same one r14-A1 named. No new escalation; r16 confirms r14-A1's call site is still pending.

## Lens hand-off (what next round should focus on)

Two paths, in priority order:

1. **architecture next round (r17)**: write the R15-A1 / C-7-LT proposal at `docs/proposals/sandbox-wake-async-response.md`. r15 already named this as the next-sprint flagship; r16 sharpens the cost (C-8b's 2× factor is a numeric coincidence; the next C-8c iteration is queued behind whatever smoke-r11 surfaces). The proposal itself is a 1-round design exercise — no code. Land the proposal in r17, implement across r18–r20.

2. **alternative: rotate to test-coverage**. R13-A1 (StubRestoreBackend-driven integration tests) has been open EMERGENCY for 8 rounds. R15-A4's cost-benefit math says ~4 of 9 cluster bugs were catchable by a ~250-LOC harness. r16's r16-A1 finding adds C-8b to the catchable list (a property test on the teardown-estimate formula would have caught the under-estimation regime at PR-time, not at smoke-r11). Test-coverage-r16 should be dispatched if architecture-r17 cannot land R15-A1 in one cycle.

If r17 is also architecture: the highest-leverage *code* finding to translate to a PR is **r16-A3 (extract `retry_policy` module)** — low dependency, drops `restore_handler.rs` below `db.rs`, isolates the next C-8c iteration to one module. R15-A3's exact recommendation; one more cycle of cost just measured.
