# Sandbox/snapshot-restore — api-surface r17 review

Date: 2026-05-25 (UTC, catchup)
HEAD at audit: `7664b4b0` (last reviewed: r16 at `792a7aa5`).
Round 17 — catchup behind concurrency/perf/test-cov r17. Read-only.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`, `crates/core/**`.

## Summary

- **5 R16 PR2 spec gates verified end-to-end against landed PR1+PR2
  code.** All 5 closed at HEAD `7664b4b0`. PR1 `a0888d9e` + PR2
  `98032273` + R16-S\* follow-ups (`b2b6c3c9`, `4ab58eac`,
  `c3038389`, `fa4fe63c`, `96678eaa`) all in.
- **1 new MINOR finding (R17-API1)**: PR2 added a cluster of new
  `pub` items that are crate-internal-only (`detach_isolated`,
  `spawn_wake_jobs_gc`, `WAKE_JOBS_T_KEEP`, `WAKE_JOBS_GC_POLL_SECS`).
  Candidates for `pub(crate)` tightening — orthogonal to R10-API1
  cluster.
- **3 carries open** (`R10-API1`, `R14-API2`, `R10-API4+R12-API1`).
  Re-verified live at HEAD; all unchanged.
- **`pub`-token count**: 848 (r16) → 1002 (r17). +154 from PR1/PR2
  surface (`WakeJobRow`, `WakeJobState`, `WakeErrorCode`,
  `WakeMachine`, `WakeResponseMode`, `WakeLifecycleConfig`,
  `detach_isolated`, GC sweep constants, metrics fns). Expected; PR2
  is the biggest single landed delta in the audit window. Most are
  legitimately external (tests crate consumes `WakeMachine` +
  `WakeJobRow`); R17-API1 cluster is the narrow opportunity.
- **Backlog**: 7 → 5 (R16-API1/2/3 closed; M1/M2 from r16 closed by
  PR2 implementation choices; R17-API1 new). Closure velocity
  r16→r17 = +4 (record high for this lens; PR2 cleared the design-
  pre-review queue in one shot).

## CRITICAL

None.

## IMPORTANT

### R10-API1 (8th-round carry) — `_test_build_auth_from_sealed` orphan `pub`

- **Where**: `crates/sandbox/src/restore.rs:613`. Re-verified — only
  the definition site exists; zero callers anywhere
  (`grep -rn '_test_build_auth_from_sealed' --include="*.rs"` yields
  1 hit).
- **State**: LIVE. Now the longest-running api-surface carry; was
  flagged for tightening cluster alongside PR1 (didn't happen) and
  PR2 (didn't happen). Cheapest open finding any lens — 1-token
  edit (`pub fn` → `#[cfg(test)] fn` if same-file, else delete).
- **Recommendation**: bundle with R17-API1 below into a single
  visibility-tightening commit.

### R14-API2 (5th-round carry) — `Retry-After` docstring drift

- **Where**: `restore_handler.rs:58` (docstring promises
  `Retry-After`), `admin_handlers.rs:1157-1163` (response builder
  emits no header).
- **State**: LIVE. Verified at HEAD.
- **Sequencing**: C-7-LT phase 5 deletes the sync path entirely.
  Two equally cheap options:
  1. Drop the `(Retry-After)` parenthetical now (1-token edit).
  2. Wait for phase-5 deletion (which removes the docstring
     itself).
  Recommend (1) since the sync path is still live and the
  docstring is wrong today.

### R10-API4 + R12-API1 (8th + 6th round carry) — readyz §10.0 drift cluster

- **Where**: `crates/sandbox-agent/src/handlers.rs:498-510` +
  `crates/sandbox/src/handlers.rs:132-139`. Verified verbatim at
  HEAD: `{"status":"draining"}`, `{"status":"reaper-down"}`,
  `{"status":"backend-unhealthy"}`, `{"status":"ready"}`.
- **State**: LIVE. Both sites unchanged across r10→r17.
- **Sequencing**: orthogonal to C-7-LT (readyz is liveness, not
  wake). No urgency unless SRE keys on `error`/`message`.
  Recommend carving out as "shape-stable-pre-§10.0" with a
  one-line comment OR converting to envelope. Decision belongs to
  the SRE dashboard owner.

## MINOR

### R17-API1 — PR2 added `pub` items that are crate-internal-only (NEW)

- **Where**:
  - `crates/sandbox/src/detach.rs:76` — `pub fn detach_isolated(...)`.
    All 6 callsites are in `crates/sandbox/src/*.rs` (sweep,
    wake_machine, registry, lib, snapshot_store_gcs, admin_handlers,
    backend/nomad_ch). Zero external consumers.
  - `crates/sandbox/src/sweep.rs:332` — `pub fn
    spawn_wake_jobs_gc(state)`. Only call: `lib.rs:915` (same
    crate).
  - `crates/sandbox/src/sweep.rs:82,97` — `pub const
    WAKE_JOBS_GC_POLL_SECS`, `WAKE_JOBS_T_KEEP`. Only readers in
    `sweep.rs` itself.
- **Problem**: cluster of `pub` items with zero external callers.
  Each is `pub(crate)`-eligible. Consistent with R10-API1's pattern
  (long-tail orphan `pub` items accumulate after big landings).
- **Fix**: change `pub fn` → `pub(crate) fn`, `pub const` →
  `pub(crate) const`. 4 one-token edits.
- **Severity**: MINOR — visibility hygiene; no wire impact.
  Bundle with R10-API1 into a single visibility commit.

## Cross-lens consensus

- **architecture r17**: tracked PR2's WakeMachine as the structural
  cutover — api-surface independently verified the wire contract
  (R16-API1/2/3) landed correctly. Converges.
- **concurrency r17**: noted R17-M2 (`WakeMachine.lessee` field
  populated but never read inside `run`) — api-surface flags
  `WakeMachine` fields as `pub` and consumed by tests, so a fix
  there could/should reduce the public field count, but doesn't
  change the public surface shape per se.
- **test-coverage r17**: confirmed 4 of 6 PR2 integration tests
  exist for the WakeMachine path. The wire-format test
  (`render_wake_poll_response`-direct call covering 7
  `WakeErrorCode` variants at `admin_handlers.rs:2283-2306`) is a
  pure api-surface test and landed alongside the implementation —
  the right place for that pin.
- **code-quality r17**: should pick up R17-API1 cluster +
  long-running R10-API1.

## Lens hand-off

- **To architecture r18**: validate the WakeMachine `rollback_with`
  trait surface (`wake_machine.rs:545`+) — error-code classification
  is api-surface-adjacent but the runtime semantics of "which
  variants are recoverable" is architecture.
- **To security r17/r18**: pre-review the `agent_url` PR2 surface.
  `WakeJobRow.agent_url` is `pub String` and lands in the 200-OK
  poll body. Confirmed admin-bearer gated at
  `admin_handlers.rs:1711`; if a non-admin path ever polls (e.g.
  gateway forwarding), capability check needed.
- **To test-coverage r17**: confirm a regression test pins the
  POST replay path (`replay: true` body field, same `wake_id`).
  The PR2 stub fixtures at `sandbox_pg_e2e.rs:4400+` are pg-gated;
  there's no in-process test for the `find_pending_wake_for_sandbox`
  branch at `admin_handlers.rs:1556-1568`.

## R16 PR2 spec gate verdict

| # | Gate | Verdict | Cite |
|---|---|---|---|
| 1 | §10.0 field-name parity on failed-state poll body (`error`/`message`, NOT `error_code`/`error_message`) | **CLOSED** | `admin_handlers.rs:1813-1830` uses `ErrorEnvelope::new(...).with_extra(...)` → `error_envelope.rs:88-110` emits `{error, message, ...extra}`. Test pin at `admin_handlers.rs:2283-2306` asserts `body["error"] == code.wire_code()` across all 7 variants. |
| 2 | 3-char typed_id prefix `wak_` via `new_wake_id()` | **CLOSED** | `crates/core/src/typed_id.rs:179` `pub fn new_wake_id() -> String { generate(WAKE_PREFIX) }`. Round-trip test at `typed_id.rs:241-248` (`wake_prefix_is_three_chars`) asserts `WAKE_PREFIX.len() == 3` + parse roundtrip. POST handler uses it at `admin_handlers.rs:1627`; GET handler validates with `parse_with_prefix(.., "wak")` at `:1722`. |
| 3 | `WakeErrorCode::wire_code()` reuses existing envelope codes 1:1 (`vm_index_unavailable`, `restore_backend_failed`, `livez_timeout`, …) | **CLOSED** | `crates/sandbox/src/db.rs:1551-1561`. Test pin at `db.rs:3351-3370` (`wake_error_code_wire_code_uses_existing_envelope_codes`) locks all 7 mappings. Render path uses `wire_code()` not `as_str()` at `admin_handlers.rs:1818`. |
| 4 | `?sync=1` deprecation telemetry counter registered + bumped on every sync use | **CLOSED** | Counter at `crates/sandbox/src/metrics.rs:121` (`WAKE_SYNC_DEPRECATED: AtomicU64`). Bump at `admin_handlers.rs:1453` on the `take_sync_path` branch (covers BOTH `?sync=1` AND default-Sync mode, per the comment at `:1449-1452`). Test at `metrics.rs:362-368` (`wake_sync_deprecated_counter_monotonic`). Counter accessor at `metrics.rs:222` (`wake_sync_deprecated_value()`). |
| 5 | Idempotency POST/GET status-code matrix (202 / 202+replay / 200 / 404) | **CLOSED** | POST matrix at `admin_handlers.rs:1521-1528` (docstring) + `:1556-1568` (in-flight replay → 202+`replay: true`+existing `wake_id`) + `:1677-1685` (fresh → 202+`replay: false`+new `wake_id`). GET matrix at `admin_handlers.rs:1693-1699` (docstring) + `render_wake_poll_response` at `:1794-1842` (terminal-ok → 200 flat, terminal-failed → 200 envelope, intermediate → 202, not-found → 404). Tests at `admin_handlers.rs:2256-2358` pin all 4 GET-shape cases. |

**All 5 gates CLOSED.** PR2's wire contract matches the api-surface r16 spec exactly. No drift between proposal-as-revised and landed code.

## Trend

- **`pub`-token count**: r16 = 848; **r17 = 1002**. Δ = +154
  from PR1/PR2 (`WakeMachine` struct + 7 `pub` fields,
  `WakeJobRow` struct + 11 `pub` fields, `WakeErrorCode` +
  `WakeJobState` enums + methods, `WakeResponseMode` +
  `WakeLifecycleConfig`, GC sweep + metrics, `detach_isolated`
  helper). R17-API1 identifies 4-5 of these for `pub(crate)`
  tightening.
- **`Result<_, String>`**: sandbox 161 (6 rounds flat),
  sandbox-agent 14 (8 rounds flat) — PR1/PR2 used `?` /
  `Result<_, sqlx-like Error>` instead.
- **Net new wire endpoints r16→r17**: **+1 landed**
  (`GET /admin/sandboxes/{id}/wake/{wake_id}`). POST shape
  changed (now dual-mode) but path is the same.
- **Closure velocity**: r15→r16 = 0; **r16→r17 = +4** (record;
  3 design-pre-review findings closed by PR2 landing, M2 closed
  by PR1's `WakeErrorCode::wire_code` 1:1 mapping).
- **Backlog open-item count**: r16: 7; **r17: 5** (3 carries + 2
  cluster R10-API4+R12-API1 + R17-API1 new).

## Two most-critical citations

1. **`crates/sandbox/src/admin_handlers.rs:1813-1830`** — failed-state
   poll response now uses `ErrorEnvelope::new(StatusCode::OK,
   wire_code, message).with_extra({state: "failed", ...})`. Field
   names on the wire are `error`/`message` per
   `error_envelope.rs:88-110`, NOT the proposal-pre-review
   `error_code`/`error_message`. R16-API1 CLOSED.
2. **`crates/core/src/typed_id.rs:179` + `:241-248`** — `wak_`
   3-char prefix wired through `new_wake_id()` with a roundtrip
   invariant test that fails CI on regression. Every wake-job id
   on the wire matches the global `^[a-z]{3}_[A-Za-z0-9]{22}$`
   pattern. R16-API2 CLOSED.
