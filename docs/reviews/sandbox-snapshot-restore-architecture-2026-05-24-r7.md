# Architecture review — 2026-05-24 round 7

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `27aa393a` (r6 reviewer artifacts; last Rust commit `93348b91`)
**Lens**: architecture — r6 triage progress; concrete `LeasedVmSlot` sketch; fix-induced-finding metric refresh
**Prior**: `…-architecture-2026-05-24-r6.md` (HEAD `1066a319`)

## Summary

7 findings (2 critical, 3 important, 2 minor). Cycle delta since
r6: **3 of 14 r6-triage items closed** at `93348b91` via
`pub(crate)` on `register_restored`, `nomad_ch_handle()`,
`vm_index_allocator()` (`backend/mod.rs:388,467-505`). First cycle
in 4 to NOT amplify the backlog — `93348b91` net +0;
`27aa393a` docs-only. r6's fix-induced-finding ratio of ≥1 is
broken. Remaining gap: the enum-as-trait shape (R3-A1, R5-A1) is
hidden, not fixed — `pub(crate)` shuts the leak; the `Err("backend
X doesn't support …")` dead-arm pattern persists.

## CRITICAL

**C1. r6-triage status after `93348b91`.**

| Triage item | Closes (r6 mapping) | Landed | Open |
|---|---|---|---|
| 1. `LeasedVmSlot` RAII | R4-A2, R5-A2, R5-C1, R5-T1 (+B18/B19) | 0 | all 4 + B-class |
| 2. `SnapshotCapableBackend` split | R3-A1, R5-A1, R5-API1, R5-API2, R4-S1 | 3 (`93348b91`) | R3-A1, R5-A1 |
| 3. `AppStateBuilder` typed-state | R4-A1, B21-struct, R5-S1-struct | 0 | all 3 |

Net: 3/14 landed, all in proposal 2 — the two highest-leverage
proposals (1 and 3) are untouched.

**C2. `LeasedVmSlot` sketch — actual LOC ≈ 220, not 150.** Concrete
shape grounded in `nomad_ch.rs:201-225,945-955,977-1163,1650-1681`:

```rust
// nomad_ch.rs near NomadChSandbox (:201).
// Owns (state-map slot, vm_index reservation) atomically.
pub(crate) struct LeasedVmSlot {
    sandbox_id: Uuid,
    vm_index: u16,
    state: Arc<RwLock<HashMap<Uuid, NomadChSandbox>>>,
    allocator: Arc<Mutex<VmIndexAllocator>>,
    inserted: bool,    // record staged in map
    committed: bool,   // caller succeeded; Drop becomes no-op
}

impl LeasedVmSlot {
    // Create-side: vm_index alloc + vacant-entry check.
    pub(crate) fn acquire(sandbox_id, allocator, state) -> Result<Self,_>;
    // Restore-side: vm_index already reserved by shared allocator
    // (B18); guard adopts it so Drop releases on cancel. Closes R5-C1.
    pub(crate) fn adopt_restored(sandbox_id, vm_index, allocator, state) -> Self;
    // Stage record post-/livez. Closes R5-A2 (4 insert paths → 1).
    pub(crate) fn install_record(&mut self, s: NomadChSandbox) -> Result<(),_>;
    pub(crate) fn commit(mut self) { self.committed = true; }
    pub(crate) fn vm_index(&self) -> u16 { self.vm_index }
}

impl Drop for LeasedVmSlot {
    fn drop(&mut self) {
        if self.committed { return; }
        if self.inserted {
            self.state.write().unwrap_or_else(|p| p.into_inner())
                .remove(&self.sandbox_id);
        }
        // Cancel before /livez Ok: no live VM, sync release is safe.
        // Cancel after /livez Ok: caller should `commit()` and let
        // normal stop_inner (:1076-1163) run the fence-then-release.
        self.allocator.lock().unwrap_or_else(|p| p.into_inner())
            .release(self.vm_index);
    }
}
```

Call-sites: `CreateGuard` (`:1879-1920`) folds in (~40 LOC
removed); `register_restored` (`:1650-1681`) → `adopt_restored` +
plumbing at `restore_handler.rs:485-499` (~30 LOC); wake-cancel
ownership transfer across 3 trait calls (~50 LOC); fence-bypass
cancel branch in Drop is the bulk (~50 LOC). **Honest estimate
~220 LOC, 2 files.** Closes R4-A2, R5-A2, R5-C1, R5-T1; prevents
B18/B19-class regressions.

## IMPORTANT

**I1. SnapshotCapableBackend split — still worth it, lower
urgency.** `backend/mod.rs:175-180,449-505` still has 4 methods
that `Err("backend X doesn't support …")` for 2/3 variants.
`pub(crate)` made them in-crate-only but did not delete them;
`admin_handlers.rs::wake_sandbox` still does `if let
Backend::NomadCh(_) = …` dispatch. Necessary but not sufficient.
Priority drops from "ship before B-class fix #5" → "ship before
any 2nd SnapshotCapable impl". ~300 LOC estimate holds.

**I2. `AppStateBuilder` typed-state — incremental migration
viable.** HEAD count: **8 `with_*`** (7 on `AppState` at
`lib.rs:217,276,316,324,350,361,372` + 1 on `SandboxConfig`
at `config.rs:599`) + **2 `new_fixture`** (`lib.rs:393`,
`config.rs:619`). 3-phase shippable migration:
(1) `SnapshotPipeline{store,ch,restore}` collapses 3 of 7 builders;
makes the "all-Some or all-None" invariant at `lib.rs:142-147`
type-enforced. ~80 LOC.
(2) `PersistenceLayer{db,persist}` collapses 2 more;
`assert_persist_required_when_snapshot_enabled` (`lib.rs:809`)
becomes a method; R6-A1 escape hatch auto-closes by `#[cfg(test)]`
colocation. ~120 LOC.
(3) `new_fixture` → `#[cfg(test)]`. ~30 LOC moved + ~200 LOC
fixture migration across `config.rs:744`, `backend/docker.rs:832`,
`backend/nomad_ch.rs:3449`, 4 e2e tests. **Total ~400 LOC matches
r6.** Each phase ships standalone — no big-bang refactor needed.

**I3. R3-A3 wrapper urgency driver flips.** R6-C3 framed bash
Rustification as "W1 structural closure". Cycle 2026-05-23 reframes
it: bug #22's clock-resync landed in Rust
(`restore_handler.rs:465-498`), not bash; wrapper LOC stable at
456 but reach shrinking as controller absorbs features. R6-C1's
3-line `RESUME_PID=$!` fix is open across 5 rounds — bus-factor
liability, not scaling. New driver: ownership clarity, not LOC.

## MINOR

**M1. First non-amplifying cycle.** B17→3, B18→2, B19→8, B20→1,
B21→1 (#22); `93348b91` (visibility narrowing)→0; `27aa393a`
(docs-only)→0. Metric inverted because the cycle was conservative
refactor, not additive feature. Confirms r6's hypothesis:
structural conservatism breaks the spiral.

**M2. Proposal 1 stall is the binding constraint.** R4-A2
(`LeasedVmSlot`) opened r4; three cycles later, zero LOC. Each
cycle picks visibility narrowing or B-class fix instead. Closure
rate: proposal 1 = 4 items + B-class prevention / ~220 LOC ≈ 18
LOC/closure; proposal 2 partial = 3 items / ~50 LOC ≈ 17
LOC/closure but no structural win. If a future P0 goal is "open
criticals < 10", proposal 1 is the only viable path — every other
fix has been a single-item closure.
