# crates/plugin-db — Deferred Backlog

Auto-managed by the pilot-cron-worker. Last reviewed: 2026-05-22 00:45.

Source reviews triaged (14 total):
- `plugin-db-api-surface-2026-05-22-r1.md`
- `plugin-db-architecture-review-2026-05-21.md`
- `plugin-db-architecture-review-2026-05-21-round2.md`
- `plugin-db-architecture-review-2026-05-22-r3.md`
- `plugin-db-code-critique-2026-05-21.md`
- `plugin-db-code-critique-2026-05-22-r2.md`
- `plugin-db-concurrency-2026-05-22-r2.md`
- `plugin-db-docs-audit-2026-05-22-r1.md`
- `plugin-db-error-ux-2026-05-22-r1.md`
- `plugin-db-migration-pipeline-2026-05-22-r1.md`
- `plugin-db-performance-2026-05-22-r1.md`
- `plugin-db-performance-2026-05-22-r2.md`
- `plugin-db-security-2026-05-22-r1.md`
- `plugin-db-test-coverage-2026-05-22-r2.md`

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
- **Status as of 2026-05-22 00:17**:
  - Code still exists? The original 4-parse chain is closed. `exec.rs:83-87` returns `Vec<Value>` directly; `crud.rs:105-108` does one serialise; `ResolveValue::Json` parses once in V8.
  - Blocker: Further reduction requires plumbing `Vec<Value>` directly to V8 (new `ResolveValue::JsonValue` shape) — design change in `zeroship-runtime::state`.
  - Already-superseded-by: `cc7fff89 plugin-db: thread Vec<Value> end-to-end (drop serde round-trip)` — most of the win is in.
- **Effort**: medium (multi-crate; requires a new `ResolveValue` shape)
- **Pickable this cycle**: no — the residual is structural (Rust value → V8 value at the boundary) and crosses the runtime crate boundary.

---

## IMPORTANT (mechanical, actionable)

### [I2] `SchemaRefused` has `.code` on OpError but error-ux review flagged absence (verify state)
- **Source**: `plugin-db-error-ux-2026-05-22-r1.md` §4a
- **File**: `crates/plugin-db/src/error.rs` (DbError::SchemaRefused arm in `to_op_error`)
- **Description**: Error-UX review said `SchemaRefused.to_op_error()` calls `OpError::error(envelope_json)` (no `.code`), forcing the SDK to `JSON.parse(e.message)`. Commit `d2aeada6 plugin-db/error: stamp .code on SchemaRefused (SDK can now branch on validation_refused)` appears to have closed this — verify before deciding.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Partial closure. Commit `d2aeada6` claims `.code` is now stamped on `SchemaRefused`. Spot-verify needed against `error.rs` `to_op_error()` arm and the SDK's `mapNativeError`.
  - Blocker: none if commit closed it; SDK contract test is the verification gate.
  - Already-superseded-by: likely `d2aeada6` — promote to SUPERSEDED if `error.rs` confirms.
- **Effort**: small (verification, not a fix)
- **Pickable this cycle**: yes for verification; promote to SUPERSEDED if confirmed.

---

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

### [I6] `release_advisory_lock` trait signature returns `()` (code-critique I-NEW-2 / R1 I6)
- **Source**: `plugin-db-code-critique-2026-05-22-r2.md` §I-NEW-2; `plugin-db-code-critique-2026-05-21.md` §I6
- **File**: `crates/plugin-db/src/backend/mod.rs` (trait declaration); `crates/plugin-db/src/backend/postgres.rs:157-160`
- **Description**: `async fn release_advisory_lock(&self, client: &Self::Client, key1: &str, key2: &str);` — no `Result`. The impl swallows errors silently (`let _ = client.query_text_params(...).await`). `apply.rs` was fixed by issuing the unlock inline (not via the trait method), so this trait method has no production callers — but it remains a footgun for any future caller added behind the trait.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes. `backend/postgres.rs:157-160` matches the review verbatim.
  - Blocker: none. The trait change is mechanical: `→ Result<(), DbError>`. The one impl returns `Ok(())`. The trait has no production callers, so call-site burden is zero.
  - Already-superseded-by: N/A
- **Effort**: small (signature change + `Ok(())` in one impl)
- **Pickable this cycle**: yes — single-file mechanical change; closes a footgun. But also low-urgency because no production caller hits it.

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

### [I8] Cursor advance not atomic with COMMIT in real-run backfill (migration-pipeline I2)
- **Source**: `plugin-db-migration-pipeline-2026-05-22-r1.md` §3-I2
- **File**: `crates/plugin-db/src/migrations.rs:550-573`
- **Description**: Real-run `exec_commit_batch` issues COMMIT, then calls `update_backfill_progress` as a separate statement. If the process crashes between COMMIT and the cursor update, the next run resumes from the old cursor and re-processes the already-committed batch. `migrateOne` idempotency is documented as the user's responsibility, but the platform contract should either include the cursor advance inside the COMMIT or document the non-exactly-once semantics explicitly.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes (reviewed code unchanged).
  - Blocker: design — the fix is to move the `update_backfill_progress` UPDATE inside the same transaction as the user updates. Requires re-checking the transaction boundaries and the audit-row lock ordering (FOR UPDATE).
  - Already-superseded-by: N/A
- **Effort**: medium (transaction re-shaping, ordering review)
- **Pickable this cycle**: no — design decision (atomic vs documented at-least-once) needs user input.

---

### [I9] `running_consumers` slot diverges from live task on rapid teardown (concurrency)
- **Source**: `plugin-db-concurrency-2026-05-22-r2.md` §2 "RUNNING_CONSUMERS slot can diverge"
- **File**: `crates/plugin-db/src/replication_ops.rs:221-226` (mark_consumer_running before spawn; unmark inside spawned future)
- **Description**: If the runtime is shutting down when the future is dropped before polling `run_supervised` to completion, `unmark_consumer_running` never executes. The slot stays marked "running" but no task is live. Subsequent `startReplicationConsumer()` short-circuits with `alreadyRunning: true` and leaves the app with no consumer and no local-emit. Long-lived isolates never trigger this; rapid LRU eviction mid-spawn does.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes — `replication_ops.rs:251-257` confirms `mark_consumer_running` is called before `spawn`, `unmark` inside the future body.
  - Blocker: small — needs an RAII Drop guard wrapping `app_for_task` so cancellation also clears the slot.
  - Already-superseded-by: N/A
- **Effort**: small (one Drop-guard struct, ~20 lines)
- **Pickable this cycle**: yes — bounded, mechanical.

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

### [I11] Stale docs reference deleted `callbacks.rs` and retired `TX_CONN` thread-local (docs-audit drift #1-#7)
- **Source**: `plugin-db-docs-audit-2026-05-22-r1.md` §2 drift table rows 1-7, §3 missing preambles
- **File**: `docs/reference/db.md:123,592`; `docs/proposals/zeroship-db.md:76,196`; `crates/plugin-db/src/lib.rs:173`; `crates/plugin-db/src/orchestrator/mod.rs:13`; `crates/plugin-db/src/v8_classes/mod.rs:17`; `crates/plugin-db/src/orchestrator/register_model/validate.rs:17-18`; `crates/plugin-db/src/error.rs:3-7`; missing preambles in `crud.rs`, `diff.rs`, `replication_ops.rs`
- **Description**: Docs reference deleted `callbacks.rs` (split into `orchestrator/register_model/*`) and the retired `TX_CONN`/`TX_TOKEN` thread-locals (now `IsolateDbContext` fields). Three high-traffic files (`crud.rs`, `diff.rs`, `replication_ops.rs`) lack module preambles. The `validate.rs` comment misrepresents the current return type as `Result<_, String>` when it's `DbError::SchemaRefused`.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes — confirmed multiple drift sites in the docs-audit review; spot-checks at the cited lines match.
  - Blocker: none. Pure doc-edit sweep.
  - Already-superseded-by: N/A (note `replication_ops.rs` got a doc preamble, see earlier read — partial closure; `crud.rs` and `diff.rs` still lack module-level `//!` blocks)
- **Effort**: small (multi-file doc sweep, no code change)
- **Pickable this cycle**: yes — pure docs.

---

### [I12] `validate_field_name` permits non-ASCII identifiers (test-coverage GAP-1; security MINOR)
- **Source**: `plugin-db-test-coverage-2026-05-22-r2.md` §3 GAP-1; `plugin-db-security-2026-05-22-r1.md` §2 "MINOR — validate_collection not called for field names"
- **File**: `crates/plugin-db/src/query.rs:104-121`
- **Description**: `validate_collection` rejects non-ASCII; `validate_field_name` does not. Field name `"café"` (4 chars, 5 bytes) passes. `quote_ident` prevents injection but Postgres byte-truncation at 63 could alias two distinct fields. Either tighten to ASCII-only (matching `validate_collection`) or document the intentional unicode-permit policy.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes — `query.rs:104-121` confirmed: only checks empty, null-byte, and 63-byte length.
  - Blocker: small policy decision (allow unicode or not).
  - Already-superseded-by: N/A
- **Effort**: small (~5 lines + unit test)
- **Pickable this cycle**: yes — policy + one-liner + test.

---

### [I13] Missing unit tests for `queue_or_emit` / `drain_pending_emits_on_commit` (test-coverage GAP-2)
- **Source**: `plugin-db-test-coverage-2026-05-22-r2.md` §3 GAP-2 (priority HIGH)
- **File**: `crates/plugin-db/src/exec.rs:205-254` (queue_or_emit, drain_pending_emits_on_commit, clear_pending_emits)
- **Description**: Subscription event delivery gates have no unit tests; both paths exercise only thread-local context and the `IsolateDbContext` has `cfg(test)` helpers. A misfired branch silently drops or double-delivers subscriber events. Two-test pair: one with tx active → events queue; one without tx → events emit immediately.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes — `exec.rs:205-254` confirmed; no `#[cfg(test)] mod tests` in `exec.rs`.
  - Blocker: none. Pure test addition.
  - Already-superseded-by: N/A
- **Effort**: small (~30 lines of test code, no code change)
- **Pickable this cycle**: yes — pure test.

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

### [I16] `IsolateDbContext` fields are `pub(crate)` rather than private; `tx_token_counter` advertises mutation path (api-surface I3; R3 M1+M2)
- **Source**: `plugin-db-api-surface-2026-05-22-r1.md` §I3; `plugin-db-architecture-review-2026-05-22-r3.md` §4-MINOR-M1/M2
- **File**: `crates/plugin-db/src/context.rs:70-158`
- **Description**: `pool`, `db_url`, `tx_conn`, `registered_models`, `auto_tx_owned`, `tx_token`, `tx_token_counter`, `pending_emits`, `mig_lock`, `running_consumers`, `backend` are all `pub(crate)`. Accessors exist for all of them. Direct field mutation bypasses any future invariant checks. `tx_token_counter` is particularly egregious — only `next_tx_token()` should touch it.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes — `context.rs:70-158` confirmed.
  - Blocker: none.
  - Already-superseded-by: N/A
- **Effort**: small (visibility flips + a compile check)
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

### [I19] Migration `apply::run_op` silently no-ops unhandled `ChangeKind`s (code-critique M6; migration-pipeline C2)
- **Source**: `plugin-db-code-critique-2026-05-21.md` §M6; `plugin-db-migration-pipeline-2026-05-22-r1.md` §3-C2 (rated CRITICAL but contingent on classifier evolution)
- **File**: `crates/plugin-db/src/orchestrator/register_model/apply.rs:137` (`ChangeKind::DropColumn | ChangeKind::DropIndex => Ok(()),`)
- **Description**: Today benign because validate filters destructive ops; the silent `Ok(())` is a latent trap. A future reclassification (e.g., `DropIndex` as Compatible for "drop unused index") would silently audit-lie — write a `running` row, then an `applied` row, with no DDL executed and no `sql` field populated.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes — `apply.rs:137` matches.
  - Blocker: none. Fix is `unreachable!("classifier returned Drop as non-destructive")` or `debug_assert!(false, …)`.
  - Already-superseded-by: N/A
- **Effort**: small (one-line + comment)
- **Pickable this cycle**: yes — but the impact is latent.

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

### [I21] Hand-rolled JSON in `create_index_with_recovery_audited` (code-critique I-NEW-3)
- **Source**: `plugin-db-code-critique-2026-05-22-r2.md` §I-NEW-3
- **File**: `crates/plugin-db/src/backend/postgres.rs:478-525, 558-568`
- **Description**: Three return paths hand-roll JSON via `format!`. The `replace('"', "\\\"")` escaping misses `\n`, `\r`, `\t`, and Unicode control characters; any of which in a Postgres error message produces syntactically invalid JSON. SDK does `JSON.parse` on the envelope; malformed JSON becomes an opaque parse error masking the cause. The function returned `Result<(), String>` per the review — verify against the `ff220fce plugin-db/backend: create_index_with_recovery returns Result<(), DbError> (last trait outlier); serde_json-based envelope` commit, which appears to have closed this.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Likely closed — `ff220fce` claims `serde_json-based envelope`. Spot-verify against `backend/postgres.rs:478-525`.
  - Blocker: none if closed.
  - Already-superseded-by: likely `ff220fce` — promote to SUPERSEDED if `format!`-based JSON is gone.
- **Effort**: small (verification only)
- **Pickable this cycle**: yes for verification.

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

### [I23] `unmark` no-op when `return_mig_client` slot is empty masks state bugs (code-critique I7)
- **Source**: `plugin-db-code-critique-2026-05-21.md` §I7
- **File**: `crates/plugin-db/src/context.rs:365` (set_mig_lock missing debug_assert); `crates/plugin-db/src/context.rs:385-389` (return_mig_client silent no-op)
- **Description**: `set_mig_lock` constructs `MigrationLock` and relies on convention. `return_mig_client` silently no-ops when slot empty — masks state-machine bugs. Asymmetric with `set_tx_token` / `tx_conn` which carry debug_asserts.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes (reviewed code unchanged).
  - Blocker: none. Add `debug_assert!(lock.client.is_some())` on `set_mig_lock`; `tracing::warn!` on empty `return_mig_client`.
  - Already-superseded-by: N/A
- **Effort**: small (~5 lines)
- **Pickable this cycle**: yes.

---

### [I24] `exec_mutation_with_emit` redundant string clone on tuple values (perf review residue)
- **Source**: `plugin-db-performance-2026-05-22-r1.md` §3-I2 (now closed via `c54a9f15` for the broker side, but the per-row tuple build still pays)
- **File**: `crates/plugin-db/src/exec.rs:180-192`
- **Description**: For every returned row, builds a `HashMap<String, String>` cloning every column name. For an INSERT returning a 20-column row, 20 String clones per emitted event. Broker fan-out is now zero-clone (per `c54a9f15`), but the per-row build is unchanged. If the WAL consumer fast-paths via `has_subscribers` (commit `78a95d3b` + `967a7362` closed this on the WAL side), the local-emit path here still pays unconditionally.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes — `exec.rs:180-192` builds the HashMap unconditionally.
  - Blocker: small — wrap in `broker::has_subscribers(app_id, collection)` check (the same fast-path used in `wal_consumer::emit_for_tuple`).
  - Already-superseded-by: N/A (broker side closed, mutation-emit side open)
- **Effort**: small (one conditional + reuse existing `broker::has_subscribers`)
- **Pickable this cycle**: yes — small, mirrors a recently-landed pattern.

---

### [I25] `OBJECT_PREFIX` in `LIKE` predicate is format-string interpolated (security MINOR; replication M2)
- **Source**: `plugin-db-security-2026-05-22-r1.md` §2 "MINOR — OBJECT_PREFIX literal"; `plugin-db-migration-pipeline-2026-05-22-r1.md` §3 M2 (related: SELECT-then-DROP race comment lies about implementation)
- **File**: `crates/plugin-db/src/replication.rs:318,431`
- **Description**: `format!(r"... WHERE slot_name LIKE '{OBJECT_PREFIX}%'")`. `OBJECT_PREFIX = "__zs_"` is a `const &str`, so no injection risk today. Future maintainer changing the constant to include `%` or `_` would break LIKE semantics. Safe fix: `$1 || '%'` bind.
- **Status as of 2026-05-22 00:17**:
  - Code still exists? Yes.
  - Blocker: none.
  - Already-superseded-by: N/A
- **Effort**: small (one param-bind change × 2 sites)
- **Pickable this cycle**: yes — purely defensive.

---

## SUPERSEDED (already fixed; remove next cycle)

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

### [S28] Error-UX — double `db:` prefix
- **Source**: `plugin-db-error-ux-2026-05-22-r1.md` §4e
- **Fixed by**: `60ca1ad6 plugin-db: use Display instead of Debug in user-facing error messages; drop double db: prefix; route tx_connect_failed via from_pg`. Also closed §4b for `tx_connect_failed` (now via `from_pg`).

---

## Pilot Pick

**Cycle of 2026-05-22 00:17** picked Pick #1 [I1] (now closed; see [S28a]) plus three new CRITICALs surfaced by this cycle's reviewers ([S28b] bootstrap lock leak, [S28c] cross-app replication override × 2, [S28d] test-helpers visibility). Pick #2 [I11] doc sweep is rolled forward to the next cycle.

### Pick #2 (rolled forward): **[I11] Stale docs reference deleted `callbacks.rs` and retired `TX_CONN` thread-local**
- **File** (multi):
  - `docs/reference/db.md:123, 592` — `TX_CONN` → `IsolateDbContext::tx_conn`
  - `docs/proposals/zeroship-db.md:76, 196` — annotate "superseded; see `orchestrator/register_model/{bootstrap,plan,validate,apply}.rs`"
  - `crates/plugin-db/src/lib.rs:173` — doc comment naming retired thread-local
  - `crates/plugin-db/src/orchestrator/mod.rs:13` — same
  - `crates/plugin-db/src/v8_classes/mod.rs:17` — same
  - `crates/plugin-db/src/orchestrator/register_model/validate.rs:17-18` — comment misrepresents the return type as `Result<_, String>`
  - `crates/plugin-db/src/error.rs:3-7` — present-tense "is being migrated"
  - Missing module preambles: `crates/plugin-db/src/crud.rs`, `crates/plugin-db/src/diff.rs`
- **Fix sketch**: Pure doc-edit sweep. Replace `TX_CONN` references with `IsolateDbContext::tx_conn`; add a superseded annotation block at the head of `docs/proposals/zeroship-db.md` listing the new implementation site; add `//!` preambles to `crud.rs` and `diff.rs`; tense-fix `error.rs` preamble.
- **Why this cycle**: small (zero behaviour change), high-leverage for newcomer navigability (the task router in AGENTS.md sends people directly to these files), and unblocks the docs-audit review from regressing as a recurring finding.
- **Verification gate**:
  - `cargo doc -p zeroship-plugin-db --no-deps` (zero warnings — currently clean per `29b8a013`).
  - `grep -rn 'TX_CONN\|TX_TOKEN\|callbacks\.rs' crates/plugin-db/src/ docs/reference/db.md docs/proposals/zeroship-db.md` — expect zero hits after the sweep (the names should appear only as historical context with explicit "renamed to" annotations, if at all).
