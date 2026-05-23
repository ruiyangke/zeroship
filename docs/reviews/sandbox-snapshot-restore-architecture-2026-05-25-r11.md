# Sandbox/snapshot-restore — architecture r11 review

Date: 2026-05-25 (UTC)
HEAD at audit: `4f441a20`
Round 11 of N (architecture lens).
Prior round: `sandbox-snapshot-restore-architecture-2026-05-25-r10.md`.

Scope read: `crates/sandbox/**`, `crates/sandbox-agent/**` only.

Working-tree note: `crates/sandbox/src/backend/nomad_ch.rs` is
**mid-edit** for T-7 (nomad-driver-ch controller-integration; +241/-89
LOC vs HEAD). The added code is a `TaskDriverMode { RawExec, ChPlugin }`
enum + `task_driver_mode_from_env` + the `build_nomad_job_json_with(...,
mode)` lower-level builder. Findings below pin the committed shape at
`4f441a20`; the T-7 delta is called out where relevant to R11-A2.

## Summary

5 NEW findings (1 critical, 3 important, 1 minor). The 4 commits
landed since r10 (`228569d3` err_safe sanitization, `2c10f63a` pg-pw
uid check, `e4e5db60` AEAD key uid check, `c8000537` capability doc)
are pure bug-fixers + doc updates — **zero structural movement**, +342
insertions, **none** closing a carry-forward at the structural level.
Counter-evidence: the 4 commits collectively widened the
"copy-pasted root-owned secret-file loader" surface from 3 to 4 sites
(R11-A1). Every flagship carry-forward from r10 holds:
**R4-A2 LeasedVmSlot** (7+ cycles), **R3-A1 / R5-A1 / R10-A3** Backend
enum split, **R10-A2** RestoreBackend facade, **R10-A4** nomad_ch.rs
4923-LOC. The T-7 working-tree feature flag (`TaskDriverMode`) lands
~150 LOC of "two parallel jobspec assemblers in one function" that
R11-A2 below predicts will need a post-T-8 split.

## Module size table (committed HEAD; not working tree)

| File | LOC | Δ vs r10 | Action |
|---|---:|---:|---|
| `crates/sandbox/src/backend/nomad_ch.rs` | 5072\* | +149\* | **R10-A4** carry-forward + R11-A2 post-T-8 sketch. \*working-tree only; HEAD = 4923 (unchanged from r10). |
| `crates/sandbox/src/db.rs` | 3108 | +105 | **R10-A1** carry-forward; +69 LOC of R9-S4c test scaffolding at `:2986-3055` (good growth, but reinforces R11-A1 4-site duplication). |
| `crates/sandbox/src/restore_handler.rs` | 2367 | 0 | R10-A2 carry-forward — half-mitigated only by the trait collapse described in r10. |
| `crates/sandbox/src/lib.rs` | 2267 | 0 | R4-A1 carry-forward (`with_*` × 7 + `new_fixture`). `boot_init_sandbox_id` (R8-API1) lives in **sandbox-agent**, not here — does not affect AppState builder count (see "AppState builder accretion" below). |
| `crates/sandbox-agent/src/handlers.rs` | 2224 | +214 | R9-T7 test additions (`init_sandbox_id_from_env` direct tests); healthy growth. Out of architecture scope. |
| `crates/sandbox/src/admin_handlers.rs` | 1781 | 0 | T9/T10/R10-A7 carry-forward. |
| `crates/sandbox/src/snapshot_store_gcs.rs` | 1640 | 0 | Per r11 perf review, healthy. |
| `crates/sandbox-agent/src/sig.rs` | 1590 | 0 | Out of arch scope. |
| `crates/sandbox/src/backend/k8s.rs` | 1580 | 0 | Out of arch scope (no snapshot-path interaction). |
| `crates/sandbox/src/handlers.rs` | 1371 | +87 | R10-Q1 fix added 3 err_safe sites + tests. Healthy. |
| `crates/sandbox-agent/src/proxy.rs` | 1331 | 0 | Out of arch scope. |
| `crates/sandbox/src/snapshot_aead.rs` | 1277 | +81 | R9-S4 R11-A1-sibling tests; healthy growth. |
| `crates/sandbox/src/persist.rs` | 1216 | +83 | R9-S4b tests; healthy growth, but R11-A1 candidate sibling. |
| `crates/sandbox/src/config.rs` | 1051 | 0 | `pub fn new_fixture` carry-forward. |

**No file crossed the 4000-LOC boundary newly this cycle**. `nomad_ch.rs`
remains the only >4000 — at 4923 LOC committed (and 5072 LOC in working
tree, +149 from T-7's ChPlugin flag + parallel jobspec block). R10-A4
prediction holds: every new feature lands at the bottom.

## Findings (NEW since r10)

### [R11-A1] 4-site root-owned-secret-file loader is now the dominant copy-paste anti-pattern in the controller crate — extract a `secret_io::read_root_owned_secret_file` helper (CRITICAL, architecture-r11)

- **Files** (exact landing spots of the byte-for-byte-identical 4-step
  pattern: stat → mode == 0o400 → uid == 0 → read+length-check):
  - `crates/sandbox/src/snapshot_aead.rs:185-217` (`RootKek::from_path`)
  - `crates/sandbox/src/persist.rs:333-369` (`AeadKey::from_path`)
  - `crates/sandbox/src/db.rs:824-851` (`enforce_password_file_mode`)
  - `crates/sandbox/src/lib.rs:924-957` (`load_admin_token`) — **missing
    the uid==0 arm; tracked as R9-S4d in deferred:566, NOT YET CLOSED**.
- **Symptom**: The R9-S4 series (cca1e74d → e4e5db60 → 2c10f63a)
  landed the same ~20-LOC arm three times. Each commit explicitly
  cites the previous as the model:
  - `persist.rs:325-332` documents "Mirrors `RootKek::from_path`".
  - `db.rs:810-823` documents "Mirrors `persist::AeadKey::from_path`".
  - The fourth site (`lib.rs::load_admin_token`) is acknowledged in
    `docs/reviews/sandbox-snapshot-restore-deferred.md:564` as
    "Fourth sibling identified" — same vulnerability shape, same
    one-line fix, deferred so the closing commit "stays focused on
    the pg-password loader."

  The pattern is now an *invariant* of the controller: any boot-time
  secret-bearing file MUST be (mode 0o400, uid 0, expected length, on
  Unix). All 4 sites also share:
  - Error-string idiom: `"{ENV_NAME}={path:?}: ..."`.
  - Two `#[cfg(unix)]` arms (the non-Unix arm is silently skipped).
  - The same "non-root attacker can pre-create chmod-400" threat model
    (cited verbatim in 3 of 4 doc-comments).

  R11-Q2 from r11 code-quality flagged the same surface from a
  code-quality lens (deferred:774). This finding promotes it to a
  structural concern because:
  1. A *fifth* secret-file caller is already plausible — e.g., the
     forthcoming K8s service-account token loader, a Stripe webhook
     signing-key loader, or any operator-private bearer added during
     the cluster-driver scaffold work. Each one will copy the pattern
     by precedent.
  2. The 4 sites have *already diverged* on minor points (e.g.,
     `snapshot_aead.rs` uses `std::fs::read` then `copy_from_slice`;
     `persist.rs` uses `File::open` + `read_exact` for the same 32-byte
     buffer; `db.rs` returns `Result<()>` with `DatabaseError`; `lib.rs`
     returns `Result<Option<String>>` and is the one missing R9-S4d).
     Future fixers will fix *one* and not the others (R9-S4d already
     proves this).
- **Why it matters**: This is the same architectural class as R3-A1's
  "Backend enum has 5 Err-returning methods" — a pattern duplicated
  across the codebase that should be a single function. Difference:
  the secret-file loader is a true *helper* (no trait, no lifetime,
  no callback) — the extraction is mechanical, low-risk, and closes
  R9-S4d in the same diff. It also pre-empts the divergence drift
  (point 2 above) that already started.
- **Action**: Land alongside R9-S4d:
  1. Create `crates/sandbox/src/secret_io.rs` (new module, ~80 LOC
     including 4 tests).
  2. Expose `pub(crate) fn read_root_owned_secret_file(env_name: &str,
     path: &Path, expected_len: Option<usize>) -> Result<Vec<u8>, String>`.
     The `env_name` parameter feeds the error-string idiom; `expected_len
     == Some(n)` enforces the length-check (the AEAD/KEK case);
     `expected_len == None` is the variable-length case (admin token).
  3. Convert all 4 sites: each becomes a 1-line call + a wrapper that
     converts `Vec<u8>` into its specific in-memory form
     (`[u8; 32]` / `String` / etc.) + the crate-specific error type.

  See "Architectural sketch — R11-Q2 helper placement" below for the
  recommended module location and naming.

  Cost: ~80 LOC added (new module + tests), ~120 LOC removed across the
  4 sites, net **~-40 LOC** + 4 sites converge on a single invariant.
  Closes R9-S4d + R11-Q2 + this finding in one diff.

### [R11-A2] T-7's `TaskDriverMode { RawExec, ChPlugin }` is the in-tree feature-flag for the bash-wrapper → Go-driver migration — post-T-8 module shape needs planning NOW, not after the flag flips (IMPORTANT, architecture-r11)

- **Files** (working-tree, NOT committed):
  - `crates/sandbox/src/backend/nomad_ch.rs:2213-2244` (`TaskDriverMode`
    + `task_driver_mode_from_env`).
  - `crates/sandbox/src/backend/nomad_ch.rs:2399-2464` (the `match mode
    { RawExec => ..., ChPlugin => ... }` block inside
    `build_nomad_job_json_with` — 65 LOC of two parallel jobspec
    assemblers in one function).
  - `crates/sandbox/src/backend/nomad_ch.rs:2378-2380` (the
    `ZSBX_RESTORE_FROM` env entry — kept under BOTH modes per the
    debugging-redundancy comment at `:2262-2266`).
  - HEAD wrapper-mention count (`grep -c wrapper`): 75; references the
    bash wrapper directly at `:2406` (production code path) and at
    `:3789,3874,3877,3954,4000,4016` (test fixtures pinning specific
    line numbers in `crates/sandbox/scripts/nomad-vm-wrapper.sh`).
- **Symptom**: T-7 adds the controller-side switch; T-8 will validate
  the new `Driver: "ch"` path on cluster and (per the T-7 doc-comment
  at `:2236-2238`) at some point the bash wrapper goes away. When that
  happens, `nomad_ch.rs` carries:
  1. **Dead code**: the entire `RawExec` arm of `build_nomad_job_json_with`
     at `:2402-2408` + the `ZSBX_*` env-block construction at `:2334-2374`
     (the Env block is largely redundant under ChPlugin per the comment
     at `:2326-2328` — once the wrapper is gone, the Env can drop too,
     but multiple test fixtures pin its shape).
  2. **Dead tests**: the `nomad-vm-wrapper.sh:153,222,364,641`
     line-pinning regression tests at `:3954-4016` become unreachable
     contracts (the wrapper is gone, the validators it tested are
     replaced by the Go driver's HCL decode).
  3. **Hot duplication**: every per-VM input now travels via TWO
     channels — the legacy `Env: { ZSBX_* }` AND the typed
     `Config: { vm_index, sandbox_id, ... }`. The doc-comment admits
     it's "largely redundant" — which means every future per-VM input
     gets added in two places.
- **Why it matters**: When T-8 lands, the cleanup is **mechanically
  pure deletion** but the file is already 5072 LOC and the deletion
  spans 6 zones (function bodies, test fixtures, ENV constants,
  validator-line-pin tests, wrapper_path field references, the helper
  `cpus_boot` interpretation). Splitting `nomad_ch.rs` BEFORE T-8 lands
  isolates the bash-wrapper concern into one child module that can be
  deleted as a unit, instead of carving out 6 zones from a 5000+ LOC
  file. The R10-A4 split sketch already isolates `jobspec.rs` — that's
  the single file that would absorb the T-8 deletion cleanly. **The
  ordering matters**: do R10-A4 first, do T-8 second, and T-8 becomes
  `rm crates/sandbox/src/backend/nomad_ch/jobspec_rawexec.rs` + delete
  3 callers. Do them in reverse order and T-8 is a 5072-LOC surgical
  edit.
- **Action**:
  1. **Land R10-A4 (nomad_ch.rs module split) ahead of T-8** — already
     recommended in r10 as the top mechanical fix. Add to its module
     layout a `jobspec_rawexec.rs` (the bash-wrapper-specific assembler)
     vs `jobspec_chplugin.rs` (the Go-driver-specific assembler), with
     `jobspec.rs` as the dispatcher carrying `TaskDriverMode`:

     ```
     backend/nomad_ch/jobspec/
     ├── mod.rs              (TaskDriverMode + task_driver_mode_from_env
     │                       + build_nomad_job_json dispatcher;
     │                       ~50 LOC)
     ├── rawexec.rs          (the existing ZSBX_* env block + raw_exec
     │                       Config + the wrapper_path reference;
     │                       ~150 LOC. Whole file deletes at T-8.)
     ├── chplugin.rs         (the typed `ch` Config block; ~100 LOC.
     │                       Becomes the only assembler at T-8.)
     └── common.rs           (cpus_boot, NOMAD_CPU_MHZ_ADVISORY,
                              Resources block, KillTimeout — survives
                              both paths; ~60 LOC.)
     ```

  2. **Post-T-8 delta** (predicted shape of `nomad_ch.rs` AFTER the bash
     wrapper is removed):
     - `jobspec_rawexec.rs` → deleted entirely (~150 LOC).
     - `jobspec/mod.rs` → drops `TaskDriverMode` enum + the dispatch
       match (~30 LOC); `chplugin.rs` becomes the only call.
     - The `nomad-vm-wrapper.sh` shell script → deleted from
       `crates/sandbox/scripts/`.
     - Test fixtures at `nomad_ch.rs:3874-4016` → deleted or rewritten
       against the Go driver's TaskConfig schema (~120 LOC).
     - `cfg.nomad_ch.wrapper_path` field → removed from
       `SandboxConfig` (`config.rs`).
     - **Net delete**: ~400 LOC across nomad_ch.rs + a whole shell
       script (~700 LOC). The post-T-8 `nomad_ch.rs` (assuming R10-A4
       split happens first) is structurally cleaner: no two-channel
       per-VM input, no ZSBX_* env duplication, no
       `[!0-9a-zA-Z_]`-line-pinning regression tests.

  Doing R10-A4 ahead of T-8 is a one-time ~5000-LOC mechanical move
  that pre-positions T-8's cleanup. Doing T-8 first means doing R10-A4
  on a *smaller* file post-T-8 — superficially easier, but the working
  tree's 5072 LOC of half-migrated code is the file we have today.
  Best architectural lever: split the file at its existing seams now,
  delete one child at T-8.

### [R11-A3] AppState `with_*` builder count unchanged at 7 + `new_fixture` — R4-A1 has now stalled 8+ cycles; `boot_init_sandbox_id` (R8-API1) lives in sandbox-agent, not AppState (IMPORTANT, architecture-r11)

- **Files**:
  - `crates/sandbox/src/lib.rs:218,277,317,325,351,362,373` — 7 `pub fn
    with_*` builders (count via
    `grep -nE "pub fn with_|pub fn new_fixture"`).
  - `crates/sandbox/src/lib.rs:394` — `pub fn new_fixture` (still pub,
    still in production crate code rather than `#[cfg(test)]`).
  - `crates/sandbox/src/config.rs:619` — `pub fn new_fixture` (the
    sibling on `SandboxConfig`, also still pub).
  - `crates/sandbox-agent/src/lib.rs:99` — `pub fn boot_init_sandbox_id`
    (R8-API1 closed at `10bddc20`; for the **agent**, not AppState).
- **Symptom**: r10 (and r9, r8, r7, r6, r5, r4) flagged the
  `AppStateBuilder` accretion. The r10 baseline noted "7 + new_fixture"
  and predicted "zero structural movement next cycle if the trio of
  flagship items keeps deferring." Diff against r10 at HEAD:
  - `lib.rs` builder count: 7 → 7 (unchanged).
  - `lib.rs::new_fixture` visibility: `pub` → `pub` (unchanged).
  - `config.rs::new_fixture` visibility: `pub` → `pub` (unchanged).
  - The 4 commits since r10 touch `db.rs` + `handlers.rs` +
    `persist.rs` + `version.rs` — none touched `lib.rs`.

  One *clarification*: R8-API1's `boot_init_sandbox_id` wrapper closed
  10bddc20 lives in **`crates/sandbox-agent/src/lib.rs:99`**, not in
  the controller `crates/sandbox/src/lib.rs`'s `AppState`. So r10's
  question "after R8-API1's `boot_init_sandbox_id` wrapper, has
  AppState's builder gotten cleaner or messier?" → it is **unchanged**.
  R8-API1 was orthogonal to AppState: a sandbox-agent agent-side
  bootstrap wrapper, not a controller-side state-builder change.
- **Why it matters**: 8 cycles is a long time for a structural finding
  to sit. The fix is mechanical (typed required-fields struct +
  `Builder` pattern for actuals). The blocker is that the test surface
  threads through `AppState::new_fixture` + `SandboxConfig::new_fixture`,
  so a typed builder needs a `#[cfg(test)]` constructor with the same
  ergonomics — solvable in the same diff but the diff isn't getting
  written.
- **Action**: Land R4-A1 as one PR. Sketch:
  ```rust
  pub struct AppStateBuilder {
      config: SandboxConfig,
      backend: Backend,
      // Optional wirings (the seven with_*):
      admin_token: Option<Zeroizing<String>>,
      persistence: Option<Arc<Persistence>>,
      database: Option<Arc<Database>>,
      snapshot_store: Option<Arc<dyn SnapshotStore>>,
      ch_remote: Option<Arc<dyn ChRemoteClient>>,
      restore_backend: Option<Arc<dyn RestoreBackend>>,
      // …
  }
  impl AppStateBuilder {
      pub fn new(config: SandboxConfig, backend: Backend) -> Self { ... }
      pub fn with_admin_token(mut self, t: Zeroizing<String>) -> Self {...}
      // … etc, but as plain field setters, not "construct + mutate"
      pub fn build(self) -> AppState { ... }
  }
  #[cfg(test)]
  impl AppStateBuilder {
      pub fn fixture(...) -> AppState { ... }   // replaces new_fixture
  }
  ```
  The two `new_fixture` pubs become `#[cfg(test)] pub`. Closes R4-A1
  + half of R10-A6 in one diff.

### [R11-A4] `claim_orphan_transient_for_recovery` (db.rs:2520) is still in db.rs — R10-A1's recommended `db_recovery.rs` extraction has not happened; the inline `read_snapshot_row` in restore_handler.rs:332 is still inline (IMPORTANT, architecture-r11)

- **Files**:
  - `crates/sandbox/src/db.rs:2520` — `claim_orphan_transient_for_recovery`
    (the CAS that closes C1-FOLLOWUP) is still in `db.rs`; only one
    caller in production (`sweep.rs:182`).
  - `crates/sandbox/src/db.rs:2407` (approx) —
    `transient_state_lease_expired_sandboxes`, the partner query.
  - `crates/sandbox/src/sweep.rs:108` — `recovery_target` (the §9.2
    transient-state → recovery-state lookup), still in `sweep.rs`.
  - `crates/sandbox/src/restore_handler.rs:332` — inline
    `client.query_opt(SELECT snapshot_artifact_path, …)`, still living
    outside `db.rs`.
- **Symptom**: Verbatim re-statement of R10-A1 with one cycle of
  evidence:
  - r10 → r11: `db.rs` grew by +105 LOC (test additions for R9-S4c).
    None of that growth is the recovery layer — but db.rs is now 3108
    LOC. The recovery-CAS continues to share a file with sandbox CRUD,
    host registry, migration runner, share-token logic, share-event
    insert path.
  - r10's recommended extraction (`db_recovery.rs` carrying the sweep
    query + the recovery CAS + the inline `read_snapshot_row` lift from
    restore_handler + the `recovery_target` lift from sweep) is exactly
    250 LOC of code movement, zero semantic change. It didn't happen
    this cycle.
- **Why it matters**: The §6.1 / §9.2 recovery layer is **structurally
  the same shape as the snapshot orchestrator** (T9/T10/R10-A7) — a
  workflow with one production caller (`sweep.rs`), pg-gated tests in
  the integration crate, and an open architectural seam against
  `db.rs`. Both want to be lifted out of their current homes; both have
  been deferred multiple cycles. The longer this sits, the more likely
  a third recovery item (a §9.2 fourth transient state — `SealRotating`
  is a candidate per the AEAD epoch design) accretes the same anti-
  pattern onto db.rs.
- **Action**: Same as R10-A1 — extract `crates/sandbox/src/db_recovery.rs`
  (or `crates/sandbox/src/recovery.rs`, broader name). Sketch:

  ```
  crates/sandbox/src/recovery.rs (new)
  ─ pub async fn transient_state_lease_expired_sandboxes(db, threshold)
  ─ pub async fn claim_orphan_transient_for_recovery(db, ...)
  ─ pub async fn read_snapshot_row(db, sandbox_id)   ← lifted from
                                                       restore_handler.rs:332
  ─ pub fn recovery_target(status)                   ← lifted from
                                                       sweep.rs:108
  ```

  All four functions are pg-aware but logic-light — they don't need
  the rest of db.rs's plumbing (pool open / role split / host
  identity). The `Database` reference passes in by `&Database`.

  Cost: ~280 LOC moved, +60 LOC of new module boilerplate, no
  semantic change. Drops db.rs to ~2830 LOC. **Pair with R11-A1's
  `secret_io.rs` for the "new module" tax**: both are extraction-only
  moves that close 2 carry-forwards + the R11-Q2 deferred. The
  `recovery.rs` file is the natural home for any future §9.2 work.

### [R11-A5] `ErrorEnvelope` cross-crate drift check: zero drift since r10; r10's "keep duplicated" verdict stands; T-8's nomad-driver-ch will NOT trigger the "third caller" condition (MINOR, architecture-r11)

- **Files**:
  - `crates/sandbox/src/error_envelope.rs` (216 LOC, last touched
    `469e22c8` for B18 — *not* a wire-shape change).
  - `crates/sandbox-agent/src/error_envelope.rs` (201 LOC, last touched
    `fc3e9972` for R8-A4 mass migration).
- **Symptom**: Direct `diff -u` of the two files (see analysis trail):
  the controller-side adds `extra: Option<Value>` + `no_store: bool`
  + chainers `with_extra()` + `no_store()`; the agent-side has neither.
  The agent's doc-comment at `:34-42` documents the cross-crate
  contract explicitly. **No structural changes since r10** in either
  file (per `git log --since="2026-05-22"`).
- **Why it matters**: R10-A5's verdict was "keep duplicated;
  re-open only if a third in-tree caller appears (e.g. a
  `crates/sandbox-snapshot-store` or a `nomad-driver-ch` HTTP wrapper)".
  Cross-checking the T-7 working-tree diff: `nomad-driver-ch` is a
  **separate Go crate** (`PluginName` at `nomad-driver-ch/ch/driver.go`)
  — Go, not Rust, so it cannot consume `ErrorEnvelope` from
  `zeroship-core` even if we lifted it. The controller-side talks to
  the Go driver via Nomad's task-driver gRPC (per `:2410-2464` typed
  `Config`), not via a shared Rust HTTP envelope. **The T-8 landing
  does not trigger the third-caller condition.**

  Forward look: the cluster-driver scaffold (referenced in r10 at
  `ee4a76c3` — not searched this cycle) remains the only plausible
  third Rust caller. Re-check r12 if that lands.
- **Action**: **Close R10-A5's question.** Re-mark the cross-crate
  envelope duplication as INTENTIONAL with the agent's doc-comment as
  the citation. Promote the resolution from r10's "verdict" status to
  a closed item in the carry-forward.

## Closed by recent commits

- **R10-Q1** raw error leaks at handlers.rs — closed at `228569d3`.
- **R9-S4b** AEAD key uid check — closed at `e4e5db60`.
- **R9-S4c** pg-password uid check — closed at `2c10f63a`.
- **R7-API2** capability-list-is-diagnostic — closed at `c8000537` (doc
  change only; no architectural movement).

None of these closed an architecture-level finding. The R9-S4 trio
*strengthens* R11-A1 (4-site duplication grew from 3 to 3-with-1-pending).

## Carry-forward (still open from earlier rounds)

- **[R4-A2 / R5-A2]** LeasedVmSlot RAII guard — STILL not landed,
  **8th cycle**. The R10-C1 working-tree `unregister_restored`
  interim fix at `restore_handler.rs:1187-1188` is now in committed
  state at `be246395`. R11-A1's recommended ordering (do R10-A4 split
  first → then R10-A3 trait split → then LeasedVmSlot lives on the
  trait as a method-level RAII) holds.
- **[R3-A1 / R5-A1 / R10-A3]** `Backend` enum 5-Err-returner split —
  count at HEAD = **5** (no change). Pattern density is now an
  invariant the codebase has internalized. Promoted to CRITICAL in
  r10; remains CRITICAL.
- **[R3-A2 / R10-A2]** `RestoreBackend` trait is a facade over
  `NomadCHBackend` — 7 methods, 3 of them one-line delegations, two
  `Arc<NomadCH>`-typed fields hidden behind the trait. No change at
  HEAD.
- **[R3-A3]** wrapper bash → Rust sidecar — *deferred indefinitely*
  per the T-7/T-8 path (the wrapper is going away entirely, replaced
  by the Go driver). r9's "rewrite the wrapper in Rust" sub-finding is
  subsumed by R11-A2's "delete the wrapper at T-8" — the structural
  cure is to delete, not to rewrite. **Status change: close R3-A3, open
  R11-A2 instead.**
- **[R3-A4]** `StopDisposition` enum — still `stop_inner(.., bool)` at
  `nomad_ch.rs:986-989`. Lands cheaply as part of R10-A4's split.
- **[R4-A1 / R10-A6]** AppState builder accretion — 8th cycle. Same
  shape as r10 (R11-A3 above).
- **[R10-A4]** nomad_ch.rs 4923/5072 LOC — un-split. R11-A2 above
  argues for splitting *before* T-8 lands, not after.
- **[R10-A1 / R11-A4]** db.rs at 3108 LOC carrying the recovery CAS —
  un-extracted.
- **[T9 / T10 / R10-A7]** ControllerIdleSnapshotter duplicates
  admin_handlers' 70-LOC orchestration — unchanged at `sweep.rs:325-405`
  ↔ `admin_handlers.rs:1250-1325`.
- **[r9 C3]** AEAD fail-OPEN on GCS path — still at `lib.rs:654-666`,
  no boot-gate. Architecture lens: this is r11's strongest *security*
  carry-forward through arch's lens because the "log + proceed" shape
  inverts the design's fail-CLOSED invariant. (Track in security-r11.)

---

## Architectural sketch — R11-Q2 / R11-A1 helper placement

**Recommendation: NEW MODULE — `crates/sandbox/src/secret_io.rs`**.

Rationale for a new module over extending `persist.rs`:

1. **`persist.rs` is sealed-record IO** — it's about marshalling
   `SealedAuth` records into/out of AEAD-sealed files on disk, including
   the `seal_filename_for(sandbox_id)` path-derivation, the v1/v2/v3
   record format evolution, and `unseal_one`/`seal` operations. The
   *one* shared concern with the helper (`AeadKey::from_path`) is
   incidental — `AeadKey` happens to be a root-owned-secret-file, but
   the rest of the file does not deal with secret-file IO at all.
   Folding the helper into `persist.rs` would add a new responsibility
   to a file that already mixes record-format logic with key-loading.

2. **`db.rs` is even less appropriate** — it's pg / migration / host /
   sandbox-row CRUD; the password-file loader is the *only* secret-IO
   in that 3108-LOC file, and it's there for historical reasons
   (the pg DSN gets the password injected from a file by
   `inject_password_if_configured` at db.rs ~870).

3. **`snapshot_aead.rs` is AEAD primitives** — adding a generic
   "root-owned secret file" helper there mixes the snapshot-encryption
   surface with a controller-wide loader concern. Same anti-pattern.

4. **A new `secret_io.rs` matches the existing module-naming convention**
   in the crate (`files.rs`, `persist.rs`, `auth.rs` are all
   responsibility-named files with one job). It also signals "this is
   a small, stable helper" to future contributors — no precedent for
   accreting unrelated logic into it.

Recommended surface:

```rust
// crates/sandbox/src/secret_io.rs

use std::path::Path;

/// Read a root-owned secret file. Unix only enforces the (mode=0o400,
/// uid=0) invariant; on non-Unix this is a plain read with the length
/// check. `env_name` is threaded through error messages so operators
/// see the env var that pointed at the bad file.
///
/// `expected_len = Some(n)` enforces exact length (the AEAD key, the
/// KEK, future fixed-size secrets). `expected_len = None` accepts
/// any non-empty content (the admin token, future variable-length
/// bearers).
///
/// The boot-time enforcement matches the threat model documented in
/// R9-S4 (and siblings R9-S4b/S4c/S4d): a non-root attacker who can
/// pre-create a chmod-400 file at the secret path before the
/// controller starts could supply an attacker-controlled key,
/// breaking confidentiality / integrity of every secret derived from
/// it. The "uid == 0" check matches systemd-style secret loading at
/// `/etc/zeroship/`.
pub(crate) fn read_root_owned_secret_file(
    env_name: &str,
    path: &Path,
    expected_len: Option<usize>,
) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("{env_name}={path:?}: stat: {e}"))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o400 {
            return Err(format!(
                "{env_name}={path:?}: mode={mode:o} must be 0o400"
            ));
        }
        let uid = meta.uid();
        if uid != 0 {
            return Err(format!(
                "{env_name}={path:?}: owner uid {uid} != 0 \
                 (refusing to load; chown root:root the file)"
            ));
        }
    }
    let raw = std::fs::read(path)
        .map_err(|e| format!("{env_name}={path:?}: read: {e}"))?;
    if let Some(want) = expected_len {
        if raw.len() != want {
            return Err(format!(
                "{env_name}={path:?}: length={} bytes, expected {want}",
                raw.len()
            ));
        }
    } else if raw.is_empty() {
        return Err(format!("{env_name}={path:?}: file is empty"));
    }
    Ok(raw)
}
```

Migration of the 4 callers:

| Caller | Now | After |
|---|---|---|
| `snapshot_aead.rs::RootKek::from_path` (32 LOC) | inline 4-step | 3-line wrapper: read → `copy_from_slice` → `Self::from_bytes` |
| `persist.rs::AeadKey::from_path` (36 LOC) | inline 4-step + `File::open + read_exact` | 3-line wrapper: read → `copy_from_slice` → return `Self` |
| `db.rs::enforce_password_file_mode` (27 LOC) | inline 3-step (no length check) | 3-line wrapper, `expected_len = None`, ignore the returned Vec (just want the validation) |
| `lib.rs::load_admin_token` (33 LOC) — **closes R9-S4d** | inline 2-step (mode only, no uid) | 4-line wrapper: read → `String::from_utf8_lossy` → trim → return; `expected_len = None` |

Net: ~80 LOC new module + ~25 LOC × 4 wrappers, removes ~128 LOC of
inline 4-step blocks. ~**-50 LOC** at the crate level. Closes R9-S4d
(`lib.rs` gets the uid==0 arm "for free" via the helper).

## Architectural sketch — `Database` pool churn (R11-P1 / open_pool 25× pattern)

**Recommendation: per-compio-worker `thread_local!<RefCell<Option<Pool>>>`,
NOT `Arc<Pool>` on the struct nor `OnceLock<Pool>`.**

Forced by `compio_postgres::Pool` being `!Send + !Sync` (documented at
db.rs:300-308 and at `crates/compio-postgres/src/pool.rs:15-17` — the
Pool uses `Rc<RefCell<...>>` internally). This blocks both naive
alternatives:

1. ❌ **`Arc<Pool>` field on `Database`**: would require `Pool: Send +
   Sync`. `Database` ships through `Arc<AppState>` which ntex requires
   to be `Send + Clone` for the worker-factory closure. Adding a
   `!Send` field forces the whole `AppState` to be `!Send`, which
   breaks ntex's worker-factory bounds. Non-starter.

2. ❌ **Global `OnceLock<Pool>`**: same `Send + Sync` blocker.
   `OnceLock<T>` requires `T: Send + Sync` for `OnceLock::get()` to be
   shared across threads. Non-starter.

3. ✅ **Per-worker `thread_local!<RefCell<Option<Pool>>>`** in db.rs:
   each ntex worker thread owns its own `Pool` (which is `!Send` but
   never leaves its origin thread); `open_pool()` becomes "if the
   thread-local is `None`, lazily build one; else clone the existing
   handle". This is exactly the pattern db.rs:497-507's TODO already
   sketches:

   > "the fix is a per-compio-thread `thread_local!` holding a
   > long-lived `Pool`, lazily initialized on first use; deferred to a
   > follow-up because `compio_postgres::Pool` is `!Send` + `!Sync`,
   > so a naive thread-local works in principle but interacts subtly
   > with ntex's worker-factory bounds (factory must be `Send + Clone`;
   > thread-locals satisfy that as long as we don't try to share a
   > `Pool` across worker boundaries — which we don't, since each
   > worker thread has its own `thread_local`)."

   The right shape is:

   ```rust
   thread_local! {
       static POOL: RefCell<Option<Rc<Pool>>> = RefCell::new(None);
       static POOL_AUDIT: RefCell<Option<Rc<Pool>>> = RefCell::new(None);
       static POOL_GDPR: RefCell<Option<Rc<Pool>>> = RefCell::new(None);
   }

   impl Database {
       async fn open_pool(&self) -> Result<Rc<Pool>> {
           // try thread_local cache; if absent, build + insert
       }
   }
   ```

   `Rc<Pool>` is the right inner type (not raw `Pool`) — `compio-postgres`
   doesn't ship a shareable handle, so `Rc` is the cheap clone that
   lets multiple call sites in a single worker share the pool without
   threading lifetimes through the call graph.

**Note**: The architecture lens does NOT prescribe the perf-r11 change
itself — that's R11-P1's call (and the perf review predicts ~10-75 ms
savings per wake from this alone). Architecture's contribution: the
ownership shape MUST be thread-local, NOT `Arc<Pool>` on `Database` or
a global `OnceLock`. The struct stays as it is; the storage is at the
thread, not the type.

## Architectural sketch — post-T-8 nomad_ch.rs shape

After T-8 (Go driver becomes default + bash wrapper removed),
`crates/sandbox/src/backend/nomad_ch.rs` shrinks by ~400-700 LOC.
Assuming **R10-A4's module split lands first** (the strong
recommendation in R11-A2), the post-T-8 module tree looks like:

```
crates/sandbox/src/backend/nomad_ch/
├── mod.rs              (struct + new + probe + is_healthy + state map;
│                       ~280 LOC, unchanged shape)
├── create.rs           (create + ReleaseCreating + CreateGuard RAII +
│                       host_dir setup + FICLONE; ~600 LOC)
├── stop.rs             (stop / stop_preserving_state / stop_inner +
│                       StopDisposition enum from R3-A4; ~400 LOC)
├── restore.rs          (restore_from_sealed +
│                       restore_from_pg_and_sealed + register_restored
│                       + unregister_restored + LeasedVmSlot Drop
│                       once R4-A2 lands; ~400 LOC)
├── exec.rs             (exec / read_file / write_file / delete_file /
│                       file_tree / session_auth; ~600 LOC)
├── jobspec/
│   ├── mod.rs          (dispatcher; only ChPlugin remains after T-8;
│                       ~30 LOC, down from ~80 pre-T-8)
│   ├── chplugin.rs     (the typed `ch` Config block; ~100 LOC. The
│                       SOLE jobspec path post-T-8.)
│   └── common.rs       (cpus_boot, NOMAD_CPU_MHZ_ADVISORY, Resources
│                       block, KillTimeout; ~60 LOC)
│   [ rawexec.rs DELETED at T-8 ]
├── http.rs             (http_get_unsigned, http_post_json_unsigned,
│                       http_delete_unsigned, http_signed_async,
│                       signed_blocking_call, send_ureq, AgentResponse;
│                       ~400 LOC, unchanged)
├── wait.rs             (wait_for_alloc_running, wait_for_job_gone,
│                       wait_for_agent_livez, wait_for_agent_silent;
│                       ~500 LOC, unchanged)
└── allocator.rs        (VmIndexAllocator + tests; ~200 LOC, unchanged)
```

Deleted at T-8 (all in `jobspec/rawexec.rs` if R10-A4 lands first):

- `TaskDriverMode` enum + `task_driver_mode_from_env` (~30 LOC).
- The `match mode { RawExec => ... }` arm in `build_nomad_job_json_with`
  (~65 LOC + the wrapper_path reference).
- The entire `ZSBX_*` env block construction (~40 LOC — only retained
  under RawExec; ChPlugin reads from typed Config).
- The `nomad-vm-wrapper.sh` line-number regression tests at
  `nomad_ch.rs:3874-4016` (~120 LOC).
- The `cfg.nomad_ch.wrapper_path` field on `SandboxConfig` (~5 LOC in
  `config.rs`, ~3 LOC in test fixtures elsewhere).

Plus outside `nomad_ch.rs`:

- `crates/sandbox/scripts/nomad-vm-wrapper.sh` — entire script (~700
  LOC). The single most consequential delete in the snapshot/restore
  feature.

Total post-T-8 delete: **~400 LOC of Rust + ~700 LOC of shell**. The
remaining `nomad_ch.rs` (post-split, post-T-8) is structurally
indistinguishable from a normal Nomad backend: typed jobspec, typed
restore path, no two-channel per-VM input, no shell-script-line-pinned
regression tests.

**If R10-A4 does NOT land first**, T-8 is a surgical edit across a
5072-LOC file with 6 zones to touch. Strong architectural recommendation:
**land R10-A4 in the next cycle, before T-7's flag flips to default
ChPlugin.**

---

## What's structurally new vs. r10

| Item | r10 state | r11 state | Δ |
|---|---|---|---|
| `RestoreBackend` trait methods | 7 | **7** | 0 |
| `RealRestoreBackend` Arc-NomadCH fields | 2 | **2** | 0 |
| `Backend` enum Err-returners | 5 | **5** | 0 |
| `NomadCHBackend` pub methods | 27 | **27** at HEAD; **+2 (task_driver_mode_from_env, build_nomad_job_json_with) in working tree** | 0 / +2 wt |
| db.rs LOC | 3003 | **3108** (+105, all test growth) | +105 |
| `with_*` builders | 7 | **7** | 0 |
| `pub fn new_fixture` | 2 | **2** | 0 |
| `restore_handler.rs` LOC | 2367 | **2367** | 0 |
| `nomad_ch.rs` LOC | 4923 | **4923 at HEAD; 5072 in working tree (+149 from T-7 flag)** | 0 / +149 wt |
| Root-owned-secret-file loader sites | 3 | **4** (R9-S4d still pending) | +1 |
| `ErrorEnvelope` cross-crate drift | flagged | **zero drift; R10-A5 close confirmed** | unchanged |

Every line of growth this cycle either reinforces an existing
architectural finding (db.rs +105 LOC of test scaffolding for R9-S4c —
itself a sibling of the R11-A1 4-site duplication) or is functionally
neutral (handlers.rs +87 LOC for R10-Q1 fix). Zero structural movement
toward closing the flagship 4 (R4-A2, R10-A3, R10-A4, R10-A1).

## Recommended order of attack (updated; 5 PRs)

Unchanged order from r10's recommendation, **with one new insertion**:

1. **R11-A1** secret_io.rs extraction + R9-S4d closure (NEW THIS
   ROUND). ~80 LOC new module, ~120 LOC removed across 4 sites, fully
   mechanical. Closes R9-S4d + R11-Q2 + this finding.
2. **R10-A1 / R11-A4** db.rs split → `recovery.rs` (~280 LOC moved,
   mechanical, zero risk). Closes the §6.1 / §9.2 recovery seam.
3. **R10-A7** snapshot orchestrator extraction (~150 LOC moved, closes
   T9 + T10).
4. **R10-A4** nomad_ch.rs module split — **MUST land before T-7's flag
   flips to ChPlugin default**, per R11-A2. ~5000 LOC moved across 8-9
   child modules, mechanical, lands `StopDisposition` enum from R3-A4
   in `stop.rs` + isolates the bash-wrapper concern in
   `jobspec/rawexec.rs` for T-8 deletion.
5. **R10-A3 + R10-A2 + R4-A2 (LeasedVmSlot) together** — the
   structural fix. `SnapshotCapableBackend` trait + collapse
   `RestoreBackend` into it + `LeasedVmSlot` RAII becomes a method-
   level concept on the trait. ~600 LOC. Closes 4 carry-forwards.
6. **R4-A1 / R11-A3 / R10-A6** AppStateBuilder — closes 8-cycle
   carry-forward + `pub fn new_fixture` on both `AppState` and
   `SandboxConfig`.

Total: 6 PRs. PR #1 is new and the cheapest of the lot (~30 minutes of
mechanical work). PR #4 is now load-bearing for T-7/T-8 (architecture
finding R11-A2: do the split before T-8's cleanup, not after). The
other 4 are unchanged from r10's ranked list.

Closes 6 carry-forwards + 5 r11-new findings. Net file-count change:
+~10 modules, but each existing 2000+ LOC file drops below 1500.
