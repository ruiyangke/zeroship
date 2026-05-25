# Sandbox snapshot-restore code-quality review — 2026-05-25 r17

**Reviewer**: code-quality-r17 (cron-pilot)
**HEAD**: `a0888d9e`
**Prior round**: r16 (HEAD `2e9ae598`)
**Lens**: code-quality
**Scope**: C-7-LT-PR1 — 7 commits `3d8acc23..a0888d9e` (detach helper, admin/sweep/registry/lib/gcs migrations to `detach_isolated`, `WakeResponseMode`, migration 0009, `WakeJobRow`/state/error/CRUD).

## Summary
- **7 findings**: 0 critical, 2 important, 5 minor.
- Surface area: +1400 / -105 across 10 files; one new module (`detach.rs`, 244 LOC) + one new migration + ~340 LOC of new pg CRUD + ~350 LOC of new e2e tests.
- `cargo build -p zeroship-sandbox --release`: **clean compile** (the only post-build error is an unrelated FS-permission write to `target/release/`).
- `cargo test -p zeroship-sandbox --lib`: **360 pass / 1 ignored** (+13 over r16's 347, exactly as advertised).
- No new `.unwrap()` / `.expect()` in production code; all sites are in `#[cfg(test)]`. No new `#[allow(dead_code)]`. No new sloppy `pub` items (the two new public symbols, `detach_isolated` and `WakeResponseMode`, are correctly module-public; `WakeJobRow` + variants are consumed by PR2 and `tests/sandbox_pg_e2e.rs` so must be `pub`).
- SQL: all 5 wake_jobs queries use `$N` placeholders; the one `format!`-built statement (`update_wake_job_state`) interpolates a **statically-chosen literal** (`", ready_at = now()"` or `""`), so the injection surface is zero.
- Thread-name discipline (15-byte kernel limit): **one over-limit name** ships (R17-I1).

## CRITICAL

None.

## IMPORTANT

### [R17-I1] `snap-health-loop` thread name is 16 bytes — silently truncated to `snap-health-loo` by the kernel, contradicting the rest of the R16-I1 family
- **File**: `crates/sandbox/src/lib.rs:1117`
- **Issue**: `detach.rs` module docs (L51-57) commit to the contract "callers SHOULD pass a short prefix (≤ 15 chars)". Other migrated sites land at 12-15 bytes (`snap-takeover`=13, `snap-heartbeat`=14, `snap-transient`=14, `snap-idle-gc`=12, `snap-idle-evict`=15, `snap-l2-upload-<tail>` truncates to `snap-l2-upload-`). `snap-health-loop` is **16 bytes**, so `pr_set_name` will truncate to `snap-health-loo` in `ps`/`top -H` — the very symptom the rest of the family is meticulous about avoiding.
- **Snippet**:
  ```rust
  crate::detach::detach_isolated("snap-health-loop", move || async move {
      loop {
          // ...
  ```
- **Suggested fix**: rename to `snap-health` (11 bytes) or `snap-healthloop` (15 bytes); also pin a unit-test in `detach.rs` that walks all known callers' names and asserts `≤ 15` bytes (the doc says "callers SHOULD pass", but nothing enforces it).
- **Rust-side name** (`std::thread::current().name()`) is preserved verbatim by `detach_isolated`, so `tracing` consumers that pick up the thread name are unaffected; the symptom is OS-level (debugger / `ps` / `top -H`) only.

### [R17-I2] `update_wake_job_state` rewrites server-set `error_code`/`error_message`/`agent_url` on every call — there is no way to advance state while preserving an existing error field
- **File**: `crates/sandbox/src/db.rs:2973-3012`
- **Issue**: the parameters `error_code: Option<WakeErrorCode>` and `error_message: Option<&str>` are bound directly into `error_code = $2::TEXT, error_message = $3::TEXT`. `None` writes `NULL` to those columns — there is no `COALESCE($N, error_code)` fallback (which the function _does_ have for `agent_url` at L2993). Concretely, if PR2's state machine writes `(Failed, Some(LivezTimeout), Some("..."), None)` and then a follow-up `update_wake_job_state(_, Failed, None, None, None)` runs (idempotent retry, takeover replay, …), the error metadata silently disappears.
- **Snippet**:
  ```rust
  "UPDATE sandbox.wake_jobs \
      SET state = $1::TEXT, \
          error_code = $2::TEXT, \
          error_message = $3::TEXT, \
          agent_url = COALESCE($4::TEXT, agent_url), \
          updated_at = now() \
          {ready_at_clause} \
    WHERE wake_id = $5::TEXT"
  ```
- **Why now**: PR1 ships the CRUD as the API contract for PR2. If PR2 lands and ships a retry/replay path before this asymmetry is fixed, observability ("why did wake X fail?") regresses on every replay. Either (a) make all three follow `agent_url`'s `COALESCE` pattern, or (b) document that callers MUST pass `Some(existing)` to preserve, and pin a test that demonstrates the contract.
- **Pinned tests** at `tests/sandbox_pg_e2e.rs:172-197` exercise only the "set on transition to failed" direction; the "preserve across re-update" path is not tested.

## MINOR

### [R17-M1] `detach_isolated` swallows the future's return value silently — `let _ = rt.block_on(fut)` drops `Result::Err` without logging
- **File**: `crates/sandbox/src/detach.rs:100`
- **Snippet**:
  ```rust
  let fut = make_fut();
  let _ = rt.block_on(fut);
  ```
- Currently all callers return `()` so `T` is unit and there is nothing to log. But the signature accepts `F: Future<Output = T>, T: 'static`, so a future that returns `Result<(), E>` would have its `Err` discarded with no trace. The module doc commits to "fire-and-forget" semantics — the discard is intentional — but the helper would benefit from constraining `T = ()` to make that contract type-level explicit, or matching on `T: Debug` and `tracing::warn!`-ing non-unit returns. Not blocking.

### [R17-M2] `WakeJobRow.state_str` round-trip falls back to `Failed` on unknown discriminator — the test in `db.rs:3214-3232` exercises `from_str_opt` returning `None`, but no test asserts the from-pg coercion
- **File**: `crates/sandbox/src/db.rs:1539-1556` (`wake_job_row_from_pg`)
- **Comment claim**: "Unknown discriminator strings round-trip as `Failed` / `Internal` (defense in depth — the CHECK constraint should keep the column in-domain, but a row inserted by a forward-incompatible binary shouldn't crash the reader)."
- The CHECK constraint **prevents** a row with a junk state from existing in the first place (validated by `wake_jobs_state_check_constraint_enforced` at `tests/sandbox_pg_e2e.rs:332-364`), so the defense-in-depth path is structurally unreachable today. Either drop the fallback (let `unwrap_or` become `expect("CHECK constraint guarantees in-domain")`) — which would make a forward-incompatible binary panic loudly instead of producing a phantom `Failed` row that confuses operators — or add a unit test that constructs a `compio_postgres::Row` with an out-of-domain string and asserts the substitution behaviour.

### [R17-M3] `WakeResponseMode::from_env` reads the env at boot only — operators cannot toggle without restart, but the doc on `AppState::wake_response_mode` doesn't say so
- **File**: `crates/sandbox/src/lib.rs:160-172` and `crates/sandbox/src/config.rs:881-905`
- The flag is correctly resolved once in `AppState::new` (L802-806) and stored on `AppState`; reading at boot is right (no env-mutation at runtime is a key invariant). The doc on the field says "PR2 reads this flag in the wake handler", but doesn't say "this is a static, boot-time read; SIGHUP / config-reload is not supported." A one-line addition would set the operator expectation.

### [R17-M4] `gc_expired_wake_jobs` allows `older_than == 0`, which on a heavily-loaded box may delete a row that's still in flight
- **File**: `crates/sandbox/src/db.rs:3060-3078`
- The SQL is `updated_at < now() - make_interval(secs => $1::BIGINT)`. With `older_than == 0`, the predicate becomes `updated_at < now()`, which matches every terminal row regardless of how recently it terminated. The pinned test at `tests/sandbox_pg_e2e.rs:296-317` explicitly relies on this for assertion convenience, so the behaviour is intentional — but a production caller passing `Duration::ZERO` by accident would aggressively GC rows the client hasn't observed yet (the 202-Accepted shape *requires* the client to be able to poll and observe `Ok` at least once before GC). Add a `debug_assert!(older_than >= Duration::from_secs(1))` or a brief doc note that production callers should pass a value larger than the client's polling interval.

### [R17-M5] `WakeResponseMode::from_env` test fixture uses `unsafe { std::env::set_var }` with a private `ENV_LOCK` — there is already a similar lock at `db::tests`; both could share a workspace-level fixture
- **File**: `crates/sandbox/src/config.rs:911-942`
- Style/dedup nit. The comment at L913-917 correctly cites the pre-existing pattern in `db::tests`, but the two mutexes are still distinct, so a future test that exercises both env spaces in parallel (e.g. an integration test that sets `SANDBOX_WAKE_RESPONSE_MODE` AND a db env var) won't be protected by either lock. Workspace-wide test fixture (a `tests/common.rs` or shared `dev-deps` helper) would prevent the future trip-hazard. Not blocking PR1.

## Cross-lens consensus

- **detach.rs is a high-quality extraction.** The factory-closure shape, `Send + 'static` bounds, log-and-drop semantics, panic isolation (each future runs on its own OS thread, so a panic inside `rt.block_on(fut)` is contained), and pinned tests for happy/immediate/panic/many/name-preservation match the C-3/C-6 contract that was open-coded at the three pre-existing sites. Comment density (lines 1-57 of module doc) is high but each paragraph carries a contract claim (why factory, why dedicated thread, why error-level on failure, why ≤15-char name) — consistent with the project's "comments explain why" convention.
- **No tokio leakage.** All migrated sites use `compio::time::sleep`, `compio::runtime::Runtime::new`, and `compio::runtime::spawn`'s `detach()` is replaced cleanly; zero `tokio::` references in the diff.
- **Wake job state types mirror the schema 1:1** — `WakeJobState` and `WakeErrorCode` enum variants match the migration's CHECK constraint values exactly. Adding a variant to the enum without bumping the migration would NOT round-trip — comment at `db.rs:1442-1444` (the `WakeJobState` doc) and `migrations/0009_wake_jobs.sql:54-79` (the CHECK domain) explicitly call out this coupling.
- **Test coverage is honest.** PR1 e2e tests exercise insert/get round-trip, all transitions Pending→Restoring→Ok and Pending→Failed, the idempotency-lookup contract (returns only non-terminal rows), GC discriminates terminal vs. non-terminal, and the CHECK constraint actually rejects bogus state literals. The five tests at `tests/sandbox_pg_e2e.rs:3651-3994` are gated `#[ignore = "needs Postgres"]` consistent with the rest of the file.

## Lens hand-off — PR2 code-quality gates

1. **Thread-name discipline**: when PR2 adds new spawn sites (wake state-machine driver, takeover sweep for non-terminal rows), pre-commit-check all thread names against the 15-byte kernel limit. Suggest adding a `detach::tests::all_known_names_fit_kernel_limit` test that walks a static `&[&str]` of every name used in the crate (or, better, asserts the bound *inside* `detach_isolated` and emits a `debug_assert!` warning if the name is over-limit).
2. **`update_wake_job_state` field-preservation**: PR2 must NOT call `update_wake_job_state(_, _, None, None, None)` after a failure has been recorded, or the error metadata gets nulled (R17-I2). Either fix the function (`COALESCE` all three optional fields) or pin a test that demonstrates the "preserve on re-update" contract before PR2 wires any retry path.
3. **`gc_expired_wake_jobs` minimum threshold**: PR2's sweep task must pass `older_than >= max(client_polling_interval, takeover_lease_ttl)` — the client must observe `Ok` at least once before the row can be GC'd. Either enforce via `debug_assert!` in the function, or document the minimum in the doc comment with a forward reference to PR2's sweep cadence env var.
4. **`WakeJobRow.state_str` fallback path**: drop the silent `Failed` substitution OR add a coverage test (R17-M2). PR2 should not ship a code path that depends on the fallback — it should rely entirely on the CHECK constraint for correctness, and panic loudly if the fallback ever fires.
5. **Static-read semantics of `WakeResponseMode`**: PR2 must NOT add a runtime env-reload path; the flag is boot-time only by design. If PR2 needs per-app overrides, add them as request-scope plumbing, not env-reread (R17-M3).
