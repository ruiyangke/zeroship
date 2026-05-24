# Sandbox/snapshot-restore — performance r20 review

Date: 2026-05-25 (UTC)
HEAD at audit: `b18782f6`. Last perf round: r19 (`bffa6f1d`).
This round evaluates the R19-T1 / R19-API1 follow-ups, smoke-r15
telemetry (now the baseline since r16 didn't materialise), the
R20-I1 doc-bloat call from the r20 code-quality lens, and the new
C-7-LT-4 driver-side rewriter overhead introduced in driver v6.

## Summary

**4 new findings, 0 CRITICAL, 1 IMPORTANT, 2 MINOR, 1 INFO.** Nothing
landed between r19 and HEAD touches the SLO floor: R19-T1 is a 9-line
test addition, R19-API1 is an SQL-literal rephrase + tracing field
add (no hot-path change), and R19-I4 is a defensive retry loop that
only fires on a sub-ms terminal-during-race window (no measurable
cost on the happy path). Smoke-r15 captures the same SNAPSHOT 14.46 s
constant we've measured since r12 (AEAD baseline) and exposes a new
WAKE structural pattern (60 s ch.sock probe budget at 100 ms cadence)
that is **the new WAKE floor on the unhappy path** until C-7-LT-4
driver v6 closes the underlying CH `--restore` ENOENT. R20-I1 (doc
bloat at `from_host_fence_timeout`) has zero perf impact — rustdoc
is not codegen-visible. C-7-LT-4 driver v6's read-parse-rewrite-write
of the snapshot config is sub-ms on the WAKE critical path.

**Phase 3 win queue (unchanged from r19):** R16-P3 → R16-P5 → R11-P1
→ R17-P2. **R16-P2 remains CLOSED** (obsoleted by R19-I1). The
SNAPSHOT 14.46 s reproducible-cluster-constant continues to validate
R16-P3 as the largest single SLO win (~3-4 s).

## CRITICAL

None.

## IMPORTANT

### R20-P1 — smoke-r15 confirms SNAPSHOT 14.46 s is a reproducible cluster constant; R16-P3 ranking holds

**File:Line:** smoke-r15 cluster log §"Validation 3" (CREATE 6455 ms /
SNAPSHOT 14464 ms / WAKE 109729 ms). `crates/sandbox/src/snapshot_aead.rs:377-453`
(`encrypt_in_place`) + `crates/sandbox/src/snapshot_store.rs:184-223`
(`compute_artifact_sha256`).

**The data:** SNAPSHOT p50 wall across the AEAD-enabled cycles:

| Cycle | SNAPSHOT p50 | Notes |
|---|---|---|
| smoke-r11 | 6314 ms | KEK env UNSET (passthrough — not comparable) |
| smoke-r12 | ~14.6 s | first cycle with KEK provisioned |
| smoke-r13 | 14.72 s | r19 baseline |
| smoke-r14 | 14.72 s | matches r13 |
| **smoke-r15** | **14464 ms** | -260 ms vs r14, within run-to-run jitter |

SNAPSHOT is now a **reproducible cluster constant within ±2 %** across
four cycles. The r19 root-cause (AEAD encrypt + canonical-SHA re-read
on the L1 put critical path) is confirmed by the lack of variance —
if GCS or any non-AEAD path contributed material wall, we'd see
seconds of jitter. We do not.

**R16-P3 (fused AEAD + SHA) remains the #1 Phase-3 win.** The estimated
~3-4 s/SNAPSHOT cut from r19 is unchanged — the bottleneck has not
moved. R16-P5 (gzip-before-AEAD) stacks as PR-b for SNAPSHOT 14.5 s →
~8-9 s.

**One new SNAPSHOT signal in r15 worth noting**: smoke-r15 ran with
controller v30 (R19-I1 + R19-C1) and driver v5 (C-7-LT-3 probe + CH
stderr capture). Neither change touches the SNAPSHOT path — the +/-
260 ms vs r14 is purely jitter, not a v30 perf delta. Treat 14.46 s
as the new measured baseline; r19's 14.72 s figure remains within
the same ±2% envelope.

**Action**: no change. R16-P3 stays Tier-3 PR-a. SNAPSHOT roadmap
unchanged.

## MINOR

### R20-M1 — R19-I4 3-attempt INSERT retry: ~0 cost on the happy path, bounded on the pathological one

**File:Line:** `crates/sandbox/src/db.rs:3041-3120` (`insert_wake_job`),
introduced at `f2485210` (R19-I4).

The retry loop wraps the existing single-shot INSERT in `for attempt
in 0..3`. On the **happy path** (no conflict — first INSERT returns
`rows_affected == 1`): the loop body executes once and returns
immediately — **no measurable difference** vs the pre-R19-I4 shape.

On the **conflict + winner-found path** (winner is still non-terminal):
same as before — one INSERT + one `find_pending_wake_for_sandbox`
SELECT, returns `Replay(winner)`. Cost identical to pre-R19-I4.

On the **pathological retry path** (winner went terminal in the sub-ms
window between PG conflict resolution and the follow-up SELECT): each
extra attempt costs **one extra INSERT (~1-3 ms)** + **one extra
SELECT (~1-3 ms)** = ~2-6 ms per retry. Bounded at 3 attempts = ~12-18
ms worst case before returning `Validation`. The path is structurally
rare (requires two rapid-fire terminal transitions in sub-ms each
window) and the bound is small.

**Steady-state controller CPU impact:** 0. The branch only fires when
`find_pending_wake_for_sandbox` returns `None` after an ON CONFLICT —
which the r17 review pinned as "shouldn't happen in practice
(state-machine transitions take >> 1ms)". INFO/MINOR — no
re-prioritization.

### R20-M2 — C-7-LT-4 driver v6 config rewrite is sub-ms on the WAKE critical path

**File:Line:** `nomad-driver-ch/ch/config_rewrite.go:80-145`
(`rewriteAndAssertUnderTaskDir`) + `:206-260` (call sites for
`disks[].path`, `serial.file`, `console.file`, `fs[].socket`).
Introduced at `c1df13d3` and pinned in `dea68995` (driver v6).

The driver's restore branch now reads the snapshot's `config.json`,
JSON-parses it, walks 4 path-bearing field shapes, runs
`rewriteAndAssertUnderTaskDir` on each (regex match + alloc-prefix
substitution + `..`-component scan + `filepath.Clean` containment),
and writes the rewritten config back before invoking
`cloud-hypervisor --restore`.

**Expected cost on the WAKE critical path:**

| Stage | Estimated wall | Notes |
|---|---|---|
| Read `config.json` (typical ~2-4 KB) | <100 µs | one cold read; OS page cache covers any retries |
| `json.Unmarshal` | ~50-150 µs | tiny doc, no nested arrays beyond `disks`/`net` |
| 4 × `rewriteAndAssertUnderTaskDir` | ~10-20 µs total | regex match + small `filepath.Clean`s |
| `json.Marshal` + write back | ~100 µs | tiny doc |
| **Total** | **~300-500 µs** | rounded to sub-ms; ~0.005% of WAKE wall |

The rewrite happens **once** per restore (not per probe attempt), in
the driver's `startTaskRestoreBranch` before the CH spawn. It does
NOT live in the 100 ms ch.sock probe loop. On the post-C-7-LT-4
happy path (WAKE ~3-10 s projected), the rewrite is ~0.01% of wall.
On the unhappy path (still hitting the 60 s probe budget), the
rewrite would run once at +0 ms then sleep; total cost ~0.001%.

**Net**: zero SLO impact. The rewrite is a security/correctness win
(R15-S2 allow-list now enforced in Go, closing the bypass the bash
wrapper was guarding) with no measurable wall cost. INFO — no
roadmap entry needed.

### R20-M3 — smoke-r15 WAKE 109.7 s reveals a NEW unhappy-path WAKE floor (60 s ch.sock probe budget) that C-7-LT-4 must close

**File:Line:** smoke-r15 §"Driver ch.sock probe metrics" (verbatim:
`attempts=599 within 1m0s`); driver v5 `startTaskRestoreBranch` probe
loop (100 ms cadence × 60 s budget).

**Wake decomposition** (smoke-r15 verbatim trace, t=0 at POST):

| Stage | Wall | Source |
|---|---|---|
| POST → 202 mint | 87 ms | controller |
| `pending → reserving_slot` | 540 ms | controller |
| `reserve_vm_index_with_retry` (fence clear at attempt ~16, +probe) | 31.7 s | controller (capped by `from_host_fence_timeout(30, async)` = 70 s) |
| `restoring` (driver alloc submit + ch.sock probe at 60 s budget) | 77.1 s | driver |
| terminal `failed` | 0 ms | controller |
| **Total** | **109.73 s** | |

**The 60 s ch.sock probe budget is the new floor** for unhappy-path
WAKE wall time, replacing r14's 10 s flat budget. Pre-C-7-LT-4
landing this is structurally guaranteed: every WAKE on an
un-rewritten snapshot config burns the full 60 s before terminal.
**Post-C-7-LT-4 (driver v6)**, the CH process should bind ch.sock
within sub-100 ms of `--restore` exec, so the probe clears at
attempts=1-3 (~100-300 ms total). Expected post-fix unhappy-path
WAKE floor reverts to ~30-50 s (source-teardown 30 s + slot reserve
~2 s + restore ~1-5 s + CH boot to agent /livez ~5-15 s).

**Action**: monitor smoke-r16 (or whichever cycle ships driver v6 +
C-7-LT-4) for the probe-attempts metric — verify `attempts < 10`
post-fix. **If WAKE wall stays > 30 s** post-C-7-LT-4, escalate
because there's a structural cost we haven't surfaced yet (most
likely candidate: CH `--restore` itself reading + parsing the 1 GB
snapshot before binding the API socket).

Probe metric (`attempts=N`, `lastErr=...`) is **already plumbed
through to the Nomad task event** per driver v5 PR2; no additional
instrumentation needed. MINOR — observation, not a code change.

## INFO

### R20-I1 — R20 code-quality doc-bloat call has zero perf impact

**File:Line:** `crates/sandbox/src/restore_handler.rs:184-301`.

The r20 code-quality lens raised R20-I1 IMPORTANT: the rustdoc above
`from_host_fence_timeout` grew from ~103 → ~118 lines after r19-A4's
NON-NORMATIVE banner layered on the C-8a/C-8b/C-7-LT-1 historical
narrative without retracting it.

**Perf-lens take:** **zero impact.** rustdoc is stripped pre-MIR; it
doesn't affect codegen, inlining (rustc's heuristic is MIR-line not
source-line), or compile time (doc parse is sub-µs). Defer to the
code-quality lens's ADR-extraction recommendation; no perf roadmap
entanglement.

## Quantified-win roadmap (carry from r19, unchanged)

| Finding | File:Line | Win | Effort | Risk |
|---|---|---|---|---|
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:413`, `snapshot_store.rs:184` | **~3-4 s/SNAPSHOT** (confirmed by r15's 14.46 s constant) | ~80 LOC | medium |
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | SNAPSHOT ~3.5 s + WAKE ~2 s + 95% storage | ~200 LOC | medium |
| **R11-P1** Per-thread pg pool | `db.rs:515-521` | ~400-600 ms/wake controller CPU + SLO floor | ~150 LOC | medium |
| **R17-P2** Active-set cache for poll | `admin_handlers.rs:1741` | ~300-500 ms wasted pg work/wake | ~60 LOC | low |
| **R17-P1** Skip intermediate set_state writes | `wake_machine.rs:358, 381, 443-456, 128` | ~4-10 ms/wake | ~30 LOC | medium |
| **R18-P1** Drop pre-INSERT idempotency SELECT | `admin_handlers.rs:1565-1587` | ~5-15 ms × 100% wake POSTs | ~25 LOC | low |
| **R17-P1c** Plumb pre-flight row into machine | `wake_machine.rs:185`, `admin_handlers.rs:1585-1604` | ~2-6 ms/wake | ~15 LOC | ~zero |
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms | 2 LOC | ~zero |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low |
| **R18-P2** vm_index attempts-used metric | `restore_handler.rs:288-348` | instrumentation | ~5 LOC | ~zero |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3.8 s background (moot post-R16-P5) | 1 LOC | ~zero |
| **R17-P3 / R17-P4 / R18-M2 / R16-P8** trivia/instrumentation | (various) | ~zero–polish | small | ~zero |
| ~~R16-P2~~ livez 500 → 200 ms | `nomad_ch.rs:3091` | n/a | **CLOSED (R19-I1)** | |

**Top-3 SLO wins (unchanged):** R16-P3 (~3-4 s/SNAPSHOT) → R16-P5
(~3.5 s SNAPSHOT + 2 s WAKE) → R11-P1 (~400-600 ms wake-POST/poll).

## Cross-lens consensus

- **Code-quality r20 (R20-I1 doc bloat)**: perf lens concurs the doc
  has zero codegen impact; the readability concern is real but lives
  in the code-quality budget. Recommend ADR extraction without
  perf-roadmap entanglement.
- **Concurrency r20**: not yet read in detail by perf lens; the
  R19-C1 takeover sweep + R19-I1 two-phase probe shapes carried
  forward from r19 are unaffected.
- **Security r19 (R19-S1 driver-side symlink eval gap)**: perf lens
  notes the C-7-LT-4 rewriter deliberately skips `os.Stat /
  filepath.EvalSymlinks` (documented at `config_rewrite.go:99-107`)
  — a symlink walk would cost ~ms per field × 4 fields ≈ ~4 ms
  added wall, which is still sub-1% of WAKE but a real cost
  reviewers should be aware of when weighing the security trade-off.
  No perf objection to adding the check.
- **API surface r19 (R19-API1)**: closed at `fde4f51c` with SQL
  literal rephrased + R19-C1 lineage moved to `tracing::warn!`
  closure_ref field. Perf-neutral — the new tracing field is one
  extra static `&str` per claim event (claim events happen on the
  unhappy path at 60 s cadence; zero hot-path impact).

## Lens hand-off

**Tier 2 (CLOSED):** R16-P1 + R16-P2 — R16-P2 obsoleted by R19-I1;
R16-P1 is ~150 ms polish, doesn't justify its own PR. Unchanged.

**Tier 2.5 (1 PR, ~10 ms/wake + ~5-15 ms/wake POST, low risk):**
R17-P1c + R17-P3 + R17-P4 + R17-P2 + R18-P1 + R18-P2 metric.
Unchanged from r18/r19.

**Tier 2.75 (1 PR, R11-P1; ~400-600 ms controller CPU + SLO floor,
medium risk):** unchanged.

**Tier 3 (2 PR, ~6-7 s SLO win, medium risk):**
- **R16-P3 fused encrypt + SHA (PR-a)** — Tier-3 lead. r15
  reproducible 14.46 s constant confirms the SLO target.
- **R16-P5 gzip-before-encrypt (PR-b)** — gates on PR-a's chunk loop.

**Tier 4 (1 PR, instrumentation):** R16-P8 L1 hit/miss metrics +
R18-P2 attempts-used metric + R18-M2 bounded GC LIMIT. Unchanged.

R16-P4 + R16-P7 + R17-P1a/b defer until Tier-3 lands.

## Carry-forward (r19)

All r19 items remain OPEN with no priority change. R20-M3 (60 s
ch.sock probe as new unhappy-path WAKE floor) is a structural
observation that closes itself when C-7-LT-4 driver v6 lands; no
roadmap entry needed.

## Closures since r19

- **R19-T1** (test coverage for `WakeWorkerAborted` in §10.0
  renderer) — LANDED at `6fbfafb3`. No perf impact.
- **R19-API1** (rephrase takeover error_message; move R19-C1
  lineage to tracing field) — LANDED at `fde4f51c`. No perf impact
  (one extra static `&str` in a 60 s-cadence claim event log line).
- **R19-I4** (3-attempt INSERT retry on terminal-during-race) —
  LANDED at `f2485210`. ~0 cost on happy path; ~12-18 ms bounded
  worst case on pathological retry path. No SLO impact.

## Focus-area notes (brief)

**Post-cutover SLO projection** (smoke-r16, driver v6 + C-7-LT-4):
- CREATE: ~4.5-5.1 s (R19-I1 already in place).
- SNAPSHOT: ~14.5 s (unchanged; AEAD path).
- WAKE happy: ~3-10 s (C-7-LT-4 closes the 60 s probe waste).
- WAKE unhappy floor: ~30-50 s (source-teardown + reserve + restore + CH boot + /livez).

**Tier-3 stacked projection** (R16-P3 + R16-P5 landed):
SNAPSHOT 14.5 → ~8-9 s. WAKE happy 3-10 → ~1.5-7.5 s.
