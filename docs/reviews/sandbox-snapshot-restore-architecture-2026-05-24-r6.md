# Architecture review — 2026-05-24 round 6

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `1066a319`
**Lens**: architecture — higher-order patterns; fix-induced-finding ratio; refactor sketch
**Prior**: `…-architecture-2026-05-24-r5.md` (HEAD `15b4f9a8`)

## Summary

7 findings (3 critical, 2 important, 2 minor). r5 closure since prior:
**R3-Q2 closed at `ac6a6bf2`** (`lib.rs:276`). No Rust commits between
r5 and r6 (`1066a319`/`ad348101` docs-only; `36c1b733` shell). Net
cycle delta: 1 closure / 25+ open criticals carried. Dominant pattern:
**fix-induced finding amplification ≥ 1** — each B-series patch has
opened more structural items than it closed.

## CRITICAL

**C1. Fix-induced-finding ratio ≥ 1 across the last 4 B-fixes.**
Counting deferred backlog entries seeded by each fix: B17
(`nomad-vm-wrapper.sh:340-419` resume) → 3 (R3-A3, R3-T3, W1 stays);
B18 (`lib.rs:617-640` shared allocator) → 2 (R4-A2, R4-S1); B19
(`backend/mod.rs:175-180,449-505`) → **8** (R5-A1, R5-A2, R5-API1,
R5-API2, R5-Q1, R5-S1, R5-T1, R5-C1); B20 → 1 (B21). 1 fix → ≥3 new
criticals on average. Backlog grew from ~12 (r3) to 25+ at r6. The
branch is debt-spiraling. The shape of every B-fix is the same —
"add another method to `Backend`, another `with_*` on `AppState`,
another null-check in the wiring block" — exactly what r3-A1 /
r4-A1 / r4-A2 warned against. Structural fixes have a real
downstream-closure multiplier (I1); spot-fixes do not.

**C2. Five `AppState` fields are 2.5 concepts, not 5.**
`lib.rs:60-160` field clusters: `snapshot_store` + `ch_remote` +
`restore_backend` (`:158-160`) are co-instantiated at `:580-674`
("all three Some or all None", `:143-147`) — single
`SnapshotPipeline` concept fragmented across 3 trait-objects + 3
builders + 3 null-checks. `persist` + `database` overlap: `persist`
holds sealed signing keys; `database` holds the row that references
them. B21 exists *because* the wake path needs both, plus
`SANDBOX_PERSIST_AUTH=1`, plus `state.persist = Some(_)`, plus the
dispatch guard at `restore_handler.rs:450-463` — 4 boolean
conjuncts in 4 files for "is sealed-record persistence on?".
`config` is the only standalone concept. Honest count: **3**
(`BootConfig`, `PersistenceLayer{db,persist}`,
`SnapshotPipeline{store,ch,restore}`).

**C3. The wrapper bash is becoming vestigial.** 456 LOC
(`nomad-vm-wrapper.sh`); controller reach has grown — B8 pulled
config.json rewriting into `restore_handler::rewrite_config_json`;
B17's `ch-remote resume` only fires under controller-driven restore;
B19's `register_restored` is a controller-side post-condition for a
wrapper-spawned VM via the new `nomad_ch_handle()` escape hatch.
R3-A3 proposes folding bash into `zsbx-vm-wrapper` Rust — that is
where the code is already heading. While bash stays, W1
(`sed -i` at `:359` against attacker-influenceable JSON) cannot
structurally close. The wrapper's value-add is now `exec` + a
cleanup trap.

## IMPORTANT

**I1. Triage — 3 structural fixes that maximally collapse backlog.**

1. **`LeasedVmSlot` RAII** (`nomad_ch.rs:944-952` + `:1087-1091` +
   `:1650-1681`). Holds state-map `OccupiedEntry` + `vm_index`
   reservation atomically; Drop or `.commit()` is the only release
   path. **Closes**: R4-A2, R5-A2, R5-C1, R5-T1, B18/B19 surface
   fixes subsumed. Prevents the entire B-class pattern (9 of 25
   backlog items). ~150 LOC, 1 file.

2. **`SnapshotCapableBackend: Backend` trait split**
   (`backend/mod.rs:425-528`). `Arc<dyn SnapshotCapableBackend>` at
   the 4 nomad-only call sites. **Closes**: R3-A1, R5-A1, R5-API1,
   R5-API2, R4-S1, plus the 47-line null-check wiring in r5-I3.
   Deletes 5 `Err("backend X doesn't support …")` arms. ~300 LOC
   across 3 files.

3. **`AppStateBuilder` typed-state + 3-group field collapse**
   (`lib.rs:50-407`, `:580-674`). `PersistenceLayer::from_env`
   makes `SANDBOX_PERSIST_AUTH=1` an invariant, not a silent boolean.
   `SnapshotPipeline` makes "all three Some" a type. **Closes**:
   R4-A1, R3-Q2 siblings, B21 structurally, R5-S1 fail-OPEN
   (impossible to construct snapshot pipeline without persist).
   ~400 LOC across 4 files.

**Migration order**: day 1-2 `LeasedVmSlot` (internal to nomad-ch,
lands first) → day 3-4 trait split (exposes `Arc<dyn …>` for next
step) → day 5-7 `AppStateBuilder` + groups. <850 LOC delta across
6 files; existing tests anchor regression detection. **Estimated
downstream closure: ~14 of 25 backlog criticals via structural
subsumption.**

**I2. `restore_handler.rs:162-170` trait default `Ok(())` is a
type-level fixable footgun (R5-Q1 carry).** Remove the default —
test stubs make the no-op decision explicitly, not via the trait
falling through silently. Today's behavior disagrees with prod's
loud-fail at `:1018-1047`; this is one line of trait-surface
change.

## MINOR

**M1. `backend/mod.rs:34-40` "Why an enum, not a `dyn Trait`" comment
is now wrong.** `nomad_ch_handle()` (`:467-474`) +
`vm_index_allocator()` (`:449-456`) hand out `Arc<NomadCHBackend>`
for dyn-style dispatch — the stated invariant is already violated.

**M2. R3-Q2 closure (`ac6a6bf2`) is the exemplary refactor shape.**
1-line signature change; 9 callsites unchanged because
`Result<Self,_>` → `Self` is a contraction. Same shape applies to
any future infallible builder.

## r5 status

R3-Q2 **CLOSED** (`ac6a6bf2`). r5 C1/C2/I1/I2/I3/M1 **all OPEN, no
change** (no Rust commits this round). 1/7 closed — backlog is now
review-bound, not fix-bound; the I1 triage is the unblock.
