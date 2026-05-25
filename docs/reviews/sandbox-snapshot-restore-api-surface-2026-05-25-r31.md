# Sandbox/snapshot-restore — api-surface r31 review

Date: 2026-05-25 (UTC). HEAD at audit: `729f22dd`.
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.
Read-only. Round 49 of pilot-cron loop. Prior: r30 (`ba1df3f0`), cycle 48.

Landed since r30 (api-surface impact):

- `b75728ce` R31-P1 — allocator tuning: `vm_index_ceil` default
  raised, `vm_index_release_delay_secs` 5→2. Touches
  `config.rs` defaults + one rustdoc rewrite. **No pub deltas.**
- `4d10ba45` R31-M1 — comment-only cleanup, replaces 8 stale
  rustdoc `wrapper` references in `nomad_ch.rs` and
  `restore_handler.rs` with ch-driver equivalents. **No symbol
  surface change.**
- `29a2dc95` concurrency r31 paperwork — `docs/reviews/` only.
- `729f22dd` controller pin v38→v39 — `scripts/` only.

## Summary

**Net pub-count delta r30 → r31: 0.** Indent-allowed pub items in
`crates/sandbox/src/`: 300 at ba1df3f0 = 300 at 729f22dd. Zero
`+pub` / `-pub` lines in `git diff ba1df3f0..729f22dd -- crates/
sandbox/src crates/sandbox-agent/src`.

**Zero new findings, zero closures. Backlog steady at 11.**

R31-M1 closes the in-source half of R30-API3 (8 rustdoc-comment
sites referencing the deleted wrapper); the operator-facing
`docs/runbooks/sandbox-nomad-ch.md` + the AGENTS.md task-router
line still describe the deleted architecture, so R30-API3 stays
open as the docs-only residual.

## CRITICAL

None.

## IMPORTANT

### [R26-API1] (carry, 6 rounds) driver-side `nomad_driver_ch_destroy_task_unreaped_total` has no operator-readable surface

Out-of-tree `nomad-driver-ch` repo. Unchanged. Owner: out-of-tree
driver / observability ADR.

### [R20-API1] (carry, 8 rounds) schema-marker rewriter sites unchanged

4 path-derivation rewriter sites in `restore_handler.rs` /
`nomad_ch.rs`. `cdcd670d` (last round) deleted the fifth site
(`wrapper_path`) correctly. Quadruply-motivated; held. Owner:
security (driver-side validator landing).

## MINOR

### [R30-API1] (carry) `NomadStopPermits` + `NomadStopPermitGuard` + 7 methods `pub` with zero external consumers

`nomad_ch.rs:558,609,632,638,654,715,729` + `lib.rs:306`. All 9
items still `pub`. Concurrency r31 paperwork (`29a2dc95`) did
NOT include a narrowing PR. Fix: 9-line mechanical
`pub`→`pub(crate)`. Owner: code-quality r32.

### [R30-API2] (carry) `metrics::inc/dec_nomad_stop_permits_in_use` `pub fn`

`metrics.rs:496,505`. Callers in-file only. Bundle with R27-API3
17-item sweep. Owner: code-quality r32.

### [R30-API3] (carry, half-closed) operator-facing docs still describe pre-T-8 architecture

`docs/runbooks/sandbox-nomad-ch.md` audience / prerequisites /
config table / triage all reference deleted `raw_exec` + wrapper.
**AGENTS.md task router** still lists `crates/sandbox/scripts/
nomad-vm-wrapper.sh` as a route — that file was deleted at
`cdcd670d`; an operator following the routing table would `ls` a
non-existent path. R31-M1 cleaned 8 in-source rustdoc sites; this
docs-surface half was out of scope. Fix: ~30-line prose update +
1-line AGENTS.md edit. Owner: code-quality r32.

### [R29-API1] (carry) `release_vm_index_after` + `spawn_delayed_release_in_worker` `pub fn`

`nomad_ch.rs:396,438`. R31-P1 shrank the runtime delay value but
did NOT touch visibility. Owner: code-quality r32.

### [R29-API2] (carry) `spawn_delayed_release_in_worker` `pub` + `#[allow(dead_code)]` + zero production callers

`nomad_ch.rs:436-455`. Pick one of three coherent endings (narrow
/ delete / find caller). Owner: code-quality r32 / architecture.

### [R28-API3] (carry) `test-support` feature lacks crate-level rustdoc warning

`Cargo.toml:71-77` + `lib.rs:1-9`. 5 active `#[doc(hidden)]`
markers ride on the gate (`sweep.rs:494`, `restore.rs:613`,
`snapshot_handler.rs:693`, `db.rs:521`,
`restore_handler.rs:1307`). Each gated `cfg(any(test, feature =
"test-support"))` — production rlib strips them correctly. The
`pub` is needed because external `tests/` consume them; doc-hidden
is the right marker. **No intent-leak.** Crate-root rustdoc still
unmarked. Owner: code-quality r32.

### [R27-API1] (carry) `Backend::builder` rustdoc lacks cross-reference to `BackendBuilder`

`backend/mod.rs:260-264`. Owner: code-quality r32.

### [R27-API3] (carry, 17-item sweep steady) `metrics::*_value` + `inc/dec_*` `pub` with zero external consumers

`metrics.rs` — 17 functions enumerated in r30. No new metrics
functions this round; target list unchanged. Owner: code-quality
r32.

### [R27-API4] (carry) `WakeErrorCode` rustdoc table 8 of 10 variants

`db.rs:1725-1736` table; `wire_code()` match at `:1749-1762`
covers 10. Missing from table: `StagingPathMissing`,
`AgentVersionMissing`. Owner: code-quality r32.

### Other carries (no movement)

R19-API2, R22-API2, R22-API3, R23-API2, R23-API3, R24-API2,
R24-API3, R24-MIG1, R24-SWEEP1, R25-API3, R25-API4, R25-API5,
R26-API5.

## §10.0 envelope state post-r31

**Inventory delta from r30**: none. R31-M1 is rustdoc-only;
R31-P1 changes config defaults not wire shape. The 32 §10.0
codes are unchanged. `WakeErrorCode` remains 10 variants.

**Fallible handler scan**: 12 `pub async fn` admin handlers at
`admin_handlers.rs:405-2055` (`list_all_sandboxes`,
`get_sandbox_detail`, `list_user_sandboxes`, `list_user_shares`,
`list_hosts`, `export_user`, `delete_user`, `snapshot_sandbox`,
`wake_sandbox`, `poll_wake`, `cold_boot_sandbox`,
`metrics_endpoint`). All route through `crate::error_envelope::
{ErrorEnvelope, error_response}` (imported at `:71`). The
`error_envelope` module's surface is entirely `pub(crate)`
(struct `:45`, fn `:130`, mod `:139`). **Envelope consistency:
clean.**

## R31-API-VERIFY1 — orphan-reference scan for deleted cutover symbols

Per brief: verify no callers dangle after the T-7/T-8 cutover at
`cdcd670d`.

1. **`TaskDriverMode`** — zero matches in
   `crates/sandbox/src/` + `crates/sandbox-agent/src/`.
2. **`SANDBOX_TASK_DRIVER`** — zero matches.
3. **`nomad_vm_wrapper` / `nomad-vm-wrapper.sh`** — zero matches
   in Rust source. R31-M1 cleaned the 4 stale rustdoc
   comment-refs r30 noted incidentally.
4. **`wrapper_path`** — zero matches.
5. **`task_driver_mode_from_env`** — zero matches.
6. **`raw_exec`** — 14 surviving references in
   `crates/sandbox/src/` (10 `nomad_ch.rs`, 2 `restore_handler.rs`,
   2 `snapshot_store.rs`). **All 14 are in COMMENTS or
   test-assertion strings** pinning the negative invariant
   ("raw_exec `command` field must NOT appear in ch driver
   Config"). Correctness pins, not dead references. Appropriate.
7. **AGENTS.md task router** lists `nomad-vm-wrapper.sh` — folded
   into R30-API3 (docs drift).

**Verdict**: orphan-reference scan **clean** in Rust source.

## R31-API-VERIFY2 — pub surface delta scan

Per brief: identify new public surface added by `ade8fb46`
(NomadStopPermits) that should be crate-private.

1. **`ade8fb46`** landed last round; the 9 new pub items are
   r30-A1 = R30-API1 (carry above).
2. **r31 commits** diffed via `git diff ba1df3f0..729f22dd --
   crates/sandbox/src crates/sandbox-agent/src`: **zero `+pub`
   lines, zero `-pub` lines.** Surface byte-identical.
3. **Indent-allowed pub-item count**: r30 = 300; r31 = 300.
4. **`#[doc(hidden)]` items**: 6 sites (1 stale comment-ref at
   `metrics.rs:376` + 5 active markers). All 5 active markers
   are R28-API2 test-support leak fences with intact gating.
   None should be narrowed (external tests consume them); doc-
   hidden is the correct marker. **No intent-leak items.**
5. **`error_envelope` module** — entirely `pub(crate)`.

**Verdict**: zero pub-surface drift this round.

## R31-API-VERIFY3 — error-envelope consistency

Per brief: verify every fallible admin handler returns the §10.0
wire envelope. **All 12 admin handlers import
`error_envelope::{error_response, ErrorEnvelope}` at
`admin_handlers.rs:71`.** `poll_wake` envelope rustdoc at
`:1967-1969` matches §10.0:
`{error: <wire_code>, message, state: "failed", wake_id, …}`.
`wire_code()` callsites at `:1995`, `:2497`, `:2529` produce
codes from the typed enum.

**Verdict**: envelope consistency **clean**. Open carry is
R24-API3 (async `which` field) only.

## Cross-lens consensus

- **R31-M1**: net-positive cleanup; closes 8 rustdoc-comment
  sites. Could have folded R30-API3's docs half into same PR;
  didn't. Half-closure recorded.
- **R31-P1**: runtime-behavior tuning only; no api-surface
  delta. The rustdoc rewrite on `config.rs:444-454` is good
  housekeeping (operator-readable explanation of the v24
  OFD-probe rationale).
- **Security r31 / Performance r31**: no api-surface intersect
  this round.

## Lens hand-off

- **Architecture r32**: R26-API1, R24-API3 unchanged.
- **Test-coverage r32**: all r30/r31 handoffs unchanged.
- **Security r32**: R20-API1 (8-round, quadruply-motivated).
- **Code-quality r32**: bundle R30-API1 (9 items) + R30-API2
  (2 items) + R27-API3 (17 items) + R29-API1/2 + R30-API3
  (docs + AGENTS.md router) + R28-API3 + R27-API1 + R27-API4
  + R26-API5 + R22-API3 + R23-API2 + R23-API3 + R25-API5 into
  one mechanical sweep PR (~50 lines code + ~30 lines prose).

## Backlog

24 entries total open. Pre-r30 carries (no movement r31): R19-API2,
R22-API2, R22-API3, R23-API2, R23-API3, R24-API2, R24-API3,
R24-MIG1, R24-SWEEP1, R25-API3, R25-API4, R25-API5, R26-API5,
R27-API1, R27-API3 (17 items), R27-API4, R28-API3, R29-API1,
R29-API2 — all MINOR. R20-API1 + R26-API1 IMPORTANT (multi-round).
r30 carries: R30-API1 (9 items), R30-API2, R30-API3 (half-closed
by R31-M1) — all MINOR.

Net: r30 open = 11 → r31 open = 11 (0 closures, 0 new; net 0).

## Trend

- **r17-r22**: §10.0 envelope + typed-error landed.
- **r23-r27**: pre-flight + observability surface; BackendBuilder
  consolidated.
- **r28-r29**: minimum-disclosure forward-pressure; backlog drops
  to 9.
- **r30**: T-7/T-8 cutover SHRINKS pub surface; r30-A1 semaphore
  ADDS pub surface (with overshoot); backlog rises to 11.
- **r31 (this round)**: **steady state**. Two runtime-behavior
  landings (R31-P1 + R31-M1) + two paperwork commits. **Zero
  pub-surface delta** (300 → 300). R31-M1 closes the in-source
  half of R30-API3. No new findings, no closures.

**Defining theme of r31: no-news-is-good-news.** The cycle
landed runtime improvements (semaphore install + release-delay
shrink + ceil bump) and a rustdoc cleanup, all without leaking new
pub surface. The carry backlog is steady at 11; all 9 active
MINOR items are mechanical narrowings that will close in a single
~50-line code-quality sweep PR whenever scheduled. The 2
IMPORTANT items remain out-of-tree (R26-API1) and
multi-round-blocked (R20-API1).

**Net pub-count delta: 0.**

**Biggest leak**: same as r30 — `AppState::nomad_stop_permits()`
at `lib.rs:306` returning `&Arc<NomadStopPermits>`, handing an
external test harness everything it needs to acquire and hold a
permit indefinitely (DoS shape against `stop_inner`). Fix:
one-line `pub` → `pub(crate)`.
