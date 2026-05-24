# Sandbox/snapshot-restore — test-coverage r27 review

Date: 2026-05-25 (UTC). HEAD at audit: `01288c18` (worktree tip;
T-8b-stress-r4 RED review docs landed). Brief instructed
HEAD `d0d7abd6` **or later** — current is 4 commits ahead
(precursor + R26-I1 closure + deferred-backlog tag + stress-r4
docs). Round 27. Prior: `docs/reviews/sandbox-snapshot-restore-
test-coverage-2026-05-25-r26.md` (HEAD `3d431eb8`).

Bundle landed since r26:

- `883df7fe` r3-A precursor: `fetch_local_nomad_node_id` +
  `parse_nomad_agent_self_node_id` (pure parser extracted) +
  `AppState.local_nomad_node_id` + counter — **+6 tests** (5
  parser shapes + 1 counter monotonic).
- `901dfbf2` R25-T4 closure: `classify_host_dir_entry` +
  `host_dir_eligible_by_db` pure helpers — **+13 sweep tests**.
- `9b623f44` r3-A cold-boot Constraints emission — **+0 tests
  in this commit** (production-only — its companion test
  shipped in d71f1a8c; commit message claim of "+4 tests" is
  inaccurate, see [R27-DOC1]).
- `d71f1a8c` r3-A restore-path Constraints emission — **+3
  tests** (`restore_jobspec_includes_*`, `_omits_*`, and the
  cross-emitter parity contract `node_affinity_constraints_
  parity_between_cold_boot_and_restore_emitters`).
- `34b52cf1` HOST_DIR_GC_GRACE_SECS tightening 3600→600 — pin
  test landed earlier at 901dfbf2 (`host_dir_gc_grace_default_
  and_floor_pinned`).
- `7647cd4d` R22-S1 Mode A sanitize widening — **+7 tests**
  (path/typed-id strip + combination + preserves + idempotency).
- `6c475c30` R26-I1 precursor (visibility bump) — **+0 tests**.
- `b5ec01a1` R26-I1 closure (delete WakeSnapshotMeta) — **+0
  tests** (pure DRY collapse).
- `01288c18` T-8b-stress-r4 docs only.

## TL;DR

- **Lib delta r26 → r27: +29** (459 → 488, verified via
  `cargo test -p zeroship-sandbox --lib` at HEAD `01288c18` =
  488 PASS / 0 fail / 1 ignored). Pg-gated unchanged at 91, all
  skip-clean when DATABASE_URL is unset (verified — 0 passed
  / 0 failed / 91 ignored). The +29 lib delta breaks down: +6
  r3-A precursor parser/counter, +13 R25-T4 sweep, +3 r3-A
  restore-path Constraints + parity contract, +7 R22-S1
  sanitize widening. Trapezoid-of-coverage shape from r26
  STAYS — entry-side test depth grew (9 helper tests at
  `extract_failed_task_event_msgs`); exit-side (`wake_jobs.
  error_message` round-trip + admin handler `message` field)
  picked up 0 new tests.

- **R25-T4 sweeper unit tests — CLOSED at `901dfbf2`.** The
  6 destructive-on-misfire gates the brief flagged carry-over
  for 2 rounds got pinned via a refactor that extracted two
  pure helpers (`classify_host_dir_entry` for FS gates,
  `host_dir_eligible_by_db` for DB gates) and a 13-test
  matrix covering every gate + clock-skew semantics +
  grace-boundary inclusion + the user-subdir invariant. Quality
  is high: gates are stated as algebra (skip / under-grace /
  candidate), tests are table-driven, no pg dependency for
  the FS branch. The end-to-end `run_host_dir_gc_once` body
  still relies on pg-gated coverage (1 test in
  `sandbox_pg_e2e.rs`) but the pure helpers are the
  load-bearing logic. This was the highest-leverage open ask
  from r25/r26; closed cleanly.

- **R22-S1 sanitize widening — 7 new tests at `7647cd4d`.**
  Brief asked: are 7 tests sufficient or are sibling masking
  sites missed? Verified by `Grep "sanitize_error_message\("
  crates/sandbox/src` → returns ONE production call site at
  `wake_machine.rs:165` (Phase::Failed → pg write of
  `error_message`). The sync path (`do_restore_inner`) and
  the admin handler do NOT call sanitize — they read the
  pre-sanitized value out of `wake_jobs.error_message`. So the
  single-site posture is intentional: sanitization happens at
  the write boundary, all readers see the already-sanitized
  text. **7 tests at the single emission site is sufficient
  for the sanitizer surface** — but the round-trip from
  driver-msg → SubmitRestoreError → Backend(String) →
  sanitize → pg row → wire is still pin-less past the
  sanitizer entry (carries to R27-T1, the verbatim-msg
  propagation exit test ask that's been open 6 rounds now).
  See also [R27-S1] for an adjacent concern: the sanitizer
  preserves `/tmp/...` paths intact, which is correct for
  binary-path readability but means a driver-msg containing
  `/tmp/cloud-hypervisor.sock.<sandbox-id>` would NOT have
  the typed-id stripped (the `/tmp/...` prefix consumes the
  entire path including the typed-id suffix). The R26-T7 hint
  about path-redaction-vs-typed-id ordering applies in
  reverse here.

- **R25-T2 cold-boot mirror test — STILL OPEN (5th cycle).**
  Brief asked: STILL OPEN 4 rounds; verify status; propose
  path forward. Verified at HEAD: `submit_restore_job_rejects_
  missing_workspace_img` at `restore_handler.rs:3362` is still
  the only "preflight rejects pre-submit" assertion. The cold-
  boot `create()` at `nomad_ch.rs:530` has NO companion test
  asserting `submit_nomad_job` doesn't fire when
  `workspace.img` is absent. **Note (NEW)**: stress-r4's CREATE
  rate moved from 22 % → 100 % via r3-A node-affinity (not
  via preflight), so the WORKER_COUNT>1 cluster signal that
  R25-T2 would catch is now invisible at cluster level. This
  changes the rationale: R25-T2 is no longer needed for
  cluster-failure reproduction (placement pinning prevents
  the cross-node ENOENT); it's needed as a contract test
  pinning "controller refuses to submit a Nomad job for a
  workspace.img that doesn't exist locally" — defensive
  invariant only. Severity demoted MINOR for r27 given r3-A's
  closure of the cluster-observed failure mode. Path forward:
  if the ask is still open at r28, fold it into an "API
  contract for Backend.create" test bundle alongside
  existing `assert_disk_image_present_rejects_missing_path`
  / `create_ext4_image_if_missing_skip_path_rejects_zero_
  byte_file`. See [R27-T2] below.

- **R26-T1 verbatim-msg propagation exit tests — STILL OPEN
  (6th cycle).** Brief asked: 9 entry tests, 0 exits — still
  open? Verified: `Grep "verbatim|driver_msg|disk\[" crates/
  sandbox/tests/` → 1 unrelated match (preview_share Set-Cookie
  text). Zero matches for `extract_failed_task_event_msgs` or
  `disk\[N\]` patterns in any integration test. The
  pg-gated `wake_machine_classifies_submit_failure` at
  `sandbox_pg_e2e.rs:5300` asserts the `error_code` field on
  failure but NOT the `error_message` content — closest test
  to the exit and it doesn't anchor the verbatim text
  contract. The admin-side `GET /admin/sandboxes/{id}/wake/
  {wake_id}` propagation test for the `message` field also
  doesn't exist. **6-round carry. This is the highest-
  leverage open r27 ask** given stress-r4's WAKE failure mode
  (46/51 RED on a deterministic CH `AlreadyLocked` lock
  collision) — operators need the verbatim driver-msg to
  diagnose the wedge from pg + admin queries without ssh.

- **R3-A test coverage assessment — quality is high,
  trajectory is mixed.**
  - **R26-T3 parser (5 JSON shapes) CLOSED at `883df7fe`.**
    Used a cleaner approach than the brief asked for: extracted
    `parse_nomad_agent_self_node_id` pure helper so unit
    tests don't need a mock HTTP server. 5 cases cover
    lowercase / PascalCase / missing-client / empty-string /
    malformed-body. Quality is high.
  - **R26-T2 cross-emitter parity CLOSED at `d71f1a8c`.**
    `node_affinity_constraints_parity_between_cold_boot_and_
    restore_emitters` asserts byte-identical Constraints
    array between cold-boot and restore-path builders for
    BOTH Some and None paths. This is exactly R22-T1's
    discipline applied to Job-level Constraints. Quality is
    high.
  - **R26-T4 boot-failure non-fatal fallback — PARTIALLY
    CLOSED**. The brief said "CLOSED" but verification at
    HEAD shows the closure is partial: the **counter
    monotonicity** is tested (`inc_nomad_node_id_lookup_
    failure_monotonic` at `metrics.rs:549`); the **parser
    failure shapes** are tested (5 parser tests above); but
    there is NO lib test driving `AppState::new` /
    `AppState::from_config` against an unreachable agent
    that asserts `(boot completes, state.local_nomad_node_id
    is None, counter is bumped)` as one composed contract.
    The fixture path at `lib.rs:519` hardcodes `None`
    (assumed-shape), and no test exercises the
    `fetch_local_nomad_node_id().await → Err → demote-to-WARN
    → continue boot` round-trip at the boot orchestration
    level. **The pieces are tested individually; the
    composition contract is asserted only via prose**
    (lib.rs:225-244 comment block). Carries to R27-T3
    (re-tagged ask).
  - **R26-T6 placement-audit cluster test — STILL OPEN.**
    No `tests/cluster_placement_audit.sh` or equivalent
    `WORKER_COUNT=2` harness landed. The cluster lens caught
    the cross-node race in stress-r3 (47/47 CREATE fail);
    r3-A closed the placement bug at the production-code
    level; stress-r4 validated CREATE=100 %. So the
    placement-audit cluster test is now a **regression
    guard**, not a diagnostic gap — its value shifted from
    "catch the bug before $20-30 stress" to "prevent r3-A
    regression at PR-time for ~$2-3/run." Severity stays
    IMPORTANT; the harness should land before r28 or be
    formally deferred.

- **NEW [R27-T1] stress-r4 WAKE wedge has no unit-test
  surface (HIGHEST LEVERAGE).** Stress-r4 RED at 3/60 e2e
  with a single dominant WAKE failure: CH's virtio-blk
  `ExclusiveWrite` lock collision on `rootfs.img`. The
  failure occurs cycle-N+1 when CH's prior-cycle process
  still holds the file lock. Brief asked: what test-coverage
  gap could close this? **Answer**: the failure is a
  state-machine ordering bug between (a) STOP returns
  HTTP 200 to caller (synchronous fast-path) and (b) Nomad
  job-stop + CH process exit + file-lock release (async).
  The Backend trait surface that mediates this is
  `Backend::stop_for_real()` and the wake-side
  `submit_restore_job()` precondition. NO test asserts "after
  stop_for_real returns Ok, the rootfs.img lock is released
  before the next submit_restore_job for the same sandbox_id
  is permitted." This is a contract that doesn't exist in
  source — but it needs to, and the test would name the
  contract before the production fix lands. See [R27-T1]
  body for the testable shape. ~80 LOC lib test using the
  existing `RealRestoreBackend` fixtures.

- **NEW [R27-T2] R25-T2 demoted: cold-boot mirror test
  becomes defensive only.** Stress-r4's 60/60 CREATE
  validates r3-A node-affinity closes the cross-node ENOENT.
  R25-T2's original motivation (catch the cross-node race
  unit-test-side) is now closed at a HIGHER abstraction
  layer (placement pinning). The cold-boot mirror test still
  has value as a defensive invariant ("controller must NOT
  call submit_nomad_job for a workspace.img it can't stat
  locally") but the cluster-failure-reproduction urgency is
  gone. Demoting from IMPORTANT to MINOR. See [R27-T2].

- **NEW [R27-T3] R26-T4 partial-closure ask reshaped.** The
  pieces (parser shapes, counter monotonicity) are tested;
  the COMPOSITION at boot time (Err path → WARN + counter +
  None field + continue) is not. The composition contract
  matters because a future refactor changing
  `AppState::from_config` to `?`-propagate the fetch failure
  silently violates the non-fatal-boot contract and the prose
  is the only guard. A 30-LOC lib test exercising
  `AppState::boot_with_fake_node_id_fetch(Err(...))` would
  pin the composition. Carries to r28 backlog.

- **NEW [R27-T4] R26-I1 DRY collapse landed without a parity
  test for the unified reader.** The 36-LOC WakeSnapshotMeta
  duplicate at `wake_machine.rs:712-769` was deleted in
  `b5ec01a1`; the wake-side now calls
  `restore_handler::read_snapshot_row` directly via the
  precursor commit's pub(crate) bump. Brief asked the
  test-coverage angle: are there parity tests pinning the
  two stay in sync? — moot, the duplicate is gone. Brief
  asked: if they DRY-collapse, what's the test plan to
  validate the helper signature? — **NO unit test for
  `read_snapshot_row` exists at HEAD.** Coverage is via
  pg-gated e2e tests (the wake_machine_classifies_* trio
  + the restore-side terminal tests). A SQL-shape regression
  (column reordering, type change) would surface as a
  pg-gated test break — but a Rust-side change to the
  `SnapshotRowMeta` field set (e.g., adding a field the wake
  path doesn't consume but the cold-boot path requires)
  would NOT be caught by any test before reaching e2e. The
  ask is small (~40 LOC pg-gated test asserting
  `read_snapshot_row` returns the expected SnapshotRowMeta
  for a seeded snapshot row) and the cost of NOT having it
  scales with the number of consumers, currently 5.

- **NEW [R27-T5] stress-r4 surfaced a "STOP returns before
  CH process exits" timing wedge — STOP fast-path has no
  contract test for downstream lock release.** Stress-r4 §
  "Failure breakdown" identifies the cycle:
  `cycle N: alloc → CH holds rootfs.img EX lock → STOP returns 200
  → controller's Nomad job-stop is async, CH not yet reaped,
  lock not yet released → cycle N+1: wake → AlreadyLocked`.
  The STOP handler at `handlers.rs` returns 200 the moment
  the controller-side cleanup completes, NOT when the Nomad
  alloc + CH process actually terminate. No lib test names
  this as a contract ("STOP HTTP semantics — what's
  guaranteed about cleanup state at 200-return?"). Severity
  IMPORTANT — this is the same shape as R23-I1 (typed-error
  contract was missing; failure was caught by cluster). See
  [R27-T5] for testable form.

- **NEW [R27-DOC1] cold-boot Constraints commit (`9b623f44`)
  message claims "+4 tests" but the diff adds zero tests.**
  Verification: `git diff 9b623f44^..9b623f44 -- crates/
  sandbox/src/backend/nomad_ch.rs | grep "^+\s*#\[test\]"` →
  empty. The "+4 tests" referenced are presumably the +3
  tests that landed one commit later in `d71f1a8c` (restore-
  path) plus the cross-emitter parity test (which DOES
  exercise the cold-boot builder). Documentation accuracy
  issue, not a coverage issue — flagging for r27 backlog
  because the parity test bundles the cold-boot-only
  pin into the restore-side commit, making it harder to
  audit per-commit test deltas. Trivial fix: split the
  parity test into its own commit, or amend the cold-boot
  commit message. Process-discipline carrier, R26-T5 lineage.

- **NEW [R27-S1] sanitize_filesystem_paths preserves /tmp
  paths intact — operator-readable but typed-id-leaking.**
  R22-S1 widening intentionally preserves `/tmp/...` and
  `/usr/local/bin/...` for binary-path readability (see the
  whitelist-by-prefix design at `wake_machine.rs:1063`).
  Issue: cloud-hypervisor writes its API socket to
  `/tmp/cloud-hypervisor.sock.<sandbox-uuid>` in some
  configurations. A driver-msg containing this path would
  surface verbatim through the pg `error_message` column —
  the path itself is not sensitive, but the UUID
  suffix doxxes the sandbox identifier (already redacted
  by the typed-ID pass IF it had typed-prefix form, but
  raw UUIDs are not in the typed-id allow-list at line
  1118). 1-line widening: extend `strip_typed_ids` to also
  match the bare Uuid::simple form (`[0-9a-f]{32}`) when
  surrounded by word boundaries. ~20 LOC test for both shapes.
  Severity MINOR — the leak is sandbox_id (not user_id /
  tenant identity); requires correlation with separate
  pg query to escalate.

- **NEW [R27-T6] sweep tests are pure but the destructive
  body has 1 pg-gated test only.** R25-T4's closure pinned
  every FS / DB gate via pure helpers (13 tests). The
  end-to-end `run_host_dir_gc_once` orchestration (loop
  body, error handling around `rm_rf`, count accumulation)
  still has just 1 pg-gated test (`run_host_dir_gc_once_
  honors_db_gate` or similar). If the loop body silently
  swallows an `rm_rf` error and the classifier-returned
  count fails to decrement, the helper-test green light
  doesn't catch it. Severity MINOR — the helpers are the
  load-bearing logic; the orchestration is well-shaped.
  ~30 LOC pg-gated test would close.

- **Pre-existing-failure carries from brief**: R23-I1
  (terminal-overwrite WARN error_code threading) CLOSED at
  `1c255a00`. R24-I1 / R24-I2 CLOSED (folded into convergent
  fixer). R25-I1 / R25-I2 / R25-S1 CLOSED at `022f778a` +
  `79871194`. R25-T4 CLOSED at `901dfbf2`. R25-T5 entry-side
  CLOSED via `3d431eb8`; **exit-side STILL OPEN** (R26-T1
  carry → R27-T1 status). R26-T2 / R26-T3 CLOSED at
  `883df7fe` + `d71f1a8c`. R26-T4 **PARTIALLY** CLOSED
  (carries to R27-T3). R26-I1 CLOSED at `b5ec01a1` but
  introduces R27-T4 carry.

- **Stress-r4 verdict (NEW)**: RED at 3/60 e2e (5.0 %). New
  failure surface (CH `rootfs.img` `AlreadyLocked`) — the
  r3-A node-affinity bundle did its job (smoke 1/1, CREATE
  60/60) and **moved the bottleneck**. R27-T1 is the
  testable closure ahead of the next fix landing. The
  6-cycle WORKER_COUNT=2 placement-audit (R26-T6) is now a
  REGRESSION guard for r3-A, not a diagnostic — still
  un-landed.

## CRITICAL

None.

## IMPORTANT

### [R27-T1] [NEW] Stress-r4 WAKE wedge has no unit-test surface — HIGHEST LEVERAGE

- **Where**: stress-r4 review at `docs/reviews/sandbox-
  snapshot-restore-cluster-2026-05-25-T8b-stress-r4.md:67-
  113` documents the WAKE failure as a deterministic CH
  `rootfs.img` `ExclusiveWrite` lock collision across
  STOP → cycle-N+1 WAKE boundary. The cycle:
  ```
  cycle N:  alloc creates → CH spawns → CH holds rootfs.img EX lock
            → STOP returns 200 (controller-side complete)
            → CH process not yet reaped, Nomad job-stop async
  cycle N+1: wake alloc → restore → CH AlreadyLocked → fail
  ```
- **What's missing**: NO lib test pins the contract "STOP →
  next WAKE for same sandbox_id must succeed." The Backend
  trait at `crates/sandbox/src/backend/mod.rs` defines
  `stop_for_real()` and `restore_from_snapshot()` (or
  equivalent) but no test fixture exercises sequential
  stop→restore against the same sandbox_id observing the
  `rootfs.img` lock contention. The `stop_preserving_state_
  does_not_remove_host_dir` test at `nomad_ch.rs` covers
  state preservation but NOT lock release semantics.
- **What WOULD close it**: 2 representative lib tests
  exercising the timing contract:
  1. **Helper-level**: a pure test asserting "for a per-
     sandbox `rootfs.img` path, no two concurrent
     `OpenOptions::write(true)` handles can coexist." This
     is a OS-semantics pin that documents what r4's bug
     depends on (Linux fcntl/flock semantics + CH's
     virtio-blk lock_type=Write). Trivial — 15 LOC.
  2. **Backend-level**: drive `stop_for_real()` against
     `RealNomadCHBackend`, then immediately attempt to
     `OpenOptions::write(true).open(rootfs_path)` from the
     test thread. If the lock is held, the test reproduces
     stress-r4's failure shape inside a unit test. ~60 LOC
     using existing fake-nomad fixtures.
  - Together these document the contract failure-side BEFORE
    the production fix lands. The fix likely lives in
    `stop_for_real()` (block until CH process exit + flock
    release) or in `submit_restore_job()` (poll for lock
    availability with backoff) — either path needs a test
    anchor.
- **Why this matters**: stress-r4 is RED with this single
  dominant mode. The next bundle (r4-A?) will ship a fix; if
  it ships without a test anchor, the same shape can land
  again under a different cycle pattern (e.g., snapshot then
  immediate wake, vs stop then immediate wake). This is the
  same lesson as R26-T3/T4 (write the test BEFORE the fix).
- **Severity IMPORTANT**. Highest-leverage open ask in r27
  given the active stress wedge. ~75 LOC. Land BEFORE the
  r4-A fix commit, not after.

### [R27-T3] [NEW] R26-T4 partial-closure: boot-time composition contract unpin

- **Where**:
  - Production: `lib.rs:670-698` (the
    `fetch_local_nomad_node_id().await { Ok(id) => Some(id),
    Err(e) => { inc_counter(); warn!(); None } }` composition).
  - Tests at HEAD: parser (5 cases) + counter monotonicity (1
    case) — **individual pieces tested, composition not**.
- **What's missing**: a lib test asserting the boot path's
  contract:
  ```
  Given: a fixture AppState boot configured with a
         nomad_addr that returns 404 on /v1/agent/self
  When:  AppState::from_config(cfg).await is invoked
  Then:  Ok(state) is returned
   AND:  state.local_nomad_node_id == None
   AND:  nomad_node_id_lookup_failures_value() incremented
   AND:  jobspec_builder_with_state_returns_no_constraints()
  ```
- **What WOULD close it**: a single ~30 LOC test using the
  existing `spawn_404_mock` (or equivalent) fixture pattern,
  plus a follow-up assertion that the resulting backend's
  builder emits an OMIT Constraints jobspec (links R27-T3 to
  R26-T2's parity test by exercising the SAME builder under
  the SAME state — should be a one-line addition).
- **Why this matters**: the comment block at lib.rs:225-244
  is prose. A future refactor changing the boot path to
  `?`-propagate the fetch failure violates the contract and
  the prose is the only guard. Stress-r4 showed the
  controller boot DOES happen against a healthy Nomad agent
  in cluster — the Err path is exercised in the cluster
  only when the agent restarts mid-controller-boot, which
  is impossible to schedule deterministically without a
  chaos test. Lib test is the only oracle.
- **Severity IMPORTANT**. 30 LOC ask. Mirrors R27-T1 in
  spirit (composition contract that "the pieces work
  individually" doesn't catch).

### [R27-T1-CARRY] R26-T1 / R25-T5 carries: verbatim-msg propagation exit tests — 6th cycle

- **Where**: chain unchanged from r26:
  - Entry: `extract_failed_task_event_msgs` at `nomad_ch.rs:
    2776-2843` (9 helper tests).
  - Cold-boot composition: `nomad_ch.rs:2645`
    (`wait_for_alloc_running`).
  - Restore composition: `restore_handler.rs:2577-2588`.
  - Sanitize: `wake_machine.rs:790`
    (`sanitize_error_message`, 28 tests at the function).
  - Pg write: `wake_machine.rs:165-181`.
  - Wire egress: `GET /admin/sandboxes/{id}/wake/{wake_id}`
    200 body `message` field.
- **What still has 0 tests**: the chain from
  `wait_for_alloc_running` Err → `RestoreHandlerError::
  Backend(String)::to_string()` → `sanitize_error_message`
  → pg write → wire render. R22-S1's 7 new sanitize tests
  pin the function in isolation; the propagation OF a
  driver-msg through that function to pg is un-pinned.
- **Why r27 escalates this carry**: stress-r4 (46/51 WAKE
  failures) makes verbatim-msg propagation operationally
  critical. Operators diagnosing 46 wedged sandboxes need
  the verbatim CH `AlreadyLocked` text in pg + admin queries
  to triage. If R22-S1's sanitizer accidentally strips the
  CH path or the typed-id form mid-msg, operators lose the
  diagnostic and need to ssh+grep journald (the only
  fallback). The cluster test (stress-r4) is the wire-egress
  oracle today; that's the same shape as the cluster being
  the test for everything else.
- **What WOULD close it**: 2 tests representative (carried
  unchanged from r26):
  - **pg-gated** at `sandbox_pg_e2e.rs:5530` (next to the
    R23-I1 terminal-overwrite tests): drive
    `wake_machine_classifies_submit_failure` with a stub
    `StubRestoreBackend::submit_returns_error_with_text(
    "ch: rpc error: code = Unknown desc = ch: ...
    AlreadyLocked")`. Assert post-write `final_row.
    error_message` contains "AlreadyLocked" AND
    "rpc error" (operator-actionable substrings) AND the
    sanitizer redacted the alloc-dir path. ~30 LOC on top
    of existing fixtures.
  - **admin-handler** at `sandbox_admin_e2e.rs`: seed a
    `wake_jobs` row with the verbatim text pre-redacted;
    hit `GET /admin/sandboxes/{id}/wake/{wake_id}` under
    `AdminRole::ReadWrite`; assert the 200 body `message`
    contains the substrings. ~40 LOC.
- **Severity IMPORTANT**. 6-round carry. With stress-r4 in
  the rear-view, this is no longer hypothetical — operators
  are reading `wake_jobs.error_message` for 46 wedged rows
  RIGHT NOW.

### [R27-T5] [NEW] STOP HTTP semantics — what's guaranteed at 200-return?

- **Where**: STOP handler at `handlers.rs` returns 200 the
  moment controller-side cleanup completes. Nomad job-stop
  is async; CH process exit + `rootfs.img` lock release are
  downstream of that.
- **What's missing**: no test names the contract STOP→200
  semantically promises. The contract today is implicit
  ("controller is done; Nomad will tear down eventually");
  stress-r4 surfaces the failure mode when "eventually" is
  longer than the next-wake interval. Without a documented
  contract, the fix space is open: tighten to async-and-
  wait, leave async-and-warn, or fix at submit_restore_job
  with a lock-acquire backoff.
- **What WOULD close it**: a test bundle pinning the
  current contract (whatever the fix lands as):
  1. **Optimistic STOP**: `stop_returns_immediately_before_
     ch_exit_when_async_mode` — pin the fast-path
     semantics.
  2. **Conservative STOP**: `stop_waits_for_lock_release_
     when_conservative_mode_set` — pin the (proposed)
     synchronous semantics behind a feature flag.
  3. **WAKE precondition**: `submit_restore_job_blocks_on_
     existing_lock_with_backoff` — pin the wake-side gate.
  - At minimum 1 of these 3 needs to land with whatever
    fix ships for the stress-r4 wedge.
- **Why this matters**: R23-I1's lesson was "typed contract
  for error shapes prevents silent regression." R27-T5 is
  the analog for STOP timing semantics. Without a test,
  the next refactor can shift the contract without any CI
  signal.
- **Severity IMPORTANT**. New ask post stress-r4. Land WITH
  the r4-A fix commit.

### [R27-T6-CARRY] R26-T6 placement-audit cluster test — STILL OPEN

- **Where**: no harness landed; the brief flagged
  `tests/cluster_placement_audit.sh` (WORKER_COUNT=2, 6
  cycles, assert allocs land on staging worker).
- **What changed since r26**: stress-r4 validated r3-A
  works at CREATE (60/60). The placement-audit cluster
  test's role shifted from "catch the bug pre-stress" to
  "regression guard for r3-A." Lower urgency, same value.
- **What WOULD close it**: per r26's spec — 6 cycles × 2
  workers × ~30s cold-boot per = ~6 min wall, ~$2-3/run.
  The assertion is `Allocation.NodeID == controller.local_
  nomad_node_id` at submit time. Recoverable from controller
  logs OR via Nomad API `GET /v1/allocation/{alloc_id}`
  post-cycle.
- **Severity IMPORTANT**. Carry unchanged. Land before r28
  or formally defer.

## MINOR

### [R27-T2] [NEW] R25-T2 cold-boot mirror test demoted MINOR

- **Where**: ask was `create_must_stage_workspace_img_
  before_submit_nomad` mirroring `submit_restore_job_
  rejects_missing_workspace_img` at `restore_handler.rs:
  3362`. Still un-landed.
- **What changed since r26**: stress-r4's CREATE 100 % proves
  r3-A node-affinity closes the cross-node-ENOENT failure at
  a higher abstraction layer. The mirror test's original
  motivation (cluster-failure reproduction) is gone.
- **Remaining value**: defensive invariant — "controller MUST
  NOT call submit_nomad_job for a workspace.img that can't
  be stat'd locally." Codifies a should-never-happen state
  to surface refactors that break it.
- **Severity MINOR (demoted)**. ~80 LOC ask. Defer to a
  future "Backend API contract" test bundle if not picked up
  individually.

### [R27-T4] [NEW] R26-I1 DRY collapse: no unit test for unified `read_snapshot_row`

- **Where**: `restore_handler.rs:781` (`pub(crate) async fn
  read_snapshot_row`). 5 consumers across `restore_handler.rs`
  and `wake_machine.rs:272`.
- **What's missing**: NO direct unit test (lib or pg-gated)
  exercises `read_snapshot_row` independently. Coverage is
  via consumers: `wake_machine_classifies_*` (3 pg tests),
  `do_restore_inner` e2e (covered indirectly via the
  restore-side pg tests).
- **Why this matters now**: the DRY collapse landed at
  `b5ec01a1` — wake-side now relies on the same reader as
  cold-boot/restore. A future field addition (e.g., new
  `rootfs_path: PathBuf` to support the stress-r4 fix in
  R27-T1) would need to compile-check at all 5 consumer
  sites but isn't pinned with a "shape" test. Cost of
  missing test scales with consumer count.
- **What WOULD close it**: a single pg-gated test at
  `sandbox_pg_e2e.rs::read_snapshot_row_*`:
  ```
  let sid = seed_snapshot_row(&db, ...).await;
  let meta = read_snapshot_row(&db, sid).await.unwrap();
  assert_eq!(meta.sha256, "<known>");
  assert_eq!(meta.vm_index, 7);
  assert_eq!(meta.user_id, "usr_alice");
  assert!(meta.artifact_path.contains("snap_"));
  ```
  ~40 LOC.
- **Severity MINOR**. New ask, post-R26-I1 closure. Land
  with the next snapshot-row schema change or proactively
  in r28.

### [R27-S1] [NEW] sanitize_filesystem_paths whitelist preserves /tmp paths intact

- **Where**: `wake_machine.rs:1063` (`strip_filesystem_paths`)
  whitelist-by-prefix on `/var/zeroship/`, `/opt/nomad/`,
  `/etc/zeroship/`. Everything else (incl. `/tmp/...`,
  `/usr/local/bin/...`) is preserved.
- **What stress-r4 surfaces**: CH writes its API socket to
  `/tmp/cloud-hypervisor.sock.<sandbox-uuid>` (verified via
  `crates/sandbox/scripts/nomad-vm-wrapper.sh`). A driver
  error mentioning this path would surface the bare UUID
  through pg `error_message` to RO admin bearers.
- **Why this is MINOR not IMPORTANT**: the UUID exposed is
  the sandbox_id (not the user_id / tenant identity); RO
  bearers can already see the same sandbox_id via
  `sandboxes` table reads; the path itself is not sensitive
  (`/tmp/cloud-hypervisor.sock.*` is well-known).
- **What WOULD close it**: extend `strip_typed_ids` to
  also match the bare `Uuid::simple` form (32 hex chars,
  word-boundary-bounded) — ~10 LOC code + 3 LOC test.
  Alternative: extend the whitelist with `/tmp/cloud-
  hypervisor.sock.*` redaction — narrower, ~5 LOC.
- **Severity MINOR**. R22-S1 closure was correctly scoped
  to the high-leverage leak paths; this is a secondary leak
  surface stress-r4 incidentally surfaces.

### [R27-T6-LIB] [NEW] Sweep orchestration body has 1 pg-gated test only

- **Where**: `sweep.rs:run_host_dir_gc_once` (220 LOC, post-
  R25-T4 helper extraction). The body's loop + error handling
  around `tokio::fs::remove_dir_all` + scan-count
  accumulation is NOT directly unit-tested.
- **What's missing**: a pg-gated test asserting the
  orchestration body's count semantics: "given a directory
  with 5 candidate UUIDs of which 3 pass DB-eligibility, 2
  rm_rf calls succeed, 1 fails with EACCES — the returned
  `(scanned, reaped)` is (5, 2) and the failure path
  doesn't poison the loop."
- **What WOULD close it**: ~30 LOC pg-gated test seeding
  the host_state_dir + sandbox rows + injecting a
  permission-denied entry. The helpers are well-tested
  (13 tests post-R25-T4); the orchestration is not.
- **Severity MINOR**. The helpers are load-bearing; the
  orchestration is well-shaped. Low-leverage add. Carry to
  r28.

### [R27-DOC1] [NEW] Cold-boot Constraints commit message inaccuracy

- **Where**: commit `9b623f44` message claims "Tests +4"
  but the diff adds zero tests in `nomad_ch.rs`.
  Verification: `git diff 9b623f44^..9b623f44 -- crates/
  sandbox/src/backend/nomad_ch.rs | grep "^+\s*#\[test\]"`
  → empty.
- **What's accurate**: the "+4 tests" referenced are the +3
  tests + 1 cross-emitter parity test that ship one commit
  later in `d71f1a8c` (restore-path). The parity test does
  exercise the cold-boot builder, so its functional
  coverage spans both commits — but the commit-message
  attribution is misleading and makes per-commit test-delta
  audits harder.
- **What WOULD close it**: amend the cold-boot commit
  message to say "Tests: companion parity test ships with
  the restore-path commit (d71f1a8c)." Trivial.
- **Severity MINOR**. Documentation-discipline carrier;
  R26-T5 lineage.

### [R26-T7-CARRY] r3-C case-insensitive Type match

- **Where**: `nomad_ch.rs:4699` (`is_diagnostic_event_type_
  matches_known_types`).
- **Status**: **CLOSED**. Verified at HEAD: the test at
  line 4703 (`assert!(is_diagnostic_event_type("driver
  failure"));`) and line 4704 (`assert!(is_diagnostic_event_
  type("  Driver Failure  "));`) cover lowercase + trim.
  The production code presumably uses `.trim().eq_ignore_
  ascii_case(...)` to match. R26-T7 closed silently within
  the r3-C bundle.

## Trend table — r17 → r27

| Cycle | sandbox lib | pg-gated | Δ lib | Δ pg | Notes |
|-------|-------------|----------|-------|------|-------|
| r17   | 373         | 74       | +29   | +9   | PR1+PR2 |
| r18   | 402         | 83       | +29   | +9   | PR2-FOLLOWUP + GATE-C2 + C-7-LT-1 |
| r19   | 424         | 87       | +22   | +4   | C-7-LT-2 + R17-S1 + R18-I1 + R19-C1 + R19-I1 |
| r20   | 426         | 87       | +2    | 0    | R19-I4 shims + R19-T1 widening |
| r21   | 428         | 87       | +2    | 0    | r17-Q3 DataIntegrity round-trip |
| r22   | 432         | 89       | +4    | +2   | R10-API4 + R20-C1 + R22-I1 counter |
| r23   | 432         | 89       | 0     | 0    | (no audit; smoke-r23 cycle) |
| r24   | 433         | 91       | +1    | +2   | R22-T1 parity + R23-I1 counter e2e |
| r25   | 454         | 91       | +21   | 0    | T1 admin-RO bundle (~16) + v34 helper (5); pg unchanged |
| r26   | 459         | 91       | +5    | 0    | r3-C diagnostic-event-preference (4) + is_diagnostic allow-list (1) |
| **r27** | **488**   | **91**   | **+29** | **0** | r3-A precursor parser (+6) + R25-T4 sweep (+13) + r3-A restore parity (+3) + R22-S1 sanitize (+7); pg unchanged |
| r9→r27 | +192       | +17      | —     | —    | wedge + sweep + sanitizer + probe + retry + drift + envelope + parity + counter-e2e + RO-admin + driver-msg helper + diagnostic-event preference + r3-A node-affinity + sanitize widening |

Of the +29 lib delta from r26→r27:
- **+6 r3-A precursor**: 5 parser shapes + 1 counter
  monotonic. (`883df7fe`)
- **+13 R25-T4 sweep**: 7 FS-gate classifier + 5 DB-gate +
  1 host_dir_gc_grace floor pin. (`901dfbf2`)
- **+3 r3-A restore-path + parity**: includes / omits +
  cross-emitter parity contract. (`d71f1a8c`)
- **+7 R22-S1 sanitize**: var/zeroship + opt/nomad + etc/
  zeroship + typed-id + combination + preserves + idempotent.
  (`7647cd4d`)

ZERO of the +29 are at the verbatim-msg-propagation EXIT
hops (`wake_jobs.error_message` round-trip / admin handler
`message` field). Trapezoid-of-coverage at the propagation
chain widens: 9 tests at the helper entry, 28 at the
sanitize function, 0 between sanitize-call-site and wire-
egress. 6-round carry.

## Carry-forward table (r26 → r27)

| Tag | r26 status | r27 status | Note |
|-----|------------|------------|------|
| R25-T1 stress harness polling invariant | IMPORTANT, OPEN | **STILL OPEN** | No `tests/stress_harness_invariant.rs` landed. |
| R25-T2 cold-boot workspace.img preflight | IMPORTANT, OPEN (4th) | **DEMOTED MINOR (5th)** | stress-r4 CREATE 60/60 closes the cluster-failure motivation; remaining ask is defensive invariant only. See R27-T2. |
| R25-T3 driver multi-cycle race | IMPORTANT, OPEN | **STILL OPEN** | r3-B (cross-worktree) added 3 tap-collision-poll tests for a different race. Multi-cycle vm_index reuse unchanged. |
| R25-T4 sweeper unit tests | IMPORTANT, OPEN | **CLOSED at `901dfbf2`** | 13 new tests via pure-helper extraction. Quality is high. |
| R25-T5 verbatim-msg exit tests | PARTIAL CARRY → R26-T1 | **STILL OPEN (R27-T1-CARRY)** | Entry-side closed at r3-C; exit-side 0 tests still. 6-round carry. |
| R25-T6 UTF-8 truncation safety | MINOR, OPEN | **OPEN, carry** | 1-line fix + 1 test still un-landed. |
| R26-T1 verbatim-msg exit tests | IMPORTANT, OPEN | **STILL OPEN (R27-T1-CARRY)** | See R25-T5 carry. |
| R26-T2 r3-A cross-emitter parity | IMPORTANT, OPEN | **CLOSED at `d71f1a8c`** | `node_affinity_constraints_parity_between_cold_boot_and_restore_emitters`. Asserts both Some/None shapes byte-identical. |
| R26-T3 fetch_local_nomad_node_id parser | IMPORTANT, OPEN | **CLOSED at `883df7fe`** | 5 parser shape pins via pure-helper extraction (cleaner than the brief's mock-server ask). |
| R26-T4 r3-A boot-failure fallback | IMPORTANT, OPEN | **PARTIALLY CLOSED** | Parser + counter tested individually; composition contract un-pinned. See R27-T3. |
| R26-T5 4-for-4 ADR CI guard | MINOR | **OPEN, deferred** | Process-discipline; unlikely to land. R27-DOC1 is a sibling. |
| R26-T6 placement-audit cluster test | IMPORTANT, OPEN | **STILL OPEN (R27-T6-CARRY)** | Now regression-guard not diagnostic; stress-r4 validated r3-A architecturally. |
| R26-T7 case-insensitive Type match | MINOR | **CLOSED** | Verified at `nomad_ch.rs:4699-4714` (trim + lowercase covered). |
| R26-I1 SnapshotRowMeta DRY | code-quality | **CLOSED at `b5ec01a1`** | Pure refactor; no parity test added — new R27-T4 ask. |
| R22-T3 retry-race pg test | IMPORTANT, OPEN (6th) | **OPEN, 7th** | Concurrency lens silent again this round. |
| R21-T1 r17-Q3 DataIntegrity | MINOR, OPEN (7th) | **OPEN, 8th** | Carry. |
| R18-T7 probe_and_classify loops | OPEN, carry | OPEN, carry | Still 0 hits. |
| R18-T9 OS-thread detach pattern | OPEN, carry | OPEN, carry | 4 sites uncovered. |
| R19-T2 takeover_once lib coverage | OPEN, carry | OPEN, carry | Counter wired; loop untested. |
| R19-T3 R19-C1 claim-count metric | OPEN, carry | OPEN, carry | Co-file with R22-T3. |

## Bundle-specific would-have-caught analysis

### Does r3-A close R25-T5?

**No — entry-side closure unchanged from r26.** r3-A is a
placement-pinning fix; the verbatim-msg propagation chain
is orthogonal. Stress-r4 validated r3-A architecturally
(CREATE 60/60) but the WAKE 46/51 RED rows that operators
are reading from pg right now ARE the chain — and the
admin-handler-side `message` render is still un-pinned.

### Does R25-T4's pure-helper extraction sufficiently cover the host_dir GC?

**Yes for the gate matrix; partial for the orchestration.**
The 13 tests pin every FS gate (skip-users / non-UUID /
non-dir / under-grace / grace-boundary / clock-skew) and
every DB gate (absent / terminal / non-terminal /
snapshotted / pending-wake-veto). The `run_host_dir_gc_once`
body — the `rm_rf` calls, error accumulation, count
semantics — has 1 pg-gated test. See R27-T6-LIB. The
extraction is high quality; the residual gap is small
(orchestration body is ~30 LOC after the helpers carry
the gate logic).

### Does R22-S1's 7 new tests sufficiently cover the sanitize widening?

**Yes for the sanitizer surface; the propagation chain
remains the gap.** Verified: `sanitize_error_message` has
ONE production call site at `wake_machine.rs:165` (the
Phase::Failed → pg write boundary). The single-site
posture is intentional; 7 tests + the 21 pre-existing
sanitize_* tests = 28 tests at the function. The chain
FROM `wait_for_alloc_running` Err TO `sanitize_error_
message` is un-pinned (R27-T1-CARRY).

### Does r3-A's restore-path commit close R25-T2?

**No — different layer.** r3-A closes the cross-node
ENOENT at the placement-constraint layer (Nomad scheduler
won't pick the wrong worker); R25-T2 asks for the
controller-side preflight to reject before submit. The
restore-side preflight exists (`submit_restore_job_
rejects_missing_workspace_img`); the cold-boot side does
not. Stress-r4 makes R25-T2 lower priority (cluster no
longer surfaces the failure) but the defensive contract
ask stands. See R27-T2.

### Pre-existing-failure closure status (per brief)

| Tag | r27 status | Source |
|-----|------------|--------|
| R23-I1 | **CLOSED** | terminal-overwrite WARN at `1c255a00`; pg-gated tests at `sandbox_pg_e2e.rs:5530, :5617`. |
| R24-I1 | **CLOSED** | `1c255a00` (error_code + error_message threaded into terminal-overwrite WARN). |
| R24-I2 | **CLOSED (rolled in)** | Folded into convergent fixer at `79871194` + `022f778a`. |
| R25-I1 | **CLOSED** | Same convergent fixer. |
| R25-I2 | **CLOSED** | Same convergent fixer. |
| R25-S1 | **CLOSED** | Same convergent fixer (typed `WakeErrorCode::StagingPathMissing`). |
| R25-T4 | **CLOSED (NEW this cycle)** | `901dfbf2` pure-helper extraction + 13 tests. |
| R26-T2 | **CLOSED (NEW this cycle)** | `d71f1a8c` parity contract test. |
| R26-T3 | **CLOSED (NEW this cycle)** | `883df7fe` parser tests. |
| R26-T4 | **PARTIALLY CLOSED (NEW this cycle)** | Pieces tested; composition not. R27-T3 ask. |
| R26-I1 | **CLOSED (NEW this cycle)** | `b5ec01a1` DRY collapse; R27-T4 ask follows. |
| R26-T7 | **CLOSED (NEW this cycle)** | `nomad_ch.rs:4699-4714` covers case-insensitive + trim. |

10 closures + 1 partial closure since r26 review. **The
most productive cycle since r25's PR1+PR2 bundle.**

### Working-tree compile cleanliness

At HEAD `01288c18`: `cargo test -p zeroship-sandbox --lib`
= **488 PASS / 0 fail / 1 ignored** (verified this audit).
Sole ignored test is `snapshot_store_gcs::tests::gcs_live_
round_trip` (requires GCS credentials; expected). Build
clean.

DATABASE_URL='' `cargo test -p zeroship-sandbox --test
sandbox_pg_e2e` = **0 PASS / 0 FAIL / 91 ignored**
(verified this audit). All pg-gated tests are clean-skip
when DATABASE_URL is unset. No pg-gated test silently
no-ops or panics on missing env; the `#[ignore = "needs
Postgres; …"]` annotations are consistent across all 91
tests.

Pre-existing `sandbox_preview_share_e2e.rs` 5 failures
flagged in r26 brief: NOT verified this audit (out of
test-coverage-lens scope; carries to preview lens).

## What stress-r4 (RED, 3/60) and any stress-r5 will
exercise that unit tests miss

Re-enumerated against the four cluster-only surfaces post
r3-A landing + stress-r4 verdict:

1. **CH `rootfs.img` `ExclusiveWrite` lock collision
   across STOP → cycle-N+1 WAKE** (NEW from stress-r4).
   46/51 wedged. R27-T1 lib test is the only path to anchor
   the contract before the r4-A fix lands.

2. **Multi-cycle `vm_index` race** (R25-T3 carry,
   cross-worktree driver-side). Stress-r4 reports 11
   stranded `zsbx-nm-*` interfaces on w1 — consistent with
   the multi-cycle race shape. r3-B closed the EBUSY
   collision (single-cycle race) but not the multi-cycle
   reuse. Still cluster-only.

3. **Sweeper non-racing live wakes** (R25-T4 closed at
   the helper level; orchestration body has 1 pg test).
   Stress-r4 reports 20 host_dirs un-reaped — consistent
   with the 600s grace window not elapsing. R27-T6-LIB
   would pin the orchestration's count semantics; the
   bigger gap (grace-vs-cycle-cadence interaction) needs
   pg-gated coverage that doesn't exist.

4. **Boot-time `fetch_local_nomad_node_id` non-fatal
   fallback** (R26-T4 partial closure → R27-T3 ask).
   Stress-r4 boots successfully against healthy Nomad
   agent; the Err path is exercised only via chaos test
   (deliberate agent restart mid-controller-boot), which
   the stress harness doesn't do. R27-T3 lib test is the
   only oracle.

### Gaps cluster signal still has to close (independent
of stress-r4 verdict)

- **R27-T1 lock-collision contract** — testable in lib;
  stress-r4 is the ONLY oracle today and it's RED.
- **R27-T3 boot-time composition** — testable in lib;
  cluster cannot exercise without chaos test.
- **R27-T5 STOP timing semantics** — testable in lib;
  cluster catches asymmetrically (stress-r4 surfaced via
  cycle-N+1 collision, but the underlying timing contract
  isn't named).

## Lib tests gating cutover

- **488 lib + 91 pg-gated + ~75 other integration** (admin/
  persist/preview/typed_id e2e). Plus driver-side counts
  unchanged this round.
- **stress-r5 gate sufficiency** (post-r4-A fix landing —
  whatever shape it takes): existing 488 lib pin the r3-A
  bundle. **Seven high-leverage seams remain un-pinned**:
  - R27-T1 stress-r4 rootfs.img lock contract (NEW)
  - R27-T3 r3-A boot-failure composition (CARRY from
    R26-T4 partial closure)
  - R27-T5 STOP HTTP timing semantics (NEW)
  - R26-T1 verbatim-msg exit tests (6-round carry)
  - R25-T3 multi-cycle vm_index race (cross-worktree)
  - R27-T2 cold-boot workspace.img preflight mirror
    (demoted MINOR)
  - R27-T6-LIB sweep orchestration body (MINOR)
- **T-8b-cutover gate sufficiency**: **INSUFFICIENT until
  R27-T1 lands** (and the r4-A fix lands behind it). The
  cluster IS the test for the WAKE wedge today. R27-T3 +
  R26-T1 are next-priority for hardening the lib-side
  oracle.

## Cross-lens consensus

- **R27-T1** (stress-r4 lock collision): test-cov + cluster
  + concurrency. Cluster lens caught the failure; test-cov
  asks for the lib anchor; concurrency-lens may have
  upstream view on the file-lock semantics.
- **R27-T3** (R26-T4 partial closure): test-cov +
  api-surface. Composition contract is api-surface domain;
  test enforces it.
- **R27-T1-CARRY** (R26-T1 verbatim-msg): test-cov +
  security + cluster. Security-r25 R25-S1 closed the typed-
  error half; test-cov half remains. Stress-r4 elevates
  this from hypothetical to operational.
- **R27-T4** (R26-I1 unified reader): test-cov + code-
  quality. R26-I1 closed the dup; R27-T4 asks for the
  contract test that the brief flagged as a possibility.
- **R27-T5** (STOP semantics): test-cov + api-surface +
  concurrency. STOP HTTP contract is api-surface; timing
  contract is concurrency; the test pinning both is
  test-cov.
- **R27-S1** (sanitize whitelist UUID): test-cov +
  security. Secondary leak surface to R22-S1 closure.
- **R26-T5** (4-for-4 ADR CI): test-cov + architecture.
  R27-DOC1 is a sibling discipline-carrier.
- **R27-T6-CARRY** (placement-audit cluster): test-cov +
  cluster. Now regression-guard, not diagnostic.

## Lens hand-off

- **api-surface r27+**: R27-T5 (STOP HTTP timing semantics)
  is a contract-shape ask. The STOP handler's 200-return
  semantics need to be named before the r4-A fix lands.
  R27-T3 (boot composition) also lives partially in
  api-surface land.
- **architecture r27+**: R27-T1's fix-shape decision (block
  on lock vs poll-with-backoff vs feature-flag both) is
  architectural. The test (R27-T1) is independent of the
  fix-shape choice; the architecture lens needs to call the
  shape.
- **code-quality r27+**: R27-DOC1 (commit-message accuracy)
  is process-discipline. R25-T6 (UTF-8 boundary safety)
  carries.
- **concurrency r27+**: R22-T3 (R20-T1 retry-race pg test)
  now 7 rounds open. R25-T3 (multi-cycle vm_index race)
  also concurrency-adjacent. R27-T1's lock-release timing
  is the new concurrency surface.
- **security r27+**: R27-S1 (sanitize UUID widening) is a
  small-surface secondary leak. R26-T1 carry's security
  half (typed error path) closed at R25-S1; test-cov half
  remains.
- **cluster r27+**: R27-T6-CARRY (placement-audit) — now
  regression-guard role. Stress-r5 design needs to address
  R27-T1's lock-collision wedge.

## To test-cov r28 backlog (~480 LOC total)

1. **R27-T1** stress-r4 lock-collision contract (helper-level
   + Backend-level). ~75 LOC. **HIGHEST LEVERAGE**. Land
   BEFORE r4-A fix commit.
2. **R26-T1** (R25-T5 6th-round carry → R27-T1-CARRY)
   verbatim-msg exit tests. ~70 LOC pg-gated + admin-handler.
   IMPORTANT.
3. **R27-T5** STOP HTTP timing semantics contract. ~60 LOC.
   Land WITH r4-A fix.
4. **R27-T3** (R26-T4 partial-closure carry) r3-A boot-
   failure composition test. ~30 LOC lib. IMPORTANT.
5. **R27-T6-CARRY** placement-audit cluster harness. ~200
   LOC harness (cluster-lens design). IMPORTANT, now
   regression-guard role.
6. **R25-T1** (3rd-round carry) `snapshot_stress.py`
   polling-loop invariant test. ~120 LOC. IMPORTANT.
7. **R25-T3** (3rd-round carry, cross-worktree) multi-cycle
   `vm_index` race test. ~80 LOC Go. IMPORTANT.
8. **R22-T3** (7th-round carry) retry-race pg tests. ~80
   LOC.
9. **R27-T2** (R25-T2 demoted) cold-boot workspace.img
   preflight mirror. ~80 LOC. MINOR.
10. **R27-T4** (R26-I1 follow-up) `read_snapshot_row`
    pg-gated test. ~40 LOC. MINOR.
11. **R27-T6-LIB** sweep orchestration body pg test. ~30
    LOC. MINOR.
12. **R27-S1** sanitize bare-UUID widening. ~13 LOC. MINOR.
13. **R27-DOC1** commit-message amendment / split.
    Trivial. MINOR.
14. **R25-T6** (3rd-round carry) UTF-8 cap safety. ~20 LOC.
    MINOR.

Delisted this round: R25-T4 (CLOSED), R26-T2 (CLOSED),
R26-T3 (CLOSED), R26-T7 (CLOSED — verified at
`nomad_ch.rs:4699-4714`).

## Notes for r28

- **The shift this round**: r27 is the highest-throughput
  closure cycle since r19/PR1+PR2. **10 closures + 1
  partial + 1 DRY collapse** vs r26's 4 closures. The
  pattern: when the cluster surfaces a failure, the
  test-cov lens asks for a unit-level anchor; this cycle
  3 such asks landed (R26-T2 parity, R26-T3 parser, R25-T4
  sweep). The remaining open asks (R27-T1, R27-T3,
  R27-T5) follow the same pattern post stress-r4.
- **The shift in R25-T2**: stress-r4 closes the cluster-
  failure-motivation for the cold-boot mirror test. The
  ask survives as a defensive invariant only; demoted
  MINOR.
- **The bottleneck-displacement pattern**: r3-A's
  node-affinity bundle moved the WAKE bottleneck from
  cross-node ENOENT to same-host CH lock collision.
  R27-T1 is the testable closure for the new wedge;
  R27-T5 is the contract test for the upstream STOP timing.
  Without these landing with the r4-A fix, the cluster
  continues to be the oracle for both.
- **R27-T1 landing-gate recommendation**: do NOT merge
  r4-A's fix (whatever shape — block-on-lock vs poll vs
  feature-flag) without R27-T1 helper-level test AT
  MINIMUM. R27-T5 + R26-T1 are with-fix asks. R27-T5 names
  the contract; R26-T1 closes the diagnostic-egress test
  chain.
- **Trapezoid-of-coverage chain (6th round)**: 9 entry tests
  + 28 sanitize tests + 0 exit tests. The chain is the
  most-tested function in the codebase at the function-
  level, AND the most-untested chain end-to-end. r27 added
  7 sanitize tests; the chain's mid-section is now
  asymmetrically deep.
- **Pattern carry-forward from r26**: "the marginal gaps
  moved BACK INTO the crate" thesis HOLDS — and r27 is
  the cycle where the crate-side debt got paid down. r27
  closures cluster around r3-A landing-gates (R26-T2/T3 as
  pre-merge tests) and crate-side hygiene (R25-T4
  sweep + R26-I1 DRY collapse). The new debt (R27-T1 +
  R27-T5) is the next layer up — Backend trait timing
  contracts that no test currently names.
- **Highest-leverage closure for r28**: **R27-T1**
  (stress-r4 lock contract) — ~75 LOC, prevents the next
  r4-A fix from landing without an anchor for the wedge
  it claims to close. R26-T1 pg-gated exit test is
  second — closes the trapezoid by one rung; with stress-
  r4's WAKE 46/51 RED, operators are reading these rows
  RIGHT NOW.
- **No emoji, no celebratory framing**: r27 is the
  highest-throughput closure cycle in 10 rounds, the
  trapezoid-of-coverage held for 6 rounds, the placement-
  pinning fix moved the cluster bottleneck not closed it,
  and stress-r4 RED at 3/60 needs R27-T1 as the
  landing-gate for whatever r4-A ships. Net direction:
  active sprint, debt down on landed gates, new debt up
  on stress-r4 surface.
