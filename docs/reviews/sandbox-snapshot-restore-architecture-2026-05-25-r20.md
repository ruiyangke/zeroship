# Sandbox snapshot-restore architecture review — 2026-05-25 r20

**Reviewer**: architecture-r20 (post-cluster-cycle-17 retrospective lens)
**HEAD**: `8718120b` (`feat/sandbox-snapshot-restore`, driver v7 pin landed)
**Predecessor**: r19 at `87f40229`. Diff: R19-T1 (`6fbfafb3`), R19-API1
(`fde4f51c`), r25/r26 reviewer artifacts (`b18782f6`, `2fed96bc`),
R20-I1 ADR extract (`ed30f5d0`), driver v6 + v7 pins (`dea68995`,
`8718120b`), smoke-r16 review (`c248ed3a`).
**Lens**: architecture (READ-ONLY).

## Summary

17 cluster cycles. **17 distinct production-only signals** (one per
cycle). The chain has now converged: C-4..C-8c was *architecture*
diagnosis (the teardown wall-time was misread), C-7-LT-2..LT-5 was
*infrastructure-bug* diagnosis (ureq probe wedge, ch.sock readiness,
driver-side path rewriter port), and **C-7-LT-6** (driver-side
per-field allow-list, fix `f73f2b49`, pinned `8718120b`) is the first
defect of the chain that is **pure validator-rule tuning** — no
architectural surface change, no protocol change, no new control loop.
This is the architectural maturity signal: the system has stopped
revealing new layers and is now hardening an existing layer's
invariants.

The price for getting there is a debt the maturity arc surfaces:
**three coexisting `config.json` rewriters** (Rust controller for
network fields, Go driver for path fields, bash wrapper for path
fields — last one not yet retired). The driver-v7 fix is correct
in isolation but the multi-rewriter contract has no versioning, no
ownership rule, and no enforcement that exactly one path-rewriter
runs at exec time. This is r20's load-bearing architecture finding.

Five findings (2 IMPORTANT, 3 MINOR). r19-A1 (vm_index leak ledger
+ reaper) carries forward; smoke-r14..r16 have all reported `leak
counter = 0`, which empirically de-prioritises but does not retire
the architectural gap.

## CRITICAL

None.

## IMPORTANT

### [r20-A1] Three coexisting `config.json` rewriters with no schema-version contract — pick one path-rewriter and retire the other before driver v7 ships GREEN

- **Where**:
  - Rust controller, `restore_handler.rs:1063-1101` (`rewrite_config_json`):
    rewrites `net[].tap`, `net[].mac` from `vm_index` at job-submit
    time. Explicit comment at `:1075-1079` documents the
    division — paths are excluded *by intent* because the controller
    can't know `NOMAD_TASK_DIR`.
  - Bash wrapper, `nomad-vm-wrapper.sh:486-624`: rewrites
    `disks[*].path`, `serial.file`, `console.file`, `fs[*].socket`
    at exec time under `assert_under_task_dir` (R15-S2 anchored
    allow-list).
  - **NEW** Go driver, `nomad-driver-ch/ch/config_rewrite.go`
    (`f73f2b49`): rewrites the same path-shape fields the wrapper
    does, with the C-7-LT-6 per-field allow-list
    (`PathFieldRuntimeFile`, `PathFieldDisk`, `PathFieldFsSocket`).
    Reads `sandbox_id` from the alloc's `driverConfig` so per-tenant
    isolation keys on the *current* alloc's id, not a wildcard.
- **What I see**:
  1. **Driver bypasses wrapper for restore.** Smoke-r16 (`c248ed3a`)
     confirms the Go driver consumes `restore/config.json` verbatim
     (the wrapper was originally designed for fresh-boot, where
     rootfs is `cp`'d INTO the alloc before the rewriter runs —
     `nomad-vm-wrapper.sh:300-305`). The wrapper's strict
     `assert_under_task_dir` for disks WOULD reject the persistent
     workspace path; the driver port has to relax it. So the two
     rewriters now have *different rules* for the same field. If
     the wrapper is ever re-introduced on the restore branch
     (operator override, fallback path, future refactor), it
     rejects what the driver accepted.
  2. **No schema-version marker on the artifact.** `SnapshotMetadata`
     (`snapshot_store.rs`) carries `ch_version` (CH binary version)
     but no rewriter-contract version. A v2 snapshot that adds a
     new path field (e.g. `vsock.socket`, `api_socket`, already
     foreshadowed in r15) cannot be detected at parse time — it
     silently flows through any rewriter that doesn't know to
     touch it.
  3. **Audit ambiguity.** Operator forensics on a wedged wake's
     final `config.json` cannot tell which layer last wrote the
     value. The pattern is identical to a multi-writer cache
     coherence problem — without versioning + ownership, the
     contract drifts.
- **What's the right architectural endpoint**: **driver-only** path
  rewriting + **controller-emitted versioned schema marker**.
  - **Driver owns path rewrites** (post-C-7-LT-6 — the per-field
    allow-list, sandbox_id-keyed isolation, content-addressed-roots
    plumbing are already in place; the validator-tuning is done).
  - **Wrapper retires its path-rewrite block** (`nomad-vm-wrapper.sh:486-624`
    deleted in the same PR cycle that lands driver v7 GREEN). The
    wrapper continues to own non-rewrite responsibilities (tap
    plumbing, env-var setup, exec). Test contract: a snapshot whose
    `disks[*].path` lives under `/var/zeroship/ch/<sbx>/…` MUST
    succeed via driver; the wrapper's `assert_under_task_dir`
    block must never run on a restore alloc.
  - **Controller emits `_zsbx_path_schema_version: 1`** as a top-
    level field in `config.json` at snapshot capture time. The
    driver's rewriter checks the marker at restore entry; mismatch
    returns a typed error (`UnsupportedPathSchemaVersion`) rather
    than silently producing a partial-rewritten config. The
    network rewriter on the controller stays as-is; bump the
    marker when *either* contract changes a field.
  - **Field-coverage matrix as a unit test.** Pin every path-bearing
    field the driver knows about in `tests/restore_task_test.go`;
    fail the build if a new field shows up in a captured config
    without a corresponding `PathFieldKind` constant.
- **Severity**: IMPORTANT (not CRITICAL — driver v7 fixes the
  functional bug C-7-LT-6 surfaced; the multi-writer contract debt
  is a deferred architectural debt, not a runtime regression).

### [r20-A2] r19-A1 carry — VmIndexAllocator leak recovery still on paper; cleanup_orphans_at_startup confirmed unchanged

- **Where**: `nomad_ch.rs:431-464` (`cleanup_orphans_at_startup`)
  enumerates Nomad jobs only — no `VmIndexAllocator` touch. Three
  leak-comment sites identified in r19 (`nomad_ch.rs:1081`, `:1162`,
  `:1184`) unchanged. No grep hits for `leak_reaper`, `leak_ledger`,
  `VmIndexLeakLedger`, or `SlotLeaked` outside of r19's own review
  text and smoke-r13.
- **What I see**: r19-A1 promised a `VmIndexLeakLedger` + periodic
  reaper. Neither has landed since r19. Smoke-r14/r15/r16 have all
  reported `vm_index_leaks_total = 0` in the operator log
  (smoke-r16 `c248ed3a:97-104`) — the empirical leak rate is
  effectively zero now that C-7-LT-2 fixed the probe-wedge root
  cause. The fence_passed=true line has carried THREE cluster
  cycles in a row (smoke-r14, r15, r16). The vm_index reserve race
  has been exercised on the slow side (smoke-r16 resolved at
  attempt 17/36, no leak).
- **Re-prioritization given C-7-LT-2 holds**: this is now a
  **defence-in-depth gap**, not an active-pathology gap. Re-rank
  from CRITICAL → IMPORTANT. The architectural argument from r19
  still stands (the recovery contract is documented but unimplemented;
  restart-as-reclaim races a live source agent), but the empirical
  blast radius has shrunk to "the next pathology that bypasses
  C-7-LT-2." Recommended sequencing: **defer the ledger; ship the
  per-process reaper first.** A `detach_isolated` loop that walks
  `VmIndexAllocator`'s held-but-unreferenced set every N seconds
  and re-runs `wait_for_agent_silent` on each derived agent URL
  releases liveness without controller restart and doesn't need a
  pg schema change. The ledger can wait for the first measurable
  multi-process leak (which would require fence_passed=false to
  return; today it has not).
- **Why it matters**: r19's three lying-comment sites still claim
  "orphan-prune will reclaim." Either implement reaper or delete
  the comments. Smoke ops on call should not be reading code that
  describes a behaviour the runtime doesn't implement (the C-8b
  retrospective in r19 already cost the project ~12 cycles for
  exactly this reason).

## MINOR

### [r20-A3] R20-I1 ADR-extraction is the correct precedent — audit the other `from_X_timeout`-class functions for the same treatment

- **Where**: R20-I1 (`ed30f5d0`) extracted `from_host_fence_timeout`'s
  ~118-line doc into `docs/decisions/2026-05-25-vm-index-retry-policy.md`
  with full empirical-history trace (C-7 → C-8a → C-8b → smoke-r13
  retrospective → C-7-LT-1). This is the *first* time this codebase
  has used an ADR to carry the empirical-history of a load-bearing
  constant; the precedent is exactly the discipline r19-A4 asked
  for (refuse doc comments that describe behaviour the code does
  not enforce; the ADR is non-normative, the function code is the
  enforcement).
- **What I see**: candidate functions for the same treatment, ranked
  by load-bearing-constant density:
  1. **`wait_for_agent_silent`** (`nomad_ch.rs` near `:3220-3438`):
     C-7-LT-2's two-phase TCP probe + `consecutive_misses` semantics.
     The constants here (CONNECT_TIMEOUT=150ms, miss-threshold=2,
     probe cadence) come out of smoke-r13's retrospective. Today
     they live in comments only. **Recommend extract**.
  2. **`wait_for_job_gone`** (`nomad_ch.rs:2780,2805`): hard-coded
     30 s (`Duration::from_secs(30)` at `:1051`) that r19-A4
     identified as load-bearing for the teardown wall-time
     composition story. C-8b's "2× factor" coincidence pivoted on
     this constant. **Recommend extract** — short ADR, but ties up
     the r19-A4 thread definitively.
  3. **`INSERT_WAKE_JOB_MAX_RETRIES = 3`** (R19-I4, `f2485210`):
     small constant, isolated, ADR-overhead probably exceeds
     value. **Skip**.
  4. **`takeover_threshold_secs` / `takeover_interval_secs`**
     (R19-C1, `8d163d58`, sweep.rs:440): these *should* have an ADR
     describing the 60 s threshold derivation vs `from_host_fence_timeout`'s
     ceiling. Today they're a code default + env-var pair; the
     interaction with the wake-machine's worst-case is undocumented.
     **Recommend extract**.
- **Action**: bundle (1)+(2)+(4) into a single follow-up
  `docs/decisions/2026-05-25-teardown-timing-constants.md`. Same
  shape as the R20-I1 ADR; consolidates the timing-constant model.

### [r20-A4] Smoke-r14..r16 carries `fence_passed=true` three cycles in a row — promote C-7-LT-2 from "fix" to "architectural invariant"

- **Where**: smoke-r14, smoke-r15, smoke-r16 verbatim:
  `probes=2 consecutive_misses=2 elapsed_ms=300`. Identical line,
  three cycles. The two-phase TCP probe is the load-bearing
  invariant the rest of the teardown pipeline now assumes.
- **Action**: rename the `crates/sandbox/src/backend/nomad_ch.rs`
  doc block at `:3070-3090` from "previous implementation /
  today" prose to a contract statement: *"The host-fence probe
  MUST connect-fail on two consecutive probes within
  `host_fence_timeout` to declare fence_passed=true; ureq
  request-deadline IS NOT sufficient and was the source of the
  C-4..C-7-LT-1 phantom (see
  docs/decisions/2026-05-25-vm-index-retry-policy.md §
  smoke-r13)."* Pin a `#[cfg(test)]` integration test that drives
  `wait_for_agent_silent` against a SYN-blackhole stub and asserts
  the connect-deadline path fires within 300ms. This converts
  "smoke confirmed it" into "regression-tested."

### [r20-A5] r19-A5 carry — other ureq sites: R19-I1 (livez) landed; Nomad-side ureq sites confirmed lower-risk; deferred-verification of R19-I1 two-phase probe still outstanding

- **Where**: R19-I1 landed (`82478a6b`, smoke-r16 review confirms
  the two-phase probe is in v30). But smoke-r14, r15, r16 *never
  exercised* it — the wake state machine has not reached
  `livez_polling` in three cycles because the bug surfaced at an
  earlier layer (C-7-LT-3 → C-7-LT-4 → C-7-LT-6). R20-I1's
  recommendation that smoke-r17 will be the first cycle to
  exercise R19-I1 is now the load-bearing test for that subsystem.
- **What I see**: the other ureq sites (`http_get_unsigned` /
  `http_delete_unsigned` for Nomad API at `nomad_ch.rs:2866-2906`,
  `signed_blocking_call` for in-VM agent at `:2989-3007`) remain
  on the spawn_blocking+ureq.timeout pattern. r19-A5's audit
  classified them as LOW risk (Nomad API is server-side, kernel-RST
  on failure) and that classification still holds.
- **Action**: tag the smoke-r17 brief with "first cycle that
  must exercise R19-I1 two-phase probe; falsification criterion
  = state machine reaches `livez_polling`." Until that cycle
  passes GREEN, R19-I1 is shipped but unverified-in-prod.

## Cross-lens consensus

- **API-surface r20 (`2fed96bc`)**: R20-API1 independently
  identified the three-rewriter coexistence (overlaps r20-A1).
  api-surface scopes the wire-contract piece (schema marker field
  name + envelope shape); **architecture (this review) scopes the
  ownership-rule + retirement sequence**. Aligned recommendation:
  driver owns path rewrite, wrapper retires path-rewrite block,
  controller emits the version marker.
- **Concurrency r20 (`b18782f6`)**: R20-C1 (WakeMachine terminal
  clobbers takeover-sweep terminal) is a separate write-write
  race surface unrelated to r20-A1 but coupling to r20-A2's
  recovery contract — if the sweep + machine both terminal-fail
  a wake at the slot-leaked state, the audit-trail loss is exactly
  the r19-A1 pathology in a different layer.
- **Code-quality r20**: R20-I1 ADR extract is the precedent
  r20-A3 generalises; the code-quality lens has independently
  recommended `from_host_fence_timeout`'s doc be ADR-extracted
  (now done at `ed30f5d0`).
- **Test-coverage r20**: a controller-level integration test
  that stubs `wait_for_agent_silent` to return `Err` and drives
  `stop()` (r19-A1's fix sketch test) is now overdue. The empirical
  leak rate is 0 across three cycles, so the test is the only
  remaining check that the leak-counter wiring works end-to-end.

## Lens hand-off

1. **API-surface r21**: define the `_zsbx_path_schema_version`
   field wire contract (top-level integer, default 1, presence
   required at snapshot capture, driver rejects mismatch with
   typed error). Coordinate with r20-A1.
2. **Code-quality r21**: bundle the r20-A3 ADR extracts —
   `wait_for_job_gone` + `wait_for_agent_silent` + takeover-sweep
   thresholds — into one `docs/decisions/2026-05-25-teardown-timing-constants.md`.
3. **Test-coverage r21**: land the deferred r19-A1 integration test
   (stubbed `wait_for_agent_silent` → forced `fence_passed=false`
   → assert leak telemetry fires + assert vm_index NOT released).
4. **Concurrency r21**: pick up R20-C1 (WakeMachine vs sweep
   write-write race) — the CAS-guard on `update_wake_job_state`
   is the structural fix; architecture-r20 endorses.
5. **Cluster-smoke r17**: first cycle to exercise R19-I1 two-phase
   livez probe; falsification criterion = state machine reaches
   `livez_polling` and emits `probes=K cadence=…` trace.

## r19-A1..A5 carry status (recap)

| r19 finding | r20 status |
|---|---|
| r19-A1 (vm_index leak ledger + reaper) | OPEN, **re-ranked CRITICAL → IMPORTANT** (r20-A2). 3 lying comments unchanged. Defence-in-depth, not active. |
| r19-A2 (wake_jobs takeover sweep) | **CLOSED** at `1d3724fe` + `8d163d58` (R19-C1 PR1+PR2). New write-write race surfaced by concurrency-r20 (R20-C1). |
| r19-A3 (per-phase wall-time metric for `stop()`) | OPEN — no commits. Empirical pressure low (fence_passed=true 3 cycles). |
| r19-A4 (`from_host_fence_timeout` doc comment) | **CLOSED** at `8e085598` + `ed30f5d0` (ADR extract). Precedent generalised in r20-A3. |
| r19-A5 (other ureq sites audit) | **CLOSED for livez** at `82478a6b`; Nomad-side sites confirmed LOW risk and skipped. R19-I1 in-prod-verified deferred to smoke-r17 (r20-A5). |
| r19-A6 (r18 carry-forwards) | superseded by r19-A2 closure + smoke chain progression. |

Architectural maturity model: r1..r10 was "discover the surface,"
r11..r19 was "diagnose the misreads," **r20 is the first cycle
where the chain is producing only validator-tuning signals, no
surface change**. Phase B cutover blocker list shrinks from "fix
the architecture" to "retire the redundant rewriter + add the
schema-version marker + ship the deferred reaper."
