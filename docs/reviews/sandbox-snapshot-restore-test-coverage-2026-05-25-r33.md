# Sandbox/snapshot-restore — test-coverage r33 review

Date: 2026-05-25 (UTC). HEAD at audit: `fe8c9216` (R32-P1 parallel
mkfs + cycle-53 reviewer paperwork; `sandbox-snapshot-restore`
worktree; clean tree). Round 33. Prior: `docs/reviews/
sandbox-snapshot-restore-test-coverage-2026-05-25-r32.md` (HEAD
`8d82ecde`, 551 lib tests).

**Lib test count at HEAD: 551 passed; 0 failed; 1 ignored**
(`cargo test --lib -p zeroship-sandbox` verified locally, 4.12 s).
**Unchanged from r32.** Pg-gated count: **94** (unchanged;
`grep -cE "^\s*#\[compio::test\]"` on `tests/sandbox_pg_e2e.rs`).

## TL;DR

This round answers the five brief questions directly.

- **Q1 (R32-P1 parallel `mkfs.ext4` coverage)**: NOT covered by a
  unit test that exercises the two-image-creation-in-parallel path.
  The brief said "lib tests sufficient" — accurate at the
  *idempotency* layer (3 existing tests pin
  `create_ext4_image_if_missing` post-conditions, see §[R33-V1]),
  but no test runs the `std::thread::scope`-bracketed call shape
  introduced at `nomad_ch.rs:1155-1174`. The threading wrapper is
  uncovered. Severity: **MINOR**, opening **[R33-T1]** below.

- **Q2 (lib count = 551)**: **Confirmed.** Diff from r32's 551 is
  net 0. R32-P1 added zero tests; the new `std::thread::scope`
  block is reached only on a cold-boot CREATE path that requires
  root + a real `mkfs.ext4` binary, so neither it nor its existing
  helpers `create_ext4_image_if_missing` get a new harness in this
  round.

- **Q3 (R32-T2 carry — wake_machine orchestrator pg-gated)**:
  **OPEN. 2nd carry.** `run_wake_jobs_takeover_once` still has zero
  orchestrator-layer integration test. SQL primitive coverage
  unchanged (5 pg-gated cases at `tests/sandbox_pg_e2e.rs:4740,
  4792, 4855, 4912, 4916`). Cadence-pin
  (`wake_jobs_takeover_cadence_is_60s` at `sweep.rs:1554`) and
  threshold-pin (`:1565`) unchanged, neither exercises the
  wrapper. See **[R33-T2]**.

- **Q4 (cadence cherry-pick `a6e517b2` + `4ac1e526` time-based
  assertion)**: NOT covered, and **correctly so**. The cadence
  reductions (150→50ms livez probe, 250→100ms alloc_running parse-
  error + end-of-loop, 250→100ms in restore_handler blocking
  variants) are pure timing tweaks inside polling loops bounded by
  upper-budget timeouts. No constants are pinned (the literals are
  inline in the `compio::time::sleep(Duration::from_millis(50))`
  expressions, not named constants), so there is nothing for a
  cadence test to read. **Recommendation**: leave uncovered; if a
  reviewer wants pin-level discipline here, the prerequisite is to
  extract `LIVEZ_POLL_CADENCE` and `ALLOC_POLL_CADENCE` as named
  consts at module scope — that refactor is independent of test
  coverage. See **[R33-G1]** below.

- **Q5 (R32-T1-derive_url carry — 2 hardcoded 7777 sites in
  `restore_handler.rs`)**: **STILL OPEN.** Both surviving sites
  unchanged: `restore_handler.rs:2396` (`derive_agent_url`) and
  `restore_handler.rs:2475` (`register_restored`). The brief
  references "3 hardcoded 7777" — that was r32's pre-narrow count;
  r32 closed the third site at `nomad_ch.rs:2043` last round. Now
  it's **2 sites** and **2nd carry** since the narrow. See
  **[R33-T3]**.

**TOTAL NEW r33 items: 1 (R33-T1 MINOR — threading-shape coverage).
R32-T2 carried (2nd round). R32-T1-derive_url carried as R33-T3
(2nd round at the narrowed-to-2 surface).** All other r31/r32
carries unchanged.

---

## CRITICAL

None.

---

## IMPORTANT

No new IMPORTANT items in r33. All prior IMPORTANT carries
unchanged (see §Carries unchanged below).

---

## MINOR

### [R33-T1] [NEW] R32-P1 parallel-mkfs threading shape uncovered

**Where**: `crates/sandbox/src/backend/nomad_ch.rs:1155-1174`
(the `std::thread::scope(|s| { … })` block staging
`workspace.img` + `home.img` concurrently).

R32-P1 (`2faaf39b`) split the two `create_ext4_image_if_missing`
calls — previously sequential inside one spawn_blocking — into two
`std::thread::scope` children with a shared error-fold at the
join sites:

```rust
let (workspace_res, home_res) = std::thread::scope(|s| {
    let workspace_h = s.spawn(|| { create_ext4_image_if_missing(...) });
    let home_h      = s.spawn(|| { create_ext4_image_if_missing(...) });
    let w = workspace_h.join().unwrap_or_else(|p| Err(format!(..)));
    let h = home_h.join().unwrap_or_else(|p| Err(format!(..)));
    (w, h)
});
workspace_res?;
home_res?;
```

**Existing coverage** (sufficient for the inner helper):
- `create_ext4_image_if_missing_skips_when_file_exists`
  (nomad_ch.rs:6073) — idempotent path.
- `create_ext4_image_if_missing_skip_path_rejects_zero_byte_file`
  (`:6114`) — skip-path post-condition.
- `workspace_image_path_is_host_dir_join_workspace_img` (`:6034`).

**Coverage gap**: nothing exercises the *concurrent-call shape*.
Specifically uncovered:
- `workspace_res` propagates first when both fail (commit-msg
  promises "workspace.img's error is returned first").
- A panic in one child surfaces as `JoinHandle::join`'s `Err(Any)`
  → mapped string Err (commit-msg promises "match the existing
  outer spawn_blocking panic catch-all").
- Both children write disjoint paths and DO complete in either
  order (no accidental in-line serialisation via shared lock).

**What WOULD close it** (~30 LOC, no Postgres needed): a unit test
that pre-creates BOTH image paths as zero-byte sentinels (already
the rejection path the existing tests exercise), then dispatches
the same closure pattern with idempotent-skip paths or a function-
pointer indirection to a fault-injecting variant. Easier
alternative: extract the `std::thread::scope` body into a small
helper `fn stage_two_images_in_parallel(p1, sz1, p2, sz2)` and
test the helper directly with pre-created files (covers the
join+fold logic without needing mkfs).

**Severity**: MINOR. R32-P1 is a perf knob; if it regresses, the
worst-case is silent re-serialisation (slower CREATE, no
correctness change). The error-fold sites are 4 LOC each — low
mutation risk. But this is the first 3rd-party threading
primitive introduced into the CREATE hot path, and pinning the
ordering+panic-mapping contract is cheap insurance. Defer if
Phase-4 LOC budget is tight.

---

### [R33-T2] [CARRY, 2nd] `run_wake_jobs_takeover_once` orchestrator
integration test (pg-gated)

**Where**: `crates/sandbox/src/sweep.rs:388-425`.

Unchanged from R32-T2. SQL-layer coverage remains thorough (5
pg-gated cases). Orchestrator wrapper (database.is_none() short-
circuit, threshold lookup, log dispatch) untested. Same ~30 LOC
pg-gated single `#[compio::test]` would close it. Same Phase-4
LOC budget tradeoff applies — defer if other Phase-4 gates take
precedence.

**Round**: 2nd carry.

---

### [R33-T3] [CARRY, 2nd at narrowed-surface] derive_url hardcoded
`:7777` in `restore_handler.rs`

**Where (still hardcoded)**:
- `crates/sandbox/src/restore_handler.rs:2396`
  (`RealRestoreBackend::derive_agent_url`)
- `crates/sandbox/src/restore_handler.rs:2475`
  (`RealRestoreBackend::register_restored` agent_url construction)

Both surviving sites unchanged from r32 close. `nomad_ch.rs`
closed at r32. ~2-line fix: import
`zeroship_sandbox_agent::AGENT_PORT` (already used at
`nomad_ch.rs:1284`) and replace the literals. Severity: MINOR.

**Round**: 2nd carry at the narrowed-to-2 surface (5th
chronologically since R13-T-derive_url first opened).

---

### Carries unchanged from r32

All items in r32's §Carries unchanged remain at the same severity
and intent at r33 entry. Round-counter increments:

| Tag | r32 round | r33 round | Notes |
|-----|-----------|-----------|-------|
| R30-T1 admin call-chain integration | 3rd | **4th** | Phase-4 gate. |
| R29-T2 T5 drive() integration | 4th | **5th** | Phase-4 gate. |
| R29-T3 staging-skip contract | 4th | **5th** | Phase-4 gate. |
| R28-T2 verbatim-msg exit | 11th | **12th** | Phase-4 gate. |
| R27-T3 boot-failure composition | 7th | **8th** | Phase-4 gate. |
| R30-T2 futures::join! cancel-safety | 3rd | **4th** | Optional. |
| R30-T3 wake_machine half-dead rollback | 3rd | **4th** | pg-gated. |
| R29-T4 BackendBuilder unit tests | carry | **carry** | — |
| R29-T5 release-log emission | optional | **optional** | — |
| R29-T1 housekeeper docstring | doc-only | **doc-only** | — |
| R27-T6-LIB sweep orchestration | 6th | **7th** | pg-gated. |
| R27-T4 read_snapshot_row pg | 5th | **6th** | pg-gated. |
| R28-S1 sanitize bare-UUID widening | carry | **carry** | Phase 2. |
| r1-DISC-2 transport-flake variants | optional | **optional** | — |
| R22-T3 retry-race pg | 12th | **13th** | — |
| R31-T1 gc_stop_chunked total-processed | 2nd | **3rd** | ~5 LOC. |
| R31-T2/R31-A1-H3 vm_index mutex stress | 2nd | **3rd** | ~25 LOC. |
| R25-T1 stress harness | 8th | **9th** | Cross-worktree. |
| R25-T3 vm_index race | 8th | **9th** | Cross-worktree. |
| R28-T3 ext4 magic | cross-worktree | unchanged | Driver. |
| R28-T5 wire-schema parity | cross-worktree | unchanged | Phase 3. |
| R28-T8 prod-state driver fixture | cross-worktree | unchanged | Driver. |

---

## [R33-V1] Verification — R32-P1 idempotent-helper coverage

The r33 brief asked specifically whether the parallel mkfs path is
"covered by any test". Verified by enumerating
`create_ext4_image_if_missing`'s test surface:

```
nomad_ch.rs:6073  create_ext4_image_if_missing_skips_when_file_exists
nomad_ch.rs:6114  create_ext4_image_if_missing_skip_path_rejects_zero_byte_file
nomad_ch.rs:6034  workspace_image_path_is_host_dir_join_workspace_img
nomad_ch.rs:??    user_home_image_path_is_root_user_home_img (test name in run output)
restore_handler.rs:?  submit_restore_job_rejects_missing_user_home_img
config.rs:?       workspace_image_size_gb_zero_is_rejected_at_startup
```

All six tests target the **inner helper or path-derivation**.
None of them invoke the `std::thread::scope(|s| { … })` block at
`nomad_ch.rs:1155-1174`. The parallel-call shape is reached only
from `create()` proper, which requires:
- A live Nomad endpoint (the function `submit_nomad_job`s
  immediately after).
- root + `mkfs.ext4` in `$PATH` (the actual subprocess invocation).
- A non-`driver_stages_disk_images` config (the bypass branch
  short-circuits the spawn_blocking at line 1127).

Hence the threading shape (ordering of error returns, panic
mapping) is reachable only at cluster cycle time. **Verdict**:
the helper layer is covered; the orchestration layer is not. This
matches r32's recurring pattern — "predicate-unit-tested but
orchestrator-not-tested" — flagged at R32-T2 close. R33-T1 is the
latest instance.

---

## [R33-G1] Gap analysis — should cadence-tightening commits pin
the new timings?

The cherry-pick at `a6e517b2` (livez 150→50ms, alloc_running
250→100ms across 5 sites in `nomad_ch.rs` + `restore_handler.rs`)
and the followup at `4ac1e526` (parse-error sleep 250→100ms)
introduce 6 raw `Duration::from_millis(N)` literals on hot
polling paths. None are extracted to named constants.

**Should they be unit-tested?** No — but not because the constants
are unimportant. Three reasons:

1. **No symbol to pin.** Each literal is inline at its call site
   (`compio::time::sleep(Duration::from_millis(50)).await`,
   `std::thread::sleep(Duration::from_millis(100))`). A cadence
   test would have to re-read the literal via source-string match
   (brittle) or via behavioural timing (flaky in CI).

2. **Bounded by upper-budget timeouts.** The cadence reductions
   only change p50 latency — the upper bound is set by
   `agent_livez_timeout_secs`, `alloc_running_timeout_secs`,
   etc., all of which ARE configurable and ARE tested (e.g.
   `wait_for_alloc_running_surfaces_unreachability` lib-test at
   nomad_ch.rs:tests, plus 5 livez-related tests). A cadence
   regression here would manifest as "slower than expected p50",
   caught by perf-cycle reviewers (perf-r32 §"Where the 8.9s c=1
   CREATE goes"), not by lib tests.

3. **Refactor prerequisite.** Pinning these would first require
   extracting `const LIVEZ_POLL_CADENCE: Duration =
   Duration::from_millis(50);` etc. at module scope. That refactor
   IS reasonable — single-source-of-truth, mirrors the
   `CONNECT_TIMEOUT` already at `nomad_ch.rs:3910`,
   `wait_for_agent_silent`'s `PROBE_CADENCE` at `:4109`. But the
   refactor is independent of test coverage; the test follows
   only if the constants are extracted.

**Recommendation**: leave uncovered. If a future round wants to
discipline this surface, the prerequisite refactor is the
gating step:
1. Extract `LIVEZ_POLL_CADENCE_MS = 50` and
   `ALLOC_POLL_CADENCE_MS = 100` at module scope.
2. Add a `cadence_constants_pinned` lib-test asserting the
   literal values (mirrors `wake_jobs_takeover_cadence_is_60s`
   at sweep.rs:1554).
3. That's a ~15 LOC follow-up, low-risk.

**Mark this question CLOSED** without a R33-T-cadence carry. If
the refactor lands, the test follows automatically.

---

## Phase 4 cutover gate sufficiency

**INSUFFICIENT** (unchanged from r32). Same 8 asks:

- **R30-T1** admin-snapshot detach-chain integration (4th).
- **R29-T2** T5 drive() integration (5th).
- **R29-T3** staging-skip contract (5th).
- **R28-T1** Phase 3 manifest validation (cross-worktree).
- **R28-T2** verbatim-msg exit (12th).
- **R28-T3** ext4 magic (cross-worktree).
- **R28-T5** wire-schema parity (cross-worktree).
- **R27-T3** boot-failure composition (8th).

Controller-side LOC budget unchanged: **~355 LOC**. New r33 item
(R33-T1 ~30 LOC) is MINOR and not a Phase-4 gate.

---

## Delta accounting

Lib test count: **551** (r32 baseline 551, **+0 net**).

| Commit (r32→r33 range) | What landed in scope | Tests added |
|---|---|---|
| `2faaf39b` R32-P1 parallel mkfs | `std::thread::scope` wrap of 2 existing helpers | 0 (per brief; covered indirectly via helper tests, see R33-T1) |
| `fe8c9216` reviewer paperwork | cycle-53 docs/reviews/ | 0 (docs-only) |

Net: 0 lib, 0 pg-gated. Pg-gated unchanged at 94.

---

## To test-cov r34 backlog (~475 LOC controller-side)

1. **R30-T1** — admin-snapshot detach-chain. ~80 LOC pg-gated.
   **IMPORTANT (4th).**
2. **R29-T2** — T5 drive() integration. ~120 LOC pg-gated.
   **IMPORTANT (5th).**
3. **R29-T3** — staging-skip contract. ~50 LOC.
   **IMPORTANT (5th).**
4. **R28-T2/R26-T1** verbatim-msg exit. ~75 LOC.
   **IMPORTANT (12th).**
5. **R27-T3** boot-failure composition. ~30 LOC.
   **IMPORTANT (8th).**
6. **R31-T1** gc_stop_chunked total-processed. ~5 LOC. **MINOR (3rd).**
7. **R31-T2/R31-A1-H3** vm_index mutex stress. ~25 LOC. **MINOR (3rd).**
8. **R33-T2** [CARRY 2nd] takeover-sweep orchestrator integration.
   ~30 LOC pg-gated. **MINOR.**
9. **R33-T1** [NEW] parallel-mkfs threading shape. ~30 LOC.
   **MINOR.**
10. **R33-T3** [CARRY 2nd] derive_url AGENT_PORT (2 sites). ~2 LOC.
    **MINOR.**
11. **R30-T2** futures::join! cancel-safety. ~40 LOC. **MINOR (4th).**
12. **R30-T3** wake_machine half-dead rollback. ~60 LOC pg-gated.
    **MINOR (4th).**
13. **R29-T4** BackendBuilder unit tests. ~40 LOC. **MINOR.**
14. **R29-T5** release-log emission. ~25 LOC. **MINOR (opt).**
15. **R29-T1** housekeeper docstring. ~5 LOC. **MINOR doc.**
16. **R27-T6-LIB** sweep orchestration. ~30 LOC pg-gated.
    **MINOR (7th).**
17. **R27-T4** read_snapshot_row pg. ~40 LOC pg-gated.
    **MINOR (6th).**
18. **R28-S1** sanitize bare-UUID. ~10 LOC. **MINOR; Phase 2.**
19. **r1-DISC-2** transport-flake. ~30 LOC. **MINOR (opt).**
20. **R22-T3** retry-race pg. ~80 LOC. **MINOR (13th).**

20 items at r33 close; net **+1** from r32 (R33-T1 new; R33-T2 and
R33-T3 are carries with rename for clarity, not new). R33-G1
(cadence pin question) closed without action.

---

## Notes for r34

- **r33 is a perf-knob round.** R32-P1 (parallel mkfs) is the only
  semantic-relevant landing; the rest is reviewer paperwork. The
  threading shape it introduces is the first new uncovered
  primitive in 3 rounds. R33-T1 is the appropriate-layer ask.

- **Recurring pattern continues**: predicate-tested but
  orchestrator-untested. R32-T2 (wake_jobs_takeover wrapper)
  open at 2nd round. R33-T1 (parallel-mkfs wrapper) opens at the
  same layer. Both are <40 LOC asks; both could land in one
  Phase-4-adjacent PR.

- **Cadence question (R33-G1) is closed**: leave the 6 inline
  literals as-is until a refactor extracts module-scope consts.
  Don't bolt on a tracing-test or behavioural-timing test as a
  half-measure.

- **R33-T3 is approaching cycle 6 since R13-T-derive_url first
  surfaced** (6 cycles ago, 2 closures + 2 new sites split-out
  along the way). At ~2 LOC fix cost, the open-round is the news,
  not the surface. Bundle with any `restore_handler.rs` edit
  this cycle to amortise.

- **Build state**: `cargo test -p zeroship-sandbox --lib` 551
  passed / 0 failed / 1 ignored at HEAD `fe8c9216`, 4.12 s.
  Pre-existing warnings unchanged.

- **Backlog cardinality**: 20 at r33 close (19 at r32 close,
  +1 R33-T1 new; R33-T2/T3 are renames of R32-T2 and R32-T1
  respectively).
