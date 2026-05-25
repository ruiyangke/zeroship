# Sandbox/snapshot-restore — architecture r15 review

Date: 2026-05-25 (UTC)
HEAD at audit: `2afbb2dd` (C-8 + C-8a fix landed; cluster pin v24).
Round 15 of N (architecture lens).
Prior round: `sandbox-snapshot-restore-architecture-2026-05-25-r14.md`
at `d673e043` (one commit before `91ce9be5`).

Scope read: `crates/sandbox/**`, `crates/sandbox-agent/**` only.

## Summary

**5 NEW findings (1 CRITICAL, 2 IMPORTANT, 2 MINOR)**, all
re-framings of patterns the 9-cycle Phase B smoke series has now
made indisputable. **Phase B at HEAD `2afbb2dd` has surfaced 9
distinct production bugs (C-1, C-2, C-3, C-4, C-5, C-6, C-7, C-8,
C-8a) in 9 cluster cycles** — every smoke iteration except the
no-op re-test r3 peeled exactly one layer. That is not a curve;
that is a probability distribution. r15 reads the distribution and
identifies the **two diagnostic-debt wedges** that, if landed before
r1, would have caught **≥2 cluster bugs each** (R15-A1, R15-A2).

**The biggest leverage finding (R15-A1, CRITICAL)** is the **wake
contract** itself. C-4, C-6, C-7, C-8, C-8a are **five distinct
bugs caused by one design choice**: the wake handler returns
synchronously on the same HTTP request that triggered it, so its
correctness is coupled to two clocks the controller cannot bound —
the source-teardown wall-time (host_fence + Nomad purge + best-
effort `/shutdown` connect) and the client connection deadline (60
s, set by the stress client and ntex defaults). The C-8 patch
landed in `2afbb2dd` (dual-ceiling retry budget) is correct
defense-in-depth, but it's the **fifth code patch** stacked on the
same broken contract. The structural fix has had a name since the
C-7 commit message coined it: **C-7-LT = `202 Accepted` + status
URL polling**. Promoting C-7-LT from a deferred long-term note to
the **next sprint's flagship** is the only move that closes the
cluster's bug discovery loop on this code path.

**The runner-up (R15-A2, IMPORTANT)** is phase tracing as
**discipline**, not a one-off. The r14 review (R14-A4) flagged
this and 1 cycle later **nothing has moved**. `restore_handler.rs`
still has 21 phase-tracing lines; `snapshot_handler.rs`,
`admin_handlers.rs`, `nomad_ch::stop_inner` still have 0. C-6's
diagnosis took 2 cluster cycles (r5 → r7) to localize precisely
because `stop_inner`'s 4-step teardown was opaque. r15 elevates
this from "would be nice" to "the next cluster cycle's bug WILL be
in a phase-traceless handler — the choice is whether to pay the
discovery tax once more or fix this before T-8b-stress".

**Module size** (R15-A3): `restore_handler.rs` is now **3444
LOC**, up +343 since r14 (was 3101) and **+782 LOC in two
cycles**. The growth driver is the same as r14: each cluster
cycle pays interest by adding ~150-400 LOC to this one file. C-7's
fix + R14-A6's `from_host_fence_timeout` derivation + C-8a's
dual-ceiling cap + 4 new tests = +343 LOC, all on the same retry-
policy concern. The file is now the **largest in the snapshot/
restore module's hot path** (nomad_ch.rs has more LOC but is the
backend; `restore_handler.rs` is the handler the cluster series
keeps fixing). It crosses **a second threshold** at r15: it is
now **larger than `db.rs` (3303)** for the first time, making it
the **2nd-largest file in the crate**.

Counter-evidence: **R14-A6 landed at `c3edf968`** (per r14
recommendation) but the *very same cluster cycle that motivated
landing it* (smoke-r9) had to PIN HEAD AT `b8fae7b7` to **avoid**
R14-A6's regression. That's a clean architecture-lens datapoint:
the r14 finding "decouple constants from config" was correct **in
isolation** but **violated the unstated invariant "budget ≤ client
deadline"**. C-8a is the visible cost. r15 must take this on the
chin — the r14 recommendation was incomplete; the deadline ceiling
should have been part of R14-A6 from day 1.

**Cluster pattern (R15-A4, IMPORTANT)** is a frequency analysis
of the 9 bugs: C-3, C-6, C-8a are **runtime / async scheduling**;
C-1, C-2 are **env / argv plumbing**; C-4, C-5, C-7, C-8 are
**timing / budget / SLO mismatch**. **Six of nine bugs cluster on
the runtime+async+timing axis.** Test coverage priorities should
follow: integration tests that drive `restore_sandbox` with a
stub backend whose `vm_index_retry_policy` and `reserve_vm_index`
behaviour can be parameterised would have caught C-4, C-6 (via
silent-wedge assertion), C-7, C-8a — **four of the eight code-
side bugs**. That's R13-A1 / R14-T1 / R15-T1 still open after 7
EMERGENCY rounds.

R14-A1 (`detach_isolated` helper) is **still un-landed**;
CreateGuard::drop at `nomad_ch.rs:2002` still uses the bad
`compio::runtime::spawn(...).detach()` pattern. R10-A4 (nomad_ch
split, 5371+ LOC) and R10-A2/A3 (RestoreBackend trait surface)
unchanged. R4-A2 (LeasedVmSlot RAII) is now in its **14th
consecutive cycle** open.

## Module size table (compared to r14 baseline at `d673e043`)

| File | r14 LOC | r15 LOC | Δ | Action |
|---|---:|---:|---:|---|
| `crates/sandbox/src/backend/nomad_ch.rs` | 5399 | **5399** | 0 | R10-A4 + R11-A2 + R12-A4 carry — **unchanged for 4 cycles**. Still the only file > 5000 LOC. |
| `crates/sandbox/src/restore_handler.rs` | 3101 | **3444** | **+343** | C-7 fix (`493d6c1e`, +80) + R14-A6 (`c3edf968`, +234) + C-8a (`2afbb2dd`, +97 minus prior R14-A6 deletions). **Now the 2nd-largest crate file** (passed db.rs). +782 LOC in 2 cycles. |
| `crates/sandbox/src/db.rs` | 3303 | **3303** | 0 | R10-A1 / R11-A4 / R12-A5 carry-forward, **unchanged for 5 cycles**. |
| `crates/sandbox/src/lib.rs` | 2412 | **2412** | 0 | R4-A1 / R10-A6 / R11-A3 stable. |
| `crates/sandbox-agent/src/handlers.rs` | 2224 | 2224 | 0 | Out of arch scope. |
| `crates/sandbox/src/admin_handlers.rs` | 1781 | **1842** | +61 | C-6 fix landed (`91ce9be5`, +61). T9 / T10 / R10-A7 carry-forward. |
| `crates/sandbox/src/snapshot_store_gcs.rs` | 1768 | **1768** | 0 | R13-A2 carry — unchanged. |
| `crates/sandbox-agent/src/sig.rs` | 1595 | 1595 | 0 | Out of arch scope. |
| `crates/sandbox/src/backend/k8s.rs` | 1580 | 1580 | 0 | Out of arch scope. |
| `crates/sandbox/src/handlers.rs` | 1378 | 1378 | 0 | Stable. |
| `crates/sandbox/src/snapshot_aead.rs` | 1277 | 1277 | 0 | Healthy. |
| `crates/sandbox/src/persist.rs` | 1222 | 1222 | 0 | R9-S4b sibling. |
| `crates/sandbox/src/config.rs` | 1051 | 1051 | 0 | R4-A1 carry-forward. |
| `crates/sandbox/src/snapshot_handler.rs` | 955 | 955 | 0 | Stable. |

**Files > 5000 LOC**: 1 (`nomad_ch.rs` 5399, unchanged for 4
cycles).
**Files > 3000 LOC**: **3** (`restore_handler.rs` 3444,
`db.rs` 3303, `nomad_ch.rs` 5399) — same count as r14, but
**ordering changed**: restore_handler now ranks #2.
**Files > 2500 LOC**: 3, unchanged.

**The growth trajectory on `restore_handler.rs`**: r12 = 2680,
r13 = 2662, r14 = 3101, r15 = **3444**. +764 LOC in 3 cycles, all
of it driven by cluster-cycle bug fixes on the wake-retry concern
(C-4 + C-6 phase tracing + C-7 + R14-A6 + C-8a). The bug-fix-pays-
interest hypothesis (r14 R14-A2) is now an observed trend.

## Findings (NEW since r14)

### [R15-A1] The wake handler's synchronous-response contract is the **root cause** of 5 of the last 5 bugs on the wake path (C-4, C-6, C-7, C-8, C-8a) — promote C-7-LT (`202 Accepted` + poll) from "deferred long-term" to the next sprint flagship (CRITICAL, architecture-r15)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:240-267`
    (`VmIndexRetryPolicy::from_host_fence_timeout` — the C-8a cap).
  - `crates/sandbox/src/restore_handler.rs:395-453`
    (`reserve_vm_index_with_retry` — the inside of the wedge).
  - `crates/sandbox/src/admin_handlers.rs` (wake handler entry
    that owns the synchronous HTTP response).
- **Symptom timeline** (the data the 9-cycle smoke series produced):

  | Cycle | Bug | The wake contract's role |
  |---|---|---|
  | r5 → C-4 | wake returns 503 immediately because source-teardown still holds vm_index | wake response is **synchronous** ⇒ no way to wait through the teardown without holding the HTTP socket open |
  | r6 → C-4 fix | added bounded retry (60 × 2 s = 120 s wall-time) | the retry happens **inside** the synchronous response — the loop holds the socket |
  | r6 → C-6 | wake silent stall, 60 s no log | the 60 s ntex client deadline drops the future mid-sleep; the loop's success/exhaust log is **after** the await, so it never fires |
  | r7 → C-6 fix | detached teardown moved to isolated runtime | reduced cross-task contention, **but the silent-stall mechanism was not starvation — it was client cancellation** (per r8 falsification) |
  | r8 → C-7 | C-6 falsified; real cause was the 120 s budget exceeded the 60 s client deadline | the synchronous response constraint **makes 60 s a hard ceiling** on retry budget |
  | r8 → C-7 fix | shrink retry budget 120 s → 48 s + per-attempt log | budget now < client deadline; observable failure mode = clean 503 |
  | r9 → C-8 | source teardown takes ~150 s; 48 s budget exhausts before slot frees | the source-teardown wall-time is **physically larger** than any retry budget that fits inside the client deadline |
  | r14 → R14-A6 | derive budget from `cfg.host_fence_timeout_secs` (good in isolation) | but `(120 - 10) / 2 + 1 = 56 attempts × 2 s = 110 s` **silently re-introduces C-7** under default config |
  | r9 → C-8a | spotted before deploying R14-A6 | landed dual-ceiling patch (MIN of fence-derived + deadline-derived) |
  | r9 → C-8 fix (cluster cfg) | set `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` in cluster startup | acceptable for smoke; **silently degrades production SLO** for any operator using the default 120 s fence |

  All 5 bugs share **one root cause**: the wake handler's response
  is synchronous on the request that triggered it, so its
  correctness budget is **`min(source_teardown_wall_time,
  client_connection_deadline)`** — and those two numbers have **no
  contract between them**. The teardown is bounded by `host_fence
  + Nomad purge ≈ 30-150 s` depending on operator config. The
  client deadline is bounded by ntex defaults + whatever the caller
  set (60 s for the stress client). **The current architecture
  requires teardown < client deadline**, and the C-8 fix enforces
  it by **lowering the fence**, which **trades production
  resilience for smoke-pass-fail**.
- **Why CRITICAL**: this is the single largest unit of unclosed
  technical debt on the snapshot/restore path. The 9-cycle
  pattern says: every next bug on this code path will be on the
  same axis until the contract is fixed. r10 cluster smoke will
  pass under the C-8 cluster-config patch, but **the structural
  bug isn't closed — it's just been hidden behind a cluster-
  scoped configuration**.

  The C-7 commit message coined the name: `C-7-LT = 202 Accepted
  + status URL polling`. The C-8a commit message reiterates it
  ("The long-term fix remains C-7-LT ... which decouples wake
  response from the client connection deadline"). Both commits
  flag it as out-of-scope for their respective unblock; r15 says
  **it is now the in-scope concern** because the cluster has
  walked the rest of the synchronous-response design space.
- **Action**:
  1. **Promote C-7-LT to a flagship proposal.** Write
     `docs/proposals/sandbox-wake-async-response.md` covering:
     - **Endpoint contract**: `POST /admin/sandboxes/{id}/wake`
       returns `202 Accepted` with `Location:
       /admin/sandboxes/{id}/wake/status/{op_id}` (or in the
       response body) immediately after the row CAS to
       `restoring`. The handler returns. The wake work runs on a
       detached task (per R14-A1, on an isolated runtime).
     - **Status polling**: `GET /admin/sandboxes/{id}/wake/status/{op_id}`
       returns one of `{pending, succeeded, failed{error,
       message}}`. State is persisted in the existing `sandboxes`
       row (or a sibling `wake_ops` table — design choice).
     - **Idempotency**: the same `op_id` returns the same final
       result; the wake handler itself becomes idempotent on
       `(sandbox_id, generation)` so a client retry on 202 is
       safe.
     - **Backwards compatibility**: keep the synchronous response
       behind a feature flag for the first deploy, default-on
       only after the stress client switches to polling.
     - **Cancellation semantics**: client disconnect no longer
       cancels the wake — the detached task runs to completion.
       The retry budget is **no longer bounded by the client
       deadline**, so the C-8 cluster patch can be reverted
       (restore the 120 s fence default) and the retry budget
       can grow to envelope it.
     - **SLO**: status transition latency from `pending` →
       `{succeeded, failed}` becomes the new SLO; ntex client
       deadline no longer participates.
  2. **Land C-7-LT as the next sprint**, before T-8b-stress
     (`c=20`). Stress at `c=20` with the synchronous contract
     **will** re-discover C-8 (or its sibling: source-teardown
     wall-time variance across 20 concurrent wakes) under
     production fence config — the cluster-startup-script knob
     (`HOST_FENCE_TIMEOUT_SECS=30`) is a smoke-only patch, not a
     stress-time fix.
  3. **Document the wake contract** in
     `docs/reference/wake-contract.md` (or extend `zs-standard
     .md`): "wake is best-effort fire-and-forget at the HTTP
     layer; status polling is required for completion semantics."
     This is a contract change visible to clients, so it
     deserves an explicit reference doc, not just a code
     comment.
  4. **Add a tech-debt entry**: the C-8 fix in `2afbb2dd` (cluster
     config drop to 30 s) should be reverted simultaneously with
     C-7-LT landing. Track this explicitly so we don't carry
     "smoke runs with degraded fence" as a permanent platform
     property.

  Estimated cost: ~500 LOC across `admin_handlers.rs`,
  `restore_handler.rs`, `db.rs` (`wake_ops` columns), and the
  stress client (`/opt/stress/snapshot_stress.py`). ~3 PRs in
  the C-7-LT proposal. **High ROI**: closes 5 cluster bugs at
  once, returns the cluster to production-default `host_fence
  _timeout_secs=120`, and unblocks T-8b-stress without
  configuration knobs.

  **Tech-debt entry (architectural read of the C-8 fix in
  `2afbb2dd`)**: the double-ceiling pattern in
  `from_host_fence_timeout` (fence-derived AND deadline-capped,
  MIN-of-two) is **correct as defense-in-depth**, but it's a
  **code patch around a fundamental design tension** —
  synchronous wake response can't span > 60 s when source
  teardown takes ~150 s. The C-8a cap pins observable failure to
  503 (good) but **doesn't restore success on slow teardowns**
  (the wake just 503s cleanly instead of silently). C-7-LT is
  the structural fix. Mark C-8 + C-8a as "closed operationally;
  superseded by C-7-LT once landed" in the deferred file.

### [R15-A2] Phase tracing remains restore-handler-only after 1 cycle of carry — `snapshot_handler.rs` / `admin_handlers.rs` / `nomad_ch::stop_inner` are still zero-instrumented (IMPORTANT, architecture-r15)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs`: **21** `phase = "..."`
    lines (unchanged from r14, which is the surprise — C-7 added
    a per-attempt INFO log inside `reserve_vm_index_with_retry`
    but did not add new phase-tracing).
  - `crates/sandbox/src/snapshot_handler.rs`: **0**.
  - `crates/sandbox/src/admin_handlers.rs`: **0**.
  - `crates/sandbox/src/backend/nomad_ch.rs`: **0** (no `phase =
    "..."` anywhere in the 5399-LOC backend including the 4-step
    `stop_inner` teardown).
- **Symptom**: r14-A4 recommended extending phase tracing to
  `stop_inner`, `snapshot_handler`, and `admin_handlers::
  snapshot_sandbox` precisely so the next cluster cycle's bug
  would localize in 1 cycle instead of 2. Between r14 (`d673e043`)
  and r15 (`2afbb2dd`), **6 commits landed on the sandbox crate**
  (`91ce9be5`, `9afd0986`, `79b4d258`, `493d6c1e`, `c3edf968`,
  `2afbb2dd`) and **zero new phase lines** were added outside
  `restore_handler`.

  Concretely, the C-8 root-cause investigation (`teardown source
  holds vm_index ~150 s`) required reading the controller log to
  match `stop: started vm_index=1` (3:57:06.840) against `vm_index
  released vm_index=1` (3:59:30+) — a 150 s window with **no
  intermediate observability** of which sub-step of `stop_inner`
  (state_remove, shutdown, nomad_stop, wait_gone, fence,
  release) was burning time. The cluster review had to read
  Nomad's audit log to localize the time spent in `host_fence`
  vs purge. That's R14-A4's predicted failure mode, materialised.
- **Why important**: r14's R14-A4 was rated IMPORTANT with a
  **specific predictive claim**: "the next cluster cycle's bug
  WILL be in code that R14-A4 covers". r9 confirmed this — C-8's
  diagnosis was bottlenecked on `stop_inner` having no phase
  tracing. The discipline lever is **proven to have positive ROI
  in retrospect**; the only reason it hasn't shipped is that no
  PR has been written for it.

  Looking forward: r10 cluster smoke will, with high probability,
  surface either (a) a `snapshot_handler` bug (the snapshot path
  still has 0 phase lines despite holding the L1+L2 storage tier
  + the `ch pause` + `ch snapshot` + cap-the-snapshot sequence)
  or (b) a `stop_inner` race exposed by the host_fence=30 s
  cluster cfg shortening teardown enough to expose ordering
  bugs that the 120 s default masked. Both are diagnosable in
  ~1 cluster cycle if instrumented, ~2-3 if not.
- **Action**: same as R14-A4 — extract a `restore_phase` module
  per R14-A2.1, then instrument the three sibling handlers.
  Specifically:
  1. **PR A** — extract phase enum + emit helper from
     `restore_handler.rs`. ~100 LOC of new module, ~−180 LOC in
     handler (inline `tracing::info!` blocks replaced with
     `RestorePhase::PostReserveVmIndex.emit(...)`).
  2. **PR B** — apply phase tracing to `nomad_ch::stop_inner`.
     **Highest leverage** of the three since it's the most
     likely next-cluster-cycle wedge site (CreateGuard cleanup
     + the 4-step teardown both live here).
  3. **PR C** — apply to `snapshot_handler::snapshot_sandbox`
     and `admin_handlers::snapshot_sandbox` (the entry).
  4. **PR D** — phase trace coverage gate in CI: a grep that
     fails if any handler file `fn`-with-`async` boundary lacks
     a `phase = "..."` marker on at least every other await.
     Treats observability as compile-time enforced, not best-
     effort.

  Estimated cost: ~120 LOC across 3 files for PRs B+C; ~100/−180
  LOC for PR A. Saves ≥1 cluster cycle ($0.40-0.50 + ~2 hours
  human time) per future C-N bug on those paths.

### [R15-A3] `restore_handler.rs` is now the **2nd-largest** file in the crate at 3444 LOC; +343 LOC in 1 cycle is the second consecutive 10%+ growth cycle; the file is now structurally unsplittable without prerequisite test scaffolding (IMPORTANT, architecture-r15)

- **File**: `crates/sandbox/src/restore_handler.rs` (3444 LOC at
  `2afbb2dd`, was 3101 at r14, was 2662 at r13).
- **Growth attribution since r14** (+343 LOC):
  - `493d6c1e` C-7 fix (+80 LOC): `VmIndexRetryPolicy::default`
    reduced from 60×2s→25×2s; per-attempt INFO log inside
    `reserve_vm_index_with_retry`; doc updates at policy struct,
    trait method, and call site; superseded `c4_default_policy_
    envelopes_observed_teardown` with `c7_retry_budget_default_
    is_under_client_deadline`.
  - `c3edf968` R14-A6 (+234 LOC): `VmIndexRetryPolicy::from_host_
    fence_timeout` derivation, `RealRestoreBackend::vm_index_
    retry_policy` override, doc block, 4 new tests
    (`r14a6_policy_from_cfg_*`, `r14a6_real_backend_derives_*`).
  - `2afbb2dd` C-8a fix (+97 LOC, after offsetting the R14-A6
    prose deletions): the dual-ceiling cap with rewritten doc
    block and examples table, plus 1 new test
    (`r14a6_from_cfg_caps_at_client_deadline`).
  - Net: 3 commits, +343 LOC, **all on the same retry-policy
    concern**.
- **Why important**:
  - **Ranking change**: `restore_handler.rs` (3444) is now larger
    than `db.rs` (3303), making it the **2nd-largest file in
    the crate** behind `nomad_ch.rs` (5399). r14 noted "crossed
    3000 LOC for the first time"; r15 notes "passed `db.rs`".
  - **Growth pattern**: r13 → r14 +439 LOC (16%); r14 → r15
    +343 LOC (11%). **Two consecutive cycles > 10% growth on
    the same file**, both driven by cluster fixes. At this
    rate, r16 = ~3800 LOC; r17 = ~4200 LOC.
  - **Unsplittability**: R14-A2 recommended splitting into
    `restore_phase` + `restore_handler::types` modules. r15
    notes the split has a hidden cost — the 4 new R14-A6
    tests + the new C-8a test all live in the same
    `unit_tests` module at the bottom of the file (the file's
    bottom 200 LOC is now ~30% of the unit tests for the
    whole module). Splitting out the types module would mean
    splitting `unit_tests` first, which means writing the
    R13-A1 driven integration tests **first** (so the
    unit-test split doesn't lose coverage).

    R14-A2's "land after R13-A1" sequencing was correct; r15
    confirms the dependency.
- **Action**:
  1. **Re-prioritise R13-A1** (StubRestoreBackend-driven
     integration tests) as the *prerequisite for module split*,
     not just a test-coverage finding. Without R13-A1, R14-A2
     can't land safely. **R13-A1 has been open EMERGENCY for 7
     rounds.**
  2. **Extract retry-policy concerns into a sibling module**
     `restore_handler::retry_policy` (~300 LOC at HEAD —
     `VmIndexRetryPolicy` struct + its 3 constructors +
     `reserve_vm_index_with_retry` + 5 of the 7 retry unit
     tests). This is the **load-bearing growth area** — every
     bug since C-4 has added LOC here. Pulling it out caps the
     blast radius of future iterations on the wake-retry
     budget; the C-7-LT proposal (R15-A1) would also extend
     this module rather than `restore_handler.rs` itself.
     **Net ~−300 LOC on `restore_handler.rs`, file drops to
     ~3144 LOC** — back below `db.rs` but still over the 3000
     threshold.

     Land as PR independently from R13-A1 because the retry-
     policy module's tests are not coupled to the handler's
     integration tests; they test a value-object's arithmetic.
  3. **Sequencing**: R13-A1 (driven tests) → retry-policy
     module split (this finding) → R14-A2 (phase + types
     split). The retry-policy module is the *one* extraction
     that doesn't depend on R13-A1; landing it first lets the
     file shrink while R13-A1 stabilises.

### [R15-A4] 9-cycle cluster bug distribution clusters on the runtime+async+timing axis (6 of 9 bugs) — integration tests against StubRestoreBackend would catch 4 of the 8 code-side bugs; R13-A1 is now provably blocking (IMPORTANT, architecture-r15)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:1119-1300+` —
    `StubRestoreBackend` (5 fields, all retry-policy- and
    error-injection-oriented; **zero tests drive it through
    `restore_sandbox`**).
  - r14 R14-T1 / r13 R13-A1 — open EMERGENCY 7 rounds.
- **Bug distribution analysis** (the 9 Phase B cluster bugs):

  | Bug | Axis | Could StubRestoreBackend-driven test catch it? |
  |---|---|---|
  | C-1 (driver `--config` argv) | env/argv plumbing | No — driver-side, not in restore_handler. |
  | C-2 (driver disk-path resolution) | env/argv plumbing | No — driver-side. |
  | C-3 (`Tiered::put` spawn_blocking-in-spawn_blocking panic) | runtime/async | Partial — `snapshot_store_gcs.rs` is not behind the trait; would need a separate `SnapshotStore` stub. |
  | **C-4** (vm_index race) | timing/budget | **YES** — stub `reserve_vm_index` returns `Err` for N attempts then `Ok`; integration test asserts wake returns 200 after N retries. |
  | C-5 (GCS scope 403) | operator config | No — operator-side. |
  | **C-6** (silent stall — turned out to be C-7's diagnostic gap) | runtime/async | **YES** — same stub harness with a phase-tracing assertion would have shown "loop entered, no exit log" before cluster diagnostics. |
  | **C-7** (client-cancel mid-sleep) | timing/budget | **YES** — `tokio::test`-style harness with a 60s deadline + stub returning `Err` indefinitely would have shown the future being dropped without the exhaust log firing. |
  | C-8 (teardown wall-time > budget) | timing/budget | **Partial** — stub can't simulate real Nomad teardown; the budget-vs-teardown gap is environmental. |
  | **C-8a** (R14-A6 latent regression) | timing/budget | **YES** — `r14a6_from_cfg_caps_at_client_deadline` is the exact test shape; it landed AFTER C-8a was already deployed in `c3edf968`. The test was retroactive, not preventive. |

  **Code-side bugs (8 of 9 — excluding C-1/C-2 driver-side which
  are out of scope for `restore_handler` tests)**:
  - 4 catchable (C-4, C-6, C-7, C-8a)
  - 2 partial (C-3 needs `SnapshotStore` stub; C-8 needs Nomad
    simulation)
  - 2 not catchable here (C-5 GCS scope; both drivers)

  **6 of 9 bugs cluster on runtime+async+timing**; **4 of 9**
  could have been caught by **one integration test harness** —
  StubRestoreBackend driving `restore_sandbox` with phase-
  trajectory assertions. That harness has been **proposed but
  unimplemented for 7 EMERGENCY rounds**.
- **Why important**: this is a numerical, not aesthetic,
  argument. ~$3.80 (per smoke-r9 review) of cluster spend
  recovered ~50% of the bugs that a ~250-LOC test harness would
  have caught at zero cluster cost. The cluster smoke series'
  value-per-cycle drops as the cheap bugs get harder to find;
  conversely, the integration-test harness' value-per-LOC stays
  flat because each bug it catches is one cluster cycle saved.

  Crossover: at ~$0.40-0.50/cycle and ~250 LOC for the harness,
  the harness pays for itself if it catches **≥1 future
  cluster bug**. Given that the runtime+async+timing axis has
  produced 6 of the last 9 bugs and shows no signs of
  exhaustion (C-7-LT, R14-A1, R4-A2 all open), the harness is
  +EV with high confidence. The opportunity cost of *not*
  landing R13-A1 before each future Phase B cycle is roughly
  one cluster cycle per cycle.
- **Action**:
  1. **Lift R13-A1 to a hard prerequisite for T-8b-stress.** Do
     not run cluster c=20 until the StubRestoreBackend-driven
     test harness is committed. The stress run will surface
     1-3 new bugs on the same axis; localizing them with the
     harness in place costs LOC, without it costs cluster
     cycles.
  2. **Test taxonomy** to encode in the harness:
     - Happy path: stub returns Ok on first attempt → wake
       returns 200.
     - Retry-then-succeed: stub returns Err on N-1 attempts then
       Ok → wake returns 200 after `N × interval` wall-time.
       Pins C-4.
     - Retry exhausted: stub returns Err indefinitely → wake
       returns 503 with `VmIndexUnavailable` AND the exhausted-
       budget WARN log fires (assertion on captured tracing
       events). Pins C-7.
     - Client deadline race: drive the test with a 60 s
       deadline + retry budget > 60 s → assert the exhausted-
       budget log fires BEFORE the future drops. Pins C-8a.
     - Phase trajectory assertion (post-R14-A2 phase module):
       wake reaches `phase=post_clock_resync` on happy path;
       wake stops at `phase=pre_reserve_vm_index` on retry-
       exhausted path. Pins C-6 diagnostic gap.
     - Backend config invariant: for every `host_fence_timeout
       _secs` in `{0, 30, 60, 120}`, the retry budget computed
       by `RealRestoreBackend::vm_index_retry_policy()` satisfies
       `(max_attempts - 1) × interval ≤ CLIENT_DEADLINE -
       HEADROOM`. Pins the invariant R14-A6 *should have had*
       (the C-8a invariant retroactively pinned at `2afbb2dd`).
  3. **CI gate**: this harness must pass on every PR touching
     `restore_handler.rs` or `RealRestoreBackend::vm_index_
     retry_policy`. The next contributor to bump `CLIENT_
     DEADLINE_SECS` or change the policy formula gets a CI
     failure, not a cluster cycle.

  Estimated cost: ~250 LOC. ROI: probably the single highest
  per-LOC return on investment in this crate, by the back-of-
  envelope above.

### [R15-A5] R14-A6 + C-8a together establish the missing **architectural invariant** "retry budget ≤ client deadline − headroom" — but it's encoded only as a doc comment + 1 test, not enforced at the type system (MINOR, architecture-r15)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:240-267` (the dual-
    ceiling formula).
  - `crates/sandbox/src/restore_handler.rs:1559-1591` (the new
    `r14a6_from_cfg_caps_at_client_deadline` test).
- **Symptom**: `CLIENT_DEADLINE_SECS = 60` and `CLIENT_HEADROOM
  _SECS = 10` are both `const u64` inside a function body.
  `from_host_fence_timeout` is the only constructor that respects
  the cap. `VmIndexRetryPolicy::default()` returns 25×2s=48s,
  matching the cap **by hand-tuning**, not by construction. A
  future contributor adding `VmIndexRetryPolicy::from_X(x)` who
  forgets the deadline cap re-introduces C-7 / C-8a silently.

  The same invariant violation happened **inside one cycle**:
  R14-A6 landed at `c3edf968` without the deadline cap; C-8a
  fix at `2afbb2dd` added it 22 commits later. Both reviewers
  (architecture-r14 and code-quality-r14, per R14-Q4 closure)
  missed the deadline ceiling. The pattern "every new policy
  constructor must apply the deadline cap" is currently
  enforced only by code review + 1 test, not by the type
  system.
- **Why minor**: only 2 constructors exist today
  (`default` + `from_host_fence_timeout`). The pattern is
  contained. But the C-7-LT design (R15-A1) **eliminates the
  client deadline as a ceiling**, which means C-7-LT's PR will
  inevitably touch this code and the invariant changes from
  "budget ≤ deadline" to "no client deadline". Encoding the
  invariant in code makes the transition mechanical; leaving
  it as scattered constants makes it error-prone.
- **Action**: lift the constants and the cap into the type:

  ```rust
  pub struct VmIndexRetryPolicy {
      pub max_attempts: u32,
      pub interval: Duration,
  }

  impl VmIndexRetryPolicy {
      /// The hard ceiling on retry wall-time. Tied to the ntex
      /// client deadline minus a headroom; superseded once
      /// C-7-LT (async response + polling) lands.
      pub const CLIENT_DEADLINE_HARD_CEILING: Duration =
          Duration::from_secs(50);

      /// Construct a policy and verify it fits under the hard
      /// ceiling. Returns Err if `(max_attempts - 1) * interval
      /// > CLIENT_DEADLINE_HARD_CEILING`.
      pub fn try_new(max_attempts: u32, interval: Duration)
          -> Result<Self, &'static str>
      {
          let wall_time = interval.saturating_mul(
              max_attempts.saturating_sub(1));
          if wall_time > Self::CLIENT_DEADLINE_HARD_CEILING {
              return Err("retry budget exceeds client deadline; see C-7 / C-8a");
          }
          Ok(Self { max_attempts, interval })
      }
  }
  ```

  Use `try_new` in `default()` (via `expect(...)`) and
  `from_host_fence_timeout` (after the cap). The cap moves
  from a hand-applied MIN to a guarded invariant; new
  constructors are forced to handle the `Err` branch at
  compile time, not at code review.

  Cost: ~30 LOC. Closes the latent footgun R14-A6 fell into.
  **Defer until R15-A1 (C-7-LT) ships** — if C-7-LT lifts the
  hard ceiling entirely, this finding becomes moot. If C-7-LT
  is deferred more than one cycle, land R15-A5 as a
  defense-in-depth.

## End-state architecture (post-T-8b-smoke-r10 PASS, if it passes)

Per the brief's prompt 6: what's the delta from the pre-T-8b
baseline if smoke-r10 (the next cluster cycle, presumably with
the C-8 cluster-config patch in place) passes?

**Pre-T-8 baseline** (before this branch's Phase B series):
- Wake handler returned a synchronous response.
- Source teardown was best-effort detached on the ntex worker
  runtime via `compio::runtime::spawn(...).detach()`.
- Snapshot store was 2-tier (L1 local, L2 GCS) but `Tiered::
  put` spawn_blocking-in-spawn_blocking was untested under the
  cluster's call graph.
- No phase tracing.
- No vm_index retry budget (wake returned 503 immediately if
  the source slot was held).
- `cfg.host_fence_timeout_secs = 30` (the default before
  `cad098e6` bumped to 120).
- No StubRestoreBackend-driven integration tests.

**Post-r10 PASS architecture** (deltas):
- **Wake handler still returns synchronous response** — C-7-LT
  is the structural fix but is **not landed**. The wake's
  synchronous contract is preserved by **constraining the
  source teardown to be faster than the client deadline**,
  which the C-8 cluster-config patch enforces by lowering
  `host_fence_timeout_secs` to 30 s at the **worker startup
  script level** (not the production default).
- **Source teardown runs on an isolated runtime** (R14-A1 fix
  via C-6 at `91ce9be5`): dedicated OS thread + private compio
  runtime. CreateGuard::drop still uses the old bad pattern
  — **R14-A1 helper not yet extracted**.
- **Snapshot store L2 detach runs on a dedicated OS thread**
  (C-3 fix at `c890c015`): `std::thread::Builder::new().spawn`
  with sync ureq inside. **R13-A2 layering inversion not
  fixed** (the impl spawns the thread; should be the handler).
- **21 phase-tracing lines** in `restore_handler.rs` only.
  R14-A4 / R15-A2 extension to `stop_inner`, `snapshot_handler`,
  `admin_handlers::snapshot_sandbox` **not landed**.
- **vm_index retry budget = 50 s** wall-time (25 attempts × 2 s
  with dual ceiling: MIN of fence-derived and deadline-derived).
  Per-attempt INFO log inside the loop.
  `VmIndexRetryPolicy::from_host_fence_timeout` derives from
  `cfg.host_fence_timeout_secs` with the dual-ceiling cap.
- **`cfg.host_fence_timeout_secs = 120` in code default**, but
  cluster startup overrides to **30 s**. Production deployers
  using the code default still face the 503-on-slow-teardown
  failure mode (clean error now, not silent stall). The
  controller-side correctness is preserved across all fence
  values; the smoke-pass-fail is operator-config-conditional.
- **StubRestoreBackend** has 5 retry-policy fields; **0**
  tests drive it through `restore_sandbox`. R13-A1 / R15-T1
  open EMERGENCY 7 rounds.
- **C-8 + C-8a + R14-A6 retroactive test** added: 5 retry-
  policy tests pin `(max_attempts - 1) * interval ≤ 50 s`
  under various input fences. `VmIndexRetryPolicy::default`
  pinned at 25×2s=48s.

**Net delta** vs. pre-T-8 baseline:
- ✓ wake retries on vm_index unavailable (was: immediate 503)
- ✓ source teardown isolated from worker runtime (was: same runtime)
- ✓ L2 detach isolated from worker runtime (was: same runtime)
- ✓ phase tracing on the restore path (was: none)
- ✗ wake still synchronous (the root cause of C-4/C-6/C-7/C-8/C-8a)
- ✗ CreateGuard::drop still uses bad pattern
- ✗ Tiered::put still layered wrong (impl spawns, not handler)
- ✗ stop_inner / snapshot_handler / admin_handlers still untraced
- ✗ R13-A1 integration tests still missing
- ✗ cluster-cfg patch reduces production fence resilience for
  smoke pass

**Architecturally**: r10 PASS would close C-8 operationally
without closing it structurally. The branch ships a working
snapshot/restore in the smoke regime (`host_fence=30, c=1`)
but **does not ship a snapshot/restore that survives c=20 at
the production fence default**. The C-7-LT proposal (R15-A1)
is the gate to the latter; without it, T-8b-stress is forced
to either ship at degraded fence (cluster-cfg patch covers it
but production diverges from smoke) or fail at the cluster
level.

## Cluster bug pattern analysis (review brief's prompt 5)

The 9 Phase B bugs by axis:

| Axis | Count | Bugs | Test surface that would catch them |
|---|---:|---|---|
| Runtime / async scheduling | 3 | C-3, C-6, C-8a (latent) | Integration tests on the live runtime + invariant checks at the type system |
| Env / argv plumbing | 2 | C-1, C-2 | Driver-side smoke; out of scope for this crate |
| Timing / budget / SLO | 4 | C-4, C-5, C-7, C-8 | StubRestoreBackend-driven tests with deadline simulation |

**6 of 9 bugs cluster on the runtime+async+timing axis**, all in
this crate. **4 of 8 code-side bugs** (excluding C-1/C-2 driver-
side) could be caught by **one R13-A1-class harness**. The
**marginal cost** of *not* landing R13-A1 was ~$3.80 over 9
cluster cycles for ~50% of the bugs the harness would have
flagged. At a back-of-envelope average bug discovery cost of
$0.40-0.50/cycle and ~250 LOC for R13-A1, **break-even is 1-2
future bugs**. With the runtime+async+timing axis showing no
sign of exhaustion (R15-A1's C-7-LT proposal alone implicates
3+ additional refactor sites), R13-A1 is **+EV with extremely
high confidence**.

The implication for test coverage priorities:
1. **Highest priority**: integration tests on the wake/restore
   path with parameterised stub backends. Catches the timing/
   budget axis directly and the runtime/async axis with the
   help of phase-trajectory assertions.
2. **Next priority**: phase-tracing as compile-time-enforceable
   discipline (R15-A2). Localizes runtime/async bugs in 1
   cluster cycle instead of 2-3.
3. **Lower priority**: invariant-encoding at the type system
   (R15-A5). Defense-in-depth on the timing axis; superseded
   by C-7-LT (R15-A1) if it lands.
4. **Out of priority (this crate)**: driver-side argv / env
   plumbing (C-1, C-2). Lives in the driver worktree.

## C-8 fix architectural read (review brief's prompt 2)

The dual-ceiling pattern in `from_host_fence_timeout` at
`2afbb2dd` is **architecturally correct** as a defense-in-depth
patch:

```rust
let max_budget_from_fence = host_fence_timeout_secs
    .saturating_sub(CLIENT_HEADROOM_SECS);
let max_budget_from_deadline = CLIENT_DEADLINE_SECS
    .saturating_sub(CLIENT_HEADROOM_SECS);
let effective_budget = max_budget_from_fence
    .min(max_budget_from_deadline);
```

The MIN-of-two pattern guards against operator-driven knob
mismatches (raising the fence past the client deadline). The
doc block (rewritten in `2afbb2dd`) explicitly enumerates the
three cases (30s, 60s, 120s fence). The test
`r14a6_from_cfg_caps_at_client_deadline` pins the cap at 26
attempts under a 120s fence.

**But it's a CODE PATCH around a fundamental design tension**:
synchronous wake response can't span > 60 s when source-
teardown can take ~150 s. The proper fix is **async
response** — once the client deadline is no longer the
ceiling, the budget can grow to envelope any fence
configuration. The dual ceiling exists only because the
synchronous-response contract makes the client deadline a
hard upper bound on retry wall-time.

**Tech-debt entry**: mark this in
`docs/reviews/sandbox-snapshot-restore-deferred.md`:

> **C-8 / C-8a are defense-in-depth patches**, not structural
> fixes. They preserve observable failure (clean 503) while
> the source-teardown wall-time exceeds the synchronous-wake
> client deadline. The structural fix is **C-7-LT (`202
> Accepted` + status URL polling)** — see R15-A1. Until
> C-7-LT lands, the wake handler's correctness budget is
> `min(source_teardown_wall_time, 60s)`, which the cluster-
> config patch enforces by capping the fence to 30 s at
> worker startup. **Production deployments using the
> code-default 120 s fence retain the silent failure mode
> for wakes racing slow teardowns.**

## Audit: phase tracing coverage (review brief's prompt 4)

R14-A4 recommended extending phase tracing to `stop_inner`,
`snapshot_handler`, and `admin_handlers::snapshot_sandbox`.
**Status at r15**: zero landed.

| Handler | r14 phase lines | r15 phase lines | Δ |
|---|---:|---:|---:|
| `restore_handler.rs` (restore path) | 21 | 21 | 0 |
| `snapshot_handler.rs` (snapshot path) | 0 | 0 | 0 |
| `admin_handlers.rs` (entry points) | 0 | 0 | 0 |
| `backend/nomad_ch.rs::stop_inner` (teardown) | 0 | 0 | 0 |

C-7's fix did add a **per-attempt INFO log** inside
`reserve_vm_index_with_retry` (`restore_handler.rs:413-420`)
that emits `attempt`, `max_attempts`, `vm_index` on each
reserve iteration. That's *useful* but is **not** phase
tracing — it's inside an existing instrumented handler. The
R14-A4 finding is **fully open**.

## Spot-check: C-8 + C-8a code at `2afbb2dd`

| Concern | Verdict |
|---|---|
| Dual-ceiling MIN formula | **CORRECT** — `from_host_fence_timeout(120)` produces 26 attempts × 2s = 50s wall-time, matching the test contract. |
| Test coverage of the cap | **PARTIAL** — only the 120s case is pinned by `r14a6_from_cfg_caps_at_client_deadline`. The 60s and 30s cases are not pinned post-cap (the existing r14a6 tests asserted ≤ 50 s, which is satisfied vacuously for 30s and 60s). Adding pinning tests for {30, 60, 120} as a triple would be ~10 LOC. |
| Doc block rewrite | **GOOD** — the doc block at `restore_handler.rs:200-239` explicitly enumerates the three cases (30/60/120) with the new examples, and the `CLIENT_DEADLINE_SECS = 60` constant is named and documented. |
| The 30s cluster patch | **OPERATIONAL** — at `gcp-worker-startup.sh:466` the env var is set in the systemd unit. The commit message documents that production deployments needing the 120s default can override via metadata. The trade-off note is clear. |
| C-8a regression test | **PINNED** — `r14a6_from_cfg_caps_at_client_deadline` asserts `(max_attempts - 1) × interval ≤ 50_000ms` for the 120s fence input, with explicit `(60 - 10) / 2 + 1 = 26` arithmetic. |

The code is fine. The architectural concern is the **5th-patch-
on-the-same-contract pattern** (R15-A1), not the patch itself.

## Carry-forward (still open from earlier rounds)

- **[R4-A2 / R5-A2]** LeasedVmSlot RAII guard — **14th cycle**.
  Subsumed by R15-A1's C-7-LT proposal: an async-response wake
  doesn't poll for slot release, so the RAII pattern becomes a
  pure cleanup convenience, not a correctness primitive.
  Re-evaluate after C-7-LT.
- **[R3-A1 / R5-A1 / R10-A3]** `Backend` enum 5-Err-returner split
  — count at HEAD = **5** (unchanged). CRITICAL still.
- **[R3-A2 / R10-A2 / R13-A5]** `RestoreBackend` trait facade —
  **9 methods** at HEAD. 3 of 9 are one-line delegations.
- **[R3-A3]** wrapper bash → Rust sidecar — subsumed by R11-A2 /
  R12-A4.
- **[R3-A4]** `StopDisposition` enum — still `stop_inner(.., bool)`.
- **[R4-A1 / R10-A6 / R11-A3]** AppState builder accretion —
  **12th cycle**. r15 incidentally identified **no NEW `with_*`
  builders** added this cycle; trend flat.
- **[R10-A4 / R11-A2 / R12-A4]** `nomad_ch.rs` at 5399 LOC, un-
  split for **4 cycles**. Brief mentions "5371+ LOC" — actual is
  5399; this is a measurement drift in the brief that we should
  pin.
- **[R10-A1 / R11-A4 / R12-A5]** `db.rs` at 3303 LOC carrying
  the recovery CAS. Un-extracted, **unchanged for 5 cycles**.
- **[R11-A1 / R12-A2]** root-owned-secret-file 5-site
  duplication. No movement.
- **[T9 / T10 / R10-A7]** ControllerIdleSnapshotter duplicates
  admin_handlers' 70-LOC orchestration — unchanged.
- **[r9 C3]** AEAD fail-OPEN on GCS path.
- **[R13-A1 / R14-T1 / R15-T1]** StubRestoreBackend never drives
  `restore_sandbox` — **EMERGENCY 7 rounds open**. R15-A4 (this
  round) makes the cost-of-not-landing explicit.
- **[R13-A2]** L2 detach pull-up to handler — unchanged. R14-A3
  re-framed it as needing R14-A1's `detach_isolated` helper
  first.
- **[R14-A1]** `detach_isolated` helper — un-extracted.
  CreateGuard::drop at `nomad_ch.rs:2002` **still uses the bad
  pattern**.
- **[R14-A2]** `restore_handler.rs` split (phase + types
  modules) — un-landed. R15-A3 re-prioritises the retry-policy
  module as an independent split.
- **[R14-A4 / R15-A2]** Phase tracing extension — un-landed
  for 1 full cycle.
- **[R14-A5]** 5+ `std::fs::*` un-spawn_blocking sites on the
  async path — unchanged.

## Closed by recent commits (since r14)

- **C-7** (synchronous-cancel mid-sleep) — CLOSED at `493d6c1e`.
  Wake retry budget shrunk 120s → 48s; per-attempt INFO log
  added. **r15 note**: C-7 fix is *operationally* correct but
  *structurally* the C-7-LT design (R15-A1) is the proper
  closure. C-7 the bug is closed; C-7-LT the structural
  refactor remains open.
- **C-8** (teardown > budget) — CLOSED at `2afbb2dd` (cluster-
  config drop to 30 s + dual-ceiling cap in code).
- **C-8a** (R14-A6 latent regression) — CLOSED at `2afbb2dd`
  (dual-ceiling cap; test pinned).
- **R14-A6 (architecture-r14)** — CLOSED at `c3edf968` (cfg-
  derived policy + 4 tests). **r15 note**: closure was
  incomplete; R14-A6 lacked the deadline cap, surfacing as
  C-8a within 1 cycle. The c3edf968→2afbb2dd interval is the
  correctness window the R14-A6 reviewer (architecture-r14)
  missed; r15 records the closure as "valid for the brief, but
  superseded by C-8a one commit later".
- **R14-Q2** (`seal_filename_for_str` cfg gating) — CLOSED at
  `79b4d258`.
- **R14-Q3 / R14-P2** (`snap-l2-upload` thread-name simplify) —
  CLOSED at `9afd0986`.
- **C-6** (wake silent stall) — formally CLOSED at `91ce9be5`
  for the brief; r14 already noted in-flight. r15 records that
  the *r8 falsification* makes C-6 a defense-in-depth fix,
  not the proximate cause closure. C-6 is closed; C-7 closed
  the actual symptom.

## What's structurally new vs. r14

| Item | r14 state | r15 state | Δ |
|---|---|---|---|
| `restore_handler.rs` LOC | 3101 | **3444** | **+343** (+80 C-7, +234 R14-A6, +97 net C-8a) |
| Cluster bugs surfaced cumulative (Phase B) | 7 (C-1..C-7) | **9** (+C-8, +C-8a) | +2 (both timing/budget axis) |
| Cluster cycles complete (Phase B) | 7 (r1-r7) | **9** (r1-r9) | +2 cycles |
| Phase-tracing lines in `restore_handler.rs` | 21 | 21 | 0 |
| Phase-tracing-instrumented files | 1 | 1 | **0 (R14-A4 carry; 1 cycle open)** |
| `compio::runtime::spawn(...).detach()` per-request sites still risky | 1 (CreateGuard::drop) | 1 (CreateGuard::drop) | 0 |
| `RestoreBackend` trait methods | 9 | 9 | 0 |
| `RealRestoreBackend::vm_index_retry_policy` derivation | hard-coded 60×2s | cfg-derived w/ dual ceiling | structural |
| Operator-tunable knob mismatches caught at compile time | 0 | 0 | R15-A5 |
| Files where `restore_handler.rs` ranks by LOC | 4th | **2nd** | +2 ranks |
| Cluster bugs whose proximate cause was the synchronous-wake contract | 4 (C-4, C-6, C-7) | **5** (+C-8, +C-8a) | +1 architectural pattern bug per cycle |

The cycle's net structural movement is:
- **+1 cluster cycle confirming the synchronous-wake
  contract is the load-bearing tech debt** (R15-A1).
- **+1 file rank shift** (`restore_handler.rs` from 4th to 2nd
  largest in the crate).
- **+0 phase-tracing migrations** despite R14-A4's IMPORTANT
  rating last cycle (R15-A2 escalates).
- **+1 cluster bug pattern axis identified** (R15-A4: 6/9 on
  runtime+async+timing).
- **+0 r14 flagship architecture findings closed** (R14-A1
  detach_isolated, R14-A2 module split, R14-A4 phase tracing,
  R13-A1 driven tests, R10-A4 nomad_ch split, R10-A1/A3
  RestoreBackend surface — all open).
- **+1 r14 flagship discovered to have been incomplete**
  (R14-A6 lacked the deadline cap; C-8a was the cost).

## Recommended order of attack (updated for r15; 9 PRs)

Updated from r14's 11-PR plan with R15-A1 inserted as the new
flagship, R13-A1 promoted to a hard prerequisite, and the
retry-policy module split (R15-A3) as a low-dependency early
win:

1. **R15-A1 — C-7-LT proposal**: write
   `docs/proposals/sandbox-wake-async-response.md`. Land
   **proposal first** (read by team, ~1 round of review), then
   implementation in 3 PRs (handler split, status endpoint,
   stress client switch). ~500 LOC implementation. **This is
   the structural closure of C-4/C-6/C-7/C-8/C-8a.**
2. **R13-A1 — StubRestoreBackend-driven test harness** (~250
   LOC). **Must land before T-8b-stress** per R15-A4's cost-
   benefit math. EMERGENCY 8 rounds.
3. **R15-A3 — retry-policy module split** (~300 LOC moved out
   of `restore_handler.rs`, file drops to ~3144 LOC). **Low-
   dependency early win**; doesn't need R13-A1 because the
   moved tests are value-object arithmetic.
4. **R14-A1 — `detach_isolated` helper + migrate CreateGuard::
   drop** (~80 LOC helper, −15 LOC at call site). Closes the
   final per-request bad-detach site.
5. **R15-A2 (R14-A4) — phase tracing extension to `stop_inner`,
   `snapshot_handler`, `admin_handlers::snapshot_sandbox`**
   (~120 LOC). Land **after** R13-A1 so the phase-trajectory
   assertions in the test harness can extend to the new
   handlers.
6. **R14-A2 — `restore_handler.rs` phase + types module split**
   (~−80 LOC net; file drops to ~3060 LOC if combined with
   R15-A3 at step 3). Land after R13-A1.
7. **R13-A2 + R14-A3 — L2 detach pull-up via `detach_isolated`**
   (~+15 LOC handler / ~−40 LOC store).
8. **R15-A5 — encode the deadline invariant in `VmIndexRetry
   Policy::try_new`** (~30 LOC). **Skip if R15-A1 lands** —
   C-7-LT removes the client deadline as a ceiling.
9. **Existing carry-forwards** (R11-A1 secret_io, R10-A1
   recovery.rs, R10-A4 nomad_ch split, R12-A1 jobspec collapse,
   etc.) per r13/r14 ordering.

**Total: 9 PRs**, of which **PR 1 (R15-A1 / C-7-LT)** is the
new flagship. PR 2 (R13-A1) is unchanged and EMERGENCY. PR 3
(R15-A3 retry-policy split) is the new low-dependency early
win.

**The critical insertion is PR #1 (R15-A1)** — it's the only
move that closes the 5-bugs-from-one-contract pattern. Without
it, every future cluster cycle on the wake path is paying
~$0.40-0.50 to discover one more layer of the same design
tension. PR #2 (R13-A1) is the diagnostic-debt closure that
makes future C-N bugs catchable at unit-test time.

**In cycle order**:
- **First**: Write R15-A1's C-7-LT proposal (no code; 1 round
  of design review).
- **Then**: PR 2 (R13-A1 driven tests). Mandatory before T-8b-
  stress per R15-A4 math.
- **Then**: PR 3 (R15-A3 retry-policy module split). Low risk,
  drops restore_handler.rs below `db.rs` again.
- **Then**: Smoke-r10 to verify the C-8 cluster patch closes
  the synchronous-wake regime; tag any remaining bugs as
  C-7-LT-blocking.
- **Then**: PR 1 implementation (C-7-LT) in 3 sub-PRs.
- **Then**: PRs 4-7 in parallel; PRs 8-9 sequentially.

Closes 12 carry-forwards + 5 r15-new findings + provides the
structural fix to the 5-cluster-bug pattern on the wake path.
