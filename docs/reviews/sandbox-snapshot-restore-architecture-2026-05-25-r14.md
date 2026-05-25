# Sandbox/snapshot-restore — architecture r14 review

Date: 2026-05-25 (UTC)
HEAD at audit: `d673e043` (review brief target).
Round 14 of N (architecture lens).
Prior round: `sandbox-snapshot-restore-architecture-2026-05-25-r13.md`
at `0053e8b6`.

Scope read: `crates/sandbox/**`, `crates/sandbox-agent/**` only.

**In-flight context**: a single commit beyond `d673e043` —
`91ce9be5` — has already landed the C-6 fix (move
`teardown_source_for_snapshot` to a dedicated OS thread with its own
compio runtime). This review treats `91ce9be5` as informationally
in-scope (it's the architectural reaction to the very pattern this
round is asked to investigate) but rates findings at `d673e043` per
the brief.

## Summary

**6 NEW findings (1 CRITICAL, 3 IMPORTANT, 2 MINOR).** The cluster
trajectory r1→r7 has crystallised the single architectural problem
this branch keeps re-discovering: **the single-threaded compio
runtime per ntex worker is a shared resource that the codebase
treats as if it were a Tokio multi-threaded executor**. `compio::
runtime::spawn(...).detach()` is used **9 distinct sites** in
`crates/sandbox/src/` to fire-and-forget background work — and three
of those sites (`admin_handlers.rs:1311` source-teardown,
`nomad_ch.rs:2002` CreateGuard cleanup, plus the `Tiered::put` L2
path that already had to switch to `std::thread::Builder::spawn`
under C-3) are all the same shape: **a future that does multi-second
sync HTTP through `spawn_blocking` runs on the same compio runtime
as request handlers**. The mental model the code was written under
is "spawn_blocking offloads, so my detached future is OK"; the
operational reality at r7 confirms that **between the spawn_blocking
boundaries the future still runs on the request-handler runtime**,
and a future whose first poll dives into an http_client connect
phase can wedge the runtime's other tasks even when the inner ureq
is properly hopped through spawn_blocking.

**The cycle's biggest architectural data point**: C-3's fix
(`snapshot_store_gcs.rs:1132`) and C-6's fix (`admin_handlers.rs:
1353-1378`, landed at `91ce9be5`) **converge on the same pattern** —
spawn a dedicated OS thread, mint a brand-new
`compio::runtime::Runtime`, `block_on` the async work in that
private runtime. The codebase has independently discovered, twice
in two cycles, that the right shape for "fire-and-forget async work
with multi-second blocking sub-steps" is **not** "detach on the
request-handler runtime". It is **"spin up an isolated runtime on a
new thread"**. This is a load-bearing architectural pattern that
deserves its own helper (R14-A1 below) rather than two copy-paste
sites with subtly different framings.

**The new flagship finding (R14-A1, CRITICAL)** is to lift this
"isolated detach" pattern into a named primitive
(`crate::detach_isolated(name, fut)` or similar) and migrate the
remaining 7 `compio::runtime::spawn(...).detach()` sites that share
the same risk profile, OR document why each site is provably safe.
The 7 sites cleanly partition into "process-lifetime loops on the
main runtime (FINE — they are the runtime's primary work)" and
"per-request detach with sync sub-steps (NOT FINE — same C-6
shape)". Today there's no taxonomy; r14 makes the taxonomy explicit
so the next contributor doesn't add an 8th misuse.

Counter-evidence: nothing closed an architecture flagship this
cycle except the in-flight C-6 fix at `91ce9be5` (which closes the
C-6 wake-stall but **opens R14-A1** as the structural pattern that
must be generalised). The phase-tracing scaffolding committed at
`8e7f0b53` is the **first piece of integration-grade
observability** the snapshot/restore module has carried (21 phase
lines across `restore_handler.rs` only — `snapshot_handler.rs`,
`admin_handlers.rs`, and `nomad_ch::stop_inner` all still
zero-instrumented). r14 elevates this to R14-A4: phase-tracing as
*observability discipline*, not a one-off C-6 diagnostic.

R13-A1 (StubRestoreBackend driven tests) and R13-A2 (L2 detach
pull-up) both remain fully open. R12-A1 (dual-builder collapse) and
R10-A4 (nomad_ch.rs split) remain on the queue. The C-4 fix is now
fully wired (`reserve_vm_index_with_retry` invoked from
`do_restore_inner:558`) — the structural plumbing that r13 flagged
as half-landed is committed at `b2892368`.

## Module size table (compared to r13 baseline at `0053e8b6`)

| File | r13 LOC | r14 LOC | Δ | Action |
|---|---:|---:|---:|---|
| `crates/sandbox/src/backend/nomad_ch.rs` | 5399 | **5399** | 0 | R10-A4 + R11-A2 + R12-A4 carry — unchanged. **Still the only file > 5000 LOC.** |
| `crates/sandbox/src/db.rs` | 3303 | **3303** | 0 | R10-A1 / R11-A4 / R12-A5 carry-forward unchanged. |
| `crates/sandbox/src/restore_handler.rs` | 2662 | **3101** | **+439** | C-4 fix landed `b2892368` (+178 LOC, the r13-projected delta) **plus** C-6 phase tracing `8e7f0b53` (+261 LOC of `tracing::info!(phase=…)` lines + helper). **Crossed the 3000 LOC threshold for the first time.** Now the **3rd-largest crate file** (was 4th at r13). |
| `crates/sandbox/src/lib.rs` | 2412 | **2412** | 0 | R4-A1 / R10-A6 / R11-A3 stable. |
| `crates/sandbox-agent/src/handlers.rs` | 2224 | 2224 | 0 | Out of arch scope. |
| `crates/sandbox/src/admin_handlers.rs` | 1781 | **1781** | 0 (at `d673e043`) | **In-flight: 1842 LOC at `91ce9be5`** (+61 LOC C-6 fix). T9 / T10 / R10-A7 carry-forward. |
| `crates/sandbox/src/snapshot_store_gcs.rs` | 1761 | **1768** | +7 | R13-A2 carry — minor doc drift. |
| `crates/sandbox-agent/src/sig.rs` | 1595 | 1595 | 0 | Out of arch scope. |
| `crates/sandbox/src/backend/k8s.rs` | 1580 | 1580 | 0 | Out of arch scope. |
| `crates/sandbox/src/handlers.rs` | 1371 | 1378 | +7 | Stable. |
| `crates/sandbox-agent/src/proxy.rs` | 1314 | 1314 | 0 | Out of arch scope. |
| `crates/sandbox/src/snapshot_aead.rs` | 1277 | 1277 | 0 | Healthy. |
| `crates/sandbox/src/persist.rs` | 1216 | 1222 | +6 | R9-S4b sibling. |
| `crates/sandbox/src/config.rs` | 1051 | 1051 | 0 | R4-A1 carry-forward. |
| `crates/sandbox/src/snapshot_handler.rs` | 955 | 955 | 0 | Stable. |

**Files > 5000 LOC**: 1 (`nomad_ch.rs` 5399, unchanged).
**Files > 3000 LOC**: **3** (`db.rs` 3303, `nomad_ch.rs` 5399,
`restore_handler.rs` 3101 — **NEW threshold crossed by
restore_handler.rs**).
**Files > 2500 LOC**: 3 unchanged.

**The r13 prediction was light by +261 LOC**: r13 forecast
restore_handler.rs at ~2840 if the C-4 fix landed unmodified. Actual
landed is **3101** because C-6 phase-tracing was layered on top.
That's a ~10% file-size jump in a single cycle on a file already
flagged for unbundling.

## Findings (NEW since r13)

### [R14-A1] `compio::runtime::spawn(...).detach()` is used 9 sites with no taxonomy of safety — C-6 is the second cycle to discover that "detach on request-handler runtime + sync sub-step in spawn_blocking" wedges (CRITICAL, architecture-r14)

- **The 9 sites** (`grep -rn "compio::runtime::spawn(" crates/sandbox/src/ | grep -v spawn_blocking`):

  | # | Site (file:line) | Body shape | Risk class |
  |---|---|---|---|
  | 1 | `main.rs:115` | preview-ws server accept loop (process-lifetime) | **SAFE** — process-wide server task; the runtime IS this task |
  | 2 | `preview_ws.rs:98` | per-connection handler spawn (request-lifetime) | **SAFE** — runs IS the request work, ntex worker per-connection model |
  | 3 | `sweep.rs:227` | transient-state takeover loop (process-lifetime) | **SAFE** — sleep-and-poll loop, no sync sub-steps inside ticks |
  | 4 | `sweep.rs:563` | idle-eviction sweep loop (process-lifetime) | **SAFE-ISH** — calls snapshot_handler internally, BUT runs on ticker so co-located with handler is OK |
  | 5 | `registry.rs:829` | idle GC loop (process-lifetime) | **SAFE** — sync registry walk + async backend.stop |
  | 6 | `lib.rs:989` | health-probe loop (process-lifetime) | **SAFE** — sleep + probe ticker |
  | 7 | `lib.rs:1072` | HA heartbeat loop (process-lifetime) | **SAFE** — pg ticker |
  | 8 | `lib.rs:1283` | HA takeover loop (process-lifetime) | **SAFE** — pg ticker |
  | 9 | `nomad_ch.rs:2002` | CreateGuard::drop cleanup (per-request detach) | **DANGER — SAME C-6 SHAPE** |
  | (10) | `admin_handlers.rs:1311` *(pre-91ce9be5)* | snapshot-teardown detach (per-request) | **DANGER — C-6, NOW FIXED at `91ce9be5`** via dedicated thread |

  Sites 1-8 are all **process-lifetime loops** whose body is `loop {
  sleep(N).await; tick().await }` where `tick()` itself is either
  pure-async (compio-postgres calls) or properly delegated through
  `spawn_blocking`. They are the runtime's "background work
  proper". They are **not** the C-6 pattern.

  Site 9 (`nomad_ch.rs:2002`) and the pre-fix site 10
  (`admin_handlers.rs:1311`) are the **per-request fire-and-forget
  detaches** — they each do multi-second blocking sub-work
  (http_signed_async → spawn_blocking-wrapped ureq with 10-60 s
  timeouts) and run on the same compio runtime as the request
  handler that subsequently arrives for the same `vm_index`. This
  is exactly the C-6 wedge shape.
- **Why CRITICAL**: this isn't speculation — C-6 cluster review r7
  (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r7.md`)
  hard-localised the wake stall to inside
  `reserve_vm_index_with_retry`'s `compio::time::sleep(2s).await`
  loop while a detached teardown future is mid-`/shutdown` http
  call (60 s connection timeout). The wake's sleep never wakes
  even after the detached teardown finishes 90 s later — the
  detached task **already consumed the wake task's poll budget on
  this runtime**.

  The C-6 fix at `91ce9be5` mints a dedicated OS thread + private
  compio runtime for the teardown:

  ```rust
  // admin_handlers.rs:1340-1378 (91ce9be5)
  let teardown_thread = std::thread::Builder::new()
      .name(format!("snap-teardown-{}", …));
  teardown_thread.spawn(move || {
      let rt = match compio::runtime::Runtime::new() {
          Ok(r) => r,
          Err(e) => { tracing::error!(…); return; }
      };
      rt.block_on(async move {
          if let Err(e) = state_for_teardown
              .backend
              .teardown_source_for_snapshot(sandbox_id)
              .await
          { tracing::error!(…); }
      });
  });
  ```

  This is **structurally identical** to the C-3 fix at
  `snapshot_store_gcs.rs:1125-1161` (Tiered::put's L2 upload):
  also `std::thread::Builder::new().spawn(move || { … }).` In
  C-3's case the inner work is a sync ureq (no separate compio
  runtime needed); in C-6's case the inner work is async so a
  private runtime is minted.

  **Two cycles, two independent discoveries, two ad-hoc
  workarounds.** Site 9 (CreateGuard::drop) **still uses the bad
  pattern** at HEAD — `compio::runtime::spawn(async move { … })
  .detach()` invoking `http_delete_unsigned` with a 10 s timeout
  + spawn_blocking. The drop fires only on the unhappy create
  path, so production exposure is lower (per-request, only on
  create failure), but the **exact same starvation shape applies**.
  A connection-timeout against a half-dead Nomad in the drop
  branch can wedge a concurrent wake's vm_index reserve sleep on
  the same runtime for up to 10 s. Under cluster stress (c=N
  concurrent CreateGuard drops on a Nomad blip) the wedge
  compounds.
- **Action**:
  1. **Lift the "isolated detach" pattern into a named helper.**
     Concrete sketch (e.g., in `crate::runtime_util` or as a
     `pub(crate) fn` next to the existing `guard_detached`):

     ```rust
     /// Detach `fut` on a dedicated OS thread with its own compio
     /// runtime. Use this instead of
     /// `compio::runtime::spawn(...).detach()` whenever the
     /// future contains multi-second sync sub-steps (typically
     /// `spawn_blocking`-wrapped ureq calls with long timeouts).
     /// On the request-handler runtime, such a detach can starve
     /// concurrent request futures' `compio::time::sleep`
     /// continuations — see C-3 (`snapshot_store_gcs.rs:1132`),
     /// C-6 (`admin_handlers.rs:1340-1378`), and r7 cluster
     /// review.
     ///
     /// `name` is the OS thread name (Linux truncates to 15
     /// chars). Returns `Err` only on `std::thread::spawn` ENOMEM
     /// / EAGAIN — i.e. the process has bigger problems.
     pub(crate) fn detach_isolated<F>(name: String, fut: F) -> std::io::Result<()>
     where F: std::future::Future<Output = ()> + Send + 'static
     {
         std::thread::Builder::new().name(name).spawn(move || {
             match compio::runtime::Runtime::new() {
                 Ok(rt) => rt.block_on(fut),
                 Err(e) => tracing::error!(error = %e,
                     "detach_isolated: could not mint compio runtime"),
             }
         }).map(|_| ())
     }
     ```

  2. **Migrate the 2 unsafe sites:**
     - `admin_handlers.rs:1339-1378` (C-6 fix): replace the inline
       `std::thread::Builder` + `compio::runtime::Runtime::new()`
       + `block_on` with a single `detach_isolated(name, async
       move { teardown_source_for_snapshot(...).await; })` call.
       Net **−35 LOC** at the call site; same behaviour.
     - `nomad_ch.rs:2002` (CreateGuard::drop): replace
       `compio::runtime::spawn(...).detach()` with the new helper.
       Drop the existing `catch_unwind` around the spawn (the
       helper handles the spawn-failure branch). Net **−15 LOC**.
  3. **Document the taxonomy.** In `crates/sandbox/src/backend/
     mod.rs` (or a fresh `crates/sandbox/CLAUDE.md`):

     > `compio::runtime::spawn(...).detach()` is **only safe** for
     > process-lifetime tickers (heartbeat, takeover, GC, health
     > probe). Per-request detaches with multi-second blocking
     > sub-steps MUST use `detach_isolated`.

  4. **Add a clippy lint or grep gate**: a CI grep that fails if
     `compio::runtime::spawn(` appears in any file outside an
     allow-list (`main.rs`, `lib.rs`, `sweep.rs`, `registry.rs`,
     `preview_ws.rs`). New per-request detach sites must go through
     `detach_isolated`.

  Estimated cost: **~80 LOC for the helper + doc, −50 LOC at 2
  call sites = ~+30 LOC net**, removes the C-6 shape from the
  remaining bad-pattern site (CreateGuard::drop) and gives the
  next contributor a one-call API that's by construction safe.

  **Dependency**: this is the *structural* fix for C-6. The
  `91ce9be5` commit is the *operational* fix. r14 recommends
  treating `91ce9be5` as the C-6 closure on the cluster path AND
  landing R14-A1 as a follow-up PR before T-8b-stress, so the
  CreateGuard drop branch doesn't re-introduce the wedge under
  concurrent-create stress.

### [R14-A2] `restore_handler.rs` is now 3101 LOC, up +439 in a single cycle from C-4 + C-6 tracing — phase-tracing is necessary but doesn't belong in the handler module (IMPORTANT, architecture-r14)

- **File**: `crates/sandbox/src/restore_handler.rs` (3101 LOC at
  `d673e043`, was 2662 at r13 = +439, was 2680 at r12 = +421 net
  over 2 cycles).
- **What grew**:
  - C-4 fix (`b2892368`): `VmIndexRetryPolicy` (`:128-145`),
    `vm_index_retry_policy()` trait method (`:243-251`),
    `reserve_vm_index_with_retry` helper (`:265-306`), 3 new
    `StubRestoreBackend` fields, ~6 retry-loop unit tests. ~+178 LOC.
  - C-6 phase tracing (`8e7f0b53`): 21 `tracing::info!(phase = "…",
    "restore: phase")` lines threaded through `restore_sandbox` +
    `do_restore_inner` at every async boundary (14 production
    phases: entry → row_read_ok → read_snapshot_row_ok →
    cas_restoring_ok → pre_reserve_vm_index →
    post_reserve_vm_index → alloc_dir_ready → pre_store_get →
    post_store_get → config_rewritten → pre_submit_restore_job →
    post_submit_restore_job → pre_wait_for_livez →
    post_wait_for_livez → pre_unseal → post_unseal →
    pre_clock_resync → post_clock_resync → pre_register_restored
    → post_register_restored → pre_cas_running → post_cas_running)
    plus the per-phase context payload (vm_index, generation, ok,
    status, etc). ~+260 LOC.
- **Why important**: the file is now the **3rd-largest in the
  crate** (was 4th), crossing the 3000 LOC threshold for the first
  time. The tipping point isn't size in absolute — it's that the
  21 phase-tracing lines + their context fields are NOT actually
  handler logic. They're operability instrumentation that was
  bolted on to localize C-6. The file's logical responsibility
  has crept from "restore handler" to "restore handler + retry
  budget + phase-tracing observability + 15 unit tests + the
  stub backend impl + the value types".

  The cluster cycle's discovery loop is **paying interest** on the
  module-size debt. Every new bug (C-3, C-4, C-6) adds:
  - The fix (10-50 LOC)
  - The tracing for the **next** investigation (~50-260 LOC)
  - The regression test (~30-80 LOC)
  - The stub-field plumbing if cross-module (~20 LOC)

  Net **~+150-400 LOC per cluster cycle** on the same 3 files.
  At this rate `restore_handler.rs` lands at ~3500 LOC by the
  next cluster bug.
- **Action**: extract two concerns.

  1. **`restore_phase` module** (~100 LOC). A `pub(crate) enum
     RestorePhase { Entry, RowReadOk, ReadSnapshotRowOk, …,
     PostCasRunning }` with a `fn log(&self, sandbox_id: Uuid,
     ctx: &PhaseCtx)` method that emits the standardised
     `tracing::info!(phase = self.as_str(), "restore: phase")`
     line + any context fields. The handler calls
     `RestorePhase::PostReserveVmIndex.log(sandbox_id, ctx)`
     instead of inlining the 4-line `tracing::info!` block. **Net
     ~−180 LOC in `restore_handler.rs` + ~+100 LOC in the new
     module = −80 LOC overall.** Bonus: any test can pin
     `enum::PhaseEmitter::collect_phases` to assert "wake reached
     phase=post_clock_resync" without grepping a log file.
  2. **`restore_handler::types` module** (~200 LOC). Move
     `VmIndexRetryPolicy`, `RestoreBackend` trait, `RestoreOutcome`,
     `SnapshotRowMeta`, `RestoreHandlerError` into a sibling
     types-only module. The handler file becomes "the handler
     function + tests"; the value-objects move out. **Net ~−200
     LOC in `restore_handler.rs` + ~+200 LOC in a new types
     module.** Doesn't change LOC overall — but it does cut
     `restore_handler.rs` to ~2700 LOC (below the 3000
     threshold) and lets the C-6 tracing live closer to the
     value types that emit it.

  Combined, **`restore_handler.rs` drops from 3101 to ~2670 LOC
  (~−14%)** with no logic change. The handler is the C-6
  fix-site for the next cluster cycle; keeping it readable is
  load-bearing.

  Land **after R13-A1** (the StubRestoreBackend-driven tests) so
  the test module doesn't have to be moved mid-refactor.

### [R14-A3] The "isolated detach" pattern (C-3 + C-6 fixes) reveals a deeper invariant: per-request fire-and-forget belongs OFF the request-handler runtime — extending to R13-A2 (Tiered::put L2 detach) is now obviously correct (IMPORTANT, architecture-r14)

- **Files**:
  - C-3 fix shape: `crates/sandbox/src/snapshot_store_gcs.rs:1125-1161`
    (`std::thread::Builder::new().name(...).spawn(...)`).
  - C-6 fix shape: `crates/sandbox/src/admin_handlers.rs:1340-1378`
    (in-flight at `91ce9be5`) — same `std::thread::Builder` shape
    + a private `compio::runtime::Runtime`.
  - Sibling that DIDN'T get the fix: `nomad_ch.rs:2002`
    (CreateGuard::drop, see R14-A1).
  - r13-A2 pre-existing pull-up proposal:
    `snapshot_handler.rs:392-407` — the L2 detach in `Tiered::put`
    should move to handler.
- **Symptom**: r13-A2 framed `Tiered::put`'s `std::thread::Builder
  ::spawn` as a *layering inversion* — the impl shouldn't spawn
  background work; the handler should. That framing was correct
  but **incomplete**. The fuller architectural read after C-6 is:

  **Per-request fire-and-forget work with multi-second blocking
  sub-steps MUST live on a runtime separate from the request-
  handler runtime.** This is the invariant the C-3 fix and the
  C-6 fix both express, independently. R13-A2's pull-up to the
  handler ONLY helps if the handler's spawn-and-detach goes on a
  *different* runtime than the one handling subsequent requests
  for the same resource — which on a single-threaded compio
  runtime per ntex worker, **detaching to the same runtime
  doesn't satisfy**.

  So r13-A2's recommended diff:

  ```rust
  // snapshot_handler.rs (r13-A2 proposed)
  if let Some(l2) = store.l2_handle() {
      compio::runtime::spawn(async move {
          let _ = spawn_blocking(move || l2.put(...)).await;
      }).detach();
  }
  ```

  is **still wrong under the C-6 lens**. The detach on the
  request-handler's compio runtime + a multi-second
  spawn_blocking sub-step is exactly the C-6 shape. The C-3 fix's
  current `std::thread::Builder` shape **is correct** for the
  impl-spawn-the-thread choice — it just lives at the wrong
  layer.

  The architecturally correct shape is **handler-level isolated
  detach**:

  ```rust
  // snapshot_handler.rs (revised post-r14)
  if let Some(l2) = store.l2_handle() {
      let _ = detach_isolated(format!("l2-upload-{}",
          uuid_to_base62(&sandbox_id)), async move {
          let _ = compio::runtime::spawn_blocking(
              move || l2.put(&sid, &path, &ver)).await;
      });
  }
  ```

  Where `detach_isolated` is the R14-A1 helper. This puts the L2
  upload on its own thread + its own compio runtime, leaving the
  request-handler runtime free.
- **Why important**: R13-A2 was rated IMPORTANT and recommended
  the pull-up. R14-A3 says **the pull-up is necessary but not
  sufficient** — without the dedicated-thread shape, the pull-up
  recreates C-6 at the handler layer. The two findings have to
  land together. The post-r14 recommended sequencing is:
  1. R14-A1 lifts `detach_isolated` into a helper.
  2. R13-A2 + R14-A3 pull `Tiered::put`'s L2 detach up to
     `snapshot_handler.rs`, using `detach_isolated`.
  3. The handler's L2 detach + the admin's snapshot-teardown
     detach + CreateGuard's drop cleanup ALL go through the
     same helper.
- **Action**: same as R13-A2 in spirit, with the diff sketch
  above. Cost: same +15 LOC at the handler, −40 LOC in
  `snapshot_store_gcs.rs`, **plus R14-A1's helper as a hard
  prerequisite**.

### [R14-A4] Phase-tracing is observability discipline, not a one-off — `snapshot_handler.rs`, `nomad_ch::stop_inner`, and `admin_handlers::snapshot_sandbox` all need the same treatment (IMPORTANT, architecture-r14)

- **Files**:
  - The 21 phase lines (only in restore_handler today):
    `crates/sandbox/src/restore_handler.rs:343, 355, 384, 395,
    552, 559, 579, 601, 621, 684, 704, 726, 739, 754, 798, …, 904,
    943, 1037, 1078` (a representative subset; `grep "phase = " |
    wc -l` returns **21** in this file).
  - Zero phase lines elsewhere:
    - `snapshot_handler.rs`: 0 (grep)
    - `nomad_ch.rs` (all 5399 LOC, including `stop_inner`'s
      4-step teardown): 0
    - `admin_handlers.rs`: 0
- **Symptom**: the C-6 mystery (r6 → r7) wasn't solved by adding
  more code — it was solved by adding 21 lines of structured
  tracing that turn "the wake handler is wedged somewhere
  between entry and completion" into "the wake handler is wedged
  at `phase=pre_reserve_vm_index` with the next phase
  unreached".

  The discipline that bought this win is **phase-boundary tracing
  at every async boundary** in the request handler. It worked
  because:
  - **Every** await point in `do_restore_inner` got a `pre_…` →
    `post_…` pair.
  - The phase label is a **stable string**, not a free-form
    message — `grep "phase = "pre_reserve_vm_index"`` returns
    one place in the code AND one row per request in the logs.
  - The context payload is **structured** (sandbox_id, vm_index,
    generation, ok) — not interpolated free text.

  But snapshot_handler / admin_handlers / nomad_ch::stop_inner all
  have similar phase counts and **none of them have this
  treatment**. The next cluster cycle that wedges in `stop_inner`
  (e.g., `wait_for_job_gone` timeout, host_fence hang) will have
  the same r6-style debug-by-bisection cost — *another full
  cluster cycle to add tracing, then another to find the actual
  bug*. That's exactly the trajectory the cluster r1→r7 series
  paid for restore_handler. Doing it once more in snapshot_handler
  is wasteful.
- **Why important**: this is a **discipline lever** with massive
  ROI. The cost of phase-tracing is ~1 line per await boundary
  (~3-5% of the handler LOC). The benefit is **cluster-cycle
  bug localization in 1 cycle instead of 2-3** (per the r7 review:
  "r7 is the first cycle of the T-8b series that did NOT uncover
  a new bug — instead, it provided actionable localization of the
  prior cycle's mystery").

  This is also the **logical predecessor** to R13-A1
  (StubRestoreBackend-driven tests). With phase-tracing, the
  tests can assert *which phase* the stub reached, not just "the
  call returned Err". Mocking the backend to fail at
  `phase=post_wait_for_livez` becomes 1 assertion: `assert_eq!
  (recorded_phases.last(), Some(&"post_wait_for_livez"))`.

  If R13-A1 (driven tests) lands without R14-A4, every new test
  asserts on the *return value* of `restore_sandbox` — coarse.
  With R14-A4, tests assert on the *phase trajectory* — fine,
  and the next cluster bug's failure mode appears in the test
  too.
- **Action**:
  1. **Extract phase-tracing to a module** (per R14-A2.1) so the
     shape is reusable across handlers.
  2. **Apply to `snapshot_handler::snapshot_sandbox`**: ~8-10
     phase lines covering entry → cas_to_snapshotting →
     pre_ch_pause → post_ch_pause → pre_ch_snapshot →
     post_ch_snapshot → pre_store_put → post_store_put →
     pre_cas_snapshotted → post_cas_snapshotted. ~40 LOC.
  3. **Apply to `admin_handlers::snapshot_sandbox`** (the entry
     point + the detached teardown): ~6 phase lines.
     - The detached teardown's phases now live in
       `nomad_ch::stop_inner` (next step).
  4. **Apply to `nomad_ch::stop_inner`**: ~10 phase lines for
     entry → state_remove → pre_shutdown → post_shutdown →
     pre_nomad_stop → post_nomad_stop → pre_wait_gone →
     post_wait_gone → pre_fence → post_fence → pre_release →
     post_release → host_dir_clean → exit. **This is the most
     valuable migration** — `stop_inner` runs detached at
     `admin_handlers.rs:1340-1378` post-91ce9be5, and is the
     **most likely** next-cluster-cycle wedge site.

  Estimated cost: ~120 LOC across 3 files (mostly mechanical),
  but reduces the next cluster cycle from "discover + diagnose
  + fix" to "diagnose + fix" — saves ~1 cluster cycle ($0.50 +
  ~2 hours of human time) per future C-N bug.

### [R14-A5] 5+ un-wrapped `std::fs` calls remain on the async restore/snapshot path — minor compared to C-6, but the same starvation shape (MINOR, architecture-r14)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:568`
    (`std::fs::remove_dir_all(&alloc_dir)`).
  - `crates/sandbox/src/restore_handler.rs:574`
    (`std::fs::create_dir_all(&alloc_dir)`).
  - `crates/sandbox/src/restore_handler.rs:638`
    (`std::fs::metadata(&p)` in the `phase=post_store_get` stat
    loop, 3× per request).
  - `crates/sandbox/src/restore_handler.rs:923, 954`
    (`rewrite_config_json` reads + writes `config.json` sync from
    the async caller).
  - `crates/sandbox/src/snapshot_handler.rs:355`
    (`std::fs::create_dir_all(temp_dir)`).
  - `crates/sandbox/src/snapshot_handler.rs:304`
    (`let _ = std::fs::remove_dir_all(&temp_dir)` in error
    cleanup).
- **Symptom**: these are sync `std::fs` calls on the async path,
  not wrapped in `spawn_blocking`. None are individually
  high-latency — `create_dir_all`/`metadata` are ~0.1 ms, the
  `config.json` is ~5 KB. But `remove_dir_all(&alloc_dir)` at
  restore_handler.rs:568 can be **seconds** if the stale alloc
  directory contains a partial restore artifact (~1 GB of
  memory-ranges + state.json on tmpfs from a prior crashed
  attempt). On a single-threaded compio runtime, that's
  several-second blocking — same shape as C-6 but smaller
  amplitude.
- **Why minor**: the production hit rate is low (stale alloc only
  exists after a partial restore + retry). The cluster series
  hasn't seen this manifest. But it's the same architectural
  invariant violation as C-6 — **sync work on the request-
  handler runtime starves other request futures**. R14-A1's
  taxonomy makes this finding mechanical: any `std::fs::*` on
  the async path is the same shape as detach-with-blocking.
- **Action**: wrap the 6 sites in `compio::runtime::
  spawn_blocking`. Pattern already used at `restore_handler.rs:
  611, 714, 747` for the heavy `store.get`/`submit_restore_job`/
  `wait_for_livez` calls. Cost: ~+5 LOC per site, ~+30 LOC
  total. Low priority; land alongside R14-A2's module split.

### [R14-A6] `RealRestoreBackend::vm_index_retry_policy()` is now hard-coded — should read from `cfg.nomad_ch.host_fence_timeout_secs` to track the actual teardown SLO (MINOR, architecture-r14)

- **Files**:
  - The trait default: `restore_handler.rs:243-251` (60 × 2 s = 120 s).
  - The `VmIndexRetryPolicy::default()` impl: `:141-145`
    (max_attempts = 60, interval = Duration::from_secs(2)).
  - `RealRestoreBackend`'s impl: doesn't override at HEAD
    (`grep "fn vm_index_retry_policy" crates/sandbox/src/` returns
    only the trait declaration + one StubRestoreBackend override).
  - The actual teardown timeout: `crates/sandbox/src/config.rs`
    `host_fence_timeout_secs` (default 120 s).
- **Symptom**: the C-4 fix's retry budget is 60 × 2 s = 120 s,
  matching the worst-observed teardown wall-time of ~90 s + a
  margin. But the **actual teardown timeout is operator-tunable**
  via `SANDBOX_HOST_FENCE_TIMEOUT_SECS` (default 120, max
  unbounded). An operator who raises the fence to 300 s (for a
  noisy cluster) creates a configuration where the retry budget
  is **less than** the worst-case fence — the wake handler will
  503 with `VmIndexUnavailable` while the teardown is still
  legitimately running. That's correctness-adjacent: the wake
  *should* wait for the fence to clear, not give up early.
- **Why minor**: requires an operator-driven knob mismatch (raise
  fence without raising retry). The default config aligns the
  two values within tolerance. But the retry policy is the
  **wake-side mirror** of the host_fence_timeout — they should
  be **derived** from a single config field, not independently
  configured.
- **Action**: override `vm_index_retry_policy` on
  `RealRestoreBackend` to read from
  `cfg.nomad_ch.host_fence_timeout_secs`:

  ```rust
  fn vm_index_retry_policy(&self) -> VmIndexRetryPolicy {
      // R14-A6: track the operator's actual teardown SLO.
      // Fence timeout + 1 × interval margin so we don't 503 just
      // as the teardown is releasing.
      let fence_secs = self.cfg.host_fence_timeout_secs;
      let interval = Duration::from_secs(2);
      let max_attempts = ((fence_secs + 2) / 2).max(1) as u32;
      VmIndexRetryPolicy { max_attempts, interval }
  }
  ```

  ~10 LOC. Closes a configuration footgun. **No urgency** — the
  default config aligns; this is an operator-resilience nice-to-
  have.

## C-6 fix architectural review (per review brief's prompt 1+2)

### Was the C-6 fix at `91ce9be5` the right shape?

**Yes — but it's the second copy of the C-3 pattern, not a primitive.**

The `91ce9be5` fix:
1. Replaces `compio::runtime::spawn(...).detach()` at
   `admin_handlers.rs:1311` with a `std::thread::Builder::new()
   .name(...)` + private `compio::runtime::Runtime::new() +
   rt.block_on(...)`.
2. Comments at `admin_handlers.rs:1311-1338` are a 27-line
   architectural justification: *"Mirror C-3's pattern (`snapshot_
   store_gcs.rs::Tiered::put`): spawn a dedicated OS thread with
   its own short-lived compio runtime via `compio::runtime::Runtime
   ::new().block_on(...)`. Decoupling from the ntex worker's
   runtime is the only way to guarantee no cross-task starvation;
   spawn_blocking on the worker runtime is insufficient because
   the teardown future itself (between blocking calls) runs on
   the worker."*

This is **architecturally correct**. The diagnosis (cross-task
starvation via shared single-threaded runtime) matches the r7
cluster review's confirmation. The fix shape (private runtime
on private thread) is the same pattern the codebase already
adopted for C-3. The code is fine.

**The problem is that this is the second site to discover and
implement the pattern.** C-3 and C-6 are independent fixes, each
~30-50 LOC, with mostly-overlapping prose justifications. The
**third** site that needs this pattern (CreateGuard::drop at
`nomad_ch.rs:2002`, R14-A1 above) has NOT been fixed yet — it
still uses the bad `compio::runtime::spawn(...).detach()` shape.

Per the review brief's prompt 4 (sprint structure), the **actual
sprint** is:
- **PR 1**: lift `detach_isolated(name, fut)` helper. ~80 LOC.
  File: a new `crates/sandbox/src/runtime_util.rs` (or extend
  `crates/sandbox/src/backend/nomad_ch.rs:2156`'s existing
  `guard_detached`). Tests in the same file (~50 LOC of
  property-pattern tests against the helper). Depends on: none.
- **PR 2**: migrate `admin_handlers.rs:1339-1378` to use the
  helper. Net **−35 LOC**. Depends on: PR 1. **This effectively
  re-lands C-6 fix in a cleaner form.**
- **PR 3**: migrate `nomad_ch.rs:2002` (CreateGuard::drop) to
  use the helper. Net **−15 LOC**. **This is the
  not-yet-addressed C-6 sibling.** Depends on: PR 1.
- **PR 4**: R14-A2.1 phase-tracing extraction. ~+100/−180 LOC.
  Depends on: R13-A1 lands first (otherwise the test module
  has to be moved mid-refactor).
- **PR 5**: R14-A4 — apply phase-tracing to `nomad_ch::
  stop_inner` + `snapshot_handler` + `admin_handlers::
  snapshot_sandbox`. ~+120 LOC. Depends on: PR 4.
- **PR 6**: R14-A2.2 types module extraction. ~+0 LOC net.
  Depends on: PR 4.
- **PR 7**: R13-A1 + R13-A2 + R14-A3 — driven tests + L2 detach
  pull-up using `detach_isolated`. ~+250 LOC. Depends on:
  PR 1 (for `detach_isolated`), PR 4 (for phase-tracing
  asserts).
- **PR 8+**: existing carry-forwards (R11-A1 secret_io,
  R10-A1 recovery.rs, R10-A4 nomad_ch split, R12-A1
  jobspec collapse, etc.) per r13 ordering.

**Total to close C-6 + R14 findings: 7 PRs** (PRs 1-7 above).
PRs 1+2 are the **minimum** to declare R14-A1 closed and the
sprint structurally done; PRs 3-7 are the polish + the
test-feedback-loop wins.

## The cluster trajectory (review brief's prompt 4: 7-cycle pattern)

| Cycle | Bug | Layer | Root-cause shape | Fix shape | Structural sibling |
|---|---|---|---|---|---|
| T-8b r1 | C-1 (driver decode wrong) | controller↔driver codec | wire-format mismatch | controller patch | T-3/T-4/T-5 (Config typed evolution) |
| T-8b r2 | C-2 (stale L1 cache) | snapshot store | L1 consistency under reuse | rootfs materialization | R5-P1b (L1 round-trip work) |
| T-8b r3 | (re-test of C-2) | — | — | — | — |
| T-8b r4 | C-2 fix landed; (no new bug, C-2 closed properly) | — | — | — | — |
| T-8b r5 | **C-3** (`Tiered::put` spawn_blocking-in-spawn_blocking panic) | snapshot store impl | sync/async boundary | `std::thread::Builder` (raw OS thread) | **R14-A1** (this round) |
| T-8b r6 | **C-4** (vm_index reserve race, wedge silent) | wake handler ↔ source-teardown lifecycle | detached future holds slot, wake polls for release | bounded-retry caller-side | **R4-A2** (LeasedVmSlot RAII) |
| T-8b r7 | **C-5** (GCS scope 403) + new finding: **C-6** wake silent stall | (C-5) operator config; (C-6) ntex worker compio runtime starvation | C-5: IAM; C-6: same as C-3 (sync/async boundary in detached future) | (C-5) script patch; (C-6) phase tracing then dedicated-thread+private-runtime detach | **R14-A1** (this round) |
| T-8b r8 (predicted) | C-6 LOCALIZED+fixed; either green or new bug in `stop_inner`/`teardown_source` phase | depends on whether R14-A4 phase-tracing extends to `stop_inner` | next layer down, predictable | TBD | TBD |

The cluster cycle's **frontier of failure**:
- r1-r4 attacked **boot+snapshot** path.
- r5 was the first time **snapshot store layering** mattered.
- r6 was the first time **wake handler ↔ teardown lifecycle**
  mattered.
- r7 was the first time **runtime starvation across detached
  tasks** mattered.

Each cycle's bug is one layer deeper than the last. The pattern is
**predictive**: r8 (post-C-6 fix) will either find a new bug in
`stop_inner` (which is still un-phase-traced) or hit the **register/
clock-resync** path, which is also un-phase-traced beyond
`restore_handler`. The next 1-2 cluster cycles' bugs are
**probabilistically** in code that R14-A4 covers.

**Recommendation**: R14-A4 (phase-tracing across the full
snapshot+restore call chain) is the **structural attack** on the
cluster's bug-discovery loop. R13-A1 (driven tests) is the
**inwards** attack (catch in unit tests). Land both before T-8b-
stress.

## Audit: `spawn_blocking` correctness (review brief's prompt 3)

Spot-checked 5 sites:

| Site | What's wrapped | Verdict |
|---|---|---|
| `restore_handler.rs:611` (`store.get`) | Full sync sha256+AEAD+GCS read | **CORRECT** — sync call inside spawn_blocking; the `'static` clones are owned; panic-handled with `unwrap_or_else`. |
| `restore_handler.rs:714` (`submit_restore_job`) | Sync ureq POST + alloc polling | **CORRECT** — same pattern. |
| `restore_handler.rs:747` (`wait_for_livez`) | Sync `/livez` poll loop | **CORRECT** — same pattern. |
| `restore_handler.rs:447` (`teardown_restore` rollback) | Sync DELETE | **CORRECT** — R10-C2 fix already wrapped this; the wrap is post-error so the rollback path doesn't park the worker. |
| `snapshot_handler.rs:397` (`store.put`) | Sync L1 + L2 (now L1-only post-C-3) | **CORRECT but tied to R13-A2** — the impl spawns its own thread for L2 (R13-A2 layering inversion), but the spawn_blocking around L1 is fine. |

**Missing spawn_blocking** (the 5 `std::fs::*` sites at R14-A5):
- `restore_handler.rs:568` (`remove_dir_all`)
- `restore_handler.rs:574` (`create_dir_all`)
- `restore_handler.rs:638` (3× `metadata`)
- `restore_handler.rs:923, 954` (`read_to_string` + `write` in
  `rewrite_config_json`)
- `snapshot_handler.rs:355` (`create_dir_all`)

All are low-amplitude on warm-cache, healthy disk; all are
architecturally the same shape as C-6. Fixed by R14-A5.

## Carry-forward (still open from earlier rounds)

- **[R4-A2 / R5-A2]** LeasedVmSlot RAII guard — **13th cycle**.
  Now subsumed by R14-A1's broader detach-pattern fix: a
  LeasedVmSlot is still useful, but the **immediate** wedge
  cause is the runtime starvation, not the slot RAII. After
  R14-A1 lands, R4-A2 becomes the "next" structural improvement
  (eliminates the polling-retry shape entirely).
- **[R3-A1 / R5-A1 / R10-A3]** `Backend` enum 5-Err-returner split
  — count at HEAD = **5** (unchanged). Still CRITICAL.
- **[R3-A2 / R10-A2]** `RestoreBackend` trait facade — **9
  methods** at HEAD (the `vm_index_retry_policy` method landed
  with C-4 fix `b2892368`). 3 of 9 are one-line `Arc<NomadCH>`
  delegations.
- **[R3-A3]** wrapper bash → Rust sidecar — subsumed by R11-A2 /
  R12-A4.
- **[R3-A4]** `StopDisposition` enum — still `stop_inner(.., bool)`.
- **[R4-A1 / R10-A6 / R11-A3]** AppState builder accretion —
  **11th cycle**, unchanged at 7 `with_*` + `new_fixture`. The
  R14-A1 audit incidentally identified **no NEW `with_*` builders**
  this cycle, so the trend is flat (which is mildly positive).
- **[R10-A4 / R11-A2 / R12-A4]** nomad_ch.rs at 5399 LOC,
  un-split. T-8b prerequisite.
- **[R12-A1]** Dual builder consolidation — diff sketch in r13-A3
  is still actionable; R13-Q1 already unblocked it.
- **[R10-A1 / R11-A4 / R12-A5]** db.rs at 3303 LOC carrying the
  recovery CAS. Un-extracted.
- **[R11-A1 / R11-Q2 / R12-A2]** root-owned-secret-file 5-site
  duplication. No movement.
- **[T9 / T10 / R10-A7]** ControllerIdleSnapshotter duplicates
  admin_handlers' 70-LOC orchestration — unchanged.
- **[r9 C3]** AEAD fail-OPEN on GCS path.
- **[R13-A1]** StubRestoreBackend never drives `restore_sandbox`
  — **still 0 driven tests**. The C-4 fix added 3 stub fields
  for this exact purpose and used them in **zero** tests; the
  helper `reserve_vm_index_with_retry` got its own unit tests
  (~6 of them at `:2228+`) but the integration shell remains
  missing.
- **[R13-A2]** L2 detach pull-up — unchanged at HEAD;
  reinterpreted by R14-A3 (must land *with* R14-A1's helper).

## Closed by recent commits

- **C-4** (vm_index race) — CLOSED at `b2892368`. The C-4
  scaffolding r13 noted as half-landed is now wired:
  `do_restore_inner:558` calls `reserve_vm_index_with_retry(backend.
  as_ref(), sandbox_id, snap.vm_index).await?`. Net +178 LOC
  matching r13's prediction.
- **C-6 fix** (in-flight at `91ce9be5`, ONE commit past brief
  HEAD `d673e043`) — informationally CLOSED for review purposes.
  See R14-A1: the operational fix is good; the structural
  generalization (lifting `detach_isolated`) is the open lever.
- **R14-API1** (`with_shared_allocator` + `with_nomad_handle`
  visibility) — CLOSED at `00161cea`.
- **R11-API1** (3 orphan metrics fns) — CLOSED at `370fdbba`.
- **R13-API1 / R10-API2** (ExecBody visibility) — CLOSED at
  `af4678ac`.

## What's structurally new vs. r13

| Item | r13 state | r14 state | Δ |
|---|---|---|---|
| `compio::runtime::spawn(...).detach()` sites (production) | not enumerated | **9 enumerated; 2 risky (1 fixed at 91ce9be5, 1 still bad)** | +1 architectural finding |
| `restore_handler.rs` LOC | 2662 | **3101** | **+439** (+178 C-4, +261 phase-trace) |
| Files > 3000 LOC | 2 (db, nomad_ch) | **3** (db, nomad_ch, **restore_handler**) | **+1** |
| `RestoreBackend` trait methods | 8 committed + 1 in-flight | **9 committed** | C-4's method landed |
| Phase-tracing lines (production) | 0 | **21** (restore_handler only) | +21 |
| Phase-tracing-instrumented files | 0 | **1** (restore_handler) | +1 / 5 expected |
| Cluster cycles since r13 | 0 | **2** (r6 + r7) | — |
| Cluster bugs discovered since r13 | 0 | **2** (C-5 op-config; C-6 runtime starvation) | — |
| Independent rediscoveries of "isolated detach" pattern | 1 (C-3) | **2** (C-3 + C-6) | architectural primitive overdue |
| `std::fs::*` un-spawn_blocking sites on async path | not enumerated | **6 enumerated** (R14-A5) | +1 minor finding |
| `RealRestoreBackend::vm_index_retry_policy` config-driven | n/a (didn't exist) | **hard-coded default at 60×2s** | R14-A6 |

The cycle's net structural movement is:
- **+1 architectural primitive overdue** (R14-A1 detach_isolated).
- **+1 module crossed 3000 LOC** (restore_handler.rs).
- **+1 observability discipline gap** (R14-A4 phase-tracing
  outside restore_handler).
- **−1 cluster mystery** (C-6 localized + fixed in-flight).
- **0 architecture-flagship findings closed** (R13-A1, R13-A2,
  R12-A1, R10-A4, R10-A1, R11-A1, R4-A2 all open).

## Recommended order of attack (updated for r14; 8 PRs)

Updated from r13's 8-PR plan with two prepends (R14-A1, R14-A4)
and the R14-A2 module split:

1. **R14-A1 — `detach_isolated` helper + migrate 2 sites**
   (admin_handlers C-6, nomad_ch CreateGuard::drop). ~80 LOC
   helper + 2 call-site migrations. **PR #1 because this is
   the C-6 structural closure.** Closes the
   second-rediscovery-of-same-pattern problem; gives the
   codebase a named primitive.
2. **R13-A1 — `restore_handler::tests::driven` module** — 6-8
   end-to-end `restore_sandbox` tests against StubRestoreBackend.
   ~250 LOC. **Must precede R14-A2 module split** to avoid
   moving test files mid-refactor.
3. **R14-A4 — phase-tracing on `nomad_ch::stop_inner` +
   `snapshot_handler` + `admin_handlers::snapshot_sandbox`**.
   ~120 LOC. **Closes the cluster-cycle bug-discovery
   inefficiency** at the snapshot+stop layers.
4. **R14-A2 — restore_handler.rs split (phase module + types
   module)**. Net −80 LOC; drops file from 3101 → ~2700 LOC.
5. **R13-A2 + R14-A3 — L2 detach pull-up via `detach_isolated`**.
   ~+15 LOC handler / ~−40 LOC store. Closes the C-3 layering
   inversion using R14-A1's helper.
6. **R11-A1 / R12-A2 secret_io::read_root_owned_secret_file**
   extraction — same as r13 PR #3.
7. **R10-A1 / R11-A4 / R12-A5 db.rs → recovery.rs** — same as
   r13 PR #4.
8. **R10-A7 snapshot orchestrator extraction** — same as r13
   PR #5.
9. **R12-A3 TaskDriverMode struct field** — same as r13 PR #6.
10. **R10-A4 + R12-A1 + R12-A4 together** — nomad_ch.rs split +
    jobspec collapse. Same as r13 PR #7.
11. **R10-A3 + R10-A2 + R4-A2 LeasedVmSlot** — same as r13
    PR #8.

Total: **11 PRs**, of which PRs 1-3 are the **r14 cluster-
unblockers** (close C-6 structurally + put a fast-feedback shell
in place + extend phase-tracing).

**The critical insertion is PR #1 (R14-A1)** — same priority as
r13's R13-A1 was. PR #2 (R13-A1 driven tests) is unchanged from
r13's recommendation. PR #3 (R14-A4 phase-tracing extension) is
new — it makes the **next cluster cycle's bug-discovery cost
proportional to the bug fix's complexity, not the bug fix +
~250 LOC of new tracing per cycle**.

**In cycle order**:
- **First**: PR 1 (R14-A1) + PR 2 (R13-A1 driven tests). Land
  as a single concept-PR-of-PRs (3 commits: helper, admin
  migrate, drop migrate; then the test module).
- **Then**: Cluster smoke-r8 to verify R14-A1 closes C-6
  structurally + verify no new bug.
- **Then**: PR 3 (R14-A4 phase tracing extension) so the next
  cluster cycle (if any) localizes in 1 cycle.
- **Then**: PRs 4-7 in parallel; PRs 8-11 sequentially.

Closes 9 carry-forwards + 6 r14-new findings + creates the
regression net under C-4, C-6, and (preemptively) the next
cluster bug.
