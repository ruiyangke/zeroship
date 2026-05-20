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

use crate::audit::{ActorKind, ChangeClass, TerminalStatus};
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
    pub(crate) client: Option<Client>,
}

/// Sentinel structured error envelope — matches the SDK's expected
/// `error.code` JSON shape.
fn err(code: &str, message: &str) -> String {
    serde_json::json!({ "code": code, "message": message }).to_string()
}

fn err_already_running() -> String {
    err(
        "migration_already_running",
        "another worker is currently running this migration",
    )
}

fn err_cancelled() -> String {
    err(
        "migration_cancelled",
        "migration was cancelled by an operator",
    )
}

fn err_not_cancellable(state: &str) -> String {
    err(
        "migration_not_cancellable",
        &format!("migration in state '{state}' cannot be cancelled"),
    )
}

/// Open a dedicated client. Mirrors `exec_begin` in `callbacks.rs`.
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

fn lock_snapshot() -> Option<(String, String, i64, bool)> {
    MIG_LOCK.with(|m| {
        m.borrow().as_ref().map(|l| {
            (
                l.name.clone(),
                l.collection.clone(),
                l.audit_id,
                l.dry_run,
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
) -> Result<String, String> {
    validate_collection(collection)
        .map_err(|e| format!("db: invalid collection: {e}"))?;
    if name.is_empty() {
        return Err(err("invalid_argument", "migration name must not be empty"));
    }

    let already_active = MIG_LOCK.with(|m| m.borrow().is_some());
    if already_active {
        return Err(err(
            "migration_already_active",
            "another migration is already active on this worker",
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
        .map_err(|e| format!("db: create schema failed: {e}"))?;
    crate::audit::ensure_audit_table_exists(pool, app_id).await?;

    let client = open_dedicated_client().await?;

    let lock_sql =
        "SELECT pg_try_advisory_lock(hashtext('zs_mig:' || $1)::int4, hashtext($2)::int4) AS got";
    let lock_rows = client
        .query_text_params(lock_sql, &[app_id, name])
        .await
        .map_err(|e| format!("db: advisory_lock query failed: {e}"))?;
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
        let sql = format!(
            r#"UPDATE "{app_id}"."__zeroship_migrations"
                SET status = 'pending',
                    validate_cursor = NULL,
                    dead_letter_pks = NULL,
                    error = NULL,
                    applied_at = NULL,
                    updated_at = NOW(),
                    details = jsonb_set(COALESCE(details, '{{}}'::jsonb), '{{processed}}', '0'::jsonb)
                WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2"#
        );
        client
            .query_text_params(&sql, &[collection, name])
            .await
            .map_err(|e| format!("db: migration reset failed: {e}"))?;
    }

    let lookup_sql = format!(
        r#"SELECT id, status, validate_cursor, dead_letter_pks, details
            FROM "{app_id}"."__zeroship_migrations"
            WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2
            ORDER BY id DESC LIMIT 1"#
    );
    let existing = client
        .query_text_params(&lookup_sql, &[collection, name])
        .await
        .map_err(|e| format!("db: migration lookup failed: {e}"))?;

    let (audit_id, cursor, processed, dead_letter_pks) = if let Some(row) = existing.first() {
        let id: i64 = row.get("id");
        let status: String = row.get("status");
        if status == "cancelled" {
            // Refuse — operator must reset to clear state.
            let _ = release_advisory(&client, app_id, name).await;
            drop(client);
            return Err(err_cancelled());
        }
        let cursor: i64 = row.try_get::<_, i64>("validate_cursor").unwrap_or(0);
        let processed = read_processed_from_row(row);
        let dlp = read_dead_letter_pks(row);

        let upd_sql = format!(
            r#"UPDATE "{app_id}"."__zeroship_migrations"
                SET status = 'running',
                    owner_session_id = pg_backend_pid()::text,
                    last_heartbeat_at = NOW(),
                    updated_at = NOW(),
                    error = NULL
                WHERE id = $1::bigint"#
        );
        let id_s = id.to_string();
        client
            .query_text_params(&upd_sql, &[id_s.as_str()])
            .await
            .map_err(|e| format!("db: migration set running failed: {e}"))?;
        (id, cursor, processed, dlp)
    } else {
        let schema_version =
            crate::audit::next_schema_version(pool, app_id).await.unwrap_or(1);
        let details = serde_json::json!({
            "processed": 0,
            "dryRun": dry_run,
        });
        let sql = format!(
            r#"INSERT INTO "{app_id}"."__zeroship_migrations"
                (collection, phase, change_class, change_kind, details,
                 ddl_sql, status, deploy_id, applied_by_kind, schema_version,
                 owner_session_id, last_heartbeat_at, validate_cursor)
                VALUES ($1, 'backfill', $2, $3, $4::jsonb,
                        NULL, 'running', $5, $6, $7::integer,
                        pg_backend_pid()::text, NOW(), 0)
                RETURNING id"#
        );
        let deploy_id = std::env::var("ZEROSHIP_DEPLOY_ID")
            .unwrap_or_else(|_| "cold_start".to_string());
        let details_s = details.to_string();
        let sv_s = schema_version.to_string();
        let rows = client
            .query_text_params(
                &sql,
                &[
                    collection,
                    ChangeClass::Additive.as_sql(),
                    name,
                    details_s.as_str(),
                    deploy_id.as_str(),
                    ActorKind::Auto.as_sql(),
                    sv_s.as_str(),
                ],
            )
            .await
            .map_err(|e| format!("db: migration insert failed: {e}"))?;
        let id: i64 = rows
            .first()
            .map(|r| r.get::<_, i64>("id"))
            .ok_or_else(|| "db: migration insert returned no row".to_string())?;
        (id, 0i64, 0i64, Value::Array(vec![]))
    };

    MIG_LOCK.with(|m| {
        *m.borrow_mut() = Some(MigrationLock {
            name: name.to_string(),
            collection: collection.to_string(),
            audit_id,
            dry_run,
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

fn read_processed_from_row(row: &compio_postgres::Row) -> i64 {
    let bytes = row.raw_value("details");
    let Some(bytes) = bytes else { return 0 };
    if bytes.len() < 2 {
        return 0;
    }
    let json_str = std::str::from_utf8(&bytes[1..]).unwrap_or("null");
    let parsed: Value = serde_json::from_str(json_str).unwrap_or(Value::Null);
    parsed
        .get("processed")
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

fn read_dead_letter_pks(row: &compio_postgres::Row) -> Value {
    let bytes = row.raw_value("dead_letter_pks");
    let Some(bytes) = bytes else { return Value::Array(vec![]) };
    if bytes.len() < 2 {
        return Value::Array(vec![]);
    }
    let json_str = std::str::from_utf8(&bytes[1..]).unwrap_or("null");
    serde_json::from_str(json_str).unwrap_or(Value::Array(vec![]))
}

/// Fetch a batch of rows after `cursor`.
pub async fn exec_fetch_batch(
    app_id: &str,
    cursor: i64,
    batch_size: i64,
) -> Result<String, String> {
    if batch_size <= 0 || batch_size > 10_000 {
        return Err(err("invalid_argument", "batchSize must be in (0, 10000]"));
    }

    let Some((name, collection, _audit_id, _dry_run)) = lock_snapshot() else {
        return Err(err(
            "no_active_migration",
            "migrationFetchBatch called without migrationBegin",
        ));
    };

    // Re-check cancel state under the lock client.
    let client = take_lock_client().ok_or_else(|| {
        err("no_active_migration", "lock client missing — migration not active")
    })?;

    let status_sql = format!(
        r#"SELECT status FROM "{app_id}"."__zeroship_migrations"
            WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2
            ORDER BY id DESC LIMIT 1"#
    );
    let status_rows = client
        .query_text_params(&status_sql, &[collection.as_str(), name.as_str()])
        .await;
    match status_rows {
        Ok(rows) => {
            let status = rows
                .first()
                .map(|r| r.get::<_, String>("status"))
                .unwrap_or_default();
            if status == "cancelled" {
                return_lock_client(client);
                return Err(err_cancelled());
            }
        }
        Err(e) => {
            return_lock_client(client);
            return Err(format!("db: status read failed: {e}"));
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
    let _ = client
        .query_text_params(
            &format!(
                r#"UPDATE "{app_id}"."__zeroship_migrations"
                    SET last_heartbeat_at = NOW()
                    WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2 AND status = 'running'"#
            ),
            &[collection.as_str(), name.as_str()],
        )
        .await;

    return_lock_client(client);

    let rows = rows_result.map_err(|e| format!("db: migration fetch failed: {e}"))?;
    let row_jsons: Vec<Value> = rows.iter().map(crate::callbacks::row_to_json).collect();
    Ok(serde_json::json!({ "rows": row_jsons }).to_string())
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
) -> Result<String, String> {
    let Some((name, collection, audit_id, dry_run)) = lock_snapshot() else {
        return Err(err(
            "no_active_migration",
            "migrationCommitBatch called without migrationBegin",
        ));
    };

    let updates_arr = updates.as_array().ok_or_else(|| {
        err("invalid_argument", "updates must be a JSON array")
    })?;

    let client = take_lock_client().ok_or_else(|| {
        err("no_active_migration", "lock client missing")
    })?;

    // BEGIN
    if let Err(e) = client.execute("BEGIN", &[]).await {
        return_lock_client(client);
        return Err(format!("db: BEGIN failed: {e}"));
    }

    // Apply each update.
    for upd in updates_arr {
        let Some(obj) = upd.as_object() else {
            let _ = client.execute("ROLLBACK", &[]).await;
            return_lock_client(client);
            return Err(err(
                "invalid_argument",
                "each update entry must be an object",
            ));
        };
        let id = match obj.get("id").and_then(Value::as_i64) {
            Some(v) => v,
            None => {
                let _ = client.execute("ROLLBACK", &[]).await;
                return_lock_client(client);
                return Err(err(
                    "invalid_argument",
                    "each update entry must have a numeric id",
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
            return Err(format!("db: migration row UPDATE failed (id={id}): {e}"));
        }
    }

    // Commit or rollback.
    let final_sql = if dry_run { "ROLLBACK" } else { "COMMIT" };
    if let Err(e) = client.execute(final_sql, &[]).await {
        return_lock_client(client);
        return Err(format!("db: migration {final_sql} failed: {e}"));
    }

    // Audit row update — only persist cursor/dead_letter/processed on a
    // real run. Dry runs explicitly do NOT advance state (B1.6).
    if !dry_run {
        let dlp_str = dead_letter_pks.to_string();
        let nc_s = next_cursor.to_string();
        let pt_s = processed_total.to_string();
        let id_s = audit_id.to_string();
        let upd_sql = format!(
            r#"UPDATE "{app_id}"."__zeroship_migrations"
                SET validate_cursor = $2::bigint,
                    dead_letter_pks = $3::jsonb,
                    details = jsonb_set(COALESCE(details, '{{}}'::jsonb), '{{processed}}', to_jsonb($4::bigint)),
                    last_heartbeat_at = NOW(),
                    updated_at = NOW()
                WHERE id = $1::bigint"#
        );
        if let Err(e) = client
            .query_text_params(&upd_sql, &[id_s.as_str(), nc_s.as_str(), dlp_str.as_str(), pt_s.as_str()])
            .await
        {
            return_lock_client(client);
            return Err(format!("db: audit row update failed: {e}"));
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
                return Err(err(
                    "invalid_argument",
                    &format!("unknown terminalStatus '{other}'"),
                ));
            }
        };

        let term_sql = format!(
            r#"UPDATE "{app_id}"."__zeroship_migrations"
                SET status = $2,
                    error = COALESCE($3, error),
                    updated_at = NOW(),
                    applied_at = CASE
                        WHEN $2 IN ('applied','applied_with_dead_letter') AND applied_at IS NULL THEN NOW()
                        ELSE applied_at
                    END,
                    owner_session_id = NULL
                WHERE id = $1::bigint AND status IN ('running','pending')"#
        );
        let id_s = audit_id.to_string();
        let err_s = error_message.unwrap_or("").to_string();
        let _ = client
            .query_text_params(
                &term_sql,
                &[id_s.as_str(), terminal.as_sql(), err_s.as_str()],
            )
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
) -> Result<String, String> {
    crate::audit::ensure_audit_table_exists(pool, app_id).await?;
    let sql = format!(
        r#"SELECT id, status, validate_cursor, dead_letter_pks, details, error
            FROM "{app_id}"."__zeroship_migrations"
            WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2
            ORDER BY id DESC LIMIT 1"#
    );
    let rows = pool
        .query_text_params(&sql, &[collection, name])
        .await
        .map_err(|e| format!("db: migration status read failed: {e}"))?;
    let Some(row) = rows.first() else {
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
    let status: String = row.get("status");
    let cursor: i64 = row.try_get::<_, i64>("validate_cursor").unwrap_or(0);
    let processed = read_processed_from_row(row);
    let dlp = read_dead_letter_pks(row);
    // SQL NULL and empty-string both mean "no error". The migrations
    // SDK's parseNative treats any string in `error` as a thrown
    // exception (`throw new Error(errVal)`), so emitting `""` would
    // surface as a zero-message failure on the caller side.
    let error: Option<String> = row
        .try_get::<_, String>("error")
        .ok()
        .filter(|s| !s.is_empty());
    let is_done = matches!(
        status.as_str(),
        "applied" | "applied_with_dead_letter" | "failed" | "cancelled"
    );
    Ok(serde_json::json!({
        "exists": true,
        "status": status,
        "cursor": cursor,
        "processed": processed,
        "deadLetterPks": dlp,
        "isDone": is_done,
        "error": error,
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
) -> Result<String, String> {
    crate::audit::ensure_audit_table_exists(pool, app_id).await?;
    // Read current status.
    let lookup_sql = format!(
        r#"SELECT id, status FROM "{app_id}"."__zeroship_migrations"
            WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2
            ORDER BY id DESC LIMIT 1"#
    );
    let rows = pool
        .query_text_params(&lookup_sql, &[collection, name])
        .await
        .map_err(|e| format!("db: migration cancel lookup failed: {e}"))?;
    let Some(row) = rows.first() else {
        return Err(err_not_cancellable("missing"));
    };
    let status: String = row.get("status");
    if status != "pending" && status != "running" {
        return Err(err_not_cancellable(&status));
    }
    let id: i64 = row.get("id");

    let upd_sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET status = 'cancelled',
                updated_at = NOW(),
                owner_session_id = NULL,
                error = COALESCE(error, 'cancelled by operator')
            WHERE id = $1::bigint AND status IN ('pending','running')"#
    );
    let id_s = id.to_string();
    pool.query_text_params(&upd_sql, &[id_s.as_str()])
        .await
        .map_err(|e| format!("db: migration cancel update failed: {e}"))?;

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
) -> Result<String, String> {
    crate::audit::ensure_audit_table_exists(pool, app_id).await?;
    let upd_sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET status = 'pending',
                validate_cursor = NULL,
                dead_letter_pks = NULL,
                error = NULL,
                applied_at = NULL,
                updated_at = NOW(),
                details = jsonb_set(COALESCE(details, '{{}}'::jsonb), '{{processed}}', '0'::jsonb)
            WHERE collection = $1 AND phase = 'backfill' AND change_kind = $2"#
    );
    pool.query_text_params(&upd_sql, &[collection, name])
        .await
        .map_err(|e| format!("db: migration reset failed: {e}"))?;
    Ok(serde_json::json!({ "ok": true }).to_string())
}

/// Internal helper for the worker shutdown path — drop any active
/// migration lock so the connection is released. Safe to call when no
/// migration is active.
pub fn release_active_lock() {
    MIG_LOCK.with(|m| *m.borrow_mut() = None);
}
