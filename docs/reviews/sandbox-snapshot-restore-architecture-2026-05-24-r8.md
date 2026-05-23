# Architecture review — 2026-05-24 round 8

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `c07cbb62` (r7 reviewer artifacts; last Rust commit `cdd2e677`)
**Lens**: architecture — B-class stabilised; consolidation triage; B22 wire/boundary
**Prior**: `…-architecture-2026-05-24-r7.md` (HEAD `27aa393a`)

## Summary

8 findings (3 critical, 3 important, 2 minor). All B-bugs
B14-B23 CLOSED at HEAD. r7's "first non-amplifying cycle"
held one cycle: the two Rust commits since (`6f5d41b8`
clock-resync, `cdd2e677` `Arc<dyn>` flip) net +2 structural
items (R7-S1, R7-S2) + perf re-characterisation (R7-P1).
Triage-debt: **3/14 r6-triage items landed (all proposal 2); 11
open.** Of 20+ deferred items, **≥9 would close as side effect
of executing the 3-proposal triage** (see C1).

## CRITICAL

**C1. Triage subsumption — proposal execution stalls 3/14.**

| Proposal | Subsumes (file:line) | Closed | Open |
|---|---|---|---|
| 1. `LeasedVmSlot` RAII | R4-A2, R5-A2, R5-C1, R7-C2, R5-T1, B18/B19-class | 0 | 6 |
| 2. `SnapshotCapableBackend` split | R3-A1, R5-A1, R5-API1/2, R4-S1 | 3 (`93348b91`) | 2 |
| 3. `AppStateBuilder` typed-state | R4-A1, R3-Q2-pattern, R7-S2-pattern | 0 | 3 |

Total subsumable on execution: **11 of 20**. The "3 pub(crate)
commits closed 3 items net" framing masks the leverage gap:
proposal 1 alone closes 6 items + prevents B18/B19-class
regressions documented at `nomad_ch.rs:944-952,1087-1091`. Three
cycles since R4-A2 opened — zero LOC against it.

**C2. R7-S2 `derive_agent_url` default is arch-level fail-OPEN,
not security-only.** `restore_handler.rs:182-184` adds a trait
method whose default returns `http://127.0.0.1:0`. Control-flow
consequence: `do_restore_inner` at `restore_handler.rs:512-519`
unconditionally calls `derive_agent_url` then sends a signed
POST to whatever it returns. A `StubRestoreBackend` that forgets
to override silently POSTs to a sentinel. Same anti-pattern as
R5-Q1's `register_restored` default `Ok(())` — and *that one*
landed with an explicit warn-comment about silent no-ops. The
pattern repeats unblocked by the prior warning. Required-method
or `panic!` default; never a sentinel URL.

**C3. B22 introduced a wire contract with no round-trip test.**
`/_clock_resync` is now a stable RPC: agent
(`crates/sandbox-agent/src/handlers.rs:579-637`) and controller
(`crates/sandbox/src/restore_handler.rs:1444-1505`) must agree on
canonical body `{"ts":<unix_secs>}`, header set
(`x-sbx-{timestamp,nonce,signature}`), and `CanonicalKind::V1`.
Agent advertises `"clock.resync-v1"` at
`version.rs:54`; controller never reads it (R7-API2). The
`pub fn verify_kind_skew_bypass` at `sig.rs:356` is pub on
`pub mod sig` — same anti-pattern R4-S1/R5-API1/2 just closed,
regressed across the crate boundary (R7-API1). The boundary
arch-r3 called "clean" is muddied not by `use`-graph (controller
already imports `sandbox_agent::sig` from 6 sites) but by
contract surface area. Add a controller↔agent canonical-string
round-trip integration test that exercises both `verify_kind`
and `verify_kind_skew_bypass` against the same canonical
builder, so a unilateral edit in either crate fails the build.

## IMPORTANT

**I1. AppStateBuilder reaches 10 builders; inflection passed.**
HEAD count: 7 on `AppState` (`lib.rs:217,276,316,324,350,361,372`)
+ 1 on `SandboxConfig` (`config.rs:599`) + 2 on
`RealRestoreBackend` (`restore_handler.rs:926,943`). Plus
`AppState::new_fixture` at `lib.rs:393` still `pub` — test
scaffolding leaked to prod API. r7-I2's 3-phase migration
subsumes R4-A1 + auto-closes R6-A1's escape hatch + collapses
the `assert_persist_required_when_snapshot_enabled` boot guard
at `lib.rs:809` into the type. Next field (the next will be
A1's AEAD-prod-wrap) adds another `with_*` rather than being
type-required.

**I2. R7-P1 architectural pattern: half-applied
`spawn_blocking` discipline.** `cdd2e677` wrapped `store.get`;
`snapshot_handler.rs:316-373,566-621` still runs
`ChRemoteClient::pause`, `ch.snapshot`, and
`LocalDiskSnapshotStore::put` synchronously inside the async
handler. `snapshot_store.rs:99` doc-comment mandates
`spawn_blocking`; zero put-callers honour it. Asymmetric
application of an explicit discipline is itself a structural
smell.

**I3. R7-C1 detached spawn has no shutdown-quiescence story.**
`admin_handlers.rs:1310-1324` fires
`compio::runtime::spawn(...).detach()` on every snapshot (R6-P1
fix). `AppState::trigger_shutdown` at `lib.rs:177-191` flips a
flag + writes pg, but detached teardowns continue against a
draining controller until process exit. Architecture-level fix:
a `DetachedTaskTracker` on `AppState` that shutdown awaits with
bounded timeout. Same shape protects every future R6-P1-style
detach.

## MINOR

**M1. Boundary stability assessment (positive).**
`sandbox-agent::sig` is imported from 6 controller sites
(`backend/{nomad_ch,docker,k8s}.rs`, `preview.rs`,
`preview_ws.rs`, `persist.rs`, `restore.rs`,
`restore_handler.rs`). Adding `/_clock_resync` doesn't widen
the dependency direction. The muddying is contract-surface,
not dependency-direction. Boundary remains clean; C3 is the
right finding.

**M2. Fix-induced-finding ratio re-inverted to ≥1.**
r7's non-amplifying cycle was a one-cycle phenomenon: the
moment B22 (substantive feature) landed, the ratio bounced.
Confirms r6's hypothesis from the opposite direction — *only*
visibility narrowing / no-op refactors break the spiral; *any*
feature work amplifies. r9 binding question: can proposal 1
(LeasedVmSlot, ~220 LOC, 2 files) ship as a single conservative
refactor without adding features?
