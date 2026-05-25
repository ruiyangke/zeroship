# Sandbox snapshot-restore code-quality review — 2026-05-25 r20

**Reviewer**: code-quality-r20 (cron-pilot)
**HEAD**: `dea68995`
**Prior round**: r19 (HEAD `b8654600`)
**Lens**: code-quality
**Scope**: R19-C1-PR1 (`1d3724fe` db.rs +118 / migrations/0012 / tests/sandbox_pg_e2e.rs +273) + R19-C1-PR2 (`8d163d58` sweep.rs +156 / config.rs +99 / lib.rs +14) + R19-I1 two-phase probe (`82478a6b` nomad_ch.rs +369/-4) + R19-I4 retry loop (`f2485210` db.rs +153/-46) + r17-Q2 doc fix (`ce66c10f` restore_handler.rs +18/-7) + R14-API2/R18-API2/r19-A4 doc rewrite (`8e085598` restore_handler.rs +30/-13).

## Summary

- **6 findings**: 0 critical, 1 important, 5 minor.
- `cargo test -p zeroship-sandbox --lib --release`: **425 pass / 1 ignored / 0 failed** (+11 over r19's 414; matches the per-commit claims: PR1 +4 pg-gated only / PR2 +5 lib / R19-I4 +2 lib).
- `cargo build -p zeroship-sandbox --tests`: **2 warnings, unchanged from r19** (unused `SandboxAuth` import in `restore.rs:43`; unused `WAKE_JOBS_T_KEEP` const in `sweep.rs:96`). **No new warnings from any of the six commits.**
- R19-C1 ships clean: PR1 lands the `WakeErrorCode::WakeWorkerAborted` variant + migration 0012 + `claim_orphan_wake_for_recovery` SQL; PR2 wires the 60 s cadence + `MIN_TAKEOVER_THRESHOLD_SECS=30` floor + `detach_isolated("wake-takeover", ...)` spawn shape. The four-way co-closure (R19-C1 + R18-A1 + R17-T4 + R19-A2) is accurate — all four findings tracked the same writer-without-reader gap on `lessee_updated_at`.
- R19-I1 two-phase probe correctly composes Phase 1 `probe_agent_reachable_tcp(addr, 150ms)` (PR1's reusable function) + Phase 2 retained ureq `/livez`. The +3 contract tests (`happy_socket_then_livez_ok` / `socket_never_accepts_returns_timeout_clean` / `socket_accepts_late_succeeds_within_budget`) pin the wedge-fix invariant — an unroutable TEST-NET-1 address must time out in ≤3 s on a 700 ms budget, never the kernel's 30 s SYN-retransmit ceiling.
- R19-I4 3-iteration retry is bounded and observable: `INSERT_WAKE_JOB_MAX_RETRIES = 3` constant, debug-log per retry iteration with attempt counter, distinct error variant on exhaustion with grep-able message shape.
- r17-Q2 doc off-by-one closed at `ce66c10f` — all five example lines now read `(N−1 sleeps × interval)` with the formula note prepended; matches the `(attempts - 1) * INTERVAL_SECS` math at L353.
- R18-API2 (`with_wake_response_mode` pub → pub(crate)) lands clean; the only caller is `lib.rs:785`, in-crate.

## CRITICAL

None.

## IMPORTANT

### [R20-I1] `from_host_fence_timeout` doc inflated from ~103 → ~118 lines after r19-A4 NON-NORMATIVE banner; R19-M2 ADR-extraction recommendation now overdue

- **File**: `crates/sandbox/src/restore_handler.rs:184-301` (118 lines of rustdoc above a 60-line function body)
- **Issue**: r19-M2 recommended either (a) prepending a `**NON-NORMATIVE**` marker + tightening the doc, OR (b) extracting the commit-stamped narrative to a `docs/decisions/` ADR. The r19-A4 doc rewrite at `8e085598` chose path (a) — it added the NON-NORMATIVE banner *and* a 14-line two-path empirical model — but did NOT remove the original C-8a/C-8b/C-7-LT-1 historical narrative. Net change: +15 lines of new explanatory prose without the corresponding deletion. The doc now carries BOTH the disclaimed-as-wrong model and its retraction inline.
- **Snippet** (L235-254 — the new content layered on top of the legacy narrative):
  ```rust
  /// **60.164 s for `host_fence=30 s`** — exactly 2× the fence.
  ///
  /// **NON-NORMATIVE teardown model (see smoke-r13 retrospective for
  /// empirical ground truth)**: the 2× ratio at fence=30 s turned out
  /// to be a numeric coincidence, not a compositional model. ...
  ///   - **Agent-dies path**: ...
  ///   - **Agent-hangs path**: ...
  /// We retain `teardown_estimate = 2 * host_fence_timeout_secs` as a
  /// conservative safety margin, not as a model. ...
  ```
- **Why this matters now**: future operators reading this rustdoc will spend their first-pass cost twice — once on the original C-8b 2× model, once on the disclaimer. The banner mitigates incorrectness but compounds inflation. The r19-M2 path (b) recommendation (extract to `docs/decisions/2026-05-XX-c7-family-retry-policy.md`) is now structurally cheaper than continuing to layer.
- **Suggested fix**: extract L214-283 (C-8a / C-8b / C-7-LT-1 historical narrative + NON-NORMATIVE banner) to an ADR under `docs/decisions/`. Leave the rustdoc with: the formula, the constants table, and the examples (L256-300). Reduces the doc to ~50 lines, preserves all historical context behind a single `See docs/decisions/...` link.
- **Status**: r19-M2 carried; r17-Q1 doc bloat compounded by r19-A4 layering. Promoted from MINOR to IMPORTANT this round because the inflation crossed a readability threshold (118 lines now exceeds the function body itself).

## MINOR

### [R20-M1] `claim_orphan_wake_for_recovery` overwrites `error_message` unconditionally — defensible, but the SQL hard-codes the breadcrumb string; consider lifting to a const for grep + i18n hygiene

- **File**: `crates/sandbox/src/db.rs:3334-3337`
- **Snippet**:
  ```rust
  error_message = \
      'wake worker aborted: controller did not \
       complete the wake within the timeout \
       (see operator runbook)', \
  ```
- **Issue**: the breadcrumb string is inline in the UPDATE SQL. The doc at L3306-3312 explains the overwrite-not-COALESCE contract (deliberate, contrasts with `update_wake_job_state`'s COALESCE behaviour). But a future test that asserts on the exact text (or an operator dashboard that pattern-matches) would couple to a literal embedded in SQL, where rg-by-message-text only finds it via the multi-line continuation.
- **Suggested fix**: hoist to a `const WAKE_TAKEOVER_ERROR_MESSAGE: &str = "..."` near `WakeErrorCode::WakeWorkerAborted` and interpolate with `format!` / parameterised SQL. Pure tidy-up; not blocking.
- **State**: new in r20.

### [R20-M2] `claim_orphan_wake_for_recovery` uses `RETURNING wake_id` but discards the rows — the doc justifies it as "symmetric with future debug logging", but the unused query path is a smell

- **File**: `crates/sandbox/src/db.rs:3324-3329`
- **Snippet**:
  ```rust
  // RETURNING wake_id lets us COUNT the rows updated — even on
  // pg drivers where `execute()` returns rowcount, RETURNING is
  // the canonical "what did I touch" surface and keeps the
  // shape symmetric with future debug logging that wants the
  // ids.
  let rows = client.query("UPDATE ... RETURNING wake_id", &[&secs]).await?;
  Ok(rows.len() as u64)
  ```
- **Issue**: the comment is honest about the trade-off, but `client.execute()` on `compio-postgres` does return the rowcount directly — the `RETURNING wake_id` + `query()` + `rows.len()` adds a wire round-trip's worth of row data that's thrown away. Two options: (a) drop RETURNING and use `client.execute(...)` for a smaller wire size, OR (b) actually log the ids at TRACE level so the symmetry the comment promises is realised.
- **State**: new in r20. Not a behaviour bug — the SQL is correct and the row count is accurate.
- **Suggested fix**: option (b) is cheaper — add a `tracing::trace!(target: "sandbox::wake::takeover", claimed_ids = ?rows.iter().map(|r| r.get::<_, String>(0)).collect::<Vec<_>>(), ...)` once `tracing_max_level_trace` is enabled, then the comment's "future debug logging" is the current debug logging. Deferred.

### [R20-M3] R19-I1's Phase 2 retains `ureq::get(...).timeout(500ms)` — the inner timeout is still request-deadline, not connect-deadline; the wedge surface shrinks but doesn't vanish

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3130-3140`
- **Snippet**:
  ```rust
  let livez_status = compio::runtime::spawn_blocking(move || {
      ureq::get(&probe_url)
          .timeout(Duration::from_millis(500))
          .call()
          .map(|r| r.status())
          .ok()
  }).await...
  ```
- **Issue**: Phase 1 gates SYN-blackhole correctly. But once Phase 1 succeeds (TCP ACK), Phase 2 spends a `spawn_blocking` + ureq call with `.timeout(500ms)` — ureq's `.timeout` is still request-deadline (covers connect + write + first-byte-read collectively). If the agent's HTTP server accepts the connection then hangs reading headers (rare but observable when the agent process is mid-fork or the userspace HTTP stack is wedged), Phase 2 still burns up to 500 ms per probe. At a 150 ms cadence + 500 ms worst-case probe, the effective cadence floats to 650 ms — outside the 150 ms designed cadence.
- **Why it's MINOR**: the failure shape (TCP accept + HTTP hang) is much rarer than the SYN-blackhole shape Phase 1 closed; the outer 700 ms / `agent_livez_timeout` budget still caps total wall-time; the +1 contract test `socket_never_accepts_returns_timeout_clean` already pins the worst case.
- **Suggested fix**: document the residual 500 ms-per-probe ceiling in the doc above `wait_for_agent_livez` (or in `parse_agent_probe_addr`) — current docstring promises "Phase 2 bounded by the outer deadline" which is true but elides the per-probe ceiling. Two-line addition.
- **State**: new in r20.

### [R20-M4] R19-I4 retry loop reuses the same pooled `client` across all 3 attempts — if the first INSERT fails with a transient pg error, the client is in an uncertain state for retries 2 + 3

- **File**: `crates/sandbox/src/db.rs:3059-3084`
- **Issue**: the `client = pool.get().await?` is acquired ONCE outside the `for attempt in 0..INSERT_WAKE_JOB_MAX_RETRIES` loop. If `client.execute(...)` returns an error (mapped to `DatabaseError::Pg` via `?` at L3084), the function exits — that's correct, no retry-on-pg-error semantics, only retry-on-conflict-then-none semantics. But the loop body has no `.map_err(...).transpose()`-style escape valve: a connection blip on retry attempt 2 would still propagate, which is correct, but the comment block above the loop doesn't explain that retry is conflict-shape-only, not error-shape.
- **Status**: not a bug — the existing semantics (retry only on `rows_affected == 0` + `find_pending → None`) are the intended contract; pg errors short-circuit. But the rustdoc at L3044-3053 doesn't make the "retry only on conflict, never on pg error" contract explicit. A future reader could assume the retry handles transient `client` errors too.
- **Suggested fix**: append to L3053 doc: *"Pg errors short-circuit out of the loop unchanged; the retry handles only the conflict-then-terminal-race shape."* One-line addition.
- **State**: new in r20.

### [R20-M5] R19-M1 sanitizer CIDR-table refactor still open — `match_rfc1918_at` unchanged at `wake_machine.rs:751-833` since R17-S1

- **File**: `crates/sandbox/src/wake_machine.rs:751-833`
- **State**: unchanged since r19. The function still has the 172.16/12 + 100.64/10 dynamic-second-octet duplication R19-M1 flagged. No new prefix landed this round, so the breakeven point ("if a fourth dynamic-second-octet block lands") has not been crossed.
- **Suggested fix**: same as r19-M1 — table-driven refactor deferred until the next IANA-reserved block is added.

### [R20-M6] R19-M5 `insert_wake_job_fresh` helper still uncreated — 10 fixture sites still copy the 6-line `assert!(matches!(...))` block

- **File**: `crates/sandbox/tests/sandbox_pg_e2e.rs` (10 sites per `git show 531db5c3 --stat` — unchanged since R18-I1)
- **State**: unchanged since r19. R19-I4 added +2 unit tests but no new pg-gated fixture sites this round; the breakeven point ("one new site") has not been crossed.
- **Suggested fix**: same as r19-M5 — extract on next fresh-insert fixture site.

## Cross-lens consensus

- **R19-C1 ships clean.** SQL is parameterised correctly (`$1::BIGINT` for the threshold; no string interpolation, no SQL injection surface). The `state NOT IN ('ok', 'failed')` WHERE clause inside the UPDATE serialises concurrent takeover races at the pg row-lock level — exactly one caller can transition a given row. The threshold floor (`MIN_TAKEOVER_THRESHOLD_SECS = 30`) is justified by the wake-ladder's worst single-stage timeout (the agent /livez poll at 30 s); a 60 s default gives a one-stage safety margin. The poll cadence (`WAKE_JOBS_TAKEOVER_POLL_SECS = 60`) is hard-coded — operators tune the *threshold*, not the cadence, which is the right invariant since the SQL is cheap.
- **R19-I1's reuse of `probe_agent_reachable_tcp`** is correct — same compio-native function PR1 introduced; no new wedge surface at the TCP layer. Phase 2's residual 500 ms-per-probe is the only remaining concern (R20-M3).
- **R19-I4 is correctly bounded.** Hard limit of 3 documented in the const + doc; exhaustion produces a distinct `DatabaseError::Validation` variant with a grep-able message ("pathological rapid-transition race"). The +2 unit tests pin both the variant + the message shape so a future rename can't silently break operator-visible diagnostics.
- **Visibility tightening (`with_wake_response_mode` pub → pub(crate))**: verified `Grep` for callers — only `lib.rs:785` calls it, in-crate. No orphan callers; the demotion is safe.
- **No new `unwrap()` / `expect()` in production code** across all six commits. The only `unwrap` is in test fixtures (`std::net::TcpListener::bind(...).unwrap()` etc.). Saturating-math discipline preserved (`secs as i64` is the lone narrowing cast in `claim_orphan_wake_for_recovery`; behaviour matches the pre-existing `gc_expired_wake_jobs` pattern at L3262).

## Lens hand-off — concurrency / architecture / api-surface / test-coverage / performance

1. **Concurrency**: R19-C1's `detach_isolated("wake-takeover", ...)` correctly isolates the takeover sweep from the shared ntex runtime — same pattern as `spawn_wake_jobs_gc`. The R20-M4 retry-on-pg-error contract clarification is concurrency-adjacent (multi-attempt loop semantics). Flag for r20+.
2. **Architecture**: R20-I1 is structurally an architecture concern (where does a 118-line commit-stamped narrative live — rustdoc or ADR?). Defer to architecture-r20 for the ADR-extraction call.
3. **Api-surface**: R18-API2 (`with_wake_response_mode` pub → pub(crate)) verified clean — no orphan callers. R19-C1's `claim_orphan_wake_for_recovery` is `pub async fn` on `Database`; the in-crate sweep is the only caller — api-surface lens may consider pub(crate) demotion for consistency.
4. **Test coverage**: R19-C1 pg-gated suite (4 tests) covers stale / fresh / terminal / concurrent-race semantics. R19-I1 contract tests (3 new) cover happy / unroutable / late-bind paths. R19-I4 unit tests (2 new) cover the retry-outcome variants + error message shape. Test discipline is high; the only DRY-it-up open question is R20-M6 (helper extraction).
5. **Performance**: R20-M2 (`RETURNING wake_id` + `rows.len()` discards the data) is a micro-performance flag; the row-data wire cost is bounded by the orphan count (rare event, typically 0-1 rows per sweep). Performance-r20 to weigh whether the trade-off is worth changing.
6. **No regressions**: lib tests 425/0/1, +11 over r19. No new warnings. Saturating-math discipline preserved. Visibility tightening reduces api-surface (positive). Two pre-existing warnings (`SandboxAuth` import, `WAKE_JOBS_T_KEEP` const) carried — neither introduced this round.

## Carried-finding status

| Finding | Source | r20 state |
| --- | --- | --- |
| r17-Q1 (doc inflation in `from_host_fence_timeout`) | r17 → r18-M2 → r19-M2 | **WORSE** — doc grew 103 → 118 lines after r19-A4 banner. Promoted to R20-I1. |
| r17-Q2 (doc off-by-one `36 × 2 = 70`) | r17 → r18-M3 → r19-M3 | **CLOSED** at `ce66c10f`. All five example lines now read `(N−1 sleeps × interval) = Xs`. |
| r17-Q3 (silent `WakeJobState::Failed` fallback at `db.rs:1588`) | r17 → r18-M4 → r19-M4 | **OPEN** (unchanged). Migration 0009 CHECK still makes it unreachable. Cosmetic only. |
| R18-I1 (test-fixture hardening) | r18 → r19 closed | **CLOSED** at `531db5c3` per r19. |
| R19-C1 (wake_jobs takeover sweep CRITICAL) | r19 CRITICAL | **CLOSED** at `1d3724fe` + `8d163d58`. Co-closes R18-A1 / R17-T4 / R19-A2. |
| R19-I1 (`wait_for_agent_livez` two-phase probe) | r19 IMPORTANT | **CLOSED** at `82478a6b`. |
| R19-I4 (`insert_wake_job` 3-iteration retry) | r19 IMPORTANT | **CLOSED** at `f2485210`. |
| R19-M1 (sanitizer CIDR-table refactor) | r17-S1 → r19-M1 | **OPEN** — carried to R20-M5. No new prefix landed; breakeven not crossed. |
| R19-M5 (`insert_wake_job_fresh` helper extraction) | r18 → r19 | **OPEN** — carried to R20-M6. No new fresh-insert fixture site this round. |
