# Sandbox/snapshot-restore — test-coverage r26 review

Date: 2026-05-25 (UTC). HEAD at audit: `3d431eb8` (worktree tip;
r3-C verbatim-msg propagation fix). Working-tree carries the
**r3-A in-flight** delta on `nomad_ch.rs` + `lib.rs` + `metrics.rs`
(jobspec-emission half NOT yet landed — see below). Round 26.
Prior: `docs/reviews/sandbox-snapshot-restore-test-coverage-
2026-05-25-r25.md` (HEAD `92c45d26`).

Bundle landed since r25 (controller v34 follow-up + driver v14
follow-up):
- `05440498` r3-B (driver, cross-worktree): tap collision poll —
  3 new Go tests (`net_test.go:471 / :517 / :584`).
- `3d431eb8` r3-C: extract_failed_task_event_msgs prefers
  `Driver Failure` over `Alloc Unhealthy` — 4 new lib tests
  (`nomad_ch.rs:4528 / :4575 / :4603 / :4624`).

In-flight (this cycle): **r3-A node-affinity** —
`fetch_local_nomad_node_id` + boot-time fetch + `AppState.
local_nomad_node_id` field + `inc_nomad_node_id_lookup_failure`
counter. Jobspec emission half (Constraints block in
`build_nomad_job_json`) NOT yet in tree. Per brief instructions,
this review treats r3-A as **read-only / in-flight** but flags
the test landscape r3-A will need on landing.

## TL;DR

- **Lib delta r25 → r26: +5** (454 → 459, verified via
  `cargo test -p zeroship-sandbox --lib` at HEAD `3d431eb8` =
  459 PASS / 0 fail / 1 ignored). Pure r3-C bundle: 4 new tests
  pinning the diagnostic-event preference + a 5th `is_diagnostic
  _event_type_matches_known_types` allow-list pin. Pg-gated
  unchanged at 91.
- **R25-T5 (verbatim-msg propagation exit tests) — STILL OPEN.**
  r3-C added 4 tests at the entry point — they pin the helper's
  fixed two-pass selection, not the chain from
  `wait_for_alloc_running` → `RestoreHandlerError::Backend` →
  `sanitize_error_message` → `wake_jobs.error_message` →
  `GET /wake/{id}` 200 `message`. Verified via
  `Grep "extract_failed_task|driver_msgs|disk\[.*workspace\.img"
  crates/sandbox/tests/` → 0 matches. The entry-side fix is
  load-bearing (stress-r3 showed silent no-op against generic
  Nomad envelope) but downstream sanitisation / wire-render hops
  remain un-pinned. Carries → R26-T1.
- **R25-T4 (sweeper unit tests) — STILL OPEN, NOT IN FLIGHT.**
  Brief said "about to be addressed by a parallel fixer this
  cycle". Verified: `git status` shows `crates/sandbox/src/
  sweep.rs` unmodified; `grep -nE "fn |#\[test\]" sweep.rs`
  returns only the pre-existing 3 tests at `:1421` (lifecycle
  pin), `:1456` (cadence pin), `:1467` (threshold pin) — none
  exercise `run_host_dir_gc_once`. The host_dir GC sweeper is
  unchanged from r25. Six destructive-on-misfire gates, zero
  coverage. R26 commentary maintained.
- **R25-T2 (cold-boot `create()`-level mirror test) — STILL
  OPEN.** r3-A's in-flight changes touch `lib.rs` boot path +
  add `fetch_local_nomad_node_id` helper — they do NOT add the
  `create_must_stage_workspace_img_before_submit_nomad` test
  that mirrors `submit_restore_job_rejects_missing_workspace_img`
  at `restore_handler.rs:3295`. R24-T2 carries verbatim into 4th
  cycle.
- **R25-T3 (driver multi-cycle race test) — STILL OPEN.** r3-B
  added 3 collision-poll tests (`net_test.go:471 / :517 / :584`)
  for the tuntap-add EBUSY race, NOT the multi-cycle vm_index
  reuse shape that produced stress-r2's 9 stranded interfaces.
  Different race. R25-T3 carries.
- **NEW [R26-T2] r3-A node-affinity ships without parity test.**
  When r3-A's jobspec-emission half lands, the cross-emitter
  parity discipline (R22-T1 shape — cold-boot and restore
  emitters MUST agree on every jobspec field) needs to cover
  the new `Constraints` block. Today's parity test at
  `nomad_ch.rs::tests::ch_plugin_config_field_list_matches_
  driver_v13` covers `ChPlugin Config` field set ONLY — not the
  top-level `Constraints` array. A r3-A landing that adds
  Constraints to cold-boot but forgets restore (or vice versa)
  would silently re-introduce the cross-node race for half the
  callers. New ask before r3-A merges.
- **NEW [R26-T3] r3-A boot-time fetch lacks unit-test coverage.**
  `fetch_local_nomad_node_id` at `nomad_ch.rs:3080-:3119` parses
  `GET /v1/agent/self` JSON with a 4-branch lookup (`stats` |
  `Stats`, `client` | `Client`, `node_id` | `NodeID`,
  `as_str()` + non-empty guard) and 5 failure shapes (non-200,
  parse error, missing path, empty string, IO error). Zero unit
  tests against a mock 200/404/garbage-JSON/`{}` server. The
  `spawn_404_mock` / `spawn_mock_at` fixtures at `nomad_ch.rs:
  ~6300` exist precisely for this pattern. ~80 LOC ask.
- **NEW [R26-T4] r3-A boot-fallback path has no integration
  test.** When `fetch_local_nomad_node_id` returns Err, lib.rs
  bumps the counter + WARNs + sets `local_nomad_node_id = None`
  + continues boot. No lib test pins "boot succeeds AND state
  field is None AND counter incremented when /v1/agent/self
  returns 404". The fixture path (`new_fixture` at lib.rs:519
  hardcodes `None`) ASSUMES this shape; no test asserts it as
  a contract. A future refactor changing boot to abort on
  lookup failure breaks the field's non-fatal contract with
  zero CI signal.
- **NEW [R26-T5] No verbatim-observable CI discipline test.**
  Architecture r25-A4 ADR documents the 4-for-4 playbook
  ("before patching at layer N+1, capture verbatim observable
  first"). r3-C is the playbook applied: stress-r3 captured
  the Nomad alloc JSON before r3-C's two-pass fix, which became
  the test fixture at `nomad_ch.rs:4528-:4573`. No CI test
  enforces the discipline for FUTURE fixes. A regression where
  a new `nomad_ch.rs` patch ships without a `tests::` companion
  pinning the observable shape lands clean. Suggested:
  `scripts_lint.rs`-style policy test (1 LOC delta in
  `nomad_ch.rs::tests` per `nomad_ch.rs` production-code delta
  ≥ 50 LOC). Hard to encode cleanly; flagging as discipline
  carrier, not concrete ask.
- **NEW [R26-T6] Cluster-side regression test for cross-node
  placement.** 3 stress REDs in a row (-r1, -r2, -r3) share a
  pattern: WORKER_COUNT=1 smoke passes (no other worker to
  schedule on); WORKER_COUNT=3 stress fails (78% on r3 from
  scheduler picking a non-staging node). The minimal cluster
  test that catches this WITHOUT a full 60-cycle stress run
  is a WORKER_COUNT=2 + 6-cycle "placement audit" — assert
  every alloc lands on the node that staged its `workspace.img`.
  ~30 cycles × $0.40/cycle vs $20-30 for full stress. New
  proposal for the cluster lens.
- **Pre-existing-failure carries from brief**: R23-I1, R24-I1,
  R24-T1, R25-I1, R25-I2, R25-S1 all CLOSED per
  `docs/reviews/sandbox-snapshot-restore-deferred.md`. R24-I2
  rolled into the convergent R23-API1 / R25-S1 / R25-I1 / R25-I2
  fixer at `79871194` + `022f778a`. R24-T2 still open
  (re-counted in R25-T2 carry). R25-T2 / R25-T3 / R25-T4 /
  R25-T5 all open (this round's IMPORTANT carries).
- **stress-r3 (RED 1/60) tipped the priority order**: cluster
  signal implicated all three open R25-T-N gaps. r3-A targets
  the cross-node race; r3-B targets the tuntap collision; r3-C
  targets the opaque error envelope. **All three landed code
  changes without proportional test-coverage additions for the
  downstream chain.** R25 closing thesis ("cluster doing work
  unit tests should be doing") holds in r26.

## CRITICAL

None.

## IMPORTANT

### [R26-T1] R25-T5 carries: verbatim-msg propagation has 4 NEW entry tests but 0 exit tests

- **Where**:
  - Entry helper (r3-C bundle): `nomad_ch.rs:2776-:2843`
    (production, two-pass selection). Tests at `:4371, :4411,
    :4432, :4445, :4467, :4528 (NEW r3-C), :4575 (NEW r3-C),
    :4603 (NEW r3-C), :4624 (NEW r3-C)`. Total 9 helper tests.
  - Cold-boot composition: `nomad_ch.rs:2645` (`let driver_msgs
    = extract_failed_task_event_msgs(a)` → composed into
    `wait_for_alloc_running` Err return string).
  - Restore composition: `restore_handler.rs:2577-:2588`.
  - `wait_for_alloc_running` Err: passed to caller, wrapped as
    `RestoreHandlerError::Backend(String)` at do_restore_inner.
  - `sanitize_error_message`: applies the §10.0 redaction.
  - Wake-job sink: `wake_machine.rs` writes `error_message` to
    `wake_jobs`.
  - Wire egress: `GET /admin/sandboxes/{id}/wake/{wake_id}` 200
    body `message` field.
- **What r3-C closed**: the FIRST hop. The reverse-walk picking
  the last (generic) DisplayMessage is fixed; the two-pass
  selection prefers Driver Failure / Task Setup Failure / Failed
  Validating Task / Failed Artifact Download / Exec Plugin /
  Setup Failure types. 4 new tests pin the load-bearing fixture
  (the exact stress-r3 Events[] sequence) + 3 edge shapes
  (multiple-driver-failure first-wins, fallback to last when no
  diagnostic type, allow-list bounds).
- **What r3-C did NOT close**: all 5 downstream hops. A
  regression at ANY of:
  - `wait_for_alloc_running` Err formatting (loses the verbatim
    text in a `format!()` truncation),
  - `RestoreHandlerError::Backend(String)::to_string()` round-trip,
  - `sanitize_error_message` over-redaction (the §10.0 redactor
    matches `disk[N] /…/<file>.img` patterns; a regex tweak that
    drops the path AND the filename loses both error context),
  - `wake_machine.rs` truncation of `error_message` before write,
  - admin handler masking the `message` field for `AdminRole::
    ReadOnly`,

  would silently re-introduce the opaque envelope shape that
  v34's d638b10f → r3-C chain exists to remove. 9 helper tests
  pin the entry; ZERO tests pin any later hop.
- **Why this matters NOW**: stress-r3 RED outcome (1/60 e2e OK
  per `docs/reviews/sandbox-snapshot-restore-T-8b-stress-r3.md`)
  proved the entry-side fix was load-bearing — without it, the
  47/47 CREATE failures arrived as "Unhealthy because of failed
  task" and ops had no actionable trail. The downstream chain
  is asymmetrically tested: 9 tests at the entry, 0 at the
  exit. The next regression at any of the 5 downstream hops
  lands silently.
- **What WOULD close it** (representative — pick 2):
  - **pg-gated** at `sandbox_pg_e2e.rs`: drive a WakeMachine
    `restore_failed` terminal where the error string composed
    by the producer side contains `disk[1] /var/zeroship/ch/
    <sid>/workspace.img does not exist`. Assert
    `wake_jobs.error_message` post-write contains both the path
    fragment and the controller-staging hint. ~50 LOC fixture
    on top of the existing R23-I1 terminal tests at
    `sandbox_pg_e2e.rs:5530, :5617`.
  - **admin-handler** at `sandbox_admin_e2e.rs`: hit `GET
    /admin/sandboxes/{id}/wake/{wake_id}` against a row with
    the verbatim text pre-seeded; assert the 200 body `message`
    contains the text under `AdminRole::ReadWrite` and (per
    security-r25 R25-S1 typed error path) carries the
    `staging_image_missing` code under `AdminRole::ReadOnly`
    instead of the host path. ~50 LOC.
  - **lib-level** at `wake_machine.rs::tests`: stub
    `RestoreBackend` returning the verbatim text from
    `submit_restore_job`; assert the resulting wake-row
    `error_message` carries it through the
    `Backend(e).to_string()` → `sanitize_error_message`
    composition. ~30 LOC. Lower-coverage but cheapest.
- **Severity IMPORTANT**. 5th-round carry of the verbatim-text
  end-to-end ask (R23-I1 chain + R24-T2 + R25-T5 + R26-T1). 4
  entry-side tests landed this round, 0 exit-side. The
  trapezoid-of-coverage shape persists.

### [R26-T2] [NEW] r3-A node-affinity will land without cross-emitter parity test

- **Where** (in-flight, not yet committed):
  - `lib.rs:201-:222` (new `AppState.local_nomad_node_id`).
  - `lib.rs:644-:698` (new boot-time `fetch_local_nomad_node_id`
    + counter bump + WARN log path).
  - `nomad_ch.rs:3078-:3120` (new `fetch_local_nomad_node_id`
    helper).
  - `metrics.rs:150-:166` (new `NOMAD_NODE_ID_LOOKUP_FAILURES`
    counter).
  - **NOT YET PRESENT**: `Constraints` block emission in
    `build_nomad_job_json` (cold-boot) and the restore-side
    builder. The state-field is wired; the emission half is
    still in flight.
- **Existing parity discipline** (R22-T1 reference):
  - `nomad_ch.rs::tests::ch_plugin_config_field_list_matches_
    driver_v13` (~r24 add) pins the cold-boot `ChPlugin Config`
    field list. Driver-side `tests/task_config_test.go`
    enforces the receiver side.
  - This test covers the **inner config**, not the **top-level
    jobspec**. The Constraints block lives at `Job[0].
    TaskGroups[0].Constraints` (or `Job[0].Constraints` /
    `Affinities`). No test pins this.
- **What r3-A's jobspec emission will introduce**: 2 new emission
  sites:
  1. Cold-boot `build_nomad_job_json` reads
     `state.local_nomad_node_id` → emits Constraints if `Some`.
  2. Restore-side `restore_handler::build_*_jobspec` (analogous
     site) reads `state.local_nomad_node_id` → emits Constraints
     if `Some`.
- **Asymmetric-emission gap**: a r3-A patch that updates
  cold-boot but forgets restore (or vice versa) leaves 50% of
  the workload exposed to cross-node placement. Stress-r3 saw
  cold-boot cross-node failure; a restore-side cross-node
  failure has the same symptom (workspace.img ENOENT, but on
  the `unfreeze` side after a snapshot-cycle reuses the local
  staging path expectation).
- **What WOULD close it BEFORE r3-A merges** (gate-before-land
  ask):
  - `tests::cold_boot_and_restore_jobspecs_emit_node_constraint_
    when_local_nomad_node_id_is_some` — drive both builders with
    a synthetic `state` carrying `local_nomad_node_id = Some(
    "fake-node-uuid")`. Assert both emitted JSON bodies have the
    SAME `Constraints` array shape (operator on `${node.unique.
    id}` equal to `fake-node-uuid` — Nomad's standard node-id
    constraint form per the docs). Pin the LTarget / RTarget /
    Operand exactly.
  - `tests::cold_boot_and_restore_jobspecs_omit_constraints_
    when_local_nomad_node_id_is_none` — same fixture with
    `None`; assert NO `Constraints` field in either body
    (fallback to pre-r3-A behaviour). Critical for the
    boot-failure non-fatal contract (R26-T4).
  - **Land WITH r3-A's emission commit, not after.** R22-T1's
    discipline is "parity test ships with the field"; r3-A
    needs the same.
- **Severity IMPORTANT**. Pre-emptive gate-test for a fix
  that's about to land. The test cost is 80 LOC; the no-test
  cost is a silent 50%-coverage regression that re-opens the
  exact stress-r3 race for restore.

### [R26-T3] [NEW] `fetch_local_nomad_node_id` JSON parsing has zero unit tests

- **Where**: `nomad_ch.rs:3080-:3119` (in-flight, +40 LOC). The
  helper:
  - Issues `http_get_unsigned(GET /v1/agent/self)` with 5 s
    timeout.
  - Validates `status == 200`.
  - Parses body via `serde_json::from_str`.
  - 4-branch case-insensitive lookup: `stats|Stats →
    client|Client → node_id|NodeID`.
  - `as_str()` + non-empty guard.
- **Failure shapes, all un-asserted**:
  1. **HTTP unreachable** (DNS / connect refused / network).
  2. **Non-200 response** (e.g., 404 against single-server
     agent, 503 during agent boot).
  3. **Parse error** (HTML 502 page from a proxy, truncated
     body).
  4. **Missing path** (`{"stats": {}}` → no `client` key).
  5. **Empty string** (`{"stats": {"client": {"node_id": ""}}`).
  6. **Case-sensitivity drift** — the defensive `Stats|stats`
     branch handles agent-version drift; a regression that
     hard-codes lower-case-only silently breaks against an
     older Nomad without test signal.
- **What WOULD close it**: 5-6 lib tests using the existing
  `spawn_mock_at` / `spawn_404_mock` fixtures at `nomad_ch.rs:
  ~6300` (the same pattern that powers `wait_for_agent_livez_*`
  test bundle):
  - `fetch_local_nomad_node_id_returns_id_on_lowercase_shape`
  - `fetch_local_nomad_node_id_returns_id_on_uppercase_shape`
    (pins the case-insensitive defensive branch)
  - `fetch_local_nomad_node_id_errors_on_404`
  - `fetch_local_nomad_node_id_errors_on_missing_field`
  - `fetch_local_nomad_node_id_errors_on_empty_string`
  - `fetch_local_nomad_node_id_errors_on_invalid_json`

  ~80-100 LOC under `nomad_ch.rs::tests`. Pattern exists at
  `wait_for_agent_livez_socket_accepts_but_livez_500` (line
  ~6380 area) — same mock-server harness, different
  endpoint.
- **Severity IMPORTANT**. The helper is the data source for the
  jobspec emission contract (R26-T2) — a parse bug here ships
  None silently, the jobspec omits Constraints, and cross-node
  placement returns. This is the controller-side equivalent of
  the wrapper-script preflight: invisible failure mode without
  test signal.

### [R26-T4] [NEW] r3-A boot-failure fallback has no integration test

- **Where**:
  - Production (in-flight): `lib.rs:644-:698`. Pattern:
    ```rust
    let local_nomad_node_id: Option<String> =
        match crate::backend::nomad_ch::fetch_local_nomad_node_id(...)
        {
            Ok(id) => Some(id),
            Err(e) => {
                crate::metrics::inc_nomad_node_id_lookup_failure();
                tracing::warn!(...);
                None
            }
        };
    ```
  - Fixture (in-flight): `lib.rs:512-:519` (`new_fixture` hard-
    codes `local_nomad_node_id: None`).
- **The non-fatal contract**: a transient Nomad-agent failure at
  controller boot MUST NOT block the controller from coming up.
  Field is `None`, jobspec omits Constraints, cluster falls
  back to pre-r3-A random placement. Counter records the
  degraded shape for operator alerting.
- **What's missing**: a lib test in `tests/` (or, if scope
  allows, in `lib.rs::tests`) that drives `AppState::new` /
  `AppState::boot_inner` against a mock Nomad agent returning
  404 on `/v1/agent/self` and asserts:
  1. `AppState::new` returns `Ok` (boot completes).
  2. The resulting `state.local_nomad_node_id == None`.
  3. The counter `nomad_node_id_lookup_failures_value() == 1`
     (test-only accessor at `metrics.rs:333`).
- **Why this matters**: the comment block at lib.rs:225-:244 is
  load-bearing prose ("Boot does NOT block on this — the
  controller boots without the constraint and falls back to the
  pre-r3-A random-placement behaviour"). Prose is not a test.
  A future refactor changing boot to `?`-propagate the
  fetch failure violates the contract; nothing trips.
- **Related**: the partner contract is "when `Ok(id)`, field
  is Some(id)". The R26-T2 parity test covers the consumer
  side; this covers the producer side. Both contracts needed.
- **Severity IMPORTANT**. Reproducibility-of-boot is the entire
  point of the fallback. ~30-40 LOC lib test, integrates with
  the same mock-server fixture R26-T3 needs.

### [R26-T6] [NEW] Minimal cluster regression test for cross-node-placement

- **Context**: three stress-r1/r2/r3 REDs share a regression
  shape that WORKER_COUNT=1 smoke tests cannot catch. The
  test-cov lens has been pointing at this since R24 ("the
  cluster catches positive failures but only AFTER landing in
  production shape; a unit test catches the same bug before
  any cluster cycle costs $20-30").
- **What stress-r3 surfaced specifically**: at WORKER_COUNT=3,
  78% of CREATE allocs landed on a non-staging node and
  ENOENT'd. The bug is invisible at WORKER_COUNT=1 (no other
  node to schedule on) and at full WORKER_COUNT=3 stress
  (high-noise; failure mode buried in 47/60 RED).
- **What WOULD close it (cluster-lens proposal, deferred to
  cluster lens)**: a `tests/cluster_placement_audit.sh` script
  (or T-8b-placement-audit harness):
  - WORKER_COUNT=2 (smallest non-degenerate).
  - 6 cycles of CREATE on each worker.
  - Assert: every alloc's `Allocation.NodeID` matches the
    controller's `local_nomad_node_id` at the time of submit
    (recoverable from controller logs or via the Nomad API
    `GET /v1/allocation/{alloc_id}` after).
  - 6 cycles × 2 workers × ~30s cold-boot per = ~6 min wall;
    ~$2-3 vs $20-30 for full stress.
- **Why this is a test-cov ask not a cluster-lens-only ask**:
  the audit can be encoded as a TEST. Every PR that touches
  `build_nomad_job_json` or `submit_nomad_job` triggers a
  CI-time placement audit. Today the audit happens at full
  stress only (after the change has landed). Move it left.
- **Severity IMPORTANT**. Cross-lens (cluster + test-cov). The
  audit doesn't replace stress — it catches the SPECIFIC
  cross-node-placement regression class WORKER_COUNT=2 can
  expose for 1/10th the cost.

## MINOR

### [R26-T5] [NEW] No CI discipline test for 4-for-4 verbatim-observable ADR

- **Where**: `docs/decisions/2026-05-25-restore-debug-playbook.md`
  (ADR codifying r24-A3 / r25-A4) documents the
  "before patching at layer N+1, capture verbatim observable
  first" rule. r3-C is the most recent application — stress-r3's
  alloc JSON became the test fixture at `nomad_ch.rs:4528-:4573`.
- **What's missing**: a structural CI guard that prevents the
  pattern "production-code patch in `nomad_ch.rs` lands without
  a paired test fixture pinning the observable shape".
- **Why this is MINOR not IMPORTANT**: the rule is
  process-discipline not code-discipline. A CI test enforcing
  it would have to be heuristic (LOC threshold, file-pair
  detection) and would false-positive on doc-only / refactor
  / unsafe-block-narrowing commits.
- **Best-effort form, if pursued**: a `scripts_lint.rs`-style
  check rejecting commits where `crates/sandbox/src/backend/
  nomad_ch.rs` production-code delta ≥ 50 LOC AND
  `nomad_ch.rs::tests` test-code delta == 0. Carve-outs via
  `[skip-tests-required]` commit-message tag. ~40 LOC linter.
- **Severity MINOR**. The discipline is real; the test
  enforcement is harder to encode cleanly than the discipline
  is to follow manually. Flagging for r27+ if a pattern of
  bypasses emerges; currently 3-for-3 on the v34+r3-C bundle
  (4 entry tests, 0 exit — see R26-T1 for the qualifier).

### [R26-T7] r3-C `extract_failed_task_event_msgs_picks_first_driver_failure_when_multiple` doesn't test the case-insensitive Type match

- **Where**: `nomad_ch.rs:4603-:4622` (`picks_first_driver_
  failure_when_multiple`).
- **What it tests**: two `"Type": "Driver Failure"` events in
  sequence; first wins.
- **What it doesn't test**: `is_diagnostic_event_type` at
  `nomad_ch.rs:~2820` likely does an exact-match (`event_type
  == "Driver Failure"`). A Nomad version emitting `"driver
  failure"` (lower-case) or `"Driver Failure "` (trailing
  space) would fail the match silently and fall through to the
  reverse-walk again, masking the fix.
- **What WOULD close it**: 1 new test with mixed-case /
  whitespace-padded Type strings; pin the spec contract (exact
  match vs lenient). Likely 1-line code change to
  `is_diagnostic_event_type` (call `.trim().eq_ignore_ascii_
  case(...)`) + 1 regression test. ~10 LOC.
- **Severity MINOR**: hypothetical; no observed Nomad version
  emits non-canonical Type strings. Low-priority defensive add.

## Trend table — carry from r25

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
| **r26** | **459**   | **91**   | **+5** | **0** | r3-C diagnostic-event-preference (4) + is_diagnostic allow-list (1); pg unchanged |
| r9→r26 | +163       | +17      | —     | —    | wedge + sweep + sanitizer + probe + retry + drift + envelope + parity + counter-e2e + RO-admin + driver-msg helper + diagnostic-event preference |

Of the +5 lib delta from r25→r26: **all 5 are at the
verbatim-msg helper entry**. Zero pin the chain at any later
hop. The trapezoid-of-coverage shape from r25 widens — the
entry has 9 tests now, the exit still has 0.

Of the in-flight working-tree delta (r3-A jobspec emission half
not yet committed): **+0 lib tests for `fetch_local_nomad_
node_id`** (R26-T3 ask), **+0 tests for boot-failure fallback**
(R26-T4 ask), **+0 cross-emitter parity test** (R26-T2 ask).
r3-A is shipping 3 ask-shaped test gaps.

## Carry-forward table (r25 → r26)

| Tag | r25 status | r26 status | Note |
|-----|------------|------------|------|
| R25-T1 stress harness polling invariant | IMPORTANT, OPEN | **STILL OPEN** | No `tests/stress_harness_invariant.rs` landed. SHA-pin still tracks GCS, not in-repo behaviour. |
| R25-T2 cold-boot workspace.img preflight | IMPORTANT, OPEN | **STILL OPEN (4th cycle)** | r3-A touched `lib.rs` boot path; did NOT add cold-boot create() mirror. Restore-side mirror at `restore_handler.rs:3295` standing alone for 4th round. |
| R25-T3 driver multi-cycle race | IMPORTANT, OPEN | **STILL OPEN** | r3-B added 3 tap-collision-poll tests (different race). Multi-cycle vm_index reuse unchanged. |
| R25-T4 sweeper unit tests | IMPORTANT, OPEN | **STILL OPEN (NOT IN FLIGHT)** | Brief said parallel fixer would address; verified sweep.rs unmodified. 6 destructive-on-misfire gates, 0 coverage. |
| R25-T5 verbatim-msg exit tests | IMPORTANT, OPEN | **PARTIAL CARRY → R26-T1** | r3-C added 4 ENTRY tests (load-bearing fixture pin). 0 EXIT tests still. |
| R25-T6 UTF-8 truncation safety | MINOR, OPEN | **OPEN, carry** | 1-line fix + 1 test still un-landed. |
| R25-T7 trend delta convention | MINOR, INFO | n/a | Process note; not actionable. |
| R25-T8 working-tree compile error | INFO | **RESOLVED** | R23-API1 fixer committed at `79871194` + `022f778a`. New working-tree state is r3-A in-flight (R26-T2/T3/T4 ask). |
| R22-T3 retry-race pg test | IMPORTANT, OPEN (5th) | **OPEN, 6th** | Concurrency lens silent again this round. |
| R21-T1 r17-Q3 DataIntegrity integration | MINOR, OPEN (6th) | **OPEN, 7th** | Carry. |
| R18-T7 probe_and_classify loops | OPEN, carry | OPEN, carry | Still 0 hits. |
| R18-T9 OS-thread detach pattern | OPEN, carry | OPEN, carry | 4 sites uncovered. |
| R19-T2 takeover_once lib coverage | OPEN, carry | OPEN, carry | R19-C1 counter wired; loop itself untested. |
| R19-T3 R19-C1 claim-count metric | OPEN, carry | OPEN, carry | Co-file with R22-T3. |

## Bundle-specific would-have-caught analysis

### Does r3-C close R25-T5?

**Entry side YES, exit side NO** — see R26-T1. 4 new tests pin
the `extract_failed_task_event_msgs` two-pass selection logic.
They cover:
- The exact stress-r3 alloc Events[] fixture (Received → Task
  Setup → Driver Failure → Restart Signaled → Alloc Unhealthy)
  — pinning the load-bearing observable.
- The fallback path (no diagnostic Type → reverse-walk last
  event).
- Multiple-Driver-Failure picks first (temporal-order tie-break).
- The diagnostic Type allow-list bounds.

They do NOT cover any of `wait_for_alloc_running` Err,
`RestoreHandlerError::Backend`, `sanitize_error_message`,
`wake_jobs.error_message` write, or `GET /wake/{id}` 200
`message` field. The chain from helper → wire is asymmetrically
tested.

### Does r3-A (in-flight) close R25-T2 or R24-T2?

**No.** r3-A's in-flight changes:
- Add `fetch_local_nomad_node_id` helper.
- Add boot-time fetch + counter + state field.
- WILL add (not yet) Constraints emission in jobspec builders.

None of these touch the `workspace.img` cold-boot staging
precondition. R24-T2's ask ("`create_must_stage_workspace_img_
before_submit_nomad` test mirroring restore-side") is unchanged
and unaddressed in this cycle. **4th-round carry.**

### Does r3-B close R25-T3?

**No.** r3-B's 3 new Go tests at `net_test.go:471 / :517 / :584`
cover the tuntap-add EBUSY collision-poll race (kernel netdev
release async). They do NOT cover the multi-cycle vm_index reuse
race that produced stress-r2's 9 stranded interfaces. Different
hop — r3-B is the SECOND tap-add attempt after `link delete`
returns; the multi-cycle race is the gap between worker A's
`DestroyTask` and worker B's `StartTask` (or same worker
different cycle) reusing the same `vm_index`.

R25-T3 unchanged.

### Sweeper test coverage (R25-T4)

**Zero.** Verified:
- `git diff HEAD -- crates/sandbox/src/sweep.rs` → empty.
- `grep -nE "fn |#\[test\]" sweep.rs | tail -10` →
  pre-existing 3 tests only (`recovery_target_pins_proposal_
  table`, `wake_jobs_takeover_cadence_is_60s`, `wake_lifecycle_
  takeover_threshold_floor_and_default_pinned`).
- No tests reference `run_host_dir_gc_once` or `spawn_host_dir_
  gc`.

The brief said "about to be addressed by a parallel fixer this
cycle". The fixer did not land into the working tree by this
audit's HEAD pin (`3d431eb8`). If it lands later in the cycle,
r27 will re-audit; for r26 the gap stands.

### Pre-existing-failure closure status (per brief)

| Tag | r26 status | Source |
|-----|------------|--------|
| R23-I1 | **CLOSED** | `docs/reviews/sandbox-snapshot-restore-deferred.md:1842` (Path B pg-gated e2e tests) |
| R24-I1 | **CLOSED** | `:1846` (terminal-overwrite WARN error_code threading) |
| R24-I2 | **CLOSED (rolled in)** | Folded into R23-API1 / R25-S1 / R25-I1 / R25-I2 convergent fixer at `79871194` + `022f778a` (typed `WakeErrorCode::StagingPathMissing`). Not explicitly tagged but the path-helper extraction + typed-id form closure covers the cited shape. |
| R25-I1 | **CLOSED** | Same convergent fixer above. |
| R25-I2 | **CLOSED** | Same convergent fixer above. |

All five pre-existing failures CLOSED as of r26. The brief's
ask to confirm closure status: **confirmed for all 5**, with
R24-I2 having been silently rolled into the typed-error
convergent commit rather than tagged individually.

### Working-tree compile cleanliness

At HEAD `3d431eb8` (committed): `cargo test -p zeroship-
sandbox --lib` = **459 PASS / 0 fail / 1 ignored** (verified
this audit). Full `--tests` build clean; 5 failing tests in
`sandbox_preview_share_e2e.rs` (`delete_per_token_returns_
501_deferred_to_phase_5`, `missing_sec_fetch_site_returns_400_
client_too_old`, `scope_forbidden_via_cookie_conversion_returns_
403`, `ro_token_post_via_dispatch_returns_404_uniform`,
`expired_token_returns_401_with_expired_code`) are in the
preview-share area (unrelated to snapshot-restore), all
panicking at fixture line `526 / 798 / 648 / 615 / 578` —
likely a pre-existing flake or unrelated fixture drift,
flagged for the preview-lens. **Not r3-A / r3-B / r3-C
regression.**

## What stress-r4 (next cycle, after r3-A lands) will exercise that unit tests miss

Enumerated against the four contracts crossing the cluster
once r3-A's emission half merges:

1. **Cross-node placement constraint emission** (r3-A). The
   cluster IS the only oracle today. R26-T2 gate-test will fix
   that BEFORE the next stress cycle if it lands with r3-A.
   Without R26-T2: asymmetric-emission regression (cold-boot
   only, or restore only) cannot be detected pre-cluster.

2. **Boot-failure fallback at controller restart** (r3-A
   non-fatal contract). Stress-r4 will run against a stable
   Nomad agent — won't exercise the failure path. The only
   cluster signal for this contract is a chaos test
   (deliberately kill Nomad agent during controller restart) —
   which isn't in the stress harness. R26-T4 lib test is the
   only path.

3. **Verbatim-msg propagation, downstream hops** (R26-T1). If
   stress-r4 reproduces any cold-boot CREATE failures, the
   `wake_jobs.error_message` rows will be the cluster's
   verdict on whether the v34 + r3-C chain works end-to-end.
   But the failure-cause readback IS the test — if r4 is
   GREEN, the chain is validated stochastically only; if RED,
   the diagnosis is "where did the verbatim text get lost?"
   without a unit-test anchor for re-examination.

4. **Sweeper non-racing live wakes** (R25-T4). Unchanged from
   r25. 60-cycle stress with 5-min sweep cadence vs ~46s WAKE
   wall gives ~9% per-tick collision over the run.
   `find_pending_wake_for_sandbox` gate MUST suppress.
   No unit test.

### Gaps cluster signal still has to close (independent of
stress-r4 verdict)

- **r3-A jobspec-emission half landing without R26-T2/T3/T4
  tests would be a 4-for-4 ADR breach** — the playbook says
  "before patching at layer N+1, capture verbatim observable
  first". r3-A captures stress-r3's `Allocation.NodeID`
  cross-node observable as motivation but doesn't translate
  it into a fixture for the emission contract. R26-T2 closes
  this.
- **Deterministic pin on the r3-A mental model.** If stress-r4
  is GREEN at WORKER_COUNT=3, the cross-node race is fixed
  stochastically; if RED, the model has no unit anchor (R26-T3
  / R26-T4) for re-examination.

## Lib tests gating cutover

- **459 lib + 91 pg-gated + ~75 other integration**
  (`sandbox_admin_e2e.rs:26 ntex` + `sandbox_persist_e2e.rs:0`
  + `sandbox_preview_e2e.rs:6` + `sandbox_preview_share_e2e.rs:
  22 (5 currently failing — unrelated)` + `sandbox_preview_ws_
  e2e.rs:7 compio` + `sandbox_typed_id_e2e.rs:12` +
  `scripts_lint.rs:2`). Plus 133 driver-side (+3 from r3-B
  net_test.go poll additions).
- **stress-r4 gate sufficiency** (post-r3-A landing): existing
  459 lib + 91 pg exercise the bundle's pure-helper additions.
  **Six high-leverage seams remain un-pinned**: cold-boot
  create preflight (R25-T2, 4th cycle), sweeper eligibility
  matrix (R25-T4), end-to-end driver-msg propagation
  (R26-T1), multi-cycle vm_index race (R25-T3), r3-A
  jobspec-emission parity (R26-T2 — pre-emptive), r3-A
  boot-failure fallback (R26-T4).
- **T-8b-cutover gate sufficiency**: INSUFFICIENT until at
  least R26-T2 (parity, gate-test for r3-A) AND R25-T4
  (sweeper destructive-on-misfire gates) have unit-level
  coverage. The cluster IS the test today for both.

## Cross-lens consensus

- **R26-T1** (R25-T5 carry): test-cov + security. Path-leak
  redaction at `sanitize_error_message` shares the surface
  with `wake_jobs.error_message` write — security-r25 R25-S1
  closure was the typed-error path; the test ask remains.
- **R26-T2 / R26-T3 / R26-T4** (r3-A): test-cov +
  api-surface. R22-T1 parity discipline is the api-surface
  carrier; test-cov enforces it. Cross-emitter parity is the
  exact shape R22-T1 closed for ChPlugin Config — now needs
  re-application at the top-level jobspec for Constraints.
- **R26-T6** (cross-node placement audit): test-cov +
  cluster lens. The audit is the minimal cluster test that
  catches the bug pre-stress; lives in cluster-lens
  territory but test-cov is the discipline carrier.
- **R26-T5** (verbatim-observable ADR CI): test-cov +
  architecture. r25-A4 ADR codifies the discipline; r26
  flags that no CI test enforces it. Likely won't land —
  hard to encode cleanly.
- **R25-T4** (sweeper): test-cov-only. Destructive-on-misfire
  semantics make this a test-cov-priority pattern; brief
  hinted at parallel fixer this cycle (not landed at audit
  pin).

## Lens hand-off

- **api-surface r26+**: R26-T2 is a sibling of R22-T1 and
  should ship WITH r3-A's emission commit, not after. The
  cross-emitter parity discipline is api-surface's domain;
  test-cov is the carrier.
- **architecture r26+**: R26-T5 (4-for-4 ADR CI guard) is an
  architecture-lens ask but lives uncomfortably between
  discipline and code. Likely deferred.
- **code-quality r26**: R25-T6 (UTF-8 boundary safety) is a
  1-line fix + 1 test still un-landed. Carries.
- **concurrency r26**: R22-T3 (R20-T1 retry-race pg test) now
  6 rounds open. r24's escalation rule expired without
  resolution. R25-T3 is concurrency-adjacent (multi-cycle
  vm_index race).
- **security r26**: R26-T1's chain shares the propagation
  surface with R25-S1 (path-leak via error string) — typed-
  error path closed the security half; test-cov half remains.
- **cluster r26+**: R26-T6 (placement audit) needs cluster-
  lens design. Out of scope for test-cov to specify the
  harness; in scope to flag as a regression class the cluster
  catches asymmetrically.

## To test-cov r27 backlog (~550 LOC total)

1. **R26-T2** r3-A cross-emitter parity test (cold-boot +
   restore jobspecs emit identical Constraints shape when
   `local_nomad_node_id` is Some; both omit when None).
   ~80 LOC. **GATE-BEFORE-R3-A-LANDS**.
2. **R26-T3** `fetch_local_nomad_node_id` JSON parsing tests
   (5-6 mock-server cases). ~80-100 LOC. **WITH-R3-A-LANDS**.
3. **R26-T4** r3-A boot-failure non-fatal fallback lib test
   (404 mock → boot completes + state field None + counter
   bumps). ~30-40 LOC. **WITH-R3-A-LANDS**.
4. **R25-T4** (5th-round carry) sweeper eligibility-gate
   matrix tests — ~120 LOC pg-gated + ~50 LOC lib (non-DB
   gates). IMPORTANT.
5. **R26-T1** (R25-T5 6th-round carry) verbatim-msg exit
   tests (pg-gated `wake_jobs.error_message` + admin-handler
   `GET /wake/{id}` 200 `message`). ~100 LOC. IMPORTANT.
6. **R25-T2** (4th-round carry) cold-boot `create_must_stage_
   workspace_img_before_submit_nomad` mirror. ~80 LOC.
   IMPORTANT.
7. **R25-T1** (2nd-round carry) `snapshot_stress.py`
   polling-loop invariant test. ~120 LOC. IMPORTANT.
8. **R25-T3** (2nd-round carry, cross-worktree) multi-cycle
   vm_index race test (Go, `tests/start_task_test.go`). ~80
   LOC. IMPORTANT.
9. **R26-T6** placement-audit cluster test (WORKER_COUNT=2,
   6 cycles). Cluster-lens design. ~200 LOC harness.
   IMPORTANT.
10. **R22-T3** (6th-round carry) R20-T1 retry-race pg tests.
    ~80 LOC.
11. **R25-T6** (2nd-round carry) UTF-8 cap safety. ~20 LOC.
    MINOR.
12. **R26-T7** diagnostic Type case-insensitivity test +
    1-line lenient match. ~10 LOC. MINOR.

Delisted this round: none.

## Notes for r27

- **The shift this round**: r3-C closed the entry-side of
  R25-T5 with 4 load-bearing fixture tests. Net test-cov win;
  partial closure. r3-A is in-flight with 3 ask-shaped gaps
  (R26-T2/T3/T4) that should land WITH the emission commit
  per R22-T1 discipline. r3-B added 3 driver tests for a
  different race than R25-T3.
- **r3-A landing-gate recommendation**: do NOT merge r3-A's
  jobspec-emission half without R26-T2 (cross-emitter parity)
  AT MINIMUM. R26-T3 + R26-T4 are with-fix asks. Without
  R26-T2, an asymmetric-emission regression is a 50%-coverage
  re-introduction of the stress-r3 cross-node race.
- **R25-T4 carries unchanged after a stated parallel
  fixer-this-cycle expectation**. Verified working-tree
  state: sweep.rs untouched at audit pin. If the parallel
  fixer lands later in the cycle, r27 re-audits. Until then
  the 6-gate / 3-destructive-on-misfire / 0-test gap stands
  for the 2nd consecutive round.
- **Test-coverage trapezoid widening**: 9 tests at
  `extract_failed_task_event_msgs` entry, 0 at any exit.
  Each round adds entry-side tests to fix the LATEST
  cluster-observed regression; the chain to wire egress
  stays un-pinned. r27 highest-leverage closure: R26-T1
  pg-gated exit test (~50 LOC) → 1 test → flips the chain
  from 9/0 to 9/1 trapezoid.
- **Pattern carry-forward from r25**: "the marginal gaps
  moved BACK INTO the crate" thesis still holds. r3-A is
  shipping crate-side test debt; r3-C closed some
  crate-side test debt; r3-B is cross-worktree (driver). Net
  crate-side test debt direction this round: roughly flat
  on commits-landed (r3-C delta), debt-increasing on
  in-flight (r3-A delta).
- **Highest-leverage closure for r27**: R26-T2 (r3-A
  parity gate-test) — ~80 LOC, prevents the asymmetric-
  emission re-introduction of the stress-r3 race. Must
  land WITH r3-A's jobspec-emission commit, not after.
  R26-T1 pg-gated exit test (~50 LOC) is second — closes
  the trapezoid by one rung.
- **No emoji, no celebratory framing**: the bundle this
  round added entry-side test depth, in-flight is adding
  crate-side production code without proportional test
  additions, and the 6-gate destructive-on-misfire sweeper
  gap is still uncovered. Net direction: needs r3-A
  test-bundle ahead of merge to avoid the playbook breach.
