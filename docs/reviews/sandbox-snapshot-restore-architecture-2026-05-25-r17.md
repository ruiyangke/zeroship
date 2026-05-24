# Sandbox snapshot-restore architecture review — 2026-05-25 r17

**Reviewer**: architecture-r17 (PR1 landing review)
**HEAD**: `a0888d9e` (`feat/sandbox-snapshot-restore`)
**Scope**: C-7-LT-PR1 just-landed surface (7 commits, +13 lib tests).
**Lens**: architecture (read-only; no code edits).

## Summary

PR1 lands the scaffolding for the async-wake state machine (`wake_jobs` table + CRUD + `WakeResponseMode` flag) AND closes R14-A1 / R16-I1 by extracting `detach_isolated` and migrating 8 call sites onto it. The detach helper is sharp — module-level docs name the !Send constraint, factory shape, kernel naming truncation, and all three failure modes (panic, ENOMEM, runtime-construction); 5 tests cover them. The 5 producer-side migrations are byte-for-byte semantic preserving (verified diff-by-diff against the pre-refactor blocks). The schema gets indexes right (partial on non-terminal `state`, supporting GC range scan, supporting idempotency lookup).

The CRITICAL finding is in the CRUD surface, not the table: `update_wake_job_state` never touches `lessee_updated_at`, so any wake whose mid-flight exceeds the takeover threshold will be stolen out from under its still-progressing owner. Two IMPORTANT findings warn that PR1 left one C-6-shaped site unmigrated (`CreateGuard::drop` in `nomad_ch.rs`) and that the takeover sweep PR2 needs is not actually supported by the indexes that landed.

3 critical / important findings; 2 minor; 0 closed-item re-flags.

## CRITICAL

### [r17-A1] `update_wake_job_state` never bumps `lessee_updated_at` — PR2's takeover sweep WILL steal active wakes mid-flight

- **Location**: `crates/sandbox/src/db.rs:2973-3012` (`update_wake_job_state`); migration intent at `crates/sandbox/migrations/0009_wake_jobs.sql:26-28` ("takeover discipline mirroring the `sandboxes` table").
- **What I see**: The SQL at `:2988-2997` updates `state`, `error_code`, `error_message`, `agent_url`, `updated_at`, and (conditionally) `ready_at`. **It does NOT touch `lessee_updated_at`.** So `lessee_updated_at` is frozen at the value `INSERT` server-defaulted (`now()` at insert time).

  The C-6 wake itself runs through six non-terminal states (Pending → ReservingSlot → Restoring → LivezPolling → ClockResyncing → Registering). Empirical wake budget (per R15-A1 / C-8b) lives at ~50 s under the synchronous contract; under the async contract PR2 is removing that ceiling, so wakes can legitimately sit in `Restoring` or `LivezPolling` for tens of seconds. If the takeover threshold matches the `sandboxes.lessee_updated_at` discipline (~30s in `sweep.rs`), the takeover sweep will see a row with a `lessee_updated_at` that's older than threshold and conclude the owner crashed — even though `update_wake_job_state` was just called 200 ms ago.

  Mirror site: `sweep.rs::run_transient_takeover_once` (`crates/sandbox/src/sweep.rs:130-210`) uses `claim_orphan_transient_for_recovery` which CAS-fences on `(host_id, generation, lessee_updated_at still stale)`. PR2 will write its analogue against `wake_jobs`, and that analogue will fire on healthy rows.

- **Why it matters**: This is the exact concurrency-bug-shape R14-C1 / C1-FOLLOWUP closed for `sandboxes`. Re-introducing it on the very next table the project lands is the kind of architectural drift that's invisible at PR1 review (no sweep yet) but lights up the moment PR2 wires the takeover scan.

- **Fix sketch** (for PR2):
  1. Add `lessee_updated_at = now()` to the SQL at `db.rs:2988-2997`. Trivial; one line.
  2. Alternative: introduce a separate `renew_wake_job_lease(wake_id, lessee)` method that bumps only `lessee_updated_at`, and require the wake state machine to call it on every state transition. More invasive; less ergonomic.
  3. Add a `pg_e2e` test that walks `Pending → ReservingSlot → Restoring`, sleeps past the eventual takeover threshold, and asserts the row's `lessee_updated_at` is recent. Without this assertion the bug regresses silently.

## IMPORTANT

### [r17-A2] Wake-job takeover index is NOT supported by the landed schema — PR2's takeover sweep will full-scan the non-terminal partial index, not a `(lessee, lessee_updated_at)` index

- **Location**: `crates/sandbox/migrations/0009_wake_jobs.sql:106-118` (the three landed indexes).
- **What I see**: The three indexes are:
  1. `wake_jobs_sandbox_idx` on `(sandbox_id)` — idempotency lookup. Correct.
  2. `wake_jobs_state_idx` on `(state)` partial WHERE non-terminal — *named* for the takeover scan, but only indexes `state`. The takeover scan needs `WHERE state NOT IN ('ok','failed') AND lessee_updated_at < now() - threshold`. With this index alone, pg will scan all non-terminal rows and filter by `lessee_updated_at` in-memory.
  3. `wake_jobs_updated_at_idx` on `(updated_at)` — GC sweep. Correct.

  None of them is keyed on `(lessee, lessee_updated_at)` or `(lessee_updated_at)` to support the actual takeover query. At low load this is fine — non-terminal rows are bounded by the wake budget. But the lessee-CAS pattern in `sandboxes` uses an index on `(lessee_updated_at)` (see `sandbox.sandboxes` schema in `0001_initial.sql`) precisely so the sweep doesn't degenerate.

- **Why it matters**: this is the kind of "schema looks right at PR1, sweep performance degrades at PR2 scale" trap. The fix is cheap (one more index) but has to happen *in PR2's migration*, not later, because a forward-only schema means every PR2-after-PR2 fix carries its own version bump cost.

- **Fix sketch**: PR2's wake-handler migration adds either:
  - `CREATE INDEX wake_jobs_lessee_updated_at_idx ON sandbox.wake_jobs (lessee_updated_at) WHERE state NOT IN ('ok','failed');` (compound partial — direct support for the takeover query), or
  - `CREATE INDEX wake_jobs_lessee_idx ON sandbox.wake_jobs (lessee, lessee_updated_at);` (broader; supports per-host wake enumeration too).

### [r17-A3] `CreateGuard::drop` in `nomad_ch.rs` still uses `compio::runtime::spawn(...).detach()` — same C-6 fingerprint, NOT migrated by PR1's R16-I1 sweep

- **Location**: `crates/sandbox/src/backend/nomad_ch.rs:1995-2002` (the `compio::runtime::spawn` inside `catch_unwind` inside `Drop`).
- **What I see**: PR1's R16-I1 commit (`96fa5f0f`) migrated six periodic-loop sites but explicitly excluded only the `#[cfg(test)] shutdown_tests` fixture. `CreateGuard::drop`'s detached cleanup task fires on every failed-create path and issues `http_delete_unsigned` against Nomad with a 10 s timeout (`:2014`), then waits on the purge (`wait_for_job_gone` 30 s at `nomad_ch.rs:1051`). The closure runs on the ntex worker compio runtime — same shared-runtime starvation surface the C-6 wedge exploits.

  The fact that this Drop is wrapped in `catch_unwind` is *because* runtime-down at process teardown can panic `spawn`; that's an orthogonal hazard. The C-6 fingerprint here is "long-await HTTP call on the shared worker runtime", which is identical to the sites that were migrated.

- **Why it matters**: A `create` failure under fleet load (e.g., during a smoke or stress run that exercises the failure path) lands the cleanup on the same runtime that's serving subsequent wake requests. The mechanism is identical to C-6 (`teardown_source_for_snapshot`'s 60 s wedge).

- **Fix sketch** (deferred; LT-PR2 or LT-PR3):
  1. Inside the `catch_unwind` block at `nomad_ch.rs:2001`, replace `compio::runtime::spawn(async move { ... }).detach()` with `crate::detach::detach_isolated("nomad-guard-cleanup", move || async move { ... })`. The runtime-down panic is exactly the failure mode `detach_isolated` already handles (logs and drops via the runtime-construction-fail branch).
  2. The `catch_unwind` wrapper becomes redundant since `detach_isolated` swallows spawn failure internally — clean simplification.

### [r17-A4] `WakeResponseMode::from_env()` is read-once at boot — no admin kill-switch for the async path

- **Location**: `crates/sandbox/src/config.rs:875-882` (`from_env`); read at `crates/sandbox/src/lib.rs:804` (boot-time only).
- **What I see**: The flag is resolved once during `AppState::from_config` and frozen on the `Arc<AppState>`. PR2 will branch on it per request. Once PR2 lands and operators flip `SANDBOX_WAKE_RESPONSE_MODE=async`, the only way back to `Sync` is a controller restart. For a feature-flag whose purpose is "structural change to the wake response contract", that's a brittle rollback story.

- **Why it matters**: the spec docs (`docs/proposals/c7-lt-async-wake.md`, referenced in `b64d2f39` commit message) explicitly position this as a contract migration. If async causes a regression in prod that isn't caught in smoke/stress, the rollback cost is a fleet restart — exactly the kind of operational hazard a feature flag is supposed to *avoid*.

- **Fix sketch** (for PR2 or a follow-up):
  1. Store the flag inside an `Arc<AtomicU8>` (or `Arc<ArcSwap<WakeResponseMode>>` if more shapes get added) and expose an admin endpoint `POST /admin/wake-response-mode {mode}` that flips it at runtime. Same pattern as the runtime-tunable timeouts in `config.rs`.
  2. Add a `metrics.wake_response_mode` gauge so operators can read the live value from prometheus instead of having to ssh to a controller to check env.

### [r17-A5] `update_wake_job_state` unconditionally clears `error_code` / `error_message` on every state advance — caller must remember to re-pass them or lose the failure context

- **Location**: `crates/sandbox/src/db.rs:2988-2997` (the UPDATE SQL).
- **What I see**: The SQL sets `error_code = $2::TEXT` and `error_message = $3::TEXT` unconditionally. If the caller advances state with `update_wake_job_state(id, Failed, None, None, None)` after a previous call already populated `error_code = Some(SlotUnavailable)`, the NULL overwrites the populated value. The doc string at `:2965-2972` is silent on this — only `agent_url` documents the `COALESCE`-preserving behaviour (`:2993`).

  Same applies the other direction: if the caller advances `Pending → ReservingSlot` and forgets to clear a stale `error_message` that somehow lingered, the row will carry an error in a non-failed state.

- **Why it matters**: PR2's state machine will dispatch this method from multiple places (retry path, deadline-handler path, success path). Asymmetric semantics — `agent_url` preserves on NULL, `error_code/message` do not — invites the kind of stale-state bug where a wake's terminal error_code was set by `Restoring` then nuked by a subsequent `LivezPolling` advance.

- **Fix sketch** (for PR2):
  1. Either make `error_code` / `error_message` `COALESCE($2, error_code)` symmetric with `agent_url`, AND require callers to explicitly NULL them via a separate `clear_wake_error(wake_id)` method, OR
  2. Document the unconditional-set semantics explicitly on the method docstring and add an invariant test: `state IN ('ok','failed')` ↔ `(error_code IS NULL) = (state = 'ok')` (the CHECK constraint allows error_code on success today; consider tightening).
  3. A pg trigger that enforces "error_code is non-null iff state='failed'" would catch the API misuse at write time, not at read time.

## MINOR

### [r17-A6] `detach_isolated`'s `name: impl Into<String>` allocates per dispatch — consider `&'static str` for the hot loop sites

- **Location**: `crates/sandbox/src/detach.rs:76-82`.
- **What I see**: The signature takes `impl Into<String>` and clones internally (`:83 let inner_name = name.clone()`). Every dispatch from a static-name caller (`"snap-health-loop"`, `"snap-heartbeat"`, `"snap-takeover"`) allocates a `String` twice (`Into<String>` from `&'static str` plus the clone). At loop-spawn-time (once per process) this is utterly fine. At per-iteration call sites (none today, but PR2's wake handler will be one), this becomes a per-wake allocation pair.

  Not a defect — the helper is designed to be a once-per-task call. Worth a tiny doc-tweak: "Hot-loop callers SHOULD ensure `name` is `&'static str` to avoid allocation; the `Into<String>` shape exists for the per-sandbox-id naming pattern (`snap-teardown-<tail8>`)."

- **Suggested fix**: documentation only; the API is correct as written.

### [r17-A7] `detach.rs` lives in `crates/sandbox/src/` — promoting to `crates/core/` would require adding `compio` to core, which is a worse trade

- **Location**: `crates/sandbox/src/detach.rs`.
- **What I see**: The helper hard-codes `compio::runtime::Runtime::new()` at `:87`. `crates/core/` deliberately has zero `compio` dep (`crates/core/src/observability.rs` is the only file mentioning it, and that's just a comment about log bridging). Pushing `detach_isolated` to core would force `core → compio` which has fanout to every crate that depends on core.
- **Why it doesn't matter**: no other sandbox-adjacent crate has the C-3/C-6 fingerprint today (gateway uses ntex's own worker pool; runtime crate's V8-per-thread topology has no shared-async-runtime). If a second consumer emerges, the move-to-core conversation reopens; today it's correctly at `crates/sandbox/src/`.

## Cross-lens consensus

- **Perf lens**: r17-A2 (missing takeover index) is the only perf-shaped finding. Performance impact is bounded until PR2 lands; flagged here so the perf lens of PR2 has prior signal.
- **Concurrency lens**: r17-A1 (missing `lessee_updated_at` bump) is identical in shape to the R14-C1 fix that landed for `sandboxes`. The concurrency reviewer for PR2 should walk the wake-handler call sites and verify every `update_wake_job_state` call site is paired with the bump (or that the bump is enforced server-side).
- **API surface lens**: r17-A4 (no admin kill-switch) overlaps the API-surface lens's "operator controls" rubric. r17-A5 (asymmetric error_code clearing) is an API-shape finding the API lens should pick up at PR2 review.

## Lens hand-off

**To PR2's reviewer panel:**
1. Concurrency lens: verify `update_wake_job_state` bumps `lessee_updated_at` (r17-A1) before reviewing the takeover sweep.
2. Perf lens: verify a `(lessee_updated_at)` or `(lessee, lessee_updated_at)` partial-on-non-terminal index lands with the PR2 schema (r17-A2).
3. API-surface lens: verify `error_code` / `error_message` semantics are consistent across `update_wake_job_state`'s callers (r17-A5).
4. Operability lens: verify there's a runtime kill-switch for `WakeResponseMode` (r17-A4).

## PR2 architectural gates (3-5 things PR2 must avoid)

1. **MUST NOT** ship a wake-handler that calls `update_wake_job_state` without ALSO bumping `lessee_updated_at` (r17-A1). Either fix the SQL in PR2's first commit OR introduce a separate `renew_wake_job_lease` method and call it on every transition.
2. **MUST NOT** ship the takeover sweep without an index that supports its WHERE clause (r17-A2). Specifically, the partial index on `(lessee_updated_at)` filtered to non-terminal states.
3. **MUST NOT** rely on the boot-time `WakeResponseMode` read for rollback (r17-A4). Either wire an admin endpoint or accept a documented "controller restart required to roll back" operational cost — and *document it* in the proposal.
4. **MUST NOT** branch on `wake_response_mode` per request without a `metrics.wake_response_mode` gauge that operators can read live. (Pairs with r17-A4.)
5. **MUST NOT** leave `CreateGuard::drop` in `nomad_ch.rs` on the shared worker runtime (r17-A3) when PR2's structural fix is "remove the C-6-style synchronous-wake contract". The fix is a 1-line `detach_isolated` migration; landing C-7-LT without migrating it leaves a sibling-shaped wedge in the very subsystem PR2 is restructuring.
