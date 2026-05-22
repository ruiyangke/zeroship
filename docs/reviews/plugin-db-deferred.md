# crates/plugin-db — Deferred Backlog

Auto-managed by the pilot-cron-worker. Last reviewed: 2026-05-22 15:47.

**Cycle 15:47 closures (3)**: 5-cycle error-ux carry on `audit.rs:818` alphabet-naming (`02ead3f4`) + NEW-R14-2 F1 warn-shape drift in `validate.rs:100` (folded into `d07616a2`) + NEW-R9 `update_audit_status` docstring terminal-status enumeration (same commit). 3 reviewers returned: **security r12 = 86 (+1)** (F2 upgrade audit clean — ALTER race-free under PG semantics); **docs-audit r9 = 92 (+1)** (F2 docs exemplary; one new NIT closed inline); **test-coverage r14 = 87 (+1)** (capture-layer NEW-R11-1+R12-1 CLOSED at `0bf71f27`; surfaced NEW-R14-1 = MEDIUM CHECK-ALTER upgrade-path untested; NEW-R14-2 = drift just closed).

**Design-loop round 2 closed**: reviser landed all 3 round-2 CRITICALs + 10 IMPORTANT + 11 MINOR + 7 newly-visible missing concepts. Doc now 1141 lines (9% over 1050 target band; reviser explained — 7 new missing-concept paragraphs required dense additions). Round 3 critic in flight.



**Cycle 15:17 closures (2 + design-loop round 2)**: **F2** resolved via two commits — `14d7608f` (initial Failed+marker pattern) then `6afab751` (upgrade to dedicated `ValidationRefused` terminal per migration-pipeline r13's recommendation; INSERT-direct, no orphan window). 3 reviewers returned: **performance r13 = 82 (+1)** with honest correction (V8 half of [C3] still unmeasured; r14 needs `bench_v8_json_parse`); **concurrency r12 = 89 (±0)** (3rd-round plateau; capture-layer audit clean); **migration-pipeline r13 = 87 (+1)** (caught the Failed-marker semantic regression and recommended the upgrade). Design-loop **round 2 critic = 77/100 (+15)** — all 7 round-1 CRITICALs CLOSED; abstraction-level 38→86; 3 new CRITICALs from round-1 revision (MV/CDC storm; SQLite drop-namespace ordering; PG WAL sub-protocol support unstated). Round-2 reviser in flight.

**[C3] actionability correction**: performance r13's honest read — the Rust-side residual (4.24 µs at 50-col) IS real, but the V8 `JSON.parse` half (which is what `ResolveValue::JsonValue` would actually buy back) isn't measured by either bench. [C3] is actionable for design work, not yet for sizing the win. r14 forcing function: V8-side bench measuring `serde_json::Value` → V8 boundary.

**Active filter (cycle 14:47 onward — design redesign in progress)**: the full system redesign in worktree `proposal/db-system-design` will reshape the `Backend` trait surface and `IsolateDbContext` shape. Backlog picks during the redesign window MUST be **refactor-safe** — i.e., they survive the capability-trait split unchanged. Skip the rest until the design lands.

**Refactor-safe (continue picking)**: F1 sweeper-half, F2 terminal-state resolution, [I43] security DoS, [C3] (now actionable per cycle 14:47 measurement), tracing-subscriber test infrastructure (NEW-R11-1 + NEW-R12-1 + R13 carry), [I3] migrations advisory lock RAII (Drop guard is independent of trait shape), [I14] lenient integration test, [I22] mint_* Box leak (V8 Weak pattern is independent of backend), all doc-drift items, all test-coverage gaps that don't touch trait surface, all error-message improvements.

**Design-pending (skip; would be redone by the trait split)**: further `pub`/`pub(crate)` visibility sweeps on context/backend module, anything that explicitly changes Backend trait method shape (would be redone), [I15] RegisterContext calling convention, [I17] dispatch_op type ascription, I-R11-1..3 (further accessor / set_pool / create_ad_hoc_backend), [C1] itself (it IS the redesign), [C2] query.rs split (architect r10's "defer until third caller"), [I16]-style field privatization (already partially landed; further work pending design).



**Cycle 14:47 closures (2 + [C3] graduation)**: I-R11-1 (`91771aaf` — demote 27 IsolateDbContext accessors to `pub(crate)`; arch r11's predicted +2 ceiling step from 92→94 on api-surface) + bench_first_row_or_null (`4e9dbafb` — perf r13 forcing function landed with measured numbers). 3 reviewers returned: **test-coverage r13 = 86 (±0)** (GAP-3 now 7-cycle carry; tracing-subscriber dev-dep unused 2 cycles; 3 NEW LOW bench-self-test gaps); **error-ux r11 = 94 (+1.5)** (6-cycle finalise_backfill carry CLOSED by `7c6bd2ec`+`18aee490`; F1 warn-shape unified across 6 sites); **bench fixer** landed `4e9dbafb` with measured numbers (narrow 560 ns / medium 2.6 µs / wide 50-col **11.66 µs**) — **3.9× the r12 3 µs threshold**. **[C3] graduates from blocked to actionable-now** (entry updated by the fixer; see below).

**Cycle 14:17 closures (1 + design milestone)**: [I16] continued — privatization confirmed by architecture r11 / security r11 / api-surface r11 (already committed at cycle 13:47 as `f6adb68b`). **Design milestone**: full plugin-db system design doc drafted in worktree `proposal/db-system-design` (~3000 LOC) covering positioning (PG=prod, SQLite=dev), 15-capability trait split (closes [C1]), SQLite workaround mapping for all 25 cross-cutting capabilities, migration pipeline, reactive queries (PG WAL + SQLite update_hook+outbox), auth subsystem, metering, multi-tenancy, threat model, 6 implementation phases, open questions. Not committed (per `feedback_proposal_workflow.md`). 3 reviewers returned: **architecture r11 = 95 (+2)** — privatization was structurally meaningful (api-surface +3, module-boundaries +3, coupling-debt +2); **docs-audit r8 = 91 (+4)** — all cycle-13:17/13:47 commits land docs cleanly; **security r11 = 85 (+1)** — privatization compile-time-enforces `tx_token_counter` invariant; surfaced r10 location-inaccuracy (corrected below). Total +7 across 3 lenses.

**r10 location correction** (per security r11): [I43] blocking `pg_advisory_lock` is at `crates/plugin-db/src/backend/postgres.rs:118` (always-compiled), NOT in `auth/bootstrap.rs` as the deferred entry previously claimed. The site is in production builds regardless of `--features hardening`. The DoS-within-app concern remains; the fix path is unchanged.

**Cycle 13:47 closures (1)**: [I16] (`f6adb68b` — privatize 11 `IsolateDbContext` fields; api-surface r11 confirmed +2 ceiling step). 3 reviewers returned: **api-surface r11 = 92 (+1, NEW-R10-1 closed)**, **migration-pipeline r12 = 86 (±0, r11's clarification — I35 narrower than predicted)**, **performance r12 = 81 (±0, harness can't see I35; recommends `Row::new_for_test` cross-crate constructor + `bench_row_to_json`)**. Total +1 across 3 lenses.

**Cycle-14:17 perf forcing function — LANDED**:
- `bf75e866 compio-postgres: add test-utils feature + Row/Statement/Column builders` — cross-crate enabler. `test_utils.rs` (+134 LOC) gated behind `test-utils` Cargo feature; `row_for_test` synthesises a real DataRow wire-format message and feeds it through `postgres_protocol::Message::parse`, so the resulting Row exercises the same `RowIndex` + decode paths as live traffic. Production builds never see the module.
- `75d9ae5c plugin-db: add bench_row_to_json measuring [I35] index-lookup fix` — Criterion harness over narrow (3-col) / medium (10-col) / wide (50-col) shapes via the new `row_to_json_for_bench` `#[doc(hidden)]` wrapper.

**Measured [I35] win** (pre vs post `251d53b4`; criterion defaults; Intel Xeon @ 2.80 GHz):

| shape         | pre-I35  | post-I35 | delta   |
|---------------|----------|----------|---------|
| narrow_3cols  | 218.34 ns| 216.80 ns|  -0.7%  |
| medium_10cols | 1332.5 ns| 1273.1 ns|  -4.3%  |
| wide_50cols   | 8951.3 ns| 7762.0 ns| -13.0%  |

The wide-row delta (p < 0.05) is the ground-truth size of the [I35] win. Earlier "speculative ~5×" was over-stated — the realistic win is single-digit-% at typical column counts; double-digit at 50-col. JSON decode + serde dominate; the per-column linear-scan was real but not the bottleneck.

**Next-cycle forcing function (perf r13)**: with row-decode now benchable, the carrier shifts to [C3] (`first_row_or_null` serde round-trip in `crates/plugin-db/src/crud.rs:107`). Wide-row `row_to_json` at 7.77 µs is now visibly smaller than the JSON-string + V8 `JSON.parse` tail it feeds. Add a `bench_first_row_or_null` sibling that includes the `.to_string()` boundary cost; if it lands above ~3 µs at 50-col, [C3] graduates from "deferred" to a typed-`Value` resolver redesign (the runtime-crate boundary change the deferred entry currently blocks on).

**Cycle 13:17 closures (3)**: [I35] (`251d53b4` — `row_to_json` O(N²) → O(N) via index lookup) + NEW-R12-2 (`18aee490` — finalise_backfill warn-shape drift caught by test-coverage r12 in my own 7c6bd2ec commit) + NEW-R10-1 (`bac64c0e` — 5 mig_lock accessor visibility demotions, 2-cycle carry from r9 NEW-R9-3). 3 reviewers returned: **api-surface r10 = 91 (+2, closed 2 of r9's NEW findings)**, **test-coverage r12 = 86 (+1, GAP-2 closed, found my drift)**, **concurrency r11 = 89 (+1, plateau broken by F1 warn-half forcing function)**. Total +4 across 3 lenses. New LOW findings: NEW-R12-1 (no tests for I6's new Err arms) + r12 recommends `tracing-subscriber` test pattern for the 9 emission sites accumulated over cycles 10:47–12:47.

**Cycle 12:47 closures (2)**: F1 warn-half style unification (`7c6bd2ec` — pinned 6-site shape per code-critique r11 MINOR-R11-1) + `finalise_backfill` name/collection (6th cycle error-ux carry, folded into same commit). 3 reviewers returned: **migration-pipeline r11 = 86 (+1)** plateau broken by warn-half forcing function; **error-ux r10 = 92.5 (+1.5)** plateau broken across all 3 prior commits; **code-critique r11 = 95 (±0)** plateau held but cycle audit clean.

**Cycle 12:17 closures (4)**: [I6] release_advisory_lock returns Result (`51c342e8`) + [F1] warn-half (`fcf7ce3c` — 5 `let _ = update_audit_status` sites converted to structured `tracing::warn!`) + 2 doc-drift cleanups (`71a457a1` — auth/mod.rs Backwards-compatibility section + lib.rs module-visibility note, both from docs-audit r7). 3 reviewers returned: **docs-audit r7 = 87 (+4)**, **migration-pipeline r10 = 85 (±0, plateau n=3)**, **security r10 = 84 (+1, credited [I12])**. Total +5 score-points across 3 lenses. Reviewers under-covered this cycle: error-ux, architecture, concurrency, code-critique, performance, test-coverage, api-surface.

**Cycle 11:17 closures (4)**: [I12] ASCII tightening (`403b3891`) + [I25] retro-closed via `c0590506` + 3 doc/vis cleanups (`4cab871a` — slot_status visibility, context.rs:404 docstring, error.rs preamble) + [I13] testable subset (`ae5570dc`). 4 reviewers returned: **code-critique r10 = 95 (+1)**, **api-surface r9 = 89 (+7)**, **test-coverage r11 = 85 (+1)**, **performance r11 = 81 (±0, no forcing function)**. Total +9 score-points across 4 lenses — significantly above cycle 10:47's +2.

**Cycle 10:47 closures (2)**: MAJOR-R9-5 (auth/* hardening gate, commit `2fa9472e`) + [I23] (mig_lock state-drift tracing, commit `5d9acab8`). 4 reviewers returned: architecture r10 = 93 (+1, credited the hardening gate), security r9 = 83 (+1, same credit), error-ux r9 = 91 (±0), concurrency r10 = 88 (±0, surfaced one NEW MINOR-latent — Subscription::close at broker.rs:359-369 holds borrow_mut across w.wake).

**Cycle 10:30 backlog audit**: 7 IMPORTANTs were carrying stale status; closures verified in code and moved to SUPERSEDED. Remaining open: 3 CRITICAL (all blocked) + 14 IMPORTANT (was 15 pre-13:47; [I16] closed this cycle). [I31]/F1 is now half-closed (warn-half landed; sweeper-half still needs design).

**STRONG PLATEAU SIGNAL (sustained through cycle 10:47)**: cycle 09:30's 4-of-4 ±0 movement has only marginally improved — cycle 10:47's +1/+1/0/0 came entirely from the hardening cfg-gate (a single forcing function), not from forward motion on lens-specific findings. Architecture reviewer recommends capping at r11 if the `query.rs` split lands; concurrency reviewer recommends skipping cycles until the two carried IMPORTANTs land. Migration-pipeline reviewer notes "first non-positive movement since r2"; performance reviewer "explicitly recommends NOT running r10 without a forcing function".

**Recommended next actions** (for user attention):
1. **Cross-crate I5** — auth/* `--harden` wire-up in `crates/control/`. Requires scope grant; not in plugin-db pilot scope.
2. **Land a `cargo bench` harness** at `crates/plugin-db/benches/` — would unblock performance review's anti-fabrication block (currently every perf finding is annotated "unknown — needs measurement"). Bench-driven r10 could re-enable forward motion on perf.
3. **F1+F2 schema migration** (orphan Running + Pending audit rows) — needs deployment story.
4. **Downshift cadence**: reduce cron from `:17/:47` to once per hour or longer until a forcing function lands.

Source reviews triaged (25 total):
- `plugin-db-api-surface-2026-05-22-r1.md`
- `plugin-db-api-surface-2026-05-22-r9.md`
- `plugin-db-architecture-review-2026-05-21.md`
- `plugin-db-architecture-review-2026-05-21-round2.md`
- `plugin-db-architecture-review-2026-05-22-r3.md`
- `plugin-db-architecture-review-2026-05-22-r10.md`
- `plugin-db-code-critique-2026-05-21.md`
- `plugin-db-code-critique-2026-05-22-r2.md`
- `plugin-db-code-critique-2026-05-22-r10.md`
- `plugin-db-concurrency-2026-05-22-r2.md`
- `plugin-db-concurrency-2026-05-22-r10.md`
- `plugin-db-docs-audit-2026-05-22-r1.md`
- `plugin-db-docs-audit-2026-05-22-r7.md`
- `plugin-db-error-ux-2026-05-22-r1.md`
- `plugin-db-error-ux-2026-05-22-r9.md`
- `plugin-db-migration-pipeline-2026-05-22-r1.md`
- `plugin-db-migration-pipeline-2026-05-22-r10.md`
- `plugin-db-performance-2026-05-22-r1.md`
- `plugin-db-performance-2026-05-22-r2.md`
- `plugin-db-performance-2026-05-22-r11.md`
- `plugin-db-security-2026-05-22-r1.md`
- `plugin-db-security-2026-05-22-r9.md`
- `plugin-db-security-2026-05-22-r10.md`
- `plugin-db-test-coverage-2026-05-22-r2.md`
- `plugin-db-test-coverage-2026-05-22-r11.md`

HEAD at triage time: `5be3c1a1`. Recent fix-wave commits absorbed: `a00c41fd`, `5be3c1a1`, `cac3e542`, `b4e533e2`, `37a0ef76`, `d7cfc089`, plus `2fe9e9f0`, `b2496364`, `ff220fce`, `60ca1ad6`, `967a7362`, `78a95d3b`, `c54a9f15`, `cc7fff89`, `a0fec06a`, `d2aeada6`, `e8463ef0`, `b94fbdeb`, `f1c475f5`, `de01b3a0`, `d27ea71e`, `0816feb0`, `29b8a013`, `a3561ae4`, `81345420`, `10fe0b82`, `094261e1`, `52ff1c83`.

---

## CRITICAL (blocked or needs design)

### [C1] Backend trait is half-applied (architecture R2-I1 / R2-I2 / R3-I1)
- **Source**: `plugin-db-architecture-review-2026-05-21-round2.md` §4-I1, §4-I2; `plugin-db-architecture-review-2026-05-22-r3.md` §5
- **File**: `crates/plugin-db/src/backend/mod.rs:68-355` (trait); `crates/plugin-db/src/migrations.rs:185-707` (six fns take `&PostgresBackend`); `crates/plugin-db/src/replication.rs:1-570` (bypasses trait); `crates/plugin-db/src/wal_consumer.rs:1-1258` (bypasses trait)
- **Description**: The 26-method `Backend` trait is consumed generically only by `orchestrator/register_model/{plan,validate,apply}.rs` (`B: Backend`). Every other caller — `migrations::exec_*`, `v8_classes/migration.rs::ensure_backend`, `replication_ops.rs::ensure_pool`, and the entire `replication.rs` + `wal_consumer.rs` pair — names `&PostgresBackend` or talks directly to the raw `Pool`. The seam exists in name only; a future contributor adding a 27th method has no structural barrier preventing Postgres-specific leakage.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes. Grep `migrations.rs` line 185: `pub(crate) async fn exec_begin(backend: &PostgresBackend, …)` — concrete type, not `&impl Backend`. `replication.rs` and `wal_consumer.rs` do not import `Backend` at all.
  - Blocker: **Design decision needed.** R2/R3 reviews explicitly recommend two paths: (1) shrink — delete the trait, keep `PostgresBackend` as a concrete struct; (2) grow — pull `query.rs` builders + replication slot ops behind the trait. R2 §6 recommends path (1) "for now" and (2) "when a second backend is in flight" (>6 months out per AGENTS.md).
  - Already-superseded-by: N/A
- **Effort**: large (multi-file refactor or full removal)
- **Pickable this cycle**: no — needs explicit design decision from the user; both paths involve >5 files and one chooses an irreversible direction.

---

### [C2] `query.rs` is 4278 LOC; `build_aggregate` is 211 LOC of inline match arms (architecture R1-I5)
- **Source**: `plugin-db-architecture-review-2026-05-21.md` §3-IMPORTANT-I5; `plugin-db-architecture-review-2026-05-21-round2.md` §5; `plugin-db-architecture-review-2026-05-22-r3.md` §5
- **File**: `crates/plugin-db/src/query.rs:1405-1616` (build_aggregate inline match per operator)
- **Description**: Every aggregator (`$sum`, `$avg`, `$count`, `$percentile_cont`, `$max`, `$min`, `$first`, `$last`, etc.) is a match branch on a string discriminator; adding a new aggregator means editing `build_aggregate` plus `build_having_condition` and possibly `build_field_condition`. No registry / no `Aggregator` trait. Combined with the file's 4278 LOC, this is the single largest unrefactored module.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes. `query.rs` is 4324 lines at HEAD (grep confirms `pub(crate) fn build_aggregate` near line 1405); commit history shows zero refactor commits touching the aggregate path since R1.
  - Blocker: R3 explicitly defers this until an aggregator extension is needed (third caller). No active demand.
  - Already-superseded-by: N/A
- **Effort**: large (design + multi-builder refactor)
- **Pickable this cycle**: no — explicitly deferred until a third caller arrives or a new aggregator (`$median`, `$stddev`, `$variance`) is required.

---

### [C3] Serde round-trip on read path (perf C1)
- **Source**: `plugin-db-performance-2026-05-22-r1.md` §2-C1; `plugin-db-performance-2026-05-22-r2.md` §2 "STILL IN FLIGHT"
- **File**: `crates/plugin-db/src/exec.rs:83-87`, `crates/plugin-db/src/crud.rs:105-108`, `crates/plugin-db/src/v8_bridge.rs` (`rows_to_json_value`)
- **Description**: Original perf C1 flagged `findOne` paying ~4 serde parse/serialise round-trips. Partial closure: `cc7fff89` ("thread Vec<Value> end-to-end") landed and `exec_query` now returns `Vec<Value>` (`exec.rs:83`); `crud::first_row_or_null` (line 105) does one `.to_string()` followed by V8 `JSON.parse`. **The triple-round-trip is gone — net 2 operations, down from 4.** What remains is the final `to_string`→`JSON.parse` boundary cost, which is structural for `ResolveValue::Json`.
- **Status as of 2026-05-22 (perf r13 measurement)**:
  - Code still exists? The original 4-parse chain is closed. `exec.rs:83-87` returns `Vec<Value>` directly; `crud.rs:105-108` does one serialise; `ResolveValue::Json` parses once in V8.
  - **`bench_first_row_or_null` measurements** (the r12 forcing function for this entry; harness lives at `crates/plugin-db/benches/bench_first_row_or_null.rs`; covers `&[Row] → rows_to_json_value → first_row_or_null` lowering, i.e. the Rust-side half of the `findOne` resolve path; the V8 `JSON.parse` tail is downstream and not in this number):
    | shape         | bench time |
    |---------------|------------|
    | narrow_3cols  | ~560.61 ns |
    | medium_10cols | ~2.625 µs  |
    | wide_50cols   | ~11.66 µs  |
  - **Graduation verdict: ACTIONABLE-NOW.** The 50-col point at 11.66 µs is ~3.9× the r12-defined 3 µs threshold; medium at 2.6 µs is just under, narrow comfortably below. The wide-row residual is large enough that the cross-crate `ResolveValue::JsonValue` redesign is no longer "speculative tail" — it is the dominant cost on the wide-row read path (`row_to_json` was 7.76 µs at 50-col per cycle 14:17; the additional ~3.9 µs here is the `.to_string()` boundary on top of the decode work).
  - Blocker: Further reduction requires plumbing `Vec<Value>` directly to V8 (new `ResolveValue::JsonValue` shape) — design change in `zeroship-runtime::state`. The bench now quantifies the win envelope (up to ~3.9 µs at 50-col; less at narrow/medium).
  - Already-superseded-by: `cc7fff89 plugin-db: thread Vec<Value> end-to-end (drop serde round-trip)` — most of the win is in.
- **Effort**: medium (multi-crate; requires a new `ResolveValue` shape)
- **Pickable this cycle**: yes — graduated from "deferred" to "actionable-now" by the bench above. The cross-runtime-crate refactor is now justified by a measured 50-col cost above the threshold the r12 forcing function set.

---

## IMPORTANT (mechanical, actionable)

### [I3] Migration advisory lock has no RAII guard (security DoS)
- **Source**: `plugin-db-security-2026-05-22-r1.md` §2 "Advisory-lock DoS via stalled migration client"; `plugin-db-concurrency-2026-05-22-r2.md` §2 "exec_commit_batch leaves mig_lock slot occupied on dry-run COMMIT network failure"
- **File**: `crates/plugin-db/src/migrations.rs:548-553` (dry-run ROLLBACK path); migration `mig_lock` lifecycle in `IsolateDbContext`
- **Description**: `exec_commit_batch` for `dry_run=true` issues ROLLBACK; if that call returns Err, `return_lock_client(client)` is not called and `mig_lock` slot remains `Some` without a client. Subsequent calls see `has_mig_lock() == true`, attempt `take_lock_client()` → None, trigger `"lock client missing"`. Same shape on real-run COMMIT failure. Not a silent data-corruption risk (self-consistent), but observable as a stuck migration that only clears on isolate teardown. The register_model path got its RAII unlock fix (`apply.rs`, commit `37a0ef76`), but the migration backfill path did not.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes. `migrations.rs:548-553` shows the dry-run ROLLBACK branch returning Err without resetting `mig_lock`.
  - Blocker: small design touch — needs an RAII guard analogous to `SuppressGuard` (wal_consumer.rs) to release lock client + slot on Drop.
  - Already-superseded-by: N/A
- **Effort**: medium (needs an RAII type or manual finally-style cleanup at every return site)
- **Pickable this cycle**: yes if a single-file fix is acceptable; the RAII path is multi-file.

---

### [I4] `exec_fetch_batch` re-serialises rows to JSON string (perf N-I2)
- **Source**: `plugin-db-performance-2026-05-22-r2.md` §3 N-I2
- **File**: `crates/plugin-db/src/migrations.rs:402-403`
- **Description**: `let row_jsons: Vec<Value> = rows.iter().map(row_to_json).collect(); Ok(Value::Array(row_jsons).to_string())` — caller (v8_classes/migration.rs:229) passes the String to V8 `JSON::parse`. Same double-serialise pattern that `cc7fff89` closed on the CRUD hot path is still present on the backfill batch path. Backfill is lower-frequency than CRUD, so lower urgency, but a single-file fix lifts the same cost.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes. `migrations.rs:402` confirmed via grep.
  - Blocker: none (single-file change, parallel to the closed CRUD fix).
  - Already-superseded-by: N/A
- **Effort**: small (single-file; mirror the `cc7fff89` pattern)
- **Pickable this cycle**: yes.

---

### [I5] `acquire_dedicated_client` detached connection task — no JoinHandle / FD leak window (perf I3)
- **Source**: `plugin-db-performance-2026-05-22-r1.md` §3-I3; `plugin-db-performance-2026-05-22-r2.md` §2 "STILL OPEN I3"; `plugin-db-code-critique-2026-05-21.md` C2 (Stage 8 closed the eprintln half)
- **File**: `crates/plugin-db/src/backend/postgres.rs:76-82`
- **Description**: `compio::runtime::spawn(async move { connection.run().await }).detach()` — no JoinHandle returned, no cancellation tie to the `Client` lifetime. Under transaction rollback storms, io_uring SQE slots and file descriptors leak proportional to error rate. Latent under steady state; bites under elevated error rates. The `eprintln!` → `tracing::error!` half was closed (commit `094261e1`); the lifetime-tie half remains.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes. `backend/postgres.rs:76-82` shows `.detach()` with no handle retention.
  - Blocker: design — needs a wrapper type that holds both `Client` and `JoinHandle`, plus updating every `tx_conn` / `mig_lock` slot to carry it.
  - Already-superseded-by: half — observability closed (`094261e1`), lifecycle open.
- **Effort**: medium (touches `IsolateDbContext` field types + every acquire/release site)
- **Pickable this cycle**: no — latent only; cheaper to pick after a real FD-exhaustion incident motivates the wrapper design.

---

### ~~[I6] `release_advisory_lock` trait signature returns `()`~~ — CLOSED cycle 12:17
- **Closed by**: `51c342e8 plugin-db/backend: release_advisory_lock returns Result (I6)`
- Trait now returns `Result<(), DbError>`; Postgres impl lifts via `DbError::from_pg`. Cycle 12:17 audit found two production callers in `migrations.rs` (not zero as the deferred entry claimed) — both now `tracing::warn!` on Err and continue, matching `OrchestratorLockGuard::release`'s pattern. The lock auto-releases on session end so this stays observability-only. 352 lib tests pass.

---

### [I7] Lenient-strictness `validation_refused` audit rows orphan in `pending` (migration-pipeline I4)
- **Source**: `plugin-db-migration-pipeline-2026-05-22-r1.md` §3-I4
- **File**: `crates/plugin-db/src/orchestrator/register_model/validate.rs:67-85`, `apply.rs:183`
- **Description**: In `lenient` mode validate writes `pending` audit rows for destructive ops and returns `Ok(ApprovedPlan)` with the destructive ops included; apply then skips them via `if op.class == Destructive { continue; }`. The `pending` rows never get a `Running`→`Applied/Failed` transition. The audit table accumulates phantom `pending` rows on every lenient deploy that has a destructive op, with no terminal state. Operators querying `status = 'pending'` see phantom in-flight work.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes (reviewed code unchanged since the migration-pipeline review).
  - Blocker: small design — pick "skip + update to `skipped` terminal status" or "don't write the `pending` row at all in lenient mode".
  - Already-superseded-by: N/A
- **Effort**: small (one branch in `validate.rs` or one transition in `apply.rs`)
- **Pickable this cycle**: yes — small, but needs the operator-visible state-machine decision (which terminal state to use).

---

### [I10] Audit-table `validate_cursor` column name is misleading (migration-pipeline M1)
- **Source**: `plugin-db-migration-pipeline-2026-05-22-r1.md` §3-M1
- **File**: `crates/plugin-db/src/audit.rs:509`
- **Description**: Column was named `validate_cursor` for a pre-Stage-3 concept; it is now used as the general-purpose scroll cursor for the backfill loop. Operators reading the audit table see `validate_cursor = 450` and may assume it relates to a validation check rather than the last-read row id. Wire-change requires a migration.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes — `audit.rs:509` confirmed (column name unchanged).
  - Blocker: wire compatibility — every existing audit row has the old column name. Renaming requires a backfill migration.
  - Already-superseded-by: N/A
- **Effort**: medium (DB migration + code rename)
- **Pickable this cycle**: no — wire-change with low operational benefit; defer indefinitely or document.

---

### ~~[I12] `validate_field_name` permits non-ASCII identifiers (test-coverage GAP-1; security MINOR)~~ — CLOSED cycle 11:17
- **Closed by**: `403b3891 plugin-db/query: validate_field_name rejects non-ASCII (I12)`
- Added the same `is_ascii_alphanumeric() || '_'` check `validate_collection` uses. Two new unit tests (`validate_field_name_rejects_non_ascii`, `validate_field_name_accepts_ascii_allowlist`) pin the contract. 349 tests pass.

---

### ~~[I13] Missing unit tests for `queue_or_emit` / `drain_pending_emits_on_commit` (test-coverage GAP-2)~~ — CLOSED (testable subset) cycle 11:17
- **Closed by**: `ae5570dc plugin-db/exec: unit tests for queue_or_emit / drain / clear (I13)`
- Three new unit tests in `exec::tests` cover three of the four branches: (a) `queue_or_emit` autocommit → immediate `emit_local`; (b) `drain_pending_emits_on_commit` publishes every queued event + 2nd drain is no-op; (c) `clear_pending_emits` drops queue without firing. The fourth branch (`queue_or_emit` with a real tx parked) requires a `compio_postgres::Client` that isn't constructible outside the driver crate — that branch is covered by `gap_b_subscriber_does_not_observe_pre_commit_state` in `tests/integration.rs`. 352 lib tests pass (was 349, +3).

---

### [I14] Missing `lenient` strictness integration test (test-coverage GAP-3)
- **Source**: `plugin-db-test-coverage-2026-05-22-r2.md` §3 GAP-3 (priority MEDIUM)
- **File**: `crates/plugin-db/tests/integration.rs` (no `lenient` test in current suite)
- **Description**: Validate-strictness has three branches: strict (tested), off (tested), lenient (untested). A regression in lenient would silently discard destructive ops without error. Creator-facing staging-mode flow is uncovered.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Confirmed by the reviewer's `grep -rn "lenient" tests/integration.rs` returning zero hits.
  - Blocker: needs live Postgres (integration test).
  - Already-superseded-by: N/A
- **Effort**: small (~50 lines copying an existing a2_* test)
- **Pickable this cycle**: yes — pure test (gated on local Postgres).

---

### [I15] `RegisterContext` calling convention is inconsistent (architecture R2-I5)
- **Source**: `plugin-db-architecture-review-2026-05-21-round2.md` §4-I5
- **File**: `crates/plugin-db/src/orchestrator/register_model/{bootstrap,plan,validate,apply}.rs`
- **Description**: `bootstrap` returns `(RegisterContext, PooledClient<'p>)` by value. `plan` and `validate` take `&RegisterContext`. `apply` takes `RegisterContext` by value (destructures it). The by-value-at-end pattern works only because `apply` is last; a future stage insertion would force a redesign.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes (reviewed code unchanged).
  - Blocker: small refactor; needs consensus on either all-by-ref or `&mut`.
  - Already-superseded-by: N/A
- **Effort**: small (signature alignment across 4 files)
- **Pickable this cycle**: yes — but low leverage; deferred until a fifth stage proposal.

---

### ~~[I16] `IsolateDbContext` fields are `pub(crate)` rather than private~~ — CLOSED cycle 13:47
- **Closed by**: `f6adb68b plugin-db/context: privatize IsolateDbContext fields (I16)`
- All 11 data fields (pool, db_url, registered_models, tx_conn, auto_tx_owned, tx_token, tx_token_counter, pending_emits, mig_lock, running_consumers, backend) demoted from `pub(crate)` to private. api-surface r11 (the round that returned with this commit landed) confirmed zero direct-field consumers outside `context.rs` at HEAD; every external caller already used accessors. `tx_token_counter` mutation path now compile-time-enforced through `next_tx_token()`. Per api-surface r11's plateau math this was the +2 step to 96; the +1 to 92 had already landed via NEW-R10-1 (`bac64c0e`). Six `MigrationLock` fields stay `pub(crate)` since `set_mig_lock` constructs them from outside the impl — follow-up sweep could add a `MigrationLock::new(...)` ctor.
- **Pickable this cycle**: yes — but requires confirming no test crate reaches in (the `pub` modules `broker`, `query`, `v8_classes` allowance suggests external tests may touch fields).

---

### [I17] `dispatch_op` resolve closures need explicit type ascription (code-critique I2)
- **Source**: `plugin-db-code-critique-2026-05-21.md` §I2
- **File**: `crates/plugin-db/src/crud.rs:472, 431, 551`
- **Description**: `Resolve: FnOnce(R) -> ResolveValue` and `EFut: Future<Output = Result<R, _>>` are independent generics; the inferer can't tie `R` between them, so each call site annotates the closure argument (`|n: i64| ...`, `|json: String| ...`). Footgun for future ops. Note: now that `R = Vec<Value>` on read/write paths (post `cc7fff89`), only the count op uses `i64` — the friction is lower than at the time of the review.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Partial — the type signatures are unchanged, but the actual closures may have simplified after `cc7fff89`. Re-spot-check needed.
  - Blocker: none — split into `run_value_op` / `run_count_op` (R-typed wrappers).
  - Already-superseded-by: partial — `cc7fff89` reduced the number of distinct `R` types in use.
- **Effort**: small (one helper split)
- **Pickable this cycle**: yes — but low leverage.

---

### [I18] `unwrap()` on V8 allocation primitives (code-critique I4)
- **Source**: `plugin-db-code-critique-2026-05-21.md` §I4; `plugin-db-code-critique-2026-05-22-r2.md` §"unwrap() ~35"
- **File**: `crates/plugin-db/src/v8_bridge.rs:124,277,302`; `orchestrator/register_model/mod.rs:74`; `orchestrator/transaction.rs:55,67`; `orchestrator/auto_tx.rs:262-268`; `v8_classes/transaction.rs:303`
- **Description**: `v8::PromiseResolver::new`, `v8::String::new`, `v8::Function::new` all return `Option` and fail under OOM / terminating-isolate conditions. Today those panic with an opaque message. R2 confirms ~35 such sites in production code, almost all V8 boilerplate.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes (~35 sites; `register_model/mod.rs:74` shown above).
  - Blocker: needs a central helper `try_make_resolver(scope) -> Result<…, OpError>` and a clippy lint cascade.
  - Already-superseded-by: N/A
- **Effort**: medium (helper + global sweep)
- **Pickable this cycle**: no — large surface; cheaper to wait for a real OOM incident or to land the helper in a coordinated sweep.

---

### [I20] WAL replication: cross-tenant isolation is Rust-only (security)
- **Source**: `plugin-db-security-2026-05-22-r1.md` §2 "WAL replication credentials scope"; `plugin-db-architecture-review-2026-05-22-r3.md` §5
- **File**: `crates/plugin-db/src/wal_consumer.rs:361`, `crates/plugin-db/src/replication_ops.rs:199`
- **Description**: WAL consumer enforces `(rel.namespace != self.app_id)` in Rust; no Postgres-side per-app role separation. All consumers share one Postgres role with visibility into all published schemas. A relation-cache race or a future code path passing the wrong app_id could leak cross-tenant data.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes.
  - Blocker: design — proposal §R5-R8 (SECURITY DEFINER slot ownership) deferred to P8c.
  - Already-superseded-by: N/A
- **Effort**: large (per-app Postgres roles, slot ownership)
- **Pickable this cycle**: no — explicit P8c deferral.

---

### [I22] `mint_*` Box leak risk on isolate teardown — 7 copies (code-critique I3)
- **Source**: `plugin-db-code-critique-2026-05-21.md` §I3
- **File**: `crates/plugin-db/src/v8_classes/{db,collection,transaction,migration,migrations,replication,subscription}.rs`
- **Description**: Each wrapper uses `Box::into_raw` + V8 Weak finalizer. V8 docs warn finalizers may not run on isolate dispose. Each becomes a permanent leak on shutdown; long-running workers that recycle isolates accumulate leaks. Same unsafe pattern copy-pasted 7 times — a regression in one site is invisible in the others.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes — 7 sites confirmed by `cargo doc` and grep history.
  - Blocker: design — needs an `install_wrapper<T>(scope, obj, state: Box<T>)` helper that owns `Box::into_raw` + finalizer wiring once.
  - Already-superseded-by: N/A
- **Effort**: medium (factor a helper + apply to 7 sites)
- **Pickable this cycle**: no — needs a careful helper design; risk of correctness regression across all v8_classes.

---

### ~~[I23] `unmark` no-op when `return_mig_client` slot is empty masks state bugs (code-critique I7)~~ — CLOSED cycle 10:47
- **Closed by**: `5d9acab8 plugin-db/context: surface mig_lock state drift via tracing (I23)`.
- `set_mig_lock` now logs at `error` with prev/new lock identity if the slot was occupied (shadow), then proceeds with `replace` so the worker stays recoverable. `return_mig_client` now logs at `warn` (with rationale "expected only on operator-cancel race") instead of silently dropping the client. Same operator-visibility intent as the deferred entry's debug_assert proposal; converted to tracing so the slot's unit tests (which deliberately exercise the swap-on-replace shape with `MigrationLock { client: None }`) keep passing.

---

### ~~[I25] `OBJECT_PREFIX` in `LIKE` predicate is format-string interpolated (security MINOR; replication M2)~~ — CLOSED cycle 11:17 (verification: already closed at `c0590506`)
- **Closed by**: `c0590506 plugin-db/v8_classes/replication: scope watchdog + dropAbandoned to self.app_id (CRITICAL)` — the cross-app scoping fix also converted both sites to `slot_name LIKE $1` parameter binds.
- Verification (cycle 11:17): `grep -n "LIKE '" crates/plugin-db/src/replication.rs` returns only two docstring references (lines 25 and 57); no `format!(... LIKE '{OBJECT_PREFIX}...')` remains in code. The `LIKE $1` form at the four real SQL sites (lines 413, 547, plus the watchdog/dropAbandoned CTEs) binds the per-app prefix safely. The two remaining `LIKE '__zs_%'` literals in `auth/bootstrap.rs:891,953` live inside SECURITY DEFINER CREATE FUNCTION bodies (different security model — server-side SQL, not Rust string interpolation) and only compile under `--features hardening` post-`2fa9472e`.

---

### [I31] Migration-pipeline: orphan `Running` DDL audit rows have no heartbeat/sweeper (migration-pipeline r2 F1) — WARN-HALF CLOSED cycle 12:17
- **Source**: `plugin-db-migration-pipeline-2026-05-22-r2.md` §F1
- **File**: `crates/plugin-db/src/orchestrator/register_model/apply.rs:160-190`; `crates/plugin-db/src/backend/postgres.rs` (3 retry sites)
- **Description**: Worker dying between DDL completion and audit terminal-update leaves the row in `Running` forever. No `owner_session_id`/heartbeat on DDL rows, no sweeper to terminalise abandoned entries.
- **Warn-half closed (cycle 12:17)**: `fcf7ce3c plugin-db: warn on audit-status update failure (F1 warn-half)` — all 5 `let _ = update_audit_status(...)` sites converted to `if let Err(e) = ...` + structured `tracing::warn!` with app_id / audit_id / attempt / inner-error. The DDL error still propagates via `result` / `refuse(...)` so JS sees the failure; the warn surfaces the audit-write secondary failure operators can grep for. Closes the observability half of F1.
- **Remainder**: full F1 still open — needs design for sweeper cadence + `owner_session_id`/heartbeat column + watchdog policy. The warn logs are observability-only; a stuck `Running` row still requires an operator-driven reset until the sweeper lands.
- **Effort (remaining)**: medium-large (needs new schema column + background task)
- **Pickable this cycle**: no — design decision still required for the sweeper.

---

### [I32] Migration-pipeline: orphan `Pending` audit rows never reach terminal state (migration-pipeline r2 F2)
- **Source**: `plugin-db-migration-pipeline-2026-05-22-r2.md` §F2
- **File**: `crates/plugin-db/src/orchestrator/register_model/validate.rs:74-85` (writes Pending); `apply.rs` (skips destructive ops without terminalising)
- **Description**: Strict and lenient paths both leave `Pending` rows stranded. Operator queries on `status='pending'` see phantoms.
- **Status as of 2026-05-22 01:10**: actionable but needs design for the cancellation/refusal flow.
- **Effort**: medium
- **Pickable this cycle**: no — paired with [I31].

---

### ~~[I35] row_to_json O(N²) per row in column count~~ — CLOSED cycle 13:17 + MEASURED cycle 14:17
- **Closed by**: `251d53b4 plugin-db/v8_bridge: row_to_json O(N²) → O(N) via index lookup (I35)`
- **Measured by**: `bf75e866` (compio-postgres test-utils) + `75d9ae5c` (bench_row_to_json) — see header table for narrow/medium/wide deltas. Wide-row (50-col) win: **-13.0%** (8.95 µs → 7.76 µs, p < 0.05). Narrow (3-col): noise. Medium (10-col): -4.3%. The change is real but smaller than the pre-measurement guess — JSON decode + serde dominate the wide-row budget; the per-column linear-scan was second-order. Affects every CRUD read path.

---

### [I43] bootstrap.rs still uses blocking pg_advisory_lock (security r4 IMPORTANT)
- **Source**: `plugin-db-security-2026-05-22-r4.md` §"sharpened IMPORTANT"
- **File**: `crates/plugin-db/src/orchestrator/register_model/bootstrap.rs:107`
- **Description**: Bootstrap uses blocking `pg_advisory_lock` with no try-with-deadline / per-app cap. `migrations.rs` uses `try_acquire_advisory_lock`. The cbd12944 RAII refactor could have unified on the try-pattern but didn't — cross-tenant pool starvation risk remains: one app blocked on its lock holds a connection that other apps can't reach.
- **Status as of 2026-05-22 03:25**: design decision needed — non-blocking with retry/backoff vs blocking with cap. Either is a meaningful semantic change.
- **Effort**: small (mechanical swap to try-acquire) but requires a retry/backoff policy decision.
- **Pickable this cycle**: rolled forward; needs design input.

---

## SUPERSEDED (already fixed; remove next cycle)

### [S88] IMPORTANT (performance r4 N4-I3; cycle 13:17) — [I35] row_to_json O(N²) per row in column count
- **Closed by**: `251d53b4 plugin-db/v8_bridge: row_to_json O(N²) → O(N) via index lookup (I35)`
- Switched `row_to_json` to enumerate `(idx, col)` and thread `idx: usize` into `column_to_json` (renamed from `name: &str`). All 9 internal `try_get` / `raw_value` call sites updated. compio-postgres's `RowIndex for usize` is O(1) bounds-check; `RowIndex for str` is linear-scan with case-insensitive retry. Affects every CRUD read path. No bench delta this commit (no forcing function per perf r11). 352 lib tests pass.

### [S87] IMPORTANT (error-ux r10 + code-critique r11 MINOR-R11-1; cycle 12:47) — F1 warn-shape unification + finalise_backfill name/collection
- **Closed by**: `7c6bd2ec plugin-db: unify F1 warn-half shape + add name/collection to finalise_backfill warn`
- Pinned the F1 warn shape across 6 sites (5 from `fcf7ce3c` + the cycle-06:00 finalise_backfill warn) to identical field names: `app_id = %app_id`, `audit_id`, `transition = "..."`, `audit_err = %audit_err`. Added `name` + `collection` to the finalise_backfill warn (6th-cycle error-ux carry — they were already in lexical scope via `lock_snapshot()`).

### [S86] IMPORTANT (docs-audit r7 NEW × 2; cycle 12:17) — auth/mod.rs + lib.rs preambles after hardening cargo gate
- **Closed by**: `71a457a1 plugin-db: doc-drift cleanup after hardening cargo gate (docs-audit r7)`
- Two preambles missed when cycle-10:47's `2fa9472e` landed the `hardening` Cargo feature. (a) `auth/mod.rs` "Backwards compatibility" section described `--harden` CLI flag as the opt-in; rewritten to describe the cfg-gate as the actual compile-time mechanism and note `--harden` as one possible runtime opt-in once the module ships. (b) `lib.rs` module-visibility note explained the `test-helpers` cfg-fork but omitted the three-arm `hardening` ladder; now documents the cross-product + updates the `required-features` reference. Both NEW IMPORTANTs from docs-audit r7 closed.

### [S85] HIGH (migration-pipeline r2 §F1; cycle 12:17) — F1 warn-half (observability)
- **Closed by**: `fcf7ce3c plugin-db: warn on audit-status update failure (F1 warn-half)`
- All 5 `let _ = update_audit_status(...).await;` sites converted to `if let Err(e) = ...` + structured `tracing::warn!` with `app_id` / `audit_id` / `attempt` / inner-error context. Sites: `apply.rs:163-170` (Applied terminal), `apply.rs:178-186` (Failed terminal on DDL Err), `backend/postgres.rs` (INVALID-index loop), `backend/postgres.rs` (data-violation retry), `backend/postgres.rs` (transient/non-transient index build failure). DDL errors still propagate via `result` / `refuse(...)`; retry semantics unchanged. The warn surfaces audit-write secondary failures operators can grep for. **Sweeper-half remains open at [I31]** — needs design for owner_session_id/heartbeat column + watchdog policy.

### [S84] IMPORTANT (code-critique I-NEW-2 + r1 I6; cycle 12:17) — [I6] release_advisory_lock silent error swallow
- **Closed by**: `51c342e8 plugin-db/backend: release_advisory_lock returns Result (I6)`
- Trait method now returns `Result<(), DbError>` (was `()`); Postgres impl propagates `compio_postgres::Error` via `DbError::from_pg`. Cycle-12:17 audit corrected the deferred entry's "no production callers" claim — two callers exist in `migrations.rs` (cancelled-refusal path at :288, backfill-finalise path at :651). Both updated to `tracing::warn!` on Err and continue; the lock auto-releases on session end so this stays observability-only.

### [S82] HIGH (test-coverage r2 GAP-2 + r11 carry; cycle 11:17) — [I13] queue_or_emit / drain / clear unit-test gap
- **Closed by**: `ae5570dc plugin-db/exec: unit tests for queue_or_emit / drain / clear (I13)`
- Three new unit tests cover the autocommit-emit, the COMMIT drain (with second-drain no-op assertion), and the ROLLBACK clear paths. The 4th branch (in-tx queue) needs a `compio_postgres::Client` which isn't constructible in unit tests; covered by `gap_b_subscriber_does_not_observe_pre_commit_state` in `tests/integration.rs`. 352 lib tests pass (was 349).

### [S83] IMPORTANT (code-critique r10 + api-surface r9 NEW-R9-1/NEW-R9-2; cycle 11:17) — 3 doc/visibility cleanups
- **Closed by**: `4cab871a plugin-db: 3 doc/visibility cleanups from cycle 11:17 reviewers`
- (a) `replication::slot_status` demoted `pub` → `pub(crate)`; no production caller, prior docstring referenced a "V8 `replicationStatus` callback" that doesn't exist (api-surface r9 NEW-R9-1).
- (b) `context.rs:404` docstring on `return_mig_client` referenced "`set_mig_lock` debug_asserts above" — but cycle 10:47's `5d9acab8` explicitly used `tracing::error!` instead (code-critique r10 + api-surface r9 NEW-R9-2). Updated to cite the actual tracing branch.
- (c) `error.rs` §2 preamble framed `hex_decode`/`hex_nibble` as "internal to auth/session.rs" — but the whole subtree is cfg-gated behind `--features hardening` post-`2fa9472e` (code-critique r10 IMPORTANT). Added a one-clause note.

### [S80] MINOR (security r1 + test-coverage r2 GAP-1; cycle 11:17) — [I12] validate_field_name unicode aliasing
- **Closed by**: `403b3891 plugin-db/query: validate_field_name rejects non-ASCII (I12)`
- Tightened `validate_field_name` to ASCII alphanumeric + underscore (same allowlist as `validate_collection`); two new unit tests pin both the rejection set (café / naïve / 日本 / em-dash / space) and the positive ASCII shape. Eliminates a class of byte-truncation aliasing where two unicode-spelled fields could collide on the same Postgres column after the 63-byte NAMEDATALEN truncation. Net 349 lib tests pass (was 347, +2 new).

### [S81] MINOR (security r1 + migration-pipeline r1 M2; cycle 11:17) — [I25] OBJECT_PREFIX LIKE format-interpolation, retro-closed
- **Closed by**: `c0590506 plugin-db/v8_classes/replication: scope watchdog + dropAbandoned to self.app_id (CRITICAL)` (which also did [S48] cycle 04:35)
- Cycle-11:17 backlog audit found this entry stale: the cross-app scoping fix at `c0590506` ALREADY converted both `replication.rs` LIKE sites to `slot_name LIKE $1` parameter binds. The deferred entry's quoted format-string is no longer present in `replication.rs`. Remaining `LIKE '__zs_%'` literals in `auth/bootstrap.rs:891,953` live in SECURITY DEFINER CREATE FUNCTION bodies (server-side SQL, not Rust interpolation) and only compile under `--features hardening` post-`2fa9472e`. No new commit needed.

### [S78] MAJOR (code-critique r9 MAJOR-R9-5; cycle 10:47) — auth/* subtree pollutes default builds with 58 dead-code warnings
- **Closed by**: `2fa9472e plugin-db/auth: gate dormant auth subtree behind hardening feature`
- Added a `hardening = []` Cargo feature; the entire `auth/*` subtree (`crates/plugin-db/src/auth/{mod,bootstrap,keys,session}.rs`, ~2,860 LOC, ~76 KB) is now `#[cfg(all(feature = "hardening", …))]`-gated. Default `cargo build -p zeroship-plugin-db --lib` drops from 74 → 12 plugin-db warnings (58% reduction in noise floor per architecture r10). Integration tests still probe the auth surface — `[[test]] integration` now requires `["test-helpers", "hardening"]`. Security r9 verified zero in-code production callers of `crate::auth::` (only doc-comment refs + integration tests); credited +1 to score. The eventual control-plane wire-up (per the auth-r1 design) flips the feature on.

### [S79] IMPORTANT [I23] (code-critique r9 I7; cycle 10:47) — return_mig_client silent no-op + set_mig_lock missing state-machine assertions
- **Closed by**: `5d9acab8 plugin-db/context: surface mig_lock state drift via tracing (I23)`
- `set_mig_lock` now `tracing::error!`s with prev/new lock identity on a shadow-replace (the begin path should have gated on `has_mig_lock`); release builds still proceed with `replace` to keep workers recoverable rather than panicking. `return_mig_client` switched from a silent `if let Some` to a `match` with `tracing::warn!` on the empty-slot arm (expected only on operator-cancel race). Same operator-visibility intent as the deferred entry's debug_assert proposal; converted to tracing so the slot's unit tests (which deliberately exercise the swap-on-replace shape with `MigrationLock { client: None }`) keep passing. 347 lib tests pass.

### [S1] CRITICAL C1 — `audit.rs` returns `Result<_, String>` (api-surface)
- **Source**: `plugin-db-api-surface-2026-05-22-r1.md` §C1
- **Fixed by**: `f1c475f5 plugin-db/audit: convert helpers to Result<_, DbError> + async_fn_in_trait`, `0816feb0 plugin-db/error: add impl From<compio_postgres::Error> for DbError`. Audit module now declares the `Result<_, DbError>` contract in its preamble; `audit.rs:192-792` all return `Result<_, DbError>`.

### [S2] CRITICAL C2 — `Backend::create_index_with_recovery` returned `Result<(), String>` (api-surface, R3 I3, code-critique R2 M4)
- **Source**: `plugin-db-api-surface-2026-05-22-r1.md` §C2; `plugin-db-architecture-review-2026-05-22-r3.md` §4 I3; `plugin-db-code-critique-2026-05-22-r2.md` §I-NEW-3
- **Fixed by**: `ff220fce plugin-db/backend: create_index_with_recovery returns Result<(), DbError> (last trait outlier); serde_json-based envelope`. Verified at `backend/mod.rs:348-355`: signature is `async fn create_index_with_recovery(...) -> Result<(), DbError>`. The `serde_json-based envelope` half closes the I-NEW-3 hand-rolled JSON escaping hazard simultaneously.

### [S3] CRITICAL C1 (migration-pipeline) — replication schema/publication case mismatch
- **Source**: `plugin-db-migration-pipeline-2026-05-22-r1.md` §3 C1
- **Fixed by**: `a00c41fd plugin-db/replication: fix schema/publication case mismatch (CRITICAL C1 silent WAL delivery failure)`. Adds `publication_sql_uses_quoted_original_case_schema` regression test; uses `quote_ident(app_id)` for schema reference.

### [S4] CRITICAL C1 (code-critique) — advisory lock leak on Pass-1 failure
- **Source**: `plugin-db-code-critique-2026-05-21.md` §C1; `plugin-db-architecture-review-2026-05-22-r3.md` §3
- **Fixed by**: `37a0ef76 plugin-db/orchestrator/register_model: always unlock advisory lock on Pass-1 error`; subsequent extension `b4e533e2 plugin-db/orchestrator/register_model: release advisory lock on plan/validate error` covers the plan/validate-stage path too.

### [S5] CRITICAL C2 (code-critique) — `eprintln!` in connection-task error paths
- **Source**: `plugin-db-code-critique-2026-05-21.md` §C2
- **Fixed by**: `094261e1 plugin-db: route connection-task errors through tracing (was eprintln)`. Grep confirms zero `eprintln!` calls in `src/`.

### [S6] CRITICAL C3 (code-critique) — audit-row failure paths drop SQLSTATE
- **Source**: `plugin-db-code-critique-2026-05-21.md` §C3
- **Fixed by**: `10fe0b82 plugin-db: stop erasing DbError SQLSTATE on audit-row log paths` + `audit.rs` migration in `f1c475f5`. `audit.rs:55-71` `coded_sql` preserves variant + SQLSTATE.

### [S7] IMPORTANT I-NEW-1 — `insert_backfill_running` silent id=0
- **Source**: `plugin-db-code-critique-2026-05-22-r2.md` §I-NEW-1
- **Fixed by**: `d7cfc089 plugin-db/audit: insert_backfill_running returns Internal error on empty RETURNING (silent id=0 bug)`. Verified at `audit.rs:613-618`: `ok_or_else(|| DbError::Internal { … })`. Regression test at `audit.rs:882`.

### [S8] CRITICAL perf N-C1 — WAL consumer allocates HashMaps before subscriber check
- **Source**: `plugin-db-performance-2026-05-22-r2.md` §3 N-C1
- **Fixed by**: `78a95d3b plugin-db/broker: has_subscribers fast-path predicate` + `967a7362 plugin-db/wal_consumer: early-return in emit_for_tuple when no subscribers (perf CRITICAL N-C1)`. `wal_consumer.rs:548` gates on `broker::has_subscribers` before `tuple_to_map`.

### [S9] CRITICAL perf C1 (partial) / C2 — serde round-trip on read + mutation
- **Source**: `plugin-db-performance-2026-05-22-r1.md` §2 C1+C2
- **Fixed by**: `cc7fff89 plugin-db: thread Vec<Value> end-to-end (drop serde round-trip)`. `exec_query` / `exec_mutation` return `Vec<Value>`; `exec_mutation_with_emit` iterates the live `Value`s without re-parsing (`exec.rs:148+`). Residual structural cost remaining is tracked as [C3] above (not the same finding — most of perf C1's 4-parse chain is now 2).

### [S10] IMPORTANT perf I1/I2 — broker `publish` Vec alloc + per-subscriber HashMap clone
- **Source**: `plugin-db-performance-2026-05-22-r1.md` §3 I1+I2
- **Fixed by**: `c54a9f15 plugin-db/broker: share ChangeEvent payload via Rc to drop per-subscriber clone`. `broker.rs:439-462` uses `subs.retain` + `Rc::new(event.clone())` once before fan-out.

### [S11] IMPORTANT perf I4 — `AuditExecutor` boxes every future
- **Source**: `plugin-db-performance-2026-05-22-r1.md` §3 I4
- **Fixed by**: `f1c475f5 plugin-db/audit: convert helpers to Result<_, DbError> + async_fn_in_trait`. `audit.rs:431-437` uses `#[allow(async_fn_in_trait)] async fn query_text`. No `Pin<Box<dyn Future>>` in the trait. Trait is `pub(crate)`.
- Residual: trait return type is still `Result<Vec<Row>, compio_postgres::Error>` rather than `Result<Vec<Row>, DbError>` (R3 M3) — but boxing is gone. Promote the residual to MINOR if it surfaces in a future review.

### [S12] API-surface I2 — internal modules over-exported
- **Source**: `plugin-db-api-surface-2026-05-22-r1.md` §I2
- **Fixed by**: `2fe9e9f0 plugin-db: demote internal modules from pub to pub(crate) (api-surface I2)` + `5be3c1a1 plugin-db/lib: re-promote broker + v8_classes to pub for external test crates`. Final state in `lib.rs:33-55`: only `broker`, `error`, `query`, `v8_classes` are `pub`; rest are `pub(crate)`. Comments explain each `pub` exception names the external test file pinning visibility.

### [S13] API-surface I5 — `_pub`-suffixed `#[doc(hidden)] pub fn` helpers in query.rs
- **Source**: `plugin-db-api-surface-2026-05-22-r1.md` §I5
- **Fixed by**: `b2496364 plugin-db/query: rename and demote _pub-suffixed doc-hidden helpers`. `query.rs:380` shows `pub(crate) fn normalize_fk_action`; `value_to_param` similar. `build_create_table_with_fks` is still `pub` (line 184) — externally-visible because used by tests; that's a separate exception.

### [S14] API-surface I6 — `migrations.rs` production functions are `pub`
- **Source**: `plugin-db-api-surface-2026-05-22-r1.md` §I6
- **Fixed by**: `2fe9e9f0`. Verified: `exec_begin`, `exec_fetch_batch`, `exec_commit_batch`, `exec_status`, `exec_cancel`, `exec_reset`, `release_active_lock` are now `pub(crate)` (e.g. `migrations.rs:185`, `:332`, `:417`, `:615`, `:656`, `:689`, `:713`). The `*_with_pool` wrappers (lines 732-820) remain `#[cfg(any(test, feature="test-helpers"))]` and correctly `pub`.

### [S15] MINOR M3 — `DbError` lacks `#[non_exhaustive]`
- **Source**: `plugin-db-api-surface-2026-05-22-r1.md` §M3
- **Fixed by**: confirmed at `error.rs:40` — `#[non_exhaustive]` is present.

### [S16] IMPORTANT (R2 I3) — `replication_ops` string rail + register_model envelope
- **Source**: `plugin-db-architecture-review-2026-05-21-round2.md` §4 I3
- **Fixed by**: `a0fec06a plugin-db/replication_ops: route 9 dispatch sites through DbError::to_op_error (drop into_string)` for replication_ops; `b94fbdeb plugin-db/orchestrator/register_model: pipeline returns Result<(), DbError> end-to-end` for register_model.
- Verified: `replication_ops.rs:71,82-86,103,116-120,138,153-157,201-206,213-219,229-237` all route through `DbError::*.to_op_error()`. `orchestrator/register_model/mod.rs:108-258` is end-to-end `Result<(), DbError>`.

### [S17] IMPORTANT (R3 I1) — register_model `Result<(), String>` signatures
- **Source**: `plugin-db-architecture-review-2026-05-22-r3.md` §4 I1
- **Fixed by**: `b94fbdeb`. Confirmed `exec_register_model` (line 108), `run_pipeline` (line 159), `exec_register_model_with_pool` (line 246) all `-> Result<(), DbError>`. Dispatch site (line 97) uses `e.to_op_error()`.

### [S18] IMPORTANT — `SchemaRefused` lacked `.code` on JS exception
- **Source**: `plugin-db-error-ux-2026-05-22-r1.md` §4a
- **Fixed by**: `d2aeada6 plugin-db/error: stamp .code on SchemaRefused (SDK can now branch on validation_refused)`. Verify against the SDK's `mapNativeError` next cycle.

### [S19] Code-critique R1 — `Result<_, String>` rail still pervasive (M1)
- **Source**: `plugin-db-code-critique-2026-05-21.md` §M1
- **Fixed by**: end-to-end migration across `audit.rs` (`f1c475f5`), `migrations.rs`+pipeline (`b94fbdeb`), `replication_ops.rs` (`a0fec06a`). R2 of code-critique confirms zero `Result<_, String>` in production code paths.

### [S20] Code-critique R1 I5 — TIMESTAMP/date arithmetic can overflow
- **Source**: `plugin-db-code-critique-2026-05-21.md` §I5
- **Fixed by**: `81345420 plugin-db/v8_bridge: checked_* TIMESTAMP arithmetic (infinity-safe)`. R2 of code-critique confirms `checked_div`/`checked_add` at `v8_bridge.rs:406,429-430` returning `Value::Null` on overflow.

### [S21] Test-coverage R1 (p8a2 hang) — replication slot accumulation in tests
- **Source**: `plugin-db-test-coverage-2026-05-22-r2.md` §"What Got Better"
- **Fixed by**: `2b4aff4b plugin-db/tests: explicit ROLLBACK/unlock-all on test-only client teardown (fixes p8a2 ordering hang)` + `52ff1c83 plugin-db/tests: defensive global sweep of __zs_* slots + publications in c1_cleanup`.

### [S22] Security IMPORTANT #1 — `validate_collection` reserved-prefix + length
- **Source**: `plugin-db-security-2026-05-22-r1.md` §2 IMPORTANT first item
- **Fixed by**: `d27ea71e plugin-db/query: reject __zeroship_*, pg_*, and >63-byte collection + field names (security IMPORTANT #1)`. `query.rs:61-121` confirms 5 rejection arms + null-byte + `validate_field_name`. 11 unit tests at `query.rs:4163-4277`.

### [S23] Quality — `cargo doc` warnings
- **Source**: `plugin-db-test-coverage-2026-05-22-r2.md` §"What Got Better"
- **Fixed by**: `29b8a013 plugin-db: fix all rustdoc warnings (quality-evaluator #3)`.

### [S24] DbError completeness — `From<compio_postgres::Error>`, `From<QueryError>`, retry-hint contract
- **Source**: `plugin-db-test-coverage-2026-05-22-r2.md` §2d
- **Fixed by**: `0816feb0 plugin-db/error: add impl From<compio_postgres::Error> for DbError` + `de01b3a0 plugin-db/error: cover SQL-violation variants + retry-hint contract + From<QueryError>` + `e8463ef0 plugin-db/error: impl Display + std::error::Error for DbError; integration tests use {to_string}`.

### [S25] Quality — backend trait Debug opacity + compile-time trait-shape assertions
- **Source**: implicit guard added during the fix-wave
- **Fixed by**: `a3561ae4 plugin-db/backend: compile-time trait-shape assertions + Debug opacity guard`.

### [S26] Quality — gate doc-hidden test helpers behind cfg(test)
- **Source**: `plugin-db-code-critique-2026-05-21.md` §I8
- **Fixed by**: `852990d8 plugin-db: gate doc-hidden test helpers behind cfg(test)`. `lib.rs:162,190,212,249,264,273,281` all `#[cfg(any(test, feature = "test-helpers"))]`.

### [S27] State — IsolateDbContext unit tests
- **Source**: `plugin-db-code-critique-2026-05-21.md` §M2 (partial — broker / wal_consumer / read_set still carry their own thread-locals)
- **Fixed by**: `d03c8c08 plugin-db/context: unit tests covering tx lifecycle + mig lock + pending emits + consumers` (34 tests, per test-coverage R2).

### [S28a] IMPORTANT [I1] (this cycle) — `audit_bootstrap_failed` discards DbError variant code
- **Closed by**: `ed697c45 plugin-db/migrations: preserve typed DbError variants at 4 audit-write sites` + `b63d0e4b plugin-db/migrations: fix _pub-rename leak from cherry-pick`
- All four sites at `migrations.rs:253,646,687,720` now route through the new `map_audit_bootstrap_err()` helper, preserving SQLSTATE-coded variants (Transient, LockNotAvailable, etc.) and only wrapping `Internal{}` with the operator-facing prefix. 3 new unit tests guard the discipline.

### [S28b] CRITICAL (concurrency r3 NEW; this cycle) — bootstrap.rs advisory-lock leak on error paths
- **Source**: `plugin-db-concurrency-2026-05-22-r3.md` §"CRITICAL"
- **Closed by**: `3bb41fa1 plugin-db/bootstrap: release advisory lock on error paths`
- Function `bootstrap()` acquired a session-scoped advisory lock then ran `ensure_app_schema`, `ensure_audit_table`, `next_schema_version`, `build_create_indexes`, `build_named_indexes` via `?`-propagation. Failures dropped the `PooledClient` back into the pool with the lock still held — cross-app stall on next caller. Now wrapped in an inner async block that explicitly issues `pg_advisory_unlock` on error before dropping. Mirrors `apply.rs` Pass-1 pattern (`37a0ef76`) and `run_pipeline` plan/validate (`b4e533e2`).

### [S28c] CRITICAL × 2 (security r2 NEW; this cycle) — cross-app `appId` override in v8_classes/replication
- **Source**: `plugin-db-security-2026-05-22-r2.md` §"CRITICAL"
- **Closed by**: `309ed52f plugin-db/v8_classes: drop cross-app appId override (CRITICAL security)`
- `Replication::setup({appId})` and `Db::startReplicationConsumer(opts)` accepted JS-supplied `appId` overrides with NO authorization check. App A could provision replication slots/publications for any victim app and hijack the WAL stream. Override dropped — `self.app_id` (stamped at mint time from isolate context) is now the only allowed scope. 7 new unit tests assert the override is ignored. Operator-provisioning belongs in `crates/control/`, not the runtime.

### [S28d] CRITICAL (test-coverage r3 NEW; this cycle) — `cargo build --tests --features test-helpers` failed with 118 E0603 errors
- **Source**: `plugin-db-test-coverage-2026-05-22-r3.md` §"Critical findings"; `plugin-db-api-surface-2026-05-22-r2.md` §"CRITICAL"
- **Closed by**: `90d992d5 plugin-db/lib: cfg-gate module visibility on test-helpers feature`
- The api-surface r1 demotion (`2fe9e9f0`) over-reached: 8 modules consumed by `tests/integration.rs` (audit, auth, exec, migrations, orchestrator, replication, replication_ops, wal_consumer) were left `pub(crate)`, breaking the entire 4400-line integration suite. Cfg-fork the visibility on the `test-helpers` feature — `pub(crate)` in release, `pub` for tests. The release surface stays tight; the integration tests build clean again.

### [S29] IMPORTANT [I19] (cycle 01:10) — apply::run_op silent no-op on DropColumn/DropIndex
- **Closed by**: `3ef6a170 plugin-db/apply: hard-error on DropColumn/DropIndex outside destructive class`
- Latent silent-Ok arm replaced with `Err(destructive_invariant_error(op))`. Invariant check now gates the audit-row write — no orphan `Running` row on contract violation. 3 unit tests cover the error path, the canonical destructive-skip, and a 15-case sweep across all (kind × class) combinations confirming the gate doesn't interfere with other paths.

### [S30] CRITICAL (perf r3 N3-C1; cycle 01:10) — exec_mutation_with_emit built tuple before subscriber check
- **Closed by**: `49b0b98e plugin-db/exec: gate exec_mutation_with_emit tuple build behind subscriber check`
- Mutation path built `(columns, tuple)` per row before checking `is_app_suppressed` or `has_subscribers`. With WAL consumer active this was unconditionally discarded. Refactored into `emit_for_rows` helper with the gate at the top; 3 unit tests verify suppressed / no-subscriber / active-subscriber paths.

### [S31] IMPORTANT [I11] (cycle 01:10) — stale TX_CONN/callbacks.rs docs
- **Closed by**: `d53f90b0 plugin-db/docs: scrub stale TX_CONN / callbacks.rs references`
- 6 doc sites annotated or rewritten; `crud.rs`/`diff.rs` preambles already present (closed previously by `29b8a013`). Note: 9 stale TX_CONN refs remain in `transaction.rs:108,116,161,192,214,284,285`, `crud.rs:53`, `exec.rs:277`, `orchestrator/transaction.rs:143` — see new backlog entry [I26].

### [S32] IMPORTANT [I29] (cycle 01:35) — replication.rs empty-RETURNING silent default
- **Closed by**: `c83d6a8c plugin-db/replication: surface empty-RETURNING as DbError::Internal, not silent default`
- `ensure_publication_and_slot`'s `pg_create_logical_replication_slot` RETURNING was coerced via `unwrap_or_default()`; now uses `.ok_or_else(|| DbError::Internal { ... })?` mirroring `d7cfc089`'s audit.rs fix. 2 unit tests pin the wire shape (operation name + `replication:` log-scraper prefix + `no row` text).

### [S33] IMPORTANT [I34] (cycle 01:35) — broker.has_subscribers per-call (String, String) alloc
- **Closed by**: `0e58c4e8 plugin-db/broker: two-level HashMap eliminates per-call (String, String) alloc` + `b32ba383 plugin-db/broker: remove duplicate has_subscribers after cherry-pick`
- `Broker::by_key` refactored from `HashMap<(String, String), _>` to `HashMap<String, HashMap<String, _>>`. `publish`/`has_subscribers`/`drop_app` all now lookup via `&str` borrow — zero allocs on the WAL hot path. 7 new unit tests + drop_app collapses to O(1) `remove(app_id)`.

### [S35] IMPORTANT [I27] (cycle 02:05) — extract OrchestratorLockGuard RAII abstraction
- **Closed by**: `cbd12944 plugin-db/orchestrator: extract OrchestratorLockGuard RAII abstraction`
- New `crates/plugin-db/src/orchestrator/lock_guard.rs` (~254 lines + 5 unit tests). Three sites that open-coded the same explicit `pg_advisory_unlock` + drop sequence (bootstrap.rs, apply.rs, run_pipeline mod.rs) now thread an `OrchestratorLockGuard<'p>` instead. Internal `Option<PooledClient>` permits clean move-out via `release().await` (normal path) and `into_held()` (cross-scope hand-off). Drop is a `tracing::error!` fallback for catastrophic paths since Drop can't await. Also incidentally fixed: `bootstrap.rs`'s `ensure_app_schema` / `ensure_audit_table` failures now release the lock (the inner-async-block fix in `3bb41fa1` is replaced by the guard, broadening release coverage). Architect r4 + r5 top recommendation.

### [S36] IMPORTANT [I30] (cycle 02:05) — auto_tx flattened typed DbError to OpResult::Failed { String }
- **Closed by**: `8ff1b2de plugin-db/auto_tx: preserve typed DbError code through OpResult::Failed`
- `orchestrator/auto_tx.rs::auto_begin_transaction` and `auto_end_transaction` switched from `OpResult::Failed { error: String }` to `OpResult::JsValue { ... ResolveValue::RejectError(OpError) ... }` mirroring `transaction.rs`. JS surfaces now receive `e.code` and `e.hint` properties on COMMIT-path errors. 4 new unit tests. No cross-crate change needed — `setup_js_promise` already existed in the runtime.

### [S37] IMPORTANT [I36] (cycle 02:50) — query.rs per-CRUD ascii_lowercase alloc
- **Closed by**: `5ceb6daa plugin-db: drop per-CRUD lowercase alloc + tighten wal_consumer shim visibility`
- `validate_collection` previously allocated a fresh `String` to lower-case `name` for two prefix checks. Replaced with byte-slice `eq_ignore_ascii_case` against literal prefix bytes. Removes one alloc per CRUD dispatch.

### [S38] IMPORTANT [I38] (cycle 02:50) — wal_consumer.rs legacy shims demoted pub → pub(crate)
- **Closed by**: `5ceb6daa plugin-db: drop per-CRUD lowercase alloc + tighten wal_consumer shim visibility`
- The api-surface r3 finding's "dead code in release" claim was partially wrong — `local_emit_suppressed()` is called from `emit_change()` at line 223. So cfg-gating would have broken the build. Right fix: demote `pub` → `pub(crate)` on all three legacy shims (`any_app_suppressed`, `set_local_emit_suppressed`, `local_emit_suppressed`). Removes them from the release `pub` surface without breaking internal consumers.

### [S40] IMPORTANT [I40] (cycle 03:25) — subscription.rs broker entry leak on V8 alloc fail
- **Closed by**: `4cbe9fa1 plugin-db/v8_classes/subscription: defer broker subscribe until V8 alloc succeeds`
- Reordered `mint_subscription`: all fallible V8 ops (install, instance_template, new_instance, get_function, prototype get, set_prototype) now run BEFORE `broker::subscribe(...)`. Once subscribe lands, only infallible ops follow (Box::into_raw, External::new, set_internal_field, Weak::with_guaranteed_finalizer). Doc comment rewritten — was "exactly backwards" per the r4 reviewer. 2 unit tests: structural assertion (byte-offset ordering of `?` markers vs subscribe call) + happy-path subscription count.

### [S41] IMPORTANT [I41] (cycle 03:25) — update_backfill_progress race with operator reset
- **Closed by**: `37e61803 plugin-db/migrations: move update_backfill_progress BEFORE COMMIT`
- Moved the audit progress UPDATE BEFORE the COMMIT so the row lock acquired by `lock_audit_row_for_update` (FOR UPDATE) is still held. Operator `migrations.reset(...)` racing between data UPDATEs and progress write now blocks on the lock; reset can only land AFTER the new cursor commits atomically with the data. Switched the audit-update error path to `rollback_and_return` since we're now inside the transaction.

### [S42] IMPORTANT (api-surface r4 M1; cycle 03:25) — 5 mint_* helpers demoted pub → pub(crate)
- **Closed by**: `07205e54 plugin-db: demote 5 mint_* helpers + fix validate.rs SchemaRefused doc lie`
- The persistent r2→r3→r4 finding finally closed. `mint_collection`, `mint_migrations`, `mint_replication`, `mint_transaction`, `migration_start_with_spec` are all `pub(crate)` now; external test crates only need `mint_db` + `mint_subscription` (verified by grep).

### [S44] IMPORTANT [I28] (cycle 04:00) — Result<_, String> sweep in auth/* + replication.rs
- **Closed by**: `0049d9be plugin-db: sweep Result<_, String> sites in auth/* + replication.rs` + `91830cca plugin-db/replication: drop stale .into_string() after [I28] sweep`
- ~30 function signatures converted across `auth/bootstrap.rs`, `auth/keys.rs`, `auth/session.rs`, `replication.rs`, `diff.rs`. ~70 `.map_err(|e| format!(...))` sites converted to typed `DbError` variants (Transient, LockContention, Internal, Configuration, ValidationFailed). 4 dispatch boundary sites in `replication_ops.rs` no longer wrap as `DbError::Internal` — typed errors flow through. 3 P0001 RAISE messages in `init_session` promoted to typed `ValidationFailed { code: "session_signature_expired" | "session_nonce_replay" | "session_invalid_signature" }`. SDK can now branch on retryable codes for replication and auth failures. 10 new unit tests pin `.code` preservation. Site count 48→22 (remaining are intentional: trait sigs, wire-contract holdouts, internal pure decoders).

### [S73-S79] Backlog audit (cycle 10:30) — 7 IMPORTANTs verified closed and moved from open list
- **[I2] SchemaRefused .code**: Closed by `d2aeada6`. Verified in `error.rs::to_op_error` arm — `code: &'static str` stamped on SchemaRefused.
- **[I8] Cursor advance not atomic with COMMIT**: Duplicate of [I41] / F3. Closed by `37e61803 plugin-db/migrations: move update_backfill_progress BEFORE COMMIT` (cycle 03:25). Progress UPDATE now inside the BEGIN/COMMIT envelope under FOR UPDATE row lock.
- **[I9] running_consumers diverges on rapid teardown**: Closed by `386f9bf5` (lifecycle tests caught the underlying then_some bug) + `34d209b5` (mark inside guard) + `70921112` (atomic try_claim). `ConsumerRunningGuard::Drop` fires on every exit path including future-dropped-pre-poll.
- **[I21] Hand-rolled JSON in create_index_with_recovery_audited**: Closed by `ff220fce plugin-db/backend: ... serde_json-based envelope`. Verified `format!`-based JSON escaping is gone.
- **[I24] exec_mutation_with_emit redundant string clone**: Closed by `49b0b98e plugin-db/exec: gate exec_mutation_with_emit tuple build behind subscriber check`. `emit_for_rows` now early-returns if `is_app_suppressed || !has_subscribers`.
- **[I26] Stale TX_CONN/TX_TOKEN refs in 9 sites**: Closed by `09e32998` (7 files swept; 4 broken intra-doc links repaired). Remaining `TX_CONN` mentions in context.rs are explicit historical annotations.
- **[I33] update_backfill_progress race with reset**: Duplicate of [I41] / F3. Closed by `37e61803` — progress UPDATE moved inside the COMMIT envelope under the FOR UPDATE row lock.

### [S72] MAJOR (code-critique r8 R8-2; cycle 09:30) — init_pool_async code-name drift unified
- **Closed by**: `7d0bc4c5 plugin-db/exec: unify cold-init failure code to lazy_init_failed (R8-2)`
- Same underlying `init_pool_async()` failure was surfacing as two different SDK codes: `lazy_init_failed` (orchestrator/register_model) vs `not_configured` (exec.rs). Unified the exec.rs sites to `lazy_init_failed`. The remaining `not_configured` uses in exec.rs are for the distinct "pool not initialized" invariant breach.

### [S69] IMPORTANT (docs-audit r4/r5/r6 4-round hold-out; cycle 09:00) — TX_CONN/TX_TOKEN/MIG_LOCK drift sweep
- **Closed by**: `09e32998 plugin-db: scrub stale TX_CONN/TX_TOKEN/MIG_LOCK refs + demote OBJECT_PREFIX`
- ~20 inline doc/comment sites across 7 files (backend/mod.rs, crud.rs, exec.rs, lib.rs, orchestrator/transaction.rs, v8_classes/migration.rs, v8_classes/transaction.rs) referenced the retired thread-local names. Replaced with current `IsolateDbContext::tx_conn`/`tx_token`/`mig_lock` paths. 4 broken intra-doc links now resolve. Remaining mentions in context.rs are explicit historical-name annotations.

### [S70] MAJOR (api-surface r7 NEW MAJOR-R7-2; cycle 09:00) — OBJECT_PREFIX demote
- **Closed by**: `09e32998` + `bc4363f0 plugin-db/replication: demote OBJECT_PREFIX to pub(crate) (re-apply)`
- `replication::OBJECT_PREFIX` was `pub` with zero external consumers. Demoted to `pub(crate)`. Race with parallel TX_CONN fixer required a re-apply (bc4363f0).

### [S71] MAJOR (code-critique r8 R8-1; cycle 09:00) — error.rs preamble drift on Result<_, String> hold-outs
- **Closed by**: `9e392ba1 plugin-db/error: accurate enumeration of Result<_, String> hold-outs (R8-1)`
- Preamble claimed "saturated at 2 sites" but the actual count is 8 sites across 5 categories. Rewrote to accurately enumerate wire-contract envelopes, pure parsers, JS-input arg parsers, cold-init (with the "lazy_init_failed" vs "not_configured" code-name drift flagged as a follow-up), and test helpers.

### [S67] INFO (error-ux r7; cycle 08:30) — DbError::Configuration `hint` field
- **Closed by**: `f1c5184e plugin-db/error: add hint field to DbError::Configuration`
- Added `hint: Option<String>` to `Configuration` variant + new `DbError::config_hinted()` convenience constructor. `wal_level_not_logical` and `not_provisioned` (missing db_url) now ship operator-remediation prose in the `.hint` slot instead of baked into the message body.

### [S68] CRITICAL+IMPORTANT (docs-audit r6; cycle 08:30) — 3 doc drift sites
- **Closed by**: `3d79d2da plugin-db: docs-audit r6 fixes`
- migrations.rs:67-86 CRITICAL: deeefe18 left both the old + new coded_db preamble in place, contradicting each other. Collapsed to one coherent block.
- wal_consumer.rs:49 IMPORTANT: renamed obsolete `replicationConsumerStart` → `startReplicationConsumer`.
- error.rs:336-341 IMPORTANT: prefix_message preamble updated to include migrations::coded_db as the 7th consumer.

### [S64] MINOR (architecture r8 M11; cycle 08:00) — migrations::coded_db inline variant-walk
- **Closed by**: `deeefe18 plugin-db/migrations: route coded_db through the shared prefix_message`
- Last remaining inline copy of the variant-walk pattern after cbbc9059's dedup wave. `migrations::coded_db` now delegates to `crate::error::prefix_message` with the `"<context>: "` prefix; preserves double-prefix-avoidance rationale.

### [S65] MEDIUM (test-coverage r8 NEW; cycle 08:00) — classify_p0001_detail had zero direct unit tests
- **Closed by**: `f6043126 plugin-db: SQLSTATE-typed checks + classify_detail tests`
- Extracted pure DETAIL→(code, message) map into `classify_detail_token(&str)`; 7 new unit tests cover all 5 codes + unknown-token fall-through + codes-are-distinct invariant. SDK contract pinned at unit-test level.

### [S66] MINOR (error-ux r7 + code-critique r7 MIN-R7-1; cycle 08:00) — replication.rs substring-matches SQLSTATE
- **Closed by**: `f6043126 plugin-db: SQLSTATE-typed checks + classify_detail tests`
- `replication.rs:213` (`msg.contains("42710")`) and `:257` (`msg.contains("55000") || msg.to_lowercase().contains("wal_level")`) replaced with `e.as_db_error()?.code() == &SqlState::DUPLICATE_OBJECT` / `&SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE`. Locale- and formatter-independent. Same fragility class MAJOR-R5-1 closed for auth/session.rs.

### [S61] HIGH (test-coverage r7 2-round carry; cycle 07:30) — [I42] release-flag ordering structural test
- **Closed by**: `386f9bf5 plugin-db: add structural test for [I42] + lifecycle tests for ConsumerRunningGuard`
- New `release_flips_flag_after_unlock_await_structural` test in lock_guard::tests. Pins bd1e7ce1's invariant via byte-offset search through `include_str!('lock_guard.rs')`. A future revert of the await/flag order trips this test at unit-test time without needing a live PG fixture. Mirrors `mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure` pattern.

### [S62] MEDIUM (test-coverage r7; cycle 07:30) — ConsumerRunningGuard lifecycle tests + LATENT BUG FIX
- **Closed by**: `386f9bf5` (test commit)
- Lifted `ConsumerRunningGuard` from local function scope to module scope (still `pub(crate)`) so 4 lifecycle tests can directly exercise it: mark, drop-unmark, drop-unmark-on-panic, try_claim-lose-when-already-marked.
- **CRITICAL latent bug caught**: the original `try_claim` used `won.then_some(Self { app_id })` which evaluates `Self { app_id }` eagerly. On the lost-race path, the temporary Self constructed-then-dropped fired the custom Drop impl, UNMARKING the winner's claim. 70921112's atomic semantics were silently broken — both racing tasks would have ended with no consumer running. Fixed via lazy `won.then(|| Self { app_id })`.

### [S63] MINOR + 3× api-surface r6 MAJORs (cycle 07:30) — dead-code + docstring cleanup
- **Closed by**: `e5315083 plugin-db: dead-code + docstring cleanups`
- perf r7 N7-M0: session.rs::init_session map_err no longer builds dead `format!("{e}")` + source-chain walk after the DETAIL fix.
- api-surface r6 MAJOR-R6-1: `mark_consumer_running` (non-atomic) gated to test/test-helpers only; production builds no longer expose the footgun.
- api-surface r6 MAJOR-R6-2: replication_ops module docstring updated — `WalConsumer::new` documents both `ValidationFailed { code: "invalid_app_id" }` and `Configuration { code: "not_provisioned" }`.
- api-surface r6 MAJOR-R6-3: dropped unused `use crate::error::DbError;` import.

### [S57] MAJOR (code-critique r5 MAJOR-R5-1; cycle 06:55) — auth/session.rs P0001 substring matching
- **Closed by**: `a272d1af plugin-db/auth: classify P0001 RAISE via DETAIL token instead of substring`
- PG side: 5 RAISE EXCEPTION statements in the SECURITY DEFINER `init_session` function now carry `USING DETAIL = '<token>'`. Rust side: new `classify_p0001_detail()` helper reads `e.as_db_error()?.detail()` and maps to `DbError::ValidationFailed { code }`. Locale- and formatter-independent.

### [S58] MAJOR (code-critique r5 MAJOR-R5-4; cycle 06:55) — WalConsumer::new flattens DbError to ConsumerError::NotProvisioned(String)
- **Closed by**: `aa639715 plugin-db/wal_consumer: WalConsumer::new returns Result<_, DbError>`
- Removed `ConsumerError::NotProvisioned(String)` variant. `WalConsumer::new` now returns `Result<_, DbError>` directly with distinct codes: `ValidationFailed { code: "invalid_app_id" }` (developer error) vs `Configuration { code: "not_provisioned" }` (operator error). Dispatch site no longer re-stamps the typed error.

### [S59] NEW MINOR (concurrency r7; cycle 06:55) — startReplicationConsumer race window
- **Closed by**: `70921112 plugin-db/replication_ops: atomic try-claim closes startReplicationConsumer race`
- Two rapid-succession startReplicationConsumer() calls could both pass the outer `is_consumer_running` gate before either marked. Added `try_mark_consumer_running -> bool` atomic check-and-set; spawned task uses `try_claim: Option<Self>` constructor — the loser bails without spawning a duplicate consumer.

### [S60] CRITICAL (docs-audit r5 NEW; cycle 06:55) — replication_ops.rs comment block contradicted code
- **Closed by**: `4b2e7046 plugin-db/replication_ops: rewrite ConsumerRunningGuard comment block`
- Three accreted comment paragraphs across e399eeea/34d209b5/70921112 had the pre-34d209b5 "Mark BEFORE the spawn" rationale at the top contradicting the post-34d209b5 paragraphs below. Rewrote as one coherent paragraph + 3-line history block citing each commit's specific failure mode.

### [S54] IMPORTANT (migration-pipeline r5 R5-M7; cycle 06:00) — finalise_backfill let _ on terminal update
- **Closed by**: `51ced4a0 plugin-db: warn on finalise_backfill errors + document lock_guard hardening history`
- Replaced silent `let _ =` on the `finalise_backfill` await with `if let Err(e)` + `tracing::warn!` capturing app_id, audit_id, terminal state, and error. F1-family discipline regression closed.

### [S55] IMPORTANT (docs-audit r4; cycle 06:00) — lock_guard.rs preamble lacked hardening history
- **Closed by**: `51ced4a0 plugin-db: warn on finalise_backfill errors + document lock_guard hardening history`
- Added a "Hardening history" block to the lock_guard preamble naming the four design-pass commits: cbd12944 (extract), bd1e7ce1 [I42] (await-order), 808a32af [I39] (must_use+log), ffb1e101 [I44] (unlock-SQL warn). Future readers can trace the design.

### [S56] MAJOR (code-critique r6 MAJOR-R6-1; cycle 06:00) — mark_consumer_running synchronously before spawn
- **Closed by**: `34d209b5 plugin-db/replication_ops: move consumer-running mark inside ConsumerRunningGuard::new`
- The earlier e399eeea fix moved unmark into Drop but kept the mark BEFORE the spawn. Moved mark into `ConsumerRunningGuard::new()` so it fires inside the spawned future. Atomic lifecycle: any exit path (graceful, panic-mid-loop, dropped-pre-poll) hits Drop. Single-threaded per-isolate event loop makes the brief race window acceptable.

### [S50] MAJOR (code-critique r5 MAJOR-R5-2; cycle 05:25) — mark_consumer_running spawn-panic race
- **Closed by**: `e399eeea plugin-db/replication_ops: clear consumer-running marker on panic via Drop guard`
- Wrapped the spawned `run_supervised` task in a `ConsumerRunningGuard` struct with `Drop` impl that calls `unmark_consumer_running`. Fires on graceful exit AND panic-unwind — app no longer permanently marked "running" if the supervisor panics.

### [S51] MAJOR (code-critique r5 MAJOR-R5-3; cycle 05:25) — 5 duplicate coded_sql helpers deduped
- **Closed by**: `e44cc6b7 plugin-db/error: dedupe coded_sql/prefix_message across 5 sites` + helpers landing via `f7d0961c`
- Five copies of the variant-walking `coded_sql` / `prefix_message` helper (audit.rs, auth/{bootstrap,keys,session}.rs, diff.rs, replication.rs) collapsed to a single `crate::error::prefix_message` + `crate::error::coded_sql`. Net 142 LOC reduction. Per-module wrappers retained for the operator-facing prefix shape ("audit: ...", "auth/bootstrap: ...", etc.) without churning call sites.

### [S52] CRITICAL (api-surface r5 H1; cycle 05:25) — c0590506 broke test-helpers integration build
- **Closed by**: `f1f06900 plugin-db/tests: thread app_id through watchdog/dropAbandoned integration callers`
- The cross-app scope fix at c0590506 (cycle 04:35) added `app_id` parameter to `watchdog_query` + `drop_abandoned_slots` but missed three call sites in tests/integration.rs. Lib build was clean, but `--features test-helpers` build broke. Three-line fix. Pilot-discipline lesson: any signature change in a `pub` fn must include a same-commit test-helpers build verification.

### [S53] CRITICAL (docs-audit r4 NEW; cycle 05:25) — error.rs preamble drift after [I28] sweep
- **Closed by**: `f7d0961c plugin-db/error: update preamble after [I28] sweep closed the rail`
- The e37b188f preamble rewrite (cycle 01:35) listed remaining Result<_, String> sites as "replication.rs ~7, auth/* ~15, parts of diff.rs". The [I28] sweep at 0049d9be closed all of those, but the preamble drifted into the same shape as the original "lone hold-out" lie. Rewrote to accurately describe the now-narrow set of intentional hold-outs (validate stage envelope + ASCII hex pure-fns).

### [S46] IMPORTANT [I39] (cycle 04:35) — OrchestratorLockGuard Drop docs + #[must_use]
- **Closed by**: `808a32af plugin-db/orchestrator/lock_guard: must_use + louder Drop log`
- Added `#[must_use]` attribute to the guard struct so accidental `let _ = acquire(...).await` patterns surface as compile-time warnings. Strengthened Drop log with "leak:" prefix, operator-facing consequence ("Concurrent register_model callers for this app will stall"), and diagnostic checklist (cancellation / panic / forgotten release).

### [S47] MAJOR [I44] (cycle 04:35) — lock_guard.release silently swallowed unlock SQL errors
- **Closed by**: `ffb1e101 plugin-db/orchestrator/lock_guard: warn on pg_advisory_unlock errors`
- Code-critique r5 MAJOR-R5-5: the [I42] reorder kept `let _ =` on the unlock-SQL await, silently swallowing runtime errors (network blips, connection invalidation, etc.). Replaced with `if let Err(e)` + `tracing::warn!` capturing key/tag/error. Lock still auto-releases on PG session close; the warn makes a transient leak visible.

### [S48] CRITICAL (security r5 NEW; cycle 04:35) — Replication::watchdog + dropAbandoned cross-app exposure
- **Closed by**: `c0590506 plugin-db/v8_classes/replication: scope watchdog + dropAbandoned to self.app_id (CRITICAL)`
- Sibling of the cross-app `setup` hijack (309ed52f). Both `watchdog()` and `dropAbandoned()` were `#[v8_method]` exposed to tenant JS but executed cluster-wide queries with no app_id scoping. App A could enumerate every co-tenant's slot names (info disclosure) or drop their inactive slots (DoS via forced resync). Plumbed `self.app_id` through both dispatch helpers; added `WHERE slot_name LIKE '<per-app-prefix>%'` filter via parameter binds. New `resolve_watchdog_app_id` + `resolve_drop_abandoned_app_id` helpers mirror the regression-trip-wire pattern.

### [S49] IMPORTANT (architecture r6 §I4; cycle 04:35) — first_row_or_internal() helper for empty-RETURNING cluster
- **Closed by**: `eda96ead plugin-db: extract first_row_or_internal() helper for empty-RETURNING cluster`
- N=4 sibling-pattern cluster (audit.rs ×2, replication.rs, migrations.rs:326) extracted to a single `first_row_or_internal<R>(rows, op)` helper in error.rs. 3 sites converted (migrations.rs:326 left untouched per the architect's note — its sentinel-check shape doesn't fit the helper's slice signature). 2 new unit tests pin the helper's contract.

### [S45] IMPORTANT [I42] (cycle 04:00) — lock_guard.release flipped state before await
- **Closed by**: `bd1e7ce1 plugin-db/orchestrator/lock_guard: defer released-flag flip to AFTER unlock await`
- `release()` previously set `self.released = true` and took the client out of self BEFORE the unlock-SQL await. A cancellation/panic mid-await silently leaked the lock — Drop's catastrophic-log path was suppressed because `released = true`. Reordered: unlock SQL via `&`-borrow, await completes, then flip `released` and take the client. On cancellation: `released = false`, `client = Some(_)`, Drop fires its log; client drops back to pool with lock held until the underlying PG session ends.

### [S43] CRITICAL (docs-audit r3 NEW; cycle 03:25) — validate.rs preamble lied about SchemaRefused .code
- **Closed by**: `07205e54 plugin-db: demote 5 mint_* helpers + fix validate.rs SchemaRefused doc lie`
- `validate.rs:25-30` claimed SchemaRefused's `to_op_error()` arm does NOT stamp `.code`; verified in `error.rs::to_op_error()` that it DOES stamp from the static discriminator. SDK CAN branch on `err.code === "validation_refused"` directly. Rewrote the preamble.

### [S39] HIGH (error-ux r3; cycle 02:50) — recover 60ca1ad6 silently reverted by ed697c45
- **Closed by**: `dec2bd42 plugin-db/migrations: restore 60ca1ad6 fixes silently reverted by ed697c45`
- The cycle 01:10 [I1] audit-rail refactor (ed697c45) silently reverted two unrelated fixes from `60ca1ad6`: (a) line 258 `tx_connect_failed` was routed through `to_op_error()` to preserve SQLSTATE; reverted to flat string. (b) `coded_db` prefix was changed from `"db: {context} failed: {message}"` to `"{context}: {message}"` because the message already carries `"db: "`; reverted, causing doubled prefix. Both restored. Pilot-discipline lesson: a fixer's diff can be wider than its commit message claims; verify by running a follow-up review on the SAME paths.

### [S34] CRITICAL × 2 (docs-audit r2; cycle 01:35) — error.rs lone-holdout claim + db.md broken path
- **Closed by**: `e37b188f plugin-db/error + docs/db: fix docs CRITICALs from docs-audit r2`
- (a) `error.rs:9-14`'s "lone hold-out" claim was false (~30 `Result<_, String>` sites remain in `replication.rs`, `auth/*`, `diff.rs`, etc.). Rewrote preamble to accurately describe the pending sweep (now tracked as [I28]).
- (b) `docs/reference/db.md:90` pointed at deleted path `crates/runtime/src/bootstrap/db_init.js`. Repointed to `sdks/bootstrap/src/runtime-entry.ts` (embedded via `DB_INIT_JS` in `crates/runtime/src/core/init.rs`).

### [S28] Error-UX — double `db:` prefix
- **Source**: `plugin-db-error-ux-2026-05-22-r1.md` §4e
- **Fixed by**: `60ca1ad6 plugin-db: use Display instead of Debug in user-facing error messages; drop double db: prefix; route tx_connect_failed via from_pg`. Also closed §4b for `tx_connect_failed` (now via `from_pg`).

---

## Pilot Pick

**Cycle history:**
- **00:17** closed [I1] + 3 new CRITICALs
- **00:47** closed [I11], [I19], perf CRITICAL N3-C1
- **01:10** closed [I29], [I34], 2 docs CRITICALs
- **01:35** closed [I27], [I30]
- **02:50** closed [I36], [I38]; recovered 60ca1ad6 silent reversion
- **03:25** closed [I40], [I41], 5 mint_* demote, validate.rs doc CRITICAL
- **04:00** closed [I28] ~70-site sweep, [I42] lock_guard await order
- **04:35** closed [I39], [I44], NEW CRITICAL (watchdog cross-app), first_row_or_internal
- **05:25** closed MAJOR-R5-2, MAJOR-R5-3, 2 CRITICALs (test-helpers build break + error preamble drift)
- **06:00** closed R5-M7, docs-audit r4 lock_guard hardening, MAJOR-R6-1
- **06:55** closed MAJOR-R5-1, MAJOR-R5-4, concurrency-r7 race, docs-audit r5 CRITICAL
- **07:30** closed [I42] structural test, ConsumerRunningGuard lifecycle tests (+ LATENT BUG caught), 3× api-surface r6 MAJORs, perf r7 N7-M0
- **08:00** closed M11 coded_db dedup, classify_detail unit tests (+ test-coverage r8 NEW gap), MIN-R7-1 replication SQLSTATE substring-match
- **08:30** closed Configuration hint field, 3 docs-audit r6 drift sites
- **09:00** closed TX_CONN sweep (4-round hold-out), OBJECT_PREFIX demote (api-surface r7 MAJOR-R7-2), R8-1 error.rs preamble drift

**Net since pilot started**: ~52 closures, ~29 new findings. **Trajectory has plateaued** per architecture r9 + code-critique r8 — further pilot cycles have diminishing per-LOC value.

### Pick #1 (next cycle): **R8-2 init_pool_async code-name drift**
- **File**: `crates/plugin-db/src/lib.rs:351` + the two call sites (`orchestrator/register_model/mod.rs:120` synthesises `lazy_init_failed`; `exec.rs::ensure_pool` synthesises `not_configured`)
- **Fix sketch**: tighten the two synthesised codes to one canonical value, OR convert `init_pool_async` to `Result<_, DbError>` directly and pass the typed error through.
- **Why next**: small surface area; SDK sees two different codes for the same root cause (cold-start pool init failure).

### Pick #2 (next cycle, larger): **F1 + F2 (5+ cycle carry, design needed)**
- **Caveat**: requires schema migration to `__zeroship_migrations` + sweeper task. Won't land in a single cycle.

### Pick #3 (cross-crate, needs scope grant): **R7 I5 — auth/* dormancy**
- **File**: `crates/plugin-db/src/auth/*.rs` + `crates/control/src/main.rs` (out of pilot scope).
- **Fix sketch**: wire `auth::ensure_admin_schema(pool)` into `zeroship-control` startup behind a `--harden` CLI flag.
- **Why**: architecture r9's recommendation for breaking through the plateau. Requires explicit cross-crate scope grant from the user.
