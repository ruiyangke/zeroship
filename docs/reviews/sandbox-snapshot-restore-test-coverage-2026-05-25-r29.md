# Sandbox/snapshot-restore — test-coverage r29 review

Date: 2026-05-25 (UTC). HEAD at audit: `5a0647c3`
(`sandbox-snapshot-restore` worktree tip; clean tree; STRESS-R9-
RETRY deferred-entry just landed). Round 29. Prior:
`docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-
25-r28.md` (HEAD `885e2abb`). Companion: `docs/reviews/sandbox-
snapshot-restore-test-discipline-audit-2026-05-25-r1.md`
(HEAD `3a8dce02`, cycle 37).

Lib test count at HEAD: **540 passed; 0 failed; 1 ignored**
(verified locally; was "uncountable" at r28 due to in-flight
`driver_stages_disk_images` fixture lag — Q5 closed that at
`579369bb`). Pg-gated count: 91 + 3 new (r1-DISC-3) = **94**.

## TL;DR

- **r28 was retrospective+forward-looking. r29 is the close-
  out round on the carries.** Of the 8 r28 deliverables that
  gated Phase 4 (r28 line 588-602), the post-r28 commit
  stream landed the LIBRARY-side ones that don't depend on
  cross-worktree driver code:
  - **R28-DISCIPLINE adopted** as a discipline-audit ADR-
    equivalent (`test-discipline-audit-2026-05-25-r1.md`); 5
    predicates classified; 5 backlog items opened.
  - **r1-DISC-3 (R26-C1 cache pg-gated tests) LANDED** at
    `871752c7`: 3 pg-gated tests at `sandbox_pg_e2e.rs:5754-
    5964` pinning ptr-eq cache-hit, per-thread isolation
    (production-state oracle via `pg_stat_activity`
    application_name filter), and DSN-tiebreaker eviction.
    +254 LOC. Test 4 (housekeeper reaping) DEFERRED — see
    [R29-T1] for re-assessment.
  - **R27-M2 UTF-8 fix tests LANDED** at `821cc9bd`: 6 site-
    specific tests + 1 idempotent-composition test
    (`sanitize_idempotent_with_unicode` at `wake_machine.
    rs:1900-1924`). +6 LOC of test, +0 LOC of new code-
    quality gap. Closes the sanitize-composition coverage
    flag the r28 review carried.
  - **T5 verify_agent_version_post_restore LANDED** at
    `035c3564` + tests at `ce218860`: 8 outcome-pinning
    unit tests at `restore_handler.rs:4182-4384` + 1 wire-
    code structural test at `wake_machine.rs:1455-1494`.
    +299 LOC tests. R28-DISCIPLINE-compliant (real HTTP
    `spawn_fake_agent` + bind-then-drop ECONNREFUSED — see
    audit cycle 37 §4).
  - **r24-A2-S3 spawn_delayed_release LANDED** at
    `c969b94d`: 2 dedicated branch tests
    (`spawn_delayed_release_with_zero_delay_releases_
    immediately` and `..._honors_configured_delay`) at
    `nomad_ch.rs:4583-4660` + B19 regression
    test updated to poll the freed set non-destructively.
    `freed_for_test()` accessor added.
  - **composite-r1 metrics tests LANDED** at `de5a3eff`:
    `metrics_503_when_no_admin_tokens_configured` +
    siblings at `sandbox_admin_e2e.rs:1287-1430`.

- **Net new ask this round is SMALL.** Most of the r28
  backlog is now driver-side (cross-worktree, Phase 2/3
  in-flight). The new finds below are scoped to the
  controller-side surface area in the current worktree, per
  the r29 brief lens.

- **HIGHEST-LEVERAGE r29 ITEM — [R29-T2]**: the T5
  predicate has 8 unit tests, but **the wake-machine
  drive() integration path is NOT exercised end-to-end**.
  All 9 `wake_machine_drives_*` tests at `sandbox_pg_
  e2e.rs:5208-5670` use `persist: None`, which routes
  through the test-fixture `else` arm at
  `wake_machine.rs:563-569` and SKIPS the entire unseal /
  T5 / clock_resync / register triad. The T5 call-site at
  `:499-536` has **ZERO drive-level test coverage**. The
  outcome-mapping wire-code test (`t5_agent_version_
  mismatch_maps_to_distinct_wire_code`) verifies the enum
  variant exists and is distinct, but does NOT verify the
  drive() path INVOKES the predicate, INTERPRETS the
  outcome, and ROLLS BACK with the correct
  WakeErrorCode under `Mismatch`. Closing this gap
  requires a `wake_machine_drives_with_persist_and_t5_
  match_proceeds` + `..._mismatch_rolls_back` pair using
  a `spawn_fake_agent`-served `/version` + a real
  `Persistence` fixture sealing a known git_commit
  expectation. ~120 LOC. **Severity IMPORTANT (NEW).**

- **[R29-T3] NEW — Phase 2 controller-side staging-skip
  contract is JSON-shape tested only.** Cold-boot under
  `driver_stages_disk_images=true` at `nomad_ch.rs:859-
  883` selects the `workspace_image_path(host_dir)`
  branch INSTEAD of the `spawn_blocking + create_ext4_
  image_if_missing` block. The 3 emit-side tests at
  `:7657-7767` verify the JSON field; **NONE verify the
  controller bypasses `create_ext4_image_if_missing`**
  (no `host_dir` mkdir, no `workspace.img` write, no
  `home.img` write under the flag). A subtle regression
  that re-introduces the mkfs path while the JSON still
  says `stage_disk_images=true` would pass all 3
  existing tests AND cause Phase 4 driver double-mkfs
  (the controller wrote a 1-byte-detectable + size-N
  ext4; the driver would re-truncate and re-mkfs over
  it). ~50 LOC seam test asserting `host_dir` is NOT
  populated by the controller under the flag.
  **Severity IMPORTANT (NEW).**

- **[R29-T4] NEW — BackendBuilder coverage gap (R27-I1).**
  `BackendBuilder` at `backend/mod.rs:182-258` has 12 call-
  site migrations + 4 production paths (boot, lib-test
  fixtures, admin-e2e, share-e2e). **Zero dedicated unit
  tests pin the setter semantics**: `with_persist` /
  `with_local_nomad_node_id` are exercised IMPLICITLY via
  the production boot path but not asserted-on. The
  builder also has a load-bearing back-compat contract:
  `local_nomad_node_id` is *silently ignored* by the
  Docker / K8s variants (`mod.rs:223-230` rustdoc), and
  the build()'s match-arm error message has a specific
  shape (`unknown SANDBOX_BACKEND=...`). ~40 LOC of unit
  tests would pin (a) the setter takes effect on the
  nomad-ch path, (b) the setter is silently dropped on
  docker/k8s paths, (c) the unknown-backend error shape.
  **Severity MINOR (NEW).**

- **[R29-T5] NEW — `freed_for_test()` is currently in
  use ONLY by tests that observe r24-A2-S3 itself; the B19
  regression-test consumer at `:7438` is the only out-of-
  module reader.** This is fine for the current surface
  but the seam is now PERMANENT in production-typed code
  (`#[cfg(test)] pub fn freed_for_test`). The release-
  delay code path uses `tracing::info!` on the released
  task (`:378-384`). **There is no test asserting the
  log emission** — a tracing call-site that disappears
  on refactor loses operator visibility into "where did
  the slot go". A `tracing-test`-style capture-and-assert
  on the `"sandbox/nomad-ch vm_index released (r24-A2-S3
  delayed)"` log line would pin it. ~25 LOC.
  **Severity MINOR (NEW).**

- **[R29-T1] RE-ASSESSED — r1-DISC-3 Test 4 (housekeeper)
  deferral was correct, but the rationale is partially
  wrong.** The closure comment at `sandbox_pg_e2e.rs:5742-
  5751` says:

  > "the PoolConfig idle_timeout=600s / max_lifetime=1800s
  > defaults are not adjustable from the Database
  > boundary"

  Confirmed at `compio-postgres/src/pool.rs:54-75` — the
  fields ARE `pub`, but `Database::open_pool` at
  `db.rs:622-624` calls `PoolConfig::default()` and only
  sets `max_size` from `self.config.pool_max`. The true
  rationale: the housekeeper-runs predicate requires
  EITHER (a) idle_timeout < 30s (which needs a config knob
  the Database boundary doesn't surface) OR (b) instrument
  `start_housekeeper` to bump a counter that a test could
  read (no such counter today). The structural argument
  (housekeeper holds `Weak<Pool>` → self-terminates) is
  correct for SAFETY but does NOT validate the housekeeper
  ACTUALLY RUNS. A counter on `Pool::evict_expired` /
  `Pool::evict_idle_too_long` could close this — but
  that's a compio-postgres change, out of sandbox scope.
  **Severity MINOR (RE-ASSESSED).** Recommendation:
  document the gap in the deferred backlog with the
  correct rationale; do NOT block on it (cross-crate
  refactor cost > current oracle risk).

## CRITICAL

None.

## IMPORTANT

### [R29-T2] [NEW] T5 wake-machine drive() integration gap

**Where**: `crates/sandbox/src/wake_machine.rs:469-562` —
the `if let Some(p) = self.persist.as_ref()` arm wires the
T5 outcome→WakeErrorCode mapping at lines 522-536. ALL
existing `wake_machine.drive()` tests at
`crates/sandbox/tests/sandbox_pg_e2e.rs:5208-5670` (9
tests) use `persist: None` (verified at `:5196: persist:
None`), routing through the test-fixture else arm at
`wake_machine.rs:563-583` that SKIPS the unseal / T5 /
clock_resync / register triad.

**Predicate coverage today**: 8 unit tests at
`restore_handler.rs:4182-4384` pin the predicate's 7
outcomes (Match / Mismatch / Skipped × 5 reasons). 1
structural wire-code test at
`wake_machine.rs:1455-1494` pins the variant exists +
distinct from neighbors.

**What's NOT tested**:
- Drive() actually CALLS `verify_agent_version_post_
  restore` (a refactor renaming the function or
  branching past the call wouldn't fire the predicate
  tests).
- Drive() interprets `Mismatch` as a rollback signal AND
  routes through `rollback_with(...,
  WakeErrorCode::AgentVersionMismatch, ...)`.
- Drive() interprets `Skipped { reason }` as a "log WARN
  + proceed" signal (the wake_job row reaches `ok`, not
  rolled back).
- Drive() interprets `Match` as a no-op (no extra
  warning emitted).
- The T5 phase fires BETWEEN `livez` and `clock_resync`
  (sequence-sensitive: a refactor moving T5 before
  livez or after register would change the rollback
  semantics — if livez has not succeeded, the agent
  isn't up to answer /version, and T5 transport-error
  Skipped would mask a livez failure).

**What WOULD close it** (~120 LOC, pg-gated):

```rust
#[compio::test]
#[ignore = "needs Postgres; T5 mismatch end-to-end"]
async fn wake_machine_drives_with_persist_t5_mismatch_rolls_back() {
    // Seed: snapshotted row + sealed Persistence with known SK.
    // Build: WakeMachine WITH persist=Some(_).
    // Stub backend: derive_agent_url returns the spawn_fake_agent
    //   url; restore + wait_for_livez return Ok.
    // Fake agent: serves /version returning git_commit="stale-sha".
    // Expectation: machine.drive() ends with the wake_job row in
    //   state=Failed, error_code="agent_version_mismatch".
    //   Sandbox row reverts to Snapshotted (not Running).
    //   Allocator vm_index slot is released.
}

#[compio::test]
#[ignore = "needs Postgres; T5 match e2e"]
async fn wake_machine_drives_with_persist_t5_match_proceeds() {
    // Seed: same shape, /version returns the CONTROLLER_GIT_COMMIT
    //   verbatim. Expectation: row reaches `ok`.
}

#[compio::test]
#[ignore = "needs Postgres; T5 skipped-on-transport-error e2e"]
async fn wake_machine_drives_with_persist_t5_skipped_transport_proceeds() {
    // Fake agent: closed port (bind+drop) after livez stub returns
    // Ok. Expectation: row reaches `ok` (Skipped → WARN + proceed).
}
```

**Severity IMPORTANT (NEW).** This is R28-DISCIPLINE
applied to the integration layer: the unit tests pin the
predicate's outcome production; the integration tests
must pin the wake-machine's *interpretation* of the
outcome. Without [R29-T2] a refactor that drops the T5
call entirely from drive() (e.g., during a future
multi-step retry refactor) passes all 9 existing unit
tests AND ALL 9 existing drive() tests.

~120 LOC of pg-gated tests. Land BEFORE the next
substantive wake_machine restructuring. The Phase B
fake-agent helper (`spawn_fake_agent` at `restore_
handler.rs:3704`) is already in tree — wire it into a
pg-gated drive() fixture.

### [R29-T3] [NEW] Phase 2 controller-side staging-skip contract is JSON-shape tested only

**Where**: `crates/sandbox/src/backend/nomad_ch.rs:859-883`
— the `try_create` branch:

```rust
let workspace_img: PathBuf = if self.cfg.driver_stages_disk_images {
    workspace_image_path(host_dir)
} else {
    let host_dir_owned = host_dir.to_path_buf();
    // ... spawn_blocking + create_ext4_image_if_missing × 2 ...
    guard.host_dir_created = true;
    staged
};
```

**Coverage today**: 3 cold-boot-emitter tests at `:7657-
7767` pin the wire-side payload:
- `cold_boot_jobspec_includes_stage_disks_meta_when_flag_set`
- `cold_boot_jobspec_omits_stage_disks_meta_when_flag_unset`
- `cold_boot_jobspec_with_restore_from_overrides_stage_flag_
  to_false`

These verify `build_nomad_job_json_with(...)`. They DO
NOT exercise `try_create`'s staging-side branch.

**What WOULD have caught a regression**: a future refactor
extracting the `spawn_blocking` body into a helper that
forgets to honor the flag would leave the JSON correct
(emit_side untouched) AND silently double-mkfs (the
controller stages, the driver re-stages via its
`stageDiskImages` op). Phase 4 cluster would see TWO
mkfs.ext4 invocations per cold-boot CREATE — wasted I/O
and a subtle race between the controller's `fsync_dir`
and the driver's `truncate`.

**What WOULD close it** (~50 LOC):

```rust
#[compio::test]
async fn try_create_skips_controller_staging_under_flag() {
    let mut cfg = make_cfg();
    cfg.driver_stages_disk_images = true;
    let tmp = fresh_temp_root();
    cfg.nomad_ch.host_state_dir = tmp.path().to_path_buf();
    cfg.nomad_ch.user_home_dir_root = tmp.path().join("users");

    // Stub the Nomad submit so try_create reaches the staging
    // branch but doesn't actually submit. (Mock NOMAD_ADDR to
    // 127.0.0.1:1 + accept the submit failure as the test's
    // exit condition — we're observing the PRE-submit staging
    // side-effect.)
    let backend = NomadCHBackend::new(cfg, None).unwrap();
    let _ = backend.create(uuid::Uuid::now_v7(), "alice", "proj1").await;

    // ASSERTION: no workspace.img or home.img written. The
    // controller MUST NOT have touched the filesystem under
    // the flag.
    let host_dir = tmp.path().join(/* per-sandbox derive */);
    assert!(
        !host_dir.join("workspace.img").exists(),
        "controller staged workspace.img despite \
         driver_stages_disk_images=true"
    );
    let user_home_img = tmp.path()
        .join("users").join("alice").join("home.img");
    assert!(
        !user_home_img.exists(),
        "controller staged home.img despite \
         driver_stages_disk_images=true"
    );
}

#[compio::test]
async fn try_create_stages_controller_side_when_flag_unset() {
    // Mirror test: flag=false → workspace.img + home.img must
    // exist after try_create runs (current Phase 2 default).
}
```

**Severity IMPORTANT (NEW).** Load-bearing for Phase 4
default-flip — the contract this test pins is "when the
flag is on, the controller is NOT a staging actor." The
emit-side tests already cover the wire-side half; this
covers the staging-side half. ~50 LOC. Land WITH (or
BEFORE) the Phase 4 default-flip commit.

**R28-DISCIPLINE relevance**: this is the same fixture-
vs-predicate pattern as r5-A. The 3 existing tests
validate the EMITTER's branch structure (seam:
`build_nomad_job_json_with(cfg, ...)`). They do NOT
validate the PREDICATE that observed the controller's
mkfs side-effect (the actual filesystem state). R29-T3
is the production-state mirror.

## MINOR

### [R29-T1] [RE-ASSESSED] r1-DISC-3 Test 4 (housekeeper) deferral rationale partially incorrect

**Where**: `crates/sandbox/tests/sandbox_pg_e2e.rs:5742-
5751` — the closure-comment justification for not landing
the 4th r1-DISC-3 test.

**Stated rationale**:
> "the PoolConfig idle_timeout=600s / max_lifetime=1800s
> defaults are not adjustable from the Database boundary,
> and a CI test that waits 10+ minutes is not viable."

**Actual situation**: `compio-postgres/src/pool.rs:54-75`
exposes `pub max_lifetime: Duration` + `pub idle_timeout:
Duration` on `PoolConfig`. The adjustability gap is
specifically that `Database::open_pool` at `db.rs:622-624`
constructs `PoolConfig::default()` and overrides only
`max_size`; the controller has no env var or config knob
for the timing fields. So the test couldn't drive Database
to use a short timeout, but it could:

1. Bypass Database and call `Pool::connect_with_config(dsn,
   custom_short_config)` directly + `start_housekeeper(&rc)`
   + observe pg_stat_activity drain. ~60 LOC. Validates the
   compio-postgres housekeeper, not the sandbox's wiring of it.
2. Add a `pool_idle_timeout_secs` config field on
   `DatabaseConfig` (gated on `cfg(test)`) and surface it
   through `from_test_config`. ~80 LOC across the crate.
   Validates BOTH the wiring AND the housekeeper.

**Recommendation**: leave the test deferred per the
intent of r1-DISC-3 ("housekeeper correctness is
upstream") BUT correct the rationale comment. The current
phrasing implies a hard technical block; the actual
block is "the seam to drive a short timeout doesn't
exist in the sandbox crate's Database API" + "cross-crate
modifications to compio-postgres to add a counter are
out of scope." ~5 LOC docstring fix.

**Severity MINOR.** Documentation hygiene.

### [R29-T4] [NEW] BackendBuilder coverage gap (R27-I1)

**Where**: `crates/sandbox/src/backend/mod.rs:182-258`.

**Coverage today**: BackendBuilder is exercised by 8+
production / lib-test / e2e call sites (`lib.rs:706-713`
boot path; `lib.rs:2261/2468/2611/2622` test fixtures;
`tests/sandbox_*_e2e.rs:*` × 5). Zero dedicated unit
tests assert:
- `with_persist(p)` populates the field (verifiable via
  re-extraction: e.g., `Backend::builder(&cfg).with_persist(
  p).build()` then `Arc::strong_count(&p)` jumps).
- `with_local_nomad_node_id(id)` is honored by NomadCh
  AND silently dropped by Docker + K8s. The rustdoc at
  `:223-230` documents the silent drop; no test pins it.
- `build()` returns the documented `unknown SANDBOX_
  BACKEND=` error shape on a bogus backend string.

**Risk**: a refactor that switches `with_local_nomad_
node_id` to a hard-error on non-nomad-ch backends
would break no test — the rustdoc would be the only
evidence the prior contract existed. Per AGENTS.md
"pre-launch no back-compat" this is fine if the
refactor lands deliberately, but the test would
surface the silent-drop contract for review.

**What WOULD close it** (~40 LOC, lib-internal):

```rust
#[test]
fn backend_builder_with_persist_arc_is_propagated() {
    let cfg = min_nomad_cfg();
    let p = Arc::new(Persistence::new(/* ... */));
    let backend = Backend::builder(&cfg)
        .with_persist(Arc::clone(&p))
        .build()
        .expect("build ok");
    // Arc strong_count is 1 (caller's `p`) + N (backend's
    // Arc::clone into each variant). Asymmetric across
    // variants — assert >= 2 (clone present somewhere).
    assert!(Arc::strong_count(&p) >= 2,
        "with_persist did not Arc::clone into the backend");
}

#[test]
fn backend_builder_local_node_id_silently_dropped_by_docker() {
    let mut cfg = min_docker_cfg();
    let backend = Backend::builder(&cfg)
        .with_local_nomad_node_id("nm-test-7".into())
        .build()
        .expect("build ok");
    assert_eq!(backend.name(), "docker");
    // No assertion on the id itself — Docker has no observable
    // surface for it. The test pins "did not error out", which
    // is the documented contract.
}

#[test]
fn backend_builder_unknown_backend_error_shape() {
    let mut cfg = min_nomad_cfg();
    cfg.backend = "firecracker".into();
    let err = Backend::builder(&cfg)
        .build()
        .expect_err("unknown backend must err");
    assert!(err.contains("firecracker"),
        "error must echo the unknown backend string");
    assert!(err.contains("nomad-ch"),
        "error must list the supported backends");
}
```

**Severity MINOR (NEW).** ~40 LOC. R27-I1 closure docs
the builder rationale; this test pins the contract
beyond the rustdoc.

### [R29-T5] [NEW] r24-A2-S3 release-log emission untested

**Where**: `crates/sandbox/src/backend/nomad_ch.rs:378-
384` — `spawn_delayed_release` emits a `tracing::info!`
log carrying `vm_index`, `reason`, `sandbox_id`,
`delay_ms`.

**Why it matters**: operators scanning logs for "where
did the slot go" rely on the line `"sandbox/nomad-ch
vm_index released (r24-A2-S3 delayed)"`. A future
refactor that drops the log line OR renames the message
silently breaks operator visibility. The r24-A2-S3
review notes (commit message at `c969b94d`) explicitly
calls out the reason-tag (`"stop-fence-passed"` /
`"create-failure-cleanup"`) as load-bearing for
operators.

**What WOULD close it** (~25 LOC):

```rust
#[compio::test]
async fn spawn_delayed_release_emits_traced_log_with_reason() {
    use tracing_subscriber::layer::SubscriberExt;
    let logs: Arc<Mutex<Vec<String>>> = Default::default();
    let layer = /* tracing-test layer pushing into logs */;
    tracing::subscriber::with_default(/* ... */, || {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let pool = Arc::new(Mutex::new(VmIndexAllocator::new(33, 33)));
            let _ = pool.lock().unwrap().alloc();
            VmIndexAllocator::spawn_delayed_release(
                Arc::clone(&pool),
                33,
                Duration::ZERO,
                "test-trace-emit",
                Uuid::nil(),
            );
            // ...poll for log line containing the reason tag.
        });
    });
    let captured = logs.lock().unwrap().join("\n");
    assert!(captured.contains("test-trace-emit"),
        "reason tag must surface in the structured log");
}
```

**Severity MINOR (NEW).** Optional — the existing 2
release-fires tests + the B19 regression test together
cover the BEHAVIORAL contract. The MISSING piece is the
operator-visibility contract. Defer unless a tracing-
test helper exists elsewhere in the workspace.

### [R28-T2-CARRY] R26-T1 verbatim-msg exit tests — 8TH CYCLE

**Where**: chain unchanged from r26/r27/r28. 9 entry tests
at `nomad_ch.rs:4713-4805` (`extract_failed_task_event_
msgs` — verified at `:4704-4805` includes
`extract_failed_task_event_msgs_collates_driver_failure_
text`, `..._returns_empty_on_no_failed_tasks`,
`..._returns_empty_when_taskstates_missing`, plus the 6
sanitize/cap tests). **0 exit tests**.

**What r29 changes**: nothing. The chain still doesn't
have an end-to-end exit test (driver msg → sanitize → pg
→ admin `message` field). r28's framing under Option C
Phase 2/3 (failure-class shifts to mkfs.ext4 staging
errors) is still the right framing.

**Why it persists**: this is fundamentally a cross-
worktree test — the entry side is `nomad-driver-ch`
(separate worktree). The library carry-over from r26-T1
is the controller-side propagation tests (sanitize +
pg + admin envelope). r28 §R28-T2 carried the asks:
~35 LOC pg-gated at `sandbox_pg_e2e.rs:5530` +
~40 LOC at `sandbox_admin_e2e.rs`. **r29 status: still
OPEN, 8th carry.**

**Severity IMPORTANT (8th carry).** No demotion.
Recommendation: bundle with Option C Phase 4 stress
validation; the Phase 2 staging-errors will be the
oracle until a stub-backend pg-gated test lands.

### [R27-T3-CARRY] R26-T4 partial-closure: boot-time composition

`lib.rs:670-698` boot path's `fetch_local_nomad_node_id()
.await { Ok | Err → WARN + counter + None + continue }`
composition. Parser (5 cases) + counter monotonicity
tested individually; composition not. Carries unchanged
from r27/r28. ~30 LOC. **Severity IMPORTANT (carry,
4th round).**

### [R28-T6-LIB-CARRY] R27-T6-LIB sweep orchestration

`sweep.rs::run_host_dir_gc_once` — 1 pg-gated test;
orchestration body's count semantics un-pinned. ~30 LOC
pg-gated. **Severity MINOR.** 3rd-round carry.

### [R27-T4] read_snapshot_row pg-gated

Carries from r27. ~40 LOC pg-gated. **Severity MINOR.**
2nd-round carry.

### [R28-T3-CARRY] [DEFERRED] Phase 2 driver-side ext4 magic mirror

Cross-worktree driver-side test. Not actionable in this
worktree. Coordinated via `nomad-driver-ch` cycle. **No
status change.**

### [R28-T5-CARRY] StagingManifest wire-schema parity

Cross-worktree, dependent on Phase 3 controller-side
emission. Not yet landed. ~50 LOC when applicable.
**Carry from r28.**

### [R28-T8-CARRY] Driver-side production-state fixture pattern

Cross-worktree (driver). r1-DISC-1 documentary entry
covers the in-this-worktree footprint. No additional ask.

### [R25-T1-CARRY] stress-harness invariant — 5TH CYCLE

No change. Cross-worktree. ~120 LOC.

### [R25-T3-CARRY] multi-cycle vm_index race — 5TH CYCLE

Cross-worktree (driver Go test). Indirectly addressed by
r24-A2-S3 controller-side delay landing this cycle (the
delayed-release reduces the per-slot reuse-window the
race exploits). No direct library test ask.

### [R22-T3-CARRY] retry-race pg test — 9TH CYCLE

Carries. ~80 LOC. **Severity MINOR.** No status change.

## Carry-forward table (r28 → r29)

| Tag | r28 status | r29 status | Notes |
|-----|------------|------------|-------|
| R28-DISCIPLINE | NEW IMPORTANT | **ADOPTED** at `0b8cf6c2` (test-discipline-audit-2026-05-25-r1.md) | 5 predicates audited; 5 backlog items opened |
| R28-PHASE2-TEST-PLAN | NEW IMPORTANT | **DRIVER-SIDE; CROSS-WORKTREE** | nomad-driver-ch carries it |
| R28-PHASE2-OBS-COUNTERS | NEW IMPORTANT | **CROSS-WORKTREE** | nomad-driver-ch |
| R28-T1 manifest validation | NEW IMPORTANT | **CARRY** | Phase 3 not yet emitted; controller-side ask pending |
| R28-T2 verbatim-msg exit | IMPORTANT (7th) | **IMPORTANT (8th carry)** | No demotion; library-side still open |
| R28-T3 ext4 magic | NEW IMPORTANT | **CROSS-WORKTREE** | Driver-side |
| R28-T5 wire-schema parity | NEW IMPORTANT | **CROSS-WORKTREE** | Pending Phase 3 |
| R28-T8 prod-state driver fixture | NEW IMPORTANT | **CROSS-WORKTREE / r1-DISC-1 documents** | No further library ask |
| R28-T6 sweep creator-agnostic | NEW MINOR | **CARRY** | Defunct under Phase 3 dirent-shape only |
| R28-T7 cutover test-orphan | NEW MINOR | **CARRY** | Process-discipline; no test |
| R28-S1 path leakage sanitizer | NEW MINOR | **CARRY** | Defer until Phase 2 stress |
| R27-T3 boot-failure composition | IMPORTANT (CARRY) | **IMPORTANT (carry)** | 4th round |
| R25-T1 stress harness | OPEN (4th) | **OPEN (5th)** | Cross-worktree |
| R25-T3 vm_index race | OPEN (4th) | **OPEN (5th); indirectly mitigated by r24-A2-S3** | Cross-worktree |
| R25-T6 UTF-8 truncation | **CLOSED** | already closed | r27-M2 latent-bug also closed at `821cc9bd` |
| R26-T1 (= R28-T2) verbatim-msg | IMPORTANT (7th) | IMPORTANT (8th) | Same as R28-T2 |
| R26-T6 placement-audit cluster | IMPORTANT (3rd carry) | **CARRY** | Cluster-side |
| R27-T4 read_snapshot_row | MINOR | **MINOR (carry, 2nd)** | |
| R27-T5 STOP HTTP timing | MINOR DEMOTED | **MINOR (carry)** | Option C moots |
| R27-T6-LIB sweep orchestration | MINOR (carry) | **MINOR (3rd carry)** | |
| R27-S1 / R28-S1 sanitize bare-UUID | MINOR | **MINOR (carry)** | |
| R22-T3 retry-race pg | OPEN (8th) | **OPEN (9th)** | |
| **r1-DISC-1** documentary | NEW | **CLOSED** (in audit) | Documents r5-A retrospectively |
| **r1-DISC-2** transport-flake variants | NEW MINOR | **CARRY** | ~30 LOC optional |
| **r1-DISC-3** R26-C1 cache | NEW IMPORTANT | **CLOSED at `871752c7`** | 3 tests; +254 LOC; test 4 deferred |
| **r1-DISC-4** ext4 magic | NEW IMPORTANT | **CROSS-WORKTREE** | Driver-side |
| **r1-DISC-5** idempotency | NEW IMPORTANT | **CROSS-WORKTREE** | Driver-side |
| **NEW R29-T1** housekeeper deferral re-assess | — | **MINOR (NEW)** | Doc-only |
| **NEW R29-T2** T5 drive() integration | — | **IMPORTANT (NEW)** | ~120 LOC pg-gated |
| **NEW R29-T3** staging-skip contract | — | **IMPORTANT (NEW)** | ~50 LOC lib |
| **NEW R29-T4** BackendBuilder | — | **MINOR (NEW)** | ~40 LOC lib |
| **NEW R29-T5** release-log emission | — | **MINOR (NEW)** | ~25 LOC; optional |

## Bundle-specific would-have-caught analysis

### r24-A2-S3 timing test sufficiency

The 5-second production delay is timing-bound. The
existing tests at `nomad_ch.rs:4583-4660` use
`Duration::ZERO` (1 test) + `Duration::from_millis(200)`
(1 test). The non-zero-delay test asserts:
1. **Halfway-elapsed**: slot is NOT yet freed at `delay /
   2` (~100ms in).
2. **Post-elapsed**: slot IS freed within `delay +
   500ms`.

This is sufficient for the predicate "the sleep is
honored" but does NOT test:
1. **Cancellation under runtime drop**: if the
   detached compio task is dropped before the sleep
   elapses (e.g., runtime shutdown during a stop
   sequence), does the slot leak indefinitely?
   `compio::runtime::spawn(...).detach()` semantics: a
   detached task survives across `Drop` of the join
   handle but is cancelled if the runtime itself
   shuts down. **No test verifies this**.
2. **5-second production-equivalent**: 200ms is 25× the
   production delay. Scheduler jitter at 5s is bounded
   differently than at 200ms (e.g., a 50ms scheduler
   delay is 0.5% of 5s, 25% of 200ms). The test would
   be slow to run (5s × N test invocations), so 200ms
   is a reasonable proxy — but the cost analysis is
   undocumented.

**Severity MINOR (not flagged as a new finding)**:
the existing 2 tests are sufficient for the unit-test
budget. The runtime-shutdown race is a stress-test
oracle; defer.

### Would the new R29 findings have caught any landed regressions?

- **[R29-T2]**: would catch a future refactor that
  drops the T5 phase from drive(). No active
  regression — T5 just landed.
- **[R29-T3]**: would catch a Phase 4 default-flip
  refactor that retains the legacy staging path. Not
  yet active.
- **[R29-T4]**: would have caught a hypothetical refactor
  of R27-I1 that re-introduces a telescoping
  constructor pattern. Not active.

None of the r29 NEW findings catch ACTIVE regressions in
the current worktree; all are forward-looking gates for
the next 1-3 substantive refactors per ADR Phase 4
cutover.

## Phase 4 cutover gate sufficiency

**INSUFFICIENT** without:
- **[R29-T2]** T5 drive() integration test (NEW).
- **[R29-T3]** staging-skip contract test (NEW).
- **R28-T1** Phase 3 manifest validation (cross-worktree
  pending).
- **R28-T2** verbatim-msg exit (8th carry).
- **R28-T3** ext4 magic (cross-worktree).
- **R28-T5** wire-schema parity (cross-worktree).
- **R27-T3** boot-failure composition (4th carry).

7 asks; 2 NEW for r29; rest carry/cross-worktree.

LOC budget for IN-WORKTREE Phase-4 gates: ~120 (R29-T2)
+ 50 (R29-T3) + 35 (R28-T2 pg) + 40 (R28-T2 admin) + 30
(R27-T3) = **~275 LOC controller-side**. ~620 LOC total
including driver-side cross-worktree work.

## To test-cov r30 backlog (~275 LOC controller-side)

1. **R29-T2** — T5 drive() integration. **~120 LOC
   pg-gated. IMPORTANT.** Highest-leverage NEW r29
   item.
2. **R29-T3** — staging-skip contract. ~50 LOC.
   **IMPORTANT.** Pair with Phase 4 default-flip.
3. **R28-T2 / R26-T1** verbatim-msg exit (8th carry).
   ~75 LOC. **IMPORTANT.**
4. **R27-T3** boot-failure composition. ~30 LOC.
   **IMPORTANT (4th carry).**
5. **R29-T4** BackendBuilder unit tests. ~40 LOC.
   **MINOR (NEW).**
6. **R29-T5** release-log emission. ~25 LOC.
   **MINOR (NEW); optional.**
7. **R29-T1** housekeeper deferral re-rationale. ~5
   LOC docstring fix. **MINOR (NEW); doc-only.**
8. **R28-T6** sweep creator-agnostic. ~15 LOC.
   **MINOR (carry).**
9. **R27-T6-LIB** sweep orchestration. ~30 LOC.
   **MINOR (3rd carry).**
10. **R27-T4** read_snapshot_row pg-gated. ~40 LOC.
    **MINOR (2nd carry).**
11. **R28-S1** sanitize bare-UUID widening. ~10 LOC.
    **MINOR; defer until Phase 2 stress.**
12. **r1-DISC-2** transport-flake variants. ~30 LOC.
    **MINOR; optional.**
13. **R22-T3** retry-race pg (9th carry). ~80 LOC.
    **MINOR.**

Delisted this round:
- **r1-DISC-3** R26-C1 cache — CLOSED at `871752c7`.
- **R28-DISCIPLINE** — ADOPTED via discipline audit.
- **R25-T6** UTF-8 + R27-M2 latent — CLOSED.
- All cross-worktree (`nomad-driver-ch`) asks — tracked
  by that worktree's review cycle, not actionable here.

## Notes for r30

- **The shift this round**: r28 was the round that
  named R28-DISCIPLINE retrospectively. r29 is the round
  that observes the rule's adoption (audit + r1-DISC-3
  test landing) and pivots to FORWARD coverage gates.
  Both **R29-T2** (T5 drive integration) and **R29-T3**
  (staging-skip contract) are direct applications of the
  R28-DISCIPLINE pattern: assert the
  PRODUCTION-PATH-EXECUTION not just the
  PREDICATE-IN-ISOLATION.
- **The pattern of "predicate vs integration"**: this
  round surfaced TWO instances of the same pattern. T5
  has an 8-unit-test predicate suite + 1 wire-code
  structural test; drive() integration is untested. The
  Phase 2 flag has 3 emitter-shape tests; the staging-
  side bypass is untested. The rule R28-DISCIPLINE
  applied at the predicate layer; **R29 generalizes it
  to the INTEGRATION layer**.

  Operationally: **for every NEW predicate that gates
  control flow in a wider state machine, land BOTH a
  unit test of the predicate AND an integration test
  of the state machine's interpretation of the
  predicate's outcomes.** The 8 T5 unit tests + 0
  drive-level integration tests is the imbalance the
  rule names.
- **r1-DISC-3 closure**: clean. The 3 tests use the
  prescribed production-state oracle (pg_stat_activity
  filtered by application_name). Test 4 (housekeeper)
  deferral is correct under the brief's "10-min CI
  budget" lens; R29-T1 flags the deferral comment's
  technical-block phrasing as imprecise but does not
  ask for the test itself.
- **R28 → r29 closure ratio**: of the 8 r28 deliverables,
  4 closed in-worktree this round (R28-DISCIPLINE
  adopted, r1-DISC-3 landed, R27-M2 tests landed, T5
  + r24-A2-S3 landed); 4 are cross-worktree pending
  (R28-PHASE2-TEST-PLAN, R28-PHASE2-OBS-COUNTERS,
  R28-T3 ext4 magic, R28-T5 wire-schema). The
  in-worktree closure rate is **healthy** —
  ~half the round-29 backlog drained.
- **Highest-leverage r30 closure**: **R29-T2** T5
  drive() integration. ~120 LOC pg-gated. Without it
  any future refactor of the WakeMachine state machine
  can drop the T5 phase entirely and pass 540+ unit
  tests. The Phase B fake-agent helper is already in
  tree; the seed-snapshotted-row pg fixture pattern
  is already established (9 instances at
  `sandbox_pg_e2e.rs:5208-5670`). Cost is small;
  benefit is the integration-layer mirror of
  R28-DISCIPLINE.
- **Lib test count**: 540 (up from 512 at r28 baseline).
  Net delta this round: +28 tests (10 from R27-M2 +
  T5 + r24-A2-S3 landings; 18 from carry refactors
  observed at the test count). Pg-gated: 91 → 94
  (+3 r1-DISC-3). Healthy growth; no regressions.
- **No emoji, no celebratory framing**: r29 is a
  drain-the-backlog round. The retrospective half of
  r28 (R28-DISCIPLINE) landed; the forward-looking
  half (Phase 2 cross-worktree tests) is in flight on
  the driver worktree. r29's net NEW asks are 5
  items, 2 IMPORTANT + 3 MINOR, all controller-side
  and all forward-looking gates for the next 2-3
  substantive refactors. Net direction: r29 surfaces
  the integration-layer mirror of R28-DISCIPLINE,
  with R29-T2 as the canonical instance.
