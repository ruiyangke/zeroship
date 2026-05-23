# Architecture review — 2026-05-24 round 3

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `4340e3b5`
**Lens**: architecture — structural decay
**Prior**: `…-architecture-2026-05-24-r2.md` (HEAD `09dfd902`)

## Summary

8 findings (3 critical, 3 important, 2 minor). Recent commits closed **zero r2 architecture items**: `da220268` (T7 join_all), `d9b95c2e` (A6 persist `pub(crate)`), `0061b96d` (cluster Docker) are point fixes, not structural. T9/T10 still open and metastasising. Backend enum, AppState, and restore_handler are all accreting; one bright spot: sandbox-agent ↔ sandbox boundary.

## CRITICAL

**C1. `backend/mod.rs:166-497` — Backend enum has 19 methods; four are nomad-ch-only with `Err`-arms for Docker/K8s.**
`lookup_source_vm_ops` (:384-396), `teardown_source_for_snapshot` (:416-428), `restore_from_sealed` (:359-373), `restore_from_pg_and_sealed` (:433-451) all return `Err("backend X doesn't support …")` for two of three variants. The "why enum not dyn Trait" doc (:34-40) is now stale — each Phase-B method adds two stub arms.
Fix: `pub trait SnapshotCapableBackend` impl'd by `NomadCHBackend` only; lift these four off `Backend`. `AppState` carries `Option<Arc<dyn SnapshotCapableBackend>>` alongside the enum.

**C2. `sweep.rs:283-411` + `admin_handlers.rs:1102-1234` — T9 hardened, not closed.**
`snapshot_one` (:311-386) is still verbatim `admin_handlers::snapshot_sandbox` (:1141-1235); `ResolvedSourceVmOps` byte-for-byte in both. A third caller (cold-boot at `admin_handlers.rs:1297`) is queued.
Fix: new `snapshot_orchestrator.rs`:
```rust
pub(crate) struct SnapshotDeps<'a> {
    pub db: &'a Database, pub store: &'a dyn SnapshotStore,
    pub ch: &'a dyn ChRemoteClient, pub backend: &'a Backend,
    pub snapshot_l1_root: &'a Path,
}
pub(crate) async fn perform_snapshot(
    deps: SnapshotDeps<'_>, sandbox_id: Uuid,
) -> Result<SnapshotOutcome, SnapshotHandlerError>;
```
Admin wraps in 200 envelope; sweep wraps in `Result<(), String>`. `ResolvedSourceVmOps` moves here `pub(crate)`. ~80 LOC removed per caller. Closes T9 + T10 — Arc<AppState> reach disappears when the deps struct is the contract.

**C3. `restore_handler.rs:842-969` — `build_restore_nomad_job_json` is a second `NomadCHBackend` impl with no shared seam.**
`RealRestoreBackend` (:744-833) duplicates wrapper-path / env / Resources / KillTimeout from `nomad_ch::build_nomad_job_json`. Comment at :838 ("no user_id/project_id") is false — helper takes `user_id` (:849). Two `wait_for_alloc_running_blocking`, two `nomad_post_blocking` (:977-986). r2 bug-#9 MemoryMaxMB=2× duplicated by hand (:954-958).
Fix: hoist `build_nomad_job_json` to `backend/nomad_ch.rs` as `pub(crate)` over `enum JobKind { Cold { pubkey_hex }, Restore { from_dir } }`. `RealRestoreBackend` calls it.

## IMPORTANT

**I1. `lib.rs:50-132` — AppState builder churn is the wrong direction; `SnapshotWiring` is.**
r2 asked for `SnapshotWiring { store, ch, backend }: Option<_>` to collapse the trio (:129-131). Still three Options. A5/A6 added two `with_X` builders; deferred [A6b] queues five more. `new_fixture` (:281-295) lists 10 fields; collapsing the trio + a sibling `CredentialWiring` drops it to 6 and the builder ceremony evaporates.
Fix: land `SnapshotWiring` before A6b's five more builders.

**I2. `scripts/nomad-vm-wrapper.sh:336-363` (421 LOC) — wrapper-side `sed -i` JSON rewrite is wrong tool, wrong layer.**
`restore_handler::rewrite_config_json` (:439-477) already does structured serde_json rewrites for `net[]`. Path-bearing fields (`disks[].path`, `serial.file`) are re-rewritten by unanchored regex `sed` (:359) — flagged CRITICAL [W1] in deferred (sed metacharacters in attacker-influenced JSON ⇒ raw_exec-root RCE). Stated reason ("alloc UUID unknown at submit") is sound, but the fix is a `zeroship-sandbox-stage` sidecar binary (~50 LOC) invoked between :324-335 and :366. JSON surgery and `--cmdline` building (:396) don't belong in 400 LOC of bash.

**I3. `backend/mod.rs:267-272` + `nomad_ch.rs:879-924` — `StopDisposition` enum from r2 still isn't there.**
Still a bool (`remove_host_dir`). Deferred [C2] (`persist.delete` fires unconditionally regardless of bool) is the second concrete bug this enum would have prevented. Sealed-record-aware wake will need a third disposition.
Fix: `Backend::stop(id, disposition)`; derive host_dir + persist.delete gates from `matches!(disposition, ReleaseAll)`.

## MINOR

**M1. `backend/mod.rs:34-40`** — "enum not dyn Trait" doc-comment is stale; four Phase-B methods now use Err-arms to encode runtime "not supported". Refresh or delete after C1.

**M2. `crates/sandbox-agent` ↔ `crates/sandbox` boundary is clean — note as healthy.**
Zero `use zeroship_sandbox::` matches in sandbox-agent. Cargo dep flows one way (`sandbox → sandbox-agent`). Wire contract: HTTP surface + `sig::Verifier`. Pin in `sandbox-agent/README.md` so a future refactor doesn't leak controller types across.

## R1+R2 status

| finding | Status |
|---|---|
| r1: `update_lessee` dead code | OPEN — [C1] in deferred |
| r1/A1: AEAD wiring | OPEN |
| r1: Restore↔cold-boot env duplication | **HARDENED** — C3 |
| r1: Wrapper `sed -i` JSON rewrite | OPEN — [W1] CRITICAL; I2 |
| r2 C1: ControllerIdleSnapshotter duplicates admin | **OPEN** — C2 |
| r2 C2: Arc<AppState> 4-field reach | **OPEN** — closes via C2's deps-struct |
| r2 I1: stop bool-flag | **OPEN** — I3 |
| r2 I3: AppState 12-field god struct | **OPEN** — I1 (A6b will worsen) |
| r2 M1: ResolvedSourceVmOps dup | **OPEN** — folded into C2 |

**Net**: zero r2 architecture findings closed. Three new critical themes (Backend trait masquerade, restore_handler-as-second-nomad-backend, wrapper-side JSON surgery) were latent in r1/r2 and have crossed from "smell" to load-bearing wrong abstraction.
