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
//! [`compio_postgres::Client`] stashed in [`MIG_LOCK`]. The same client
//! runs every SELECT/UPDATE in the run because advisory locks are
//! invisible across connections.

use std::cell::RefCell;

use compio_postgres::{Client, Pool};
use serde_json::Value;
use zeroship_runtime::state::OpError;

use crate::audit::TerminalStatus;
use crate::query::{quote_ident, validate_collection};

thread_local! {
    /// Active migration owner state. `Some` after a successful
    /// `migrationBegin`; `None` once `migrationCommitBatch` with
    /// `isDone=true` (or `migrationCancel` on the owner thread) clears
    /// it. Single-isolate invariant — only one migration may be active
    /// per V8 thread at a time (mirrors `TX_CONN`).
    pub(crate) static MIG_LOCK: RefCell<Option<MigrationLock>> = const { RefCell::new(None) };
}

/// Lock state for the in-flight migration. The `client` is held in an
/// `Option` so callers can `take()` it across an await and `replace()`
/// it back — the same pattern `TX_CONN` uses.
pub(crate) struct MigrationLock {
    pub(crate) name: String,
    pub(crate) collection: String,
    pub(crate) audit_id: i64,
    /// Dry-run runs do not persist `validate_cursor`, dead_letter_pks,
    /// or processed updates (proposal B1.6).
    pub(crate) dry_run: bool,
    /// `audit_generation` snapshot captured at `exec_begin`. The audit
    /// row's generation is bumped by `exec_reset`; any subsequent
    /// `commit_batch` whose stored generation no longer matches the
    /// row's must ROLLBACK and surface `migration_reset_externally`
    /// (Gap X). Lives in the lock so `exec_commit_batch` reads it
    /// without an extra round-trip.
    pub(crate) start_generation: i64,
    pub(crate) client: Option<Client>,
}

/// Build a coded `OpError` for a migration lifecycle failure. The
/// runtime pump materialises a JS `Error` with `e.code` (and optional
/// `e.hint`) attached — the SDK branches on `e.code` directly, no
/// substring matching, no `JSON.parse(e.message)`.
fn coded(code: &str, message: &str, hint: Option<&str>) -> OpError {
    OpError::coded(code, message, hint.map(str::to_string))
}

/// SQL-error helper — classify the Postgres error through `DbError`
/// (so the resulting `OpError` carries the SQLSTATE-derived `.code` —
/// `unique_violation`, `serialization_failure`, `transient`, …)
/// instead of getting flattened by `format!("db: …")`. Use for the
/// many `map_err(|e| coded_sql("...", e))`-shaped sites where the
/// only information added is a context phrase ("migration insert
/// failed", etc.).
///
/// The phrase is prepended to the message so the operator sees both
/// the lifecycle context AND the SQLSTATE message; the `.code` stays
/// the SQLSTATE classification.
fn coded_sql(context: &str, e: compio_postgres::Error) -> OpError {
    let mut db_err = crate::error::DbError::from_pg(&e);
    // Re-wrap the message with the context phrase the original
    // `format!("db: <ctx>: {e}")` provided.
    match &mut db_err {
        crate::error::DbError::UniqueViolation { message }
        | crate::error::DbError::FkViolation { message }
        | crate::error::DbError::NotNullViolation { message }
        | crate::error::DbError::CheckViolation { message }
        | crate::error::DbError::Serialization { message }
        | crate::error::DbError::LockContention { message }
        | crate::error::DbError::Transient { message }
        | crate::error::DbError::Internal { message } => {
            *message = format!("db: {context} failed: {message}");
        }
        _ => {}
    }
    db_err.to_op_error()
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

/// Open a dedicated client. Mirrors `exec_begin` in
/// `orchestrator::transaction`.
async fn open_dedicated_client() -> Result<Client, String> {
    let url = crate::DB_URL
        .with(|u| u.borrow().clone())
        .ok_or_else(|| "db: not configured".to_string())?;
    let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .map_err(|e| format!("db: migration connect failed: {e}"))?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("db: migration connection task error: {e}");
        }
    })
    .detach();
    Ok(client)
}

/// Take the lock client out for an await; the caller's future is
/// responsible for putting it back via [`return_lock_client`].
fn take_lock_client() -> Option<Client> {
    MIG_LOCK.with(|m| m.borrow_mut().as_mut().and_then(|l| l.client.take()))
}

/// Restore the lock client after an await.
fn return_lock_client(client: Client) {
    MIG_LOCK.with(|m| {
        if let Some(lock) = m.borrow_mut().as_mut() {
            lock.client = Some(client);
        }
    });
}

fn lock_snapshot() -> Option<(String, String, i64, bool, i64)> {
    MIG_LOCK.with(|m| {
        m.borrow().as_ref().map(|l| {
            (
                l.name.clone(),
                l.collection.clone(),
                l.audit_id,
                l.dry_run,
                l.start_generation,
            )
        })
    })
}

/// Begin a migration run.
pub async fn exec_begin(
    pool: &Pool,
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

    let already_active = MIG_LOCK.with(|m| m.borrow().is_some());
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
    let create_schema = crate::query::build_create_schema(app_id);
    let empty: Vec<&str> = Vec::new();
    pool.query_text_params(&create_schema, &empty)
        .await
        .map_err(|e| coded_sql("create schema", e))?;
    crate::audit::ensure_audit_table_exists(pool, app_id)
        .await
        .map_err(|m| coded("audit_bootstrap_failed", &m, None))?;

    let client = open_dedicated_client()
        .await
        .map_err(|m| coded("tx_connect_failed", &m, None))?;

    let lock_sql =
        "SELECT pg_try_advisory_lock(hashtext('zs_mig:' || $1)::int4, hashtext($2)::int4) AS got";
    let lock_rows = client
        .query_text_params(lock_sql, &[app_id, name])
        .await
        .map_err(|e| coded_sql("advisory_lock query", e))?;
    let got: bool = lock_rows
        .first()
        .map(|r| r.try_get::<_, bool>("got").unwrap_or(false))
        .unwrap_or(false);
    if !got {
        // Client drops here; backend session ends; no locks were held
        // on it (since pg_try_advisory_lock returned false).
        return Err(err_already_running());
    }

    if reset {
        // Same generation bump as `exec_reset` — see Gap X.
        crate::audit::reset_backfill_row(&client, app_id, collection, name)
            .await
            .map_err(|e| coded_sql("migration reset", e))?;
    }

    let existing = crate::audit::find_latest_backfill_row(&client, app_id, collection, name)
        .await
        .map_err(|e| coded_sql("migration lookup", e))?;

    let (audit_id, cursor, processed, dead_letter_pks, start_generation) = if let Some(row) = existing {
        if row.status == "cancelled" {
            // Refuse — operator must reset to clear state.
            let _ = release_advisory(&client, app_id, name).await;
            drop(client);
            return Err(err_cancelled_on_start());
        }
        crate::audit::set_backfill_running(&client, app_id, row.id)
            .await
            .map_err(|e| coded_sql("migration set running", e))?;
        (row.id, row.cursor, row.processed, row.dead_letter_pks, row.audit_generation)
    } else {
        let schema_version =
            crate::audit::next_schema_version(pool, app_id).await.unwrap_or(1);
        let deploy_id = std::env::var("ZEROSHIP_DEPLOY_ID")
            .unwrap_or_else(|_| "cold_start".to_string());
        let id = crate::audit::insert_backfill_running(
            &client,
            app_id,
            collection,
            name,
            dry_run,
            deploy_id.as_str(),
            schema_version,
        )
        .await
        .map_err(|e| coded_sql("migration insert", e))?;
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

    MIG_LOCK.with(|m| {
        *m.borrow_mut() = Some(MigrationLock {
            name: name.to_string(),
            collection: collection.to_string(),
            audit_id,
            dry_run,
            start_generation,
            client: Some(client),
        });
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

    match crate::audit::peek_latest_backfill_status(&client, app_id, &collection, &name).await {
        Ok(status) => {
            if status.as_deref() == Some("cancelled") {
                return_lock_client(client);
                return Err(err_cancelled_mid_run());
            }
        }
        Err(e) => {
            return_lock_client(client);
            return Err(coded_sql("status read", e));
        }
    }

    let schema = quote_ident(app_id);
    let table = quote_ident(&collection);
    let sql = format!(
        "SELECT * FROM {schema}.{table} WHERE id > $1::bigint ORDER BY id LIMIT $2::bigint"
    );
    let cursor_s = cursor.to_string();
    let bs_s = batch_size.to_string();
    let rows_result = client
        .query_text_params(&sql, &[cursor_s.as_str(), bs_s.as_str()])
        .await;

    // Heartbeat — best-effort.
    let _ = crate::audit::heartbeat_backfill(&client, app_id, &collection, &name).await;

    return_lock_client(client);

    let rows = rows_result.map_err(|e| coded_sql("migration fetch", e))?;
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
/// SDK requested (via `terminal_status` — see [`AuditTerminal`]). The
/// advisory lock is released and `MIG_LOCK` is cleared.
#[allow(clippy::too_many_arguments)]
pub async fn exec_commit_batch(
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

    // BEGIN
    if let Err(e) = client.execute("BEGIN", &[]).await {
        return_lock_client(client);
        return Err(coded_sql("BEGIN", e));
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
    let locked = match crate::audit::lock_audit_row_for_update(&client, app_id, audit_id).await {
        Ok(row) => row,
        Err(e) => {
            let _ = client.execute("ROLLBACK", &[]).await;
            return_lock_client(client);
            return Err(coded_sql("audit lock", e));
        }
    };
    if let Some(row) = locked {
        if row.status == "cancelled" {
            let _ = client.execute("ROLLBACK", &[]).await;
            return_lock_client(client);
            return Err(err_cancelled_mid_run());
        }
        // Gap X: an operator's `migrations.reset` bumps
        // `audit_generation`. If our snapshot is stale we MUST NOT
        // advance the cursor past the new reset point — abort with a
        // coded error so the SDK mints a fresh wrapper.
        if row.audit_generation != start_generation {
            let _ = client.execute("ROLLBACK", &[]).await;
            return_lock_client(client);
            return Err(err_reset_externally());
        }
    }

    // Apply each update.
    for upd in updates_arr {
        let Some(obj) = upd.as_object() else {
            let _ = client.execute("ROLLBACK", &[]).await;
            return_lock_client(client);
            return Err(coded(
                "invalid_argument",
                "each update entry must be an object",
                None,
            ));
        };
        let id = match obj.get("id").and_then(Value::as_i64) {
            Some(v) => v,
            None => {
                let _ = client.execute("ROLLBACK", &[]).await;
                return_lock_client(client);
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
            params.push(crate::query::value_to_param_pub(val));
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
        if let Err(e) = client.query_text_params(&sql, &param_refs).await {
            let _ = client.execute("ROLLBACK", &[]).await;
            return_lock_client(client);
            return Err(coded_sql(&format!("migration row UPDATE (id={id})"), e));
        }
    }

    // Commit or rollback.
    let final_sql = if dry_run { "ROLLBACK" } else { "COMMIT" };
    if let Err(e) = client.execute(final_sql, &[]).await {
        return_lock_client(client);
        return Err(coded_sql(&format!("migration {final_sql}"), e));
    }

    // Audit row update — only persist cursor/dead_letter/processed on a
    // real run. Dry runs explicitly do NOT advance state (B1.6).
    if !dry_run {
        if let Err(e) = crate::audit::update_backfill_progress(
            &client,
            app_id,
            audit_id,
            next_cursor,
            dead_letter_pks,
            processed_total,
        )
        .await
        {
            return_lock_client(client);
            return Err(coded_sql("audit row update", e));
        }
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

        let _ = crate::audit::finalise_backfill(&client, app_id, audit_id, terminal, error_message)
            .await;

        let _ = release_advisory(&client, app_id, &name).await;
        // Drop the client — backend session ends, releasing all locks.
        drop(client);
        MIG_LOCK.with(|m| *m.borrow_mut() = None);
        return Ok(serde_json::json!({ "committed": !dry_run, "done": true }).to_string());
    }

    return_lock_client(client);
    Ok(serde_json::json!({ "committed": !dry_run, "done": false }).to_string())
}

/// Release the session advisory lock. Best-effort.
async fn release_advisory(client: &Client, app_id: &str, name: &str) -> Result<(), String> {
    let unlock_sql =
        "SELECT pg_advisory_unlock(hashtext('zs_mig:' || $1)::int4, hashtext($2)::int4)";
    let _ = client.query_text_params(unlock_sql, &[app_id, name]).await;
    Ok(())
}

/// Read the current audit row state for a (collection, name) pair.
/// Returns a JSON object the SDK can shape into the `status` API.
pub async fn exec_status(
    pool: &Pool,
    app_id: &str,
    name: &str,
    collection: &str,
) -> Result<String, OpError> {
    crate::audit::ensure_audit_table_exists(pool, app_id)
        .await
        .map_err(|m| coded("audit_bootstrap_failed", &m, None))?;
    let row = crate::audit::find_latest_backfill_row(pool, app_id, collection, name)
        .await
        .map_err(|e| coded_sql("migration status read", e))?;
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
    pool: &Pool,
    app_id: &str,
    name: &str,
    collection: &str,
) -> Result<String, OpError> {
    crate::audit::ensure_audit_table_exists(pool, app_id)
        .await
        .map_err(|m| coded("audit_bootstrap_failed", &m, None))?;
    // Read current status.
    let row = crate::audit::find_latest_backfill_row(pool, app_id, collection, name)
        .await
        .map_err(|e| coded_sql("migration cancel lookup", e))?;
    let Some(row) = row else {
        return Err(err_not_cancellable("missing"));
    };
    if row.status != "pending" && row.status != "running" {
        return Err(err_not_cancellable(&row.status));
    }

    crate::audit::cancel_backfill_row(pool, app_id, row.id)
        .await
        .map_err(|e| coded_sql("migration cancel update", e))?;

    Ok(serde_json::json!({ "ok": true }).to_string())
}

/// Reset a migration's state (status='pending', cursor=0, processed=0,
/// dead_letter_pks=null). Used when an operator wants to retry from
/// scratch after a `cancelled` or `failed` run.
pub async fn exec_reset(
    pool: &Pool,
    app_id: &str,
    name: &str,
    collection: &str,
) -> Result<String, OpError> {
    crate::audit::ensure_audit_table_exists(pool, app_id)
        .await
        .map_err(|m| coded("audit_bootstrap_failed", &m, None))?;
    // Gap X: bump `audit_generation` so any in-flight worker holding
    // the old generation aborts its next `commit_batch` with
    // `migration_reset_externally` instead of overwriting the cursor
    // we just zeroed.
    crate::audit::reset_backfill_row(pool, app_id, collection, name)
        .await
        .map_err(|e| coded_sql("migration reset", e))?;
    Ok(serde_json::json!({ "ok": true }).to_string())
}

/// Internal helper for the worker shutdown path — drop any active
/// migration lock so the connection is released. Safe to call when no
/// migration is active.
pub fn release_active_lock() {
    MIG_LOCK.with(|m| *m.borrow_mut() = None);
}
