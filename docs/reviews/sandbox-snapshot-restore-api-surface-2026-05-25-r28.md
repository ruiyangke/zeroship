# Sandbox/snapshot-restore — api-surface r28 review

Date: 2026-05-25 (UTC). HEAD at audit: `a3cfca10` (per brief). Two
newer commits landed during the audit (`508c3d76`,
`baf1b78c`) — both touch `crates/sandbox/scripts/*` or
`docs/reviews/*` only and are out of api-surface scope.
Read-only.

Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.
Prior api-surface review: r27 (`568c1357`), cycle 39.

Note: uncommitted worktree edits on `restore_handler.rs` +
`wake_machine.rs` (R28-I2 transport-error VersionCheckOutcome
field — in-flight bundle agent work, half-applied with a name
mismatch between the two files) are NOT audited per the brief.
Verified via `git stash push -- restore_handler.rs wake_machine.rs`
that the committed HEAD builds clean (`cargo build -p zeroship-sandbox
--lib --tests` ⇒ 2 pre-existing warnings, zero errors).

Landed since r27 (filtered to api-surface impact):

- `c2e07b2f` — **R27-API2 closure**: gates `_test_inject_sandbox`
  under `#[cfg(any(test, feature = "test-support"))]`. Introduces
  a new `test-support` feature (`default = []`) AND a self
  dev-dep (`zeroship-sandbox = { path = ".", features =
  ["test-support"] }`) so integration tests under
  `crates/sandbox/tests/` (which link the library as an EXTERNAL
  crate where `cfg(test)` does NOT apply) keep compiling. Verified
  at **R28-API-VERIFY1** below: `cargo build -p zeroship-sandbox`
  strips the symbol (nm count = 0); `cargo build -p zeroship-sandbox
  --tests` re-enables it. **R27-API2 carry CLOSED**, but introduces
  a NEW lens — the test-support feature pattern. See R28-API3.
- `9e1f6276` — **R28-C1 fix**: inlines `vm_index` release delay in
  `CreateGuard::drop` cleanup future (the prior call to
  `spawn_delayed_release` planted a timer on the short-lived
  `detach_isolated` runtime; runtime drop discarded it; slot
  leaked). Introduces no new public symbols. New unit test
  `create_guard_drop_releases_vm_index_under_isolated_runtime`
  added inside `#[cfg(test)] mod tests` (line 4620). Verified at
  **R28-API-VERIFY2** below — no `pub` surface leak.
- `c969b94d` — **r24-A2-S3 VmIndexAllocator delay**:
  `VmIndexAllocator::spawn_delayed_release(...)` lands as `pub fn`
  on `crate::backend::nomad_ch::VmIndexAllocator`. Zero external
  callers — should be `pub(crate)`. NEW finding **R28-API1** below.
- `df06d172` — **R27-I1 BackendBuilder**: telescoping
  `from_config*` cascade replaced with `BackendBuilder<'a>`. r27
  ratified the deviation from r26's recommended positional shape
  at R27-API1 + R27-API-VERIFY3. This round verifies the rustdoc-
  strengthen recommendation (move 4th-orthogonal-field rationale
  into rustdoc): **partially addressed** — the rationale is on
  the `BackendBuilder` struct rustdoc (`mod.rs:193-198`), not on
  `Backend::builder()`. See **R28-API-VERIFY3** below.
- `821cc9bd` — **R27-M2 LATENT**: char-boundary bytes-as-char fix
  in 6 sanitize-strip sites. Internal; no wire shape change.
- `2226de7a` — **Option C Phase 2**: `SandboxConfig.
  driver_stages_disk_images: bool` lands (default `false`). Pure
  config-field exposure, no wire surface change (Nomad-internal
  meta, not §10.0 envelope).
- Pilot artefacts (`de7465ac`), docs-only (`57417911`,
  `b759c82c`, `5a0647c3`, `2468ab96`, `46d1f692`, `def11cb4`,
  `a3cfca10`, `088cff61b`) — no source-of-truth impact.
- `086971d2` — driver pin v18→v19 (scripts). Out of api-surface scope.

## Summary

- **R27-API2 (`_test_inject_sandbox` cfg-gate)** — **CLOSED at
  `c2e07b2f`**. Mirrors the `freed_for_test` precedent at
  `nomad_ch.rs:399` exactly. Verified at R28-API-VERIFY1.
- **R27-API1 (BackendBuilder rustdoc strengthen)** — **PARTIALLY
  ADDRESSED**. The 4th-orthogonal-field rationale lives on
  `BackendBuilder`'s rustdoc (`mod.rs:193-198`, mentioning
  "r27-A1 staging-locality, future VFIO-handoff or tap-leak edges")
  but NOT on `Backend::builder`'s rustdoc (`:260-280`). A reader
  who jumps to `Backend::builder` via rustdoc-search lands on the
  entry point but the WHY-rationale lives one click away on the
  struct. Pure documentation drift; ratifying as PARTIALLY CLOSED.
  See R28-API-VERIFY3.
- **R27-API3 (`metrics::*_value` `pub` → `pub(crate)` sweep)** —
  **NO MOVEMENT**. The 12 zero-external-consumer accessors stay
  `pub`. Carry held for code-quality r29.
- **R27-API4 (`WakeErrorCode::AgentVersionMismatch` doc-table)** —
  **NO MOVEMENT**. The markdown table at `db.rs:1731-1741` still
  lists 8 of 10 variants. Carry held for code-quality r29.
- **R26-API1 (driver-side counter federation)** — UNRESOLVED.
  Driver-side metric surface still depends on out-of-tree
  nomad-driver-ch behaviour. Per architecture r28's staging-
  locality ADR (`docs/decisions/2026-05-24-staging-locality.md`),
  the federation question is documented as SEPARATED — the carry
  holds at IMPORTANT but no controller-side action is required.
- **R20-API1 (schema-marker rewriter sites)** — NO MOVEMENT. The
  Option C Phase 2 commit (`2226de7a`) adds a `zsbx_stage_disks`
  meta emission BEHIND `driver_stages_disk_images`; not a
  rewriter site. Carry held.
- **R24-API3 (sync/async wake-poll `extra` asymmetry)** —
  NO MOVEMENT. Carry held.
- **R26-API5 (sanitize mask token unification)** — NO MOVEMENT.
  Carry held.
- **NEW R28-API1**: `VmIndexAllocator::spawn_delayed_release`
  (`nomad_ch.rs:363`) is `pub fn` but has zero external consumers.
  Same lens as R27-API3 (R27-I2 precedent: composite-r1 #2
  closed `pub mod metrics_export` → `pub(crate)` for the same
  pattern). MINOR (NEW).
- **NEW R28-API2**: 5 additional test-scaffolding `pub` items
  on the same lens as R27-API2 — none currently cfg-gated. The
  closure of R27-API2 establishes the precedent + the
  `test-support` feature infrastructure; this finding extends
  the audit to the rest of the crate's `pub` test scaffolding.
  Targets: `Database::from_test_config` + `Database::
  set_role_dsns_for_test` + `restore_handler::
  StubRestoreBackend` + `snapshot_handler::StubSourceVmOps` +
  `sweep::RecordingIdleSnapshotter`. IMPORTANT (NEW) —
  test scaffolding on the public API.
- **NEW R28-API3**: the self dev-dep pattern introduced by
  `c2e07b2f` (`zeroship-sandbox = { path = ".", features =
  ["test-support"] }` in `[dev-dependencies]`) is a known cargo
  idiom but carries a documentation gap — no `test-support`
  feature docs in `lib.rs` warning external callers that the
  feature is NOT part of the stable API. Cosmetic; MINOR (NEW).
- **Backlog**: r27 = 11 → r28 = 11 (1 full closure: R27-API2;
  1 partial: R27-API1 (rustdoc strengthen partial); 3 new:
  R28-API1, R28-API2, R28-API3).

## CRITICAL

None.

## IMPORTANT

### [R28-API2] 5 additional test-scaffolding `pub` items mirror the R27-API2 pattern — same surface leak, no cfg gate

- **Where**: 5 sites across `crates/sandbox/src/`:
  ```
  db.rs:518               pub async fn Database::from_test_config(...)        [#[doc(hidden)]]
  db.rs:553               pub fn Database::set_role_dsns_for_test(...)        [#[doc(hidden)]]
  restore_handler.rs:1302 pub struct StubRestoreBackend                       [#[doc(hidden)]]
  snapshot_handler.rs:691 pub struct StubSourceVmOps                          [#[doc(hidden)]]
  sweep.rs:492            pub struct RecordingIdleSnapshotter                 [#[doc(hidden)]]
  ```
  All 5 carry `#[doc(hidden)]` but NOT `#[cfg(any(test,
  feature = "test-support"))]`. The R27-API2 close-out (`c2e07b2f`)
  established the `test-support` feature + the cfg-gate
  precedent; this finding extends the audit to the rest of the
  crate's `pub` test scaffolding.
- **External consumer audit**:
  - `Database::from_test_config`: 30+ call sites in
    `crates/sandbox/tests/sandbox_{pg,admin}_e2e.rs`. Zero
    `src/` callers outside `db.rs` itself.
  - `Database::set_role_dsns_for_test`: 0 call sites under
    `crates/sandbox/tests/` (only 2 doc-comment mentions at
    `:5739` and `:5937-5939`); 0 `src/` callers. Genuinely
    DEAD test scaffolding — but `pub`.
  - `StubRestoreBackend`: 10+ call sites in
    `crates/sandbox/tests/sandbox_pg_e2e.rs:2626-3318`. Zero
    `src/` callers.
  - `StubSourceVmOps`: 5+ call sites in
    `crates/sandbox/tests/sandbox_pg_e2e.rs:2432-2559`. Zero
    `src/` callers.
  - `RecordingIdleSnapshotter`: pg-gated test consumer per
    the rustdoc at `sweep.rs:488` ("Used by the pg-gated test").
    Zero `src/` callers.
- **Why IMPORTANT** (same lens as R27-API2):
  1. **Test scaffolding ships in production binaries**. Each
     of the 5 items compiles into every release `cargo build
     --release` of `zeroship-sandbox`. The R27-API2 closure
     was driven by the specific severity of `_test_inject_
     sandbox` (raw `SigningKey` + arbitrary `agent_url`), but
     the same surface-leak reasoning applies to ALL 5
     candidates:
     - `set_role_dsns_for_test`: lets a future caller swap
       a `Database`'s audit / GDPR role DSNs at runtime — i.e.
       silently re-point the audit log writer at an attacker-
       controlled Postgres. Lower-severity than the SigningKey
       injection, but the same SHAPE of attack — runtime state
       override via a test-only hook left on the public surface.
     - `from_test_config`: lets a caller build a `Database`
       bypassing the production `from_env` migration / boot-
       fail-fast logic. The `validate_dsn_scheme` check is in
       place, but the migrations / host_id-file / boot-timeout
       checks are skipped.
     - `StubRestoreBackend` / `StubSourceVmOps`: pure unit-test
       impls of trait stubs; no security exposure but the
       `pub struct` + `pub` fields surface (e.g.
       `restore_handler.rs:1303-1309`: `pub root: PathBuf;
       pub reserved: Mutex<Vec<i16>>; pub submit_called:
       AtomicBool; pub fail_reserve: bool`) is wide enough that
       a future commit could re-use the struct in a non-test
       context (the failure-mode flags would silently take
       effect in production).
     - `RecordingIdleSnapshotter`: same shape as the stubs.
  2. **`#[doc(hidden)]` is NOT a security barrier**. It hides
     the symbol from rustdoc but NOT from the linker; external
     crates can still call any `#[doc(hidden)] pub fn`. The
     R27-API2 closure deliberately chose `cfg` (compile-time
     strip) over `#[doc(hidden)]` (rustdoc-only hide); the
     consistency lens demands the same posture here.
  3. **The precedent IS shipped**. `c2e07b2f` already added
     the `test-support` feature + the self dev-dep; gating the
     5 additional items costs only 5 `#[cfg(any(test,
     feature = "test-support"))]` attribute lines + verifying
     each test consumer is in `tests/` (auto-feature) not in
     `src/` (would need explicit gate). All 5 items pass the
     audit.
  4. **Asymmetry with existing precedents**: `restore::
     _test_build_auth_from_sealed` at `:613-617` is the
     GOLD-STANDARD shape (`#[cfg(test)] #[doc(hidden)] pub(crate)
     fn`). The 5 R28-API2 candidates are weaker on three
     dimensions (no `cfg`, `pub` not `pub(crate)`, but yes
     `#[doc(hidden)]`); `freed_for_test` at `:399-400` is
     `#[cfg(test)] pub fn`; `_test_inject_sandbox` at `:1700-
     1701` is `#[cfg(any(test, feature = "test-support"))] pub
     fn`. The 5 items in R28-API2 form the ONLY remaining
     asymmetric class.
- **Fix shape** (defers to code-quality r29):
  - **Option A** (minimal, mirrors R27-API2): gate each of the
    5 items with `#[cfg(any(test, feature = "test-support"))]`.
    The `test-support` feature already exists in
    `crates/sandbox/Cargo.toml` (line 77); the self dev-dep
    already auto-enables it for integration tests; pure
    additive change.
  - **Option B** (cleaner): introduce a `pub mod test_support`
    that's itself `#[cfg(any(test, feature = "test-support"))]`,
    and move all 5 items + `_test_inject_sandbox` +
    `freed_for_test` into it. Groups present + future test
    scaffolding into one namespace; reduces visual noise on
    the rustdoc-search surface.
  - Prefer **Option A** for the same minimum-disclosure
    rationale R27-API2 followed.
- **Verification needed at closure**: for each of the 5 items,
  after gating:
  - `cargo build -p zeroship-sandbox` (no feature, no tests)
    compiles AND `nm target/debug/libzeroship_sandbox.rlib |
    grep <symbol_name>` returns zero matches.
  - `cargo build -p zeroship-sandbox --tests` compiles (the
    self dev-dep should auto-enable the feature).
  - Existing test count baseline (540 lib / 91+ pg-gated)
    unchanged.
- **Severity**: **IMPORTANT** (5 simultaneous test-scaffolding
  leaks, same shape R27-API2 closed; precedent + infrastructure
  already in place; pure subtractive close-out).
- **Owner**: code-quality r29 (mechanical fix; security
  reviewer signals on `set_role_dsns_for_test` specifically as
  the highest-severity item of the 5).

### [R26-API1] (carry) driver-side `nomad_driver_ch_destroy_task_unreaped_total` still has no operator-readable surface

- **Where**: out-of-tree `nomad-driver-ch` repo emits the counter;
  no consumer-side mention in `crates/sandbox/src/`.
- **Status r28**: Unchanged from r27. The controller-side half is
  shipped (`/metrics` at `admin_handlers.rs:2055`); driver-side
  surface continues to depend on Nomad's go-metrics fanout. The
  staging-locality ADR (`docs/decisions/2026-05-24-staging-
  locality.md`) documents the federation as SEPARATED.
- **Severity**: IMPORTANT (carry from r26).
- **Owner**: out-of-tree nomad-driver-ch / observability ADR.

### [R20-API1] (carry) schema-marker rewriter sites unchanged

- **Where**: 4 path-derivation rewriter sites — unchanged this round.
- **Status r28**: The Option C Phase 2 work (`2226de7a`) adds a
  `zsbx_stage_disks` Nomad-meta field gated on
  `cfg.driver_stages_disk_images`. The meta emission is a NEW
  hand-off mechanism — not a rewriter rewrite — and is cold-boot-
  only. Restore-path rewriters untouched.
- **Severity**: IMPORTANT (5-round carry; quadruple motivation).
- **Owner**: security (driver-side validator landing).

## MINOR

### [R28-API1] `VmIndexAllocator::spawn_delayed_release` is `pub fn` with zero external consumers; same lens as composite-r1 #2 / R27-API3

- **Where**: `crates/sandbox/src/backend/nomad_ch.rs:363-387`.
- **Snippet**:
  ```rust
  pub fn spawn_delayed_release(
      allocator: Arc<Mutex<Self>>,
      i: u16,
      delay: Duration,
      reason: &'static str,
      sandbox_id: Uuid,
  ) {
      compio::runtime::spawn(async move {
          if !delay.is_zero() {
              compio::time::sleep(delay).await;
          }
          allocator
              .lock()
              .unwrap_or_else(|p| p.into_inner())
              .release(i);
          tracing::info!(...);
      })
      .detach();
  }
  ```
- **Audit**: `grep -rn "spawn_delayed_release" crates/sandbox/`
  returns 16 hits:
  - 1 definition at `nomad_ch.rs:363`.
  - 2 production call sites — `nomad_ch.rs:1324` (`stop_inner`)
    and `nomad_ch.rs:2277-2299` (rustdoc COMMENT explaining why
    `CreateGuard::drop` does NOT use the helper after R28-C1).
    Note that R28-C1's fix REMOVED the second production caller;
    only `stop_inner` remains.
  - 6 test call sites inside `#[cfg(test)] mod tests` at
    `nomad_ch.rs:4597-4780` (`spawn_delayed_release_with_zero_
    delay_releases_immediately`, `spawn_delayed_release_honors_
    configured_delay`, etc).
  - 0 callers in `crates/sandbox/tests/` (integration).
  - 0 callers in any other crate.
- **Why MINOR (same lens as R27-API3)**:
  1. **Defensive overshoot**. The helper is consumed once in
     production (`stop_inner` at `:1324`) and 6 times in
     same-module tests. The `pub` surface is undeserved by the
     consumer set.
  2. **The composite-r1 #2 precedent applies**. `pub mod
     metrics_export` → `pub(crate)` because the single consumer
     was in the same crate. `spawn_delayed_release` has the same
     shape: single in-crate caller (`stop_inner`), same-module
     tests.
  3. **R28-C1's fix sharpens the case for narrowing**. The
     CreateGuard::drop call site that USED to consume the helper
     was inlined out; the rustdoc at `nomad_ch.rs:2277-2299`
     explicitly warns "Do NOT unify the two call sites by
     changing `spawn_delayed_release` — the bug is the runtime
     lifetime mismatch, not the helper itself." That rustdoc
     IS a contract that locks the helper to the one remaining
     long-lived-runtime caller. `pub(crate)` enforces the
     contract at the type system.
  4. **The narrowing is purely mechanical**. `pub fn` →
     `pub(crate) fn` at line 363; no call-site touches required
     (no external callers exist).
- **Recommendation**: tighten to `pub(crate) fn
  spawn_delayed_release` in one commit. Bundle with R27-API3 if
  code-quality r29 takes both.
- **Severity**: MINOR (api-surface tidiness; same minimum-
  disclosure lens composite-r1 #2 / R27-API3 already established).
- **Owner**: code-quality r29.

### [R28-API3] `test-support` feature lacks crate-level rustdoc warning that it's not API-stable

- **Where**: `crates/sandbox/Cargo.toml:71-77` declares the
  feature; `crates/sandbox/src/lib.rs:1-50` is the crate root
  with no `#[doc = ...]` reference to features.
- **The R27-API2 fix landed `c2e07b2f`** with the inline Cargo.toml
  comment:
  ```toml
  # Exposes test-only helpers (e.g. `NomadChBackend::_test_inject_sandbox`)
  # to integration tests in `crates/sandbox/tests/`. Off by default so
  # production binaries don't carry the scaffolding.
  test-support = []
  ```
  This is the ONLY documentation. A downstream caller who reads
  `cargo doc --open --features test-support` would land on the
  scaffolding's rustdoc but never see "this feature is NOT API-
  stable; do NOT enable it from production crates" — which is the
  standard discipline `tokio` / `serde` / `clap` follow for
  similar features (`tokio` uses `unstable`, `serde` uses
  `derive`, etc., each with an explicit crate-root note).
- **Why MINOR**:
  1. The cargo book's recommended idiom for test-only features
     IS to add a crate-root note. The current `Cargo.toml` comment
     is invisible to `cargo doc` consumers.
  2. **Workspace feature unification risk is dormant**. Currently
     only the self dev-dep enables `test-support`. No other
     workspace member depends on `zeroship-sandbox`. But cargo's
     feature unification means that if ANY future workspace member
     enables `test-support` on a normal `[dependencies]` entry of
     `zeroship-sandbox`, ALL workspace builds of `zeroship-sandbox`
     would silently compile WITH the feature on. A crate-root
     warning would catch this at code review.
  3. **The risk is contained today** because:
     - `default = []` (the feature is opt-in).
     - Self dev-dep activates the feature only in
       `[dev-dependencies]` resolution, segregated from normal
       compilation.
     - The five gated symbols all have non-collision names
       (`_test_inject_sandbox`, `freed_for_test`); no production
       caller would accidentally name-match.
- **Recommendation**: add a crate-root note like:
  ```rust
  //! # Features
  //!
  //! - **`test-support`** *(off by default)* — exposes test-only
  //!   helpers (`NomadCHBackend::_test_inject_sandbox`, etc.) to
  //!   integration tests under `crates/sandbox/tests/`. Enabled
  //!   automatically by the self dev-dep when running
  //!   `cargo test -p zeroship-sandbox`. **NOT API-stable; do
  //!   NOT enable from production crates.**
  ```
  Approximately 8 lines at `lib.rs` crate header (or in a
  dedicated `//! # Cargo features` section).
- **Severity**: MINOR (documentation hygiene; the feature is
  defensible — the doc warning is for downstream-callers'
  diligence, not a security control).
- **Owner**: code-quality r29.

### [R27-API1] (carry, partial closure) BackendBuilder rustdoc-strengthen — rationale on struct rustdoc, not on `Backend::builder()` entry-point

- **Where**: `crates/sandbox/src/backend/mod.rs:182-205`
  (`BackendBuilder` struct rustdoc) +
  `mod.rs:260-280` (`Backend::builder` fn rustdoc).
- **r27 recommendation** (api-surface-r27 R27-API1 `:372-381`):
  > "the `Backend::builder` docstring at `:260-279` could MOVE
  > the arch-r27-A1 staging-locality + 4th-orthogonal-field
  > rationale from the commit message INTO the rustdoc, so
  > future readers see why the builder shape was chosen over
  > the positional consolidation without git archaeology."
- **What landed**: the 4th-orthogonal-field rationale lives on
  the `BackendBuilder` STRUCT rustdoc (`:193-198`):
  ```rust
  /// **Why a builder, not a flat struct of `Option` fields**: tests +
  /// lifecycle examples that don't exercise persistence / node-pin
  /// stay one-liners (`Backend::builder(&cfg).build()?`); orthogonal
  /// extension fields (r27-A1 staging-locality, future VFIO-handoff or
  /// tap-leak edges) absorb as new `.with_*()` setters without
  /// reshaping any existing call site.
  ```
  The `Backend::builder()` fn rustdoc at `:260-280` only mentions:
  > "Replaces the prior 3-level telescoping constructor cascade
  > (R27-I1) — see git history (commit landing R27-I1) for the
  > pre-builder shape."
- **Why MINOR / partial**: rustdoc-search lands a user on
  `Backend::builder` (the entry point) FIRST; the rationale is
  one click away (the `[`BackendBuilder`]` link in line 263).
  Not a documentation void, but the WHY is split across two
  pages.
- **Recommended micro-edit**: add one line after `:267`:
  ```rust
  /// The builder shape (rather than positional args)
  /// absorbs the 4th-orthogonal-field cliff — see
  /// [`BackendBuilder`] rustdoc.
  ```
  Plus a brief reference to `docs/decisions/2026-05-24-staging-
  locality.md` so the architectural-decision link is one hop
  rather than two.
- **Severity**: MINOR (rustdoc cosmetic; partial closure).
- **Owner**: code-quality r29.

### [R27-API3] (carry, no movement) 12 of 14 `metrics::*_value` accessors are `pub` with zero external consumers

- **Where**: `crates/sandbox/src/metrics.rs` — 14 read
  accessors, 12 with zero external consumers.
- **Status r28**: No movement. r27 audited the surface
  (`r27.md:386-451`); code-quality r28 review at
  `docs/reviews/sandbox-snapshot-restore-code-quality-2026-05-25-
  r28.md` did not address the sweep.
- **Severity**: MINOR (carry).
- **Owner**: code-quality r29.

### [R27-API4] (carry, no movement) `WakeErrorCode` rustdoc table at `db.rs:1731-1741` still lists 8 of 10 variants

- **Where**: `crates/sandbox/src/db.rs:1731-1741`.
- **Status r28**: No movement. The 10th variant `AgentVersionMismatch`
  (landed r27) and the 9th variant `StagingPathMissing` (landed
  earlier) remain documented in prose below the table but absent
  from the table itself.
- **Severity**: MINOR (carry).
- **Owner**: code-quality r29.

### [R24-API3] (carry) async wake-poll envelope still missing structured `which` field

- **Where**: `admin_handlers.rs:1990-2008`. Unchanged.
- **Severity**: MINOR (carry).
- **Owner**: architecture (Phase-2 `wake_jobs.error_extra
  JSONB`).

### [R26-API5] (carry) sanitize-widening mask token unification

- **Where**: `crates/sandbox/src/wake_machine.rs:748` /
  `:1065` / `:1119`. Unchanged this round.
- **Severity**: MINOR (carry; cosmetic).
- **Owner**: code-quality r29.

### Other carries (no movement)

- **R19-API2** — `pub` → `pub(crate)` sweep; R27-API3 + R28-API1
  extend the same lens.
- **R22-API2** — controller-side readyz tests still synthesise
  response inline.
- **R22-API3** — `rootfs_source` doc asymmetry.
- **R23-API2** / **R23-API3** — comment-only / forward-pressure.
- **R24-API2** — observation only.
- **R24-MIG1** — rolling-restart hazard documentation.
- **R24-SWEEP1** — sweep heartbeat visibility.
- **R25-API3** — `Option<String>` vs newtype on Nomad node_id.
- **R25-API4** — `Result<_, String>` typed-enum forward-pressure.
- **R25-API5** — doc-strengthen on parser.

## Considered + dismissed

- **`MockChRemoteClient` (`snapshot_handler.rs:126`) is `pub`
  without `#[doc(hidden)]` AND without `#[cfg(test)]` — should
  this be in R28-API2?**: The rustdoc at `snapshot_handler.rs:37`
  documents that "PR 3b ships only `MockChRemoteClient`" — i.e.
  the production runtime CURRENTLY uses MockChRemoteClient as
  the production `ChRemoteClient` impl, pending the real
  `RealChRemoteClient` (subprocess-based) landing. So
  `MockChRemoteClient` is genuinely production-callable AND
  test-callable; `pub` is warranted. The doc-comment slightly
  misleads ("Test/mock implementation… Used by handler unit
  tests + dev-mode dry runs.") but the actual production state
  is "Mock is the only impl yet shipped". **No nit**.
- **`BackendBuilder::with_persist` + `with_local_nomad_node_id`
  consume self vs borrow self — should they take `&mut self`?**:
  The builder is one-shot at every observed call site
  (`Backend::builder(&cfg).build()?`); consuming-self is the
  standard idiom for one-shot builders (cf. `reqwest::
  ClientBuilder`, `clap::Command`). The reassign-on-conditional-
  setter pattern at `lib.rs:705-714` works correctly with
  consuming-self. `&mut self -> &mut Self` would enable
  multiple borrows but is unidiomatic for one-shot construction.
  **No nit**.
- **`BackendBuilder<'a>` carries a borrowed `&'a SandboxConfig` —
  could it own a clone instead?**: `SandboxConfig` is large
  (multi-KB; dozens of fields including nested `NomadCHConfig`,
  `K8sConfig`); cloning at builder construction time would be
  wasteful for the common one-liner pattern. The borrow lifetime
  doesn't leak awkwardly because the builder is always
  constructed + consumed in one expression. The `build()` method
  already does the `cfg.clone()` at the right moment (once per
  call). **No nit**.
- **The self dev-dep pattern is unusual — should we recommend
  switching to `dev-dependencies = { "test-support" = [...] }`
  feature-resolver instead?**: `[dev-dependencies]
  zeroship-sandbox = { path = ".", features = ["test-support"] }`
  is documented in the cargo book under "Features and
  dev-dependencies" as the canonical pattern for crates that
  need a feature auto-enabled for integration tests. The
  alternative (no self dev-dep, manual `--features test-support`
  on `cargo test`) is brittle (every contributor has to remember
  the flag). The self-dep pattern is what `tokio`, `bytes`, etc.
  use. **No nit** beyond R28-API3's documentation
  strengthen.
- **R28-API3's documentation warning could be a `#[doc =
  cfg(feature = "test-support")]` annotation on each gated
  symbol instead of a crate-root note**: `doc_cfg` is nightly-
  only (`#![feature(doc_cfg)]`); the stable workaround is a
  manual `**Available only with `test-support`.**` line in each
  symbol's rustdoc. But the current symbols are all
  `#[doc(hidden)]` (e.g., `_test_inject_sandbox` has
  `#[doc(hidden)]` semantically implied by the underscore name +
  the explicit `#[doc(hidden)]` candidates in R28-API2), so the
  rustdoc page isn't reachable. Crate-root warning is the right
  layer. **No nit**.
- **`spawn_delayed_release` could be renamed to
  `_spawn_delayed_release` (underscore prefix = "internal")
  instead of narrowing visibility**: rust convention reserves
  underscore prefixes for "unused" or "intentionally private but
  publicly named" symbols; the established idiom IS the
  visibility modifier (`pub(crate)`). Renaming would conflict
  with the `_test_` convention (which signals test-scaffolding,
  not internal). **No nit**.

## §10.0 envelope state post-r28

### Inventory (delta from r27)

```
New since r27:  None (no §10.0-touching commits this round).
                R28-C1 fix is purely internal; R27-API2 closure
                is an attribute change with no wire surface impact.
```

The 32 §10.0 codes from r27 are unchanged. `WakeErrorCode` is
still 10 variants; `wire_code` triangle still intact at
`db.rs:1680-1825`.

### POST-endpoint envelope audit (delta from r27)

| Endpoint | Required | 401 | 403 | 503 | Success | Post-r28 wire-shape drift? |
|---|---|---|---|---|---|---|
| `POST /admin/sandboxes/{id}/snapshot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 | none |
| `POST /admin/sandboxes/{id}/wake` (sync) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (`agent_url`) | none |
| `POST /admin/sandboxes/{id}/wake` (async) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 202 (`wake_id`) | none |
| `GET /admin/sandboxes/{id}/wake/{wake_id}` | RO | `unauthorized` | `insufficient_role` | `admin_api_disabled` | 200/202 | none |
| `POST /admin/sandboxes/{id}/cold-boot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | `feature_disabled` 501 | none |
| `DELETE /admin/users/{id}` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (delete tombstone) | none |
| `GET /readyz` | None | — | — | `backend_unhealthy` | `{"status":"ok"}` | none |
| `GET /metrics` | RO | `unauthorized` | `insufficient_role` | `admin_api_disabled` | `text/plain; version=0.0.4` | none |

§10.0 envelope unchanged. R28-C1's fix is internal-only (vm_index
allocator scheduling); no observable wire shape moves.

## R28-API-VERIFY1 — R27-API2 closure verification

**Mandate**: audit the test-support feature pattern's correctness
+ scan for related scaffolding leaks (per brief).

**Audit checks** (all pass):

1. **Feature declaration** ✓ `crates/sandbox/Cargo.toml:77`:
   `test-support = []` (no transitive feature dependencies, no
   default).
2. **Self dev-dep** ✓ `Cargo.toml:84`: `zeroship-sandbox = {
   path = ".", features = ["test-support"] }`. Idiomatic cargo
   pattern; documented in the cargo book ("Features and
   dev-dependencies").
3. **Gate on `_test_inject_sandbox`** ✓ `nomad_ch.rs:1700`:
   `#[cfg(any(test, feature = "test-support"))]`. Matches the
   `freed_for_test` precedent at `:399` (one-arm: `#[cfg(test)]`)
   plus an extra arm for external test crates.
4. **Symbol stripping verification** ✓ Reproduced locally:
   ```
   $ cargo build -p zeroship-sandbox 2>&1 | tail -1
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.16s
   $ nm --defined-only target/debug/libzeroship_sandbox.rlib 2>/dev/null \
       | grep -c _test_inject_sandbox
   0
   ```
   Symbol stripped from production rlib.
5. **Test-build verification** ✓ Reproduced locally:
   ```
   $ cargo build -p zeroship-sandbox --tests 2>&1 | tail -1
   Finished `dev` profile [unoptimized + debuginfo] target(s) in 17.00s
   ```
   Clean. Feature auto-enabled via dev-dep.
6. **Integration test consumers** ✓ 7 call sites under
   `crates/sandbox/tests/sandbox_{pg,typed_id,preview,
   preview_ws,preview_share}_e2e.rs` enumerate; all compile.
7. **Lib-test consumers** ✓ Per the inline doc-comment at
   `:1693-1697`, lib tests pick up the symbol via `cfg(test)`,
   integration tests via the feature. Both branches verified.

**Verdict**: R27-API2 is STRUCTURALLY CLEAN. The self dev-dep
is the standard idiom; the gate matches the `freed_for_test`
precedent.

**Follow-up observation**: the verification surfaced 5
additional test-scaffolding leaks (R28-API2) and a documentation
gap on the feature itself (R28-API3). Both are new findings
filed below.

## R28-API-VERIFY2 — R28-C1 fix verification (no new pub surface)

**Mandate**: confirm R28-C1's inline fix introduced no new
public symbols + the new test is `#[cfg(test)]`-gated.

**Audit checks** (all pass):

1. **No new pub items** ✓ `git diff 568c1357..HEAD -- 'crates/
   sandbox/src/**/*.rs' | grep '^[+-]pub '` returns only:
   ```
   +    pub fn spawn_delayed_release(   [c969b94d r24-A2-S3]
   +    pub fn freed_for_test(&self)... [c969b94d r24-A2-S3, #[cfg(test)] gated]
   ```
   The R28-C1 fix itself (`9e1f6276`) introduced ZERO new pub
   symbols — pure inline behavioural change inside a closure.
2. **Test placement** ✓ `create_guard_drop_releases_vm_index_
   under_isolated_runtime` at `nomad_ch.rs:4620` is inside
   `#[cfg(test)] mod tests {` (which starts at `:4340`).
3. **Test marker** ✓ The test is `#[test]` (sync), NOT
   `#[compio::test]` — which is the load-bearing detail per
   the test rustdoc (the `detach_isolated` pattern requires no
   ambient compio runtime).
4. **Rustdoc at the fix site** ✓ `nomad_ch.rs:2275-2301`
   documents WHY the helper call was inlined (short-lived
   runtime drop discards pending detached tasks) AND
   explicitly warns "Do NOT unify the two call sites by
   changing `spawn_delayed_release` — the bug is the runtime
   lifetime mismatch, not the helper itself." Locks the
   asymmetric contract into source.

**Verdict**: R28-C1 fix is STRUCTURALLY CLEAN; no new public
surface introduced. The asymmetric contract documented at the
fix site (CreateGuard::drop inlines; stop_inner uses helper)
strengthens the case for `pub(crate)` on `spawn_delayed_release`
(R28-API1).

## R28-API-VERIFY3 — BackendBuilder shape post-r27 verification

**Mandate**: confirm the BackendBuilder shape is healthy +
verify the r27 doc-strengthen recommendation status.

**Audit checks** (all pass with one PARTIAL on rustdoc):

1. **Struct visibility** ✓ `BackendBuilder<'a>` is `pub`;
   external test crates (e.g. `tests/sandbox_pg_e2e.rs`)
   construct backends via `Backend::builder(&cfg).build()?`.
2. **Field visibility** ✓ All 3 fields are private; setters are
   the only mutation surface.
3. **`#[must_use]`** ✓ `mod.rs:200`. Catches dropped-builder
   bugs at compile time.
4. **`#[allow(missing_debug_implementations)]`** ✓ `:201`.
   Persistence has no `Debug` impl; comment cites `sweep.rs`
   convention.
5. **Setter shape** ✓ Consuming-self (`mut self -> Self`),
   idiomatic for one-shot builders (`reqwest::ClientBuilder`,
   `clap::Command`).
6. **Setters take `T` not `Option<T>`** ✓ Per the rustdoc:
   "callers only invoke `.with_persist(p)` /
   `.with_local_nomad_node_id(id)` when they have a value."
   The production caller at `lib.rs:707-712` follows this:
   `if let Some(p) = persist.clone() { b = b.with_persist(p); }`.
7. **Lifetime annotation `<'a>`** ✓ `BackendBuilder<'_>` at the
   `Backend::builder` return type; lifetime is implicit at every
   call site (build-then-consume pattern).
8. **Rustdoc placement** ⚠️ **PARTIAL**: the 4th-orthogonal-
   field rationale lives on `BackendBuilder`'s struct rustdoc
   (`:193-198`), not on `Backend::builder`'s fn rustdoc
   (`:260-280`). r27 recommended moving the rationale INTO the
   `Backend::builder` fn rustdoc; it landed on the struct
   instead. Pure documentation drift; ratified as PARTIALLY
   CLOSED at R27-API1 (carry).

**Verdict**: BackendBuilder is STRUCTURALLY CLEAN. Rustdoc
strengthen carries forward as the only outstanding piece.

## R28-API-VERIFY4 — `freed_for_test` cfg gate verification

**Mandate** (per brief): verify `freed_for_test` is `#[cfg(test)]`
gated (r24-A2-S3 introduced it).

**Audit check** ✓ `nomad_ch.rs:399-402`:
```rust
#[cfg(test)]
pub fn freed_for_test(&self) -> &BTreeSet<u16> {
    &self.freed
}
```
Gated correctly. Callers:
- `nomad_ch.rs:4725` (`create_guard_drop_releases_vm_index_
  under_isolated_runtime` in `#[cfg(test)] mod tests`).
- `nomad_ch.rs:4760` + `:4772` (`spawn_delayed_release_*`
  in same `#[cfg(test)] mod tests`).
- `nomad_ch.rs:7560` (B19 regression test in same
  `#[cfg(test)] mod tests`).

All four callers are inside `#[cfg(test)] mod tests {` (which
starts at `:4339`). Zero external test (`tests/`) callers; the
`#[cfg(test)]` arm without the `feature = "test-support"`
extension is correct.

**Verdict**: `freed_for_test` is STRUCTURALLY CLEAN. Asymmetry
with `_test_inject_sandbox` (which carries the extra
`feature = "test-support"` arm) is INTENTIONAL — `freed_for_
test` has no integration-test consumers, so the feature arm
would be dead code.

## Cross-lens consensus

### R28-API-CROSS-R28C1 — concurrency r28 R28-C1 closure ratification

Concurrency r28's R28-C1 CRITICAL closed at `9e1f6276` with the
inline-release fix. R28-API-VERIFY2 confirms no new public
surface introduced. The asymmetric contract documented at the
fix site (CreateGuard::drop inlines; stop_inner uses helper)
informs R28-API1 (the helper has only one production caller now,
strengthening the case for `pub(crate)`). **Cross-lens
consistency**: no escalation.

### R28-API-CROSS-R29SEC — security r28 hand-off

Security r28's review (`docs/reviews/sandbox-snapshot-restore-
security-2026-05-25-r28.md`) did not surface any pub-surface
findings this round. R28-API2's `Database::set_role_dsns_for_test`
finding has security implications (silent DSN override on the
audit / GDPR roles) — flagging to security r29 for sign-off
on the priority weighting.

### R28-API-CROSS-R29CQ — code-quality r28 hand-off

Code-quality r28's review (`docs/reviews/sandbox-snapshot-
restore-code-quality-2026-05-25-r28.md`) did not address
R27-API3 / R27-API4. Both carry forward to r29 alongside
R28-API1 (`pub` → `pub(crate)` sweep extension) and R28-API3
(test-support feature documentation strengthen) + the
R27-API1 partial-closure rustdoc micro-edit.

### R28-API-CROSS-R27CONC — concurrency r27 alignment

Thread-local `Rc<Pool>` (composite-r1 #5 / R23-P1 / R11-P1
lineage) shipped without api-surface impact in r27; R28-C1's fix
is single-tasking IO scheduling discipline, also no api-surface
impact. **Cross-lens consensus**: clean.

### Other cross-lens

- **Performance r27**: no api-surface intersect this round.
- **Test-coverage r29**: owns the R22-API2 carry + R25-API1
  cold-boot-side sibling pin + extending `/metrics` coverage
  (200/401/403 paths) per the r27 hand-off.

## Lens hand-off

- **To architecture r29**:
  - r26-A1 (`BackendFailureDetail` carry) — still open.
  - R26-API1 (driver-side counter federation) — controller
    side closed via R26-API2; driver-side surface depends on
    out-of-tree nomad-driver-ch / Nomad agent metrics fanout.
  - R24-API3 Phase-2 schema decision (`wake_jobs.error_extra
    JSONB`) still open.
- **To test-coverage r30**:
  - R22-API2 carry.
  - R25-API1 cold-boot-side sibling pin still open.
  - §10.0 envelope enumeration test (r23 carry).
  - Extend `/metrics` route-level integration test
    (`tests/sandbox_admin_e2e.rs:1288`) to 200/401/403 paths
    per r27 hand-off.
- **To security r29**:
  - R20-API1 schema-marker carry (5-round + quadruply-
    motivated).
  - **NEW R28-API2** subset: `Database::set_role_dsns_for_test`
    specifically — silent runtime DSN override on the audit /
    GDPR pools is the highest-severity item of the 5
    candidates; security sign-off requested on whether the
    cfg-gate is sufficient or whether the function should be
    deleted entirely (current external-consumer count is zero;
    only 2 doc-comment mentions in `tests/sandbox_pg_e2e.rs`).
- **To code-quality r29**:
  - **NEW R28-API1** `VmIndexAllocator::spawn_delayed_release`
    `pub` → `pub(crate)`. Bundle with R27-API3 (12
    `metrics::*_value` accessors) if a single sweep is taken.
  - **NEW R28-API2** 5 test-scaffolding items `pub` →
    `#[cfg(any(test, feature = "test-support"))]`. Mechanical
    fix following the R27-API2 template. Verification recipe
    documented inline.
  - **NEW R28-API3** add a `# Features` rustdoc note at
    `lib.rs` crate-root documenting `test-support` is not
    API-stable.
  - **R27-API1 partial-closure carry**: add a one-line
    cross-reference from `Backend::builder` rustdoc to
    `BackendBuilder` struct rustdoc (so the rationale is one
    hop, not two).
  - **R27-API3 carry**: 12 `metrics::*_value` accessors
    `pub` → `pub(crate)`.
  - **R27-API4 carry**: extend the rustdoc table at
    `db.rs:1731-1741` from 8 to 10 rows.
  - **R26-API5 carry** sanitize mask token unification.
  - R22-API3 (`rootfs_source` doc asymmetry) carry.
  - R23-API2 / R23-API3 carries.
  - R25-API5 (parser doc-strengthen) carry.
- **To concurrency r29**: no api-surface findings cross over
  this round.

## Backlog carry table

| ID | First round | Status r28 | Severity | Lens to own |
|---|---|---|---|---|
| R19-API2 | r19 | Open (carry; R27-API3 + R28-API1 extend scope) | MINOR | code-quality |
| R20-API1 | r20 | Open (5-round carry; quadruply-motivated; held) | IMPORTANT | security |
| R22-API2 | r22 | Open (carry) | MINOR | test-coverage |
| R22-API3 | r22 | Open (comment-only) | MINOR | code-quality |
| R23-API2 | r23 | Open (comment-only) | MINOR | code-quality |
| R23-API3 | r23 | Open (forward-pressure / rustdoc rule) | MINOR | code-quality |
| R24-API2 | r24 | Open (observation only) | MINOR | — |
| R24-API3 | r24 | Open (async/sync `extra` asymmetry; carry) | MINOR | architecture |
| R24-MIG1 | r24 | Open | MINOR | code-quality / docs |
| R24-SWEEP1 | r24 | Open | MINOR | code-quality |
| R25-API3 | r25 | Open (observation) | MINOR | — |
| R25-API4 | r25 | Open | MINOR | code-quality |
| R25-API5 | r25 | Open (doc-strengthen) | MINOR | code-quality |
| R26-API1 | r26 | Open (driver-side half still has no operator surface) | IMPORTANT | architecture |
| R26-API5 | r26 | Open (carry; no movement) | MINOR | code-quality |
| R27-API1 | r27 | **PARTIALLY CLOSED** (rationale on struct rustdoc; not on `Backend::builder` fn rustdoc) | MINOR | code-quality |
| R27-API2 | r27 | **CLOSED at c2e07b2f** (verified R28-API-VERIFY1) | — | — |
| R27-API3 | r27 | Open (carry; no movement) | MINOR | code-quality |
| R27-API4 | r27 | Open (carry; no movement) | MINOR | code-quality |
| **R28-API1** | **r28** | NEW — `spawn_delayed_release` `pub` → `pub(crate)` | MINOR | code-quality |
| **R28-API2** | **r28** | NEW — 5 test-scaffolding leaks; same lens as R27-API2 | **IMPORTANT** | code-quality / security |
| **R28-API3** | **r28** | NEW — `test-support` feature lacks crate-root doc warning | MINOR | code-quality |

Net: r27 open = 11 → r28 open = 11 (1 full closure: R27-API2;
1 partial: R27-API1; 3 new: R28-API1, R28-API2, R28-API3).

## Trend

- **r17-r19**: §10.0 envelope discipline (RIPS pins, wire-code
  inventory) — settled.
- **r20-r22**: typed-error surface (WakeErrorCode triangle,
  RestoreHandlerError, sanitize_error_message) — landed +
  widened.
- **r23-r25**: pre-flight typed channels (StagingPathMissing,
  StagingPreflight), cross-emitter parity — landed.
- **r26**: observability-API surface gap promoted to
  IMPORTANT — controller `/metrics` missing,
  driver-controller hand-off undefined.
- **r27**: observability-API surface CLOSED on the controller
  side. Backend construction API consolidated (BackendBuilder
  landed). 4 composite-r1 cleanups closed within round.
- **r28 (this round)**: minimum-disclosure forward-pressure.
  R27-API2 closed (test-scaffolding cfg-gate); the closure
  surfaces the BROADER pattern — 5 more test-scaffolding
  leaks (R28-API2), 1 more `pub`-overshoot helper (R28-API1),
  1 feature-doc gap (R28-API3). The IMPORTANT item this round
  (R28-API2) is the same shape R27-API2 just closed; the close-
  out template is already shipped (the `test-support` feature
  + the self dev-dep), making R28-API2 a mechanical follow-on.
  **The defining api-surface theme of r28 is precedent-
  propagation: R27-API2's close-out shipped infrastructure
  (the `test-support` feature) that NOW makes 5 additional
  scaffolding leaks trivial to close in one PR. Concurrency
  r28's R28-C1 fix added no new surface but, in inlining the
  CreateGuard::drop helper, sharpened the case for narrowing
  `spawn_delayed_release` to `pub(crate)` — the helper's only
  remaining production caller is `stop_inner`.**

The r28 backlog stays flat at 11 despite 1 closure + 3 new
findings: the new findings ride on the R27-API2 precedent and
are mechanical to close. Multi-round carries continue to
dominate the IMPORTANT class (R20-API1, R26-API1) plus the
NEW R28-API2 — none of the IMPORTANTs are bug-class regressions
from this round; all are forward-pressure on minimum-disclosure
or out-of-tree dependencies.
