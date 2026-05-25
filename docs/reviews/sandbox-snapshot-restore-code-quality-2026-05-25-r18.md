# Sandbox snapshot-restore code-quality review — 2026-05-25 r18

**Reviewer**: code-quality-r18 (cron-pilot)
**HEAD**: `370d13e6`
**Prior round**: r17 (HEAD `a0888d9e`)
**Lens**: code-quality
**Scope**: GATE-C2 (3 commits `1cfc9182` + `678ec197` + `db248cbf`) + C-7-LT-1 (`f9996fcf`). +smoke-r12 script triplet (`f9a9c5f0` + `7664b4b0` + `0f4b1a98`) reviewed for ops-side discipline only.

## Summary
- **4 findings**: 0 critical, 1 important, 3 minor.
- `cargo test -p zeroship-sandbox --lib --release`: **402 pass / 1 ignored** (+6 over r17's 396, exactly matching the +3 GATE-C2 + +3 C-7-LT-1 claim).
- `cargo test -p zeroship-sandbox --tests --no-run`: clean compile. 2 lib warnings remain (unused `SandboxAuth` import; unused `WAKE_JOBS_T_KEEP` const) — both pre-existing, not r18-introduced.
- GATE-C2 closes the R17 CRITICAL (R17-C2 wake-POST TOCTOU). The partial UNIQUE INDEX + `ON CONFLICT (sandbox_id) WHERE … DO NOTHING` is the *correct* atomic primitive (handler-side TX bracket was the alternative and would have been heavier).
- C-7-LT-1 cleanly threads `WakeResponseMode` through `VmIndexRetryPolicy::from_host_fence_timeout`. Sync branch preserves the C-8a/C-8b dual-ceiling MIN verbatim; Async branch drops the 50 s deadline cap and pins `2×fence + HEADROOM`. Both branches use `saturating_{mul,add,sub}` end-to-end — overflow-safe for any `host_fence_timeout_secs: u64`.
- The Pre-existing failing tests (`wake_machine_drives_snapshotted_to_ok`, `wake_machine_classifies_livez_failure`) are **not actual code bugs** — they fail only because the local pg fixture (`postgres://…:5440`) is unreachable in this worktree. Both are `#[ignore = "needs Postgres"]` and run only when the operator brings up `docker compose up -d postgres`. See "Pre-existing failing tests" below.

## CRITICAL

None.

## IMPORTANT

### [R18-I1] `make_machine` test fixture discards `InsertWakeJobOutcome`, silently masking a regression that the seeded sandbox already has a non-terminal wake row
- **File**: `crates/sandbox/tests/sandbox_pg_e2e.rs:4674`
- **Snippet**:
  ```rust
  db.insert_wake_job(&row).await.unwrap();
  ```
- **Issue**: post-GATE-C2 the signature is `Result<InsertWakeJobOutcome>`, but the test fixture pattern of "seed a fresh sandbox, insert ONE wake row, drive it" relies on `Inserted`. If a future refactor (or a test-ordering bug under `--test-threads=1`) leaves a non-terminal row from a prior test, the call returns `Replay(...)` and the test silently drives the OLD wake-id row while asserting on the new one — surfacing as `wake_job row must exist after drive` panicking on the freshly-minted `wake_id` even though the machine ran fine on someone else's row.
- **Suggested fix**: `assert!(matches!(db.insert_wake_job(&row).await.unwrap(), InsertWakeJobOutcome::Inserted));` — same one-line cost as the existing `unwrap()`, but catches the silent-replay regression that the unique-index landing now structurally enables.
- **Why now**: r18 is the first round where the silent-replay path even exists. Hardening the fixture before the test-suite scales further reduces "weird intermittent failure" debugging in PR3+.

## MINOR

### [R18-M1] `insert_wake_job` follow-up SELECT can theoretically race the winner to terminal — current behaviour is `DatabaseError::Validation` which surfaces as 500 to the client
- **File**: `crates/sandbox/src/db.rs:3043-3057`
- **Issue**: the conflict path reads via `find_pending_wake_for_sandbox` (which filters `state NOT IN ('ok','failed')`). If the winner's state machine drives all the way through to a terminal state in the microseconds between the conflicting INSERT and the follow-up SELECT, the SELECT returns `None` and the function returns `Validation(...)`. The doc at `db.rs:3008-3014` acknowledges this ("shouldn't happen in practice (state-machine transitions take ≫ 1ms)") and frames silent re-insertion as the worse alternative — that framing is correct.
- However: an explicit retry-once-with-relaxed-filter pattern would be strictly more robust (i.e. on `None`, SELECT for the latest row regardless of state and return `Replay(that)` with the now-terminal state surfaced through the same envelope). The handler-side replay branch already passes `existing.state.as_str()` straight into the JSON, so a terminal state would render correctly. Trade-off is more code for a vanishing-rare path; current behaviour is acceptable but worth a deferred note.
- **Suggested fix**: deferred — keep current `Validation` surfacing but pin a unit test that constructs the race shape via a manual SQL teardown between the two queries, so a future refactor sees the contract.

### [R18-M2] R17-Q1 / doc inflation in `from_host_fence_timeout` is now ~103 lines (was ~80 pre-r17), driven by another commit-stamped block
- **File**: `crates/sandbox/src/restore_handler.rs:184-287`
- The C-7-LT-1 graft adds 22 doc lines (266-287: a fresh "C-7-LT-1" section + 6 worked examples by mode). Each prior section (C-7, R14-A6, C-8a, C-8b) followed the same accrete-by-commit pattern, so the doc now reads as "five commit messages stacked" rather than "one focused contract statement". Useful for code-archeology, hostile for first-time readers trying to learn the formula.
- **State of R17-Q1**: still present and structurally worse. A future round should consider extracting the commit-history narrative into a sibling `docs/decisions/` ADR (one ADR for the whole C-7 family) and leaving the rustdoc tight on the *current* contract.
- **Not blocking**: every paragraph carries a contract claim and the unit tests at `r14a6_*` + `c8b_*` + `c7_lt_1_*` each cite their respective doc block, so the inflation is at least navigable.

### [R18-M3] R17-Q2 / off-by-one observation: the `+1` in `attempts_from_budget` is correct but the doc examples mis-state the wall-time vs. attempt count in one place
- **File**: `crates/sandbox/src/restore_handler.rs:340` (formula) + L278-285 (doc examples)
- The formula `(effective_budget / INTERVAL_SECS).saturating_add(1)` is correct: `max_attempts = N+1` produces `N` sleeps × `INTERVAL_SECS` = `effective_budget` seconds of wall-time. At fence=30 async, budget=70 → 36 attempts → 35 sleeps × 2s = 70 s wall-time. Correct.
- The doc at L280-281 says "2*30 + 10 = 70 s → 36 attempts × 2 s = 70 s budget" — but 36×2 = 72, not 70. The wall-time math is `(attempts - 1) × interval`, not `attempts × interval`. This same minor doc-math slip is repeated in commit `f9996fcf`'s message (and r17 flagged the analogous pattern in r17-M2's section). The asserts in `c7_lt_1_async_mode_fence_30_yields_70s_budget` are correct (`wall_ms = interval * (attempts - 1)`); only the doc prose is loose.
- **State of R17-Q2**: not actually an off-by-one in the formula, but a *doc-prose* off-by-one that has now been replicated into the new C-7-LT-1 section. Easy textual fix; deferred.

### [R18-M4] R17-Q3 / silent Failed/Internal fallback at `wake_job_row_from_pg` is still present (r17-M2 unchanged)
- **File**: `crates/sandbox/src/db.rs:1588`
- ```rust
  state: WakeJobState::from_str_opt(state_str).unwrap_or(WakeJobState::Failed),
  ```
- The CHECK constraint at migration 0009 prevents any out-of-domain `state` from existing in the row, so this fallback is structurally unreachable. r17 recommended either (a) `expect()`-ing the in-domain invariant or (b) pinning a coverage test for the fallback. Neither has happened in r18.
- The companion silent-fallback in `wake_machine::classify_failure` (`_ => WakeErrorCode::Internal` at `wake_machine.rs:601`) is *defensive* (the comment at L597-600 explicitly justifies it as defense-in-depth on pre-flight errors) and is appropriate as-is.
- **State of R17-Q3**: still open at `db.rs:1588`. Low risk (CHECK prevents the case), but the "panic loudly on impossible state" version would be a clearer operator contract.

## Pre-existing failing tests

`wake_machine_drives_snapshotted_to_ok` and `wake_machine_classifies_livez_failure` (and the rest of `wake_machine_e2e::*`) are `#[ignore = "needs Postgres; C-7-LT-PR2 wake_machine …"]`. Direct invocation in this worktree fails at `crates/sandbox/tests/sandbox_pg_e2e.rs:38` with `Connection refused` against `postgres://postgres:zeroship@localhost:5440/zeroship` — pg fixture port `5440` is not bound in this environment (a `pgvector/pgvector:pg16` on `5432` exists for an unrelated marketplace service, but the sandbox fixture explicitly uses `5440`).

**Verdict: environmental, not a code bug.** No flagging required. The same tests would also need the C-7-LT-PR2 wake_machine module to be wired (the test imports `zeroship_sandbox::wake_machine::WakeMachine` and constructs it directly) — verified the import resolves and the struct is `pub` with `pub` field-init for `database`/`backend`/`snapshot_store`/`persist`/`sandbox_id`/`wake_id`/`lessee`. Build is clean.

## Cross-lens consensus

- **GATE-C2 is the correct primitive.** Partial UNIQUE INDEX + `ON CONFLICT (col) WHERE predicate DO NOTHING` is the standard pg pattern for "at-most-one row per dimension under a state predicate"; PostgreSQL infers the partial unique index from the `WHERE` clause on the ON CONFLICT and uses it for the conflict target. Postgres-correct, race-free atomic, no advisory locks needed. Migration name `wake_jobs_sandbox_pending_uniq` is distinct from any existing index (verified via grep on migrations/) so `CREATE UNIQUE INDEX IF NOT EXISTS` cannot collide with a non-unique pre-existing index of the same name.
- **C-7-LT-1 mode threading is clean.** The `WakeResponseMode` is resolved once at boot (`lib.rs:647`, fail-CLOSED via `from_env()?`), threaded through `RealRestoreBackend::with_wake_response_mode` (builder, returns `self`), and read once per wake-retry via `vm_index_retry_policy()`. The `Default = Sync` on the field is appropriate: every fixture constructor (`StubRestoreBackend`, unit tests at `restore_handler.rs:2898+`) gets the pre-C-7-LT-1 budget shape without needing to know about the new field, while production wiring uniformly overrides via the builder.
- **Saturating math everywhere.** `from_host_fence_timeout` uses `saturating_mul` (L304), `saturating_sub` (L324, L326), `saturating_add` (L336, L340). Overflow at `u64` math would require `host_fence_timeout_secs > u64::MAX / 2 ≈ 9.2e18`; the config CHECK constraints cap this at a few thousand. Panic-safe.

## Lens hand-off — concurrency / architecture / api-surface

1. **Concurrency**: r18 IMPORTANT R18-I1 (test fixture should assert `Inserted` outcome explicitly) is concurrency-adjacent — the silent-replay risk only materialises under test ordering races, which concurrency cares about. Flagging for r19.
2. **Architecture**: r18 MINOR R18-M2 (doc inflation in `from_host_fence_timeout`) is the long-tail ADR-extraction recommendation. Suggest the architecture lens consider whether a `docs/decisions/2026-05-XX-c7-family-retry-policy.md` would absorb the C-7 / R14-A6 / C-8a / C-8b / C-7-LT-1 history and let the rustdoc tighten to "what the function does today".
3. **Api-surface**: GATE-C2 added `InsertWakeJobOutcome` (`Inserted` | `Replay(WakeJobRow)`) as a new `pub` symbol on `Database`. R18 has no api-surface findings on it but flag for r19 the question of whether `Replay` should carry the *full* `WakeJobRow` or just a typed-id (`wake_id`) — the handler only reads `existing.wake_id` and `existing.state.as_str()`, so the rest of the row is dead weight on the API.
4. **Test coverage**: r18 MINOR R18-M1 (deferred terminal-mid-race pin for `insert_wake_job`) is one for the test-cov lens.
5. **No regressions**: lib tests 402/0/1, +6 over r17. Saturating-math discipline preserved; no new `.unwrap()` / `.expect()` in production code; no new `#[allow(dead_code)]`; thread-name discipline (15-byte cap) preserved across the GATE-C2 + C-7-LT-1 diffs.
