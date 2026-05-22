//! B1 — data backfill orchestrator. Native side of the
//! `@zeroship/migrations` component (`docs/proposals/zeroship-db.md`
//! section B1).
//!
//! ## Surface
//!
//! Two layers:
//!
//! - **Per-run methods on the `Migration` wrapper**
//!   ([`crate::v8_classes::migration::Migration`]):
//!   `.fetchBatch(cursor, batchSize)`, `.commitBatch(updates, …)`,
//!   `.status()`, `.cancel()`, `.reset()`. The wrapper is minted by
//!   `env.db.migrations.start(spec)` and owns the dedicated client
//!   that holds the advisory lock for the run.
//!
//! - **By-name observation methods** on `env.db.migrations` (the
//!   `Migrations` v8_class namespace): `.status({name, collection})`,
//!   `.cancel({name, collection})`, `.reset({name, collection})`. These
//!   read the audit row directly and never touch the advisory lock, so
//!   they're safe to call from any worker without colliding with an
//!   in-flight run.
//!
//! Public functions in this module (`exec_begin`, `exec_fetch_batch`,
//! `exec_commit_batch`, `exec_status`, `exec_cancel`, `exec_reset`)
//! are the underlying SQL executors driven by both layers.
//!
//! ## Architectural split
//!
//! The SDK keeps the per-row loop in JavaScript: it calls
//! `.fetchBatch()` to read rows, runs `migrateOne` per row (so
//! per-row try/catch and dead-letter accumulation live in the SDK),
//! and posts results back via `.commitBatch()`. The native side
//! owns: advisory-lock lifetime, audit-row state machine, SQL
//! execution, cursor monotonicity, dry-run rollback.
//!
//! ## Lock lifetime
//!
//! `pg_try_advisory_lock(hashtext('zs_mig:<app>')::int4,
//! hashtext(<name>)::int4)` is session-scoped, held on a dedicated
//! `Backend::Client` stashed in the per-isolate context's
//! `mig_lock` slot. The same client runs every SELECT/UPDATE in the
//! run because advisory locks are invisible across connections.

use serde_json::Value;
use zeroship_runtime::state::OpError;

use crate::audit::TerminalStatus;
use crate::backend::{Backend, PostgresBackend};
use crate::context::MigrationLock;
use crate::query::{quote_ident, validate_collection};

/// Convenience alias — the migration loop holds a backend-owned
/// client across awaits. `<PostgresBackend as Backend>::Client` is
/// the concrete `compio_postgres::Client` today; future backends
/// can swap in their own session type without rewriting every site
/// that pulls the lock client out of the per-isolate context.
type LockClient = <PostgresBackend as Backend>::Client;

/// Build a coded `OpError` for a migration lifecycle failure. The
/// runtime pump materialises a JS `Error` with `e.code` (and optional
/// `e.hint`) attached — the SDK branches on `e.code` directly, no
/// substring matching, no `JSON.parse(e.message)`.
fn coded(code: &str, message: &str, hint: Option<&str>) -> OpError {
    OpError::coded(code, message, hint.map(str::to_string))
}

/// Stamp a typed `DbError` with a lifecycle context phrase and
/// convert to `OpError`. Used by the many `map_err(|e| coded_db(...,
/// e))`-shaped sites in this file where the typed `DbError` came back
/// from a Backend method (already SQLSTATE-classified) and we just
/// want to prepend "migration insert failed: " etc. before crossing
/// the V8 boundary.
///
/// Thin wrapper around [`crate::error::prefix_message`] — kept as a
/// migrations-local helper so the call sites read naturally. The
/// variant-walk logic lives in `crate::error` (architecture r8 M11;
/// previously this function open-coded the same match arms found in
/// audit.rs, auth/*.rs, diff.rs, replication.rs — all consolidated
/// at cbbc9059, this file followed at deeefe18).
///
/// `.code` is preserved from the typed variant (`unique_violation`,
/// `serialization_failure`, `transient`, …); the lifecycle phrase
/// goes into the message body only.
fn coded_db(context: &str, e: crate::error::DbError) -> OpError {
    let mut db_err = e;
    // `message` already starts with "db: " from walk_pg_chain;
    // prepend only the lifecycle context phrase to avoid the
    // doubly-prefixed "db: {context} failed: db: ..." output
    // (restored from 60ca1ad6, silently reverted by ed697c45,
    // re-restored by dec2bd42).
    crate::error::prefix_message(&mut db_err, &format!("{context}: "));
    db_err.to_op_error()
}

/// Map an `ensure_audit_table` failure to an `OpError` while preserving
/// the SDK-facing `.code` discipline.
///
/// SQLSTATE-coded variants (`Transient`, `LockContention`,
/// `UniqueViolation`, …) and pre-coded ones (`SchemaRefused`,
/// `ValidationFailed`, …) flow through `to_op_error()` verbatim so the
/// SDK can branch on `.code` (e.g. retry on `transient`,
/// surface-and-back-off on `lock_not_available`).
///
/// Only the catch-all `Internal` arm gets re-wrapped with the
/// operator-facing `audit_bootstrap_failed` code — that's the bucket
/// where there is no SQLSTATE to preserve and the operator wants to
/// know *which* lifecycle step bootstrapped the audit table.
///
/// Mirrors the apply.rs:87-101 discipline so every migrations-lifecycle
/// surface (`begin`, `status`, `cancel`, `reset`) maps audit-bootstrap
/// failures the same way.
fn map_audit_bootstrap_err(e: crate::error::DbError) -> OpError {
    match e {
        crate::error::DbError::Internal { message } => coded(
            "audit_bootstrap_failed",
            &format!("audit bootstrap failed: {message}"),
            None,
        ),
        other => other.to_op_error(),
    }
}

fn err_already_running() -> OpError {
    coded(
        "migration_already_running",
        "another worker is currently running this migration",
        Some(
            "another worker already holds the advisory lock. \
             Call `migrations.cancel({name, collection})` from there or wait for it to finish.",
        ),
    )
}

fn err_cancelled_on_start() -> OpError {
    coded(
        "migration_cancelled",
        "this migration was previously cancelled",
        Some(
            "run was cancelled. Call `migrations.reset({name, collection})` first, \
             or pass `{ reset: true }` to start fresh.",
        ),
    )
}

fn err_cancelled_mid_run() -> OpError {
    coded(
        "migration_cancelled",
        "migration was cancelled by an operator",
        None,
    )
}

fn err_reset_externally() -> OpError {
    coded(
        "migration_reset_externally",
        "audit row was reset by an operator while this run was in flight",
        Some(
            "another operator called `migrations.reset({name, collection})` while \
             this worker held the advisory lock. Mint a fresh wrapper via \
             `env.db.migrations.start(spec)` to resume from the new cursor.",
        ),
    )
}

fn err_not_cancellable(state: &str) -> OpError {
    coded(
        "migration_not_cancellable",
        &format!("migration in state '{state}' cannot be cancelled"),
        None,
    )
}

/// "This `Migration` wrapper is no longer live" — returned by
/// `fetchBatch` / `commitBatch` after the run has been finalised /
/// cancelled / reset. Surfaced with a recovery hint so SDK consumers
/// know how to mint a fresh wrapper.
pub(crate) fn err_not_active() -> OpError {
    coded(
        "migration_not_active",
        "this Migration wrapper has finalised",
        Some(
            "this Migration wrapper has finalised (commit `isDone: true`, cancel, or reset). \
             Mint a fresh one via `env.db.migrations.start(spec)`.",
        ),
    )
}

/// Take the lock client out for an await; the caller's future is
/// responsible for putting it back via [`return_lock_client`].
fn take_lock_client() -> Option<LockClient> {
    crate::context::with_mut(|c| c.take_mig_client())
}

/// Restore the lock client after an await.
fn return_lock_client(client: LockClient) {
    crate::context::with_mut(|c| c.return_mig_client(client));
}

fn lock_snapshot() -> Option<(String, String, i64, bool, i64)> {
    crate::context::with(|c| c.mig_lock_snapshot())
}

/// Begin a migration run. Routes connection / SQL execution through
/// the [`Backend`] facade (Stage 8e-R2).
pub async fn exec_begin(
    backend: &PostgresBackend,
    app_id: &str,
    name: &str,
    collection: &str,
    dry_run: bool,
    reset: bool,
) -> Result<String, OpError> {
    validate_collection(collection).map_err(|e| {
        coded(
            "invalid_collection",
            &format!("db: invalid collection: {e}"),
            None,
        )
    })?;
    if name.is_empty() {
        return Err(coded(
            "invalid_argument",
            "migration name must not be empty",
            None,
        ));
    }

    let already_active = crate::context::with(|c| c.has_mig_lock());
    if already_active {
        return Err(coded(
            "migration_already_active",
            "another migration is already active on this worker",
            None,
        ));
    }

    // Ensure schema + audit table exist. The B1 surface is callable
    // before any DDL deploy, so we bootstrap defensively. `CREATE
    // SCHEMA IF NOT EXISTS` + audit-table IF NOT EXISTS are both
    // idempotent.
    backend
        .ensure_app_schema(app_id)
        .await
        .map_err(|e| coded_db("create schema", e))?;
    backend
        .ensure_audit_table(app_id)
        .await
        .map_err(map_audit_bootstrap_err)?;

    let client = backend
        .acquire_dedicated_client()
        .await
        // Route through `to_op_error()` so the SQLSTATE-derived code
        // (`transient`, etc.) and its retry hint reach the SDK, rather
        // than collapsing every connection failure to `tx_connect_failed`.
        // Restored from 60ca1ad6 — silently reverted by ed697c45 during
        // the audit-rail refactor; caught by error-ux r3.
        .map_err(|e| e.to_op_error())?;

    let lock_key = format!("zs_mig:{app_id}");
    let got = backend
        .try_acquire_advisory_lock(&client, &lock_key, name)
        .await
        .map_err(|e| coded_db("advisory_lock query", e))?;
    if !got {
        // Client drops here; backend session ends; no locks were held
        // on it (since pg_try_advisory_lock returned false).
        return Err(err_already_running());
    }

    if reset {
        // Same generation bump as `exec_reset` — see Gap X.
        backend
            .reset_backfill_row_client(&client, app_id, collection, name)
            .await
            .map_err(|e| coded_db("migration reset", e))?;
    }

    let existing = backend
        .find_latest_backfill_row(&client, app_id, collection, name)
        .await
        .map_err(|e| coded_db("migration lookup", e))?;

    let (audit_id, cursor, processed, dead_letter_pks, start_generation) = if let Some(row) = existing {
        if row.status == "cancelled" {
            // Refuse — operator must reset to clear state.
            backend
                .release_advisory_lock(&client, &lock_key, name)
                .await;
            drop(client);
            return Err(err_cancelled_on_start());
        }
        backend
            .set_backfill_running(&client, app_id, row.id)
            .await
            .map_err(|e| coded_db("migration set running", e))?;
        (row.id, row.cursor, row.processed, row.dead_letter_pks, row.audit_generation)
    } else {
        let schema_version = backend
            .next_schema_version(app_id)
            .await
            .unwrap_or(1);
        let deploy_id = std::env::var("ZEROSHIP_DEPLOY_ID")
            .unwrap_or_else(|_| "cold_start".to_string());
        let id = backend
            .insert_backfill_running(
                &client,
                app_id,
                collection,
                name,
                dry_run,
                deploy_id.as_str(),
                schema_version,
            )
            .await
            .map_err(|e| coded_db("migration insert", e))?;
        if id == 0 {
            return Err(coded(
                "internal",
                "db: migration insert returned no row",
                None,
            ));
        }
        // Freshly INSERTed row — DEFAULT 0 for audit_generation.
        (id, 0i64, 0i64, Value::Array(vec![]), 0i64)
    };

    crate::context::with_mut(|c| {
        let _previous = c.set_mig_lock(MigrationLock {
            name: name.to_string(),
            collection: collection.to_string(),
            audit_id,
            dry_run,
            start_generation,
            client: Some(client),
        });
        debug_assert!(
            _previous.is_none(),
            "exec_begin: mig_lock slot already occupied"
        );
    });

    Ok(serde_json::json!({
        "auditId": audit_id,
        "cursor": cursor,
        "processed": processed,
        "status": "running",
        "deadLetterPks": dead_letter_pks,
    })
    .to_string())
}

/// Fetch a batch of rows after `cursor`.
pub async fn exec_fetch_batch(
    backend: &PostgresBackend,
    app_id: &str,
    cursor: i64,
    batch_size: i64,
) -> Result<String, OpError> {
    if batch_size <= 0 || batch_size > 10_000 {
        return Err(coded(
            "invalid_argument",
            "batchSize must be in (0, 10000]",
            None,
        ));
    }

    let Some((name, collection, _audit_id, _dry_run, _start_gen)) = lock_snapshot() else {
        return Err(coded(
            "no_active_migration",
            "migrationFetchBatch called without migrationBegin",
            None,
        ));
    };

    // Re-check cancel state under the lock client.
    let client = take_lock_client().ok_or_else(|| {
        coded(
            "no_active_migration",
            "lock client missing — migration not active",
            None,
        )
    })?;

    match backend
        .peek_latest_backfill_status(&client, app_id, &collection, &name)
        .await
    {
        Ok(status) => {
            if status.as_deref() == Some("cancelled") {
                return_lock_client(client);
                return Err(err_cancelled_mid_run());
            }
        }
        Err(e) => {
            return_lock_client(client);
            return Err(coded_db("status read", e));
        }
    }

    let schema = quote_ident(app_id);
    let table = quote_ident(&collection);
    let sql = format!(
        "SELECT * FROM {schema}.{table} WHERE id > $1::bigint ORDER BY id LIMIT $2::bigint"
    );
    let cursor_s = cursor.to_string();
    let bs_s = batch_size.to_string();
    // Use the client directly to fetch rows — we need the raw `Row`
    // values for `row_to_json` rendering, not just a count. The
    // backend trait's `client_exec` returns affected-rows only, so
    // we stay on the concrete Client here for this one SELECT.
    let rows_result = client
        .query_text_params(&sql, &[cursor_s.as_str(), bs_s.as_str()])
        .await;

    // Heartbeat — best-effort.
    let _ = backend
        .heartbeat_backfill(&client, app_id, &collection, &name)
        .await;

    return_lock_client(client);

    let rows = rows_result.map_err(|e| coded_db("migration fetch", crate::error::DbError::from_pg(&e)))?;
    let row_jsons: Vec<Value> = rows.iter().map(crate::v8_bridge::row_to_json).collect();
    Ok(Value::Array(row_jsons).to_string())
}

/// Commit (or roll back) one batch worth of updates.
///
/// `updates` is a JSON array of `{ id: number, set: { col: value, ... } }`
/// objects. `dead_letter_pks` is a JSON array of i64 row PKs the SDK
/// determined should be skipped. `next_cursor` is the highest id read
/// in this batch (so the next fetch picks up after it).
///
/// If `is_done=true`, the audit row is driven to the terminal status the
/// SDK requested (via `terminal_status` — see the `AuditTerminal` enum). The
/// advisory lock is released and the per-isolate `mig_lock` slot is cleared.
#[allow(clippy::too_many_arguments)]
pub async fn exec_commit_batch(
    backend: &PostgresBackend,
    app_id: &str,
    updates: &Value,
    dead_letter_pks: &Value,
    next_cursor: i64,
    processed_total: i64,
    is_done: bool,
    terminal_status: Option<&str>,
    error_message: Option<&str>,
) -> Result<String, OpError> {
    let Some((name, collection, audit_id, dry_run, start_generation)) = lock_snapshot() else {
        return Err(coded(
            "no_active_migration",
            "migrationCommitBatch called without migrationBegin",
            None,
        ));
    };

    let updates_arr = updates.as_array().ok_or_else(|| {
        coded("invalid_argument", "updates must be a JSON array", None)
    })?;

    let client = take_lock_client().ok_or_else(|| {
        coded("no_active_migration", "lock client missing", None)
    })?;

    // Helper: rollback + restore the client to the per-isolate slot.
    async fn rollback_and_return(backend: &PostgresBackend, client: LockClient) {
        let _ = backend.client_exec(&client, "ROLLBACK", &[]).await;
        return_lock_client(client);
    }

    // BEGIN
    if let Err(e) = backend.client_exec(&client, "BEGIN", &[]).await {
        return_lock_client(client);
        return Err(coded_db("BEGIN", e));
    }

    // Gap C: lock the audit row FOR UPDATE inside the batch's own
    // transaction so a concurrent `migrations.cancel({name, collection})`
    // on another connection serialises against the commit. If the row
    // is already `cancelled` (operator cancelled between fetchBatch
    // and commitBatch), ROLLBACK and surface a coded error so the SDK
    // can stop the loop cleanly. The row lock holds for the rest of
    // the batch's mutations, which means concurrent cancels block
    // until COMMIT — at which point they see `status='running'` flip
    // to whatever the SDK requested (or stay running for another pass).
    let locked = match backend
        .lock_audit_row_for_update(&client, app_id, audit_id)
        .await
    {
        Ok(row) => row,
        Err(e) => {
            rollback_and_return(backend, client).await;
            return Err(coded_db("audit lock", e));
        }
    };
    if let Some(row) = locked {
        if row.status == "cancelled" {
            rollback_and_return(backend, client).await;
            return Err(err_cancelled_mid_run());
        }
        // Gap X: an operator's `migrations.reset` bumps
        // `audit_generation`. If our snapshot is stale we MUST NOT
        // advance the cursor past the new reset point — abort with a
        // coded error so the SDK mints a fresh wrapper.
        if row.audit_generation != start_generation {
            rollback_and_return(backend, client).await;
            return Err(err_reset_externally());
        }
    }

    // Apply each update.
    for upd in updates_arr {
        let Some(obj) = upd.as_object() else {
            rollback_and_return(backend, client).await;
            return Err(coded(
                "invalid_argument",
                "each update entry must be an object",
                None,
            ));
        };
        let id = match obj.get("id").and_then(Value::as_i64) {
            Some(v) => v,
            None => {
                rollback_and_return(backend, client).await;
                return Err(coded(
                    "invalid_argument",
                    "each update entry must have a numeric id",
                    None,
                ));
            }
        };
        let set = obj.get("set").cloned().unwrap_or(Value::Null);
        let Some(set_obj) = set.as_object() else {
            // No fields to update — SDK returned undefined from
            // migrateOne. Skip.
            continue;
        };
        if set_obj.is_empty() {
            continue;
        }

        // Build UPDATE statement.
        let mut params: Vec<String> = vec![id.to_string()];
        let mut assignments: Vec<String> = Vec::new();
        for (col, val) in set_obj {
            // Reject reserved columns to keep the cursor stable.
            if col == "id" {
                continue;
            }
            params.push(crate::query::value_to_param(val));
            assignments.push(format!(
                "{} = ${}",
                crate::query::quote_ident(col),
                params.len()
            ));
        }
        if assignments.is_empty() {
            continue;
        }

        let schema = crate::query::quote_ident(app_id);
        let table = crate::query::quote_ident(&collection);
        let sql = format!(
            "UPDATE {schema}.{table} SET {} WHERE id = $1::bigint",
            assignments.join(", ")
        );
        let param_refs: Vec<&str> = params.iter().map(String::as_str).collect();
        if let Err(e) = backend.client_exec(&client, &sql, &param_refs).await {
            rollback_and_return(backend, client).await;
            return Err(coded_db(&format!("migration row UPDATE (id={id})"), e));
        }
    }

    // Audit row update — only persist cursor/dead_letter/processed on a
    // real run. Dry runs explicitly do NOT advance state (B1.6).
    //
    // Issued BEFORE COMMIT (migration-pipeline r3 R3-I3 / backlog [I41]):
    // the row lock acquired by `lock_audit_row_for_update` is held until
    // COMMIT, so any operator `migrations.reset(...)` that races between
    // the data UPDATEs and the progress write blocks. Issuing the
    // progress UPDATE after COMMIT released the lock first, creating a
    // window where reset clobbered the cursor we were about to write —
    // fresh runs then resumed from the stale pre-clobber cursor.
    if !dry_run {
        if let Err(e) = backend
            .update_backfill_progress(
                &client,
                app_id,
                audit_id,
                next_cursor,
                dead_letter_pks,
                processed_total,
            )
            .await
        {
            rollback_and_return(backend, client).await;
            return Err(coded_db("audit row update", e));
        }
    }

    // Commit or rollback. The audit progress UPDATE above is now part of
    // this transaction; if COMMIT fails, neither data UPDATEs nor cursor
    // advance — fresh attempts resume from the prior cursor without the
    // reset-clobber race.
    let final_sql = if dry_run { "ROLLBACK" } else { "COMMIT" };
    if let Err(e) = backend.client_exec(&client, final_sql, &[]).await {
        return_lock_client(client);
        return Err(coded_db(&format!("migration {final_sql}"), e));
    }

    // Terminal handling — if isDone, drive the row to a terminal status
    // and release the lock.
    if is_done {
        let terminal = match terminal_status.unwrap_or("applied") {
            "applied" => TerminalStatus::Applied,
            "applied_with_dead_letter" => TerminalStatus::AppliedWithDeadLetter,
            "failed" => TerminalStatus::Failed,
            "cancelled" => TerminalStatus::Cancelled,
            other => {
                return_lock_client(client);
                return Err(coded(
                    "invalid_argument",
                    &format!("unknown terminalStatus '{other}'"),
                    None,
                ));
            }
        };

        // Discarding finalise_backfill errors silently can leave the
        // audit row stuck in Running (migration-pipeline r5 R5-M7,
        // F1 family). Log via tracing::warn so operators see the
        // stall; we still continue with lock release because the row
        // state is already as-good-as-it-gets at this point.
        if let Err(e) = backend
            .finalise_backfill(&client, app_id, audit_id, terminal, error_message)
            .await
        {
            tracing::warn!(
                app_id = %app_id,
                audit_id = audit_id,
                terminal = ?terminal,
                error = %e,
                "finalise_backfill failed; audit row may stay in 'running' \
                 status until next reset() — investigate if the operator \
                 sees stuck migrations"
            );
        }

        let lock_key = format!("zs_mig:{app_id}");
        backend.release_advisory_lock(&client, &lock_key, &name).await;
        // Drop the client — backend session ends, releasing all locks.
        drop(client);
        crate::context::with_mut(|c| c.clear_mig_lock());
        return Ok(serde_json::json!({ "committed": !dry_run, "done": true }).to_string());
    }

    return_lock_client(client);
    Ok(serde_json::json!({ "committed": !dry_run, "done": false }).to_string())
}

/// Read the current audit row state for a (collection, name) pair.
/// Returns a JSON object the SDK can shape into the `status` API.
pub async fn exec_status(
    backend: &PostgresBackend,
    app_id: &str,
    name: &str,
    collection: &str,
) -> Result<String, OpError> {
    backend
        .ensure_audit_table(app_id)
        .await
        .map_err(map_audit_bootstrap_err)?;
    let row = backend
        .find_latest_backfill_row_pool(app_id, collection, name)
        .await
        .map_err(|e| coded_db("migration status read", e))?;
    let Some(row) = row else {
        return Ok(serde_json::json!({
            "exists": false,
            "status": null,
            "cursor": 0,
            "processed": 0,
            "deadLetterPks": [],
            "isDone": false,
            "error": null,
        })
        .to_string());
    };
    Ok(serde_json::json!({
        "exists": true,
        "status": row.status,
        "cursor": row.cursor,
        "processed": row.processed,
        "deadLetterPks": row.dead_letter_pks,
        "isDone": row.is_done,
        "error": row.error,
    })
    .to_string())
}

/// Cancel a migration. Allowed only when status is `pending` or
/// `running` (proposal B1, "Cancel happens-before the next batch").
/// Returns `{ ok: true }` on transition, structured error otherwise.
pub async fn exec_cancel(
    backend: &PostgresBackend,
    app_id: &str,
    name: &str,
    collection: &str,
) -> Result<String, OpError> {
    backend
        .ensure_audit_table(app_id)
        .await
        .map_err(map_audit_bootstrap_err)?;
    // Read current status.
    let row = backend
        .find_latest_backfill_row_pool(app_id, collection, name)
        .await
        .map_err(|e| coded_db("migration cancel lookup", e))?;
    let Some(row) = row else {
        return Err(err_not_cancellable("missing"));
    };
    if row.status != "pending" && row.status != "running" {
        return Err(err_not_cancellable(&row.status));
    }

    backend
        .cancel_backfill_row_pool(app_id, row.id)
        .await
        .map_err(|e| coded_db("migration cancel update", e))?;

    Ok(serde_json::json!({ "ok": true }).to_string())
}

/// Reset a migration's state (status='pending', cursor=0, processed=0,
/// dead_letter_pks=null). Used when an operator wants to retry from
/// scratch after a `cancelled` or `failed` run.
pub async fn exec_reset(
    backend: &PostgresBackend,
    app_id: &str,
    name: &str,
    collection: &str,
) -> Result<String, OpError> {
    backend
        .ensure_audit_table(app_id)
        .await
        .map_err(map_audit_bootstrap_err)?;
    // Gap X: bump `audit_generation` so any in-flight worker holding
    // the old generation aborts its next `commit_batch` with
    // `migration_reset_externally` instead of overwriting the cursor
    // we just zeroed.
    backend
        .reset_backfill_row_pool(app_id, collection, name)
        .await
        .map_err(|e| coded_db("migration reset", e))?;
    Ok(serde_json::json!({ "ok": true }).to_string())
}

/// Internal helper for the worker shutdown path — drop any active
/// migration lock so the connection is released. Safe to call when no
/// migration is active.
pub fn release_active_lock() {
    crate::context::with_mut(|c| c.clear_mig_lock());
}

// ─────────────────────────────────────────────────────────────────────
// Test-only wrappers — keep the integration tests on `Rc<Pool>`
// without forcing every call site to construct a PostgresBackend.
// Each wrapper takes a pre-wrapped `Rc<Pool>` and forwards to the
// canonical backend-taking surface above.
//
// Gated behind `cfg(any(test, feature = "test-helpers"))` so the
// symbols don't ship in release builds; the `tests/integration.rs`
// file declares `required-features = ["test-helpers"]` so it can
// still reach them. Production code goes through the v8_classes
// layer.
// ─────────────────────────────────────────────────────────────────────

#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn exec_begin_with_pool(
    pool: std::rc::Rc<compio_postgres::Pool>,
    app_id: &str,
    name: &str,
    collection: &str,
    dry_run: bool,
    reset: bool,
) -> Result<String, OpError> {
    let backend = make_test_backend(pool);
    exec_begin(&backend, app_id, name, collection, dry_run, reset).await
}

#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn exec_fetch_batch_with_pool(
    pool: std::rc::Rc<compio_postgres::Pool>,
    app_id: &str,
    cursor: i64,
    batch_size: i64,
) -> Result<String, OpError> {
    let backend = make_test_backend(pool);
    exec_fetch_batch(&backend, app_id, cursor, batch_size).await
}

#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn exec_commit_batch_with_pool(
    pool: std::rc::Rc<compio_postgres::Pool>,
    app_id: &str,
    updates: &Value,
    dead_letter_pks: &Value,
    next_cursor: i64,
    processed_total: i64,
    is_done: bool,
    terminal_status: Option<&str>,
    error_message: Option<&str>,
) -> Result<String, OpError> {
    let backend = make_test_backend(pool);
    exec_commit_batch(
        &backend,
        app_id,
        updates,
        dead_letter_pks,
        next_cursor,
        processed_total,
        is_done,
        terminal_status,
        error_message,
    )
    .await
}

#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn exec_status_with_pool(
    pool: std::rc::Rc<compio_postgres::Pool>,
    app_id: &str,
    name: &str,
    collection: &str,
) -> Result<String, OpError> {
    let backend = make_test_backend(pool);
    exec_status(&backend, app_id, name, collection).await
}

#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn exec_cancel_with_pool(
    pool: std::rc::Rc<compio_postgres::Pool>,
    app_id: &str,
    name: &str,
    collection: &str,
) -> Result<String, OpError> {
    let backend = make_test_backend(pool);
    exec_cancel(&backend, app_id, name, collection).await
}

#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn exec_reset_with_pool(
    pool: std::rc::Rc<compio_postgres::Pool>,
    app_id: &str,
    name: &str,
    collection: &str,
) -> Result<String, OpError> {
    let backend = make_test_backend(pool);
    exec_reset(&backend, app_id, name, collection).await
}

/// Build an ad-hoc PostgresBackend wrapping an owned `Rc<Pool>`.
/// Reads the URL from the per-isolate context (set by
/// `set_db_url_for_tests` in the test harness).
#[cfg(any(test, feature = "test-helpers"))]
fn make_test_backend(
    pool: std::rc::Rc<compio_postgres::Pool>,
) -> crate::backend::PostgresBackend {
    let url = crate::context::with(|c| c.db_url()).unwrap_or_default();
    crate::backend::PostgresBackend::new(pool, url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DbError;
    use zeroship_runtime::state::OpErrorKind;

    /// Regression: an `ensure_audit_table` failure that classifies as
    /// `DbError::Transient` (Postgres class 08 — connection drop, etc.)
    /// must reach JS as `.code = "transient"` with the retry hint, NOT
    /// as `.code = "audit_bootstrap_failed"`. Pre-fix the four
    /// audit_bootstrap_failed sites in this file flattened the typed
    /// `DbError` via `format!("{e:?}")`, stripping the SQLSTATE-derived
    /// classification — the SDK saw `audit_bootstrap_failed` for what
    /// is actually a transient backend failure and could not retry.
    #[test]
    fn map_audit_bootstrap_err_preserves_transient_code() {
        let e = DbError::Transient {
            message: "connection refused".into(),
        };
        let op = map_audit_bootstrap_err(e);
        match &op.kind {
            OpErrorKind::CodedError { code, hint } => {
                assert_eq!(
                    code, "transient",
                    "Transient SQLSTATE variant must reach JS as `.code = transient`, \
                     not get flattened to `audit_bootstrap_failed`"
                );
                assert!(
                    hint.is_some(),
                    "Transient must carry the retry-after-backoff hint so the SDK can act"
                );
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
        assert_eq!(op.message, "connection refused");
    }

    /// Companion: `LockContention` (Postgres 55P03 — lock not
    /// available) must reach JS as `.code = "lock_not_available"`. This
    /// surfaces on `pg_try_advisory_lock` contention paths the SDK
    /// branches on to retry-with-backoff or surface "another worker
    /// holds the lock".
    #[test]
    fn map_audit_bootstrap_err_preserves_lock_contention_code() {
        let e = DbError::LockContention {
            message: "lock not available".into(),
        };
        let op = map_audit_bootstrap_err(e);
        match &op.kind {
            OpErrorKind::CodedError { code, .. } => {
                assert_eq!(code, "lock_not_available");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }

    /// Catch-all: `Internal` (no SQLSTATE — unclassified) IS the one
    /// arm that should still be wrapped with the `audit_bootstrap_failed`
    /// code + operator-facing prefix, because there's no SDK-actionable
    /// classification to preserve.
    #[test]
    fn map_audit_bootstrap_err_wraps_internal_with_prefix() {
        let e = DbError::Internal {
            message: "some unclassified failure".into(),
        };
        let op = map_audit_bootstrap_err(e);
        match &op.kind {
            OpErrorKind::CodedError { code, .. } => {
                assert_eq!(code, "audit_bootstrap_failed");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
        assert_eq!(
            op.message,
            "audit bootstrap failed: some unclassified failure"
        );
    }
}
