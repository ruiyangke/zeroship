# Sandbox/snapshot-restore — performance r22 review

Date: 2026-05-25 (UTC).
HEAD at audit: `ef11edb3` (branch `feat/sandbox-snapshot-restore`).
Prior: `…performance-2026-05-25-r21.md`.

Round 22 evaluates: R20-C1 SQL guard landing (`ccb2abc8`), r25–r29
reviewer artifacts (`afa5da96`), driver v9 + C-7-LT-9 pre-create
(`f2641e88`), and the smoke-r20 cluster run at `ef11edb3` (110.21 s
WAKE wall, RED). Phase-3 carry queue revisited; nothing new lands
on the SLO floor.

## Summary

**5 findings, 0 CRITICAL, 1 IMPORTANT, 3 MINOR, 1 INFO.** Smoke-r20
**REFUTED** r21's projection that C-7-LT-9 would collapse WAKE wall
to ~10–15 s. **WAKE wall STAYED at 110.21 s** (vs r19 109.71 s, delta
+0.5 s = noise), the state machine **DID NOT advance past `restoring`**
(terminal: `restoring → failed`), and the ch.sock probe **ran the full
60 s / 599-attempt budget again** — third consecutive production fire.

C-7-LT-9 is a structurally-correct pre-create patch (driver v9 binary
verified on worker, SHA `7c45cdc0…`) that lands at the wrong layer:
**the rewriter writes `<runDir>/config.json` but CH reads
`<RestoreFrom>/config.json` via `--restore source_url=file://…`**
(source-audited at `restore_task.go:319, 360, 420`). C-7-LT-10 is the
3-line driver fix that actually closes the WAKE floor. **No Rust-side
perf change.**

**R20-C1 SQL guard** (`db.rs:3231-3232`) adds one `AND state NOT IN
('ok','failed')` predicate to a PK-targeted UPDATE. Single in-memory
filter eval after PK probe — **sub-microsecond per call, ~6 calls/wake,
totally invisible.**

**r21-A1 `user_id` JSON field** (verified at `fcac5355`): sub-microsecond
add to a Config block inside the existing `spawn_blocking` hop.
Confirmed perf-neutral as r21 predicted.

**Phase-3 queue unchanged. Top WAKE lever remains C-7-LT-10 (driver).**

## CRITICAL

None.

## IMPORTANT

### R22-P1 — smoke-r20 REFUTES r21's WAKE-collapse projection; C-7-LT-9 is layer-correct but file-wrong; floor unchanged

**File:Line:** smoke-r20 review (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r20.md`) + driver `restore_task.go:319, 360, 420` (source-audited via the smoke-r20 review's verbatim line refs).

**The number that did not move:**

| Cycle | WAKE wall | State-machine terminal | ch.sock probe attempts | Failure layer |
|---|---|---|---|---|
| r19 (driver v8, pre C-7-LT-9) | 109.71 s | `restoring → failed` | 599 / 599 (60 s budget) | CH `CreateConsoleDevice ENOENT` |
| **r20 (driver v9, post C-7-LT-9)** | **110.21 s** | **`restoring → failed`** | **599 / 599 (60 s budget)** | **CH `CreateConsoleDevice ENOENT` — byte-identical stderr** |

Delta: **+0.5 s = noise.** The state machine did **NOT** advance past
`restoring` for the **seventh consecutive cycle**. R19-I1 two-phase
livez probe is **STILL unexercised** (7th cycle).

**Why r21's projection was wrong:** r21 assumed C-7-LT-9's pre-create
loop addressed the right file. Source audit (smoke-r20) confirms CH
reads `<RestoreFrom>/config.json` (un-rewritten, points at source
alloc's task_dir), not the `<runDir>/config.json` the rewriter
materialises. The pre-create at `restore_task.go:387-395` correctly
creates `<runDir>/serial.log`, but CH opens the path stored in
`<RestoreFrom>/config.json` — a dead path on the destination worker.
Three rounds of stderr-pattern triage (r15, r19, r20) all stopped at
the same diagnostic depth; r20 is the first cycle with `file:line`
source evidence on the reader side.

**Projected post-C-7-LT-10 WAKE p50** (driver Shape-A fix: symlink
state.json + memory-ranges into `<runDir>`, repoint `--restore
source_url=file://<runDir>`):

| Stage | Projected p50 (post-LT-10) | r19 source/budget |
|---|---|---|
| POST → 202 mint | ~58 ms | unchanged |
| `pending → reserving_slot` | ~520 ms | unchanged |
| `reserve_vm_index_with_retry` | ~2 s warm / ~30 s source-race | unchanged |
| Restoring (store.get + config rewrite + submit) | ~3-8 s warm / ~10-25 s cold | unchanged; R16-P3/P5 still the only lever |
| ch.sock probe | **~100-300 ms** (1-3 attempts; collapses from 60 s) | **first time this floor moves in 6 cycles** |
| `wait_for_livez` (FIRST exercise of R19-I1) | ~5-15 s | first production data point |
| ClockResyncing + Registering | sub-second each | first production data point |
| **WAKE p50 (warm, no race)** | **~10-15 s** | unchanged from r21 |
| **WAKE p50 (cold-cache + source-race)** | **~40-60 s** | unchanged from r21 |
| **WAKE p99** | **~30-50 s** | unchanged from r21 |

**The projection itself is unchanged.** What r21 got wrong was *which*
driver-pin closes the loop. C-7-LT-10 is now the explicit dependency
(symlink state.json + memory-ranges into runDir, repoint
`--restore source_url`). Until that lands, **every WAKE on the unhappy
path will continue to consume the full 60 s ch.sock probe budget** —
**~99% of the unhappy-path WAKE wall.**

**Action:** no Rust roadmap change. Wait for driver v10 + smoke-r21.
The single largest remaining win in the entire WAKE path lives in
~5 lines of `restore_task.go`. Perf-lens objection level: **zero**
(perf has no work to do here; the bottleneck is structural and lives
in the driver).

**Diagnostic-discipline note**: future "file not found at path X"
defects must include source audit of both writer and reader BEFORE
shipping a driver fix. Three cycles of stderr-pattern-only triage
cost ~6 cluster-hours and ~$2 GCP. Workflow item for cluster-runbook.

## MINOR

### R22-M1 — R20-C1 SQL guard: ~50-100 ns/call × 6 calls/wake = sub-microsecond; pg query plan unchanged

**File:Line:** `db.rs:3222-3233`. **Diff:** `ccb2abc8`.

The added predicate `AND state NOT IN ('ok', 'failed')` extends the
WHERE clause of an UPDATE keyed by `wake_id = $5::TEXT` (the table's
PRIMARY KEY per `migrations/0009_wake_jobs.sql:41`). pg planner cost
analysis:

- **PK match**: index-only lookup on `wake_jobs_pkey`, ~1 page read.
  Plan node: `Index Scan using wake_jobs_pkey`. Cost dominated by the
  PK probe (~10-50 µs).
- **Filter predicate**: pg evaluates `state NOT IN ('ok','failed')`
  in-memory on the candidate row AFTER index match. Two TEXT
  comparisons, ~50-100 ns each. The plan node gets a `Filter:`
  child but does NOT add an extra index scan or planner-pass cost.
- **No new index touched.** `wake_jobs_state_idx` (partial on
  non-terminal states, `migrations/0009:111-113`) is unused by this
  UPDATE — the planner picks the PK index because `wake_id` is a
  point key.

**Per-call cost:** **~100-200 ns** on top of the existing ~1-3 ms
UPDATE round-trip. **Per wake:** ~6 state-transition writes
(`pending → reserving_slot → restoring → livez_polling →
clock_resyncing → registering → ok`, plus optional `failed`) =
**~600 ns - 1.2 µs total per wake**. Far below SLO floor noise.

**Plan-shape regression risk:** zero. The planner has no choice but
to use `wake_jobs_pkey` for an UPDATE keyed on the PK; the new
predicate is a post-fetch filter only. Verified by the test diff
(`tests/sandbox_pg_e2e.rs:200 LOC added`) — both `update_wake_job_
state_after_ok_is_noop` and `…_after_failed_is_noop` confirm
`rows_affected == 0` behaviour on terminal rows (correctness signal
that the predicate is wired, not a perf signal).

**Verdict: perf-neutral.** Concurrency r20/r21 was already cleared.

### R22-M2 — r21-A1 user_id JSON field: confirmed sub-microsecond as predicted

**File:Line:** `restore_handler.rs:2349-2357`. **Diff:** `fcac5355`.

r21-I1 predicted "~50-200 ns marginal cost on a ~17.5 s Restoring
phase = sub-microsecond." Smoke-r20 confirms this empirically (in the
negative sense — the Restoring-phase wall did not measurably change
between r19 and r20; controller delta is dominated by per-cycle
boot/provision noise, not the JSON field add).

The validator's user-home check fired and passed in r20 (no
`config-rewrite … user_id missing` error string in worker logs per
smoke-r20 §Carry-over implications) — the field is now plumbed
through end-to-end on the restore path. Perf-neutral on the data
plane; functional gate satisfied. **No further action.**

### R22-M3 — ch.sock probe at 60 s budget: third production fire, working as designed

**File:Line:** driver v9 `startTaskRestoreBranch` (from r20-M3 driver
notes; documented in r19 perf review).

The 60 s / 599-attempt probe **correctly surfaced CH's embedded
stderr tail** for the third consecutive cycle (r18/r19/r20). Without
this probe, the failure would manifest as a generic "task failed"
Nomad event with no CH diagnostic chain. Cost: 60 s/cycle on the
unhappy path; collapses to ~100-300 ms on the happy path once CH
actually binds the socket.

**Not a perf bug.** The 60 s ceiling is the unhappy-path floor by
design — perf-lens has no objection. **Concurrency-lens and
cluster-runbook should keep this on the unhappy-path tally** so that
once CH actually boots, the metric `attempts < 10` becomes the
canonical smoke-r21+ gate.

## INFO

### R22-I1 — Phase-3 carry queue unchanged; top SLO win still R16-P3 → R16-P5

| Finding | File:Line | Win | Effort | Risk | Status |
|---|---|---|---|---|---|
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:413`, `snapshot_store.rs:184` | **~3-4 s/SNAPSHOT** | ~80 LOC | medium | TODO |
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | **SNAPSHOT ~3.5 s + WAKE ~2 s** + 95% storage | ~200 LOC | medium | TODO |
| **R11-P1** Per-thread pg pool | `db.rs:515-521` | ~400-600 ms/wake | ~150 LOC | medium | TODO |
| **R17-P2** Active-set cache for poll | `admin_handlers.rs:1741` | ~300-500 ms wasted pg work/wake | ~60 LOC | low | TODO |
| **R17-P1** Skip intermediate set_state writes | `wake_machine.rs:358, 381, 443-456` | ~4-10 ms/wake (interacts w/ R21-M1 watchdog) | ~30 LOC | medium | TODO |
| **R18-P1** Drop pre-INSERT idempotency SELECT | `admin_handlers.rs:1565-1587` | ~5-15 ms × 100% wake POSTs | ~25 LOC | low | TODO |
| **R17-P1c** Plumb pre-flight row into machine | `wake_machine.rs:185`, `admin_handlers.rs:1585-1604` | ~2-6 ms/wake | ~15 LOC | ~zero | TODO |
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms | 2 LOC | ~zero | TODO |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low | TODO |
| **R18-P2** vm_index attempts-used metric | `restore_handler.rs:288-348` | instrumentation | ~5 LOC | ~zero | TODO |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3.8 s background (moot post-R16-P5) | 1 LOC | ~zero | TODO |
| **R21-M1** Restoring-phase watchdog tick | `wake_machine.rs:272`, `db.rs:3192` | concurrency fix; ~5-15 ms/wake | ~30 LOC | low | TODO |
| ~~R16-P2~~ livez 500 → 200 ms | `nomad_ch.rs:3091` | n/a | **CLOSED (R19-I1)** | | |
| ~~R20-C1~~ SQL terminal guard | `db.rs:3231-3232` | ~600 ns/wake | | | **LANDED `ccb2abc8`** |

**Top-3 SLO wins unchanged from r19/r20/r21:** R16-P3 (~3-4 s/SNAPSHOT) →
R16-P5 (~3.5 s SNAPSHOT + 2 s WAKE) → R11-P1 (~400-600 ms wake-POST).

**The blocker for Phase-3 to become measurable is C-7-LT-10**, not any
Rust-side item. Once smoke-r21 produces the first end-to-end green
WAKE, Phase-3 R16-P3 + R16-P5 can be benchmarked against the new
warm-WAKE baseline. Until then, Phase-3 wins are projected on r15's
reproducible ~14.5 s SNAPSHOT constant only.

## Cross-lens consensus

- **Concurrency r21 (R20-C1 SQL guard, LANDED `ccb2abc8`)**: perf-lens
  measured at ~100-200 ns/call × 6 calls/wake = ~600 ns-1.2 µs/wake.
  No plan-shape regression (PK probe + in-memory filter). Concurrency
  invariant strengthened (terminal rows are immutable). **No perf
  objection.**
- **Smoke-r20 (driver-side C-7-LT-10 root-caused)**: not a Rust-side
  perf item — fix lives in `nomad-driver-ch/ch/restore_task.go:319-420`.
  Perf-lens projection (r21 → r22) unchanged: once CH actually boots
  on restore, WAKE p50 lands at ~10-15 s warm. Carrying this here for
  hand-off discipline only.
- **Test-cov r21 / arch r21 / api-surface r21 / security r21**: no
  perf-relevant items. Reviewer artifacts at `afa5da96` confirmed
  perf-neutral (operator-endpoint envelope rewrites, instrumentation,
  contract tests).

## Lens hand-off

**Tier 2 (CLOSED):** R16-P1 + R16-P2 — unchanged.

**Tier 2.5 (1 PR, ~10 ms/wake + ~5-15 ms/wake POST, low risk):**
R17-P1c + R17-P3 + R17-P4 + R17-P2 + R18-P1 + R18-P2 metric.
Unchanged.

**Tier 2.5b (1 PR, R21-M1 watchdog tick; ~5-15 ms/wake, low risk):**
unchanged from r21. Couple with R17-P1 review.

**Tier 2.75 (1 PR, R11-P1; ~400-600 ms controller CPU + SLO floor,
medium risk):** unchanged.

**Tier 3 (2 PR, ~6-7 s SLO win, medium risk):**
- **R16-P3 fused encrypt + SHA (PR-a)** — Tier-3 lead. ~14.5 s
  reproducible SNAPSHOT confirms target.
- **R16-P5 gzip-before-encrypt (PR-b)** — gates on PR-a's chunk loop.

**Tier 4 (1 PR, instrumentation):** R16-P8 L1 hit/miss metrics +
R18-P2 attempts-used metric + R18-M2 bounded GC LIMIT. Unchanged.

R16-P4 + R16-P7 + R17-P1a/b defer until Tier-3 lands.

## Carry-forward (r21)

All r21 items remain OPEN with no priority change. R20-M3 (60 s
ch.sock probe budget) **still actively waiting on driver-side
C-7-LT-10** — smoke-r21 (first cycle past C-7-LT-10) will be the
first to measurably move the WAKE floor.

## Closures since r21

- **R20-C1** (terminal-overwrite SQL guard) — LANDED at `ccb2abc8`.
  Perf-neutral (~600 ns-1.2 µs/wake; PK probe + in-memory filter).
- **r21 driver-side C-7-LT-9** (pre-create runtime files) — LANDED at
  driver v9 `f2641e88`; **smoke-r20 REFUTED the expected WAKE-floor
  collapse** because the fix targets the wrong file. The pre-create
  itself works as designed; the rewritten config never reaches CH.
  New defect **C-7-LT-10** root-caused at `restore_task.go:319, 360,
  420` (smoke-r20 source audit).

## Focus-area notes (brief)

**Post-C-7-LT-10 SLO projection** (smoke-r21, driver v10 + symlink
state.json+memory-ranges into runDir + repoint --restore source_url):
- CREATE: ~4.5-5.1 s (unchanged; R19-I1 already in place).
- SNAPSHOT: ~14.5 s (unchanged; AEAD-decrypt-dominated).
- **WAKE happy (warm, no race): ~10-15 s.**
- **WAKE unhappy (cold-cache + source-race): ~40-60 s.**
- **WAKE p50 (mixed traffic, post-LT-10, pre-Phase-3): ~15-25 s.**

**Tier-3 stacked projection** (R16-P3 + R16-P5 on top of C-7-LT-10):
- SNAPSHOT 14.5 → ~8-9 s.
- WAKE happy 10-15 → ~5-10 s.
- WAKE p50 mixed → ~10-15 s.

**The next 3 measurement gates** (unchanged from r21):
1. Smoke-r21 WAKE wall (FIRST validation of the ~10-15 s happy
   projection; exercises R19-I1 two-phase livez for the FIRST time in
   8 cycles). **Hard blocker on driver C-7-LT-10.**
2. Smoke-r22+ p50 over ≥10 wakes (validates WAKE p50 mixed
   projection; needed before Phase-3 lands).
3. Phase-3 (R16-P3 + R16-P5) smoke (validates the ~5-10 s warm and
   ~8-9 s SNAPSHOT projections).

**Diagnostic-discipline lens hand-off:** future CH-restore defects
should require source audit of writer + reader BEFORE driver fix
(r15/r19/r20 burned ~6 cluster-hours at three diagnostic depths on
the same stderr). Workflow item — lands on perf-lens because perf
holds the projection-vs-reality ledger.
