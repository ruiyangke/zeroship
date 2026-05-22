# crates/plugin-db — Deferred Backlog

Auto-managed by the pilot-cron-worker. Last reviewed: 2026-05-22 05:25.

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

### [I26] Stale TX_CONN/TX_TOKEN refs in 9 sites (docs-audit follow-up; cycle 01:10)
- **Source**: cycle-01:10 [I11] docs-sweep agent — stragglers outside the agent's 8-site scope guard
- **File**: `crates/plugin-db/src/v8_classes/transaction.rs:108,116,161,192,214,284,285`; `crates/plugin-db/src/crud.rs:53`; `crates/plugin-db/src/exec.rs:277`; `crates/plugin-db/src/orchestrator/transaction.rs:143`
- **Description**: Inline doc/comment references to the retired `TX_CONN` / `TX_TOKEN` thread-locals (now `IsolateDbContext` fields). All in production code; not user-visible but newcomer-confusing.
- **Status as of 2026-05-22 01:10**: pickable next cycle. Pure doc edits.
- **Effort**: small (~9 line edits, single sweep)
- **Pickable this cycle**: yes — pure docs.

---

### [I31] Migration-pipeline: orphan `Running` DDL audit rows have no heartbeat/sweeper (migration-pipeline r2 F1)
- **Source**: `plugin-db-migration-pipeline-2026-05-22-r2.md` §F1
- **File**: `crates/plugin-db/src/orchestrator/register_model/apply.rs:163-186` — `update_audit_status` errors silently discarded via `let _ = …`
- **Description**: Worker dying between DDL completion and audit terminal-update leaves the row in `Running` forever. No `owner_session_id`/heartbeat on DDL rows, no sweeper to terminalise abandoned entries.
- **Status as of 2026-05-22 01:10**: design needed (sweeper cadence, ownership claim, watchdog policy). Not a simple mechanical fix.
- **Effort**: medium-large (needs new schema column + background task)
- **Pickable this cycle**: no — design decision required.

---

### [I32] Migration-pipeline: orphan `Pending` audit rows never reach terminal state (migration-pipeline r2 F2)
- **Source**: `plugin-db-migration-pipeline-2026-05-22-r2.md` §F2
- **File**: `crates/plugin-db/src/orchestrator/register_model/validate.rs:74-85` (writes Pending); `apply.rs` (skips destructive ops without terminalising)
- **Description**: Strict and lenient paths both leave `Pending` rows stranded. Operator queries on `status='pending'` see phantoms.
- **Status as of 2026-05-22 01:10**: actionable but needs design for the cancellation/refusal flow.
- **Effort**: medium
- **Pickable this cycle**: no — paired with [I31].

---

### [I33] Migration-pipeline: update_backfill_progress race with reset (migration-pipeline r2 F3)
- **Source**: `plugin-db-migration-pipeline-2026-05-22-r2.md` §F3
- **File**: `crates/plugin-db/src/migrations.rs:577-598`
- **Description**: `update_backfill_progress` runs OUTSIDE the BEGIN/COMMIT envelope and without an `audit_generation` predicate. Operator `reset` between COMMIT and the progress UPDATE is silently clobbered — fresh runs resume from stale cursor.
- **Status as of 2026-05-22 01:10**: actionable; add `audit_generation` column or move progress UPDATE into the COMMIT.
- **Effort**: medium (schema migration + WHERE clause)
- **Pickable this cycle**: rolled forward.

---

### [I35] row_to_json O(N²) per row in column count (performance r4 N4-I3)
- **Source**: `plugin-db-performance-2026-05-22-r4.md` §"N4-I3"
- **File**: `crates/plugin-db/src/v8_bridge.rs:357` (`column_to_json`); `compio-postgres/src/row.rs:65-82` (`row.try_get` linear scan)
- **Description**: Each `column_to_json(row, col.name(), ...)` call does `row.try_get::<_, T>(name)` which linear-scans `columns()` to find the index. For a 20-column row: 400 string compares per row. Hot on every `findOne` and large `find`.
- **Status as of 2026-05-22 02:05**: actionable; fix is index-by-position. Either change `row_to_json` to iterate by `enumerate()` index, or cache the column→index map once at the start.
- **Effort**: small (single function refactor; compio-postgres may need a position-aware accessor exposed)
- **Pickable this cycle**: rolled forward.

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
- **05:25** closed MAJOR-R5-2 (consumer-running Drop guard), MAJOR-R5-3 (coded_sql dedup), CRITICAL (c0590506 test-helpers build break), CRITICAL (error.rs preamble drift)

**Net since pilot started**: ~30 closures, ~23 new findings.

### Pick #1 (next cycle): **Migration-pipeline r5 R5-M7 — finalise_backfill let _ on terminal update**
- **File**: `crates/plugin-db/src/migrations.rs:639-641`
- **Fix sketch**: Same `let _ = ` discipline regression as F1 family — `finalise_backfill`'s terminal audit-row UPDATE error is discarded. Either `tracing::warn` on Err (cheap) or surface via the function's Result (more invasive).
- **Why next**: small, mirrors recent F1-class hardening; closes a regression-prone pattern.

### Pick #2 (next cycle): **Docs-audit r4 — lock_guard.rs preamble incomplete after 3-pass hardening**
- **File**: `crates/plugin-db/src/orchestrator/lock_guard.rs:1-50`
- **Fix sketch**: Append a "Hardening history" block naming `[I42]` (bd1e7ce1 await-order), `[I39]` (808a32af must_use+log), `[I44]` (ffb1e101 unlock-SQL warn) so a future reader can trace the design.
- **Why**: docs-audit r4 IMPORTANT; recurring drift pattern after multi-pass hardening; doc-only.

### Pick #3 (next cycle, design needed): **[I43] bootstrap.rs blocking pg_advisory_lock**
- **File**: `crates/plugin-db/src/orchestrator/register_model/bootstrap.rs:107`
- **Fix sketch**: Switch from blocking `pg_advisory_lock` to `try_acquire_advisory_lock` with a backoff loop and a per-app cap.
- **Caveat**: needs design decision on retry/backoff policy + max-wait semantics.
