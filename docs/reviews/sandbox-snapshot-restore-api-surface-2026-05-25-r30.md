# Sandbox/snapshot-restore — api-surface r30 review

Date: 2026-05-25 (UTC). HEAD at audit: `ba1df3f0`.
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.
Read-only. Round 48 of pilot-cron loop. Prior api-surface review: r29
(`e66d5efb`), cycle 43.

Landed since r29 (filtered to api-surface impact):

- `9ac5b850` — R28-API2 sweep (5 test-pub items gated). Already closed
  at r29 via R29-API-VERIFY3. Carries no r30 delta.
- `81b6e689` — snap-idle-gc parallelization (R29-P1). No new pub items;
  out of api-surface scope.
- `ade8fb46` — r30-A1: `NomadStopPermits` type + `nomad_stop_permits`
  on `AppState`. **Introduces 3 new public structs
  (`NomadStopPermits`, `NomadStopPermitGuard`) and 7 new pub methods.**
  Audited at **R30-API-VERIFY1** below.
- `cdcd670d` — T-7 + T-8 cutover: delete `TaskDriverMode`,
  `task_driver_mode_from_env`, `NomadCHConfig::wrapper_path`.
  **Shrinks the public surface.** Orphan-reference audit at
  **R30-API-VERIFY2** below.
- `c3670845` — scripts cleanup (wrapper gs_pull + env vars removed
  from `gcp-worker-startup.sh`). No `src/**/*.rs` delta; out of
  api-surface scope for Rust symbols. One docs impact noted at
  **R30-API-NEW3** below.

## Summary

- **R29-API1 (`release_vm_index_after` + `spawn_delayed_release_in_worker`
  `pub` → `pub(crate)`)** — **NO MOVEMENT**. Both helpers remain `pub`
  on HEAD with zero external-crate consumers (confirmed: no external
  crate depends on `zeroship-sandbox`). Carry held; MINOR.
- **R29-API2 (`spawn_delayed_release_in_worker` `pub` + `#[allow(dead_code)]`
  contradiction)** — **NO MOVEMENT**. The helper retains its
  `#[allow(dead_code)]` allow at `nomad_ch.rs:436` with zero production
  call sites. Carry held; MINOR.
- **R28-API3 (test-support feature lacks crate-root rustdoc warning)**
  — **NO MOVEMENT**. `lib.rs:1-9` still has no `# Features` section.
  Carry held; MINOR.
- **R27-API1 (BackendBuilder rustdoc cross-reference from
  `Backend::builder` fn)** — **NO MOVEMENT**. `mod.rs:260-264` fn
  rustdoc still has no cross-reference link to `BackendBuilder` struct
  rationale. Carry held; MINOR.
- **R27-API3 (13 of 15 `metrics::*_value` accessors `pub` with zero
  external consumers)** — **EXPANDED**. The r30-A1 landing adds 2
  more `pub` metrics functions (`nomad_stop_permits_total_value`,
  `nomad_stop_permits_in_use_value`) that have zero external test
  consumers (only `metrics_export.rs` in-crate). Revised target = **15
  narrowings**, not 13. Carry held; MINOR. See **R30-API-VERIFY1** §4.
- **R27-API4 (`WakeErrorCode` rustdoc table 8-of-10)** — **NO MOVEMENT**.
  `db.rs:1731-1741` still missing `StagingPathMissing` and
  `AgentVersionMissing`. Carry held; MINOR.
- **R26-API1 (driver-side counter federation)** — **NO MOVEMENT**. Carry
  held; IMPORTANT.
- **R20-API1 (schema-marker rewriter)** — **NO MOVEMENT**. Carry held;
  IMPORTANT.
- **NEW R30-API1** — `NomadStopPermits` (struct + 4 methods) and
  `NomadStopPermitGuard` (struct) land as `pub` in `nomad_ch.rs` with
  zero external-crate consumers. Minimum-disclosure lens: `pub(crate)`
  is correct. See below; MINOR.
- **NEW R30-API2** — `metrics::inc_nomad_stop_permits_in_use` and
  `metrics::dec_nomad_stop_permits_in_use` are `pub fn` but are only
  called from within `nomad_ch.rs` (acquire + guard Drop). Same lens
  as the R27-API3 `*_value` sweep (13 → 15 narrowings target). MINOR.
- **NEW R30-API3** — `docs/runbooks/sandbox-nomad-ch.md` still
  describes the pre-T-8 `raw_exec` + wrapper-path architecture at
  lines 3, 7, 30, 43, 53, 83, 96, 100, and table row
  `SANDBOX_NOMAD_CH_WRAPPER_PATH`. After `cdcd670d` the wrapper is
  fully deleted and `SANDBOX_NOMAD_CH_WRAPPER_PATH` no longer exists;
  an operator following the runbook would install a wrapper that the
  controller never invokes. API-surface scope: the runbook IS the
  operator contract for the `nomad-ch` backend. MINOR (docs).
- **Backlog**: r29 = 9 → r30 = 11 (0 closures, 3 new findings
  R30-API1/2/3; net +2).

## CRITICAL

None.

## IMPORTANT

### [R26-API1] (carry) driver-side `nomad_driver_ch_destroy_task_unreaped_total` still has no operator-readable surface

- **Where**: out-of-tree `nomad-driver-ch` repo. No consumer-side
  mention in `crates/sandbox/src/` this round.
- **Status r30**: Unchanged from r29.
- **Severity**: IMPORTANT (carry from r26).
- **Owner**: out-of-tree nomad-driver-ch / observability ADR.

### [R20-API1] (carry) schema-marker rewriter sites unchanged

- **Where**: 4 path-derivation rewriter sites — unchanged this round.
- **Status r30**: No movement. The T-7/T-8 cutover at `cdcd670d`
  deleted the `wrapper_path` field (the fifth rewriter site) — that
  deletion is CORRECT. The remaining 4 rewriter sites in the live code
  path are unchanged.
- **Severity**: IMPORTANT (7-round carry; quadruply-motivated).
- **Owner**: security (driver-side validator landing).

## MINOR

### [R30-API1] NEW — `NomadStopPermits` and `NomadStopPermitGuard` land as `pub` with zero external-crate consumers; `pub(crate)` is the right visibility

- **Where**:
  - `crates/sandbox/src/backend/nomad_ch.rs:557` — `pub struct NomadStopPermits`
  - `crates/sandbox/src/backend/nomad_ch.rs:578` — `pub fn new`
  - `crates/sandbox/src/backend/nomad_ch.rs:608` — `pub async fn acquire`
  - `crates/sandbox/src/backend/nomad_ch.rs:631` — `pub fn permits_available`
  - `crates/sandbox/src/backend/nomad_ch.rs:637` — `pub fn capacity`
  - `crates/sandbox/src/backend/nomad_ch.rs:653` — `pub struct NomadStopPermitGuard`
  - `crates/sandbox/src/backend/nomad_ch.rs:714` — `pub fn install_nomad_stop_permits` on `NomadCHBackend`
  - `crates/sandbox/src/backend/nomad_ch.rs:728` — `pub fn nomad_stop_permits` on `NomadCHBackend`
  - `crates/sandbox/src/lib.rs:306` — `pub fn nomad_stop_permits` accessor on `AppState`

- **Audit**:
  - Zero external crates depend on `zeroship-sandbox` (confirmed:
    `Cargo.toml` search across `crates/` returns only the crate itself
    and the self dev-dep).
  - `NomadStopPermits::new` is called at `lib.rs:550` and `lib.rs:785`
    (both `AppState` constructors — same crate).
  - `NomadStopPermits::acquire` is called only from `stop_inner`
    (`nomad_ch.rs:1466`) and from in-module tests. Zero in-crate callers
    outside `nomad_ch.rs` call `acquire`.
  - `NomadStopPermitGuard` is returned by `acquire` and held as
    `Option<NomadStopPermitGuard>` in `stop_inner`. No in-crate caller
    outside `nomad_ch.rs` materialises the guard type by name.
  - `AppState::nomad_stop_permits()` accessor (`lib.rs:306`) is `pub fn`
    and returns `&Arc<NomadStopPermits>`. The rustdoc at `lib.rs:299-305`
    gives "intended for tests + future call sites that want to inspect
    `permits_available()` or `capacity()` for monitoring / chaos-test
    injection." This is the only forward-facing consumer rationale — and
    all identified consumers are in-crate test code or in-module
    production code.
  - The `install_nomad_stop_permits` setter and `nomad_stop_permits`
    getter on `NomadCHBackend` are called from `lib.rs` (same crate)
    only.

- **The problem (minimum-disclosure lens)**: `NomadStopPermits` is the
  concurrency governor for ALL Nomad `/shutdown` ladders. `acquire` is
  `pub` and `AppState::nomad_stop_permits()` is `pub` — meaning
  out-of-crate code (external test harnesses, integration shims) could:
  1. Call `AppState::nomad_stop_permits()`.
  2. Clone the returned `Arc`.
  3. Call `Arc::acquire().await` in a loop.
  4. Hold the returned `NomadStopPermitGuard` indefinitely.
  5. Starve `stop_inner`, blocking every teardown path until the guard
     drops.

  This is not a theoretical risk: the chaos-inject use case mentioned in
  the rustdoc (`lib.rs:303`) is exactly this shape. The distinction
  between "chaos-inject from test code" and "accidental hold from
  production callsite" is the visibility modifier, not a convention.

  The correct shape is:
  - `pub(crate) struct NomadStopPermits` — already consumed only
    in-crate.
  - `pub(crate) async fn acquire` — already consumed only in
    `stop_inner` + in-module tests.
  - `pub(crate) struct NomadStopPermitGuard` — corollary of narrowing
    `acquire`.
  - `pub(crate) fn install_nomad_stop_permits` /
    `pub(crate) fn nomad_stop_permits` on `NomadCHBackend` — called
    only from `lib.rs`.
  - `pub(crate) fn nomad_stop_permits` on `AppState` — called from
    `lib.rs` + in-module tests only.

  `NomadStopPermits::new` warrants a separate note: `lib.rs` calls it
  via the full path `crate::backend::nomad_ch::NomadStopPermits::new`.
  If `NomadStopPermits` is narrowed to `pub(crate)`, this call is
  still valid (same crate). `NomadStopPermits::new` can therefore be
  `pub(crate) fn new` as well.

  `permits_available` and `capacity` are called from `metrics_export.rs`
  (same crate, via the `AppState` accessor) and from tests (same crate).
  Both can be `pub(crate)`.

- **Note on `AppState::nomad_stop_permits` being `pub`**: the accessor
  crosses a module boundary (from `backend::nomad_ch` into `lib.rs`)
  but not a crate boundary. Narrowing to `pub(crate)` is sufficient
  to eliminate the external-DoS shape while preserving the in-crate
  chaos-test use case the rustdoc mentions.

- **Why MINOR and not IMPORTANT**: `zeroship-sandbox` has no external
  dependent crates — the `pub` overshoot only affects test harnesses
  that link via the `test-support` self dev-dep. The test harnesses
  run in the same process. The DoS shape requires intentional misuse
  of the permit API, not an accidental programming error. The
  minimum-disclosure principle motivates the narrowing on tidiness
  grounds even though the blast radius is limited.

- **Recommendation**: narrow all 9 items listed above to `pub(crate)`.
  Bundle with the R27-API3 sweep (13 → 15 narrowings) and R29-API1
  (2 more helpers) into a single code-quality pass — all 17+ narrowings
  are mechanical `s/^pub fn/pub(crate) fn/` or
  `s/^pub struct/pub(crate) struct/` touches with no call-site changes
  required (all callers are in-crate).
- **Severity**: MINOR.
- **Owner**: code-quality r31.

### [R30-API2] NEW — `metrics::inc_nomad_stop_permits_in_use` and `metrics::dec_nomad_stop_permits_in_use` are `pub fn` with zero external-module consumers; same lens as R27-API3

- **Where**:
  - `crates/sandbox/src/metrics.rs:496` — `pub fn inc_nomad_stop_permits_in_use`
  - `crates/sandbox/src/metrics.rs:505` — `pub fn dec_nomad_stop_permits_in_use`

- **Audit**:
  - `inc_nomad_stop_permits_in_use` is called only at
    `nomad_ch.rs:621` (inside `NomadStopPermits::acquire`).
  - `dec_nomad_stop_permits_in_use` is called only at
    `nomad_ch.rs:667` (inside `NomadStopPermitGuard::drop`).
  - Zero callers in `tests/` (confirmed); zero callers in `metrics_export.rs`
    (the export module calls `nomad_stop_permits_in_use_value()` for
    the gauge read — not the inc/dec primitives).
  - The `set_nomad_stop_permits_total` caller is `lib.rs:788` (in-crate),
    making it consistent with other `set_*` metrics functions — all `pub`
    on HEAD.

- **Why this belongs in R27-API3's sweep**: the inc/dec primitives are
  write-only gauge mutations that only `nomad_ch.rs` should invoke —
  same pattern as `inc_takeover_orphan`, `inc_clock_rewind` etc.
  which have zero external consumers. An external caller invoking
  `inc_nomad_stop_permits_in_use()` without holding a real permit
  would corrupt the `sandbox_nomad_stop_permits_in_use` gauge (it
  would read higher than the actual in-flight count), giving operators
  false saturation signals. This is a mild correctness concern (gauge
  noise) not a safety concern (the cap enforcement is in the flume
  bounded channel, not the gauge). Hence MINOR.

- **Note on `set_nomad_stop_permits_total` and `nomad_stop_permits_total_value`**:
  `set_nomad_stop_permits_total` is called from `lib.rs` (cross-module,
  in-crate). `nomad_stop_permits_total_value` is called from
  `metrics_export.rs` (in-crate) and from in-module tests. Both can be
  `pub(crate)`. These two join the R27-API3 scope expansion noted in
  the Summary (13 → 15 target).

- **Recommendation**: add all 4 new `nomad_stop_permits_*` functions
  in `metrics.rs` to the R27-API3 sweep target list. Confirmed revised
  total = 17 (13 original + `nomad_stop_permits_total_value` +
  `nomad_stop_permits_in_use_value` + `inc_nomad_stop_permits_in_use` +
  `dec_nomad_stop_permits_in_use`). Mechanical; no call-site changes.
- **Severity**: MINOR.
- **Owner**: code-quality r31 (bundle with R27-API3 sweep).

### [R30-API3] NEW — `docs/runbooks/sandbox-nomad-ch.md` still describes the pre-T-8 `raw_exec` + wrapper architecture after `cdcd670d` deleted the wrapper

- **Where**: `docs/runbooks/sandbox-nomad-ch.md`:
  - Line 3 (Audience): "nomad agent (`raw_exec`) + Cloud Hypervisor + virtiofsd"
  - Line 7 (What this is): "…one Nomad `raw_exec` job per sandbox. Each job invokes
    `crates/sandbox/scripts/nomad-vm-wrapper.sh`, which spawns: 3× virtiofsd…"
  - Line 30: "Nomad agent with `raw_exec` enabled."
  - Line 43: "Wrapper installed at the controller-configured path
    (`SANDBOX_NOMAD_CH_WRAPPER_PATH`, default `/etc/zeroship/nomad-vm-wrapper.sh`).
    …the controller validates `wrapper_path.exists() && executable` at startup…"
  - Line 53 (config table): `SANDBOX_NOMAD_CH_WRAPPER_PATH | /etc/zeroship/nomad-vm-wrapper.sh |
    Path the raw_exec task invokes.`
  - Line 83 (CreateGuard section): "races a still-alive prior wrapper for `tap=zsbx-nm-<idx>`"
  - Line 96 (triage): "Common causes: … raw_exec disabled, missing cloud-hypervisor /
    virtiofsd on PATH…"
  - Line 100 (triage): "Allocation reached `running` (the wrapper started)…"

- **The problem**: after `cdcd670d`, the controller unconditionally emits
  `Driver="ch"` in every Nomad jobspec. `SANDBOX_NOMAD_CH_WRAPPER_PATH`
  is NOT parsed from the environment; the `NomadCHConfig` struct no
  longer has a `wrapper_path` field. `nomad-vm-wrapper.sh` is deleted
  from the scripts directory. An operator following this runbook would:
  1. Enable `raw_exec` on the Nomad agent (no longer required — the
     `ch` driver has its own exec model).
  2. Install the wrapper script at `/etc/zeroship/nomad-vm-wrapper.sh`
     (the file no longer exists in the repo; the controller never
     references it).
  3. Set `SANDBOX_NOMAD_CH_WRAPPER_PATH` (silently ignored by the
     controller — the env var is no longer parsed).
  4. Waste a full host-setup debugging session wondering why the
     "wrapper" step is documented but the controller doesn't validate it.

  The runbook also omits the `SANDBOX_NOMAD_STOP_CONCURRENCY` env
  var (added at `ade8fb46`). This is present in the config table at
  line 63 already (correctly added by `ade8fb46`) but the "Host
  prerequisites" section still lists `virtiofsd` as a runtime
  dependency (line 31), which is no longer a prerequisite for the `ch`
  driver path.

- **Scope**: the runbook IS the operator-facing API contract for the
  `nomad-ch` backend. A stale runbook that describes deleted env vars
  and deleted dependencies misleads operators and violates the
  "operator-readable surface" standard the api-surface lens holds the
  codebase to.

- **Why MINOR not IMPORTANT**: `zeroship` is pre-launch, no production
  operators exist. The docs gap creates confusion only for the dev
  team running the stack locally (single node) or staging. The
  `sandbox-nomad-ch.md` runbook is not a wire-format contract — it's
  operator prose. The underlying code is correct; only the prose is stale.

- **Recommendation**: update `docs/runbooks/sandbox-nomad-ch.md` in the
  same PR that closes R30-API1. The update is prose-only:
  1. Audience / What-this-is: replace "Nomad `raw_exec` job" with
     "Nomad job using the `ch` Go plugin driver". Remove "3× virtiofsd"
     from the launch description (the ch driver manages its own child
     processes).
  2. Host prerequisites: remove the `raw_exec`-enabled item and the
     wrapper-path item. Add: "The `nomad-driver-ch` plugin binary
     installed in Nomad's `plugin_dir`."
  3. Config table: remove the `SANDBOX_NOMAD_CH_WRAPPER_PATH` row.
  4. Triage: replace wrapper references with ch-driver equivalents.
  This is a 30-line prose change with no logic impact.
- **Severity**: MINOR (docs / operator-contract staleness).
- **Owner**: code-quality r31.

### [R29-API1] (carry) `VmIndexAllocator::{release_vm_index_after, spawn_delayed_release_in_worker}` are `pub fn` with zero external-crate consumers

- **Where**: `crates/sandbox/src/backend/nomad_ch.rs:395` and `:437`.
- **Status r30**: No movement. Both remain `pub`. All call sites are
  in-crate (`restore_handler.rs` calls `VmIndexAllocator::new` via the
  same pub path; `release_vm_index_after` call sites at `:1625` and
  `:2590` are in-crate). `spawn_delayed_release_in_worker` still has
  zero production callers AND is `#[allow(dead_code)]`.
- **Severity**: MINOR (carry).
- **Owner**: code-quality r31.

### [R29-API2] (carry) `spawn_delayed_release_in_worker` is `pub fn` + `#[allow(dead_code)]` + zero production call sites — pick one of three coherent endings

- **Where**: `crates/sandbox/src/backend/nomad_ch.rs:436-455`.
- **Status r30**: No movement. `#[allow(dead_code)]` at `:436` remains
  in place; zero production callers; the rustdoc at `:418-435` still
  reads "No current call site in this crate uses this helper."
- **Severity**: MINOR (carry).
- **Owner**: code-quality r31.

### [R28-API3] (carry) `test-support` feature lacks crate-level rustdoc warning that it is not API-stable

- **Where**: `crates/sandbox/Cargo.toml:71-77` + `crates/sandbox/src/lib.rs:1-9`.
- **Status r30**: No movement. Now 5 items ride on the gate
  (unchanged from r29 closure count). The `ade8fb46` r30-A1 landing
  adds no new test-support items.
- **Severity**: MINOR (carry).
- **Owner**: code-quality r31.

### [R27-API1] (carry) BackendBuilder rustdoc — rationale on struct rustdoc, not on `Backend::builder()` entry-point

- **Where**: `crates/sandbox/src/backend/mod.rs:260-264`.
- **Status r30**: No movement. `Backend::builder` fn rustdoc still has
  no link to the `BackendBuilder` struct rationale. The `cdcd670d`
  T-7/T-8 cutover touched `mod.rs` (backend description update) but
  did not touch the `builder()` fn rustdoc.
- **Severity**: MINOR (carry).
- **Owner**: code-quality r31.

### [R27-API3] (carry, scope expanded 13→15→17) `metrics::*_value` and `metrics::inc/dec` accessors are `pub` with zero external consumers

- **Where**: `crates/sandbox/src/metrics.rs` — 15 functions originally;
  now 17 after r30-A1.
- **Status r30**: Target count revised upward. `nomad_stop_permits_total_value`
  and `nomad_stop_permits_in_use_value` join the r29 list of 13 `*_value`
  narrowing targets (confirmed zero external test consumers in
  `tests/`). Additionally, `inc_nomad_stop_permits_in_use` and
  `dec_nomad_stop_permits_in_use` are promotable to this sweep
  (per R30-API2). Revised full target list — 17 functions:
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
  nomad_stop_permits_total_value      ← NEW r30
  nomad_stop_permits_in_use_value     ← NEW r30
  inc_nomad_stop_permits_in_use       ← NEW r30
  dec_nomad_stop_permits_in_use       ← NEW r30
  ```
  (`lost_leadership_value` + `wake_terminal_overwrite_blocked_value`
  remain `pub` — confirmed external consumers at
  `tests/sandbox_pg_e2e.rs:947+960` and `:5584-5672` respectively.)
- **Severity**: MINOR (carry; scope expanded).
- **Owner**: code-quality r31.

### [R27-API4] (carry) `WakeErrorCode` rustdoc table at `db.rs:1731-1741` still lists 8 of 10 variants

- **Where**: `crates/sandbox/src/db.rs:1731-1741`.
- **Status r30**: No movement. `StagingPathMissing` and `AgentVersionMismatch`
  still absent from the table; the triangle remains functionally intact.
- **Severity**: MINOR (carry).
- **Owner**: code-quality r31.

### [R24-API3] (carry) async wake-poll envelope still missing structured `which` field

- **Where**: `admin_handlers.rs:1990-2008`. Unchanged.
- **Severity**: MINOR (carry).
- **Owner**: architecture (Phase-2 `wake_jobs.error_extra JSONB`).

### [R26-API5] (carry) sanitize-widening mask token unification

- **Where**: `crates/sandbox/src/wake_machine.rs:748` / `:1065` /
  `:1119`. Unchanged this round.
- **Severity**: MINOR (carry; cosmetic).
- **Owner**: code-quality r31.

### Other carries (no movement)

- **R19-API2** — `pub` → `pub(crate)` sweep; R27-API3 + R29-API1 +
  R29-API2 + R30-API1 + R30-API2 extend the same lens.
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

## §10.0 envelope state post-r30

### Inventory (delta from r29)

```
New since r29:  None (no §10.0-touching commits this round).
                ade8fb46 adds NomadStopPermits to the controller
                process but the semaphore is internal state; it
                does not surface in any wire response.
                cdcd670d deletes wrapper_path; no wire-format
                fields change.
```

The 32 §10.0 codes from r28/r29 are unchanged. `WakeErrorCode`
remains 10 variants; the `wire_code` triangle is intact
(R27-API4 rustdoc-table gap is the only open item).

### POST-endpoint envelope audit (delta from r29)

No wire-shape drift from any r30-range commit. All §10.0 endpoints
carry the same shape as r29. No new endpoints introduced.

## R30-API-VERIFY1 — r30-A1 `NomadStopPermits` surface verification

**Mandate** (per brief): verify `NomadStopPermits` type and
`nomad_stop_permits` AppState field have appropriate visibility; check
whether `pub` is the correct level.

**Audit checks**:

1. **`NomadStopPermits` struct visibility** — `nomad_ch.rs:557`:
   `pub struct NomadStopPermits`. Confirmed `pub` (not `pub(crate)`).

2. **`AppState.nomad_stop_permits` field visibility** — `lib.rs:258`:
   `pub(crate) nomad_stop_permits: Arc<…>`. Field is `pub(crate)`.
   **CORRECT** — the field itself is correctly restricted; an external
   caller cannot WRITE the field. The field rustdoc at `lib.rs:252-257`
   gives a clear rationale for `pub(crate)`.

3. **`AppState::nomad_stop_permits()` accessor visibility** — `lib.rs:306`:
   `pub fn nomad_stop_permits(&self) -> &Arc<…>`. The READ accessor is
   `pub`. See R30-API1 for the minimum-disclosure assessment.

4. **New `metrics::*` functions** — 4 new functions all `pub fn` at
   `metrics.rs:481-527`. `set_nomad_stop_permits_total` is called from
   `lib.rs:788` (cross-module, in-crate). `nomad_stop_permits_total_value`
   is called from `metrics_export.rs` (in-crate) and from tests.
   `inc_nomad_stop_permits_in_use` / `dec_nomad_stop_permits_in_use`
   are called only from `nomad_ch.rs` (single-module, in-crate). All 4
   could be `pub(crate)`; see R30-API2.

5. **`NomadStopPermits::new` call chain** ✓ `lib.rs:550`:
   `crate::backend::nomad_ch::NomadStopPermits::new(...)` →
   `lib.rs:785` (same pattern, `from_config` path). Both are in-crate
   calls. `NomadStopPermits::new` returning `Arc<Self>` (rather than
   `Self`) is the correct shape for a shared semaphore — caller always
   holds a refcounted handle.

6. **`config.rs` validation** ✓ `config.rs:719-726`:
   `validate()` rejects `nomad_stop_concurrency == 0` with an explicit
   error. The `NomadStopPermits::new` assert at `nomad_ch.rs:579-584`
   is belt-and-suspenders — the panic is unreachable in production
   because `validate()` fires at boot. Belt-and-suspenders is the
   right shape here (defense-in-depth against an in-process caller
   constructing `NomadStopPermits::new(0)` in test code).

7. **`install_nomad_stop_permits` / `nomad_stop_permits` on `NomadCHBackend`**
   — both `pub fn` at `:714` and `:728`. Both called from `lib.rs`
   only. See R30-API1 for narrowing recommendation.

8. **`NomadStopPermitGuard` struct** — `pub struct` at `:653`. Single
   field `refill: flume::Sender<()>` is private. The only way to
   materialize a guard is via `acquire()`. External callers can name
   the type in type-positions (e.g., holding the guard in a struct
   field) but cannot construct it directly. See R30-API1 for narrowing
   recommendation.

9. **`stop_inner` acquisition pattern** ✓ `nomad_ch.rs:1465-1468`:
   ```rust
   let _permit: Option<NomadStopPermitGuard> = match self.nomad_stop_permits() {
       Some(p) => Some(p.acquire().await),
       None => None,
   };
   ```
   The `None` arm is the unit-test / single-tenant-binary path where
   the semaphore is not installed. The `_permit` variable holds the
   guard for the full duration of `stop_inner`'s scope — correct RAII.

10. **Test count** ✓ The commit message states 549 → 555 tests (6 new).
    The load-bearing assertion `nomad_stop_permits_cap_enforced_n_plus_one_blocks`
    is present at `nomad_ch.rs:~8089` and pins the N+1 blocking property.

**Verdict**: r30-A1's `NomadStopPermits` landing is STRUCTURALLY SOUND
for its stated purpose (global concurrency cap on `/shutdown` ladders).
The field itself (`AppState.nomad_stop_permits`) is correctly
`pub(crate)`. The minimum-disclosure gap (struct + methods + accessor
at `pub` rather than `pub(crate)`) is the new R30-API1 MINOR finding;
it is a tidiness issue, not a functional defect.

## R30-API-VERIFY2 — T-7/T-8 cutover deletion cleanliness

**Mandate** (per brief): verify `TaskDriverMode`, `task_driver_mode_from_env`,
`NomadCHConfig::wrapper_path` deletions are clean; no orphan re-exports
or doc-hidden references.

**Audit checks** (all pass):

1. **`TaskDriverMode` deletion** ✓
   `grep -rn "TaskDriverMode" crates/sandbox/src/` returns zero matches.
   Only historical references survive in `docs/reviews/` (audit artefacts,
   not production surface).

2. **`task_driver_mode_from_env` deletion** ✓
   `grep -rn "task_driver_mode_from_env" crates/sandbox/src/` returns
   zero matches.

3. **`NomadCHConfig::wrapper_path` deletion** ✓
   `grep -rn "wrapper_path" crates/sandbox/src/` returns zero matches.
   The `SANDBOX_NOMAD_CH_WRAPPER_PATH` env var is also absent from
   `config.rs` (confirmed: `grep -n "SANDBOX_NOMAD_CH_WRAPPER_PATH"
   crates/sandbox/src/config.rs` returns zero matches).

4. **`nomad-vm-wrapper.sh` deletion** ✓ The file is absent from
   `crates/sandbox/scripts/` (deleted at `cdcd670d`).

5. **`raw_exec` string references in `src/`** — 7 surviving references
   in `nomad_ch.rs` (lines 2410, 2703, 3371, 5618, 5621, 5787, 5927).
   All 7 are in COMMENTS or test assertions that verify the `command`
   field (raw_exec-only) MUST NOT appear in the `ch` driver Config.
   These are correctness pins, not dead references. **Appropriate.**

6. **`backend/mod.rs` description** ✓ The module-level docstring for
   `nomad-ch` was updated at `cdcd670d` to read "Nomad job per sandbox
   using the `ch` Go plugin driver…" — no wrapper references survive.

7. **`main.rs` startup log** ✓ `cdcd670d` commit message says
   "Fix the three lib.rs test fixture structs and the main.rs startup
   log that still referenced wrapper_path." Confirmed via grep:
   `grep -n "wrapper_path" crates/sandbox/src/main.rs` returns zero
   matches.

8. **Replacement tests** ✓ `nomad_job_spec_always_uses_ch_driver`
   (`nomad_ch.rs:5771`) asserts `task["Driver"] == "ch"` AND
   `task["Config"]["command"].is_null()`. A matching restore-side test
   is not independently visible in the file (no `nomad_restore_job_spec_*`
   test name found in the search output), but the `ch_plugin_jobspec_includes_all_task_config_fields`
   test at `:5797` covers the Config block structure comprehensively.
   The restore path uses `build_restore_nomad_job_json_with` — a
   dedicated restore test at the jobspec level (analogous to
   `nomad_restore_job_spec_always_uses_ch_driver`) would pin this
   further; absence is a test-coverage carry (out of api-surface scope;
   note handed to test-coverage r31).

9. **Runbook stale** — `docs/runbooks/sandbox-nomad-ch.md` still
   describes the pre-T-8 wrapper architecture. This is the new R30-API3
   finding; the code itself is clean but the operator-facing doc lags.

**Verdict**: T-7/T-8 cutover deletions are STRUCTURALLY CLEAN in Rust
source. No orphan re-exports, no doc-hidden references, no missed
call sites. The one gap is the prose runbook (R30-API3).

## Cross-lens consensus

### R30-API-CROSS-r30A1 — concurrency r30 R30-A1 ratification

Concurrency r30's R30-A1 CRITICAL (per `c70a1d88` / `ade8fb46`) closes
cleanly. The semaphore shape is correct (flume bounded channel as
zero-tokio async semaphore is the right pattern for this workspace —
`compio` 0.18 lacks `sync::Semaphore`). The `pub` overshoot on
`NomadStopPermits` / `NomadStopPermitGuard` is a tidiness finding
(R30-API1) separate from the concurrency correctness question. The
field-level lock (`pub(crate)` on `AppState.nomad_stop_permits`) is
correctly shaped. Cross-lens consensus: clean.

### R30-API-CROSS-T7T8 — T-7/T-8 cutover ratification

The T-7/T-8 cutover (`cdcd670d`) is a NET POSITIVE api-surface delta
— it SHRINKS the public config surface by removing `wrapper_path` from
`NomadCHConfig` and deleting `TaskDriverMode` + its env-parse fn. The
three items deleted had zero long-term architectural value once the `ch`
driver became the unconditional default. The deletion is clean (VERIFY2).
The one remaining gap (stale runbook) is isolated, prose-only, and
easily fixed. Cross-lens consensus: the shrinkage is correct and well-executed.

### R30-API-CROSS-metrics — R27-API3 scope growth

Each concurrency-improving landing (r30-A1 this round, snap-idle-gc
parallel at `81b6e689` last round) adds new `pub` metrics functions
following the existing pattern, expanding the R27-API3 scope from 13 to
17. The growth pattern is predictable (each new semaphore / counter adds
a `*_total_value` + `*_in_use_value` read pair + an `inc_*` / `dec_*`
write pair) and the per-item fix is mechanical. The R27-API3 sweep
should be scheduled as a code-quality sweep that runs AFTER the last
concurrency landing for the cycle (so no new items are added mid-PR).
R31 is the correct target window if no further concurrency landings
are planned.

### Other cross-lens

- **Security r30**: no api-surface intersect this round. R20-API1
  schema-marker carry unchanged.
- **Performance r30/r31**: no api-surface intersect this round.
- **Test-coverage r31**: note from VERIFY2 §8 — a
  `nomad_restore_job_spec_always_uses_ch_driver` test pinning Driver="ch"
  for the restore jobspec path would strengthen the T-8 cutover pin.

## Lens hand-off

- **To architecture r31**:
  - R26-API1 (driver-side counter federation) — unchanged.
  - R24-API3 Phase-2 schema decision (`wake_jobs.error_extra JSONB`) —
    unchanged.
- **To test-coverage r31**:
  - R22-API2 carry.
  - R25-API1 cold-boot sibling pin.
  - §10.0 envelope enumeration test (r23 carry).
  - `/metrics` 200/401/403 integration test carry.
  - **NEW from r30 VERIFY2**: add `nomad_restore_job_spec_always_uses_ch_driver`
    test pinning Driver="ch" for the restore jobspec path.
- **To security r31**:
  - R20-API1 (schema-marker; 7-round + quadruply-motivated).
- **To code-quality r31**:
  - **R30-API1** (9 items): narrow `NomadStopPermits` + `NomadStopPermitGuard`
    + associated methods from `pub` → `pub(crate)`. Bundle with R30-API2
    + R29-API1 + R29-API2 + R27-API3 sweep into one code-quality PR.
  - **R30-API2**: narrow `inc_nomad_stop_permits_in_use` +
    `dec_nomad_stop_permits_in_use` (and optionally `set_*` + `total_value` +
    `in_use_value`) in the R27-API3 sweep.
  - **R30-API3** (NEW): update `docs/runbooks/sandbox-nomad-ch.md`
    to remove wrapper-path / raw_exec references from audience,
    prerequisites, config table, and triage sections. 30-line prose change.
  - **R29-API1** carry: `release_vm_index_after` + `spawn_delayed_release_in_worker`
    `pub` → `pub(crate)`.
  - **R29-API2** carry: resolve `spawn_delayed_release_in_worker`
    `pub` + `#[allow(dead_code)]` (delete or narrow).
  - **R28-API3** carry: add `# Features` rustdoc at `lib.rs` crate-root.
  - **R27-API1** partial-closure carry: cross-reference `Backend::builder`
    rustdoc to `BackendBuilder` struct.
  - **R27-API3 (revised 17-item sweep)**: all 17 `pub` → `pub(crate)`
    narrowings in `metrics.rs`.
  - **R27-API4** carry: extend rustdoc table from 8 to 10 rows.
  - **R26-API5** carry: sanitize mask token unification.
  - R22-API3 / R23-API2 / R23-API3 / R25-API5 carries.

## Backlog carry table

| ID | First round | Status r30 | Severity | Lens to own |
|---|---|---|---|---|
| R19-API2 | r19 | Open (carry; R27-API3 + R29-API1 + R29-API2 + R30-API1 + R30-API2 extend scope) | MINOR | code-quality |
| R20-API1 | r20 | Open (7-round carry; quadruply-motivated; held) | IMPORTANT | security |
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
| R27-API3 | r27 | Open (carry; scope expanded 13→15→17) | MINOR | code-quality |
| R27-API4 | r27 | Open (carry; rustdoc table still 8/10) | MINOR | code-quality |
| R28-API1 | r28 | **CLOSED at 62b083e1** | — | — |
| R28-API2 | r28 | **CLOSED** (verified R29-API-VERIFY3) | — | — |
| R28-API3 | r28 | Open (carry; no movement) | MINOR | code-quality |
| R29-API1 | r29 | Open (carry; `pub` → `pub(crate)` on 2 helpers) | MINOR | code-quality |
| R29-API2 | r29 | Open (carry; `pub` + `#[allow(dead_code)]` contradiction) | MINOR | code-quality / architecture |
| **R30-API1** | **r30** | NEW — `NomadStopPermits` + `NomadStopPermitGuard` (9 items) `pub` → `pub(crate)` | MINOR | code-quality |
| **R30-API2** | **r30** | NEW — `inc/dec_nomad_stop_permits_in_use` `pub` → `pub(crate)` (R27-API3 scope add) | MINOR | code-quality |
| **R30-API3** | **r30** | NEW — `sandbox-nomad-ch.md` runbook stale after T-8 cutover (wrapper-path / raw_exec refs) | MINOR | code-quality (docs) |

Net: r29 open = 9 → r30 open = 11 (0 closures, 3 new: R30-API1,
R30-API2, R30-API3; net +2).

## Trend

- **r17-r19**: §10.0 envelope discipline — settled.
- **r20-r22**: typed-error surface — landed + widened.
- **r23-r25**: pre-flight typed channels — landed.
- **r26**: observability-API surface gap promoted to IMPORTANT.
- **r27**: observability-API surface CLOSED on controller side.
  BackendBuilder consolidated. 4 composite-r1 cleanups closed.
- **r28**: minimum-disclosure forward-pressure. R27-API2 closed;
  pattern propagated to 5 more test-scaffolding leaks + 1 helper.
- **r29**: precedent-propagation pays off. R28-API1 closes with better
  fix (class-fix via typed-safe successors). R28-API2 closes
  mechanically. 2 new MINOR findings. Backlog drops to 9 (lowest
  since r24).
- **r30 (this round)**: two substantive landings (r30-A1 semaphore +
  T-7/T-8 cutover deletion) touch the public surface in opposite
  directions. The T-8 cutover is a net SHRINK (3 public items deleted:
  `TaskDriverMode` enum, `task_driver_mode_from_env`, `wrapper_path`).
  The r30-A1 landing is a net GROWTH (`NomadStopPermits`,
  `NomadStopPermitGuard`, 7 methods) that follows the established
  minimum-disclosure pattern except for the pub/pub(crate) choice.
  The runbook stale is a structural consequence of the cutover being
  code-complete but docs-incomplete. **No closures this round** — the
  code-quality r30 round (see existing `sandbox-snapshot-restore-
  code-quality-2026-05-25-r30.md` artefact) addressed other areas; the
  api-surface narrowing items were deferred to r31. Backlog rises to 11
  (+2 net) but all 11 are MINOR; the IMPORTANT class is unchanged at 2
  (both multi-round out-of-tree carries).

  **The defining api-surface theme of r30 is structural asymmetry
  between the T-8 cutover (clean deletion) and the r30-A1 semaphore
  landing (new pub surface at the right abstraction level but with
  an overshoot on visibility). Both landings are correct in their
  primary goal; the api-surface lens catches the residual exposure and
  the prose documentation gap left by the deletion.**
