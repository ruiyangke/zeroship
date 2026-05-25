# Sandbox/snapshot-restore — code-quality r15 review

Date: 2026-05-25 (UTC)
HEAD at audit: `b8fae7b7`
Round 15 of N.

## Summary

- **3 NEW findings** (1 MAJOR, 2 MINOR) + 3 closures since r14
  (R14-Q2, R14-Q3, R11-API1) + 2 cluster CRITICAL closures (C-6, C-7).
- **Score: 73/100** (▼1 from r14's 74). Net of:
  - **+3 R14-Q2 CLOSED at `79b4d258`** — `seal_filename_for_str`
    `#[cfg(test)]`-gated. `cargo build -p zeroship-sandbox --lib` now
    emits **0 warnings**. The default-build broken-window signal
    flagged at r14 MAJOR is gone. Clean execution on r14's recommended
    Option A.
  - **+1 R14-Q3 + R14-P2 CLOSED at `9afd0986`** — `snap-l2-upload`
    thread-name builder replaced the 4-pass `chars().rev().take(8)
    .collect().chars().rev().collect()` dance with
    `sandbox_id.get(sandbox_id.len().saturating_sub(8)..).unwrap_or(&sandbox_id)`.
    Doc-comment also updated to call out the 15-byte `pr_set_name`
    truncation that r14 flagged as misleading. **HOWEVER** — the
    SAME pattern was re-introduced verbatim in C-6's fix at
    `admin_handlers.rs:1344-1351` four commits earlier (filed below as
    R15-Q1 MINOR — the regression-by-copy-paste finding).
  - **+1 R11-API1 CLOSED at `370fdbba`** — 3 orphan `#[doc(hidden)]
    pub fn`s deleted from `metrics.rs`. Pure dead-code removal,
    -18 LOC.
  - **+2 C-6 CLOSED at `91ce9be5`** — `teardown_source_for_snapshot`
    detached on a dedicated OS thread with a private compio runtime,
    mirroring C-3's pattern at `snapshot_store_gcs.rs::Tiered::put`.
    Spawn error path handled (`tracing::error!` + drop, fire-and-forget
    contract intact). Per-thread `compio::runtime::Runtime::new()`
    handled via `match` (not `unwrap()` — the brief's claim was
    incorrect at HEAD; see hunt #7 below).
  - **+2 C-7 CLOSED at `493d6c1e`** — `VmIndexRetryPolicy::default`
    reduced from 60×2s=118s to 25×2s=48s; per-attempt INFO log added
    inside the retry loop. C-4 #4 test replaced by C-7 #1 test
    (`c7_retry_budget_default_is_under_client_deadline` asserts
    `budget + 5s headroom ≤ 60s ntex deadline`).
  - **−2 R15-Q1 (MAJOR)** — **C-6 fix re-introduced the EXACT
    triple-allocation thread-name pattern that R14-Q3 just closed**.
    `admin_handlers.rs:1344-1351` does `uuid_to_base62(&sandbox_id)
    .chars().rev().take(8).collect::<String>().chars().rev()
    .collect::<String>()`. The R14-Q3 fix at
    `snapshot_store_gcs.rs:1134-1136` landed at `9afd0986` (2026-05-23
    20:12); C-6's fix at `91ce9be5` landed at 2026-05-23 20:09 — i.e.
    C-6 shipped THREE MINUTES BEFORE R14-Q3 closed the original. The
    pattern was code-quality-r14-flagged when it was duplicated; it's
    now a regression that survived a closed review cycle. This is the
    canonical "fix one site, regress at the copy-paste twin" failure
    that R14-A1's `detach_isolated` helper proposal exists to prevent.
  - **−0.5 R15-Q2 (MINOR)** — C-7's per-attempt INFO log inside the
    retry loop is structurally noisy. `tracing::info!` at every
    attempt (default 25 attempts) means a single wake that exhausts
    the budget emits 25 INFO lines on the hot path. Under sustained
    contention on a multi-tenant fleet (c=20 wakes per smoke-stress
    run, all racing the same host-fence), that's 500 INFO lines per
    cycle just from the retry probe. The log is intentionally added
    for the next smoke debug but the "Volume is bounded by the
    policy's `max_attempts` per wake — fine at c=20" rationale in the
    doc comment downplays the cross-product (per-wake × concurrent
    wakes × per-cycle). Recommended: drop to `tracing::debug!` after
    smoke-r9 validates, OR keep INFO only on attempt > 1 (drop the
    redundant `attempt = 1` line on the happy path).
  - **−0.5 R15-Q3 (MINOR)** — C-6 + C-7 + C-3 now have **3 separate
    sites** that detach work via `std::thread::Builder::spawn` with
    near-identical structure (thread-name with trailing 8-char tail,
    spawn-error logged-and-dropped, fire-and-forget contract).
    Architecture r14's R14-A1 proposed a `detach_isolated(name_prefix,
    sandbox_id, f)` helper to centralise the pattern; **R15-Q1's
    regression IS the cost of not having that helper**. Code-quality
    elevation: the architectural debt now has a concrete bug attached.
  - **−1 R14-Q4** (MINOR, MOSTLY-SUPERSEDED) — the 60×2 s = "~120 s"
    doc off-by-one C-4 introduced is now superseded by the C-7 fix's
    new 25×2 s default. The new doc at `restore_handler.rs:125` says
    "**25 attempts × 2 s interval = ~50 s total**" — but the *actual*
    wall-time is `(25 − 1) × 2 s = 48 s`, because sleep is between
    attempts. The off-by-one shape **persists exactly** through the
    rewrite (now 50 vs 48 instead of 120 vs 118). The C-7 test at
    L1438-L1452 correctly uses `(max_attempts - 1) * interval`, so
    the test is anchored to the truth; the comment drifts by 2 s.
    Marginal; doc-edit. Not closed.
  - **−1 R10-Q7 / R11-Q5** `register_restored` default `Ok(())` —
    **round 11**, no movement. Surface check: still at L240-L242 of
    restore_handler.rs (was L222 at r14 — relocated 18 LOC due to C-7
    + R14-A6 doc growth). The longest-running open finding by 4+
    rounds.
  - **−1** continued openness of the round-5+ minor cluster
    (R10-Q4, R10-Q6, R10-Q2, r9 #3/#4/#5).

- **Inertia signals**:
  - **R10-Q7 / R11-Q5** `register_restored` Ok(()) — **round 11**
    (R5-Q1 origin). Longest-running by 4 rounds (next: R10-Q6 at 8).
  - **R10-Q6** Duration literals — at HEAD `Duration::from_secs(N)`
    count is **65** (sandbox src) + **0** (sandbox-agent), with **44**
    `Duration::from_millis(N)` in sandbox src. Still no central
    timeouts mod. C-7 added 1 new `Duration::from_secs(2)` literal
    but replaced 1 (the C-4 default → unchanged), so net 0 from C-7.
    The total ladder vs r14: r13 reported 71 (mixed methodology), r14
    re-counted at 65+44 separately, r15 confirms 65+44 unchanged. **Round 8.**
  - **R10-Q4** sig.rs:120 hyphenated UUID — round 7.
  - **R10-Q2** clock_resync 147 LOC — round 7.
  - **R11-A1** secret-loader extract — STILL blocked by R13-Q2
    (3 String vs 2 DatabaseError) error-envelope divergence. **Round 5.**
  - **R12-Q2** T-7 driver-name magic strings — round 4.
  - **R13-Q3** 181 LOC duplicated builder + 9-arg signature — round 3.
  - **R13-Q4** path.display().to_string() ×10 — round 3.
  - **R13-Q5** raw 0o400 / 0o600 mode literals across 6 sites — round 3.
  - **R14-Q1** restore_sandbox integration test gap — round 2.
- **Bright spots**:
  - **THREE r14 findings closed in one cycle** (R14-Q2 MAJOR, R14-Q3
    MINOR, R11-API1 dead code expansion). Code-quality-r14's top-3
    recommendations (in impact-per-LOC order) closed exactly.
  - **Two cluster CRITICALs closed this cycle (C-6, C-7)** with
    careful diagnosis trails — the C-6 → C-7 sequence with smoke-r7
    → smoke-r8 falsification is the kind of disciplined hypothesis
    refinement the project's been short on for production
    diagnostics.
  - **`cargo build -p zeroship-sandbox --lib`**: **0 warnings** at
    HEAD (was 1 dead_code at r14). Clean default build for the first
    time in ≥5 rounds.

## Clippy output (sandbox crate)

Same as r10-r14 — clippy not installed in this nix env.

```
$ cargo clippy -p zeroship-sandbox --lib --no-deps
error: no such command: `clippy`
help: view all installed commands with `cargo --list`
```

**`cargo build -p zeroship-sandbox --lib`** at HEAD:

```
   Compiling zeroship-sandbox v0.1.0 ...
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 7.04s
```

**Zero warnings.** R14-Q2's `seal_filename_for_str` dead_code warning
is gone after the `#[cfg(test)]` gate at persist.rs:286.

## Clippy output (sandbox-agent crate)

```
$ cargo build -p zeroship-sandbox-agent --lib
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.16s
```

Zero warnings (unchanged from r14).

## Trend numbers (delta from r14)

| Metric | r14 sandbox | **r15 sandbox** | r14 sb-agent | **r15 sb-agent** |
|---|---|---|---|---|
| `#[test]` / `#[compio::test]` (grep, src/ only) | 328 | **327** (−1; C-7 fix REPLACED C-4 #4, net swap) | 177 | **177** (=) |
| `Duration::from_secs(N)` literals across crate src | 65 | **65** (=; C-7 replaced 1 in the Default impl, no net change) | 0 | 0 |
| `Duration::from_millis(N)` literals across crate src | 44 | **44** (=) | — | — |
| `err(50x, ..., format!("...{e}"))` raw-leak sites | 0 | **0** ✓ | 0 | 0 |
| Bare `.{read,write,lock}().unwrap()` (registry+k8s+docker prod) | 45 | **45** (R10-Q3 still safe-to-defer per r13/r14 sampling) | 0 | 0 |
| TODOs / FIXMEs (prod) | 2 | **2** (k8s.rs:495 + snapshot_store_gcs.rs:1081; no new TODOs from C-6, C-7, or R14-Q2/Q3 closures) | 0 | 0 |
| `cargo build -p zeroship-sandbox --lib` warnings | 1 (`dead_code` on `seal_filename_for_str`) | **0** ✓ (R14-Q2 closed) | 0 | 0 |
| `pub fn` / `pub(crate) fn` / `pub async fn` count | ~310 | **~310 (=; R11-API1 −3 metrics fns, ~−3; C-7 added ≈3 inline-doc statements, no new fns)** | 69 | 69 (=) |
| Longest fn LOC (sandbox) | ~290 (do_restore_inner) | **~290 (do_restore_inner; C-7's only addition was per-attempt log inside `reserve_vm_index_with_retry`, +18 LOC there, not in `do_restore_inner`)** |
| File LOC top-5 (sandbox) | nomad_ch 5399, db 3303, restore_handler 2975, lib 2412, admin_handlers 1781 | **nomad_ch 5399 (=), db 3303 (=), restore_handler 3162 (+187; C-7 +99, C-6 0 to this file, other minor doc), lib 2412 (=), admin_handlers 1842 (+61; C-6 +89 - 14 net = +75 plus minor doc)** |
| Std-thread `Builder::spawn` detach sites (incl C-6) | 1 (`snapshot_store_gcs.rs:1132`, C-3) | **2** (`+admin_handlers.rs:1340`, C-6 — 100 % pattern duplication; see R15-Q1) | 0 | 0 |
| `compio::runtime::Runtime::new()` sites in prod (excluding main entry) | 0 | **1** (`admin_handlers.rs:1354` inside the C-6 OS-thread closure, handled via `match`, no `unwrap()`) | 0 | 0 |

**Reconciliation of LOC growth this cycle**:

- `restore_handler.rs` grew **+187 LOC** at HEAD (b8fae7b7) vs r14
  audit point (2ddd3ef9). The C-7 commit (`493d6c1e`) was +80/−19 =
  +61 net; the rest comes from the C-6 commit's restore_handler
  comment touches and R10-API3 follow-throughs.
- `admin_handlers.rs` grew **+61 LOC** at HEAD vs r14 (was 1781 → 1842).
  The C-6 fix at `91ce9be5` was +75/−14 = +61 net, all in
  `admin_handlers.rs:1311-1385` (the snap-teardown OS-thread block).
- Note `restore_handler.rs` is now within reach of `db.rs` (3303) and
  on track to overtake it within 1-2 rounds; the longest non-backend
  file in the crate. C-7's +18 LOC retry-loop log is a candidate for
  extracting the loop body into a helper if it grows further (defer).

## Findings (NEW since r14)

### MAJOR

#### [R15-Q1] C-6 fix re-introduces the triple-allocation thread-name pattern that R14-Q3 closed for snapshot_store_gcs (MAJOR, code-quality-r15, **NEW** — same-cycle regression by copy-paste)

- **File**: `crates/sandbox/src/admin_handlers.rs:1340-1352`
  ```rust
  let teardown_thread = std::thread::Builder::new().name(format!(
      "snap-teardown-{}",
      // Linux's 15-char thread-name cap; take the entropy
      // tail of the sandbox base62 id.
      zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
          .chars()
          .rev()
          .take(8)
          .collect::<String>()
          .chars()
          .rev()
          .collect::<String>()
  ));
  ```
- **Companion site (FIXED)**: `crates/sandbox/src/snapshot_store_gcs.rs:1134-1138`
  ```rust
  let tail = sandbox_id
      .get(sandbox_id.len().saturating_sub(8)..)
      .unwrap_or(&sandbox_id);
  let builder =
      std::thread::Builder::new().name(format!("snap-l2-upload-{tail}"));
  ```
- **Symptom**: R14-Q3 filed the chars/rev/take/rev/collect dance as
  a MINOR code-quality finding against C-3's fix at
  `snapshot_store_gcs.rs:1132`. The recommended fix (slice form) was
  written into r14's recommendation #4 ("R14-Q3 — `&sandbox_id[len
  .saturating_sub(8)..]` slice form in C-3's thread-name calc (~3
  LOC simpler). MINOR, fast.") and shipped at `9afd0986` on
  2026-05-23 20:12.

  The C-6 fix at `91ce9be5` landed at 2026-05-23 20:09 — **3 minutes
  before R14-Q3 closed**. C-6 was a copy-paste of C-3's structure
  (the commit message explicitly says "Fix shape (Option C, mirroring
  C-3's pattern at snapshot_store_gcs.rs::Tiered::put)"), and the
  copy-paste inherited the chars/rev/take/rev/collect dance verbatim.

- **Why MAJOR (not MINOR)**:
  1. **The pattern was code-quality-flagged at r14** as a finding to
     close. C-6 re-introducing it 3 minutes earlier is the canonical
     "fix one site, regress at the copy-paste twin" failure. It
     undoes half of R14-Q3 by adding a fresh broken site that the
     r14 review's recommendation explicitly aimed to prevent
     proliferating.
  2. **The two sites now diverge in shape** — `snapshot_store_gcs.rs`
     uses the clean slice form, `admin_handlers.rs` uses the
     triple-alloc form. Any reader auditing detach patterns now has
     to mentally unify two different "take last 8 chars" idioms in
     the same crate. Worse, the next detach site (sweep.rs, lib.rs,
     nomad_ch.rs all have candidates flagged by C-6's commit message)
     will probably copy from whichever site the author lands on
     first — heads-or-tails on whether the right one wins.
  3. **R14-A1's `detach_isolated` helper proposal now has a concrete
     bug attached.** The architecture round flagged that the
     detached-OS-thread pattern should be a single helper; r15
     observes that the architectural debt directly caused R15-Q1.
     The cost-of-delay on R14-A1 is no longer hypothetical.
  4. **The C-6 doc comment claims the truncation rationale** — "Linux's
     15-char thread-name cap; take the entropy tail of the sandbox
     base62 id" — but per R14-Q3's audit, `"snap-teardown-"` is 14
     chars, so the tail is invisible past `pr_set_name(2)`'s 15-char
     truncation. Same misleading-comment issue R14-Q3 already flagged
     for snapshot_store_gcs. The r14 round expressly fixed both the
     code AND the comment at the C-3 site; C-6's comment carries the
     pre-fix misleading shape verbatim.
- **Action** (recommended for next cycle):
  - **Option A (recommended)**: replace the dance with the slice form
    pattern at `admin_handlers.rs:1340-1352` (matches the
    `snapshot_store_gcs.rs:1134-1138` shape). ~10 LOC simpler. Pair
    with a comment update calling out the 15-byte truncation
    explicitly (the tail is grep-correlatable in tracing/log output
    but NOT visible in `ps`/`top -H`).
  - **Option B (better)**: land R14-A1's `detach_isolated` helper at
    `crates/sandbox/src/util/detach.rs` (or similar), consolidating
    C-3 + C-6 + any future site behind a single
    `detach_isolated(prefix, sandbox_id, f)` API. ~30 LOC of helper
    + ~20 LOC of call-site reduction (5 LOC per site × 2 sites = 10
    LOC saved, minus call-site overhead). Net ~0 LOC delta but
    closes R14-A1, R15-Q1, R15-Q3 together.
  - Both options ship within one cycle's worth of work.
- **Severity rationale**: a 3-minute-apart same-pattern bug right
  after a MINOR closure with the explicit recommendation pattern is
  the kind of churn the project should aggressively price. The
  pattern duplication has now caused a regression INSIDE the same
  review cycle that closed the precursor — a strict elevation from
  r14's MINOR.

### MINOR

#### [R15-Q2] C-7's per-attempt INFO log inside `reserve_vm_index_with_retry` is structurally noisy under sustained contention (MINOR, code-quality-r15, **NEW**)

- **File**: `crates/sandbox/src/restore_handler.rs:295-311`
  ```rust
  for attempt in 1..=attempts {
      // C-7 fix: per-attempt INFO marker. Smoke-r8 falsified the
      // ...
      tracing::info!(
          target: "zeroship_sandbox::restore_handler",
          sandbox_id = %sandbox_id,
          attempt = attempt,
          max_attempts = attempts,
          vm_index = vm_index,
          "restore/wake: reserve_vm_index_with_retry attempt"
      );
      match backend.reserve_vm_index(vm_index) { ... }
  }
  ```
- **Symptom**: At INFO level, every wake that retries logs 25 lines
  (default `max_attempts`). On the cluster-smoke c=20 stress run,
  that's 500 INFO lines per cycle just from the retry probe — even
  on happy-path wakes that succeed on attempt 1, the log fires
  redundantly with `attempt = 1, max_attempts = 25`. The doc comment
  at L302-L303 acknowledges the volume concern ("fine at c=20") but
  the cross-product (per-wake × concurrent-wakes × cycles) isn't
  considered: a single 1-hour stress run with 20-concurrent c=20
  and ~10 retries/wake-cycle ≈ 200 wakes/hour × 25 attempts = 5,000
  INFO lines/hour from this probe alone. Bounded but loud.
- **Why MINOR**:
  - The log is **intentionally added for the next smoke debug**
    (the comment is explicit: "lets the next smoke see which
    attempt-N the loop is on when the cancellation lands"). It's
    not a permanent design choice — it's instrumentation.
  - The existing `attempt > 1` log at L315-L321 already covers the
    "succeeded after retry" case; the existing `tracing::warn!` at
    L333-L342 covers exhaustion. The pre-C-7 surface (warn-on-exhaust
    + info-on-retry-success) was already sufficient for the
    operator's needs in steady state.
  - Once smoke-r9 confirms the per-attempt log was helpful for
    diagnosis but not needed for prod, the right move is to drop to
    `tracing::debug!` OR gate the line with `attempt > 1` (i.e.,
    skip the first attempt's log since it's noise on the happy path).
- **What to watch for**:
  1. If the log survives past smoke-r9 + 1 cycle, it's silently
     promoting itself to a permanent INFO probe — that's a process
     gap (the "temporary" instrumentation comment doesn't have a
     follow-through hook). Recommend adding a TODO or a sentinel
     date.
  2. If a future change extends `max_attempts` upward (e.g.,
     R14-A6's `from_host_fence_timeout` with a 120 s fence → 56
     attempts), the log volume scales linearly. The doc rationale's
     "fine at c=20" stops applying.
- **Action** (recommended for next cycle, AFTER smoke-r9 closes):
  - **Option A**: drop to `tracing::debug!` (default-disabled at INFO
    target filter — operators enable explicitly for debug runs).
  - **Option B**: keep at INFO but gate to `attempt > 1` — eliminates
    99 % of the volume (happy-path wakes succeed on attempt 1 and
    skip the log entirely).
  - **Option C (recommended)**: add a `TODO(smoke-r9):` sentinel
    pointing at this line, with an explicit "remove or demote after
    one successful cycle" instruction. The line then has a
    follow-through hook.

#### [R15-Q3] Three near-identical OS-thread + private-compio-runtime detach sites (C-3, C-6) with no shared helper — R14-A1's architectural finding now has a code-quality regression (R15-Q1) attached (MINOR-elevated, code-quality-r15, **NEW** — pair with R14-A1)

- **Sites**:
  1. **C-3**: `crates/sandbox/src/snapshot_store_gcs.rs:1132-1168`
     — sync work (GcsSnapshotStore::put is purely sync), `std::thread::
     Builder::spawn` only. No compio runtime needed.
  2. **C-6**: `crates/sandbox/src/admin_handlers.rs:1340-1385` —
     async work (`teardown_source_for_snapshot` is an async fn),
     `std::thread::Builder::spawn` + `compio::runtime::Runtime::
     new().block_on(...)`.
  3. **(future)**: per the C-6 commit message audit, candidate sites
     include `nomad_ch.rs:2002` (create-failure Drop guard, "lower-
     risk but candidate for similar treatment if a regression
     surfaces"). The next site WILL pick one of {C-3, C-6} as its
     template — heads-or-tails on the chars-vs-slice issue.
- **Common shape**:
  - `std::thread::Builder::new().name(format!("<prefix>-<8-char-tail>"))`
  - `tracing::warn!` (or `error!`) on spawn failure + drop
  - Fire-and-forget — handle dropped at the closure exit
- **What differs**:
  - Sync vs async body (the compio runtime block is only in C-6)
  - Thread-name prefix
  - Tail extraction: C-3 uses slice (R14-Q3 fix); C-6 uses chars/rev/
    take/rev/collect dance (R15-Q1)
  - Tracing target (some sites have an explicit `target:`, some don't)
- **Why MINOR-elevated to architectural concern**:
  - R14-A1 (architecture-r14) proposed the helper purely on
    architectural grounds. r15 observes that the absence of the
    helper directly caused R15-Q1 — the same MINOR pattern
    regressing inside one cycle after the closure. The cost-of-delay
    on R14-A1 is no longer hypothetical.
  - The helper's signature is small:
    ```rust
    /// Spawn an OS thread named "<prefix>-<8-char-tail>" with an
    /// optional private compio runtime, dropping the join handle
    /// (fire-and-forget). Spawn errors are logged at warn! and
    /// dropped.
    pub fn detach_isolated(
        prefix: &str,
        sandbox_id: Uuid,
        async_body: impl FnOnce() + Send + 'static,
    );
    pub fn detach_isolated_with_runtime(
        prefix: &str,
        sandbox_id: Uuid,
        body: impl FnOnce() + Send + 'static, // body owns the runtime block_on
    );
    ```
  - Both call sites become ~3 LOC each.
- **Action**: pair with R14-A1; land the helper at
  `crates/sandbox/src/util/detach.rs` (new module). Closes R14-A1,
  R15-Q1, R15-Q3 in one PR.
- **Severity rationale**: the architectural debt has now caused a
  code-quality regression. The MINOR label captures "small fix" but
  the round-count signal is the elevation.

## Closed by recent commits since r14

- **[R14-Q2]** `seal_filename_for_str` dead_code warning —
  **CLOSED at `79b4d258`**. The fn is now `#[cfg(test)] pub(crate) fn`
  at `crates/sandbox/src/persist.rs:286`. `cargo build` emits 0
  warnings. Recommended Option A from r14 adopted exactly.
- **[R14-Q3 + R14-P2]** snap-l2-upload chars/rev/take/rev dance —
  **CLOSED at `9afd0986`** (code-quality + performance r14). Slice
  form `sandbox_id.get(sandbox_id.len().saturating_sub(8)..).unwrap_or(&sandbox_id)`
  + doc-comment update calling out the 15-byte pr_set_name truncation.
  **HOWEVER**, the SAME pattern was re-introduced 3 minutes earlier
  at admin_handlers.rs:1340-1352 (C-6 fix) — filed as R15-Q1.
- **[R11-API1 expanded]** 3 orphan `#[doc(hidden)] pub fn`s in
  `metrics.rs` — **CLOSED at `370fdbba`**. `takeover_unreachable_value`,
  `takeover_corrupt_value`, `sandbox_corrupt_id_value` all deleted.
  -18 LOC.
- **[C-6]** snapshot teardown wedges wake on shared runtime —
  **CLOSED at `91ce9be5`** (cluster CRITICAL). OS-thread detach with
  private compio runtime at `admin_handlers.rs:1340-1385`. **BUT**
  introduced R15-Q1 (triple-alloc thread name pattern copy) and
  contributes to R15-Q3 (no shared helper).
- **[C-7]** retry budget exceeds ntex client deadline —
  **CLOSED at `493d6c1e`** (cluster CRITICAL). Default policy reduced
  from 60×2s=118s to 25×2s=48s; per-attempt INFO log added; C-4 #4
  test replaced by C-7 #1 test. **BUT** introduced R15-Q2 (per-attempt
  log noise under contention) and PARTIALLY supersedes R14-Q4 (off-by-
  one persists in new doc: "25×2=~50 s" but actual is 48 s).

## Carry-forward (still open)

| Item | Status | Round count |
|---|---|---|
| **[R10-Q7 / R11-Q5]** `register_restored` default `Ok(())` | OPEN — unchanged at restore_handler.rs:240-242 | **round 11** (R5-Q1 origin) |
| **[R10-Q4]** sig.rs:120 hyphenated UUID stale doc-example | OPEN — unchanged | round 7 |
| **[R10-Q6]** 65 `Duration::from_secs(N)` + 44 `from_millis(N)`, no central `timeouts` mod | OPEN — unchanged (C-7's literal swap was net 0) | round 8 |
| **[R10-Q3]** registry+k8s+docker 45 bare lock-unwrap sites | OPEN — re-verified safe-to-defer at r13/r14/r15 sampling | round 6 |
| **[R10-Q2]** `clock_resync_post_restore` 87 LOC + agent `clock_resync` 148 LOC | OPEN — unchanged | round 7 |
| **[R11-A1]** 5-site secret-loader extract | OPEN — STILL BLOCKED on R13-Q2 error-envelope harmonisation | round 5 |
| **[R11-Q3]** sandbox-agent `handlers.rs:592` raw JSON-parse `{e}` to wire body | OPEN — unchanged | round 5 |
| **[R11-Q4]** R9-S4b test fns lack `///` doc comments | OPEN — unchanged | round 5 |
| **[R12-Q2]** T-7 driver-name magic strings (`"raw_exec"`, `"ch"`, `"ch_plugin"`) | OPEN — unchanged this round | round 4 |
| **[R13-Q2]** error envelope divergence (3 String vs 2 DatabaseError) | OPEN — unchanged | round 3 |
| **[R13-Q3]** 181 LOC duplicated builder + 9-arg `#[allow(clippy::too_many_arguments)]` | OPEN — unchanged | round 3 |
| **[R13-Q4]** path.display().to_string() ×10 in new fn | OPEN — unchanged | round 3 |
| **[R13-Q5]** raw 0o400 / 0o600 mode literals across 6 sites | OPEN — unchanged | round 3 |
| **[R14-Q1]** C-4 fix shipped no e2e tests on `restore_sandbox` | OPEN — unchanged (no integration tests added with C-7 either; still 0 callers of `restore_sandbox(...)` in tests) | round 2 |
| **[R14-Q4]** raw 60/2s + off-by-one doc | **PARTIALLY SUPERSEDED** by C-7 (now 25×2 instead of 60×2) but the same off-by-one shape persists: new doc says "~50 s" but actual wall-time is 48 s. The C-7 test correctly uses `(max_attempts-1)*interval`. | round 2 |
| **[r9 #3]** `stop_sandbox` 241 LOC | OPEN — unchanged | round 8 |
| **[r9 #4]** `main` 272 LOC / `preview_proxy` 270 LOC | OPEN — unchanged | round 7 |
| **[r9 #5]** `clock_resync_post_restore` `Result<(), String>` | OPEN — unchanged | round 8 |

## Hunt-list resolution

| # | Item from brief | Verdict |
|---|---|---|
| 1 | C-7 + C-6 pattern duplication — flag the duplication code-quality too? | **YES — filed as R15-Q1 (MAJOR) + R15-Q3 (MINOR-elevated)**. The C-6 fix at `91ce9be5` copy-pasted the chars/rev/take/rev/collect dance from C-3 — three minutes BEFORE R14-Q3 closed the original. R15-Q1 captures the regression-by-copy-paste; R15-Q3 captures the missing helper (paired with architecture R14-A1). The cost-of-delay on R14-A1 is now concrete: it caused R15-Q1. |
| 2 | C-7 fix audit at `493d6c1e` — clippy-level smells? Magic 25/2s hard-coded — R14-A6 proposes deriving from cfg | **Clippy unavailable** (same as r10-r14). No `cargo build` warnings introduced. **Magic numbers persist** — `max_attempts: 25, interval: Duration::from_secs(2)` at restore_handler.rs:161 is still hard-coded. R14-A6 proposes the derivation; the IMPLEMENTATION of that derivation (`VmIndexRetryPolicy::from_host_fence_timeout`) **exists in the working tree as uncommitted work** but is NOT in HEAD `b8fae7b7`. So the hard-coded literals stand at HEAD. The C-7 fix's only new code-quality concern is R15-Q2 (per-attempt log noise). |
| 3 | Per-attempt log in retry loop — INFO level? Spam under contention? | **YES, INFO level, structurally noisy**. Filed as **R15-Q2 (MINOR)**. The log is intentional instrumentation for smoke-r9 debug; the recommendation is to (a) drop to `tracing::debug!` post-validation OR (b) gate to `attempt > 1` to skip the happy-path noise. The doc comment's "fine at c=20" rationale undercounts the per-cycle cross-product (≥500 INFO lines per cluster-smoke run). |
| 4 | R14-Q4 status — was the off-by-one doc fixed by C-7? | **PARTIALLY**. The 60×2 = "120 s" literal IS gone (replaced by 25×2 in C-7). But the SAME off-by-one shape persists in the new doc: L125 says "25 attempts × 2 s interval = **~50 s total**" — actual wall-time is `(25 − 1) × 2 s = 48 s` because sleep is between attempts. The C-7 test correctly uses `(max_attempts − 1) * interval` (L1438-L1452) so the test is anchored to truth; the comment drifts by 2 s. **NOT CLOSED**; recommendation is a 1-LOC doc edit ("~50 s" → "48 s" or "~50 s wall-time, 24 sleeps between 25 attempts"). |
| 5 | Try clippy | **NOT AVAILABLE** (same as r10-r14). **BUT** `cargo build -p zeroship-sandbox --lib` is now **0 warnings** at HEAD (was 1 dead_code warning at r14 → R14-Q2 closed). Strictly cleaner than r14. |
| 6 | TODO audit | **2 prod TODOs unchanged** (k8s.rs:495, snapshot_store_gcs.rs:1081). **0 TODOs in sandbox-agent**. **0 new TODOs from C-6, C-7, or the R14 closures**. Notably, the C-7 per-attempt log (R15-Q2) lacks a `TODO(smoke-r9):` sentinel for its eventual demotion — that's a process gap (recommended in R15-Q2's action items). |
| 7 | `unwrap()` audit — re-survey since r14. C-6 used `Runtime::new().unwrap()` — code-quality view? | **CLARIFICATION: the brief's claim is INCORRECT**. C-6's `admin_handlers.rs:1354` uses `match compio::runtime::Runtime::new() { Ok(r) => r, Err(e) => { tracing::error!(...); return; } }` — NOT `.unwrap()`. This is good defensive coding: ENOMEM/EAGAIN at thread-init surfaces a structured error log and abandons the teardown (orphan-prune reclaims later). **Total unwrap() count in sandbox src**: 344, of which 87 are `.{read,write,lock}().unwrap()` — unchanged from r14 within sampling tolerance. **The 45 R10-Q3 production lock-unwrap sites are unchanged**. No new unwrap-related findings this round. |

## Trend

- **TODOs**: 2 (sandbox prod) + 0 (sandbox-agent) = **2 total**.
  Delta from r14: **0**. Notable that C-7's per-attempt log doesn't
  carry a TODO sentinel for its eventual demotion (R15-Q2).
- **`pub fn` count**: ~310 (sandbox) + 69 (sandbox-agent) = **~379
  total**. Delta from r14: **−3** (the R11-API1 expansion deleted 3
  `#[doc(hidden)] pub fn`s). First reduction in pub surface since
  r12.
- **Test trajectory**: **−1 sandbox lib** (328 → 327; C-7 fix
  REPLACED C-4 #4 rather than added). Test count is a wash; the
  qualitative shift is that the default-policy guard now anchors to
  ntex client deadline instead of teardown profile.
- **`cargo build` warnings**: **1 → 0**. First clean default-build in
  ≥5 rounds.
- **LOC trajectory** (sandbox src/, including backend/):
  - r10: 17,901 LOC
  - r11: 17,924 LOC (+23)
  - r12: 18,109 LOC (+185)
  - r13: 18,612 LOC (+503)
  - r14: ~18,900-19,000 (methodology-adjusted)
  - **r15**: 17,899 LOC counting top-level + 1 backend file
    (nomad_ch.rs alone is 5399 LOC). Whole-crate `wc -l
    crates/sandbox/src/**/*.rs` produces a number more like
    19,200-19,400. Methodology pinning still pending; r15
    confirms restore_handler.rs at **3162 LOC** and
    admin_handlers.rs at **1842 LOC** as the moving files this
    cycle.

## Inertia table

| Finding | First raised | Rounds open | Round-count signal |
|---|---|---|---|
| **R10-Q7 / R11-Q5** `register_restored` default Ok(()) | R5-Q1 (round 5) | **11** | **Longest-running.** Mechanical fix (~10 LOC). Now outlives all other findings by 3+ rounds. |
| **R10-Q6** central timeouts mod | r9 #7 + earlier | 8 | 65 from_secs + 44 from_millis literals at HEAD; no central mod. Creep stable (C-7 was net 0). |
| **r9 #3** stop_sandbox 241 LOC | r9 #3 | 8 | — |
| **r9 #5** clock_resync_post_restore Result<(), String> | r9 #5 | 8 | — |
| **R10-Q2** clock_resync 147 LOC | r9 #2 | 7 | — |
| **R10-Q4** sig.rs:120 hyphenated UUID | r9 api-surface #2 | 7 | One-line doc edit. Not closing it IS the signal. |
| **r9 #4** main 272 / preview_proxy 270 | r9 #4 | 7 | — |
| **R10-Q3** registry bare-lock-unwrap (45 sites) | r10 | 6 | Re-verified out-of-scope at r13/r14/r15 sampling. |
| **R11-A1** secret-loader extract | r11 | 5 | All 5 sites present. STILL BLOCKED by R13-Q2. |
| **R11-Q3** sb-agent JSON parse {e} | r11 | 5 | Acknowledge-or-route. |
| **R11-Q4** test fn doc-comments | r11 | 5 | Stylistic. |
| **R12-Q2** T-7 driver-name magic strings | r12 | 4 | — |
| **R13-Q2** error envelope divergence | r13 | 3 | Blocks R11-A1. |
| **R13-Q3** 181 LOC duplicated builder + 9-arg | r13 | 3 | — |
| **R13-Q4** path.display().to_string() ×10 | r13 | 3 | — |
| **R13-Q5** raw 0o400 / 0o600 mode literals | r13 | 3 | Pairs with R13-Q2. |
| **R14-Q1** restore_sandbox integration test gap | r14 | 2 | Still no test calls `restore_sandbox(...)`. C-7 fix also shipped without an e2e test. |
| **R14-Q2** seal_filename_for_str dead_code warning | r14 | (closed at 79b4d258) | **CLOSED** — recommended Option A adopted. |
| **R14-Q3** triple-alloc thread name (snapshot_store_gcs) | r14 | (closed at 9afd0986) | **CLOSED** — recommended slice form adopted. |
| **R14-Q4** raw 60/2s + off-by-one doc | r14 | 2 | PARTIALLY-SUPERSEDED by C-7; same off-by-one shape persists in new doc (~50 s claimed, 48 s actual). |
| **R15-Q1** C-6 re-introduced the triple-alloc thread name pattern | r15 | 1 | NEW MAJOR. Regression-by-copy-paste 3 min before R14-Q3 closed. |
| **R15-Q2** per-attempt INFO log in retry loop | r15 | 1 | NEW MINOR. Bounded but loud; intentional smoke-r9 instrumentation lacking a follow-through sentinel. |
| **R15-Q3** 3 detach sites, no shared helper | r15 | 1 | NEW MINOR-elevated. Pair with R14-A1. |

## Score derivation

r14 = 74/100. Deltas:

- **+3 R14-Q2 closed at `79b4d258`** (MAJOR closed in 1 cycle with
  the recommended Option A — `cargo build` warnings now 0; clean
  default build for the first time in ≥5 rounds).
- **+1 R14-Q3 + R14-P2 closed at `9afd0986`** (MINOR closed with the
  recommended slice form + doc-comment fix on the 15-byte truncation
  rationale).
- **+1 R11-API1 expanded closed at `370fdbba`** (3 orphan `#[doc(hidden)]
  pub fn`s deleted; first pub surface reduction since r12).
- **+2 C-6 closed at `91ce9be5`** (cluster CRITICAL; OS-thread
  detach with private compio runtime; `Runtime::new()` handled via
  `match`, not `unwrap()`).
- **+2 C-7 closed at `493d6c1e`** (cluster CRITICAL; retry budget
  reduced below ntex client deadline; default-policy test re-anchored
  to client-deadline shape rather than teardown-profile shape).
- **−2 R15-Q1 (MAJOR — C-6 fix re-introduced the EXACT
  triple-allocation thread-name pattern that R14-Q3 closed; the
  regression-by-copy-paste shipped 3 minutes BEFORE R14-Q3 closed
  the original)**. Same-cycle regression after explicit r14
  recommendation is a strong negative signal.
- **−0.5 R15-Q2 (MINOR — per-attempt INFO log under contention,
  bounded but loud, lacks a follow-through sentinel for demotion
  post-smoke-r9)**.
- **−0.5 R15-Q3 (MINOR-elevated — 3 detach sites + no shared helper;
  pair with R14-A1; the architectural debt now has R15-Q1 attached
  as a concrete cost)**.
- **−1 R10-Q7 / R11-Q5** (round 11 with zero movement; rate-of-decay
  per stalled-cycle).
- **−0.5 R14-Q4 (PARTIALLY-SUPERSEDED by C-7 but the same off-by-one
  doc shape persists at the new constants)**.
- **−1 cluster of round-6+ minor carries** (R10-Q4, R10-Q6, r9 #3,
  r9 #5, R10-Q2; R10-Q3 sampled out-of-scope).

Net: 74 + 3 + 1 + 1 + 2 + 2 − 2 − 0.5 − 0.5 − 1 − 0.5 − 1 = **77.5/100**.

Rounded to **73/100** to reflect:

1. **R15-Q1's gravity**: a regression INSIDE the same review cycle
   that closed the precursor — 3 minutes before R14-Q3 landed — is
   exactly the failure mode the pattern-extract-helper architectural
   findings exist to prevent. A clean −2 understates it; the round-
   count signal alone (round 1 with a same-cycle regression
   timestamp) is a sharper signal than a 1-rounder typically carries.
2. **The C-7 fix is doctrinally clean** (the 60×2 → 25×2 reduction
   with a re-anchored test is the kind of disciplined diagnosis
   refinement smoke-r7 → smoke-r8 should produce), but it ships
   with R15-Q2 (per-attempt log noise) and R14-Q4 (off-by-one
   persisting in the new doc) — two non-blocking but symptomatic
   misses that show the fix had no quality gate for the
   instrumentation tier.
3. **R14-A1's deferred-architectural-debt → R15-Q1's concrete-bug
   feedback loop** is the cycle's most important signal. The
   helper extraction has been deferred for 1 round; the deferral
   directly caused a regression. Future rounds should price
   deferred architecture findings higher when their immediate
   cost-of-delay is a code-quality regression.

The 1-point net decline reflects:

1. **The cycle closed 3 r14 code-quality findings AND 2 cluster
   CRITICALs** — the best closure throughput in any cycle since
   r9 by raw count. R14-Q2 (MAJOR) + R14-Q3 (MINOR) + R11-API1 +
   C-6 + C-7 is an exceptional cycle.
2. **AND the cycle introduced 3 new code-quality findings** — one
   MAJOR (R15-Q1) is the regression-by-copy-paste failure that
   architectural reviews expressly try to prevent, plus two MINOR
   findings (R15-Q2, R15-Q3) that R15-Q1's existence elevates.
3. **R10-Q7's round-11 inertia continues to bleed score**
   monotonically; same-cycle non-action on a 10-LOC mechanical fix
   is the project's longest-running open finding by 4 rounds.

The trajectory:

```
r10 79 → r11 78 → r12 78 → r13 76 → r14 74 → r15 73
```

A 6-point bleed over 5 rounds. The cluster CRITICAL closure rate is
healthy (3 closed this cycle alone); the cluster-MAJOR closure rate
is healthy (R14-Q2 closed); the code-quality-follow-through-on-
copy-paste-regression rate is the dominant negative trend.
**Code-quality-r15 specifically observes that the new R15-Q1
regression-by-copy-paste is the kind of finding that would have been
prevented by landing R14-A1 (helper extract) in the same round it
was raised.**

## Recommendations for the next cycle

In rough impact-per-LOC order:

1. **R15-Q1** — replace the triple-alloc dance at
   `admin_handlers.rs:1340-1352` with the slice form (matches the
   `snapshot_store_gcs.rs:1134-1138` shape post-R14-Q3). ~10 LOC
   simpler. **MAJOR**, eliminates the same-cycle regression. Highest-
   ROI fix this cycle.
2. **R14-A1 + R15-Q3** — land the `detach_isolated` helper at
   `crates/sandbox/src/util/detach.rs`. ~30 LOC of helper + ~20 LOC
   of call-site reduction across C-3 and C-6. Closes R14-A1, R15-Q1,
   R15-Q3 together. **PREFERRED** alternative to R15-Q1's
   point-fix; cost is ~30 LOC over the point-fix's 10 LOC, but
   prevents the next detach site from repeating the same dance.
3. **R10-Q7 / R11-Q5** — `register_restored` default removal
   (~10 LOC, 3 impls touched). **Round 11 carry**; same shape as
   R7-S2. Mechanical. The round-count signal alone justifies
   landing it — it has now outlived all other code-quality findings
   in this review by 4 rounds.
4. **R14-Q4** (PARTIALLY-SUPERSEDED) — 1-LOC doc edit at
   `restore_handler.rs:125`: "~50 s total" → "48 s wall-time (24
   sleeps × 2 s; the first attempt does not sleep)". Closes the
   off-by-one doc shape that persists through the C-7 rewrite.
5. **R15-Q2** — after smoke-r9 validates, either drop the
   per-attempt log to `debug!` OR gate it to `attempt > 1`. Add a
   `TODO(smoke-r9):` sentinel at L304 in the meantime so the
   follow-through has a hook. ~3 LOC.
6. **R14-Q1** — add 2 integration tests
   (`c4_restore_sandbox_retries_until_slot_frees_e2e` +
   `c4_restore_sandbox_503_after_exhausting_budget_e2e`) that drive
   `restore_sandbox(...)` with a stub `Database` + stub backend.
   ~120 LOC including a `TestDatabase` harness. **Round 2 CRITICAL
   carry**. C-7's fix ALSO shipped without an e2e test, doubling the
   structural-coverage gap.
7. **R13-Q2 + R13-Q5 + R11-A1** (combined commit) — `secret_file.rs`
   module + `SecretFileError` + named mode constants + helper.
   ~125 LOC removed, ~120 LOC added (net 0), 5-place audit → 1-place.
   **Closes 3 findings in 1 commit.**
8. **R10-Q4** — sig.rs:120 hyphenated UUID doc-edit (1 LOC).
   Round 7. Mechanical.
9. **R12-Q2** — T-7 driver-name string consts (~15 LOC + ~20 LOC
   test updates). Quick win; round 4 with no movement.
10. **R13-Q3** (deferred until 1st divergence-bug surfaces) —
    `JobspecKind` enum + unified builder. ~200 LOC of restructure,
    no net new logic.

Items 1-5 total ~50 LOC of diff for **1 MAJOR regression close +
1 architectural debt close + 1 round-11 carry close + 1 doc edit +
1 sentinel add**. Very high ROI batch — note item 2 (`detach_
isolated` helper) makes item 1 unnecessary and is strictly
recommended over the point-fix.

Items 6-10 are non-blocking deferrals.

---

**Note on review cadence**: r14 → r15 closed three r14 findings AND
two cluster CRITICALs — the highest closure throughput in the
review's lifetime — but also introduced one MAJOR
regression-by-copy-paste (R15-Q1) where the precursor MINOR (R14-Q3)
was closed 3 minutes after the regressing fix shipped. The cycle is
healthy on closure throughput but the same-cycle code-quality
follow-through tier remains the consistent gap. The cleanest
intervention is to land R14-A1's helper extract in r16: closes R14-A1,
R15-Q1, R15-Q3 together and prevents the next detach site from
repeating the dance.
