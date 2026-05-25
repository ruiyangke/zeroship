# Sandbox/snapshot-restore — architecture r10 review

Date: 2026-05-25 (UTC)
HEAD at audit: `9678a840`
Round 10 of N (architecture lens).
Prior round: `sandbox-snapshot-restore-architecture-2026-05-25-r9.md`.

Scope read: `crates/sandbox/**`, `crates/sandbox-agent/**` only.

## Summary

7 NEW findings (2 critical, 3 important, 2 minor). Recent landed
since r9 (`419c154b` AEAD negative tests, `6f314025` Tiered::put
spawn_blocking, `0e71e5c4` sweep CAS predicate fix,
`10bddc20` boot_init_sandbox_id wrapper) closed two API-surface
items but introduced one new structural seam (`claim_orphan_transient_for_recovery`
in `db.rs`, mate `unregister_restored` in `nomad_ch.rs`).
**All three r8 flagship triage items still 0/N landed** (R4-A2
LeasedVmSlot, R3-A1/R5-A1 SnapshotCapableBackend split, R4-A1
AppStateBuilder); r9 C3 (AEAD fail-OPEN on GCS path) holds.

## Findings (NEW since r9)

### [R10-A1] `claim_orphan_transient_for_recovery` lands in db.rs at line 2500 — db.rs now 3003 LOC, +2 sweep-recovery seam (IMPORTANT, architecture-r10)

- **Files**: `crates/sandbox/src/db.rs:2407` (`transient_state_lease_expired_sandboxes`),
  `crates/sandbox/src/db.rs:2500` (`claim_orphan_transient_for_recovery`),
  `crates/sandbox/src/sweep.rs:127` (`run_transient_takeover_once`,
  the only caller); `crates/sandbox/src/restore_handler.rs:332`
  (`read_snapshot_row`, the inline pg reader living *outside* db.rs).
- **Symptom**: `db.rs` is now 3003 LOC carrying 41 `pub`/`pub async`
  methods. The new C1-FOLLOWUP recovery CAS adds ~150 LOC + 24 lines
  of inline doc comment for §6.1 / §9.2 semantics that exist nowhere
  else in db.rs. It is **only ever called from `sweep.rs`** —
  the rest of db.rs is pool/migration/host/sandbox CRUD with no
  knowledge of the transient-state lifecycle. Meanwhile
  `restore_handler.rs:332` does the inverse: an inline
  `client.query_opt(SELECT snapshot_artifact_path, snapshot_sha256, …)`
  living *outside* db.rs because folding a 4-column read into
  `SandboxRow` would ripple. Neither file's responsibility is clean.
- **Why it matters**: The architectural seam between "transactional
  sandbox-row CRUD" and "snapshot/restore lifecycle recovery" was
  smudged when C1-FOLLOWUP landed. db.rs grew into the recovery layer
  because that was the path of least diff resistance. The CAS predicate's
  understanding of §6.1's "other controller's wedge" is a sweep-layer
  concept (the defensive `expected_host_id == self.host_id()` check at
  db.rs:2518 enforces it), but the SQL lives in db.rs where the rest
  of the code base doesn't think about it. Future recovery additions
  (a §9.2 fourth state, a tighter takeover policy) will accrete onto
  db.rs by precedent. The mirror in restore_handler — an inline
  `query_opt` for SnapshotRowMeta — is the same anti-pattern from the
  other side.
- **Action**: Extract `crates/sandbox/src/db_recovery.rs` (the SQL
  half) + co-locate the sweep query and the recovery CAS there;
  fold the inline `read_snapshot_row` from restore_handler.rs in.
  Net effect: db.rs goes from 3003 → ~2700 LOC, sweep+restore each
  drop their inline-pg sin, and the next recovery item has an obvious
  home. Bonus: the §9.2 recovery_target match + the CAS that consumes
  it (currently split sweep.rs:108 ↔ db.rs:2500) sit beside each
  other. Sketch:

  ```
  db.rs                 ─ migrations, pool, hosts, sandbox CRUD, shares, events
  db_recovery.rs (new)  ─ transient_state_lease_expired_sandboxes
                          claim_orphan_transient_for_recovery
                          read_snapshot_row (from restore_handler)
                          recovery_target  (from sweep.rs)
  sweep.rs              ─ event-loop + orchestration only
  ```

  Cost: ~250 LOC moved, ~50 LOC of new module boilerplate, no
  semantic change. Fits in the same "split db.rs" arc as R10-A4 below.

### [R10-A2] `RestoreBackend` trait now 7 methods, two `Arc<NomadCHBackend>`-only fields hidden behind it — `Real` is structurally a second `NomadCHBackend` (CRITICAL, architecture-r10)

- **Files**: `crates/sandbox/src/restore_handler.rs:100-188` (trait
  surface), `:934-981` (`RealRestoreBackend` struct with
  `shared_allocator: Option<Arc<Mutex<VmIndexAllocator>>>` and
  `nomad_handle: Option<Arc<NomadCHBackend>>`), `:1048-1244`
  (`impl RestoreBackend for RealRestoreBackend` — every method
  conditionally routes through `nomad_handle` or
  `shared_allocator`).
- **Symptom**: Post r9's R8-A3-5 `Arc<dyn RestoreBackend>` flip,
  the trait now exposes **7 methods** (was 6 at r5):
  `reserve_vm_index`, `release_vm_index`, `restore_alloc_dir`,
  `submit_restore_job`, `wait_for_livez`, `teardown_restore`,
  `register_restored`, `derive_agent_url` — and `RealRestoreBackend`
  holds two `NomadCHBackend`-typed fields (`shared_allocator`,
  `nomad_handle`) to satisfy them. Two of those seven methods'
  production paths are pure delegation:
  - `register_restored` (`restore_handler.rs:1214`) is a 25-line
    wrapper that derives `agent_url` and calls
    `NomadCHBackend::register_restored`.
  - `reserve_vm_index` / `release_vm_index` route through
    `shared_allocator.as_ref()` to `VmIndexAllocator::reserve`
    (lines 1050-1090); the local `reservations` field is "unit
    tests only" per its own doc.

  The `teardown_restore` rollback path at `:1159-1202` now also
  reaches into `nomad_handle.unregister_restored(...)` (the new
  R10-C1 fix in the working-tree diff). That is the **third**
  `RestoreBackend` method that delegates back to `NomadCHBackend`.
- **Why it matters**: R3-A2's old finding ("restore_handler is
  silently a second NomadCHBackend impl") has *strengthened*, not
  weakened. The trait is the right abstraction — the test stub
  (`StubRestoreBackend`, 815 LOC of test scaffolding) depends on
  it — but the production impl is a thin facade over
  `NomadCHBackend`. With 3 of 7 trait methods being
  one-line delegations to `nomad_handle.*`, `RealRestoreBackend`
  is effectively a feature-flag for "snapshot/restore on top of
  nomad-ch", and the indirection is paid every call. If/when
  Docker or K8s gets a restore impl the seam *might* pay; today
  it's a debt sink — same shape as the `Backend::register_restored`
  enum delegator that's `#[allow(dead_code)]` because the trait
  routes around it (`backend/mod.rs:520`).
- **Action**: One of:
  1. **Collapse** — drop `RestoreBackend` trait, make `restore_sandbox`
     take `Arc<NomadCHBackend>` directly. Lose the test stub. Pay
     once with a different test scaffolding (a `mockall`-style
     `MockNomadCHBackend`, or feature-gate `cfg(test)` test seams
     directly on the concrete type). The 7-method trait becomes 7
     methods on the concrete struct, and the `shared_allocator` /
     `nomad_handle` Options collapse into direct field access (no
     `as_ref().ok_or_else("missing handle")` paranoia at every call
     site).
  2. **Keep, but narrow** — combine R3-A2 with R3-A1: a single
     `SnapshotCapableBackend` trait (5+5 methods from
     `Backend::*` Err-returners + `RestoreBackend::*`), with the
     test stub at the trait level. Lose `RestoreBackend`-as-distinct,
     gain unified surface for the snapshot/restore lifecycle.

  Either is a structural net win. Doing neither for another cycle
  while adding `unregister_restored` (R10-C1's working-tree fix —
  not even on the trait!) entrenches the facade.

### [R10-A3] `Backend` enum still 5 Err-returning methods despite 3 rounds of API closures — pattern density is now diagnostic, not anecdotal (CRITICAL, architecture-r10)

- **Files**: `crates/sandbox/src/backend/mod.rs:368-562` — explicit
  count via grep `Self::Docker(_) | Self::K8s(_) => Err`:
  - `:375` `restore_from_sealed`
  - `:399` `lookup_source_vm_ops`
  - `:431` `teardown_source_for_snapshot`
  - `:533` `register_restored` (added at B19, `#[allow(dead_code)]`
    because the actual call site bypasses the enum)
  - `:555` `restore_from_pg_and_sealed`
- **Symptom**: r5-A1 first flagged this at 5 methods; r9-C1 listed
  it as "stalled another cycle." Count after r9's closures
  (R5-API1, R5-API2, R8-A3-5, B19): **still 5**. Pattern: the
  `Backend` enum was the right abstraction for create/stop/exec/file
  ops where all three variants implement them; for the
  snapshot/restore lifecycle it is a uniform-error generator.
  R10-A2's expansion of `RestoreBackend` to 7 methods compounds
  this: the controller now has TWO traits (one enum-based, one dyn)
  carrying the snapshot/restore concern.
- **Why it matters**: 5 unimplemented variants × 2 backends
  × 7 LOC each = 70 LOC of `Err("backend {:?} doesn't support…")`
  shaped exactly the same. That code is dead-on-arrival for Docker
  and K8s — neither will *ever* implement v1 restore (the
  agent_url derivation isn't deterministic for those backends, as
  the doc-comments at `:362-367` admit). The enum is forcing the
  trait surface to be a least-common-denominator of three backends
  that don't share the snapshot/restore concept. R3-A1's proposed
  split — `Backend` (create/stop/exec/file) + `SnapshotCapableBackend`
  (the 5 + the 7 above) — is the structural fix. The diagnostic
  signal is that r9 closed 3 API items without touching the count;
  the 5 methods are not load-bearing for any caller.
- **Action**: Land R3-A1 / R5-A1 (`SnapshotCapableBackend` trait
  split) as the *single* structural fix that closes R10-A2 +
  R10-A3 + `#[allow(dead_code)]` on `Backend::register_restored`
  + the asymmetric `NomadCh(Arc<…>)` wrap + the `nomad_ch_handle()`
  / `vm_index_allocator()` escape hatches (B18/B19 wiring becomes
  direct trait calls, not Arc-handed-out-the-side-door). Proposal:

  ```rust
  pub enum Backend { Docker(_), K8s(_), NomadCh(Arc<NomadCH>) }
  pub trait SnapshotCapableBackend: Send + Sync {
      // From Backend (5 Err-arms today):
      async fn lookup_source_vm_ops(...) -> Result<Handle, String>;
      async fn teardown_source_for_snapshot(...) -> Result<(), String>;
      async fn restore_from_sealed(...) -> Result<SandboxAuth, String>;
      async fn restore_from_pg_and_sealed(...) -> Result<SandboxAuth, String>;
      fn register_restored(...) -> Result<(), String>;
      // From RestoreBackend (R10-A2):
      fn reserve_vm_index(...);
      fn release_vm_index(...);
      // … etc
  }
  impl SnapshotCapableBackend for NomadCHBackend { ... }   // only impl in v1
  ```

  Controller holds `Option<Arc<dyn SnapshotCapableBackend>>` set
  *iff* `config.snapshot_enabled && cfg.backend == "nomad-ch"`. The
  rest of `Backend` shrinks to ~10 methods that genuinely vary
  across three backends. `RestoreBackend` collapses into the new
  trait. R4-A2's `LeasedVmSlot` design (concurrency r10) becomes
  expressible as a method-level RAII on this trait without the
  `nomad_handle: Option<Arc<NomadCHBackend>>` escape hatch.

### [R10-A4] `nomad_ch.rs` is 4923 LOC with a 459-LOC `create()` method — splitting overdue (IMPORTANT, architecture-r10)

- **Files**: `crates/sandbox/src/backend/nomad_ch.rs` (4923 LOC);
  the single `impl NomadCHBackend` block spans 354-1868 (1514 LOC),
  with `create()` at 466-924 = 459 LOC, `stop_inner()` at 986-1273 =
  287 LOC, `restore_from_pg_and_sealed` at 1573-1657 = 84 LOC,
  jobspec builder `build_nomad_job_json` at 2226-2362 = 137 LOC,
  HTTP plumbing `http_get_unsigned` / `http_delete_unsigned` /
  `http_post_json_unsigned` / `http_signed_async` /
  `signed_blocking_call` / `wait_for_agent_livez` /
  `wait_for_agent_silent` at 2721-3197 = 477 LOC.
- **Symptom**: 27 `pub`/`pub(crate)` methods on `NomadCHBackend`,
  96 fn-decls total in the file. The `create()` function alone is
  larger than 12 of the 20 sandbox-crate source files (and larger
  than the entire `crates/sandbox-agent/src/lib.rs` at 195 LOC). The
  module documents two distinct concerns in one file:
  1. **`NomadCHBackend` controller-side API** — methods, state map,
     vm_index allocator, the `restore_*` / `register_*` /
     `unregister_*` family added in the past 3 cycles.
  2. **Job submission + HTTP plumbing** — `submit_nomad_job`,
     `stop_nomad_job`, `wait_for_alloc_running`, `wait_for_job_gone`,
     `wait_for_agent_livez`, `wait_for_agent_silent`,
     `http_signed_async`, etc., plus the 137-LOC
     `build_nomad_job_json`.
- **Why it matters**: Every new restore-path feature lands at the
  bottom of this file. The R10-C1 working-tree fix (`unregister_restored`
  + 60 LOC of regression test) lives at 1706 ↔ 4848 — fine in
  isolation, but the file is now structurally indivisible by humans.
  More critically, `create()`'s 459 LOC contains a per-user
  creating-gate, vm_index alloc, host_dir setup with `FICLONE`
  fallback, job submission, alloc-running wait, livez wait, state
  map insert, sealing — i.e. it duplicates ~60% of the restore flow
  in `restore_handler::do_restore_inner`. R5's "the restore handler
  should be the orchestrator" cohesion is broken by `create()`'s
  size; structural finding D1 from concurrency r10's appendix and
  this both point at the same file.
- **Action**: Split nomad_ch.rs into a module:

  ```
  crates/sandbox/src/backend/nomad_ch/
  ├── mod.rs              (struct + new + probe + is_healthy, ~250 LOC)
  ├── create.rs           (the 459-LOC create + ReleaseCreating +
  │                       CreateGuard RAII; ~700 LOC)
  ├── stop.rs             (stop / stop_preserving_state / stop_inner,
  │                       ~400 LOC; lands StopDisposition enum from R3-A4)
  ├── restore.rs          (restore_from_sealed,
  │                       restore_from_pg_and_sealed, register_restored,
  │                       unregister_restored; ~400 LOC)
  ├── exec.rs             (exec / read_file / write_file / delete_file /
  │                       file_tree / session_auth; ~600 LOC)
  ├── jobspec.rs          (build_nomad_job_json + cpus_boot +
  │                       user_home_image_path / workspace_image_path /
  │                       create_ext4_image_if_missing; ~250 LOC)
  ├── http.rs             (http_get_unsigned, http_post_json_unsigned,
  │                       http_delete_unsigned, http_signed_async,
  │                       signed_blocking_call, send_ureq, AgentResponse;
  │                       ~400 LOC)
  ├── wait.rs             (wait_for_alloc_running, wait_for_job_gone,
  │                       wait_for_agent_livez, wait_for_agent_silent;
  │                       ~500 LOC)
  └── allocator.rs        (VmIndexAllocator + tests; ~200 LOC)
  ```

  Mechanical, semantics-preserving. Each child module compiles
  independently of the others (HTTP doesn't know about the state map;
  jobspec doesn't know about the runtime allocator). `mod.rs`
  re-exports public types so the crate-level call sites
  (`Backend::NomadCh(_)`, `RealRestoreBackend::with_nomad_handle`)
  don't change. The 4923-LOC file becomes ~250 LOC of facade + 7
  focused modules. Worth doing alongside R10-A3 so the split
  surfaces the eventual `SnapshotCapableBackend` impl boundary
  cleanly (it would live in `restore.rs`).

### [R10-A5] `error_envelope.rs` divergence is structural, NOT duplication — verdict: keep duplicated (MINOR, architecture-r10)

- **Files**: `crates/sandbox/src/error_envelope.rs` (216 LOC),
  `crates/sandbox-agent/src/error_envelope.rs` (201 LOC).
- **Symptom**: r8-A4 flagged "duplicated across both crates."
  Diffing the two: sandbox-side carries `extra: Option<Value>` +
  `no_store: bool` + `with_extra()` + `no_store()` chainers (the
  agent has neither); agent-side has no kind-specific fields and
  no Cache-Control needs (no CSRF / cookie auth in the VM). The
  agent doc-comment at `:34-42` documents the call:

  > "We do not lift the envelope into zeroship-core yet because
  > (a) the agent's surface is smaller — no `no_store`
  > Cache-Control needed (no CSRF / cookie auth in the VM), no
  > extra fields needed at the time of writing — and (b) the
  > wire-shape contracts of the two crates evolve independently.
  > If a third caller appears, fold the two into zeroship-core
  > then."
- **Why it matters**: This is the right call. The two helpers
  share a wire shape (`{error, message}`) but not an API surface.
  Lifting both into `zeroship-core` today means either (a) one
  envelope with the union (`with_extra` / `no_store`) that the
  agent never uses — adds two never-touched fields to every agent
  error response, OR (b) two structs in `zeroship-core` which is
  the duplication moved to a different crate. The "third caller
  appears" trigger is sound.
- **Action**: **Close the carry-forward.** Mark the
  cross-crate-extraction question as resolved: duplication
  is intentional, doc-comment at `crates/sandbox-agent/src/error_envelope.rs:34-42`
  records the rationale. Re-open only if a third in-tree caller
  appears (e.g. a `crates/sandbox-snapshot-store` / a
  `nomad-driver-ch` HTTP wrapper). The cluster-driver scaffold at
  `ee4a76c3` may produce that third caller — worth re-checking in
  r12.

### [R10-A6] `with_*` builder count still 7 + `new_fixture` still `pub` — R4-A1 / R4-A2 / R3-A4 all stalled 6+ cycles (IMPORTANT, architecture-r10)

- **Files**:
  - `crates/sandbox/src/lib.rs:218,277,317,325,351,362,373` — 7
    `pub fn with_*` builders.
  - `crates/sandbox/src/lib.rs:394` — `pub fn new_fixture`.
  - `crates/sandbox/src/config.rs:619` — `pub fn new_fixture`.
  - `crates/sandbox/src/backend/nomad_ch.rs:986` — `stop_inner(..,
    remove_host_dir: bool)`; the `bool` soup R3-A4 flagged.
- **Symptom**: Diff against r9: zero change in builder count
  (matches r9's 7 + `new_fixture`). The C1-FOLLOWUP commit
  (`0e71e5c4`) touched `db.rs` + `sweep.rs` only — `lib.rs` not
  modified. The R10-P2 commit (`6f314025`) touched
  `snapshot_store_gcs.rs` only. The R9-T4 commit (`419c154b`)
  touched `snapshot_aead.rs` only. **Three commits land since r9,
  zero structural movement.**
- **Why it matters**: r4-A2 LeasedVmSlot has been open 6 cycles.
  R3-A1/A4 + R4-A1 have been open 5+ cycles. r9 C1 explicitly noted
  "9th cycle since R4-A2 opened." r10 lands the *interim* R10-C1
  patch (`unregister_restored`) which the working-tree comment at
  `restore_handler.rs:1187-1188` openly admits is the "1-line interim
  ... until [LeasedVmSlot] lands." The structural cure was diagnosed
  in r4 and re-diagnosed in r5-r9; each cycle's fixer chooses the
  cheap patch over the structural fix, and each cycle adds more
  patches that *depend* on the not-yet-landed structural cure (the
  R10-C1 working-tree fix is now coupled to R4-A2's eventual landing
  shape — a `LeasedVmSlot`'s `Drop` would naturally close the
  state-map entry that `unregister_restored` now patches).
- **Action**: r10's verdict matches r9's: **stop adding interim
  patches and execute R4-A2.** Concurrency-r10 cleared the design;
  R10-C1's working-tree `unregister_restored` proves the race exists
  and is patchable; the right shape is the RAII guard. Coupling
  R10-A6 to R10-A3 (do both in one PR): the RAII guard's `Drop`
  needs the trait surface, and the trait split needs to expose the
  guard's lifecycle (acquire on `register_restored`, drop on
  `unregister_restored`). Estimated diff size: ~400 LOC across
  `restore_handler.rs` + `nomad_ch.rs` + `lib.rs`, with maybe 200
  LOC of test edits. Comparable to one of r9's fixer batches.

### [R10-A7] `ControllerIdleSnapshotter` and `admin_handlers::snapshot_sandbox` orchestrate the same 70-LOC sequence — T9 carry-forward holds; T10's 4-field reach unchanged (IMPORTANT, architecture-r10)

- **Files**: `crates/sandbox/src/sweep.rs:330-405` (`snapshot_one`),
  `crates/sandbox/src/admin_handlers.rs:1250-1325`
  (`snapshot_sandbox` admin handler).
- **Symptom**: Side-by-side diff: both call (in order)
  `state.backend.lookup_source_vm_ops`, build `ResolvedSourceVmOps`,
  compute `snap_stage_dir`, call `snapshot_handler::snapshot_sandbox`,
  then post-success call `state.backend.teardown_source_for_snapshot`.
  Sweep's variant adds idle-batch concurrency control and detached
  task framing; admin's variant adds error-envelope mapping and a
  detached teardown spawn. The 5 orchestration steps are identical;
  the wrapping differs. The `ControllerIdleSnapshotter` struct at
  `sweep.rs:314` holds `Arc<AppState>` and reaches into 4 fields
  (`snapshot_store`, `ch_remote`, `database`, `backend`,
  `config.snapshot_l1_root`) — same 4 the admin handler reaches.
- **Why it matters**: The "5 orchestration steps" are the *snapshot
  trio's drive contract* — there is no second valid sequence. Today
  two callers each re-derive that contract from the trio. A
  third caller (e.g., the deferred `POST /admin/v1/sandboxes/{id}/snapshot-batch`
  or any future "snapshot-on-shutdown" hook) will be tempted to
  re-implement it a third time. The `ResolvedSourceVmOps` adapter is
  literally duplicated between `sweep.rs:412-429` and
  `admin_handlers.rs` (the sweep file admits this at line 408 —
  "the admin-handler copy is also private; duplicating it avoids a
  pub-export churn"). Compounded by R10-A2's R10-C1 working-tree fix,
  which also depends on the snapshot orchestration's terminal state
  contract to be right.
- **Action**: Extract a `snapshot_orchestrator::run(state, sandbox_id)
  -> Result<Outcome, Error>` (in either `snapshot_handler.rs` —
  which already owns the snapshot CAS state machine — or a new
  `snapshot_orchestrator.rs`). Body is the 70-LOC sequence. Sweep
  calls it; admin calls it; the future batch handler calls it. The
  `ResolvedSourceVmOps` adapter lifts into the orchestrator with it.
  `ControllerIdleSnapshotter` collapses to ~15 LOC (just the
  `IdleSnapshotter` trait bridge) and stops holding 4 distinct
  fields of `AppState`. T9 + T10 close together. Net diff: ~150
  LOC moved + ~60 LOC removed (two duplicated `ResolvedSourceVmOps`).

## Carry-forward (still open from earlier rounds)

- **[R4-A2 / R5-A2]** LeasedVmSlot RAII guard — STILL not landed,
  6th+ cycle. R10-C1 working-tree fix (`unregister_restored`) is an
  explicit "1-line interim" admitted in
  `restore_handler.rs:1187-1188`. Coupled to R10-A3.
- **[R3-A1 / R5-A1]** Backend enum 5-Err-returner split — confirmed
  still 5 at r10 (`backend/mod.rs:375,399,431,533,555`).
  Promoted to R10-A3 (CRITICAL) given pattern density + R10-A2
  coupling.
- **[R3-A2]** restore_handler silently a second NomadCHBackend impl
  — strengthened to R10-A2 (CRITICAL). 7-method trait, 2 of 3
  `Arc<NomadCH>`-typed fields in `RealRestoreBackend`, 3 of 7
  methods one-line delegations.
- **[R3-A3]** wrapper bash → Rust sidecar — r9 recommendation
  unchanged; r10 confirms the wrapper has grown by B24/B24-FOLLOWUP
  (SANDBOX_AGENT_SANDBOX_ID injection at line 641). Recommend
  EXECUTE per r9 deferred §.
- **[R3-A4]** `StopDisposition` enum — still `stop_inner(.., bool)`
  at `nomad_ch.rs:986-989`. Cheap to land as part of R10-A4's
  nomad_ch split (lands in the new `stop.rs` child).
- **[R4-A1]** AppStateBuilder accreting — 7 `with_*` + `new_fixture`
  unchanged from r9. R10-A6 records the inertia.
- **[T9]** ControllerIdleSnapshotter duplicates admin orchestration —
  promoted to R10-A7 with file:line evidence.
- **[T10]** ControllerIdleSnapshotter `Arc<AppState>` 4-field reach
  — same evidence as T9, closes with the orchestrator extraction.
- **[r9 C3]** AEAD fail-OPEN on GCS path — `lib.rs:654-666` still
  logs `tracing::error!("plaintext on disk + GCS")` then constructs
  the store with `kek = None` (passthrough). No
  `assert_kek_required_when_gcs_enabled` boot gate. Worth re-raising
  in r11 — it's a security carry-forward but architecture lens
  notes that the current "log + proceed" shape inverts the design's
  fail-CLOSED invariant.

## Module size table

| File | LOC | Action |
|---|---:|---|
| `crates/sandbox/src/backend/nomad_ch.rs` | 4923 | **R10-A4** — split into `nomad_ch/{mod,create,stop,restore,exec,jobspec,http,wait,allocator}.rs`. The 459-LOC `create()` alone justifies it. |
| `crates/sandbox/src/db.rs` | 3003 | **R10-A1** — extract `db_recovery.rs` (sweep query + recovery CAS + `read_snapshot_row` lift from restore_handler + `recovery_target` lift from sweep). Drops db.rs to ~2700. |
| `crates/sandbox/src/restore_handler.rs` | 2367 | Half-mitigated by R10-A2 (collapse `RestoreBackend` trait → `SnapshotCapableBackend` on `NomadCHBackend`). `StubRestoreBackend` (815 LOC test scaffolding) becomes a `MockNomadCHBackend` in test code. Net file ~1100 LOC after R10-A2 lands. |
| `crates/sandbox/src/lib.rs` | 2267 | Awaiting R4-A1 builder consolidation. Each round adds 0-100 LOC; r10 unchanged. AppStateBuilder pattern (typed required-fields, optional `with_*` for actual options) drops to ~1700. |
| `crates/sandbox-agent/src/handlers.rs` | 2010 | Out of architecture scope this round; flagged for r11. |
| `crates/sandbox/src/admin_handlers.rs` | 1781 | Reduces ~150 LOC when R10-A7 lands (orchestration extraction). |
| `crates/sandbox/src/snapshot_store_gcs.rs` | 1640 | Per r10 perf review, healthy as-is. |
| `crates/sandbox-agent/src/sig.rs` | 1590 | Out of architecture scope. |
| `crates/sandbox/src/handlers.rs` | 1284 | No new findings r10. |
| `crates/sandbox-agent/src/proxy.rs` | 1331 | Out of architecture scope. |
| `crates/sandbox/src/snapshot_aead.rs` | 1196 | r9-T4 closure (`419c154b`) added 237 LOC of negative tests — healthy growth. |
| `crates/sandbox/src/persist.rs` | 1133 | Stable across r5-r10. |
| `crates/sandbox/src/config.rs` | 1051 | `pub fn new_fixture()` at `:619` still pub (R4-A1 carry-forward). |

Aggregate: 7 source files >1500 LOC, 3 of them >2000 LOC, 1 >4000
LOC. r10's 2 splits (R10-A1 + R10-A4) drop 1 file out of the
>4000 bucket and 1 out of the >2500 bucket without changing
semantics.

---

## What's structurally new vs. r9

| Item | r9 state | r10 state | Δ |
|---|---|---|---|
| `RestoreBackend` trait methods | 6 (post-R7-S2 derive_agent_url forced) | **7** (post-B19 register_restored) | +1 |
| `RealRestoreBackend` Arc-NomadCH fields | 1 (`shared_allocator`) | **2** (+`nomad_handle`) | +1 |
| `Backend` enum Err-returners | 5 | **5** (no change) | 0 |
| `NomadCHBackend` pub methods | 25 | **27** (+`unregister_restored`, +`contains_for_test`) | +2 |
| db.rs LOC | ~2850 | **3003** (+~150 for `claim_orphan_…`) | +153 |
| `with_*` builders | 7 | **7** (no change) | 0 |
| `pub fn new_fixture` | 2 (lib.rs + config.rs) | **2** (no change) | 0 |
| `restore_handler.rs` LOC | ~2270 | **2367** (+R10-C1 inline doc + test) | +97 |
| `nomad_ch.rs` LOC | ~4820 | **4923** (+`unregister_restored` + `contains_for_test` + R10-C1 test) | +103 |

Every line of growth this cycle reinforces an existing
architectural finding. None of it closes one.

## Recommended order of attack (1 PR per item)

1. **R10-A1** db.rs split (~250 LOC moved, mechanical, zero risk).
2. **R10-A7** snapshot orchestrator extraction (~150 LOC moved,
   closes T9 + T10).
3. **R10-A4** nomad_ch.rs module split (~5000 LOC moved across 8
   child modules, mechanical, lands `StopDisposition` enum from
   R3-A4 in `stop.rs`).
4. **R10-A3 + R10-A2 + R4-A2 (LeasedVmSlot)** together — the
   structural fix. `SnapshotCapableBackend` trait + collapse
   `RestoreBackend` into it + `LeasedVmSlot` RAII becomes a
   method-level concept on the trait. Single coherent diff,
   probably ~600 LOC. Closes 4 carry-forwards.
5. **R4-A1 AppStateBuilder** — pulls in the typed-required-fields
   pattern; closes `pub fn new_fixture` on both `AppState` and
   `SandboxConfig` (R10-A6).

Total: 5 PRs, mostly mechanical except #4. Closes 6
carry-forwards + 4 r10-new findings. Net file-count change: +~10
modules, but each existing 2000+ LOC file drops below 1000.
