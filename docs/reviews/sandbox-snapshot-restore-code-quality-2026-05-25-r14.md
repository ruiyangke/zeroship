# Sandbox/snapshot-restore — code-quality r14 review

Date: 2026-05-25 (UTC)
HEAD at audit: `2ddd3ef9`
Round 14 of N.

## Summary

- **4 NEW findings** (1 CRITICAL, 1 MAJOR, 2 MINOR) + 1 closure of a long
  carry (R13-Q1) and 2 newly elevated carries (dead_code, register_restored).
- **Score: 74/100** (▼ 2 from r13's 76). Net of:
  - **+3 R13-Q1 CLOSED at `c5b9cb9d`** — `TASK_DRIVER_ENV_LOCK` unified
    into `crate::backend::nomad_ch::test_env_lock` and used by
    restore_handler::tests via `use … test_env_lock::with_task_driver_env`.
    The cross-module race is dead. Clean execution on r13's
    recommended Option 1.
  - **+2 C-4 (cluster CRITICAL) CLOSED at `b2892368`** — bounded
    retry on wake-vs-source-teardown vm_index race. Trait method
    `vm_index_retry_policy()`, helper `reserve_vm_index_with_retry`,
    `VmIndexRetryPolicy { max_attempts: 60, interval: 2 s }`. 4 unit
    tests pin the loop semantics + a fourth guards the budget envelope.
  - **+1 C-3 CLOSED at `c890c015`** — Tiered L2 upload now via
    named `std::thread::Builder::spawn` (named "snap-l2-upload-<id>"),
    spawn error path handled (warn + drop, fire-and-forget contract
    intact). Architecture r13 R13-A2 still notes the layering smell
    but code-quality wise the patch is well-formed.
  - **+1 R13-API1 + R10-API2 CLOSED at `af4678ac`** — `ExecBody`
    in both `crates/sandbox` and `crates/sandbox-agent` is now
    `pub(crate)`; both routes parse `Bytes` via
    `serde_json::from_slice(&body)`. Shapes are byte-identical
    (struct field order + types match across crates).
  - **−3 R14-Q1 (CRITICAL)** — **C-4 fix shipped ZERO end-to-end
    tests that drive `restore_sandbox`** through the bounded-retry
    path. The 4 tests (`c4_wake_retries_until_source_slot_frees`,
    `c4_wake_fails_if_slot_never_frees_within_budget`,
    `c4_wake_uses_single_attempt_when_slot_free`,
    `c4_default_policy_envelopes_observed_teardown`) all call
    `reserve_vm_index_with_retry(&stub, sid, vm_index)` *directly* —
    none exercises the `do_restore_inner` integration that wires
    retry into rollback. R13's R13-A1 / test-coverage thesis is
    structurally unresolved by this fix; code-quality view: the
    integration glue at `restore_handler.rs:517` (a single call
    site) is unaudited.
  - **−1 R14-Q2 (MAJOR)** — `seal_filename_for_str` is now warning
    `dead_code` on `cargo build` (no clippy needed). The fn is
    `pub(crate)` per R10-API3 partial demotion but is **only** called
    from `#[cfg(test)] mod tests` callers in persist.rs:867, :1135,
    :1136. Either it's truly dead production code (delete) or it's
    test-only (`#[cfg(test)]`-gate). The current state is the worst
    of both: lint-noisy AND surface-area for any future caller to
    pick up an unaudited helper.
  - **−0.5 R14-Q3 (MINOR)** — `c890c015` (C-3 fix) introduces a
    triple-allocation thread-name dance:
    `sandbox_id.chars().rev().take(8).collect::<String>().chars().rev().collect::<String>()`.
    Three intermediate `String`s + two `chars()` iterations to take
    "the last 8 chars". The base62 sandbox id is ASCII so
    `&sandbox_id[sandbox_id.len().saturating_sub(8)..]` is correct,
    O(1), and 0-alloc.
  - **−0.5 R14-Q4 (MINOR)** — C-4's `VmIndexRetryPolicy::default`
    uses raw `60` and `Duration::from_secs(2)` literals. The doc
    comment at L125 explicitly states "60 × 2 s = ~120 s" — but the
    actual sleep budget is `(60 − 1) × 2 s = 118 s` (sleep is between
    attempts, not before/after). Same shape as R10-Q6 (no central
    timeouts mod) AND r13 found 60 inline. Pre-existing pattern;
    C-4 added 1 new instance + a self-contradicting doc comment.
  - **−1 R10-Q7 / R11-Q5** `register_restored` default `Ok(())` —
    **round 10**, no movement. Surface check: still at L222 of
    restore_handler.rs with the same default. **The longest-running
    open finding now.** R5-Q1 origin (round 5).
  - **−1** continued openness of the round-5+ minor cluster
    (R10-Q4, R10-Q6, R10-Q2, r9 #3/#4/#5).

- **Inertia signals**:
  - **R10-Q7 / R11-Q5** `register_restored` Ok(()) — **round 10**
    (R5-Q1 origin). Longest-running by one round.
  - **R10-Q6** Duration literals — count regressed from 71 (r13) to
    72 (added C-4's `interval: Duration::from_secs(2)` at L143). Still
    no central timeouts mod. **Round 7.**
  - **R10-Q4** sig.rs:120 hyphenated UUID — round 6. 1-line doc edit.
  - **R10-Q2** clock_resync 147 LOC — round 6.
  - **R11-A1** secret-loader extract — still blocked by R13-Q2
    (3 String vs 2 DatabaseError) error-envelope divergence. **Round 4.**
  - **R12-Q2** T-7 driver-name magic strings — round 3.
  - **R13-Q3** 181 LOC duplicated builder + 9-arg signature — round 2.
  - **R13-Q4** path.display().to_string() ×10 — round 2.
  - **R13-Q5** raw 0o400 / 0o600 mode literals across 6 sites — round 2.
- **Bright spots**:
  - **R13-Q1 closed in one cycle** with the recommended consolidation
    shape — the cross-module env-lock race is the first CRITICAL
    code-quality finding to close inside one round since r9.
  - **Two cluster-CRITICALs closed (C-3 + C-4) within the cycle**:
    architecture round only flagged R13-A2 (layering) on C-3,
    code-quality says both fixes are well-formed with reasonable
    doc trails. The disconnect with R14-Q1 is that *zero* of the
    fixes added an end-to-end test pair to the existing pg-gated
    integration suite — a structural-coverage gap.

## Clippy output (sandbox crate)

Same as r10/r11/r12/r13 — clippy not installed in this nix env.

```
$ cargo clippy -p zeroship-sandbox --lib --no-deps
error: no such command: `clippy`
help: view all installed commands with `cargo --list`
```

**However** — `cargo build -p zeroship-sandbox --lib` (which IS available)
emits one warning at HEAD:

```
warning: function `seal_filename_for_str` is never used
   --> crates/sandbox/src/persist.rs:281:15
    |
281 | pub(crate) fn seal_filename_for_str(sandbox_id_str: &str) -> Result<String, String> {
    |               ^^^^^^^^^^^^^^^^^^^^^
    |
    = note: `#[warn(dead_code)]` (part of `#[warn(unused)]`) on by default

warning: `zeroship-sandbox` (lib) generated 1 warning
```

This is filed as R14-Q2. The lib build emits no other warnings.

## Clippy output (sandbox-agent crate)

Clippy unavailable. `cargo build -p zeroship-sandbox-agent --lib` emits
no warnings (verified at HEAD).

## Trend numbers (delta from r13)

| Metric | r13 sandbox | **r14 sandbox** | r13 sb-agent | **r14 sb-agent** |
|---|---|---|---|---|
| `#[test]` / `#[compio::test]` (grep, src/ only) | 322 | **328** (+6) | 177 | **177** (=) |
| `Duration::from_secs(N)` literals across crate | 71 | **72** (+1 — C-4's `Duration::from_secs(2)` at restore_handler.rs:143) | — | — |
| `err(50x, ..., format!("...{e}"))` raw-leak sites | 0 | **0** ✓ | 0 | 0 |
| Bare `.{read,write,lock}().unwrap()` (registry+k8s+docker prod) | 45 | **45** (R10-Q3 still safe-to-defer per r13 sampling) | 0 | 0 |
| TODOs / FIXMEs (prod) | 2 | **2** (k8s.rs:495 + snapshot_store_gcs.rs:1081; no new TODOs from C-3, C-4, R13-Q1 closures) | 0 | 0 |
| Static `Mutex<()>` test-env-lock copies in prod files | 3 | **2** (R13-Q1 CLOSED — restore_handler.rs's R12_I1_ENV_LOCK gone; nomad_ch's renamed to `TASK_DRIVER_ENV_LOCK` inside a `pub(crate) mod test_env_lock`; db.rs's `ENV_LOCK` for a DIFFERENT env var remains as the legitimate 2nd lock) | 0 | 0 |
| `pub fn` / `pub(crate) fn` / `pub async fn` count | 303 | **~310** (estimate; +4 from C-4 fix: `VmIndexRetryPolicy::default`, `reserve_vm_index_with_retry`, `vm_index_retry_policy`, public stub fields; +1 from R14-API1 `with_shared_allocator`/`with_nomad_handle` demotion) | 69 | **69** (=) |
| Longest fn LOC (sandbox) | 272 (main) / 270 (preview_proxy) / 263 (do_restore_inner) | **~290 (do_restore_inner; +27)**, main / preview_proxy unchanged; `build_restore_nomad_job_json` 181 unchanged | 147 | 148 |
| File LOC top-5 (sandbox) | nomad_ch 5371, db 3298, restore_handler 2680, lib 2412, admin_handlers 1781 | **nomad_ch 5399 (+28; minor TASK_DRIVER_ENV_LOCK relocation), db 3303 (+5), restore_handler 2975 (+295; C-4 +325 - some doc consolidation), lib 2412 (=), admin_handlers 1781 (=)** |

**Reconciliation of LOC growth this cycle**:

- `restore_handler.rs` grew **+295 LOC** at C-4 (`b2892368`). The
  commit stat is +325 LOC; the smaller observed delta reflects a
  small consolidation in the doc block around `VmIndexRetryPolicy`.
  Body-to-test ratio for C-4: ~75 LOC of production code
  (VmIndexRetryPolicy struct + default + reserve_vm_index_with_retry
  + trait method + stub fields) vs ~165 LOC of tests including the
  4 new tests. **Test ratio ~2.2:1** — substantially leaner than
  T-7's 1:6 (more tests than code) but inverted from R12-I1's 1:1.
  Inversion happens because the new helper is small and the tests
  carry full setup boilerplate per case (root dir, uuid, policy
  config, attempt counter assertions).

## Findings (NEW since r13)

### CRITICAL

#### [R14-Q1] C-4 fix shipped ZERO end-to-end tests; `restore_sandbox` integration of bounded retry is unaudited (CRITICAL, code-quality-r14, **NEW** — elevates R13-A1 / R13-T2 structural concern after observing the fix landed without the integration tier)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:517` — the one
    integration callsite:
    ```rust
    reserve_vm_index_with_retry(backend.as_ref(), sandbox_id, snap.vm_index).await?;
    ```
  - `crates/sandbox/src/restore_handler.rs:1043-1186` — the 4 C-4
    unit tests, **all of which invoke `reserve_vm_index_with_retry`
    directly** with a stub backend; **none** drives `restore_sandbox`
    or `do_restore_inner`.
  - `crates/sandbox/src/admin_handlers.rs:1378` — the ONLY
    production caller of `restore_sandbox`. Reached only by the
    pg-gated integration tests, NOT by the C-4 fix's test
    additions.
- **Symptom**: The C-4 fix is structurally correct at the helper
  level (the 4 tests exhaustively pin loop semantics) but the
  *integration glue* at L517 — the call from `do_restore_inner`,
  the error propagation through `?`, the rollback semantics if
  the retry exhausts mid-restore — has **no test that touches it
  outside of the pg-gated suite**. This is the literal definition
  of "the patch ships untested at the layer that actually wakes a
  sandbox". Per `grep -rn 'restore_sandbox(' crates/sandbox/src`:
  ```
  crates/sandbox/src/restore_handler.rs:317:pub async fn restore_sandbox(    ← defn
  crates/sandbox/src/admin_handlers.rs:1378:    let outcome = restore_handler::restore_sandbox(  ← only caller
  ```
  No test file calls `restore_sandbox(...)`.
- **Why this is CRITICAL (not MAJOR)**:
  1. **The cluster CRITICAL re-opens on regression**: C-4 was a
     real production race that surfaced at the cluster level. A
     refactor that breaks the integration callsite (e.g. swapping
     `reserve_vm_index_with_retry` to a non-async helper, or
     reorganising `do_restore_inner`'s error mapping such that the
     `?` operator now propagates a wrapped `Internal(...)` instead
     of the structured `VmIndexUnavailable { ... }`) would survive
     all 4 C-4 unit tests AND all pg-skipped CI runs — and surface
     only in a cluster smoke. The production-criticality of the
     fix demands an integration test in the same review cycle that
     introduced it.
  2. **The pattern was avoidable in C-4's own scope**: the C-4 fix
     could have added a `#[compio::test] async fn
     c4_restore_sandbox_retries_then_succeeds` that wires
     `StubRestoreBackend { reserve_succeeds_on_attempt: Some(3) }`
     into `restore_sandbox(...)` via a stub `Database` — the same
     pattern the existing pg-gated tests follow, but with the DB
     interactions stubbed out. That test would have validated the
     full path. It was not written.
  3. **R13-A1 / R13-T2 was flagged at architecture and test-coverage
     reviews in prior rounds as a structural gap**. C-4 is the
     first new CRITICAL since those flags; the fix's failure to
     address the gap on the way IN says the gap is now a pattern
     rather than an oversight. Code-quality reviewer's angle: the
     gap is now self-perpetuating because each new fix adds
     helper-level tests and skips integration tests.
  4. **The 4-test scaffolding for C-4 sets a precedent**: future
     bounded-retry tweaks (e.g., adding exponential backoff,
     adding a cluster-fallback path) will likely follow the same
     "add a helper-level test" pattern and remain disconnected
     from `restore_sandbox`. The cost of each future round of
     drift is higher than this round's because the integration
     code path will accrete more branches without coverage.
- **Action** (recommended for next cycle):
  1. **First**: add 2 integration tests in
     `crates/sandbox/src/restore_handler.rs` `mod tests`:
     - `c4_restore_sandbox_retries_until_slot_frees_e2e` — drives
       `restore_sandbox(...)` with a stub `Database` + stub backend
       configured for `reserve_succeeds_on_attempt: Some(3)`,
       asserts `Ok(RestoreOutcome { vm_index, generation, ... })`.
     - `c4_restore_sandbox_503_after_exhausting_budget_e2e` — same
       harness with `fail_reserve = true`, asserts
       `Err(RestoreHandlerError::VmIndexUnavailable { requested })`
       AND that the row was rolled back to `Snapshotted` (via the
       stub's `update_sandbox_status` call log).
  2. **Second**: extract a `TestDatabase` stub trait helper if one
     doesn't exist (likely a fresh module
     `crates/sandbox/src/restore_handler_test_db.rs`). This unblocks
     all future code-quality work on the restore path.
  3. **Estimated effort**: ~80 LOC of test harness + ~40 LOC for
     the 2 tests. Net: the integration tier finally gets coverage,
     and the next CRITICAL fix has a place to add its e2e pair
     without re-inventing the harness.
- **Severity rationale**: C-4 itself shipped a correct race fix.
  But shipping a CRITICAL fix without an integration test is a
  code-quality lapse that exposes the integration callsite to
  silent breakage on every future refactor. R13's R13-A1 / R13-T2
  flag was a "structural concern that needs a sweep"; r14 escalates
  to "the next regression is one refactor away".

### MAJOR

#### [R14-Q2] `seal_filename_for_str` emits a `dead_code` warning at HEAD; the function is `pub(crate)` but every caller is `#[cfg(test)]` (MAJOR, code-quality-r14, **NEW** — surfaces a R10-API3 follow-through that didn't land)

- **File**: `crates/sandbox/src/persist.rs:281`
  ```rust
  pub(crate) fn seal_filename_for_str(sandbox_id_str: &str) -> Result<String, String> {
      // Parse-then-canonicalize; we hash the canonical form so two
      // alternate UUID encodings (hyphenated vs. simple) collide to
      // the same file. ...
      let id: Uuid = sandbox_id_str.parse().map_err(|_| {
          format!("sandbox_id is not a valid UUID: {sandbox_id_str:?}")
      })?;
      Ok(seal_filename_for(id))
  }
  ```
- **Symptom**: `cargo build -p zeroship-sandbox --lib` emits
  ```
  warning: function `seal_filename_for_str` is never used
     --> crates/sandbox/src/persist.rs:281:15
  ```
  Every caller of the function is **inside a `#[cfg(test)] mod tests`
  block**:
  - `persist.rs:867` — `seal_path_is_inside_dir_for_evil_id` (test)
  - `persist.rs:1135` — `seal_filename_for_str_round_trips_with_canonical_uuid` (test)
  - `persist.rs:1136` — same test, second call
  No production code in `crates/sandbox/src/` or anywhere else in
  the workspace calls `seal_filename_for_str` (verified by grep).
- **History**: R10-API3 was an API-surface-r10 finding to demote
  several pub items. `seal_filename_for_str` was demoted from `pub`
  to `pub(crate)`, but the follow-through ("if no in-crate caller
  exists, delete or `#[cfg(test)]`-gate") never landed.
- **Why MAJOR (not MINOR)**:
  1. **The warning is on the default build path**. Anybody running
     `cargo build` sees this warning. It's not a perf bug; it's a
     signal-to-noise pollution: every reviewer of any unrelated
     change sees the warning and has to confirm it's pre-existing.
     A pre-existing warning that no PR ever fixes is a broken-window
     signal — it makes new warnings look acceptable.
  2. **The function has security semantics** (path-traversal
     hardening per the doc comment) — if a future contributor sees
     a `pub(crate)` helper named `seal_filename_for_str`, they may
     reach for it from production code without realising it was
     intentionally test-only. The dead state is BOTH unused AND
     callable — the worst combination for a security-relevant
     helper.
  3. **Two competing remedies exist; the round didn't pick either**:
     either (a) delete the function and inline the parse-then-hash
     in the 3 test callsites, OR (b) `#[cfg(test)]`-gate the
     function so it disappears from the production binary entirely.
     R10-API3 picked neither; the pub(crate) demotion was a
     half-measure.
- **Action** (recommended for next cycle):
  - **Option A (recommended)**: `#[cfg(test)] pub(crate) fn
    seal_filename_for_str(...)` — keeps the existing test callers
    unchanged, eliminates production dead code AND the warning
    in one line.
  - **Option B**: delete the function, inline the 2-line
    parse-then-`seal_filename_for(id)` body in the 3 test
    callsites. Removes ~10 LOC. Marginally noisier at the call
    sites but matches "no dead code, no test-only helpers
    masquerading as semi-prod".
  - Estimated: 1-line change for Option A.

### MINOR

#### [R14-Q3] C-3 fix's thread-name calculation does 3 String allocations + 2 chars iterations to take the last 8 chars of an ASCII sandbox id (MINOR, code-quality-r14, **NEW**)

- **File**: `crates/sandbox/src/snapshot_store_gcs.rs:1125-1131`
  ```rust
  let builder = std::thread::Builder::new().name(format!(
      "snap-l2-upload-{}",
      // Keep the thread name within Linux's 15-char cap by
      // taking the trailing 8 base62 chars of the sandbox id
      // (the entropy bits, not the prefix).
      sandbox_id.chars().rev().take(8).collect::<String>()
                .chars().rev().collect::<String>()
  ));
  ```
- **Symptom**: To take the last 8 chars of `sandbox_id` (a
  `String`), the code:
  1. `.chars()` iterates from the start (potentially O(N) on a
     non-ASCII string; ASCII is O(N) by bytes anyway).
  2. `.rev()` consumes the iterator and reverses it (O(N) buffer).
  3. `.take(8).collect::<String>()` allocates a `String` of 8
     reversed chars.
  4. `.chars()` iterates over that 8-char string.
  5. `.rev().collect::<String>()` reverses it again into another
     `String`.

  Net: **3 String allocations** (the outer `format!`, the reverse
  intermediate, the final reverse) + 2 char iterations over the
  whole sandbox_id + 2 reverse passes — to extract the last 8
  bytes of an ASCII-only base62 string.
- **Why MINOR**: this runs once per snapshot (~minutes apart),
  not on a hot path. Perf cost is negligible. But the line is
  ~120 chars, hard to scan, and conceals a simpler intent.
- **The correct shape** (1 allocation total — the `format!`):
  ```rust
  // `sandbox_id` is base62 ASCII (validated upstream); we take the
  // trailing 8 bytes directly to stay under Linux's 15-char thread
  // name cap. Use `saturating_sub` so a shorter-than-8-char id
  // (test fixtures, future formats) doesn't underflow.
  let tail = &sandbox_id[sandbox_id.len().saturating_sub(8)..];
  let builder = std::thread::Builder::new()
      .name(format!("snap-l2-upload-{tail}"));
  ```
  - 0 extra allocs (the slice is borrowed from sandbox_id).
  - O(1) — direct byte index.
  - Readable.
- **Action**: replace the chars/rev/take/rev dance with the slice
  form above. ~3 LOC simpler. Comes paired with the existing
  comment unchanged.
- **Adjacent code-quality observations on the C-3 fix**:
  - Thread name correctly stays inside Linux's 15-char `pr_set_name`
    cap (`"snap-l2-upload-" = 15 chars + 0` if the trailing block
    is dropped; the 8-char trailing tail pushes it to 23, which
    DOES exceed 15). Linux truncates silently at 15, so the
    8-char tail is invisible to operator tools. **The thread-name
    rationale comment is incorrect**. Fix: either drop the trailing
    8 chars entirely (just `"snap-l2-upload"`, 14 chars) or use 0
    chars and rely on the thread id from `gettid`. Either way the
    current 8-char tail is decorative.

#### [R14-Q4] C-4's `VmIndexRetryPolicy::default` uses raw `60` and `2 s` literals; the doc comment at L125 says "60 × 2 s = ~120 s" but the actual sleep budget is `(60−1) × 2 s = 118 s` (MINOR, code-quality-r14, **NEW** — instance of R10-Q6's "no central timeouts mod")

- **File**: `crates/sandbox/src/restore_handler.rs:141-145`
  ```rust
  impl Default for VmIndexRetryPolicy {
      fn default() -> Self {
          Self { max_attempts: 60, interval: Duration::from_secs(2) }
      }
  }
  ```
- **Symptom**: Both literals are unnamed; their relationship to
  the upstream observed teardown (host_fence ~60 s + Nomad purge
  ~30 s = ~90 s) is documented only in prose at L125 ("Default
  budget: 60 attempts × 2 s interval = **~120 s total**"). But
  the actual sleep budget in `reserve_vm_index_with_retry`
  (L289-L291) is:
  ```rust
  for attempt in 1..=attempts {
      match backend.reserve_vm_index(vm_index) { ... }
      if attempt < attempts {
          compio::time::sleep(retry.interval).await;
      }
  }
  ```
  The sleep is between attempts, so 60 attempts → 59 sleeps × 2 s
  = **118 s** of accumulated wall time, not 120 s. The off-by-one
  is real:
  - `c4_default_policy_envelopes_observed_teardown` at L1174 computes
    `total_ms = interval_ms * (max_attempts − 1)` — which matches
    the implementation. So the test is correct.
  - The doc at L125 ("60 attempts × 2 s interval = **~120 s total**")
    is off by 2 s — minor, but it's exactly the kind of doc/code
    drift a casual reader trusts.
- **Why MINOR**:
  - The 2 s discrepancy is within noise of the 90 s envelope
    target — both the test and the comment converge on "envelopes
    the observed teardown".
  - The literals (60, 2 s) are concentrated in one impl block,
    not scattered.
  - The `c4_default_policy_envelopes_observed_teardown` guard
    DOES catch any future shrinkage below 90 s. The implementation
    is self-defending.
- **What WOULD bite later**: if someone bumps `host_fence_timeout_secs`
  upstream (e.g., from 60 s to 120 s for safety), the 90 s test
  threshold no longer matches the observed teardown, but the test
  passes silently because 60 × 2 s ≥ 90 s. The test should reference
  the upstream `host_fence_timeout_secs` constant (if it lives in
  config) rather than a hardcoded 90 000.
- **Action**: pair with R10-Q6's central `timeouts` mod proposal.
  When that lands, the defaults become:
  ```rust
  pub const VM_INDEX_RESERVE_MAX_ATTEMPTS: u32 = 60;
  pub const VM_INDEX_RESERVE_INTERVAL: Duration = Duration::from_secs(2);
  pub const HOST_FENCE_TIMEOUT_SECS: u64 = 60;
  pub const NOMAD_PURGE_SECS: u64 = 30;
  // VmIndexRetryPolicy::default() reads from these consts.
  // c4_default_policy_envelopes_observed_teardown asserts
  //   budget ≥ HOST_FENCE_TIMEOUT_SECS + NOMAD_PURGE_SECS.
  ```
- **Minor doc fix in isolation**: update L125 to read "60 attempts
  with 2 s inter-attempt sleep ≈ 118 s total wall time (envelopes
  the observed 90 s teardown)" — eliminates the off-by-one in
  the comment without waiting for the central timeouts mod.

## Closed by recent commits since r13

- **[R13-Q1]** `R12_I1_ENV_LOCK` + `T7_ENV_LOCK` cross-module race —
  **CLOSED at `c5b9cb9d`**. Unified into
  `crate::backend::nomad_ch::test_env_lock::TASK_DRIVER_ENV_LOCK`
  + `with_task_driver_env`. restore_handler::tests imports via
  `use crate::backend::nomad_ch::test_env_lock::with_task_driver_env;`
  Verified at `restore_handler.rs:2770-2773` (the closure comment).
  The race scenario from r13 is no longer reachable: both modules
  now serialise through the same mutex symbol.
- **[C-4]** wake-vs-source-teardown vm_index race — **CLOSED at
  `b2892368`** (cluster CRITICAL). 325 LOC + 4 tests at
  restore_handler.rs:128-145, 243-251, 265-306, 517, 1043-1186.
  Tests pin: (1) succeeds-after-N retries, (2) exhausts cleanly,
  (3) single-shot when free, (4) default budget envelopes the
  observed 90 s teardown. **BUT** this commit introduced R14-Q1
  (no e2e integration test of `restore_sandbox` retry path) and
  R14-Q4 (off-by-one in the doc comment).
- **[C-3]** Tiered L2 upload detached via `std::thread::Builder::spawn`
  — **CLOSED at `c890c015`** (cluster CRITICAL). Thread named for
  `gdb`/`top -H` (with a buggy name calculation per R14-Q3); spawn
  error path logged at warn and dropped (fire-and-forget contract
  intact). Architecture R13-A2 noted the layering smell; code-quality
  view: the patch is reasonable. **BUT** introduced R14-Q3 (triple-alloc
  thread name calculation).
- **[R13-API1 + R10-API2]** `ExecBody` pub(crate) restriction +
  exec route Bytes parsing — **CLOSED at `af4678ac`**. Both
  `crates/sandbox/src/handlers.rs:797` and
  `crates/sandbox-agent/src/handlers.rs:568` now have
  `pub(crate) struct ExecBody { cmd: String, cwd: Option<String>,
  timeout_ms: Option<u64> }` — field-for-field identical. Both
  routes parse via `serde_json::from_slice(&body)`. No shape drift.
- **[C-5]** worker scope fix — **CLOSED at `d7740b03`**.
  Out-of-scope for code-quality lens (a scripts/gcloud auth fix,
  not Rust code).

## Carry-forward (still open)

| Item | Status | Round count |
|---|---|---|
| **[R10-Q7 / R11-Q5]** `register_restored` default `Ok(())` | OPEN — unchanged at restore_handler.rs:222 | **round 10** (R5-Q1 origin) |
| **[R10-Q4]** sig.rs:120 hyphenated UUID stale doc-example | OPEN — unchanged | round 6 |
| **[R10-Q6]** 70 `Duration::from_secs(N)` literals, no central `timeouts` mod (now 72 with C-4 add) | OPEN — slight regression | round 7 |
| **[R10-Q3]** registry+k8s+docker 45 bare lock-unwrap sites | OPEN — re-verified safe-to-defer at r13 sampling, no new sites this round | round 5 |
| **[R10-Q2]** `clock_resync_post_restore` 87 LOC + agent `clock_resync` 148 LOC | OPEN — unchanged | round 6 |
| **[R11-A1]** 5-site secret-loader extract | OPEN — STILL BLOCKED on R13-Q2 error-envelope harmonisation | round 4 |
| **[R11-Q3]** sandbox-agent `handlers.rs:592` raw JSON-parse `{e}` to wire body | OPEN — unchanged (the af4678ac exec route fix re-introduces the same shape at the new parse site, but on a SIGNED endpoint so blast radius is the signed payload, not a public attack surface) | round 4 |
| **[R11-Q4]** R9-S4b test fns lack `///` doc comments | OPEN — unchanged | round 4 |
| **[R12-Q2]** T-7 driver-name magic strings (`"raw_exec"`, `"ch"`, `"ch_plugin"`) | OPEN — unchanged this round; still 2 files | round 3 |
| **[R13-Q2]** error envelope divergence (3 String vs 2 DatabaseError) | OPEN — unchanged | round 2 |
| **[R13-Q3]** 181 LOC duplicated builder + 9-arg `#[allow(clippy::too_many_arguments)]` | OPEN — unchanged | round 2 |
| **[R13-Q4]** path.display().to_string() ×10 in new fn | OPEN — unchanged | round 2 |
| **[R13-Q5]** raw 0o400 / 0o600 mode literals across 6 sites | OPEN — unchanged | round 2 |
| **[r9 #3]** `stop_sandbox` 241 LOC | OPEN — unchanged | round 7 |
| **[r9 #4]** `main` 272 LOC / `preview_proxy` 270 LOC | OPEN — unchanged | round 6 |
| **[r9 #5]** `clock_resync_post_restore` `Result<(), String>` | OPEN — unchanged | round 7 |

## Hunt-list resolution

| # | Item from brief | Verdict |
|---|---|---|
| 1 | C-4 fix audit at restore_handler.rs:128-145, 243-251, 265-306, 517 — code quality, long-arg fns, magic numbers (60 × 2s), doc completeness, clippy-bait | **Mixed**: helper `reserve_vm_index_with_retry` is 3 args (clean), doc-block at L96-L127 is exemplary (alternatives weighed + rejection rationale), tests pin loop semantics with 4 focused cases. **BUT** (a) raw `60`/`2 s` literals (filed as R14-Q4 MINOR), (b) doc says "60 × 2 s = ~120 s" but actual sleep budget is 118 s (off-by-one in comment), (c) the 9-arg `do_restore_inner` upstream now has +27 LOC at the C-4 callsite, growing the longest-fn list. No clippy-bait at the C-4-introduced sites themselves. |
| 2 | C-3 fix audit at snapshot_store_gcs.rs:1132 — std::thread::Builder::spawn. Architecture r13 flagged the LAYERING (R13-A2). Code-quality view: is the thread named, is the spawn error path handled, is the join handle dropped intentionally? | **All three handled**: thread named `snap-l2-upload-<id>` via `Builder::new().name(...)`; spawn `Result` matched with `if let Err(e) = spawn_res` (logged at warn, dropped per fire-and-forget contract); join handle is implicitly dropped (not assigned to a var) — intentional detach. **BUT** the thread-name calculation is bug-adjacent: (a) triple-allocation chars/rev dance (filed as R14-Q3 MINOR), (b) the full name `"snap-l2-upload-<8-char-tail>"` is 23 chars but Linux truncates pr_set_name at 15, so the tail is invisible — the rationale comment is incorrect. |
| 3 | R13-A1 elevation — did C-4 add ANY tests that drive `restore_sandbox` end-to-end? If not, file as R14-Q-coverage critical. | **NO** — `grep -rn 'restore_sandbox(' crates/sandbox/src` returns exactly one production caller (`admin_handlers.rs:1378`) and zero test callers. All 4 C-4 tests invoke `reserve_vm_index_with_retry` *directly*. Filed as **R14-Q1 CRITICAL**. |
| 4 | `exec` route Bytes refactor at handlers.rs:797 (af4678ac) — quality of the new Bytes parsing path. Does it match sandbox-agent's exec_cmd shape? Any drift? | **Matches byte-for-byte**: both `crates/sandbox/src/handlers.rs:796-801` and `crates/sandbox-agent/src/handlers.rs:567-572` declare `pub(crate) struct ExecBody { cmd: String, cwd: Option<String>, timeout_ms: Option<u64> }`. Both parse via `serde_json::from_slice(&body)`. The sandbox-side wraps the parse error as `err(400, "invalid_input", ...)`; the sb-agent-side wraps as `err(400, ...)` (no code). Minor stylistic divergence — both surface 400 with a similar message shape; an audit-tool diffing the two would notice the missing code on sb-agent. Not a regression — pre-fix same shape. No new finding. |
| 5 | Pre-existing dead_code warning on `seal_filename_for_str` — still there post-R10-API3 partial demotion. Either delete (truly dead) or `#[cfg(test)]`-gate (only test callers). | **Still warning at HEAD** (verified via `cargo build -p zeroship-sandbox --lib`). All 3 callers are inside `#[cfg(test)] mod tests` in persist.rs (L867, L1135, L1136). Filed as **R14-Q2 MAJOR**. |
| 6 | R11-Q5 register_restored default Ok(()) — now 8-round carry. Status check. | **Still at L222 of restore_handler.rs unchanged**. r13 listed this as "round 9 (R5-Q1 origin)". r14 makes it **round 10**. No movement this cycle. Continues to be the longest-running open finding. |
| 7 | R10-Q6 70 Duration::from_secs literals — has the count moved? Check post-C-4 (60×2s + 120s budget). | **+1 — now 72** total (counting `Duration::from_secs` + `Duration::from_millis`; C-4 added `Duration::from_secs(2)` at L143 and several `Duration::from_millis(N)` in the new tests at L1071, L1112, L1146). Per `Duration::from_secs` alone: r13 was 71, r14 is 65 in sandbox src + 0 in sb-agent. (The discrepancy with r13's "71" reflects methodology: r13 counted both Duration::from_secs + from_millis; r14 separates them: 65 from_secs + 44 from_millis in sandbox src.) Either way the count regressed by ≥1 with C-4's add. Still no central timeouts mod. |
| 8 | TODO audit | **2 prod TODOs unchanged** (k8s.rs:495, snapshot_store_gcs.rs:1081). No new TODOs from C-3, C-4, or R13-Q1 closures. **0 TODOs in sandbox-agent** (unchanged). |
| 9 | Try clippy — `cargo clippy -p zeroship-sandbox --lib --no-deps 2>&1 \| head -100`. If unavailable, fall back to grep. | **NOT AVAILABLE in this nix env** (same as r10-r13). **BUT** `cargo build` IS available and emits 1 dead_code warning (R14-Q2). The `cargo build` warning surface is a strict subset of clippy's, so this is the only currently-emitted signal short of CI. |

## C-4 fix detailed audit

**Architectural shape**: The fix introduces a typed retry policy
(`VmIndexRetryPolicy`) + a generic helper (`reserve_vm_index_with_retry`)
+ a trait method (`vm_index_retry_policy()`) with a default impl
returning `VmIndexRetryPolicy::default()`. Backends override if they
want a different budget. Test stubs (`StubRestoreBackend`) override
with a tight budget (single attempt, 0-sleep) to keep existing tests
fast.

**Strengths**:

- **Excellent rationale documentation** at L96-L127: weighs three
  alternative fix shapes ((a) block snapshot until teardown done,
  (b) cross-slot fallback, (c) decouple vm_index release from
  host-fence) and explains why each was rejected. This is the kind
  of doc that future-self (or another reviewer) reads to understand
  why the chosen shape was correct.
- **The `c4_default_policy_envelopes_observed_teardown` test** is
  a structural guard against future shrinkage. If someone bumps
  `max_attempts` from 60 to 30 to "make the test faster", this
  test fails loudly.
- **`max_attempts.max(1)` at L271** correctly handles the
  pathological case where a backend overrides to 0 — the loop
  still runs once.
- **Tracing structured at the right level**: `info!` on retry-success
  (operator wants to see this), `warn!` on retry-exhaustion (alert-worthy).
- **The `if attempt < attempts` guard at L289** correctly avoids
  sleeping after the final attempt — saves up to 2 s on the
  failure path.

**Code-quality concerns** (folded into the findings above):

- **R14-Q1 CRITICAL**: no e2e test drives `restore_sandbox` with
  retry behaviour. The integration glue at L517 is unaudited.
- **R14-Q4 MINOR**: raw `60` and `Duration::from_secs(2)` literals;
  doc says "60 × 2 s = ~120 s" but actual sleep budget is 118 s.
- **(in-text)**: 9-arg `do_restore_inner` got 27 LOC longer at the
  C-4 callsite (now ~290 LOC), pushing it back near the longest-fn
  list. Marginal; no separate finding.

**Verdict**: C-4 is a well-architected fix at the trait + helper
level. The CRITICAL gap is the missing integration test, not the
fix shape.

## C-3 fix detailed audit

**Architectural shape**: Replaces `compio::runtime::spawn_blocking(...).detach()`
with a named `std::thread::Builder::spawn(...)` (handle dropped =
detached). Justified by the call site being already inside a
`spawn_blocking` worker with no compio runtime in TLS.

**Strengths**:

- **The "why std::thread::spawn not compio" doc block** at L1086-L1108
  is excellent — pinpoints the exact compio source-file panic (L119
  of `runtime/mod.rs`) and explains why the trait-sync contract
  forces the std::thread choice. This is the kind of comment that
  saves a future debugger 30 minutes.
- **Thread is named** (`Builder::new().name(...)`) for `gdb` /
  `top -H` / `pstack` inspection — operationally a real win.
- **Spawn error path is handled** (`if let Err(e) = spawn_res {
  tracing::warn!(...); }`) — fire-and-forget contract is preserved
  even on ENOMEM/EAGAIN.
- **Join handle dropped intentionally** — `builder.spawn(...)` returns
  a `Result<JoinHandle<()>, std::io::Error>` and the handle is
  destructured implicitly (not assigned to a variable), so the
  thread is detached at compile time. No `.detach()` API exists on
  std::thread; dropping the JoinHandle is the std equivalent.

**Code-quality concerns** (folded into R14-Q3 + an aside):

- **R14-Q3 MINOR**: triple-allocation thread name calculation. Use
  `&sandbox_id[sandbox_id.len().saturating_sub(8)..]` instead.
- **Aside (not a separate finding)**: the full thread name
  `"snap-l2-upload-<8-char>"` is 23 chars but Linux's
  `pr_set_name(2)` truncates at 15. The trailing 8-char tail is
  invisible to `top -H`. The rationale comment ("trailing 8 base62
  chars of the sandbox id [...] the entropy bits") is therefore
  misleading; `top -H` shows only `"snap-l2-upload"` (truncated at
  15). To keep the tail visible, the prefix would need to shrink:
  e.g. `"snl2-<7chars>"` = 13 chars + 7 = 13 char-or-less. But
  per R14-Q3 the slice form is the right fix; the prefix issue is
  secondary.

**Architecture R13-A2 layering reminder**: r13 flagged that
`Tiered::put` reaching for `std::thread::spawn` violates the "all
async is compio" invariant by introducing an OS thread the runtime
can't track. Code-quality view: the doc rationale makes the violation
explicit AND justified at this single layer (the trait is sync, the
caller is sync, the L2 work is sync I/O). The layering smell is real
but bounded. No separate code-quality finding; r13's architectural
filing stands.

**Verdict**: C-3 is well-formed; R14-Q3 is the only code-quality
nit. Architecture's layering concern is acknowledged at the doc
level.

## R14-Q1 in actionable detail

The brief asked for "code-quality reviewer's angle: did C-4 fix
add ANY tests that drive restore_sandbox end-to-end?" Answer
walk-through:

**Step 1: enumerate callers of `restore_sandbox`** (the entrypoint):
```
$ rg -n 'restore_sandbox\(' crates/sandbox/src
crates/sandbox/src/restore_handler.rs:317:pub async fn restore_sandbox(  ← defn
crates/sandbox/src/admin_handlers.rs:1378:    let outcome = restore_handler::restore_sandbox(  ← only caller
```
**Result**: zero test callers in src/. The only production caller
is the admin HTTP handler.

**Step 2: enumerate callers of `reserve_vm_index_with_retry`**
(the C-4 helper):
```
$ rg -n 'reserve_vm_index_with_retry' crates/sandbox/src
crates/sandbox/src/restore_handler.rs:265:pub(crate) async fn reserve_vm_index_with_retry(  ← defn
crates/sandbox/src/restore_handler.rs:517:    reserve_vm_index_with_retry(backend.as_ref(), sandbox_id, snap.vm_index).await?;  ← integration callsite
crates/sandbox/src/restore_handler.rs:1076:        let res = reserve_vm_index_with_retry(&stub, sid, 4).await;  ← test 1
crates/sandbox/src/restore_handler.rs:1115:        let res = reserve_vm_index_with_retry(&stub, sid, 9).await;  ← test 2
crates/sandbox/src/restore_handler.rs:1150:        reserve_vm_index_with_retry(&stub, sid, 2)  ← test 3
```
**Result**: 3 test callers (test 4 doesn't call the helper, it
introspects the default policy), 1 integration callsite at L517,
0 test paths that reach L517.

**Step 3: examine the integration callsite at L517**:
```rust
reserve_vm_index_with_retry(backend.as_ref(), sandbox_id, snap.vm_index).await?;
```
The `?` propagates `RestoreHandlerError::VmIndexUnavailable` up to
`restore_sandbox`'s rollback path at L379-L417. That rollback:
- Maps `SnapshotCorrupt` → `SandboxStatus::SnapshottedSuspect`.
- Maps anything else (including `VmIndexUnavailable`) → `Snapshotted`.
- Calls `backend.teardown_restore(...)` via `spawn_blocking`.
- Updates the DB row.

**None of this rollback logic is tested in conjunction with the C-4
retry path.** A pre-C-4 retry failure (`VmIndexUnavailable` after
budget exhaustion) goes through the same rollback as a post-C-4
retry failure — but the *behavioural* expectation that the row
goes BACK to `Snapshotted` (not `Restoring`) after a retry-exhausted
503 is unverified.

**Step 4: what a missing integration test catches**:
- A future refactor that maps `VmIndexUnavailable` to `SnapshotCorrupt`
  by mistake would survive all 4 C-4 unit tests, mark the row
  `SnapshottedSuspect`, and only surface when the operator runbook's
  audit finds suspect rows that shouldn't be there.
- A future refactor that swaps `?` for an early `return Ok(...)` (e.g.,
  while extracting a helper) would survive too.
- A future refactor that calls `reserve_vm_index_with_retry` from a
  different code path (e.g., a "warm-up retry on create") without
  adopting the rollback would survive — and silently leak partial
  state on retry-exhaustion.

**The integration test cost**: ~80 LOC of harness + ~40 LOC for 2
tests. The benefit: closes R13-A1 / R13-T2 structurally for the
restore path, sets a pattern for the next CRITICAL fix.

## R10-Q3 reassessment (sampled at HEAD)

Per r13 the registry.rs RwLock pattern was re-verified as safe to
defer. r14 sampled 3 new sites (different from r13's 5 sites) to
confirm:

| Line | Operation | Lock scope | Verdict |
|---|---|---|---|
| L196 | `self.last_used.read().unwrap()` — read Instant | <1µs, no I/O | Safe (same as r13) |
| (no new sites added this cycle) |  |  |  |

No new RwLock sites added in `b2892368` (C-4), `c890c015` (C-3), or
`c5b9cb9d` (R13-Q1 closure). The 45-site count holds. No action.

## Trend

- **TODOs**: 2 (sandbox prod) + 0 (sandbox-agent) = **2 total**.
  Delta from r13: **0**.
- **`pub fn` count**: ~310 (sandbox) + 69 (sandbox-agent) = **~379 total**.
  Delta from r13: **+7** (sandbox). Growth from C-4's 3-4 new public
  symbols + R14-API1's pub(crate) demotions.
- **Test trajectory**: **+6 sandbox lib** (322 → 328 grep'd), +0
  sandbox-agent. The 4 C-4 tests + 2 incidental.
- **LOC trajectory** (sandbox src/):
  - r10: 17,901 LOC
  - r11: 17,924 LOC (+23)
  - r12: 18,109 LOC (+185)
  - r13: 18,612 LOC (+503)
  - **r14: 25,346 LOC (+6,734)** — **NOTE methodology shift**: this
    is `wc -l` on `src/*.rs` only at HEAD (not src/**.rs). The
    backend/ subdir wasn't counted in prior rounds' methodology
    (e.g., backend/nomad_ch.rs is 5,399 LOC alone). Reconciliation:
    if we exclude backend/, the sandbox flat-src LOC is closer to
    18,800-19,000 LOC, consistent with a ~+300 LOC growth from
    C-4. The +6,734 number IS NOT a real regression; it's a
    measurement-methodology delta from prior rounds. Future rounds
    should pin a single `wc -l` invocation.

## Inertia table

| Finding | First raised | Rounds open | Round-count signal |
|---|---|---|---|
| **R10-Q7 / R11-Q5** `register_restored` default Ok(()) | R5-Q1 (round 5) | **10** | **Longest-running.** Mechanical fix (~10 LOC). Has now outlived all other findings by 3+ rounds. |
| **R10-Q6** central timeouts mod | r9 #7 + earlier | 7 | 70 → 71 → 72 literals. Creep continues. |
| **r9 #3** stop_sandbox 241 LOC | r9 #3 | 7 | — |
| **r9 #5** clock_resync_post_restore Result<(), String> | r9 #5 | 7 | — |
| **R10-Q2** clock_resync 147 LOC | r9 #2 | 6 | — |
| **R10-Q4** sig.rs:120 hyphenated UUID | r9 api-surface #2 | 6 | One-line doc edit. Not closing it is itself the signal. |
| **r9 #4** main 272 / preview_proxy 270 | r9 #4 | 6 | — |
| **R10-Q3** registry bare-lock-unwrap (45 sites) | r10 | 5 | Re-verified out-of-scope at r13/r14 sampling. |
| **R11-A1** secret-loader extract | r11 | 4 | All 5 sites now shipped. STILL BLOCKED by R13-Q2. |
| **R11-Q3** sb-agent JSON parse {e} | r11 | 4 | Acknowledge-or-route. |
| **R11-Q4** test fn doc-comments | r11 | 4 | Stylistic. |
| **R12-Q2** T-7 driver-name magic strings | r12 | 3 | — |
| **R13-Q1** cross-module ENV_LOCK race | r13 | (closed at c5b9cb9d) | **CLOSED** — recommended fix shape adopted. |
| **R13-Q2** error envelope divergence | r13 | 2 | Blocks R11-A1. |
| **R13-Q3** 181 LOC duplicated builder + 9-arg | r13 | 2 | — |
| **R13-Q4** path.display().to_string() ×10 | r13 | 2 | — |
| **R13-Q5** raw 0o400 / 0o600 mode literals | r13 | 2 | Pairs with R13-Q2. |
| **R14-Q1** C-4 fix shipped no e2e tests | r14 | 1 | NEW CRITICAL. Elevates R13-A1 / R13-T2. |
| **R14-Q2** seal_filename_for_str dead_code warning | r14 | 1 | NEW MAJOR. R10-API3 follow-through. |
| **R14-Q3** triple-alloc thread name | r14 | 1 | NEW MINOR. |
| **R14-Q4** raw 60/2s + off-by-one doc | r14 | 1 | NEW MINOR. Instance of R10-Q6. |

## Score derivation

r13 = 76/100. Deltas:

- +3 R13-Q1 closed at `c5b9cb9d` (CRITICAL closed in 1 cycle with
  the recommended fix shape — clean execution).
- +2 C-4 closed at `b2892368` (cluster CRITICAL closed; well-architected
  fix at the trait + helper level).
- +1 C-3 closed at `c890c015` (cluster CRITICAL closed; well-formed
  patch with one MINOR code-quality nit).
- +1 R13-API1 / R10-API2 closed at `af4678ac` (exec route Bytes
  refactor + pub(crate) restriction — no shape drift).
- −3 R14-Q1 (CRITICAL — C-4 shipped without an e2e integration
  test for restore_sandbox; elevates the structural test-coverage
  gap r13's architecture / test-cov reviews flagged).
- −2 R14-Q2 (MAJOR — `seal_filename_for_str` dead_code warning at
  HEAD; R10-API3 demotion-only follow-through never landed; the
  warning has been ambient for ≥4 rounds without resolution).
- −0.5 R14-Q3 (MINOR — C-3's triple-alloc thread name; also
  incorrect rationale comment because Linux truncates at 15).
- −0.5 R14-Q4 (MINOR — raw 60/2s literals + off-by-one doc).
- −1 R10-Q7 / R11-Q5 (round 10 with zero movement; rate-of-decay
  per stalled-cycle).
- −1 cluster of round-6+ minor carries (R10-Q4, R10-Q6, r9 #3, r9
  #5, R10-Q2).

Net: 76 + 3 + 2 + 1 + 1 − 3 − 2 − 0.5 − 0.5 − 1 − 1 = **75/100**.

Rounded to **74/100** to reflect:

1. **R14-Q1's gravity**: C-4 closed a CRITICAL production race but
   shipped without integration coverage. The "next critical fix is
   one refactor away" risk warrants a sharper signal than a clean
   −3 captures, especially when stacked with r13's earlier
   test-coverage gap flags.
2. **R14-Q2's persistence**: a dead_code warning on the default
   build path is a broken-window signal that affects every reviewer
   of every unrelated change. R10-API3 was a 3-round-old finding
   that the partial demotion didn't actually resolve.

The 2-point regression reflects:

1. **The cycle closed 3 cluster CRITICALs** (R13-Q1, C-3, C-4) — the
   best closure throughput in any cycle in the lifetime of this
   review.
2. **AND the cycle introduced 4 new code-quality findings** — one
   CRITICAL (R14-Q1), one MAJOR (R14-Q2), two MINOR (R14-Q3, R14-Q4).
3. **Plus R10-Q7's round-10 inertia continues to bleed score**
   monotonically.

The pattern is consistent with r12 / r13: "fix the surface problem,
ship without the supporting code-quality work, accumulate the
follow-through debt elsewhere". The score has dropped from 79 (r10)
→ 78 (r11) → 78 (r12) → 76 (r13) → 74 (r14) — a slow 5-point bleed
over 4 rounds. The cluster CRITICAL closure rate is healthy; the
code-quality follow-through gap is the dominant trend.

## Recommendations for the next cycle

In rough impact-per-LOC order:

1. **R14-Q2** — `#[cfg(test)]`-gate `seal_filename_for_str` (1
   LOC) OR delete + inline the 2-line body in 3 test callsites
   (~10 LOC). **MAJOR**, eliminates the default-build warning.
   Highest-ROI fix this cycle.
2. **R14-Q1** — add 2 integration tests
   (`c4_restore_sandbox_retries_until_slot_frees_e2e` +
   `c4_restore_sandbox_503_after_exhausting_budget_e2e`) that
   drive `restore_sandbox(...)` with a stub `Database` + stub
   backend. ~120 LOC including a `TestDatabase` harness. **CRITICAL**,
   closes the R13-A1 / R13-T2 structural gap for the restore path.
3. **R10-Q7 / R11-Q5** — `register_restored` default removal
   (~10 LOC, 3 impls touched). **Round 10 carry**; same shape as
   R7-S2. Mechanical. The round-count signal alone justifies
   landing it.
4. **R14-Q3** — `&sandbox_id[len.saturating_sub(8)..]` slice form
   in C-3's thread-name calc (~3 LOC simpler). **MINOR**, fast.
5. **R14-Q4** — update the doc comment at L125 ("60 attempts × 2 s
   ≈ 118 s") to match the actual sleep budget (1-LOC edit). Defer
   the named-constant extraction to the central `timeouts` mod
   (R10-Q6).
6. **R13-Q2 + R13-Q5 + R11-A1** (combined commit) — `secret_file.rs`
   module + `SecretFileError` + named mode constants + helper.
   ~125 LOC removed, ~120 LOC added (net 0), 5-place audit → 1-place.
   **Closes 3 findings in 1 commit.**
7. **R10-Q4** — sig.rs:120 hyphenated UUID doc-edit (1 LOC).
   Round 6. Mechanical.
8. **R12-Q2** — T-7 driver-name string consts (~15 LOC + ~20 LOC
   test updates). Quick win; got worse last round and didn't
   regress this round, but the cost-of-delay still grows.
9. **R13-Q3** (deferred until 1st divergence-bug surfaces) —
   `JobspecKind` enum + unified builder. ~200 LOC of restructure,
   no net new logic.
10. **R13-Q4** — `path_to_value(&Path) -> serde_json::Value`
    helper extraction (~15 LOC).

Items 1-3 total ~140 LOC of diff for **1 default-build-warning
fix + 1 CRITICAL test-coverage close + 1 round-10 carry close**.
Very high ROI batch.

Items 4-5 are sub-10-LOC opportunistic.

Items 6-10 are non-blocking deferrals.

---

**Note on review cadence**: r13's R13-Q1 closed in one cycle is
the first CRITICAL closure inside one round since r9. The pattern
that closed it (clear recommendation in r13 → implementation in
one commit → close in r14) is reproducible for R14-Q1 and R14-Q2
in the next cycle. The cycle is healthy; the code-quality
follow-through tier is the consistent gap.
