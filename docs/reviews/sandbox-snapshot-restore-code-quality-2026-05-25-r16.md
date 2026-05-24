# Sandbox snapshot-restore code-quality review — 2026-05-25 r16

**Reviewer**: code-quality-r16 (cron-pilot)
**HEAD**: 2e9ae598
**Prior round**: r15 (HEAD `b8fae7b7`)
**Lens**: code-quality

## Summary
- 4 findings: 0 critical, 1 important, 3 minor.
- Surface area this cycle: C-8b commit `64af1803` (146 LOC churn,
  one source file: `restore_handler.rs`) + pin bump `2e9ae598`.
- `cargo build -p zeroship-sandbox --lib`: **0 warnings** (clean
  default build, unchanged from r15).
- No new `.unwrap()` / `.expect()` / `panic!` in production code.
  Defensive `unwrap_or(u32::MAX)` at L294-295 is sound (and is
  structurally dead — see R16-Q3).
- No new `#[allow(dead_code)]` and no `#[cfg(test)]`-only items
  leaking into `pub` API.
- C-8b's `from_host_fence_timeout` arithmetic is panic-safe
  (`saturating_mul(2)`, `saturating_sub`, `saturating_add(1)`,
  `try_from(...).unwrap_or(u32::MAX)`). Only divide-by-zero risk
  is `effective_budget / INTERVAL_SECS` where `INTERVAL_SECS = 2`
  is a const — safe at HEAD but with no compile-time guard.

## CRITICAL

None.

## IMPORTANT

### [R16-Q1] `from_host_fence_timeout` doc grew to 82 lines of fix-history above a 37-line body; the summary "Formula:" line is now misleading on its own
- **File**: `crates/sandbox/src/restore_handler.rs:184-302` (`VmIndexRetryPolicy::from_host_fence_timeout`)
- **Excerpt** (L195-199 — the summary formula):
  ```rust
  /// Formula: `max_attempts = (teardown_estimate.saturating_sub(CLIENT_HEADROOM_SECS)) / INTERVAL_SECS + 1`
  /// where `teardown_estimate = 2 * host_fence_timeout_secs` (see
  /// C-8b note below). The `+ 1` accounts for the first (zero-sleep)
  /// attempt, so the wall-time `(max_attempts - 1) * INTERVAL_SECS`
  /// lands exactly at `(teardown_estimate - HEADROOM)` seconds.
  ```
- **Symptom**: the "Formula:" line states the *fence-derived ceiling
  in isolation* — but the actual implementation takes MIN of two
  ceilings (fence and deadline). The dual-ceiling shape only
  surfaces 25 lines later in the C-8a paragraph (L214-229). A reader
  skimming the doc summary will derive the WRONG attempt count for
  `host_fence_timeout_secs ≥ 30` (where the deadline ceiling
  binds). The author of `c8b_default_policy_envelopes_doubled_fence`
  caught this in the test math comment at L1675-1678 (which writes
  the full MIN expression correctly), but the function's own
  doc-summary doesn't.
- **Why IMPORTANT**: This is the third successive fix
  (R14-A6 + C-8a + C-8b) layered into a single doc block, each
  preserving the prior text and appending a "**XXX fix**:" paragraph.
  The block is now **82 doc lines + 37 code lines = 119 LOC** for
  one factory function (67% comment density). Three doc-block grafts
  in three smoke cycles is a code-quality drift signal: every future
  smoke fix that touches this function repeats the pattern, and the
  summary will drift further from the implementation. The architecture-r15
  R15-A3 finding flagged `restore_handler.rs` as #2 file in the
  crate at 3444 LOC; this function's doc bloat is one of the
  contributors.
- **Action** (recommended):
  - **Option A** (recommended): rewrite the "Formula:" line to state the
    actual MIN expression: `max_attempts = (min(teardown_estimate, CLIENT_DEADLINE_SECS).saturating_sub(CLIENT_HEADROOM_SECS)) / INTERVAL_SECS + 1`,
    plus the `teardown_estimate = 2 * host_fence_timeout_secs`
    note. ~2 LOC doc edit.
  - **Option B** (better): demote the R14-A6 + C-8a + C-8b
    fix-history paragraphs to an ADR (`docs/decisions/2026-05-25-vm-index-retry-policy.md`)
    or to a `// HISTORY:` block at the impl's bottom. Keep the
    function's `///` doc to (i) the current formula, (ii) the
    constants, and (iii) the post-C-8b examples table. Net ~−50
    LOC of doc, +60 LOC of ADR (one-time, immutable). Pairs with
    R15-A3 (file-size triage).
  - Either way the test contracts at L1567-L1631 and L1648-L1685
    remain the truth-anchor — the test math is correct.

## MINOR

### [R16-Q2] C-8b's `(N - 1) × INTERVAL = wall-time` shape persists in the new examples table — same off-by-one shape R14-Q4 has flagged for two rounds
- **File**: `crates/sandbox/src/restore_handler.rs:248-261` (the new C-8b examples table)
- **Excerpt** (L249-251):
  ```rust
  /// - `host_fence_timeout_secs = 30` → teardown_est=60 s,
  ///   fence-ceil=50 s, deadline-ceil=50 s, MIN=50 s → 26 attempts ×
  ///   2 s = 50 s budget. (Was 11 attempts / 20 s pre-C-8b — that
  ```
- **Symptom**: "26 attempts × 2 s = 50 s budget" reads as
  `N × interval = budget`, but the actual wall-time is
  `(N - 1) × interval = (26-1)*2 = 50 s` because sleep is between
  attempts (first attempt is zero-sleep). The arithmetic happens to
  work for fence=30 (where `(26-1)*2 = 50` matches "26 × 2"
  modulo the off-by-one because `26*2=52` not `50`), but the
  shape is misleading. The same shape repeats at L253-255 ("26 ×
  2 = 50"), L256-258 ("26 × 2 = 50"), L259-261 ("16 × 2 = 30",
  actual `(16-1)*2 = 30 s` happens to match if read as
  `15*2=30`).
- **Why MINOR**: This is the same shape r14-Q4 flagged
  ("~50 s claimed, 48 s actual at 25 attempts × 2 s") and r15-Q4
  carried as PARTIALLY-SUPERSEDED. C-8b's rewrite **re-introduced
  the same pattern** at a new constants set. The test at L1652-
  L1653 uses the correct `interval * (max_attempts - 1)` shape, so
  the math anchor is the test, not the doc. The off-by-one shape
  persists for the **third successive smoke cycle**.
- **Action**: rewrite each example as `→ 26 attempts; wall-time
  (26-1) × 2 s = 50 s`. ~5 LOC across the 4 examples. Pairs with
  the r15-Q4 PARTIALLY-SUPERSEDED carry.

### [R16-Q3] `u32::try_from(...).unwrap_or(u32::MAX)` at L294-295 is structurally dead — the deadline ceiling caps the input at 50, never overflowing u32
- **File**: `crates/sandbox/src/restore_handler.rs:293-296`
- **Excerpt**:
  ```rust
  let attempts_from_budget = (effective_budget / INTERVAL_SECS).saturating_add(1);
  let max_attempts = u32::try_from(attempts_from_budget)
      .unwrap_or(u32::MAX)
      .max(MIN_ATTEMPTS);
  ```
- **Symptom**: `effective_budget = min(teardown_estimate - 10, 60 - 10)
  = min(*, 50)`, so `effective_budget ≤ 50`, so
  `attempts_from_budget ≤ 50/2 + 1 = 26`. `u32::try_from(26)` always
  succeeds. The `unwrap_or(u32::MAX)` branch is unreachable at the
  current MIN-of-two design. Defensive coding, but the dead branch
  signals the author didn't reason about whether `effective_budget`
  could ever exceed `u32::MAX as u64` — it cannot, by construction
  (the deadline ceiling is a `u64` literal that fits in `u32`).
- **Why MINOR**: Defensive `unwrap_or` against impossible overflow is
  legitimate (and the existing R10-Q3 audit treats `.unwrap()` /
  `unwrap_or` defensively-justified sites as out-of-scope). But the
  comment block doesn't explain *why* the fallback exists — a future
  reader will assume `effective_budget` can be arbitrarily large
  and may design a new ceiling-removal feature on that assumption.
- **Action**: either (a) replace with `as u32` cast (the value is
  provably ≤ 26 at the deadline ceiling), with a `debug_assert!(
  attempts_from_budget <= u32::MAX as u64)` for paranoia; or
  (b) add a `// effective_budget is capped at 50 by
  CLIENT_DEADLINE_SECS - CLIENT_HEADROOM_SECS, so try_from never
  fails in practice; the unwrap_or guards against a future ceiling
  removal.` comment. ~2 LOC doc OR ~3 LOC code change.

### [R16-Q4] C-8b inherits the per-attempt INFO log (R15-Q2) and doubles its volume at fence=30 — 11→26 attempts/wake
- **File**: `crates/sandbox/src/restore_handler.rs:437-454` (the
  per-attempt INFO log inside `reserve_vm_index_with_retry`)
- **Excerpt** (L447-453):
  ```rust
  tracing::info!(
      target: "zeroship_sandbox::restore_handler",
      sandbox_id = %sandbox_id,
      attempt = attempt,
      max_attempts = attempts,
      vm_index = vm_index,
      "restore/wake: reserve_vm_index_with_retry attempt"
  );
  ```
- **Symptom**: R15-Q2 (the per-attempt INFO log) computed log volume
  at "25 attempts × c=20 stress = 500 INFO lines/cycle". Post-C-8b,
  the cluster-smoke fence=30 case now yields **26 attempts**
  (was 11 pre-C-8b: the pre-C-8b cluster fence=30 derivation gave
  11 attempts / 20 s). So the cluster's actual per-wake log
  volume more than DOUBLED in cycles where the wake exhausts —
  26 lines × c=20 = 520 lines/cycle from a single retry probe,
  *plus* the original 25-attempt happy path on conservative fences
  still has the same 500/cycle baseline. The R15-Q2 finding's
  "fine at c=20" doc rationale at L445-L446 didn't get updated
  with C-8b.
- **Why MINOR**: R15-Q2 already files the per-attempt log as
  bounded-but-loud; C-8b silently grew the bound by 2.4×. The
  comment at L444-L446 ("Volume is bounded by the policy's
  `max_attempts` per wake — fine at c=20") references a 25-attempt
  bound that's no longer the cluster-smoke shape. A reader who
  reads the comment, checks the default, and concludes "25 × 20 =
  500" will undercount by ~2.4× under the new cluster default.
- **Action**: pairs with R15-Q2 (action carries forward). When the
  per-attempt log demotion lands (smoke-r11+ trigger), update the
  comment at L444-L446 to reference both ceilings (`policy.max_attempts
  ∈ [1, 26]` at HEAD's constants). ~1 LOC doc.

## What r15 said that I confirm still live

- **[R15-Q1]** CLOSED at `7469118e` (verified — `admin_handlers.rs:1354-1357`
  uses byte-slice form matching R14-Q3). No regression.
- **[R15-Q2]** per-attempt INFO log noise — **STILL OPEN**, **AMPLIFIED**
  by C-8b (see R16-Q4). The log lacks the smoke-r9 / smoke-r11
  follow-through sentinel R15-Q2 recommended.
- **[R15-Q3]** 3 detach sites without shared helper — **STILL OPEN**.
  C-8b didn't touch detach sites. R14-A1 helper extract still
  deferred. CreateGuard::drop at `nomad_ch.rs:2002` remains
  sibling-C-6 (admin-reachable).
- **[R14-Q4]** (PARTIALLY-SUPERSEDED) the `(N-1) × interval` off-by-one
  shape persists — **CONFIRMED RE-INTRODUCED** in C-8b's new examples
  table (see R16-Q2). Now round 3.
- **[R10-Q7 / R11-Q5]** `register_restored` default `Ok(())` — STILL
  OPEN at L373-L381 (relocated +132 LOC from r15's L240-L242 due to
  the C-8b doc growth). **Round 12** — longest-running.
- **[R10-Q6]** 65 `Duration::from_secs(N)` + 44 `from_millis(N)` —
  C-8b added 0 new literals (used existing `INTERVAL_SECS`). Round 9.

## Lens hand-off

- **Architecture (R15-A3)**: file-size triage on `restore_handler.rs`
  (now `prev 3162 + ~118 LOC C-8b ≈ 3280 LOC` at HEAD) overlaps with
  R16-Q1's doc-bloat recommendation. Coordinated fix: when extracting
  `retry_policy` module per R15-A3, demote R14-A6 + C-8a + C-8b doc
  paragraphs to a co-located `HISTORY.md` or to ADR
  `docs/decisions/2026-05-25-vm-index-retry-policy.md`. ~−80 doc LOC
  + ~+250 LOC retry_policy module split.
- **Concurrency (R15-I2)**: the headroom-shape question (subtract
  CLIENT_HEADROOM when fence wins MIN) was the precursor to C-8b's
  re-baseline. C-8b changed the *teardown estimate* (1× → 2× fence)
  but did NOT change the headroom-when-fence-wins shape R15-I2
  flags. Worth a re-check from concurrency-r16.
- **Test-coverage**: the new `c8b_default_policy_envelopes_doubled_fence`
  test (L1648-L1685) is a strong unit pin — assertions on (i) ≥21
  attempts floor, (ii) ≤50_000 ms ceiling, (iii) exactly 26 attempts.
  Three layered assertions catch refactor regressions of fence factor,
  headroom shape, and exact arithmetic respectively. Good pattern;
  worth surfacing to test-coverage-r16 as a template for future
  retry-policy regressions.
- **Note for next code-quality round**: C-8b is doc-heavy but
  arithmetic-clean. The dominant code-quality bleed this cycle is
  doc bloat and the persisting `(N-1)*interval` off-by-one — both
  recurring patterns. The next round should observe whether the
  R15-Q2 + R16-Q4 per-attempt log demoted (smoke-r11 trigger) and
  whether R16-Q1's doc-extract option B landed. If neither moved,
  the doc-history-as-source pattern has reached round-3 inertia and
  warrants architectural rather than code-quality treatment.
