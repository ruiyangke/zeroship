# Sandbox/snapshot-restore — api-surface r18 review

Date: 2026-05-25 (UTC, catchup)
HEAD at audit: `87f40229` (last reviewed: r17 at `7664b4b0`).
Round 18 — catchup behind GATE-C2 + C-7-LT-1 + visibility-tightening
commit (`f27062c0`). Read-only.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`, `crates/core/**`.

## Summary

- **GATE-C2 wire contract reviewed**. `InsertWakeJobOutcome` +
  `?replay: bool` body field threaded cleanly through
  `insert_wake_job` → `wake_sandbox_async_inner` → 202 response.
  Visibility correct (enum is consumed cross-crate by
  `sandbox_pg_e2e.rs`, so `pub` is required).
- **C-7-LT-1 retry-policy signature change reviewed**.
  `VmIndexRetryPolicy::from_host_fence_timeout(secs, wake_mode)` —
  signature break absorbed by 8 in-crate callers (all unit tests in
  `restore_handler.rs`); zero cross-crate callers. Migration cost
  was paid in the landing commit.
- **R17-API1 + R10-API1 (8-round carry) CLOSED at `f27062c0`**.
  Re-verified live: `detach_isolated`, `spawn_wake_jobs_gc`,
  `WAKE_JOBS_*` constants are now `pub(crate)`;
  `_test_build_auth_from_sealed` is `#[cfg(test)] pub(crate)`.
  Zero orphan callers anywhere.
- **2 new MINOR findings**: (R18-API1) the `replay` body field has
  no wire round-trip test pinning the three emit sites; (R18-API2)
  `with_wake_response_mode` is `pub` while the sibling
  `with_shared_allocator` builder is `pub(crate)` — only the same
  one `lib.rs::from_config` caller; visibility-hygiene mismatch.
- **3 carries open** (`R10-API4 + R12-API1`, `R14-API2`). Re-verified
  live at HEAD; all unchanged. R14-API2 specifically still wrong
  today and Phase-5 deletion has not landed.
- **`pub`-token count**: 1002 (r17) → **1048 (r18)**. Δ = +46.
  Two pub items added (`InsertWakeJobOutcome` + variants; bumped
  `LATEST_MIGRATION_VERSION` const) — modest delta vs. r16→r17's
  +154 from PR1/PR2. Four `pub` items demoted (R17-API1 closure).
  Net surface growth is small and justified.
- **Backlog**: 5 → 5 (R17-API1 + R10-API1 closed; R18-API1 + R18-API2
  new; 3 carries unchanged).

## CRITICAL

None.

## IMPORTANT

### R14-API2 (6th-round carry) — `Retry-After` docstring drift

- **Where**: `crates/sandbox/src/restore_handler.rs:58` (docstring
  promises `Retry-After` on 503 `vm_index_unavailable`).
  `admin_handlers.rs` no longer renders the sync path through this
  comment in the GATE-C2 / C-7-LT-1 reshuffle — the synchronous
  wake response still emits no `Retry-After` header at any of
  the 5xx error sites.
- **State**: LIVE. Phase-5 sync-path deletion has NOT landed —
  `take_sync_path` is still gated at `admin_handlers.rs:1449-1452`
  and the sync handler still runs through `wake_sandbox_sync_inner`.
  R17's "drop now since Phase 5 deletes it anyway" gamble lost a
  round.
- **Sequencing**: equally cheap to drop `(Retry-After)` parenthetical
  now (1-token edit) or wait. Recommend **dropping NOW** — this is
  the 6th consecutive round the carry has rolled forward; the cost
  of the parenthetical lie compounds with every async-mode
  consumer who reads the docstring expecting a header.

### R10-API4 + R12-API1 (9th + 7th round carry) — readyz §10.0 drift cluster

- **Where**: `crates/sandbox-agent/src/handlers.rs:498-510` +
  `crates/sandbox/src/handlers.rs:132-139`. Verified verbatim at
  HEAD: `{"status":"draining"}`, `{"status":"reaper-down"}`,
  `{"status":"backend-unhealthy"}`, `{"status":"ready"}`.
- **State**: LIVE. Both sites unchanged across r10→r18.
- **Recommendation**: unchanged from r17 — add a "shape-stable-pre-§10.0"
  comment + invariant test OR convert to envelope. SRE-dashboard
  decision still owns the call. Bundle with the §10.0 wake contract
  pin once Phase-5 sync deletion lands.

## MINOR

### R18-API1 — `replay` body field has no wire round-trip test (NEW)

- **Where**: `admin_handlers.rs:1575`, `:1684`, `:1721` — three
  sites emit `"replay": <bool>` in the 202-Accepted JSON body
  (precheck fast-path / ON-CONFLICT loser / happy path).
- **Problem**: the GATE-C2 lib unit tests pin
  `InsertWakeJobOutcome::Inserted` vs.
  `InsertWakeJobOutcome::Replay(WakeJobRow)` at the enum boundary
  (`db.rs:3905-3945`), and pg-gated `wake_jobs_crud` tests pin
  the DB-side race (`sandbox_pg_e2e.rs:4400+`). But there is no
  in-process test that calls `wake_sandbox_async_inner` and
  asserts `body["replay"] == true` / `body["replay"] == false`
  on the resulting `HttpResponse`. The render path (`render_wake_poll_response`)
  has a wire-format test pinning all 7 `WakeErrorCode` variants
  (`admin_handlers.rs:2283-2306`); the POST replay path has no
  equivalent. A future refactor that drops the `"replay"` field
  (or flips it to a `X-Zsbx-Wake-Replay` header per §10.0
  convention — see below) would land green.
- **Severity**: MINOR — the field is new, no external consumer
  has shipped yet; the cost is one wire-format test pinning
  the three emit sites' shape.
- **Hand-off**: tracked as `R18-I1` in code-quality-r18 already,
  with the proposed test fixture wired through the existing
  `make_machine` helper. Recommend the api-surface fix and the
  code-quality fix land in the same commit.

### R18-API2 — `with_wake_response_mode` builder is `pub` not `pub(crate)` (NEW)

- **Where**: `crates/sandbox/src/restore_handler.rs:2023` —
  `pub fn with_wake_response_mode(mut self, ...) -> Self`.
- **Problem**: only caller is `lib.rs:785` inside the same crate
  (`AppState::from_config`). The sibling builder
  `with_shared_allocator` (`restore_handler.rs:2040`) is
  `pub(crate)` with the same usage pattern (one in-crate caller,
  same wiring layer). Inconsistency.
- **Fix**: `pub fn` → `pub(crate) fn`. One-token edit.
- **Severity**: MINOR — visibility hygiene; no wire impact.
  Bundle with R18-API1's test pin or carry into the next
  visibility-tightening sweep.

### Considered + dismissed

- `VmIndexRetryPolicy::from_host_fence_timeout` is `pub`
  (`restore_handler.rs:288`); 8 in-crate callers, zero external.
  Borderline `pub(crate)`-eligible — the doc-comment is the
  single source-of-truth for `effective_budget`; demoting would
  hide it from rustdoc. **Not flagged** — visibility cost paid
  for in rustdoc gain.
- `InsertWakeJobOutcome` is `pub` (`db.rs:1613`). Consumed by
  `sandbox_pg_e2e.rs:3659`. Correct — must remain `pub`.
- `Replay(WakeJobRow)` carrying the full row: code-quality-r18
  flags as "dead weight" since the handler reads only `wake_id`
  + `state`. **Reviewer call**: row is the right carrier — DB
  layer's natural unit; trimming would split the API, hurt
  fixture readability, force the next caller to re-fetch.
  Sub-200-byte memory on a path that already roundtrips the row
  through pg. Hand off to architecture if a second consumer
  materializes.

## §10.0 envelope post-GATE-C2

### Replay response shape: 202 + body field vs. header convention

The current shape is 202 Accepted + body `{wake_id, sandbox_id,
poll_url, state, replay: bool}`. The body shape mirrors the
fresh-POST 202 + `replay: false`, which is symmetric and
type-friendly.

**Possible divergence from existing platform convention**:
`docs/architecture/gateway-routing.md:274` documents an
`x-zs-idempotent-replay: true` **header** convention for
gateway-side idempotent replays (POST + `Idempotency-Key`). The
wake endpoint took the body-field route instead.

**Reviewer assessment**: not a finding. The two surfaces are
contractually different:
- Gateway idempotency reuses the FULL stored response (status +
  body); the header is the only place the replay-ness can be
  surfaced without mutating the stored body.
- Wake admin POST generates a NEW response body (`wake_id`,
  `poll_url`) regardless of replay; the body is the natural
  carrier. Surfacing `replay` as a header would force the client
  to read two channels for one logical decision.

The 202 status code matches the idempotency matrix (R16-API1 #3,
`admin_handlers.rs:1521-1528`): in-flight wake → 202 with the
existing `wake_id` + `replay: true`; fresh wake → 202 with new
`wake_id` + `replay: false`. Identical status, body shape
distinguishes. Correct.

**Recommendation**: keep the body field. If consistency with
`x-zs-idempotent-replay` matters for cross-platform tooling, add
a parallel `X-Zsbx-Wake-Replay: true` response header (5-LOC,
both channels), but not strictly required.

## Cross-lens consensus

- **architecture r18**: validates the GATE-C2 race-collapse
  structurally (the `Replay` branch's "don't spawn a second
  state machine" invariant is the load-bearing piece, not the
  wire field). Api-surface agrees: the `replay` body field is
  the surface-level reflection of the structural fix.
- **code-quality r18**: R18-I1 (test fixture discards
  `InsertWakeJobOutcome` from `make_machine`) is the test-side
  sibling of R18-API1; both should land together. Also flagged
  the `Replay(WakeJobRow)` "dead weight" question — handed back
  to api-surface in their finding-3, dismissed above.
- **concurrency r18**: cross-checked C-7-LT-1 budget math (async
  fence=30 → 70 s ≥ 60.166 s empirical). Api-surface confirmed
  the signature change is consistent across all 8 callsites.
- **test-coverage r18 (not yet written)**: handing off the
  R18-API1 wire-format test plus the `make_machine` fixture
  follow-up.

## Lens hand-off

- **To test-coverage r18**: add a wire-format unit test in
  `admin_handlers::tests` that calls `wake_sandbox_async_inner`
  through a stub `Database` returning `InsertWakeJobOutcome::Replay`
  vs. `Inserted` and asserts `body["replay"]` matches expected,
  for all three emit sites. Pure JSON-pin; no pg required.
- **To architecture r19**: track whether `Replay(WakeJobRow)`
  warrants tightening to `Replay { wake_id: String, state:
  WakeJobState }` once a second caller materializes. Today's
  shape is the right call; flag for revisit.
- **To code-quality r19**: bundle R18-API2 (`with_wake_response_mode`
  → `pub(crate)`) into the next visibility-tightening sweep.
  R14-API2 (`Retry-After` docstring) is the cheapest open
  finding any lens — 1-token edit, 6-round carry; drop it now.

## Trend

- **`pub`-token count**: r17 = 1002; **r18 = 1048**. Δ = +46
  (`InsertWakeJobOutcome` enum + 2 variants; `LATEST_MIGRATION_VERSION`
  bumped; 4 items DEMOTED to `pub(crate)` in `f27062c0`). Net
  growth is +50 from new code minus 4 from tightening; very
  modest vs. r16→r17's +154 from PR1/PR2.
- **`Result<_, String>`**: sandbox 161 (7 rounds flat),
  sandbox-agent 14 (9 rounds flat). No change.
- **Net new wire endpoints r17→r18**: 0. POST response body shape
  changed (added `replay`); GET response shape unchanged.
- **Closure velocity**: r17→r18 = +2 (R17-API1, R10-API1 — the
  8-round carry). Tied for second-highest behind r16→r17's +4.
- **Backlog open-item count**: r17 = 5; **r18 = 5** (2 closed,
  2 new, 3 carries unchanged).

## Two most-critical citations

1. **`crates/sandbox/src/db.rs:1613-1620` + `:3015-3060`** —
   `InsertWakeJobOutcome::{Inserted, Replay(WakeJobRow)}` +
   `ON CONFLICT (sandbox_id) WHERE state NOT IN ('ok','failed')
   DO NOTHING`. The structural fix for R17-C2: the DB layer now
   atomically guarantees at-most-one non-terminal wake per
   sandbox; the handler can no longer race two `WakeMachine`s
   into the rollback path that leaked the winner's vm_index.
   GATE-C2 CLOSED.
2. **`crates/sandbox/src/restore_handler.rs:288-340` +
   `:2188-2192`** — `VmIndexRetryPolicy::from_host_fence_timeout(secs,
   wake_mode)` with the async-mode branch dropping the
   `CLIENT_DEADLINE_SECS` ceiling. At fence=30 the budget rises
   from 50 s (sync) to 70 s (async), enveloping smoke-r12's
   60.166 s teardown wall-time with ~10 s slack. C-7-LT-1
   CLOSED.
