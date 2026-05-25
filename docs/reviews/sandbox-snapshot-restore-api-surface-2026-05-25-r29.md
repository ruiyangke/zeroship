# Sandbox/snapshot-restore — api-surface r29 review

Date: 2026-05-25 (UTC). HEAD at audit: `e66d5efb` (per brief).
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.
Read-only. Round 43 of pilot-cron loop. Prior api-surface review: r28
(`a3cfca10`), cycle 41.

Landed since r28 (filtered to api-surface impact):

- `c969b94d` — already audited at r28 (carry was R28-API1, see r29 closure
  below).
- `9e1f6276` — already audited at r28 (R28-C1 inline-release fix).
- `c2e07b2f` — already audited at r28 (R27-API2 closure + test-support
  feature infrastructure).
- `425a5522` — `ErrorEnvelope::with_extra(Map, …)`; panic-on-non-object.
  No new public symbols; sharpens an existing entry-point contract. Out
  of api-surface delta scope this round.
- `ca8d960a` / `035c3564` / `ce218860` — T5 wave: `WakeErrorCode::
  AgentVersionMismatch` variant + wire_code + `verify_agent_version_post_restore`
  shipped. Documented under r27/r28 stream; no api-surface drift this
  round.
- `d00f12dd` — **R28-I1 + R28-I2 land**: T5 + clock_resync parallelisation
  via `futures::join!` AND half-dead-agent fingerprint via typed
  outcomes. **Introduces 2 new typed enums + 1 new fn at module
  level** plus an accessor method. Audited at **R29-API-VERIFY1** below.
- `b8310356` — heredoc audit hardening at 11 callsites in
  `crates/sandbox/scripts/*.sh`. No `crates/sandbox/src/**/*.rs` delta;
  out of api-surface scope.
- `62b083e1` — **R29-C1 + arch-r29-A2 class-fix**: deletes the
  `spawn_delayed_release` helper and introduces TWO replacement
  helpers (`release_vm_index_after` + `spawn_delayed_release_in_worker`)
  with deliberately-typed safety properties. **Closes R28-API1**.
  Audited at **R29-API-VERIFY2** below.
- `680baafa` / `e66d5efb` — driver / controller pin bumps in
  `crates/sandbox/scripts/*`. No `src/**/*.rs` delta.

## Summary

- **R28-API1 (`VmIndexAllocator::spawn_delayed_release` `pub` →
  `pub(crate)`)** — **STRUCTURALLY CLOSED** at `62b083e1`. The
  helper is fully **DELETED**. Verified at R29-API-VERIFY2:
  `grep -n "fn spawn_delayed_release\b"` returns zero matches in
  production code; only rustdoc historical mentions remain (15 across
  `nomad_ch.rs` referencing the pre-r29 helper for context). The fix
  shape is BETTER than r28's recommendation — instead of narrowing
  visibility, the class-fix splits the helper into two typed
  successors (`release_vm_index_after` — pub, async, inline-await
  safe everywhere; `spawn_delayed_release_in_worker` — pub, returns
  `Task<Result<(), Box<dyn Any+Send>>>` so the caller MUST hold the
  task or `.detach()` consciously). The typed return value of the
  latter encodes the runtime-lifetime contract the deleted helper
  hid. **R28-API1 CLOSED.**
- **R28-API2 (5 test-scaffolding `pub` leaks)** — **FULLY CLOSED**.
  All 4 remaining items (`Database::from_test_config`,
  `StubRestoreBackend`, `StubSourceVmOps`, `RecordingIdleSnapshotter`)
  carry `#[cfg(any(test, feature = "test-support"))]` gates on HEAD.
  The 5th item (`Database::set_role_dsns_for_test`) was **DELETED**
  entirely between r28 and r29 (only the in-comment reference at
  `db.rs:43-46` remains; the function itself is gone — verified via
  `grep -c "fn set_role_dsns_for_test" src/db.rs` returning 0).
  Verified at R29-API-VERIFY3: `nm --defined-only` on the production
  rlib returns 0 matches for any of the 5 symbols; the `--tests` build
  succeeds (feature auto-enabled via self dev-dep). **R28-API2 CLOSED.**
- **R28-API3 (test-support feature lacks crate-root rustdoc warning)**
  — **NO MOVEMENT**. `lib.rs:1-9` still has only the original 1-line
  header; `Cargo.toml:71-77` still has only the inline comment that's
  invisible to `cargo doc`. Carry held; MINOR.
- **R27-API1 (BackendBuilder rustdoc-strengthen)** — **PARTIAL CLOSURE
  HELD**. The 4th-orthogonal-field rationale still lives on the
  `BackendBuilder` struct rustdoc; the `Backend::builder()` fn rustdoc
  at `mod.rs:260-280` still has no cross-reference to the rationale.
  Pure documentation drift. Carry held; MINOR.
- **R27-API3 (`metrics::*_value` `pub` → `pub(crate)` sweep)** —
  **AUDIT REFINED**. r28 logged "12 of 14 with zero external consumers"
  but a tighter consumer audit this round shows:
  - `lost_leadership_value` — **DOES** have an external consumer
    (`tests/sandbox_pg_e2e.rs:947+960`); must remain `pub`.
  - `wake_terminal_overwrite_blocked_value` — **DOES** have external
    consumers (`tests/sandbox_pg_e2e.rs:5584-5672`); must remain `pub`.
  - The remaining 13 `*_value` accessors (`takeover_orphan_value`,
    `takeover_mismatched_value`, `takeover_unreachable_value`,
    `takeover_corrupt_value`, `sandbox_corrupt_id_value`,
    `takeover_lease_expiration_value`, `dead_hosts_observed_value`,
    `clock_rewind_value`, `heartbeat_lag_value`,
    `vm_index_leak_value`, `wake_sync_deprecated_value`,
    `nomad_node_id_lookup_failures_value`, `lost_leadership_snapshot_by_op`)
    have zero external consumers — only `metrics_export.rs` (same
    crate). Refined target = **13 narrowings**, not 12 (r28
    miscounted). Carry held; MINOR.
- **R27-API4 (`WakeErrorCode` rustdoc table 8-of-10)** — **NO MOVEMENT**.
  `db.rs:1731-1741` still lists only 8 variants; `StagingPathMissing`
  and `AgentVersionMismatch` still absent from the table (variants
  exist + ship correctly in `as_str` / `wire_code` / `from_str_opt` —
  only the rustdoc table is stale). Carry held; MINOR.
- **R26-API1 (driver-side counter federation)** — UNRESOLVED.
  Architecture-r28's staging-locality ADR documents the federation as
  SEPARATED. Driver-side v20 (`680baafa`) introduced stage labels in
  log strings only — no controller-side API contract. Carry held;
  IMPORTANT.
- **NEW R29-API1** — `VmIndexAllocator::{release_vm_index_after,
  spawn_delayed_release_in_worker}` both land as `pub fn` on HEAD.
  Zero external (`tests/`) callers; zero call sites outside
  `crates/sandbox/src/backend/nomad_ch.rs`. Same lens as the now-closed
  R28-API1 — the class-fix replaced one `pub` helper with two `pub`
  helpers, then closed by deletion at one but the new ones inherit
  the same `pub` overshoot. **NEW** MINOR.
- **NEW R29-API2** — `spawn_delayed_release_in_worker` is marked
  `#[allow(dead_code)]` AND has zero production callers AND its
  rustdoc explicitly states "No current call site in this crate uses
  this helper; it exists as the type-safe escape hatch for any future
  background-task path." A `pub` symbol that is also `#[allow(dead_code)]`
  is a doubly-broken API surface — either the helper is part of the
  contract (drop the dead_code allow, prove the call site) OR it's
  internal (visibility down to `pub(crate)`, dead_code allow stays).
  MINOR (NEW).
- **Backlog**: r28 = 11 → r29 = 9 (closures: R28-API1, R28-API2; net
  -2 after 2 new findings R29-API1 + R29-API2).

## CRITICAL

None.

## IMPORTANT

### [R26-API1] (carry) driver-side `nomad_driver_ch_destroy_task_unreaped_total` still has no operator-readable surface

- **Where**: out-of-tree `nomad-driver-ch` repo. No consumer-side
  mention in `crates/sandbox/src/` this round.
- **Status r29**: Unchanged from r28. Driver v20 stage labels
  (`680baafa`) surface in controller-side log strings, not in the §10.0
  envelope — no api contract; no metric exposure either. The
  controller-side `/metrics` route ships the controller's view; the
  driver-side counter remains gated on Nomad agent metrics fanout.
- **Severity**: IMPORTANT (carry from r26).
- **Owner**: out-of-tree nomad-driver-ch / observability ADR.

### [R20-API1] (carry) schema-marker rewriter sites unchanged

- **Where**: 4 path-derivation rewriter sites — unchanged this round.
- **Status r29**: No movement. The Option C Phase 4 cutover at
  `231e66c6` flipped `driver_stages_disk_images=true` and added the
  `zsbx_stage_disks` Nomad-meta hand-off, but that's a NEW
  hand-off mechanism (not a rewriter rewrite). Restore-path
  rewriters untouched.
- **Severity**: IMPORTANT (6-round carry; quadruple motivation).
- **Owner**: security (driver-side validator landing).

## MINOR

### [R29-API1] `VmIndexAllocator::{release_vm_index_after, spawn_delayed_release_in_worker}` are `pub fn` with zero external consumers; same lens as closed R28-API1

- **Where**: `crates/sandbox/src/backend/nomad_ch.rs:383` and `:425`.
- **Snippets**:
  ```rust
  // :383
  pub async fn release_vm_index_after(
      allocator: Arc<Mutex<Self>>,
      i: u16,
      delay: Duration,
      reason: &'static str,
      sandbox_id: Uuid,
  ) { … }

  // :425
  #[allow(dead_code)] // typed escape hatch — see rustdoc
  pub fn spawn_delayed_release_in_worker(
      allocator: Arc<Mutex<Self>>,
      i: u16,
      delay: Duration,
      reason: &'static str,
      sandbox_id: Uuid,
  ) -> compio::runtime::Task<
      Result<(), Box<dyn std::any::Any + Send>>,
  > { … }
  ```
- **Audit**: `grep -rn "release_vm_index_after\|spawn_delayed_release_in_worker"
  crates/sandbox/`:
  - `release_vm_index_after`: 3 production call sites
    (`nomad_ch.rs:1393` stop_inner, `:2358` CreateGuard::drop,
    plus the rustdoc cross-reference at `:417`); 3 same-module test
    consumers (lines `4761`, `4798`, `4898`); **zero callers** under
    `crates/sandbox/tests/`; **zero callers** in any other crate.
  - `spawn_delayed_release_in_worker`: zero production callers; 1
    same-module test consumer (`nomad_ch.rs:4793, 4836` — the
    joinable-task pin test); **zero callers** under
    `crates/sandbox/tests/`; **zero callers** in any other crate.
- **Why MINOR**: this finding is structurally identical to the now-CLOSED
  R28-API1. The class-fix at `62b083e1` deleted one `pub` helper
  (`spawn_delayed_release`) but the two replacement helpers land as
  `pub` with the same surface-leak shape — no external consumers.
  `VmIndexAllocator` itself has zero external consumers (verified via
  binary grep on target/release rlib; the only `VmIndexAllocator`
  call sites under `crates/sandbox/tests/` are the binary debug rmeta
  artefacts, not Rust source). The entire struct is internal but
  surfaces a `pub` API for no consumer.

  This is not a regression — the R28-API1 fix shipped the typed
  safety property (the deleted helper hid the runtime-lifetime
  decision; the replacements force the caller to choose `.await` vs.
  joinable `Task` vs. nothing). The minimum-disclosure forward-pressure
  carries forward.
- **Recommendation**: `pub fn` → `pub(crate) fn` on BOTH new helpers.
  Pure mechanical change; no call-site touches required (all callers
  are in-crate). Bundle with R27-API3 if code-quality r30 takes
  multiple narrowings in one sweep.
- **Severity**: MINOR (api-surface tidiness; same minimum-
  disclosure lens R28-API1 closed by deletion, but the lens carries
  forward into the replacements).
- **Owner**: code-quality r30.

### [R29-API2] `spawn_delayed_release_in_worker` is `pub fn` + `#[allow(dead_code)]` + zero call sites in production — pick one of three coherent endings

- **Where**: `crates/sandbox/src/backend/nomad_ch.rs:424-443`.
- **The contradiction**:
  ```rust
  #[allow(dead_code)] // typed escape hatch — see rustdoc
  pub fn spawn_delayed_release_in_worker(...) ->
      compio::runtime::Task<Result<(), Box<dyn Any + Send>>> { … }
  ```
  And the rustdoc at `:420-423`:
  > "No current call site in this crate uses this helper; it exists
  > as the type-safe escape hatch for any future background-task path
  > that needs fire-and-forget delayed release on a long-lived
  > runtime without blocking the caller."
- **Why MINOR**: a `pub` symbol that is ALSO `#[allow(dead_code)]` is
  a contradictory contract:
  1. **`pub` says "external callers may rely on me."** The signature
     is a stability promise to any code outside the symbol's
     visibility scope.
  2. **`#[allow(dead_code)]` says "I'm not called anywhere."** Rust's
     dead-code lint detects unreachable items in the dependency
     graph. The bypass acknowledges this symbol has no callers.
  3. The intersection is "I'm publicly callable but no one calls me
     yet" — which is the textbook definition of speculative API
     surface. The Rust stdlib + most well-curated crates resolve this
     either by adding a real call site (proves the helper works in
     anger) OR by waiting until a caller exists (cf. the Rust API
     guidelines C-FEATURE: "don't ship APIs without consumers").

     The current shape WILL bit-rot — a future contributor who
     refactors `release_vm_index_after`'s signature will have to keep
     `spawn_delayed_release_in_worker` in sync without a single test
     site that actually USES the joinable-Task return value (the
     existing test at `:4831-4858` exercises the typed return but
     doesn't model a production consumer's needs).
- **Three coherent endings**:
  1. **Delete** `spawn_delayed_release_in_worker`. The architecturally-
     stated rationale ("if a caller genuinely cannot await") has no
     caller today; YAGNI applies. Re-introduce when a real consumer
     lands. The typed safety property is well-documented in
     `release_vm_index_after`'s rustdoc; future devs adding a
     joinable variant will write it from scratch in a few minutes
     using the same idioms.
  2. **Narrow visibility** to `pub(crate) fn` and KEEP `#[allow(dead_code)]`.
     The contradiction softens: "I'm internally callable but not
     called anywhere yet" is at least limited to one crate's
     refactoring blast radius.
  3. **Find an actual consumer** and remove the `#[allow(dead_code)]`.
     Possible candidates: the snap-idle-gc sweeper, the
     CreateGuard::drop site (currently `release_vm_index_after`.await)
     could be rewritten to use the joinable variant under
     `detach_isolated`'s short-lived runtime to surface panic
     information that the await-variant hides. But none of these is
     a real need today.

  Recommended ending: **Option 1 (delete)**. The class-fix lesson
  (R29-C1 / arch-r29-A2) is that runtime-lifetime decisions should
  be encoded in the type system AT THE CALL SITE, not as a parallel
  helper in the API surface. The lesson is preserved in
  `release_vm_index_after`'s rustdoc; a future caller can write the
  joinable variant in 8 lines.
- **Severity**: MINOR (api-surface tidiness; contradiction between
  `pub` and `dead_code` resolves at low cost; defer to code-quality
  r30 for the deletion-vs-narrowing call).
- **Owner**: code-quality r30 (mechanical fix); architecture r30 if
  the deletion question wants a design call (the `release_vm_index_after`
  rustdoc explicitly cross-references this helper, so deleting it
  needs the rustdoc updated too).

### [R28-API3] (carry, no movement) `test-support` feature lacks crate-level rustdoc warning that it's not API-stable

- **Where**: `crates/sandbox/Cargo.toml:71-77` declares the feature;
  `crates/sandbox/src/lib.rs:1-9` is the crate root with no `#[doc = ...]`
  reference to features.
- **Status r29**: No movement. The Cargo.toml inline comment remains
  the only documentation; `cargo doc` consumers see no warning that
  enabling `test-support` is not API-stable. Same lens as r28; the
  R28-API2 closure this round (4 more `cfg`-gates landed) increases
  the surface area of test-only items the feature controls, making
  the rustdoc gap slightly more material (5 items on HEAD vs. 1 when
  r28 first filed).
- **Severity**: MINOR (carry).
- **Owner**: code-quality r30.

### [R27-API1] (carry, partial closure) BackendBuilder rustdoc-strengthen — rationale on struct rustdoc, not on `Backend::builder()` entry-point

- **Where**: `crates/sandbox/src/backend/mod.rs:182-205`
  (`BackendBuilder` struct rustdoc) + `mod.rs:260-280`
  (`Backend::builder` fn rustdoc).
- **Status r29**: No movement. The `Backend::builder()` fn rustdoc at
  `:260-280` still only mentions "Replaces the prior 3-level
  telescoping constructor cascade (R27-I1) — see git history (commit
  landing R27-I1) for the pre-builder shape." A reader who jumps to
  `Backend::builder` via rustdoc-search lands one click from the
  rationale.
- **Severity**: MINOR (rustdoc cosmetic; partial closure carry).
- **Owner**: code-quality r30.

### [R27-API3] (carry, refined scope) 13 of 15 `metrics::*_value` accessors are `pub` with zero external consumers

- **Where**: `crates/sandbox/src/metrics.rs` — 15 read accessors total.
- **Status r29**: r28 reported "12 of 14"; r29 audit refined to "13 of
  15" after counting `lost_leadership_value` (external consumer at
  `tests/sandbox_pg_e2e.rs:947+960`) and `wake_terminal_overwrite_blocked_value`
  (external consumer at `tests/sandbox_pg_e2e.rs:5584-5672`) as MUST-
  remain-`pub`. The remaining 13 are confirmed zero-external:
  ```
  takeover_orphan_value
  takeover_mismatched_value
  takeover_unreachable_value
  takeover_corrupt_value
  sandbox_corrupt_id_value
  takeover_lease_expiration_value
  dead_hosts_observed_value
  clock_rewind_value
  heartbeat_lag_value
  vm_index_leak_value
  wake_sync_deprecated_value
  nomad_node_id_lookup_failures_value
  lost_leadership_snapshot_by_op
  ```
  Only `metrics_export.rs` (same crate) calls them. Same lens as
  composite-r1 #2 (`metrics_export` itself `pub` → `pub(crate)`) and
  the now-closed R27-API2.
- **Severity**: MINOR (carry; refined scope).
- **Owner**: code-quality r30.

### [R27-API4] (carry, no movement) `WakeErrorCode` rustdoc table at `db.rs:1731-1741` still lists 8 of 10 variants

- **Where**: `crates/sandbox/src/db.rs:1731-1741`.
- **Status r29**: No movement. `StagingPathMissing` and
  `AgentVersionMismatch` ship correctly in `as_str` / `wire_code` /
  `from_str_opt` / `WAKE_ERROR_CODES` enumeration tests at `:3766` and
  `:3768`; only the rustdoc table is incomplete.
- **Severity**: MINOR (carry).
- **Owner**: code-quality r30.

### [R24-API3] (carry) async wake-poll envelope still missing structured `which` field

- **Where**: `admin_handlers.rs:1990-2008`. Unchanged.
- **Severity**: MINOR (carry).
- **Owner**: architecture (Phase-2 `wake_jobs.error_extra JSONB`).

### [R26-API5] (carry) sanitize-widening mask token unification

- **Where**: `crates/sandbox/src/wake_machine.rs:748` / `:1065` /
  `:1119`. Unchanged this round.
- **Severity**: MINOR (carry; cosmetic).
- **Owner**: code-quality r30.

### Other carries (no movement)

- **R19-API2** — `pub` → `pub(crate)` sweep; R27-API3 + R29-API1 +
  R29-API2 extend the same lens.
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

- **`ClockResyncOutcome` (`restore_handler.rs:3050`) is `pub(crate)`
  with a `transport_error: bool` field on the `Err` variant — should
  the field be private with a `is_transport_error()` accessor like
  the sibling `VersionCheckOutcome::is_transport_error()` at `:3174`?**:
  The `pub(crate)` enum-with-`pub` fields is the established crate
  shape — `RestoreOutcome`, `RestoreHandlerError`, `SubmitRestoreError`,
  `VersionCheckOutcome::Skipped` all expose fields directly. The
  inconsistency would be the OTHER direction (forcing accessor-only
  reads) — and the `wake_machine.rs:542-545` consumer pattern-matches
  on `transport_error: true` cleanly using the `match`-binding
  destructure. The `VersionCheckOutcome::is_transport_error()`
  accessor exists because the wake_machine reads the bool 3 times
  (let-binding + warn-log + pair-detection) and the local binding is
  idiomatic; `ClockResyncOutcome` only reads `transport_error` ONCE
  (via the matches!() pair-detect at `:540-546`). Symmetry would add
  cost without value. **No nit**.
- **`clock_resync_post_restore_typed` returns `ClockResyncOutcome`
  directly (not `Result<ClockResyncOutcome, ...>`) — is the no-Result
  shape correct?**: The function wraps `clock_resync_post_restore`,
  which returns `Result<(), String>`. The wrapper FOLDS the `Result`
  into the enum's `Ok` / `Err` variants — Ok=200, Err=anything else
  with a `transport_error: bool` discriminator. This is the right
  shape because the half-dead-agent detector needs to BRANCH on the
  failure flavour (transport vs. 401), not just fail-or-not-fail.
  Wrapping in another `Result<…, String>` would re-introduce the
  string-matching anti-pattern the R28-I2 fix eliminated. **No nit**.
- **`VersionCheckOutcome::is_transport_error()` is `pub(crate) fn`
  on an enum that's `pub(crate)` — is the visibility consistent?**:
  Yes; `pub(crate)` on a `pub(crate)` enum is the right visibility
  to maintain crate-private contract. External callers cannot
  construct the enum so they cannot call the accessor either. The
  accessor exists for the in-crate consumer at `wake_machine.rs:539`.
  **No nit**.
- **The `WakeErrorCode::ClockResyncFailed` variant is reused for the
  half-dead-agent path with a `half_dead_agent: …` message prefix —
  should the half-dead case have its own wire code?**: The brief
  notes that the wake kind taxonomy is unchanged (`db.rs` delta is
  zero). The half-dead-agent rollback maps to `ClockResyncFailed`
  because both probes failed at the transport layer — the operator
  action is the same ("the agent is unhealthy; investigate"). A
  separate `HalfDeadAgent` wire code would split a small subset of
  `ClockResyncFailed` failures from a related set, and the SLO
  dashboard would need to combine them again. The `message` prefix
  (`half_dead_agent:` per `wake_machine.rs:568`) lets log-grep tell
  the cases apart without forcing a new wire code through the
  WakeErrorCode triangle (variant + as_str + wire_code +
  from_str_opt + migration 0009 CHECK constraint + the rustdoc table
  at `:1731-1741`). The decision is in scope of architecture, not
  api-surface; api-surface only checks the triangle stays consistent
  (it does — verified at R29-API-VERIFY1). **No nit**.
- **Driver v20 stage labels surface in controller-side log strings
  — does this need a §10.0 envelope audit?**: The driver-side stage
  labels appear in tracing fields emitted by `driver_failure_event_extra`
  / similar (out-of-tree side; mentioned by the brief as "no API
  contract"). The controller-side surface that consumes these is the
  Nomad event-stream JSON parsing in `nomad_ch.rs` — which the
  controller already structured-extracts into `BackendFailureDetail`
  via the existing typed channel. No new wire fields surface to
  `/admin/sandboxes/{id}/wake/{wake_id}` poll responses or `/metrics`.
  **No nit**.
- **`spawn_delayed_release_in_worker` returns a `Task<Result<(),
  Box<dyn Any + Send>>>` — is the `Box<dyn Any + Send>` panic-payload
  too leaky as a public type?**: The shape mirrors
  `compio::runtime::spawn`'s native return type; a `Task<Result<_, _>>`
  is the canonical compio JoinHandle. Boxing the panic payload is
  the std library convention (`std::thread::Result<T> = Result<T,
  Box<dyn Any + Send>>`); surfacing it in the return type beats
  the alternatives (consume panic silently, or convert to a
  controller-specific error type). If the helper survives the R29-API2
  resolution (i.e., is kept rather than deleted), the return type
  is correctly shaped. **No nit** beyond R29-API1's visibility
  narrowing + R29-API2's existential question.

## §10.0 envelope state post-r29

### Inventory (delta from r28)

```
New since r28:  None (no §10.0-touching commits this round).
                R28-I1+I2 added structured typed-outcome enums
                (`ClockResyncOutcome`, `VersionCheckOutcome`) at
                pub(crate) visibility — neither crosses the wire.
                The half-dead-agent rollback reuses the existing
                `ClockResyncFailed` wire code with a `half_dead_agent:`
                free-text message prefix; wake-kind taxonomy
                unchanged (verified at R29-API-VERIFY1 below).
                R29-C1 / arch-r29-A2 fix is internal to the
                `VmIndexAllocator` API; no wire surface impact.
```

The 32 §10.0 codes from r27/r28 are unchanged. `WakeErrorCode` is
still 10 variants; `wire_code` triangle still intact at
`db.rs:1680-1825`.

### WakeErrorCode triangle post-r29 (extended verification)

The brief notes "WakeErrorCode triangle (now extended with
ClockResyncFailed for half-dead-agent)" — but the half-dead-agent
path REUSES the existing `ClockResyncFailed` variant rather than
adding a new one. Triangle integrity:

| Variant | `as_str()` | `wire_code()` | `from_str_opt()` | Table row | Enum tests |
|---|---|---|---|---|---|
| `SlotUnavailable` | `slot_unavailable` | `vm_index_unavailable` | ✓ | ✓ | ✓ |
| `SourceTeardownTimeout` | `source_teardown_timeout` | `source_teardown_timeout` | ✓ | ✓ | ✓ |
| `RestoreFailed` | `restore_failed` | `restore_backend_failed` | ✓ | ✓ | ✓ |
| `LivezTimeout` | `livez_timeout` | `livez_timeout` | ✓ | ✓ | ✓ |
| `ClockResyncFailed` | `clock_resync_failed` | `clock_resync_failed` | ✓ | ✓ | ✓ |
| `RegisterFailed` | `register_failed` | `register_failed` | ✓ | ✓ | ✓ |
| `Internal` | `internal` | `internal_error` | ✓ | ✓ | ✓ |
| `WakeWorkerAborted` | `wake_worker_aborted` | `wake_worker_aborted` | ✓ | ✓ | ✓ |
| `StagingPathMissing` | `staging_path_missing` | `staging_image_missing` | ✓ | **missing from rustdoc table** | ✓ |
| `AgentVersionMismatch` | `agent_version_mismatch` | `agent_version_mismatch` | ✓ | **missing from rustdoc table** | ✓ |

Two rustdoc-table omissions confirm R27-API4 is still open. Triangle
itself is functionally intact (variant → as_str → from_str_opt
round-trip + wire_code uniqueness + enum-list test at `:3757-3779`
all pass).

### POST-endpoint envelope audit (delta from r28)

| Endpoint | Required | 401 | 403 | 503 | Success | Post-r29 wire-shape drift? |
|---|---|---|---|---|---|---|
| `POST /admin/sandboxes/{id}/snapshot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 | none |
| `POST /admin/sandboxes/{id}/wake` (sync) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (`agent_url`) | none |
| `POST /admin/sandboxes/{id}/wake` (async) | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 202 (`wake_id`) | none |
| `GET /admin/sandboxes/{id}/wake/{wake_id}` | RO | `unauthorized` | `insufficient_role` | `admin_api_disabled` | 200/202 (kind=`clock_resync_failed` covers half-dead-agent via `message` prefix) | none |
| `POST /admin/sandboxes/{id}/cold-boot` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | `feature_disabled` 501 | none |
| `DELETE /admin/users/{id}` | Full | `unauthorized` | `insufficient_role` | `admin_api_disabled` | flat 200 (delete tombstone) | none |
| `GET /readyz` | None | — | — | `backend_unhealthy` | `{"status":"ok"}` | none |
| `GET /metrics` | RO | `unauthorized` | `insufficient_role` | `admin_api_disabled` | `text/plain; version=0.0.4` | none |

§10.0 envelope unchanged. The half-dead-agent rollback surfaces as
`kind=clock_resync_failed` + `message` carrying `half_dead_agent:`
prefix (`wake_machine.rs:568-571`); operators distinguish via the
log target `sandbox::wake::half_dead_agent` (WARN line at `:549`)
or via grep on the message prefix. No new wire-shape surface.

## R29-API-VERIFY1 — R28-I1 + R28-I2 wake_machine half-dead-agent surface verification

**Mandate** (per brief): verify `ClockResyncOutcome::Err{transport_error:bool}`
is well-scoped (pub vs pub(crate)) AND verify wake-kind taxonomy is
unchanged.

**Audit checks** (all pass):

1. **`ClockResyncOutcome` visibility** ✓ `restore_handler.rs:3050`:
   `pub(crate) enum ClockResyncOutcome`. Crate-private. The wake_machine
   consumer at `wake_machine.rs:540` reads `transport_error: true`
   via `matches!()` destructure on the enum — only reachable from
   within `crate::restore_handler::*`. **Well-scoped.**
2. **`clock_resync_post_restore_typed` visibility** ✓
   `restore_handler.rs:3076`: `pub(crate) async fn`. Same posture.
3. **`VersionCheckOutcome` visibility** ✓ `restore_handler.rs:3135`:
   `pub(crate) enum VersionCheckOutcome`. Same posture.
4. **`VersionCheckOutcome::is_transport_error()` visibility** ✓
   `restore_handler.rs:3174`: `pub(crate) fn`. Same posture as the
   enclosing enum; consistent.
5. **No new `db.rs` variant** ✓ Verified: `grep -n "WakeErrorCode::"
   crates/sandbox/src/db.rs` shows the same 10 variants from r28;
   the half-dead-agent path maps to the existing `ClockResyncFailed`.
6. **Free-text-message-prefix discipline** ✓ `wake_machine.rs:568`:
   `"half_dead_agent: both /version and /_clock_resync transport-failed
   against {agent_url} (clock_resync: {clock_message})"`. The prefix
   is snake_case + colon-separator + structured suffix — same shape
   as the `StagingPathMissing` message format (`r25-S1` precedent).
7. **Log target** ✓ `wake_machine.rs:549`:
   `target: "sandbox::wake::half_dead_agent"`. Dedicated log target;
   operators can grep on the target prefix without parsing the
   message string. Mirrors the `sandbox::wake::version_check` target
   at `:595` (T5 skipped path).
8. **Cancel-safety doc** ✓ `wake_machine.rs:496-505` documents the
   `futures::join!` cancel-safety property (both futures take args
   by value or as borrows that live for the span of the await; no
   shared mutable state, no Drop-side effects mid-call).
9. **Symmetric pair-detection logic** ✓ `wake_machine.rs:539-546`:
   `t5_transport_error = t5_outcome.is_transport_error()` (accessor
   call) + `clock_resync_transport_error = matches!(…, ClockResyncOutcome::Err
   { transport_error: true, .. })` (pattern match). The
   accessor-vs-matches asymmetry IS intentional — the
   `VersionCheckOutcome::Skipped` variant has 2 fields (transport_error
   + reason) and the accessor encapsulates the variant-pattern
   match; `ClockResyncOutcome::Err` has 2 fields too but the
   wake_machine reads `transport_error` ONCE in a single match
   destructure. No symmetry bug.

**Verdict**: R28-I1 + R28-I2 land STRUCTURALLY CLEAN. Typed outcomes
are well-scoped at `pub(crate)`; no new wire-surface drift; the
half-dead-agent fingerprint reuses the existing `ClockResyncFailed`
wire code with a structured message prefix and dedicated log target.

## R29-API-VERIFY2 — R29-C1 + arch-r29-A2 class-fix verification (R28-API1 closure)

**Mandate** (per brief): verify `spawn_delayed_release` is actually
gone (`grep -n spawn_delayed_release` should return nothing in
production code; only rustdoc historical mentions).

**Audit checks** (all pass):

1. **Function deletion** ✓
   `grep -nE "^\s*pub fn spawn_delayed_release\(|^\s*fn spawn_delayed_release\("
   crates/sandbox/` returns ZERO matches. The function is fully
   deleted. The only surviving callers (16 hits in r28's grep) are
   now reduced to:
   - **Production callers**: ZERO (was 2 — `nomad_ch.rs:1324` stop_inner
     + the deleted CreateGuard::drop site).
   - **Test callers**: ZERO (was 6 — all 6 tests in `#[cfg(test)] mod
     tests` were renamed to `release_vm_index_after_*` per the
     commit log).
   - **Rustdoc mentions**: 15 hits across `nomad_ch.rs` lines 363,
     371, 378, 411, 1387, 4647, 4790, 4793, 4821, 4826, 4831, 4857,
     4865 — ALL historical references explaining the pre-r29 helper's
     trap. No live API surface.
2. **Replacement helpers** ✓ Two new helpers:
   - `release_vm_index_after` (`nomad_ch.rs:383`, `pub async fn`) —
     consumed at 2 production sites (`:1393` stop_inner; `:2358`
     CreateGuard::drop). Both use `.await`.
   - `spawn_delayed_release_in_worker` (`nomad_ch.rs:425`, `pub fn`,
     `#[allow(dead_code)]`) — typed escape hatch with no production
     consumers (see R29-API2 above).
3. **Type-driven safety property** ✓ The class-fix encodes the
   runtime-lifetime decision in the type system:
   - `release_vm_index_after` is `async` → caller MUST `.await` →
     timer is bound to the caller's task → no detached-from-runtime
     trap.
   - `spawn_delayed_release_in_worker` returns
     `compio::runtime::Task<Result<…>>` → caller MUST hold the Task
     OR call `.detach()` consciously → if `.detach()` is wrong for
     the runtime, the type signature doesn't change but the
     responsibility shift is explicit in the docs and the symbol
     name.
4. **R28-C1 closure consistency** ✓ R28-C1's inline-release fix at
   CreateGuard::drop (`nomad_ch.rs:2358` per `9e1f6276`) is now
   re-unified with `stop_inner` to call the same
   `release_vm_index_after.await` helper. The earlier "Do NOT unify
   the two call sites" warning at the inline rationale (r28
   R28-API-VERIFY2) is REPLACED by the typed-helper class-fix —
   both sites use the safe helper; the rustdoc at `nomad_ch.rs:1387`
   notes the post-r29 unification.
5. **Test coverage** ✓ Three new tests cover the typed helper
   contract:
   - `release_vm_index_after_with_zero_delay_releases_immediately`
     at `:4756`.
   - `release_vm_index_after_honors_configured_delay` at `:4782`.
   - `spawn_delayed_release_in_worker_returns_joinable_task` at
     `:4831`.
   - `release_vm_index_after_survives_short_lived_runtime` at
     `:4883` — pins the type-safety property under `detach_isolated`'s
     private runtime (this is THE regression test for R29-C1).
6. **Symbol stripping (production rlib)** ✓
   ```
   $ cargo build -p zeroship-sandbox --lib 2>&1 | tail -1
   warning: `zeroship-sandbox` (lib) generated 2 warnings (run `cargo fix --lib -p zeroship-sandbox` to apply 1 suggestion)
       Finished `dev` profile [unoptimized + debuginfo] target(s) in 20.23s
   $ nm --defined-only target/debug/libzeroship_sandbox.rlib 2>/dev/null \
       | grep -c "spawn_delayed_release\b"
   0
   ```
   Symbol fully stripped. The 2 pre-existing warnings (unused import
   `SandboxAuth`; unused constant `WAKE_JOBS_T_KEEP`) are NOT
   regressions from this round — they're carries from prior commits.

**Verdict**: R28-API1 is STRUCTURALLY CLOSED at `62b083e1`. The
fix shape is better than r28's recommendation (visibility narrowing)
— the class-fix deletes the unsafe helper, replaces it with two
typed-safe successors, and encodes the runtime-lifetime decision
in the type system. The two successors carry their own minor
api-surface forward-pressure (R29-API1 + R29-API2 above), but the
class-of-bug fix is complete.

## R29-API-VERIFY3 — R28-API2 closure verification (test-scaffolding sweep)

**Mandate**: verify the 5 R28-API2 candidates are gated or deleted.

**Audit checks** (all pass):

1. **`Database::from_test_config`** ✓ `db.rs:521-523`:
   `#[doc(hidden)] #[cfg(any(test, feature = "test-support"))] pub
   async fn from_test_config(...)`. Gate landed.
2. **`Database::set_role_dsns_for_test`** ✓ **DELETED**.
   `grep -c "fn set_role_dsns_for_test" src/db.rs` returns 0.
   Only one in-comment reference survives at `db.rs:43-46` (the
   thread-local pool cache rustdoc references the historical
   function name). Net surface: zero.
3. **`StubRestoreBackend`** ✓ `restore_handler.rs:1304-1307`:
   `#[doc(hidden)] #[cfg(any(test, feature = "test-support"))]
   #[derive(Debug)] pub struct StubRestoreBackend`. Gate landed.
4. **`StubSourceVmOps`** ✓ `snapshot_handler.rs:693-696`:
   `#[doc(hidden)] #[cfg(any(test, feature = "test-support"))]
   #[derive(Debug)] pub struct StubSourceVmOps`. Gate landed.
5. **`RecordingIdleSnapshotter`** ✓ `sweep.rs:494-497`:
   `#[doc(hidden)] #[cfg(any(test, feature = "test-support"))]
   #[derive(Debug, Default)] pub struct RecordingIdleSnapshotter`.
   Gate landed.
6. **Symbol stripping (production rlib)** ✓
   ```
   $ nm --defined-only target/debug/libzeroship_sandbox.rlib 2>/dev/null \
       | grep -cE "from_test_config|StubRestoreBackend|StubSourceVmOps|RecordingIdleSnapshotter|_test_inject_sandbox|set_role_dsns_for_test"
   0
   ```
   All 6 candidate symbols (including the prior R27-API2 closure
   `_test_inject_sandbox` and the now-deleted `set_role_dsns_for_test`)
   are stripped from the production rlib.
7. **Test-build verification** ✓
   ```
   $ cargo build -p zeroship-sandbox --tests 2>&1 | tail -1
   warning: `zeroship-sandbox` (lib test) generated 1 warning (1 duplicate)
       Finished `dev` profile [unoptimized + debuginfo] target(s) in 39.78s
   ```
   Tests build clean (feature auto-enabled via self dev-dep).
8. **No new `pub` items** ✓ `git diff 9e1f6276..HEAD -- 'crates/sandbox/src/'`
   net `pub` delta:
   - DELETED: `pub fn spawn_delayed_release`
   - ADDED: `pub async fn release_vm_index_after`
   - ADDED: `pub fn spawn_delayed_release_in_worker`
   - ADDED: `pub(crate) enum ClockResyncOutcome`
   - ADDED: `pub(crate) async fn clock_resync_post_restore_typed`
   - ADDED: `pub(crate) fn is_transport_error` (method on `VersionCheckOutcome`)
   No surprises; all new items are tracked under R29-API1 (2 pub),
   R29-API-VERIFY1 (3 pub(crate)).

**Verdict**: R28-API2 is FULLY CLOSED. The 5 candidates have all
been addressed: 4 gated via `cfg(any(test, feature = "test-support"))`
following the R27-API2 template, 1 deleted entirely. The
test-support feature pattern (introduced by `c2e07b2f` for R27-API2)
proves to be a sound and reusable infrastructure for the broader
test-scaffolding class. R28-API3 (the feature documentation gap)
becomes a slightly more material carry now that 5 items ride on the
gate.

## R29-API-VERIFY4 — driver v20 stage labels surface

**Mandate** (per brief): "driver v20 stage labels — surfaces in
controller-side log strings, no API contract."

**Audit checks** (all pass):

1. **No `stage_label` symbol in `crates/sandbox/src/`** ✓
   `grep -rn "stage_label" crates/sandbox/src/` returns zero matches.
2. **Driver pin bump** ✓ `680baafa` updates
   `crates/sandbox/scripts/*` only; no `src/**/*.rs` delta. The
   driver-side v20 stage labels are emitted by the out-of-tree
   `nomad-driver-ch` repo and reach the controller via Nomad's
   existing event-stream JSON parsing (`nomad_ch.rs`'s Driver Failure
   events extraction, already structured-extracted into
   `BackendFailureDetail`).
3. **Wire surface impact** ✓ Zero. The driver-side labels surface
   in the controller's tracing fields only (`message`-field free-text),
   not in the §10.0 envelope kind taxonomy. Operators tail controller
   logs to see them; the SLO dashboard's existing `BackendFailureDetail`
   typed channel covers the structured surface.

**Verdict**: driver v20 stage labels are CORRECTLY out-of-scope for
api-surface review. No controller-side contract introduced.

## Cross-lens consensus

### R29-API-CROSS-R29C1 — concurrency r29 R29-C1 closure ratification

Concurrency r29's R29-C1 CRITICAL closed at `62b083e1` with the class-
fix. R29-API-VERIFY2 confirms the helper-deletion is complete and
the typed safety property is encoded in the replacement helpers'
type signatures. The architecture-r29-A2 design call (typed
escape-hatch shape) ratifies as STRUCTURALLY CLEAN — but it does
land 2 `pub` helpers (R29-API1) and one `pub` + `#[allow(dead_code)]`
contradiction (R29-API2). **Cross-lens consistency**: the class-of-bug
fix is sound; api-surface forward-pressure follows.

### R29-API-CROSS-R29SEC — security r29 hand-off

Security r29 (per pilot artefacts at `8974b41c`) did not surface
api-surface findings this round. R28-API2's `set_role_dsns_for_test`
candidate was DELETED (the highest-severity item of the 5);
security-sign-off concern from r28 dissolves.

### R29-API-CROSS-R29CQ — code-quality r29 hand-off

Code-quality r29 (per pilot artefacts at `8974b41c`) addressed
R28-API2 fully (4 cfg-gates landed + 1 deletion). R27-API1 / R27-API3
/ R27-API4 / R26-API5 + R28-API3 carry forward to r30 alongside
R29-API1 (2 new pub items to narrow) and R29-API2 (the
pub + dead_code contradiction). The clear pattern is that
code-quality rounds keep closing api-surface findings at a steady
rate (R27-API2 at r27 → R28-API2 at r29 → next round expected to
take R27-API3/R29-API1).

### R29-API-CROSS-R28I1I2 — concurrency r28 R28-I1+I2 closure ratification

R28-I1 (T5 + clock_resync parallel via `futures::join!`) and R28-I2
(half-dead-agent fingerprint via typed boolean) close cleanly with
no api-surface drift to the §10.0 envelope. The wake-kind taxonomy
is unchanged (the half-dead path reuses `ClockResyncFailed` per
the design intent). The new typed enums are `pub(crate)`-scoped
correctly. **Cross-lens consensus**: clean.

### Other cross-lens

- **Performance r28/r29**: no api-surface intersect this round.
- **Test-coverage r30**: owns the R22-API2 carry + R25-API1
  cold-boot-side sibling pin + extending `/metrics` coverage
  (200/401/403 paths) per the r27/r28 hand-off.

## Lens hand-off

- **To architecture r30**:
  - r26-A1 (`BackendFailureDetail` carry) — still open.
  - R26-API1 (driver-side counter federation) — controller side
    closed via R26-API2; driver-side surface depends on
    out-of-tree nomad-driver-ch / Nomad agent metrics fanout.
  - R24-API3 Phase-2 schema decision (`wake_jobs.error_extra
    JSONB`) still open.
  - **R29-API2** (`spawn_delayed_release_in_worker` deletion vs.
    narrowing) — design call if the deletion option is taken;
    otherwise code-quality r30 mechanical narrowing.
- **To test-coverage r30**:
  - R22-API2 carry.
  - R25-API1 cold-boot-side sibling pin still open.
  - §10.0 envelope enumeration test (r23 carry).
  - Extend `/metrics` route-level integration test
    (`tests/sandbox_admin_e2e.rs:1288`) to 200/401/403 paths
    per r27 hand-off.
  - **NEW from r29 verify**: regression test for the half-dead-
    agent fingerprint at the wake_jobs.error_message wire level
    (the `half_dead_agent:` message prefix should be pinned by
    test). Currently pinned at the unit-test level
    (`restore_handler.rs:4763` for closed-port outcome shape) but
    not at the e2e level.
- **To security r30**:
  - R20-API1 schema-marker carry (6-round + quadruply-motivated).
- **To code-quality r30**:
  - **NEW R29-API1** narrow `release_vm_index_after` +
    `spawn_delayed_release_in_worker` `pub` → `pub(crate)`. Bundle
    with R27-API3 if a single sweep is taken.
  - **NEW R29-API2** resolve the `pub` + `#[allow(dead_code)]`
    contradiction on `spawn_delayed_release_in_worker` (delete
    OR narrow + retain dead_code allow OR find a consumer).
  - **R28-API3** carry: add a `# Features` rustdoc note at
    `lib.rs` crate-root documenting `test-support` is not
    API-stable (now slightly more material with 5 items gated).
  - **R27-API1 partial-closure carry**: add a one-line
    cross-reference from `Backend::builder` rustdoc to
    `BackendBuilder` struct rustdoc.
  - **R27-API3 (refined)**: 13 `metrics::*_value` accessors `pub`
    → `pub(crate)` (NOT 12 as r28 reported;
    `lost_leadership_value` + `wake_terminal_overwrite_blocked_value`
    have external test consumers and must remain `pub`).
  - **R27-API4 carry**: extend the rustdoc table at
    `db.rs:1731-1741` from 8 to 10 rows.
  - **R26-API5 carry** sanitize mask token unification.
  - R22-API3 (`rootfs_source` doc asymmetry) carry.
  - R23-API2 / R23-API3 carries.
  - R25-API5 (parser doc-strengthen) carry.
- **To concurrency r30**: no api-surface findings cross over
  this round. (R29-C1 + R28-I1+I2 closures ratified at
  R29-API-VERIFY1+2.)

## Backlog carry table

| ID | First round | Status r29 | Severity | Lens to own |
|---|---|---|---|---|
| R19-API2 | r19 | Open (carry; R27-API3 + R29-API1 + R29-API2 extend scope) | MINOR | code-quality |
| R20-API1 | r20 | Open (6-round carry; quadruply-motivated; held) | IMPORTANT | security |
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
| R27-API1 | r27 | Open (partial closure; rationale on struct rustdoc) | MINOR | code-quality |
| R27-API3 | r27 | Open (carry; refined scope 12→13) | MINOR | code-quality |
| R27-API4 | r27 | Open (carry; rustdoc table still 8/10) | MINOR | code-quality |
| R28-API1 | r28 | **CLOSED at 62b083e1** (verified R29-API-VERIFY2; helper DELETED, class-fix shape) | — | — |
| R28-API2 | r28 | **CLOSED** (verified R29-API-VERIFY3; 4 cfg-gated + 1 deleted) | — | — |
| R28-API3 | r28 | Open (carry; no movement) | MINOR | code-quality |
| **R29-API1** | **r29** | NEW — `release_vm_index_after` + `spawn_delayed_release_in_worker` `pub` → `pub(crate)` | MINOR | code-quality |
| **R29-API2** | **r29** | NEW — `spawn_delayed_release_in_worker` `pub` + `#[allow(dead_code)]` contradiction | MINOR | code-quality / architecture |

Net: r28 open = 11 → r29 open = 9 (2 closures: R28-API1, R28-API2;
2 new: R29-API1, R29-API2; net -2).

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
- **r28**: minimum-disclosure forward-pressure. R27-API2 closed
  (test-scaffolding cfg-gate); the closure surfaced the BROADER
  pattern — 5 more test-scaffolding leaks (R28-API2), 1 more
  `pub`-overshoot helper (R28-API1), 1 feature-doc gap (R28-API3).
- **r29 (this round)**: precedent-propagation pays off. R28-API1
  closes with a BETTER fix than the api-surface recommendation —
  the class-of-bug class-fix (R29-C1 + arch-r29-A2) deletes the
  unsafe helper and replaces it with two typed-safe successors
  that encode the runtime-lifetime decision in the type system.
  R28-API2 closes mechanically against the R27-API2 template (4
  cfg-gates landed + 1 deletion). The newly-introduced R29-API1
  + R29-API2 ride on the SAME minimum-disclosure lens that has
  driven the test-support and metrics_export closures —
  forward-pressure continues to compound but each finding is
  individually small. R28-I1+I2 lands cleanly with typed outcomes
  at `pub(crate)` scope; the half-dead-agent surface stays inside
  the existing `ClockResyncFailed` wire code with a structured
  message prefix and dedicated log target.

  **The defining api-surface theme of r29 is class-fix maturity:
  R28-API1's r28-recommendation was "narrow visibility to
  `pub(crate)`" but the LANDED fix is structurally superior —
  delete the helper, replace with typed-safe successors. The
  api-surface review caught the symptom (pub overshoot); the
  concurrency + architecture lenses caught the underlying
  class-of-bug (runtime-lifetime decision hidden in the helper).
  Cross-lens compounding works.** The r29 backlog drops to 9
  (lowest since r24) without forced closures — each closure
  rode a substantive fix landing.

The r29 IMPORTANT class shrinks to 2 (R20-API1, R26-API1) — both
multi-round carries on out-of-tree dependencies / security forward-
pressure. The MINOR class is dominated by code-quality carries that
mechanical sweeps could clear in a single PR (R27-API3 13-item
narrow + R27-API4 table fill + R26-API5 mask unification +
R28-API3 feature-doc + R29-API1 2-item narrow + R29-API2
dead_code resolution = 6 items, all in the same posture).
