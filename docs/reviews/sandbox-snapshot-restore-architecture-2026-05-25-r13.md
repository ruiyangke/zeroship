# Sandbox/snapshot-restore — architecture r13 review

Date: 2026-05-25 (UTC)
HEAD at audit: `0053e8b6`.
Round 13 of N (architecture lens).
Prior round: `sandbox-snapshot-restore-architecture-2026-05-25-r12.md`
at `46e0fa2a`.

Scope read: `crates/sandbox/**`, `crates/sandbox-agent/**` only.

## Summary

**6 NEW findings (1 critical, 3 important, 2 minor).** Five commits
landed since r12 touching the in-source surface (`c890c015` C-3 fix,
`8ed9aa90` R10-API5/6 doc fix, `c5b9cb9d` R13-Q1 env-mutex unify) plus
two script-only commits (`953a5fa3` v20 pin bump, `d7740b03` C-5 GCS
scope). One **uncommitted in-flight change** in the working tree
(`crates/sandbox/src/restore_handler.rs`, +178 LOC): the C-4 fix
scaffolding (`VmIndexRetryPolicy` + `reserve_vm_index_with_retry`
helper) is **half-landed** — the helper and trait method are defined
at `restore_handler.rs:128-145, 243-251, 265-306` but the production
caller at `do_restore_inner` line 442-444 still calls the un-retried
`backend.reserve_vm_index(snap.vm_index)` directly. This is a
**plumbing gap, not a design issue** — the C-4 sprint's design choice
(caller-side bounded retry, the `(d)` shape the cluster review
deferred to) is the right one, but it isn't wired up yet.

The cycle's **biggest architectural data point** is the cluster cycle
trajectory itself: 5 retries, 4 distinct new bugs (C-1 → C-2 → C-3 →
C-4) discovered ONE PER CYCLE. The pattern is a **production-only
test-coverage shortfall**, not a controller-side design defect. Each
of the 4 bugs is structurally the same shape: a sync/async boundary
crossing (C-1: ureq+compio interleave, C-2: stale L1 cache, C-3:
spawn_blocking-inside-spawn_blocking, C-4: detached-teardown holds a
slot the wake handler synchronously demands) that **every existing
unit test misses because it doesn't drive the integration end-to-end**.

R13-T2 (from r12 test-cov review) flagged that `StubRestoreBackend` is
**never used to drive `restore_sandbox`** in tests — it's only
exercised through 2 setter-spot-checks at `lib.rs:2107-2129`. r13
elevates this to a flagship structural finding: **R13-A1**. The same
shape applies to C-3 (`Tiered::put` only tested under `#[compio::test]`
where `spawn_blocking` resolves a compio TLS — production runs from a
worker thread with no TLS and panics) and to C-4 (the detached
teardown's `release()` is independently tested, the wake reserve is
independently tested, but no test exercises them concurrently against
a shared allocator).

Two architectural carry-forwards moved decisively:

- **R13-Q1 / R12-A1 partial close** at `c5b9cb9d` — the cross-module
  `SANDBOX_TASK_DRIVER` env-mutex unified into a sibling test module
  `nomad_ch::test_env_lock` (`pub(crate)` `TASK_DRIVER_ENV_LOCK`). The
  cross-module env-lock race is gone. The **dual-builder duplication
  itself is unchanged** — R12-A1's structural CRITICAL remains open;
  the env-mutex unify retired the test-side flake hazard, NOT the
  shared-helper consolidation.
- **C-3 fix at `c890c015`** — `std::thread::Builder::spawn` replaces
  `compio::runtime::spawn_blocking` at `snapshot_store_gcs.rs:1132`.
  This raises a **layering question** (R13-A4 below): why does
  `Tiered::put` spawn at all when the trait doc at
  `snapshot_store.rs:94-96` says callers MUST hop through
  spawn_blocking? See R13-A4 for the architectural read.

Counter-evidence: nothing closed an architecture flagship this cycle.
R13-Q1 closed a test-side race; the dual-builder problem remains.
R12-A1 / R12-A4 are now **provably independent levers** — unifying
the env-mutex (the small lever) didn't move the consolidation needle
(the large lever).

## Module size table (compared to r12 baseline at `46e0fa2a`)

| File | r12 LOC | r13 LOC | Δ | Action |
|---|---:|---:|---:|---|
| `crates/sandbox/src/backend/nomad_ch.rs` | 5371 | **5399** | **+28** | R10-A4 + R11-A2 + R12-A4 — net +28 from the R13-Q1 env-lock unify (+88 in nomad_ch, −60 in restore_handler). **Still the only file > 5000 LOC.** Crossed 5400 boundary. |
| `crates/sandbox/src/db.rs` | 3298 | **3303** | +5 | R10-A1 / R11-A4 / R12-A5 carry-forward — small drift from a doc comment touchup; recovery layer untouched. |
| `crates/sandbox/src/restore_handler.rs` | 2680 | **2662** | **−18** | R12-A4 carry-forward. **Drop is mechanical** (env-lock helper deleted as part of R13-Q1). **NB:** the working-tree-uncommitted C-4 fix adds +178 LOC; if committed unmodified, restore_handler.rs lands at ~2840 LOC (NEW threshold). |
| `crates/sandbox/src/lib.rs` | 2412 | **2412** | 0 | R4-A1 / R10-A6 / R11-A3 still at 7 `with_*` + `new_fixture` (8 grep lines). |
| `crates/sandbox-agent/src/handlers.rs` | 2224 | 2224 | 0 | Out of architecture scope. |
| `crates/sandbox/src/admin_handlers.rs` | 1781 | 1781 | 0 | T9 / T10 / R10-A7 carry-forward. |
| `crates/sandbox/src/snapshot_store_gcs.rs` | 1663 | **1761** | **+98** | **NEW** growth — the C-3 fix added the `std::thread::spawn` path + the regression test `c3_put_callable_from_non_compio_thread`. Doc comment block at 1086-1110 is ~25 LOC of architecture justification (worth its own read; see R13-A4). |
| `crates/sandbox-agent/src/sig.rs` | 1590 | 1595 | +5 | Out of arch scope. |
| `crates/sandbox/src/backend/k8s.rs` | 1580 | 1580 | 0 | Out of arch scope. |
| `crates/sandbox/src/handlers.rs` | 1371 | 1371 | 0 | Stable. |
| `crates/sandbox-agent/src/proxy.rs` | 1331 | 1314 | −17 | Out of arch scope. |
| `crates/sandbox/src/snapshot_aead.rs` | 1277 | 1277 | 0 | Healthy. |
| `crates/sandbox/src/persist.rs` | 1216 | 1216 | 0 | R9-S4b sibling. |
| `crates/sandbox/src/config.rs` | 1051 | 1051 | 0 | R4-A1 carry-forward. |
| `crates/sandbox/src/snapshot_handler.rs` | 955 | 955 | 0 | Stable. |

**Files > 5000 LOC**: 1 (`nomad_ch.rs` at 5399, unchanged set).
**Files > 2500 LOC**: 3 (`db.rs` 3303, `nomad_ch.rs` 5399,
`restore_handler.rs` 2662 — set unchanged from r12; the wake-path
file's −18 trend is reversed the moment C-4 fix's +178 LOC lands).

## Findings (NEW since r12)

### [R13-A1] `StubRestoreBackend` is configured but never used to drive `restore_sandbox` — explains why C-1 through C-4 all surfaced on cluster, not locally (CRITICAL, architecture-r13)

- **Files**:
  - Definition: `crates/sandbox/src/restore_handler.rs:844-921`
    (`pub struct StubRestoreBackend` + `impl RestoreBackend`).
  - Production handler: `crates/sandbox/src/restore_handler.rs:317-368`
    (`pub async fn restore_sandbox`).
  - **Only two usage sites in the entire crate**:
    - `crates/sandbox/src/lib.rs:2111-2114` (`with_restore_backend_replaces_existing` test — exercises the AppState setter, NOT the handler).
    - `crates/sandbox/src/lib.rs:2115-2118` (same test, just the second-call branch).
  - Grep evidence: `grep -rn "StubRestoreBackend::" crates/sandbox/src/` returns **exactly 2 hits**, both in the same test on `lib.rs:2107-2129`. **`restore_sandbox` itself has zero unit-test coverage via the stub.**
- **Symptom**: the entire restore-flow control surface
  (`do_restore_inner` at `restore_handler.rs:430-700`, ~270 LOC of
  10-step orchestration: status-CAS → reserve_vm_index → alloc_dir →
  store.get → submit_restore_job → wait_for_livez → clock_resync →
  persist.unseal → register_restored → final-status-CAS) has **zero
  fast-feedback test coverage**. The unit tests at
  `restore_handler.rs:2228-2802` (15 tests, 6 of them `#[compio::test]`)
  exercise **narrow leaf functions**:
  - `clock_resync_post_restore_*` (4 tests at lines 2228-2417) — the
    leaf `/clock_resync` handshake.
  - `r10_c1_teardown_restore_*` (3 tests at lines 2430-2654) — the
    leaf rollback teardown.
  - `r11_*` / `r9_*` / `r12_i1_*` / `r13_q1_*` — leaf jobspec/env
    smoke.

  **Not a single test drives the full `restore_sandbox` call.** The
  4 cluster bugs all manifest at the cross-leaf seam:
  - **C-1** (driver-side resource codec): manifest seam between
    controller jobspec submit and Go driver decode. No test.
  - **C-2** (stale L1 cache): manifest seam between snapshot upload's
    L1 put and the next worker's L1 get. No test.
  - **C-3** (spawn_blocking-in-spawn_blocking panic): manifest seam
    between the handler's `spawn_blocking` wrap and `Tiered::put`'s
    inner `spawn_blocking`. No test reaches this because every
    `Tiered::put` test runs under `#[compio::test]` which provides a
    TLS the production worker-thread path lacks.
  - **C-4** (wake-vs-source-teardown vm_index race): manifest seam
    between admin_handlers' detached `teardown_source_for_snapshot`
    and the wake handler's synchronous `reserve_vm_index`. The two
    sides are individually tested; **no test drives them
    concurrently against a shared `VmIndexAllocator`**.
- **Why CRITICAL**: this isn't a missing-edge-case finding — it's a
  **systemic test-architecture gap**. The stub backend was *designed*
  to drive the handler under unit-test conditions (its docstring at
  `:197-199` says *"drive `restore_sandbox` with a `StubRestoreBackend`
  (whose default `register_restored` impl is also a no-op)"* —
  signalling intent — but no test was ever written). Every cluster
  cycle since r1 has been **paying** for this gap with a fresh
  ~24-hour bug-discovery loop (provision → smoke → fail → diagnose →
  fix → re-cycle). The cluster-r5 reviewer called this out as the
  trigger to pause; r13 says it's also the trigger to land
  fast-feedback coverage. **C-4's design is fine; the lack of a
  pre-cluster local repro is what made it discoverable only at the
  last possible layer.**

  Three classes of test the stub already supports but that have no
  caller today:
  1. **Cross-step status-CAS races** — drive `restore_sandbox` with a
     pg fixture (already in scope: `db.rs` has `make_test_pool`) +
     stub backend. Assert CAS-loss surfaces correctly through the
     existing `Database::CasLost` shape.
  2. **Reserve retry semantics** (the C-4 fix's new dimension) — set
     `StubRestoreBackend::reserve_succeeds_on_attempt = Some(3)`,
     `vm_index_retry_policy = { max_attempts: 5, interval: 0 }`, and
     assert `restore_sandbox` succeeds with `attempt > 1`. **The C-4
     fix's working-tree diff adds the stub fields for this exact
     purpose but ships zero test using them.**
  3. **Teardown idempotency under partial-rollback** — `fail_livez =
     true` + a hot persist; assert the unique `teardown_restore` call
     count vs the number of in-flight restores doesn't double-count.
- **Action**: this is **the structural lever** for closing the
  cluster-cycle-discovery debt. Specifics:
  1. **Add `restore_handler::tests::driven` module** — a set of
     ~6 happy/sad-path tests against `StubRestoreBackend` driving the
     full `restore_sandbox`. Each test ~30-50 LOC. Net ~200-300 LOC of
     tests; closes 4 carry-forwards (T8, T9, R3-T3, R13-T2 partial)
     and creates a regression net under the C-4 fix.
  2. **Repeat the pattern for `Tiered::put`** — the C-3 reviewer
     added one `c3_put_callable_from_non_compio_thread` test
     (mentioned in the commit message) that runs from
     `std::thread::spawn`. **One** such test is the floor, not the
     ceiling: the architectural shape says **every** `SnapshotStore`
     trait method needs a non-compio-TLS test, because every method
     is callable from `spawn_blocking` worker threads.
  3. **Adopt as a crate-wide invariant**: any `pub` trait method that
     might be called inside `compio::runtime::spawn_blocking` MUST
     have at least one regression test that exercises it from a
     `std::thread::spawn` shape. (Document at `snapshot_store.rs:91`
     where the trait already calls out the spawn_blocking contract.)
     Estimated cost: ~150 LOC across `SnapshotStore` + `RestoreBackend`
     + `Backend::teardown_source_for_snapshot`.

### [R13-A2] `Tiered::put` spawning its OWN background thread is a layering inversion — the trait says callers MUST wrap in spawn_blocking, so the spawn belongs at the handler, not inside the impl (IMPORTANT, architecture-r13)

- **Files**:
  - Trait contract: `crates/sandbox/src/snapshot_store.rs:91-96`
    (*"All methods are blocking. Async callers should use
    `compio::runtime::spawn_blocking`"*).
  - C-3 fix site: `crates/sandbox/src/snapshot_store_gcs.rs:1068-1164`
    (`impl SnapshotStore for TieredSnapshotStore::put`).
  - Caller: `crates/sandbox/src/snapshot_handler.rs:392-407` — the
    only production caller, already wraps in
    `compio::runtime::spawn_blocking`.
- **Symptom**: the C-3 fix is correct *as a patch* — it stops the
  spawn_blocking-inside-spawn_blocking panic. But the resulting shape
  is a **layering inversion**:
  - The trait says callers MUST `spawn_blocking` (correct — the L2
    upload is ~5-30 s of GCS streaming + sha256).
  - The handler obediently `spawn_blocking`s at line 397.
  - Inside that `spawn_blocking` worker, `Tiered::put` **launches a
    second background thread** (via `std::thread::Builder::spawn` at
    `snapshot_store_gcs.rs:1132`) to detach the L2 upload.
  - So there are **two background workers per put**: one for L1 (the
    handler's `spawn_blocking`) and one for L2 (the std::thread
    spawn). The L1 work is small (rename + sha256, <1s); the L2 work
    is large (GCS upload, ~5-30s). The handler's `spawn_blocking`
    completes when L1 finishes, releasing the worker — but L2 is
    still running on the second thread, untracked, unjoined, no
    cancel.
  - **The trait's spawn_blocking contract is satisfied for L1 but
    NOT for L2.** L2 lives on an OS thread that is invisible to
    compio. If the controller process restarts during L2 upload, the
    in-flight upload is dropped silently (no shutdown hook). Today's
    fire-and-forget contract accepts this; once we add retry +
    metering (as the doc-comment at `:1080-1085` promises), the
    untracked thread becomes load-bearing.
- **Why important**: the **right shape** for fire-and-forget L2 is
  one of two options, neither of which is "the impl spawns its own
  thread":
  - **(a) Handler-level detach**: the handler does
    `let l2 = store.l2_handle(); compio::runtime::spawn(async move {
    spawn_blocking(move || l2.put(...)).await }).detach()`. The L2
    upload then lives as a compio Task — observable via
    `Runtime::shutdown_handle` (if compio ever grows one), cancelled
    on Drop if the handler chooses. This is the shape `admin_handlers.rs:1311-1324`
    already uses for the **other** detached teardown
    (`teardown_source_for_snapshot`).
  - **(b) `Tiered::put_async`**: split the trait into sync `put`
    (which does L1 only and returns immediately) and a separate
    `async fn upload_l2(meta)` that the handler awaits OR spawns at
    its own discretion. The trait keeps its "all methods blocking"
    invariant; the L2 background work becomes the handler's
    decision, not the impl's.
  The current shape (impl spawns a raw OS thread) is **option (c)**:
  trait contract intact in letter, broken in spirit. The handler now
  has **no handle to the L2 upload** — can't observe, can't cancel,
  can't wait. The C-3 fix's docstring at `:1086-1108` explicitly
  acknowledges *"The detached compio Task primitive bought us
  nothing here because we never await its handle"* — but the same
  argument applies to ANY async detach pattern: the issue isn't that
  the handle is unused, it's that **the layer that chose to
  fire-and-forget was wrong**. The store-impl ran into a sync/async
  mismatch and picked the lowest-friction escape (raw OS thread)
  instead of pushing the decision up to the handler.
- **Action**: pull the detach up one layer. Concretely:
  1. Change `TieredSnapshotStore::put` to do only L1 + return the
     meta. **Drop the std::thread::spawn entirely** (~40 LOC removed
     at `snapshot_store_gcs.rs:1080-1161`).
  2. Add `pub fn l2_handle(&self) -> Arc<L2>` (or change the existing
     `l2` field to be `pub(crate)`) so the handler can reach L2 for
     the fire-and-forget upload.
  3. In the handler, after `let meta = spawn_blocking(...put...)
     .await?;` — add `let l2 = store.l2_handle(); let sid = sid.clone();
     let path = meta.artifact_path.clone(); compio::runtime::spawn(async
     move { let _ = spawn_blocking(move || l2.put(&sid, &path, &ver))
     .await; }).detach();`.
  Net: ~−30 LOC in `snapshot_store_gcs.rs`, ~+15 LOC in
  `snapshot_handler.rs`. **Removes the layering inversion**. Closes
  the structural cost of C-3 (the patch only fixed the immediate
  panic; the layering smell remains). Also enables the doc-comment's
  promised retry-loop + metering at the right layer (the handler can
  add a shared `l2_upload_pending` Gauge that the impl can't).

### [R13-A3] The 2 jobspec builders are now provably independent of the env-mutex unify — R12-A1 is open, R10-A4 promotion path clearer (IMPORTANT, architecture-r13)

- **Files**:
  - Cold-boot builder: `crates/sandbox/src/backend/nomad_ch.rs:2311-2502`
    (`build_nomad_job_json_with`, ~191 LOC).
  - Wake-path builder: `crates/sandbox/src/restore_handler.rs:1442-1623`
    (`build_restore_nomad_job_json`, ~181 LOC).
  - Shared env-lock module: `crates/sandbox/src/backend/nomad_ch.rs:3440-3472`
    (`pub(crate) mod test_env_lock`, sibling of `mod tests`,
    27 LOC). Imported from `restore_handler.rs:2524` as
    `use crate::backend::nomad_ch::test_env_lock::with_task_driver_env;`.
- **Symptom**: r12-A1 framed the dual-builder duplication and the
  cross-module env-mutex race as a **bundled** problem with a
  bundled fix (approach (c): shared `build_nomad_job_json_for(req)`
  + the lock unifies into `jobspec.rs`). The R13-Q1 commit
  (`c5b9cb9d`) opted for **approach A** of just the env-mutex unify
  (lift `T7_ENV_LOCK` → `pub(crate)` sibling module, rename to
  `TASK_DRIVER_ENV_LOCK`, delete `R12_I1_ENV_LOCK`). This is a clean
  surgical fix to the test-side race but **leaves R12-A1 fully
  open**.

  The structural data point: the env-mutex unify took **88 lines
  added in nomad_ch.rs + 32 lines deleted in restore_handler.rs**
  (net +56 LOC across 2 files; commit message: *"approach A over B:
  the lock is conceptually tied to `task_driver_mode_from_env()` …
  Co-locating the lock and helper with the reader keeps the
  ownership story local to one file"*). The cost of the consolidation
  (R12-A1 approach (c)) is roughly **−330 LOC across the 2 builders
  + tests, +280 LOC for the shared helper**, per r12's estimate. The
  ratios are:

  | Fix | LOC churn | Surface unified | Surface still duplicated |
  |---|---:|---|---|
  | R13-Q1 (env-mutex only) | +56 | env mutex (1) | jobspec builder (2) · Env block (2) · match-mode block (2) · resources block (2) · driver-config block (2) · test fixtures (2) |
  | R12-A1 (full collapse) | −50 net | env mutex + everything above (7 surfaces) | 0 |

  **R12-A1 is roughly 100× the LOC-bang-for-buck of R13-Q1 by
  surface count**, but was deferred. Architecturally the env-mutex
  was the **least costly** part of the duplication; the actual
  consolidation lever was untouched.
- **Why important**: the env-mutex was the only piece that
  **synchronisation** required — the rest of the duplication is
  pure code shape. With the mutex unified, the consolidation case
  no longer carries any "but the lock"-flavoured complications:
  - Both builders now reference `TaskDriverMode` from the same
    canonical home (`crate::backend::nomad_ch::TaskDriverMode`).
  - Both tests now reference `test_env_lock::with_task_driver_env`
    from the same canonical home.
  - The remaining differences are **caller-supplied parameters**
    (pubkey_hex, sandbox_id form, memory_mb, restore_from) — exactly
    the shape that a `JobspecRequest` struct collapses into one
    builder.

  Concrete diff sketch for the R12-A1 collapse (now strictly
  cheaper because R13-Q1 unblocked the lock):

  ```rust
  // crates/sandbox/src/backend/nomad_ch/jobspec.rs  (NEW file)
  pub(crate) struct JobspecRequest<'a> {
      pub job_id: &'a str,
      pub vm_index: u16,
      pub mode: TaskDriverMode,
      pub kernel_dir: &'a Path,          // cfg.runtime_dir
      pub wrapper_path: &'a Path,        // cfg.wrapper_path (Raw)
      pub workspace_img: &'a Path,
      pub user_home_img: &'a Path,
      pub pubkey_hex: Option<&'a str>,   // None on restore
      pub sandbox_id: &'a str,           // .simple() form, no hyphens
      pub user_id: &'a str,
      pub project_id: Option<&'a str>,   // None on restore (Meta differs)
      pub memory_mb: u32,
      pub cpus_boot: u32,
      pub subnet_second_octet: u16,
      pub datacenter: &'a str,
      pub restore_from: Option<&'a Path>,
      pub meta_kind: &'static str,       // "create" | "restore"
  }

  pub(crate) fn build_nomad_job_json_for(
      req: &JobspecRequest<'_>,
  ) -> serde_json::Value { … one impl, ~210 LOC … }
  ```

  Cold-boot's `build_nomad_job_json` becomes a 25-LOC `JobspecRequest`
  constructor + 1 call. Wake-path's `build_restore_nomad_job_json`
  becomes the same shape. The 2 `match mode` blocks collapse to 1.
  The 2 Env blocks collapse to 1 (with `if req.restore_from.is_some()
  { env["ZSBX_RESTORE_FROM"] = … }` + `if let Some(hex) = req.pubkey_hex
  { env["ZSBX_PUBKEY_HEX"] = hex }`). The 2 Meta blocks collapse to 1
  (with `req.meta_kind` flowing through, and `req.project_id` being
  the only conditional field).

  **Cost ≈ -50 LOC net** (per r12); the same number stands today.
- **Action**: same as R12-A1 (kept open), with a clarified ordering:
  1. **R13-A3 is the diff-shape spec for R12-A1.** The
     `JobspecRequest` sketch above is the concrete value-object
     shape; the fixer can copy-paste it.
  2. **Land in the same PR as R10-A4** (split nomad_ch.rs into
     `backend/nomad_ch/{mod,jobspec,stop,…}.rs`). The natural home
     for the shared builder is `backend/nomad_ch/jobspec.rs`. The
     env-lock module (already a sibling) moves to
     `backend/nomad_ch/jobspec.rs` with the rest of the
     `TaskDriverMode` machinery.
  3. **Strictly cheaper than the r12 sketch** because the env-mutex
     unify already happened in r12→r13 — the new shared helper
     doesn't need to design around the lock-placement question.

### [R13-A4] C-3 fix's `std::thread::Builder::spawn` is correct as a patch but architecturally papers over R3-A2 (the trait-vs-trait split between SnapshotStore and RestoreBackend) (IMPORTANT, architecture-r13)

- **Files**:
  - C-3 fix: `crates/sandbox/src/snapshot_store_gcs.rs:1132`
    (`std::thread::Builder::new().name(...).spawn(...)`).
  - Trait contract reaffirmed (R13-A2 above):
    `snapshot_store.rs:91-96`.
  - Sibling pattern: `crates/sandbox/src/admin_handlers.rs:1310-1324`
    (`compio::runtime::spawn(async move { … }).detach()` for the
    teardown — the **right** detach pattern at the **right** layer).
- **Symptom**: this is the architecture-level read of R13-A2's
  layering inversion. Specifically, the C-3 fix landed at the
  **wrong layer** because:
  - The `SnapshotStore` trait is sync-by-contract (correct, mirrors
    `BlockStore`/`ObjectStore` conventions in similar codebases).
  - The handler hops through `spawn_blocking` (correct, mirrors the
    R5-P1b restore-side pattern at `cdd2e677`).
  - But the trait also needs a **fire-and-forget L2 background
    upload** for the tiered shape — which is fundamentally an async
    decision, not a sync one. The trait has no surface for it
    (intentionally — async would force the entire trait to be
    async).
  - The C-3 fix's choice was: keep the trait sync, push the
    "fire-and-forget async" inside the impl via raw `std::thread::spawn`.
    That works mechanically but breaks the "trait says sync, all
    async lives above" contract.

  The **right architectural read** is that the L2 fire-and-forget
  belongs in the same module that already does the same shape for
  the source-teardown: `admin_handlers.rs::admin_snapshot_handler`,
  which already calls `compio::runtime::spawn(async move {
  state.backend.teardown_source_for_snapshot(…).await; }).detach();`
  at `:1311-1324`. The pattern is correct; the C-3 fix just sat in
  the wrong file.
- **Why important**: this is a **structural-fingerprint cousin** of
  R3-A2 (RestoreBackend trait is a facade for NomadCHBackend) and
  R3-A1 (Backend enum 5-Err returners). All three findings share
  the same shape:
  - **A trait is declared as the abstraction boundary.**
  - **Code inside the trait impl reaches past the boundary** —
    R3-A1: delegate to enum, panic on wrong variant; R3-A2: trait
    methods are 1-line Arc<NomadCH> calls; **R13-A4: impl spawns
    its own background work to escape the sync contract**.

  The cumulative diagnosis is that the snapshot/restore module's
  **trait surfaces are too small for the operational concerns
  flowing through them**. Each individual trait method is fine in
  isolation; the lifecycle / detach / cancel / observability concerns
  that *cross* methods have no first-class representation, so impls
  cope with raw threads and Arc<NomadCH> handles.
- **Action**: the **immediate** action is R13-A2's pull-up (move the
  L2 detach to the handler). The **structural** action is to add a
  trait method on `SnapshotStore` that surfaces the fire-and-forget
  intent — something like:

  ```rust
  pub trait SnapshotStore: Send + Sync {
      // existing sync methods …

      /// Hint: this store supports a background L2 tier whose
      /// upload should be detached from the synchronous put().
      /// Returns `Some(handle)` for tiered stores; `None` for
      /// single-tier (LocalDisk, GCS direct).
      ///
      /// The handler uses the handle to issue the fire-and-forget
      /// upload on its own runtime; the impl does NOT spawn.
      fn l2_handle(&self) -> Option<Arc<dyn SnapshotStore>> { None }
  }
  ```

  With that, `TieredSnapshotStore::put` becomes 5 lines (L1 put +
  return), `TieredSnapshotStore::l2_handle` returns `Some(self.l2.clone())`,
  and the handler's spawn-and-detach lives in `snapshot_handler.rs`
  where the existing `compio::runtime::spawn_blocking` pattern is
  already established. The other 2 impls (LocalDisk, Gcs) inherit
  the default `None` → no L2 detach concern.

  Closes the C-3 layering smell **structurally** instead of
  patch-style. Estimated cost: +15 LOC trait surface, −30 LOC impl,
  +15 LOC handler. Net **−0 LOC**, but the layering is correct.

### [R13-A5] C-4 fix's `VmIndexRetryPolicy` introduces a 9th `RestoreBackend` trait method — trait surface keeps growing without R10-A2's `SnapshotCapableBackend` consolidation (MINOR, architecture-r13)

- **Files**:
  - New trait method: `crates/sandbox/src/restore_handler.rs:243-251`
    (`fn vm_index_retry_policy(&self) -> VmIndexRetryPolicy { … }`,
    default impl returns `VmIndexRetryPolicy::default()`).
  - C-4 fix's new helper (uncommitted): `restore_handler.rs:254-306`
    (`pub(crate) async fn reserve_vm_index_with_retry`).
  - StubRestoreBackend gains 3 fields (`vm_index_retry_policy`,
    `reserve_succeeds_on_attempt`, `reserve_attempts`) at
    `:851-862` (uncommitted).
- **Symptom**: the `RestoreBackend` trait was 8 methods at r12.
  C-4's fix adds a 9th (with a default impl, which softens the
  blow), plus a new `VmIndexRetryPolicy` value type. The trait
  growth rate is **+1 method per ~4 cycles** (B19 added
  `register_restored`, B22 added `derive_agent_url`, C-4 adds
  `vm_index_retry_policy`). Counting on the 2-impl population
  (StubRestoreBackend, RealRestoreBackend) means **every new method
  costs 2 implementations + 1 trait declaration + 1 default-impl
  decision**.

  Three of the 9 methods are now one-line delegations through
  `Arc<NomadCHBackend>` (`register_restored`, `derive_agent_url`,
  and the new `vm_index_retry_policy` — which on `RealRestoreBackend`
  will most likely be 1 line returning a const from `cfg`). The
  trait is a **continually growing facade** for `NomadCHBackend`,
  exactly the R3-A2 / R10-A2 carry-forward.
- **Why minor**: this is a sibling of the existing R3-A2 / R10-A2
  finding — no new structural issue, just one more datapoint on the
  trend. The C-4 fix's choice to add a trait method (rather than
  hardcode a const in the handler) is the **right call for testing
  ergonomics** (the stub overrides it to make tests fast). The
  architectural pressure is that the trait's purpose has drifted
  from *"abstract the production backend so the handler can be
  unit-tested"* to *"hold every per-call knob the handler reads"*.
- **Action**:
  1. **Land R10-A2** (`SnapshotCapableBackend` trait that subsumes
     `RestoreBackend` AND the snapshot-side methods currently on
     `Backend::teardown_source_for_snapshot`). With one trait, the
     facade-vs-impl distinction collapses: `NomadCHBackend`
     implements both surfaces, the stub implements both with
     test-friendly defaults, and the handler holds an
     `Arc<dyn SnapshotCapableBackend>`.
  2. **Defer until then**; the C-4 fix's trait-method addition is
     the right local choice. The structural fix is the R10-A2 PR,
     not blocking C-4.

### [R13-A6] R12-A3 (TaskDriverMode as backend struct field) is now mechanically cheaper after R13-Q1 — propose a sub-PR ahead of R12-A1 (MINOR, architecture-r13)

- **Files**:
  - Current env reader (still): `crates/sandbox/src/backend/nomad_ch.rs:2301`
    (`task_driver_mode_from_env()` called from
    `build_nomad_job_json`).
  - Current env reader (wake): `crates/sandbox/src/restore_handler.rs`
    (called from `submit_restore_job`).
  - Test env-lock: `crates/sandbox/src/backend/nomad_ch.rs:3440-3472`
    (the shared `test_env_lock` module from R13-Q1).
- **Symptom**: r12-A3 proposed making `TaskDriverMode` a backend
  struct field (read env once at boot, never again). r12 reasoned
  this is *"the same architectural shape as `SandboxConfig::from_env`
  at `config.rs:340-540`"* and would subsume R12-M2 (concurrency
  finding on libstd env mutex pressure).

  The R13-Q1 env-mutex unify makes R12-A3 **trivially cheaper**:
  with the lock co-located with `task_driver_mode_from_env()` (the
  canonical reader), promoting `TaskDriverMode` to a struct field
  is a **3-step mechanical change**:
  1. Add `pub(crate) task_driver_mode: TaskDriverMode` field to
     `NomadCHBackend` and `RealRestoreBackend`.
  2. Populate from `task_driver_mode_from_env()` in
     `AppState::from_config` (the one place that builds both).
  3. Delete the production env-read at `nomad_ch.rs:2301` and
     `restore_handler.rs:1116`; pass `self.task_driver_mode`
     directly to `build_nomad_job_json_with`.

  The test code keeps using `with_task_driver_env` because tests
  *want* to mutate the env — but the **production code path becomes
  env-mutex-free**. Both backends become explicitly constructed
  rather than implicitly env-driven.
- **Why minor**: cosmetic improvement on r12-A3's existing
  recommendation. The architectural argument is unchanged; the cost
  just dropped.
- **Action**: land R12-A3 as a **3-line struct-field-add PR** in
  isolation, ahead of R12-A1 (the dual-builder collapse). With
  `TaskDriverMode` as a field on both backend structs, R12-A1's
  `JobspecRequest::mode` field becomes a `self.task_driver_mode`
  read at construction instead of needing to flow through builders
  + env-read. Closes R12-M2 (concurrency); reduces R12-A1's
  diff-size by ~30 LOC; makes R10-A4's split trivially
  pass-through.

## C-3 fix architectural review

**The fixer chose `std::thread::Builder::spawn` over
`compio::runtime::spawn_blocking`.** Verified at `c890c015`
(`snapshot_store_gcs.rs:1125-1161`).

**Was this the right layer?**

Net: **mechanically correct, architecturally wrong**.

| Axis | C-3 fix (`std::thread::Builder::spawn`) | Right layer (handler-level detach) |
|---|---|---|
| Stops the panic | Yes — std::thread has no compio TLS dependency | Yes — handler-level spawn lives in async context |
| Trait contract preserved | **In letter** (sync methods all sync) | **In letter and spirit** (impl spawns nothing) |
| Cancellable on shutdown | No (raw OS thread, untracked) | Yes (compio Task; can be tracked via shutdown_handle) |
| Observable for retry/metric scaffold (the docstring's TODO) | No (no handle to add a shared Gauge against) | Yes (handler owns the upload future) |
| Sibling pattern alignment | Diverges from `admin_handlers.rs:1311-1324` (the existing fire-and-forget pattern uses `compio::runtime::spawn`) | Matches it |
| LOC | +35 (the spawn block) | +15 in handler, −40 in impl (net **−25 LOC**) |

**Could the fix have lived in `snapshot_handler.rs:392-407` (the
spawn_blocking call site)?**

Yes. Concrete sketch:

```rust
// snapshot_handler.rs, replacing lines 392-407
let meta = {
    let store_clone = Arc::clone(&store);  // Note: store, not l1
    let sid_clone = sandbox_id_typed.clone();
    let temp_dir_owned = temp_dir.to_path_buf();
    let ch_version = ch.version().to_string();
    // L1 put — sync, fast (~100ms for sha256 + rename).
    let meta = compio::runtime::spawn_blocking(move || {
        // Direct L1 access — store's `l1` field needs to become
        // `pub(crate)` OR `store` becomes `&TieredSnapshotStore`.
        store_clone.l1_put(&sid_clone, &temp_dir_owned, &ch_version)
    })
    .await
    .unwrap_or_else(/* panic_handler */)
    .map_err(SnapshotHandlerError::Store)?;

    // L2 detach — fire-and-forget, same pattern as
    // admin_handlers.rs:1311-1324's teardown_source detach.
    if let Some(l2) = store.l2_handle() {
        let sid = sandbox_id_typed.clone();
        let path = PathBuf::from(meta.artifact_path.clone());
        let ver = ch.version().to_string();
        compio::runtime::spawn(async move {
            let _ = compio::runtime::spawn_blocking(move || {
                l2.put(&sid, &path, &ver)
            }).await;
        }).detach();
    }
    meta
};
```

This adds 12 LOC to the handler, removes ~40 LOC from
`snapshot_store_gcs.rs` (the entire C-3 fix block at 1080-1161),
and preserves both the trait's sync contract AND the
spawn-and-detach pattern the codebase already uses.

**Recommendation**: track R13-A2 (the layering pull-up) as a
follow-up PR. The C-3 fix itself is ship-as-is — flipping to
handler-level detach would re-test a bunch of cluster-validated
behaviour for no immediate cluster-blocking benefit. **But the next
person to touch this code path** (most likely: the GCS PR adding
the retry loop the docstring promises) **should pull the detach up
first**, otherwise the retry + metering scaffold lives on a raw OS
thread with no compio observability.

## C-4 fix architectural assessment (in-flight, working tree)

**Status**: scaffolding committed-in-spirit but the call site is
unwired. Specifically:

- **Committed-shape**: `VmIndexRetryPolicy` struct at
  `restore_handler.rs:128-145`; trait default method
  `vm_index_retry_policy()` at `:243-251`; helper
  `reserve_vm_index_with_retry` at `:265-306`; `StubRestoreBackend`
  test fields at `:851-862`.
- **NOT-yet-wired**: `do_restore_inner` at line 442-444 still calls
  `backend.reserve_vm_index(snap.vm_index)` directly without using
  the new helper. The fix only takes effect once that 3-line edit
  lands.

**Mechanical-vs-design**: the **design** call (caller-side bounded
retry, the cluster-r5 reviewer's option (d) shape — same defaults:
60 × 2s = ~120s budget) is **correct**. The cluster-r5 reviewer
considered (a) block snapshot, (b) cross-slot fallback, (c) drop
fence — and ranked (c) most surgical. The C-4 fix chose (d) caller
retry instead, with docstring justification at `:113-123` arguing
that **(c) reopens FM-F race** (host_fence is the primary defense
against handing a live IP to a new tenant; releasing the slot
before fence-clear creates a real correctness gap). That argument
is correct — option (c) was wrong.

**Why design is fine**: the retry is **safe and cheap**:
- Safe: `reserve_vm_index` is a single mutex lock against
  `VmIndexAllocator`; retrying is idempotent. The detached
  teardown's `release()` (`nomad_ch.rs:1138`) is the **only** thing
  that ever frees this specific slot — there's no third-party
  contention. Eventually the retry succeeds.
- Cheap: 60 × 2s = 120s wall-clock worst-case is well below the
  wake-handler's overall budget (which is dominated by
  `wait_for_livez` polling ~30s + post-CH bootstrap ~5s). Adding
  ~90s of retry-wait to the wake response p99 is acceptable given
  the alternative is a 503.

**Why mechanical**: the **plumbing** is incomplete. `do_restore_inner`
ignores the new helper. The fix's commit (when it lands) will be a
3-line change: delete the `backend.reserve_vm_index(...)` map_err
line at 442-444, replace with `reserve_vm_index_with_retry(&*backend,
sandbox_id, snap.vm_index).await?`.

**Underlying design issue (per the review brief's prompt #2)**: the
**underlying** issue is **R4-A2** (LeasedVmSlot RAII), open for 11+
cycles. Specifically:

The C-4 race exists because:
1. The snapshot endpoint **detaches** `teardown_source_for_snapshot`
   (`admin_handlers.rs:1311-1324`) and returns 200 BEFORE the slot is
   released.
2. The detached teardown holds the slot for ~90s (host_fence ~60s +
   Nomad purge ~30s; `nomad_ch.rs:1086-1143`).
3. The wake handler tries to reserve the same slot synchronously.
4. The two cross-thread without any explicit handoff — the
   teardown's `release()` is the **only** signal.

**R4-A2's `LeasedVmSlot` RAII would change this from "poll for
state-machine ready" to "wait for a single-shot handoff signal"** —
the slot would be encapsulated in a guard that `release()`s on
Drop, and the handoff would be an `Arc<tokio::sync::Notify>`-style
wakeup (compio equivalent) rather than a polling loop. That's the
**structural** fix; C-4's retry loop is the **operational** fix.

**Should C-4's design be revisited in light of R4-A2?**

No. The retry loop is a strictly-cheaper interim that closes the
cluster blocker (which is 503-on-wake, not slot-leak-on-failure).
R4-A2's RAII is a follow-on; the retry loop becomes a 1-iteration
no-op once R4-A2 lands.

## Phase B Maturity assessment (per review brief's prompt #3)

**5 cluster smoke cycles, 4 distinct bugs (C-1, C-2, C-3, C-4), one
per cycle, structurally similar**. Architecturally:

| Cycle | Bug | Root cause shape | Structural sibling |
|---|---|---|---|
| r1 | C-1 (driver-decode wrong) | Codec mismatch, controller vs Go driver | T-3 / T-4 / T-5 (the typed Config evolution work) |
| r2 | C-2 (stale L1 cache) | Local-disk consistency contract | R5-P1b (the existing L1 round-trip work) |
| r3 | C-3 (panic on spawn_blocking-in-spawn_blocking) | Sync/async boundary | R13-A2 / R13-A4 (this round) |
| r4 | C-4 (vm_index race) | Detach/handoff lifecycle | R4-A2 (LeasedVmSlot RAII) |
| r5 | C-5 (GCS scope 403) | Operator config (out of arch scope) | (script) |

**Wake path (C-4) is the last layer**: cluster cycles r1-r3
attacked Boot+Snapshot; r4 was the first time the wake path was
properly reached end-to-end (because r1-r3's bugs all aborted
before wake). r4 found C-4, which is itself the wake path's
**first** integration-class issue. The pattern is: **each cluster
cycle pushes the failure frontier one stage deeper**.

**What's left architecturally?**

The cluster review's prompt #3 asks "Is the test-coverage gap (R13-T2
5-round StubRestoreBackend untested) the structural issue blocking
acceptance?"

**Yes — and r13 elevates this to R13-A1 (CRITICAL)**. The
StubRestoreBackend exists, is fully fleshed out, has fields for the
exact behaviors the cluster cycles are surfacing (
`reserve_succeeds_on_attempt` for C-4, the `fail_*` fields for C-3-
adjacent failure modes), and is **never used** to drive
`restore_sandbox`. Every C-1 through C-4 would have been caught by
a 30-LOC `#[compio::test] async fn restore_happy_path()` calling
`restore_sandbox(&db, store, Arc::new(stub), None, id, true).await`
with the right stub config.

**Acceptance criteria for "Phase B is structurally done"**:
1. **R13-A1 closed**: at least 6 `restore_sandbox` end-to-end tests
   driving the stub. (~250 LOC.)
2. **R13-A2 closed**: L2 detach pulled up to handler. (~15 LOC delta.)
3. **C-4 fix wired** (the 3-line `do_restore_inner` edit).
4. **Cluster r6 GREEN** (a SINGLE green smoke cycle is sufficient
   acceptance signal once the above 3 land; the cluster trajectory
   has been "one new bug per cycle", and r13's recommendation says
   the underlying issue is local-test coverage — closing that issue
   means the next cluster cycle is the first one not pre-doomed to
   discover another integration bug).

## Module sizes — post-r13 (per review brief's prompt #4)

- `restore_handler.rs`: **2662** (down 18 LOC due to R13-Q1's
  env-lock helper deletion). If the working-tree C-4 fix commits
  unmodified, it becomes ~2840 LOC (NEW threshold; was 2680 → 2662
  → 2840 over two cycles).
- `nomad_ch.rs`: **5399** (up 28 LOC from R13-Q1's env-lock module
  promotion). **Still the only file > 5000 LOC.**
- `snapshot_store_gcs.rs`: **1761** (up 98 LOC from C-3 fix + its
  doc-comment + regression test).
- `db.rs`: **3303** (unchanged ±5).
- `lib.rs`: **2412** (unchanged).

Sum of the two TaskDriverMode-bearing files:
`nomad_ch + restore_handler = 5399 + 2662 = 8061 LOC` (vs r12's
8051 — net stable; the shape of the duplication is unchanged).

## `build_nomad_job_json` consolidation post-R13-Q1 (per review brief's prompt #5)

**The 2 builders are byte-for-byte duplicates with one fn argument
difference** — per R12-A1's claim. r13 verifies this on the current
file content and sketches the consolidation now that the env-mutex
is unified.

**Diff between the `match mode` blocks** (cold-boot:
`nomad_ch.rs:2402-2464`; wake-path: `restore_handler.rs:1537-1585`):

| Line | Cold-boot | Wake-path | Type |
|---|---|---|---|
| RawExec match arm: `"command": …` | `cfg.nomad_ch.wrapper_path` | `cfg.wrapper_path` | Config-field path difference (SandboxConfig contains NomadCHConfig as a sub-field; restore-side holds NomadCHConfig directly). Trivial: pass the NomadCHConfig sub-ref. |
| ChPlugin's `kernel_path` | `cfg.nomad_ch.runtime_dir.join("vmlinuz")` | `cfg.runtime_dir.join("vmlinuz")` | Same. |
| `cpus`: | `cpus_boot(cfg.cpus)` | `cpus_boot` (local var u32) | Argument value vs computed-from-cfg. Move computation to caller, pass `cpus_boot: u32` arg. |
| `memory_mb`: | `cfg.memory_mb as u32` | `memory_mb` (arg) | Same — caller computes, passes u32. |
| `restore_from`: | `restore_str` (computed from `Option<&Path>`) | `alloc_dir.display().to_string()` | Caller passes `restore_from: Option<&Path>`; one impl-side `.map().unwrap_or_default()`. |
| `sandbox_id`: | `sandbox_id` (&str, already in .simple() form) | `sandbox_id.simple().to_string()` | Trivial — caller normalizes to .simple() string. |
| `pubkey_hex`: | `pubkey_hex` (&str arg) | `""` (literal) | Caller passes `Option<&str>` → `unwrap_or("")`. |

**Diff between the Env blocks** (cold-boot: `nomad_ch.rs:2334-2374`;
wake-path: `restore_handler.rs:1500-1515`):

| Key | Cold-boot | Wake-path | Type |
|---|---|---|---|
| `ZSBX_PUBKEY_HEX` | present | **absent** | Conditional on `pubkey_hex.is_some()`. |
| `ZSBX_SANDBOX_ID` | present | **absent** | Conditional on `meta_kind == "create"` OR always-present (the cold-boot wrapper validates it; restore wrapper does too — could just always emit it. **R13 verification reading**: `nomad-vm-wrapper.sh:153` validator runs for both branches per the wrapper's pre-branch check at line 222. Adding ZSBX_SANDBOX_ID to restore is harmless and probably correct.) |
| `ZSBX_RESTORE_FROM` | **absent** | present | Conditional on `restore_from.is_some()`. |
| Other 7 keys (`ZSBX_VM_INDEX`, `ZSBX_ARTIFACT_DIR`, `ZSBX_RUNTIME`, `ZSBX_WORKSPACE_IMG`, `ZSBX_USER_HOME_IMG`, `ZSBX_VM_MEMORY_MB`, `ZSBX_VM_CPUS_BOOT`, `ZSBX_SUBNET_BASE_OCTET`) | identical | identical | Move to shared builder, pure code dedup. |

**Diff between the Meta blocks** (cold-boot:
`nomad_ch.rs:2472-2477`; wake-path: `restore_handler.rs:1593-1598`):

| Key | Cold-boot | Wake-path | Type |
|---|---|---|---|
| `zeroship.user` | `user_id` | `user_id` | Same. |
| `zeroship.project` | `project_id` | **absent** | Conditional on `Option<&str>` arg. |
| `zeroship.sandbox` | `sandbox_id` (simple) | `sandbox_id.to_string()` (hyphenated UUID) | **Inconsistent** — cold-boot uses .simple(), wake-path uses default Uuid Display (hyphenated). R13 flag: this may be a latent bug; the wake path's Meta carries a different sandbox_id format than the cold-boot one. Operationally this only affects Nomad-side observability (Meta is informational only). |
| `zeroship.vm_index` | `vm_index.to_string()` | `vm_index.to_string()` | Same. |
| `zeroship.kind` | **absent** | `"restore"` | Conditional on `meta_kind` arg. |

**Diff between the Resources blocks** (cold-boot:
`nomad_ch.rs:2382-2397`; wake-path: `restore_handler.rs:1517-1529`):

| Key | Cold-boot | Wake-path | Type |
|---|---|---|---|
| `CPU` | `NOMAD_CPU_MHZ_ADVISORY` (= 500 per `nomad_ch.rs:201`) | **hardcoded `500`** | Same value, different reference. Replace with the named const. |
| `MemoryMB` | `cfg.memory_mb as u32` | `memory_mb` | Same — caller passes u32. |
| `MemoryMaxMB` | `(cfg.memory_mb * 2) as u32` | `memory_mb * 2` | Same. |

**Consolidation diff sketch** (concrete; ready for a PR):

```rust
// crates/sandbox/src/backend/nomad_ch/jobspec.rs (NEW; ~210 LOC)

use std::path::Path;
use serde_json::Value;
use super::{NomadCHConfig, TaskDriverMode, cpus_boot, NOMAD_CPU_MHZ_ADVISORY};

pub(crate) struct JobspecRequest<'a> {
    pub job_id: &'a str,
    pub nomad_cfg: &'a NomadCHConfig,
    pub vm_index: u16,
    pub workspace_img: &'a Path,
    pub user_home_img: &'a Path,
    pub sandbox_id: &'a str,  // .simple() form (32 hex, no hyphens)
    pub user_id: &'a str,
    pub memory_mb: u32,
    pub cpus_boot: u32,
    pub mode: TaskDriverMode,
    // Cold-boot-only:
    pub pubkey_hex: Option<&'a str>,
    pub project_id: Option<&'a str>,
    // Wake-only:
    pub restore_from: Option<&'a Path>,
    pub meta_kind: &'static str,  // "create" | "restore"
}

pub(crate) fn build_nomad_job_json_for(req: &JobspecRequest<'_>) -> Value {
    // Env block — 9 always-present keys + 3 conditionals
    let mut env = serde_json::json!({
        "ZSBX_VM_INDEX": req.vm_index.to_string(),
        "ZSBX_ARTIFACT_DIR": req.nomad_cfg.runtime_dir.display().to_string(),
        "ZSBX_RUNTIME": "${NOMAD_TASK_DIR}",
        "ZSBX_WORKSPACE_IMG": req.workspace_img.display().to_string(),
        "ZSBX_USER_HOME_IMG": req.user_home_img.display().to_string(),
        "ZSBX_VM_MEMORY_MB": req.memory_mb.to_string(),
        "ZSBX_VM_CPUS_BOOT": req.cpus_boot.to_string(),
        "ZSBX_SUBNET_BASE_OCTET": req.nomad_cfg.subnet_second_octet.to_string(),
        "ZSBX_SANDBOX_ID": req.sandbox_id,
    });
    if let Some(hex) = req.pubkey_hex {
        env["ZSBX_PUBKEY_HEX"] = Value::String(hex.into());
    }
    if let Some(p) = req.restore_from {
        env["ZSBX_RESTORE_FROM"] = Value::String(p.display().to_string());
    }

    let resources = serde_json::json!({
        "CPU": NOMAD_CPU_MHZ_ADVISORY,
        "MemoryMB": req.memory_mb,
        "MemoryMaxMB": req.memory_mb * 2,
    });

    // Driver + Config — collapse the 2 match blocks into 1
    let (driver_name, config): (&str, Value) = match req.mode {
        TaskDriverMode::RawExec => (
            "raw_exec",
            serde_json::json!({
                "command": req.nomad_cfg.wrapper_path.display().to_string(),
            }),
        ),
        TaskDriverMode::ChPlugin => {
            let kernel_path = req.nomad_cfg.runtime_dir.join("vmlinuz");
            let restore_str = req.restore_from
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            (
                "ch",
                serde_json::json!({
                    "vm_index": req.vm_index,
                    "kernel": kernel_path.display().to_string(),
                    "cpus": req.cpus_boot,
                    "memory_mb": req.memory_mb,
                    "restore_from": restore_str,
                    "sandbox_id": req.sandbox_id,
                    "workspace_img": req.workspace_img.display().to_string(),
                    "user_home_img": req.user_home_img.display().to_string(),
                    "pubkey_hex": req.pubkey_hex.unwrap_or(""),
                    "subnet_base_octet": req.nomad_cfg.subnet_second_octet,
                    "disks": [],
                    "fs": [],
                    "net": [],
                }),
            )
        }
    };

    // Meta — 3 always, 2 conditional
    let mut meta = serde_json::json!({
        "zeroship.user": req.user_id,
        "zeroship.sandbox": req.sandbox_id,  // always .simple() form
        "zeroship.vm_index": req.vm_index.to_string(),
    });
    if let Some(pid) = req.project_id {
        meta["zeroship.project"] = Value::String(pid.into());
    }
    if req.meta_kind == "restore" {
        meta["zeroship.kind"] = Value::String("restore".into());
    }

    serde_json::json!({
        "Job": {
            "ID": req.job_id,
            "Name": req.job_id,
            "Type": "service",
            "Datacenters": [req.nomad_cfg.datacenter],
            "Meta": meta,
            "TaskGroups": [{
                "Name": "vm",
                "Count": 1,
                "RestartPolicy": {
                    "Attempts": 0, "Mode": "fail",
                    "Interval": 30_000_000_000u64, "Delay": 5_000_000_000u64,
                },
                "ReschedulePolicy": { "Attempts": 0, "Unlimited": false },
                "Tasks": [{
                    "Name": "ch", "Driver": driver_name, "Config": config,
                    "Env": env, "Resources": resources,
                    "KillTimeout": 10_000_000_000u64,
                }],
            }],
        }
    })
}
```

**Call-site impact**:
- Cold-boot `build_nomad_job_json` (`nomad_ch.rs:2279-2303`): becomes
  a 25-LOC `JobspecRequest` constructor + 1 call. **Deletes lines
  2305-2502 entirely (~197 LOC)**.
- Wake-path `build_restore_nomad_job_json` (`restore_handler.rs:1441-1623`):
  becomes a 25-LOC `JobspecRequest` constructor + 1 call. **Deletes
  lines 1486-1622 entirely (~137 LOC)**.
- Tests: cold-boot tests at `nomad_ch.rs:4099-4327` (~228 LOC) and
  wake-path tests at `restore_handler.rs:2451-2680` (~229 LOC)
  share the same `with_task_driver_env` helper post-R13-Q1. They
  can be consolidated into ~12 jobspec tests against the shared
  builder + 4 per-call-site smoke tests (~120 LOC) — net **−337
  LOC of tests removed**.

**Net consolidation budget**:
- New: `jobspec.rs` ~210 LOC (shared builder).
- Removed: 197 + 137 + 337 = **−671 LOC across 3 files**.
- **Net: −461 LOC across the crate**.

**Cleanup of the inconsistency caught above**: the
`zeroship.sandbox` Meta key drift (cold-boot uses .simple, wake
uses hyphenated UUID) gets fixed for free — the shared builder
emits the .simple() form for both. If anything is depending on the
hyphenated form for restore-path observability, it surfaces as a
failing test under the consolidation PR — better than the current
"latent drift, nobody noticed because no test pinned it".

## Carry-forward (still open from earlier rounds)

- **[R4-A2 / R5-A2]** LeasedVmSlot RAII guard — **12th cycle**, no
  movement. **Now the underlying structural issue for C-4** (per
  the C-4 architectural assessment above). The C-4 retry loop is
  the operational interim; RAII is the structural fix.
- **[R3-A1 / R5-A1 / R10-A3]** `Backend` enum 5-Err-returner split
  — count at HEAD = **5** (unchanged). Still CRITICAL.
- **[R3-A2 / R10-A2]** `RestoreBackend` trait facade — **9 methods**
  at HEAD (was 8 at r12; the C-4 fix's `vm_index_retry_policy`
  brings it to 9 once committed). 3 of the 9 are one-line
  delegations through `Arc<NomadCH>`. Trait surface continues to
  grow without consolidation.
- **[R3-A3]** wrapper bash → Rust sidecar — subsumed by R11-A2 /
  R12-A4 (the wrapper is going away entirely at T-8).
- **[R3-A4]** `StopDisposition` enum — still
  `stop_inner(.., bool)` at `nomad_ch.rs:986-989`. Lands cheaply
  with R10-A4's split.
- **[R4-A1 / R10-A6 / R11-A3]** AppState builder accretion — 10th
  cycle, **unchanged at 7 `with_*` + `new_fixture`**.
- **[R10-A4 / R11-A2 / R12-A4]** nomad_ch.rs at 5399 LOC, un-split.
  Still a T-8 prerequisite.
- **[R12-A1]** Dual builder consolidation — R13-Q1 unified the
  env-mutex but the **structural duplication remains**. R13-A3
  provides the concrete diff-shape sketch.
- **[R10-A1 / R11-A4 / R12-A5]** db.rs at 3303 LOC carrying the
  recovery CAS + the host_id reader. Un-extracted.
- **[R11-A1 / R11-Q2 / R12-A2]** root-owned-secret-file 5-site
  duplication. No movement. (No new sites this cycle — count
  stable at 5.)
- **[T9 / T10 / R10-A7]** ControllerIdleSnapshotter duplicates
  admin_handlers' 70-LOC orchestration — unchanged.
- **[r9 C3]** AEAD fail-OPEN on GCS path — still at
  `lib.rs:654-666`, no boot-gate.

## Closed by recent commits

- **R13-Q1 / R13-C1** (`SANDBOX_TASK_DRIVER` env-mutex unification) —
  CLOSED at `c5b9cb9d`. Approach A (lift T7_ENV_LOCK → `pub(crate)`
  sibling module `nomad_ch::test_env_lock`, rename to
  `TASK_DRIVER_ENV_LOCK`, delete the duplicate `R12_I1_ENV_LOCK`).
  **Architecture impact**: closed the cross-module test-side env
  race; **left R12-A1's structural dual-builder duplication
  open**. Net +56 LOC across nomad_ch + restore_handler. The lock
  is now co-located with its canonical reader.
- **C-3** (Tiered::put spawn_blocking-inside-spawn_blocking panic) —
  CLOSED at `c890c015`. **Architecture impact**: papered over with
  `std::thread::Builder::spawn` — correct as a patch, layering
  inversion as a shape (see R13-A2 + R13-A4 above). Net +98 LOC in
  `snapshot_store_gcs.rs` (fix + regression test + ~25 LOC of
  architectural justification doc-comment).
- **R10-API5 / R10-API6** (sandbox_id wire form doc-comment fix) —
  CLOSED at `8ed9aa90`. Doc-only; no architecture impact.

None of these closed an architecture-flagship finding. The cycle's
net structural movement is: **−1 cross-module test-side env race
(R13-Q1), +0 structural collapse**.

---

## What's structurally new vs. r12

| Item | r12 state | r13 state | Δ |
|---|---|---|---|
| `RestoreBackend` trait methods | 8 | **8 committed + 1 in-flight (C-4)** | +0 / +1 |
| `RealRestoreBackend` Arc-NomadCH fields | 2 | **2** | 0 |
| `Backend` enum Err-returners | 5 | **5** | 0 |
| `NomadCHBackend` pub methods | 29 | **29** | 0 |
| `db.rs` LOC | 3298 | **3303** | +5 |
| `with_*` builders | 7 | **7** | 0 |
| `pub fn new_fixture` | 2 | **2** | 0 |
| `restore_handler.rs` LOC | 2680 | **2662** | **−18** (R13-Q1 helper deletion) |
| `nomad_ch.rs` LOC | 5371 | **5399** | **+28** (R13-Q1 module promotion) |
| `snapshot_store_gcs.rs` LOC | 1663 | **1761** | **+98** (C-3 fix) |
| Root-owned-secret-file loader sites | 5 | **5** | 0 |
| Modules with `TaskDriverMode` match arms | 2 | **2** | 0 |
| Modules with `SANDBOX_TASK_DRIVER` env-mutex | 2 (unsynchronised) | **1 (unified)** | **−1 race** |
| Logical jobspec-builder functions | 2 | **2** | 0 |
| `Tiered::put` background-detach shape | `compio::runtime::spawn_blocking` (broken) | `std::thread::Builder::spawn` (layering inversion) | structural-shape change |
| Files > 5000 LOC | 1 | **1** | 0 |
| Files > 2500 LOC | 3 | **3** | 0 |
| `StubRestoreBackend` callers driving `restore_sandbox` | **0** | **0** | 0 (R13-A1) |
| `restore_sandbox` end-to-end unit-tests | **0** | **0** | 0 (R13-A1) |
| Cluster cycles since R12 | 1 (r5 FAIL on WAKE) | — | — |

The single structural improvement this cycle (R13-Q1's env-mutex
unify) **closed a test-side race but did not move any of the 6
flagship carry-forwards** (R4-A2, R10-A3, R10-A4, R10-A1, R11-A1,
R4-A1). The C-3 fix added 98 LOC of bug-fix + regression-test +
architectural justification but left the **layering** unchanged
(R13-A2). The C-4 fix scaffolding (uncommitted) adds 178 LOC of
trait method + helper + stub field set but is **not yet wired** —
the production caller still uses the un-retried direct reserve.

## Recommended order of attack (updated; 7 PRs)

Updated from r12 with one prepend (R13-A1) and one elevation
(R13-A2):

1. **R13-A1 `restore_handler::tests::driven` module** — 6-8
   end-to-end `restore_sandbox` tests against `StubRestoreBackend`
   (~250 LOC). **Closes 4 carry-forwards** (T8, T9, R3-T3, R13-T2)
   and creates a regression net for C-1 through C-4 + future
   cluster bugs. **PR #1 because this is the structural lever for
   shortening the cluster-cycle bug-discovery loop.**
2. **R13-A2 L2 detach pull-up** — move `Tiered::put`'s `std::thread::
   spawn` out into `snapshot_handler.rs`'s spawn-and-detach
   pattern. ~−25 LOC net; closes the layering inversion. Optional
   stronger form: R13-A4's `l2_handle()` trait method.
3. **R11-A1 / R12-A2 `secret_io::read_root_owned_secret_file`
   extraction** — now mandatory after the 5th site (unchanged from
   r12 PR #1).
4. **R10-A1 / R11-A4 / R12-A5 db.rs → recovery.rs** — unchanged
   from r12 PR #2.
5. **R10-A7 snapshot orchestrator extraction** — unchanged from
   r12 PR #3.
6. **R12-A3 TaskDriverMode struct field** — 3-line struct-field-add
   per R13-A6. Closes R12-M2 + reduces R12-A1's diff-size by
   ~30 LOC. **Land before #7**.
7. **R10-A4 + R12-A1 + R12-A4 together** — nomad_ch.rs split + R12-A1
   jobspec-builder collapse (per R13-A3's concrete diff sketch).
   Estimated cost: ~−461 LOC net across the crate. **MUST land
   before T-8b-cutover**.
8. **R10-A3 + R10-A2 + R4-A2 (LeasedVmSlot) together** — unchanged
   from r12 PR #5. Closes the structural fix for C-4 (the retry
   loop becomes a 1-iteration no-op once RAII lands).
9. **R4-A1 / R11-A3 / R10-A6 AppStateBuilder** — unchanged from r12
   PR #6.

Total: **8 PRs** (was 6 in r12; +1 for R13-A1, +1 for R13-A2).

**The critical insertion is PR #1 (R13-A1)** — it changes every
subsequent PR's risk profile by giving them a fast-feedback test
shell that catches integration-class issues before cluster
validation. Estimated cost ~250 LOC of tests; closes 4 long-tail
carry-forwards as a side-effect.

Closes 8 carry-forwards + 6 r13-new findings + creates the
regression net under the C-4 fix.

PR #1 in PR-order, but in **cycle-order** it should be:
- **First**: PR #1 (R13-A1) + the C-4 fix's 3-line plumbing edit at
  `do_restore_inner:442-444` + the C-4 fix's own committed unit-test
  exercising the new `reserve_succeeds_on_attempt` stub field. Land
  as a single PR.
- **Then**: cluster cycle r6 to validate the C-4 fix on a hot path.
- **Then**: PR #2 (R13-A2) + PR #3 (secret_io) + PR #4 (recovery.rs)
  in parallel.
- **Then**: PRs #5-#9 sequentially.
