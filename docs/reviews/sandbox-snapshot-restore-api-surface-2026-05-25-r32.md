# Sandbox/snapshot-restore — api-surface r32 review

Date: 2026-05-25 (UTC). HEAD at audit: `8d82ecde`.
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.
Read-only. Round 32 of pilot-cron loop. Prior: r31 (`729f22dd`).

Commits since r31 (api-surface impact):

- `a6e517b2` perf-r32-S1 — wake-path polling cadences (`150→50ms`
  livez, `250→100ms` alloc_running) in `nomad_ch.rs` +
  `restore_handler.rs`. **No pub deltas.**
- `c56893b2` r31-S1 closure — drop `driver.raw_exec.enable=1` from
  `gcp-worker-startup.sh`. **scripts/ only.**
- `4ac1e526` perf — tighten `wait_for_alloc_running` parse-error
  sleep `250→100ms`. **One literal change, comments updated.**
- `1d58ab53` r32-T1 — emit 2 new `tracing::info!` milestones
  (`sandbox/nomad-ch create submit_done` at `nomad_ch.rs:1188-1193`,
  `sandbox/nomad-ch alloc_first_seen` at `nomad_ch.rs:3089-3094`).
  Adds 1 local var (`fn_started`, `alloc_first_seen_logged`). **No
  pub items added.**
- `8d82ecde` — `docs/reviews/` only.

## Summary

**Net pub-count delta r31 → r32: 0.** `git diff 729f22dd..8d82ecde
-- crates/sandbox/src crates/sandbox-agent/src` yields zero diff
hunks containing the token `pub`. `grep -E '^(\s+)?pub\s+(fn|struct
|enum|trait|mod|const|static|type|use|async)'` count over
`crates/sandbox/src/` = 475 at both r31 and r32 (475 = the
indent-allowed-pub-fn-only count; r31's "300" figure was the more
restrictive item-kind sweep; trend monotonic either way: **0
delta**).

**Zero new findings, zero closures. Backlog steady at 11.**

The two `tracing::info!` macros land in `pub async fn` bodies that
already exist (`create_sandbox` / `wait_for_alloc_running`). They
expand to runtime-side log emits and a couple of locals, not new
items in the symbol table. **`tracing::info!` is observability, not
api-surface.** Verified.

## CRITICAL

None.

## IMPORTANT

### [R26-API1] (carry, 7 rounds) driver-side `nomad_driver_ch_destroy_task_unreaped_total` has no operator-readable surface

Out-of-tree `nomad-driver-ch` repo. Unchanged. Owner: out-of-tree
driver / observability ADR.

### [R20-API1] (carry, 9 rounds) schema-marker rewriter sites unchanged

4 path-derivation rewriter sites in `restore_handler.rs` /
`nomad_ch.rs`. Quadruply-motivated; held pending driver-side
validator. Owner: security (driver-side validator landing).

## MINOR

### [R30-API1] (carry, 3 rounds) `AppState::nomad_stop_permits` returns `&Arc<NomadStopPermits>`

`lib.rs:306-309`:

```
pub fn nomad_stop_permits(
    &self,
) -> Option<&Arc<NomadStopPermits>> {
    &self.nomad_stop_permits
}
```

Field at `:258` already `pub(crate)`. Accessor still `pub`. The
type itself (`NomadStopPermits`) + 7 methods at
`nomad_ch.rs:558,609,632,638,654,715,729` also still `pub`. r32
landed no narrowing. **9 items.** This is the biggest leak in the
crate — external test code that calls `state.nomad_stop_permits()
.unwrap().acquire().await` could hold a permit indefinitely (DoS
shape against `stop_inner`). Fix: 9-line mechanical
`pub`→`pub(crate)`. Owner: code-quality r33.

### [R30-API2] (carry) `metrics::inc/dec_nomad_stop_permits_in_use` `pub fn`

`metrics.rs:496,505`. Callers in-file only. Bundle with R27-API3
17-item sweep. Owner: code-quality r33.

### [R30-API3] (carry, half-closed) operator-facing docs still describe pre-T-8 architecture

`docs/runbooks/sandbox-nomad-ch.md` audience / prerequisites /
config table / triage all reference deleted `raw_exec` + wrapper.
**AGENTS.md task router** still lists `crates/sandbox/scripts/
nomad-vm-wrapper.sh` as a route — that file was deleted at
`cdcd670d`. Verified at r32 audit time: AGENTS.md line listing the
wrapper path is unchanged. Fix: ~30-line prose update + 1-line
AGENTS.md edit. Owner: code-quality r33.

### [R29-API1] (carry) `release_vm_index_after` + `spawn_delayed_release_in_worker` `pub fn`

`nomad_ch.rs:396,438`. Visibility untouched. Owner: code-quality
r33.

### [R29-API2] (carry) `spawn_delayed_release_in_worker` `pub` + `#[allow(dead_code)]` + zero production callers

`nomad_ch.rs:436-455`. Pick one of three coherent endings (narrow
/ delete / find caller). Owner: code-quality r33 / architecture.

### [R28-API3] (carry) `test-support` feature lacks crate-level rustdoc warning

`Cargo.toml:71-77` + `lib.rs:1-9`. 5 active `#[doc(hidden)]`
markers gated correctly. **No intent-leak.** Crate-root rustdoc
still unmarked. Owner: code-quality r33.

### [R27-API1] (carry) `Backend::builder` rustdoc lacks cross-reference to `BackendBuilder`

`backend/mod.rs:260-264`. Owner: code-quality r33.

### [R27-API3] (carry, 17-item sweep steady) `metrics::*_value` + `inc/dec_*` `pub` with zero external consumers

`metrics.rs` — 17 functions enumerated in r30. No new metrics
functions this round; target list unchanged. Owner: code-quality
r33.

### [R27-API4] (carry) `WakeErrorCode` rustdoc table 8 of 10 variants

`db.rs:1725-1736` table; `wire_code()` match at `:1749-1762`
covers 10. Missing from table: `StagingPathMissing`,
`AgentVersionMissing`. Owner: code-quality r33.

### Other carries (no movement)

R19-API2, R22-API2, R22-API3, R23-API2, R23-API3, R24-API2,
R24-API3, R24-MIG1, R24-SWEEP1, R25-API3, R25-API4, R25-API5,
R26-API5.

## §10.0 envelope state post-r32

**Inventory delta from r31**: none. r32 commits do not touch wire
shape — all touch poll cadences, comment values, or emit new
log lines. The 32 §10.0 codes are unchanged. `WakeErrorCode`
remains 10 variants.

**Fallible handler scan**: 12 `pub async fn` admin handlers at
`admin_handlers.rs:405-2055` (`list_all_sandboxes`,
`get_sandbox_detail`, `list_user_sandboxes`, `list_user_shares`,
`list_hosts`, `export_user`, `delete_user`, `snapshot_sandbox`,
`wake_sandbox`, `poll_wake`, `cold_boot_sandbox`,
`metrics_endpoint`). All route through `crate::error_envelope::
{ErrorEnvelope, error_response}` (imported at `:71`, verified at
audit time). The `error_envelope` module's surface is entirely
`pub(crate)`. **Envelope consistency: clean.**

## R32-API-VERIFY1 — `tracing::info!` macro emit audit

Per brief: confirm new `tracing::info!` emits don't add public
surface.

1. **`sandbox/nomad-ch create submit_done`** at `nomad_ch.rs:1188-
   1193` — expands inside `pub async fn create_sandbox` body. No
   new items.
2. **`sandbox/nomad-ch alloc_first_seen`** at `nomad_ch.rs:3089-
   3094` — expands inside `async fn wait_for_alloc_running` body
   (function is `pub(crate)` already; the macro adds no item).
3. New locals: `fn_started: Instant` (`:3045`),
   `alloc_first_seen_logged: bool` (`:3053`). Function-local; not
   reachable from outside.

**Verdict**: r32-T1 trace points add zero public surface.
`tracing::info!` is a log emit, not a symbol. Clean.

## R32-API-VERIFY2 — pub surface delta scan

Per brief: identify any new `pub fn` introduced that should be
`pub(crate)`.

1. **`git diff 729f22dd..8d82ecde -- crates/sandbox/src crates/
   sandbox-agent/src | grep pub` = 0 matches.** No `+pub` lines,
   no `-pub` lines.
2. **Indent-allowed pub-fn-or-item count** at r32 in `crates/
   sandbox/src/`: 475. Unchanged from r31. (r31's "300" was a
   stricter filter on top-level items; both metrics show zero
   movement.)
3. **`#[doc(hidden)]` items**: 5 active markers gated under
   `cfg(any(test, feature = "test-support"))`. Unchanged. No
   intent-leak.

**Verdict**: zero pub-surface drift this round. No new `pub fn`
candidates for `pub(crate)` narrowing because **no `pub fn` was
introduced**.

## R32-API-VERIFY3 — R30-API1 carry status

Per brief: closed?

`lib.rs:306` still reads `pub fn nomad_stop_permits(&self) ->
Option<&Arc<NomadStopPermits>>`. **Not closed.** No narrowing
PR landed during the 5-commit window between `729f22dd` and
`8d82ecde`. r32-T1 + perf cadence tightening were the only
in-scope code changes; both orthogonal to visibility.

**Status: open, carry into r33, 3 rounds old.**

## R32-API-VERIFY4 — error-envelope consistency

Per brief: § 10.0 wire envelope still consistent across all admin
handlers?

All 12 admin handlers import `error_envelope::{error_response,
ErrorEnvelope}` at `admin_handlers.rs:71` (verified at audit). r32
commits did not touch admin_handlers.rs at all (full diff
inventory is `nomad_ch.rs` + `restore_handler.rs` + scripts).
Envelope rustdoc on `poll_wake` (`:1967-1969`) unchanged.
`wire_code()` callsites at `:1995`, `:2497`, `:2529` produce
codes from the typed enum.

**Verdict**: envelope consistency **clean**. Open carry is
R24-API3 (async `which` field) only.

## Cross-lens consensus

- **r32-T1 trace points**: net-positive observability; zero
  api-surface delta. Macro emits expand inside existing function
  bodies. The diff-block-comment at `nomad_ch.rs:1186-1190`
  ("emit submit-done milestone") makes the observability
  contract self-documenting.
- **perf-r32-S1 + 4ac1e526 cadence tightening**: literal-value
  changes only (sleep durations). Rustdoc on
  `wait_for_alloc_running_blocking` (`restore_handler.rs:2735`)
  + `wait_for_livez_blocking` doc-comments updated to match new
  values (good housekeeping; rustdoc-correctness, not surface).
- **Security r32 / Performance r32 / Concurrency r32**: no
  api-surface intersect this round.

## Lens hand-off

- **Architecture r33**: R26-API1, R24-API3 unchanged.
- **Test-coverage r33**: all r30/r31/r32 handoffs unchanged.
- **Security r33**: R20-API1 (9-round, quadruply-motivated).
- **Code-quality r33**: bundle R30-API1 (9 items) + R30-API2
  (2 items) + R27-API3 (17 items) + R29-API1/2 + R30-API3
  (docs + AGENTS.md router) + R28-API3 + R27-API1 + R27-API4
  + R26-API5 + R22-API3 + R23-API2 + R23-API3 + R25-API5 into
  one mechanical sweep PR (~50 lines code + ~30 lines prose).

## Backlog

24 entries total open. Pre-r30 carries (no movement r31/r32):
R19-API2, R22-API2, R22-API3, R23-API2, R23-API3, R24-API2,
R24-API3, R24-MIG1, R24-SWEEP1, R25-API3, R25-API4, R25-API5,
R26-API5, R27-API1, R27-API3 (17 items), R27-API4, R28-API3,
R29-API1, R29-API2 — all MINOR. R20-API1 + R26-API1 IMPORTANT
(multi-round). r30 carries: R30-API1 (9 items), R30-API2,
R30-API3 (half-closed) — all MINOR.

Net: r31 open = 11 → r32 open = 11 (0 closures, 0 new; net 0).

## Trend

- **r17-r22**: §10.0 envelope + typed-error landed.
- **r23-r27**: pre-flight + observability surface; BackendBuilder
  consolidated.
- **r28-r29**: minimum-disclosure forward-pressure; backlog drops
  to 9.
- **r30**: T-7/T-8 cutover SHRINKS pub surface; r30-A1 semaphore
  ADDS pub surface (with overshoot); backlog rises to 11.
- **r31**: steady state. Two runtime-behavior landings + two
  paperwork commits. Zero pub-surface delta. R31-M1 closes the
  in-source half of R30-API3.
- **r32 (this round)**: **steady state continues**. Five
  commits in scope (1 closure-of-S1 in scripts, 2 perf cadence
  tightenings, 1 observability r32-T1 trace, 1 docs). **Zero
  pub-surface delta** (475 → 475 by the broader filter; 300 →
  300 by r31's stricter filter — both monotonic). No new
  findings. No closures.

**Defining theme of r32: trace-instrument without leaking surface.**
The r32-T1 work added 2 `tracing::info!` milestones to attribute
CREATE-path latency between Nomad scheduling and in-VM boot —
exactly the kind of observability that historically tempts authors
to add `pub fn emit_trace(…)` helpers. r32 resisted: the emits
inline into existing function bodies, no new items. The carry
backlog is steady at 11; all 9 active MINOR items are mechanical
narrowings that will close in a single ~50-line code-quality
sweep PR whenever scheduled. The 2 IMPORTANT items remain
out-of-tree (R26-API1) and multi-round-blocked (R20-API1).

**Net pub-count delta: 0.**

**Biggest leak (3 rounds running)**: `AppState::nomad_stop_permits()`
at `lib.rs:306` returning `&Arc<NomadStopPermits>`. Fix:
one-line `pub` → `pub(crate)`. R30-API1 carries forward to r33.
