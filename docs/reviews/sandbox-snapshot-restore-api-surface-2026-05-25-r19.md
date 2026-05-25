# Sandbox/snapshot-restore — api-surface r19 review

Date: 2026-05-25 (UTC, catchup)
HEAD at audit: `44d10fe2` (last reviewed: r18 at `87f40229`).
Round 19 — catchup behind R14-API2 + R18-API2 + r19-A4 closure
(`8e085598`) and R19-C1 wake_jobs takeover (`1d3724fe` + `8d163d58`).
Read-only. Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/core/**`.

## Summary

- **R14-API2 (6th-round carry) CLOSED at `8e085598`**. Verified live:
  `restore_handler.rs:58` docstring now reads `(no Retry-After under
  async-wake contract; clients poll `GET /wake/{id}`)`. Aligns with
  the response builder at `admin_handlers.rs:1157-1163` (no header
  emitted).
- **R18-API2 (carry) CLOSED at `8e085598`**. Verified live:
  `restore_handler.rs:2031` `with_wake_response_mode` is now
  `pub(crate)`. All three sibling builders (`with_*`) are now
  uniformly `pub(crate)` with a single in-crate `lib.rs:from_config`
  caller.
- **r19-A4 (concurrency hand-off) CLOSED at `8e085598`**. Verified
  live: `from_host_fence_timeout` doc at `restore_handler.rs:231-254`
  now carries the `**NON-NORMATIVE teardown model (see smoke-r13
  retrospective)**` banner with the agent-dies / agent-hangs paths;
  the prior "Nomad purge tail ~fence-shaped" composition model is
  gone. 2× factor retained as a margin, not a model.
- **R19-C1 wake_jobs takeover sweep landed** (`1d3724fe` PR1,
  `8d163d58` PR2). Three additions, all visibility-correct:
  (a) `WakeErrorCode::WakeWorkerAborted` + wire `wake_worker_aborted`
  (snake_case; admitted by migration 0012's extended CHECK);
  (b) `pub async fn Database::claim_orphan_wake_for_recovery` — `pub`
  required for `tests/sandbox_pg_e2e.rs` cross-crate consumer
  (parity with `InsertWakeJobOutcome` from r18);
  (c) `WakeLifecycleConfig::takeover_threshold_secs` + two const
  pins (DEFAULT=60, MIN=30). Wire code reuses r16-API1 snake_case
  prescription. Schema-side CHECK names match the enum's `as_str()`
  1:1 — no drift.
- **R19-I1 wait_for_agent_livez signature unchanged at `82478a6b`**.
  Verified: `(base_url: &str, expected_fp: &str, signing_key:
  &Arc<SigningKey>, timeout: Duration) -> Result<(), String>`.
  Internal-only fn; invariant holds.
- **2 new MINOR findings** (R19-API1 wire info-leak; R19-API2
  pub-fn no-external-caller).
- **Carries open**: R10-API4 + R12-API1 readyz §10.0 drift cluster
  (10th + 8th rounds). Unchanged.
- **`pub`-token count** (loose grep): r18 = 1048 → **r19 = 1064**.
  Δ = +16. Net surface growth modest and justified.
- **Backlog**: 5 → 4 (3 closed, 2 new, 2 carries unchanged —
  R18-API1 closed at `531db5c3` per deferred log).

## CRITICAL

None.

## IMPORTANT

### R10-API4 + R12-API1 (10th + 8th round carry) — readyz §10.0 drift cluster

- **Where**: `crates/sandbox-agent/src/handlers.rs:498-510` +
  `crates/sandbox/src/handlers.rs:132-139`. Verified verbatim at
  HEAD: `{"status":"draining"}`, `{"status":"reaper-down"}`,
  `{"status":"backend-unhealthy"}`, `{"status":"ready"}`.
- **State**: LIVE. Both sites unchanged across r10→r19.
- **Recommendation**: unchanged from r17/r18 — comment + invariant
  test OR convert to envelope. SRE-dashboard decision still owns
  the call. GATE-C2 + R19-C1 work has tightened the wake-POST
  envelope side; readyz is now the last §10.0 outlier on the
  platform's standard surface.

## MINOR

### R19-API1 — `error_message` body leaks internal review ID + lessee term (NEW)

- **Where**: `crates/sandbox/src/db.rs:3303-3309` writes the literal
  `error_message = 'controller lessee abandoned this wake (R19-C1
  takeover sweep)'` to the row. `admin_handlers.rs:1856-1859`
  renders `row.error_message` verbatim as the §10.0 envelope's
  `message` on the terminal-`failed` `GET /wake/{id}` response.
- **Problem**: every external client polling a takeover-claimed
  wake gets back:
  ```json
  {"error": "wake_worker_aborted",
   "message": "controller lessee abandoned this wake (R19-C1 takeover sweep)",
   "state": "failed", ...}
  ```
  Two exposures:
  1. **Internal vocabulary**: "controller lessee" is an internal
     concept (lease-renewal lives entirely in pg + sweep code; no
     client SDK or external doc surfaces it).
  2. **Review tracking ID**: `(R19-C1 takeover sweep)` ties the
     runtime contract to an internal review-finding ID. Clients
     will grep for it; support tickets will quote it; AI retry
     loops will branch on it.
- **Severity**: MINOR — informational only; no PII / no auth
  bypass. But the cost compounds with every consumer who screenshots
  it.
- **Fix**: change the DB-side literal to client-facing copy
  (e.g., "wake worker did not finish before lease timeout; the
  sandbox is free for a fresh wake POST"). Move the breadcrumb
  (R19-C1 lineage, takeover sweep marker) to the existing
  `tracing::warn!` at `sweep.rs:402` (already has
  `target: "sandbox::wake::takeover"`). Alternatively surface
  `"takeover": true` as a structured extra and keep `message`
  short.

### R19-API2 — `pub async fn run_wake_jobs_takeover_once` has zero external callers (NEW)

- **Where**: `crates/sandbox/src/sweep.rs:388` —
  `pub async fn run_wake_jobs_takeover_once(state: &Arc<AppState>) -> u64`.
- **Problem**: doc comment claims "Public so the pg-gated tests
  can drive a single pass" — but the four `claim_orphan_*` pg-gated
  tests at `tests/sandbox_pg_e2e.rs:4633-4900` exercise the
  underlying `Database::claim_orphan_wake_for_recovery` directly,
  not this wrapper. Zero grep-able external callers.
- **Sibling**: `run_wake_jobs_gc_once` (sweep.rs:296) has the same
  shape — `pub`, doc says test-driver, no external test callers.
  Paste-precedent pair.
- **Severity**: MINOR — same visibility-hygiene flavor as r18's
  R18-API2 finding (just closed); two-token edit. Worth bundling
  with the next visibility sweep so the precedent compounds.
- **Counter**: leaving `pub` for a future test driver. A future
  external driver can flip the visibility back in the same commit
  it adds the caller — the empirical pattern across the crate is
  `pub(crate)` for `spawn_*` + `run_*_once`; these two `pub` outliers
  are the only deviation.

### Considered + dismissed

- **`WakeWorkerAborted` wire code `wake_worker_aborted`** matches
  r16-API1 snake_case. The `as_str()` ↔ `wire_code()` are identical
  for this variant (slight asymmetry vs. e.g., `Internal` → `internal`
  / `internal_error`), but db.rs:1589-1593 documents the choice
  (operational distinction from `internal_error`). Not flagged.
- **Migration 0012 schema-side CHECK admits `wake_worker_aborted`**
  exactly matching the enum's `as_str()`. Round-trip pinned at
  `db.rs:3512-3543`. Schema / enum / wire consistent. Not flagged.
- **`Database::claim_orphan_wake_for_recovery` is `pub`**: required
  for cross-crate consumption by `tests/sandbox_pg_e2e.rs` (5
  callsites at lines 4708-4884). Mirrors r18's `InsertWakeJobOutcome`.
- **`WakeLifecycleConfig::takeover_threshold_secs` + new pub consts**:
  the struct was already `pub`; field/const visibility consistent
  with siblings. Not flagged.
- **Idempotency status-code matrix at `admin_handlers.rs:1521-1528`**:
  unchanged from r16-API1. No new sliders.

## §10.0 envelope post-R19-C1

`WakeWorkerAborted` slots into the existing envelope cleanly. Render
path is the same as every other `WakeErrorCode`: terminal `failed`
row → §10.0 envelope at `render_wake_poll_response` → 200 OK with
`{error: "wake_worker_aborted", message, state: "failed", wake_id,
sandbox_id, updated_at}`. No new HTTP status, no new top-level
keys, no new branching. The wire surface grew by exactly one valid
`error` string value and migration 0012's CHECK accepts it. Right
shape — operational distinction lives in dashboards / client retry
logic, not in the envelope. The r18 `replay: bool` discussion did
not move; three emit sites unchanged. R18-API1 wire-format test
closed at `531db5c3` per the deferred log.

## Cross-lens consensus

- **architecture r19**: validates R19-C1 takeover as the structural
  fix; api-surface agrees `WakeWorkerAborted` is the wire reflection.
  Architecture tracks the sweep as the `lessee_updated_at` reader;
  api-surface flags R19-API1's internal-term leak without disputing
  the mechanism.
- **code-quality r19**: bundles sweep wiring + config tests. R19-API2's
  two `pub → pub(crate)` demotions track naturally as the next
  visibility sweep.
- **concurrency r19**: R19-C1 raised as CRITICAL there; api-surface
  confirms resolution is wire-consistent (snake_case, schema admit,
  envelope shape unchanged). R19-I1 `wait_for_agent_livez` is
  internal-only — no signature drift.

## Lens hand-off

- **To architecture r20**: track whether `error_message` should
  become a stable surface or remain freeform. Three distinct
  writers (wake state machine, takeover sweep, legacy sync path)
  produce different message styles today; a client doing
  string-matching is buying its own brittle contract.
- **To code-quality r20**: bundle R19-API2 (two `pub → pub(crate)`
  demotions on `run_wake_jobs_*_once`) into the next sweep.
  R19-API1 (error_message leak) is the cheapest open finding —
  one string-literal edit + one structured log emit; ~5 LOC.
- **To test-coverage r20**: once R19-API1 is resolved, pin the
  new fixed phrasing as an invariant so a future refactor can't
  reintroduce the lineage breadcrumb to the wire.

## Trend

- **`pub`-token count**: r18 = 1048; **r19 = 1064**. Δ = +16
  (3 new fields + 3 new consts + 1 new variant + 1 new function +
  test items; minus 1 demotion). Well-bounded.
- **`Result<_, String>`**: sandbox 161 (8 rounds flat),
  sandbox-agent 14 (10 rounds flat).
- **Net new wire envelope kinds r18→r19**: +1
  (`wake_worker_aborted`), cleanly slotted; no new status code,
  no new top-level keys.
- **Closure velocity**: r18→r19 = +3 (R14-API2, R18-API2, r19-A4
  — all in `8e085598`). Highest single-commit closure count in
  recent rounds.
- **Backlog open-item count**: r18 = 5; **r19 = 4**.

## Two most-critical citations

1. **`crates/sandbox/src/db.rs:1500-1515` +
   `crates/sandbox/migrations/0012_wake_jobs_aborted_code.sql`** —
   `WakeErrorCode::WakeWorkerAborted` + the CHECK constraint
   extension. The enum's `as_str()`, `from_str_opt`, `wire_code`
   all flow through the round-trip pin at `db.rs:3512-3543`;
   migration is forward-only + idempotent. Wire code reuses r16-API1
   snake_case; SLO dashboard can now split "controller crashed
   mid-wake" from "wake step itself failed."
2. **`crates/sandbox/src/db.rs:3303-3309` +
   `crates/sandbox/src/admin_handlers.rs:1856-1859`** — the
   `error_message` literal "controller lessee abandoned this wake
   (R19-C1 takeover sweep)" lands on the wire verbatim via
   `render_wake_poll_response`. R19-API1 above; cheapest open
   finding (~5 LOC).
