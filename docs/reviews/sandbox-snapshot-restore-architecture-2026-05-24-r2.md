# Architecture review — 2026-05-24 round 2

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `09dfd902`
**Lens**: architecture
**Prior**: `sandbox-snapshot-restore-architecture-2026-05-23-r1.md` (HEAD `8ad3cf3f`)

## Summary
7 NEW findings (2 critical, 3 important, 2 minor). Theme: **T6's idle-sweep bridge re-implements the admin handler and turns `Arc<AppState>` into a god-object handle for `sweep.rs`**. R1 status: B15 host_dir wipe genuinely closed. Zero other r1 findings closed; AEAD now correctly tracked as [A1].

## CRITICAL

**`sweep.rs:283-407` — `ControllerIdleSnapshotter` re-implements the admin handler instead of delegating**
`snapshot_one` re-derives the trio resolution (:313-320), the async `lookup_source_vm_ops` (:325), the `ResolvedSourceVmOps` adapter (:390-407 is byte-for-byte the private struct at `admin_handlers.rs:1109-1129`), and the post-success `teardown_source_for_snapshot` (:353-358 mirrors `admin_handlers.rs:1196-1206`). Two orchestrators must now stay in lockstep — any new admin step (metering, audit, error envelope) must be duplicated or idle-sweep silently diverges. Proposal § 7 said idle-eviction should *invoke* the same path, not parallel-run it.
Fix: extract `snapshot_handler::orchestrate_snapshot(state, sandbox_id)` containing lookup + adapter + handler + teardown. Admin wraps it in a 200 envelope; sweep wraps it in `Result`.

**`sweep.rs:283-294` + `lib.rs:449-454` — `Arc<AppState>` is now a layering escape hatch**
`ControllerIdleSnapshotter` holds `Arc<AppState>` and reaches into FOUR sibling fields (`snapshot_store`, `ch_remote`, `database`, `backend`) plus `config.snapshot_l1_root`. `sweep` was previously a pure DB → CAS consumer; it now imports `backend::nomad_ch::SourceVmOpsHandle` (:391), `snapshot_handler::*`, `snapshot_store::SnapshotStore`. The directed `sweep → snapshot_handler` arrow is now `sweep ↔ {snapshot_handler, backend, snapshot_store, AppState}`. Once a third sweep (LRU, GC) lands, "god-Arc passed everywhere" is locked in.
Fix: `spawn_idle_eviction_sweep(db, snapshotter, shutdown)` — the trait already abstracts the work; `AppState` shouldn't leak past `lib.rs:451`.

## IMPORTANT

**`backend/nomad_ch.rs:879-910` — `stop` / `stop_preserving_state` bool-flag masks a missing type**
Bug-#15 fix is correct, but the API surface is now (a) `Backend::stop`, (b) `Backend::teardown_source_for_snapshot`, (c) `NomadCh::stop`, (d) `NomadCh::stop_preserving_state`, (e) `NomadCh::stop_inner(remove_host_dir: bool)`. Five entry points + one bool that silently decides whether durable storage survives. Caller-side choice is invisible at the Backend trait boundary.
Fix: `enum StopDisposition { ReleaseAll, PreserveDurableState }`; `Backend::stop(id, disposition)`. Makes the invariant grep-able.

**`workspace.img` lifecycle invariant lives in 4 places with no canonical owner**
"Post-snapshot teardown MUST preserve `<host_dir>/workspace.img`" is encoded in `backend/nomad_ch.rs:887-904`, `:1108-1125`, `backend/mod.rs:398-415`, and `scripts/nomad-vm-wrapper.sh:266` + the `[ ! -f $ZSBX_WORKSPACE_IMG ]` gate. `docs/proposals/sandbox-snapshot-restore.md` doesn't mention `workspace.img` (predates virtio-blk pivot). A future wrapper hand-edit has nothing pinning the invariant.
Fix: 30-line "Durable storage lifecycle" section in the proposal (or sibling ADR) naming `workspace.img` + `home.img`, owners, and which teardown variants preserve each.

**`lib.rs:50-109` — `AppState` is a 12-field god struct with 5 `Option` half-states**
R1 flagged the trajectory; this round adds three more (`snapshot_store`, `ch_remote`, `restore_backend`). `ControllerIdleSnapshotter` already does a defensive triple-match (`sweep.rs:313-320`) because the type can't say "all three or none." Every new feature (AEAD per [A1], lessee heartbeat per [C1]) adds another Option + match.
Fix: extract `SnapshotWiring { store, ch, backend }` as one `Option<SnapshotWiring>`; callers do one `if let Some(w)`.

## MINOR

**`sweep.rs:401-407` + `admin_handlers.rs:1122-1128`** — `ResolvedSourceVmOps::teardown_source` is the same 5-line no-op stub copy-pasted into both sites. Fix: ship `pub(crate)` from `snapshot_handler`.

**`config.rs:341,648,753`** — T3's `alloc_running_timeout_secs` 60→120 has no regression test pinning the value. Env-parse (:648) + `Default` (:753) are two silent-drift paths. Fix: unit test for both; cross-link `cluster-2026-05-23-r1.md`.

## R1 status

| r1 finding | Status |
|---|---|
| `update_lessee` never called | OPEN (now [C1]) |
| `SnapshottingAborted` / `RestoringCold` terminal sinks | OPEN — still zero writers for `RestoringCold` |
| Migration 0007 four unused columns | OPEN |
| AEAD wiring | OPEN — now tracked as [A1] |
| Restore↔cold-boot env duplication (`restore_handler.rs:917-934`) | OPEN |
| Wrapper `sed -i` config.json rewrite | OPEN |
| `spawn_idle_eviction_sweep` not in prod | **CLOSED** by T6 — introduces 2 new criticals above |
| Backend enum four `Err` arms | OPEN |
| temp_dir cleanup on success | OPEN |
| `snap.artifact_path` discarded | OPEN |
| Test helper duplication | OPEN |

**Net**: 1 r1 closure (idle-sweep wiring); the closure itself spawns 2 new criticals. Bug #15 fix is clean.
