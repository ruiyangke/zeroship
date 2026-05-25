# Sandbox/snapshot-restore — test-coverage r32 review

Date: 2026-05-25 (UTC). HEAD at audit: `8d82ecde` (r32-T1 trace points
+ cadence cherry-pick + raw_exec.enable removal + parse-error sleep
tighten; `sandbox-snapshot-restore` worktree; clean tree). Round 32.
Prior: `docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r31.md`
(HEAD `3a53b7ba`, 549 lib tests).

**Lib test count at HEAD: 551 passed; 0 failed; 1 ignored**
(`cargo test --lib -p zeroship-sandbox` verified locally, 4.12 s).
Was 549 at r31 close — net **+2**. Both increments come from earlier
cycles already-on-master at r31 base, surfaced into this worktree
via the r32 cherry-picks (`a6e517b2` wake-cadence batch carries a
test, plus one preexisting catchup); the r32-T1 trace commit
`1d58ab53` itself adds zero tests, as expected for an
observability-only change. Pg-gated count unchanged at 94.

## TL;DR

This round answers the five brief questions directly.

- **Q1 (new `tracing::info!` emits)**: NOT covered by unit tests and
  SHOULD NOT be. Tracing emits at `INFO` level on the hot CREATE
  path are observability surfaces; pinning them in unit tests would
  freeze log strings as wire-format. The two emits are diagnostic
  scaffolding for cluster-cycle attribution (`journalctl … | jq`),
  not a contract. Recommendation: **leave uncovered**; if a future
  reviewer wants a smoke, use `tracing_subscriber::test::*` filter
  rather than assertion on string content. See **[R32-G1]** below.

- **Q2 (`alloc_first_seen_logged` correctness)**: **CORRECT — stays
  set across retries.** The boolean is declared at function scope
  (`let mut alloc_first_seen_logged = false;` line 3058, OUTSIDE the
  `while Instant::now() < deadline` loop at line 3059). It is only
  ever written to `true` (line 3095); never reset. Across thousands
  of poll iterations, the emit fires exactly once per call to
  `wait_for_alloc_running`. **No bug.** See **[R32-V1]** below for
  the verification trace.

- **Q3 (lib test count snapshot)**: **551 confirmed.** Diff from r31's
  549 is net +2, all from prior commits surfaced through the r32
  cherry-pick chain — not from the trace points themselves.

- **Q4 (R31-T-derive_url carry — 3 hardcoded 7777 sites)**:
  **PARTIALLY closed.** Only 2 sites remain in production code:
  `restore_handler.rs:2392` (RealRestoreBackend::derive_agent_url)
  and `restore_handler.rs:2471` (register_restored). The third
  (nomad_ch.rs:derive_agent_url) now uses `AGENT_PORT` (line 2043).
  See **[R32-T1]** below — same MINOR severity but smaller surface.

- **Q5 (pg-gated coverage for wake_jobs takeover sweep)**: SQL layer
  fully covered (5 pg-gated cases at `tests/sandbox_pg_e2e.rs:4740,
  4792, 4855, 4912, 4916`). **Orchestrator layer
  (`run_wake_jobs_takeover_once` + `spawn_wake_jobs_takeover`)
  has NO integration test** — only the cadence-pin
  `wake_jobs_takeover_cadence_is_60s` (sweep.rs:1554) and
  threshold-pin (`:1565`). See **[R32-T2]** below.

**TOTAL NEW r32 items: 2 (R32-G1 informational, R32-V1 verification).
NEW MINOR: R32-T2. R31-T-derive_url narrowed to 2 sites but still
MINOR.** All other r31 carries unchanged.

---

## CRITICAL

None.

---

## IMPORTANT

No new IMPORTANT items in r32. All r31 IMPORTANT carries unchanged
(R30-T1 admin call-chain integration; R29-T2 T5 drive() integration;
R29-T3 staging-skip contract; R28-T2 verbatim-msg exit 10th carry;
R27-T3 boot-failure composition 6th carry; R31-A1 mutex-linearization
stress — see r31 §Carries unchanged).

---

## MINOR

### [R32-T2] [NEW] `run_wake_jobs_takeover_once` lacks an
orchestrator-layer integration test (pg-gated)

**Where**: `crates/sandbox/src/sweep.rs:388-425`.

The pg-gated suite covers the SQL primitive
(`claim_orphan_wake_for_recovery`) thoroughly — 5 cases at
`tests/sandbox_pg_e2e.rs:4740/4792/4855/4912/4916` covering
(a) within-threshold rows untouched, (b) over-threshold rows claimed
to `failed/wake_worker_aborted`, (c) parallel-claim mutual exclusion,
(d) repeated-claim idempotence, (e) the row-state precondition. The
sweep wrapper `run_wake_jobs_takeover_once` is a 38-line orchestrator
around this primitive that adds: (i) `database.is_none()` short-
circuit, (ii) threshold lookup from `state.wake_lifecycle.takeover_
threshold_secs`, (iii) `Ok(n)/Err` log dispatch.

**Current coverage**: only the cadence-constant pin
(`wake_jobs_takeover_cadence_is_60s`) and the
threshold-floor pin (`wake_lifecycle_takeover_threshold_floor_and_
default_pinned`). Neither exercises the wrapper.

**What a regression would look like**: a refactor that flips the
threshold lookup from `state.wake_lifecycle.takeover_threshold_secs`
to a raw env-var read (sidestepping config validation), or one
that swallows the `Err(e)` branch silently instead of warn-logging,
would pass the SQL tests and pass the cadence-pin tests, but break
operator-visibility — the precise R19-C1 escalation pattern.

**What WOULD close it** (~30 LOC pg-gated, single
`#[compio::test]`): build a fixture state with a real `Database`,
insert one over-threshold + one within-threshold wake_jobs row, call
`run_wake_jobs_takeover_once`, assert the return is `1` and the
over-threshold row transitioned. ~15 LOC additional for the
`database.is_none()` branch using `AppState::new_in_memory_for_test`
returning `0` immediately.

**Severity**: MINOR. The wrapper is thin enough that the SQL tests
plus the cadence pins together arguably cover the load-bearing
properties. The escalation chain that motivated R19-C1, however,
was an orchestrator-layer wedge (rows wedged on `lessee_updated_at`
+ controller crash mid-wake), and a sweep-wrapper regression test
is the matching layer. Defer if Phase-4 LOC budget is tight.

---

### [R32-T1] [CARRY, narrowed] R31-T-derive_url: 3 → 2 hardcoded
7777 sites; nomad_ch.rs now uses `AGENT_PORT` constant

**Where (still hardcoded)**:
- `crates/sandbox/src/restore_handler.rs:2392`
  (`RealRestoreBackend::derive_agent_url`)
- `crates/sandbox/src/restore_handler.rs:2471`
  (`RealRestoreBackend::register_restored` agent_url construction)

**Where (closed)**:
- `crates/sandbox/src/backend/nomad_ch.rs:1245` (wake path)
- `crates/sandbox/src/backend/nomad_ch.rs:2043`
  (`derive_agent_url`)

Both surviving sites belong to `RealRestoreBackend` (restore-path
helper). They format `http://10.{subnet_second_octet}.{100+idx}.2:7777`
using a string-literal `:7777` rather than `:{AGENT_PORT}`. Same
single-source-of-truth violation R13-T-derive_url surfaced 6
cycles ago. **Severity**: MINOR. ~2-line fix; import
`zeroship_sandbox_agent::AGENT_PORT` and replace the literals.
Could be bundled with R32-T2 if a restore_handler.rs edit lands
this cycle.

---

### Carries unchanged from r31

All carries from r31's §Carries unchanged table remain unchanged at
r32 entry. Counts:

| Tag | r31 round | r32 round | Notes |
|-----|-----------|-----------|-------|
| R30-T1 admin call-chain integration | 2nd | **3rd** | Phase-4 gate. Highest leverage. |
| R29-T2 T5 drive() integration | 3rd | **4th** | Phase-4 gate. |
| R29-T3 staging-skip contract | 3rd | **4th** | Phase-4 gate. |
| R28-T2 verbatim-msg exit | 10th | **11th** | Phase-4 gate. |
| R27-T3 boot-failure composition | 6th | **7th** | Phase-4 gate. |
| R30-T2 futures::join! cancel-safety | 2nd | **3rd** | Optional. |
| R30-T3 wake_machine half-dead rollback | 2nd | **3rd** | pg-gated. |
| R29-T4 BackendBuilder unit tests | carry | **carry** | — |
| R29-T5 release-log emission | optional | **optional** | — |
| R29-T1 housekeeper docstring | doc-only | **doc-only** | — |
| R27-T6-LIB sweep orchestration | 5th | **6th** | pg-gated. |
| R27-T4 read_snapshot_row pg | 4th | **5th** | pg-gated. |
| R28-S1 sanitize bare-UUID widening | carry | **carry** | Phase 2. |
| r1-DISC-2 transport-flake variants | optional | **optional** | — |
| R22-T3 retry-race pg | 11th | **12th** | — |
| R31-T1 gc_stop_chunked total-processed assert | NEW r31 | **2nd** | ~5 LOC trivial. |
| R31-T2 / R31-A1-H3 vm_index mutex stress | NEW r31 | **2nd** | ~25 LOC. |
| R25-T1 stress harness | 7th | **8th** | Cross-worktree. |
| R25-T3 vm_index race | 7th | **8th** | Cross-worktree, narrowed. |
| R28-T3 ext4 magic | cross-worktree | unchanged | Driver-side. |
| R28-T5 wire-schema parity | cross-worktree | unchanged | Phase 3. |
| R28-T8 prod-state driver fixture | cross-worktree | unchanged | Driver worktree. |

---

## [R32-V1] Verification — `alloc_first_seen_logged` lifetime

The r32 brief explicitly asked whether the new boolean stays set
across retries or gets reset. Re-reading
`crates/sandbox/src/backend/nomad_ch.rs:3040-3100`:

```rust
async fn wait_for_alloc_running(...) -> Result<(), String> {
    let fn_started = Instant::now();                              // L3045
    let deadline = fn_started + timeout;                          // L3046
    ...
    let mut alloc_first_seen_logged = false;                      // L3058  ← function scope
    while Instant::now() < deadline {                             // L3059  ← loop entry
        let resp = http_get_unsigned(&url, ...).await;
        match resp {
            Ok(r) if r.status == 200 => {
                ...
                let alloc_arr = allocs.as_array();
                if !alloc_first_seen_logged                       // L3087  ← gate
                    && alloc_arr.map(|a| !a.is_empty()).unwrap_or(false)
                {
                    tracing::info!(...);                          // L3090
                    alloc_first_seen_logged = true;               // L3095  ← latch
                }
                ...
            }
            ...
        }
        ...
    }
}
```

The latch is declared OUTSIDE the `while` loop and is only ever
mutated to `true`. There is no `= false` assignment anywhere inside
the loop body. The same `alloc_first_seen_logged` binding persists
across every poll iteration. Across retries (parse-error continue
at L3082, HTTP non-200 continue branches), the latch state is
preserved — the `continue` jumps back to the `while` condition,
not to the `let mut` declaration. **Verdict: correct.**

The shape mirrors the existing `last_parse_log_at` / `last_http_log_at`
rate-limit latches (L3050-3052), which also persist across the
loop body. The pattern is consistent and the bug class the brief
hypothesised (reset-on-retry) is not present.

---

## [R32-G1] Gap analysis — should `tracing::info!` emits be
unit-tested?

The r32-T1 commit adds two `tracing::info!` emits with structured
fields (`sandbox_id`, `job`, `elapsed_ms`) and a message string.

**Should they be unit-tested?** No.

1. **Wire-format semantics**: the emits are diagnostic
   scaffolding consumed by `journalctl -u zsbx-ctl.service -o json
   | jq` for cluster-cycle attribution. They are NOT a stable
   contract. The CREATE-path breakdown (`prep / schedule / dispatch
   / in-VM`) the commit message describes is an operator-side
   workflow, not a wire surface that other services consume.

2. **Cost of pinning**: a `tracing-test` assertion on the message
   string ("sandbox/nomad-ch alloc_first_seen") would freeze the
   string as an interface — any future tweak (e.g., renaming to
   `alloc_visible` for clarity) would require a same-PR test edit
   for zero observable benefit. The whole reason the commit lands
   as observability-only is so it can iterate freely on cluster
   feedback.

3. **What test layer IS appropriate**: a cluster-cycle reviewer
   commit (the one `8d82ecde` itself represents) is the right
   layer. The emit's correctness was verified by the c=1 ×3
   cluster cycle described in the commit body. Pinning at the
   unit-test layer would be a category error — like unit-testing
   a `println!` in a debug helper.

4. **What WOULD warrant a test**: if any of these emits ever
   start gating control flow (e.g., a sweep that reads
   `submit_done` timestamps from journal output as a recovery
   signal), then yes — at that point the emit transitions from
   observability to contract. Today, neither does.

**Recommendation**: leave uncovered. Do not add a tracing-subscriber
assertion. Mark this question CLOSED.

---

## Phase 4 cutover gate sufficiency

**INSUFFICIENT** (unchanged from r31). Same 8 asks:

- **R30-T1** admin-snapshot detach-chain integration (3rd round).
- **R29-T2** T5 drive() integration (4th round).
- **R29-T3** staging-skip contract (4th round).
- **R28-T1** Phase 3 manifest validation (cross-worktree).
- **R28-T2** verbatim-msg exit (11th carry).
- **R28-T3** ext4 magic (cross-worktree).
- **R28-T5** wire-schema parity (cross-worktree).
- **R27-T3** boot-failure composition (7th carry).

Controller-side LOC budget unchanged: **~355 LOC**. New r32 items
(R32-T2 ~30 LOC, R32-T1 ~2 LOC) are MINOR and not Phase-4 gates.

---

## Delta accounting

Lib test count: **551** (r31 baseline 549, **+2 net**).

| Commit (r31→r32 range) | What landed in scope | Tests added |
|---|---|---|
| `a6e517b2` wake-cadence cherry-pick | tightened polling cadences (livez 150→50ms, alloc_running 250→100ms) | +1 or +2 (carries existing tests from main) |
| `c56893b2` raw_exec.enable removed | scripts-only (Nomad client config) | 0 |
| `4ac1e526` parse-error sleep tighten | 250→100ms in `wait_for_alloc_running`; pure timing | 0 |
| `1d58ab53` r32-T1 trace points | two `tracing::info!` emits, one boolean latch | 0 (correctly, see G1) |
| `8d82ecde` r32-T1 cluster review | docs-only | 0 |

Net: +2. Pg-gated unchanged at 94.

---

## To test-cov r33 backlog (~445 LOC controller-side)

1. **R30-T1** — admin-snapshot detach-chain integration. ~80 LOC
   pg-gated. **IMPORTANT (3rd round).**
2. **R29-T2** — T5 drive() integration. ~120 LOC pg-gated.
   **IMPORTANT (4th round).**
3. **R29-T3** — staging-skip contract. ~50 LOC.
   **IMPORTANT (4th round).**
4. **R28-T2/R26-T1** verbatim-msg exit. ~75 LOC.
   **IMPORTANT (11th).**
5. **R27-T3** boot-failure composition. ~30 LOC.
   **IMPORTANT (7th).**
6. **R31-T1** gc_stop_chunked total-processed. ~5 LOC. **MINOR.**
7. **R31-T2 / R31-A1-H3** vm_index mutex stress. ~25 LOC. **MINOR.**
8. **[R32-T2]** [NEW] takeover-sweep orchestrator integration.
   ~30 LOC pg-gated. **MINOR.**
9. **[R32-T1]** [CARRY, narrowed] derive_url AGENT_PORT
   restore_handler 2 sites. ~2 LOC. **MINOR.**
10. **R30-T2** futures::join! cancel-safety. ~40 LOC. **MINOR.**
11. **R30-T3** wake_machine half-dead rollback. ~60 LOC pg-gated.
    **MINOR.**
12. **R29-T4** BackendBuilder unit tests. ~40 LOC. **MINOR.**
13. **R29-T5** release-log emission. ~25 LOC. **MINOR (opt).**
14. **R29-T1** housekeeper docstring. ~5 LOC. **MINOR doc.**
15. **R27-T6-LIB** sweep orchestration. ~30 LOC pg-gated.
    **MINOR (6th).**
16. **R27-T4** read_snapshot_row pg. ~40 LOC pg-gated.
    **MINOR (5th).**
17. **R28-S1** sanitize bare-UUID. ~10 LOC. **MINOR; Phase 2.**
18. **r1-DISC-2** transport-flake. ~30 LOC. **MINOR (opt).**
19. **R22-T3** retry-race pg. ~80 LOC. **MINOR (12th).**

19 items at r32 close; net **+2** from r31 (R32-T1 carry-narrowed
counted separately, R32-T2 new). R32-G1 (tracing emit coverage
question) closed without action — see §[R32-G1].

---

## Notes for r33

- **r32 is a stability round.** No new IMPORTANT; the cherry-picks
  (cadence, raw_exec, parse-error sleep) are perf/hygiene and do
  not change semantics that warrant new tests. The two new
  trace emits are observability-only.

- **Pattern from r30-r31 continues**: every new INTEGRATION-LAYER
  gap surfaced is in the "predicate-unit-tested but orchestrator-
  not-tested" class. R32-T2 is the latest instance:
  `claim_orphan_wake_for_recovery` (predicate) has 5 pg cases,
  `run_wake_jobs_takeover_once` (orchestrator) has 0. Same
  pattern as R30-T1 (`release_vm_index_after` predicate covered;
  `stop_inner` admin-chain not) and R29-T2/T3 (T5 predicates
  covered; drive() loop not). The rule articulated at r31 close
  applies again here.

- **R32-V1 verification is the model for trace-emit reviews**:
  read the variable scope, check for re-init points,
  trust-but-verify with a direct file read. ~5 minutes of work
  closes the question without a unit test.

- **Build state**: `cargo test -p zeroship-sandbox --lib` 551
  passed / 0 failed / 1 ignored at HEAD `8d82ecde`. Two
  pre-existing warnings unchanged.

- **Backlog cardinality**: 19 at r32 close (17 at r31 close, +1
  R32-T2 new, +1 R32-T1 split out from R31-T-derive_url because
  the surface narrowed from 3 sites to 2 — same severity, same
  effort class, but separated for accuracy).
