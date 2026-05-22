//! `PostgresBackend` — the single concrete impl of [`super::Backend`].
//!
//! Wraps the `compio_postgres::Pool` and the configured URL. Every PG-
//! flavoured call moves here so consumer files (`orchestrator/*`,
//! `audit.rs`, `migrations.rs`) can stay free of `compio_postgres::Client`
//! direct references and go through the trait instead.
//!
//! The methods are intentionally thin — they forward to the existing
//! free functions in `crate::audit` / `crate::diff` / `crate::query`
//! that already do the work. The point of this file is to *name the
//! seam*, not to relocate every line of SQL.

use std::rc::Rc;

use serde_json::Value;

use crate::audit::{
    self, AuditRow, BackfillLookup, LockedAuditRow, TerminalStatus,
};
use crate::diff::LiveSchema;
use crate::error::DbError;

use super::Backend;

/// Single concrete impl of [`Backend`] backed by `compio_postgres`.
///
/// Holds the `Rc<Pool>` for the configured URL. The pool itself is
/// created by [`crate::init_pool_async`] and stashed in the per-isolate
/// context; this wrapper just provides the trait facade.
pub struct PostgresBackend {
    pool: Rc<compio_postgres::Pool>,
    /// Configured URL — used by [`Self::acquire_dedicated_client`] to
    /// open a fresh connection outside the pool (for the
    /// `db.beginTransaction()` and `migrationBegin` paths that need a
    /// connection that survives across pool-return points).
    url: String,
}

impl std::fmt::Debug for PostgresBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresBackend").finish()
    }
}

impl PostgresBackend {
    /// Build a backend handle around an already-initialised pool.
    pub fn new(pool: Rc<compio_postgres::Pool>, url: String) -> Self {
        Self { pool, url }
    }

    /// Borrow the inner pool. Provided for the few places that still
    /// need the raw `Pool` (e.g. the v8_classes layer's `ensure_pool`
    /// shim until the consumer migration completes).
    pub fn pool(&self) -> &Rc<compio_postgres::Pool> {
        &self.pool
    }

    /// Borrow the configured URL.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Backend for PostgresBackend {
    type Client = compio_postgres::Client;
    type LiveSchema = LiveSchema;

    // ----- connection lifecycle ---------------------------------------

    async fn acquire_dedicated_client(&self) -> Result<Self::Client, DbError> {
        let (client, connection) = compio_postgres::connect(&self.url, compio_postgres::NoTls)
            .await
            .map_err(|e| DbError::Transient {
                message: format!("db: backend connect failed: {e}"),
            })?;
        compio::runtime::spawn(async move {
            if let Err(e) = connection.run().await {
                eprintln!("db: backend connection task error: {e}");
            }
        })
        .detach();
        Ok(client)
    }

    async fn pool_exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        let rows = self
            .pool
            .query_text_params(sql, params)
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        Ok(rows.len() as u64)
    }

    async fn client_exec(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[&str],
    ) -> Result<u64, DbError> {
        let rows = client
            .query_text_params(sql, params)
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        Ok(rows.len() as u64)
    }

    // ----- advisory locks ---------------------------------------------

    async fn acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        // Two-key advisory lock on `(hashtext(key1)::int4,
        // hashtext(key2)::int4)`. Session-scoped — held until the
        // backend session ends or `release_advisory_lock` runs.
        let sql = "SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)";
        client
            .query_text_params(sql, &[key1, key2])
            .await
            .map_err(|e| {
                let mut err = DbError::from_pg(&e);
                // Decorate the message so operators can see which key
                // failed (the bare SQLSTATE message often doesn't show
                // the hash inputs).
                if let DbError::Internal { message }
                | DbError::Transient { message }
                | DbError::LockContention { message } = &mut err
                {
                    *message = format!("db: pg_advisory_lock({key1}, {key2}) failed: {message}");
                }
                err
            })?;
        Ok(())
    }

    async fn try_acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<bool, DbError> {
        let sql =
            "SELECT pg_try_advisory_lock(hashtext($1)::int4, hashtext($2)::int4) AS got";
        let rows = client
            .query_text_params(sql, &[key1, key2])
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        let got: bool = rows
            .first()
            .map(|r| r.try_get::<_, bool>("got").unwrap_or(false))
            .unwrap_or(false);
        Ok(got)
    }

    async fn release_advisory_lock(&self, client: &Self::Client, key1: &str, key2: &str) {
        let sql = "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
        let _ = client.query_text_params(sql, &[key1, key2]).await;
    }

    // ----- schema bootstrap + introspection ---------------------------

    async fn ensure_app_schema(&self, app_id: &str) -> Result<(), DbError> {
        let create_schema = crate::query::build_create_schema(app_id);
        let empty: Vec<&str> = Vec::new();
        self.pool
            .query_text_params(&create_schema, &empty)
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        Ok(())
    }

    async fn introspect_schema(&self, app_id: &str) -> Result<Self::LiveSchema, DbError> {
        crate::diff::read_live_schema(&self.pool, app_id)
            .await
            .map_err(|m| DbError::Internal { message: m })
    }

    async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError> {
        crate::diff::estimate_row_count(&self.pool, app_id, collection)
            .await
            .map_err(|m| DbError::Internal { message: m })
    }

    // ----- audit table reads/writes -----------------------------------

    async fn ensure_audit_table(&self, app_id: &str) -> Result<(), DbError> {
        audit::ensure_audit_table_exists(&self.pool, app_id)
            .await
            .map_err(|m| DbError::Internal { message: m })
    }

    async fn next_schema_version(&self, app_id: &str) -> Result<i32, DbError> {
        audit::next_schema_version(&self.pool, app_id)
            .await
            .map_err(|m| DbError::Internal { message: m })
    }

    async fn write_audit_row(&self, app_id: &str, row: &AuditRow) -> Result<i64, DbError> {
        audit::write_audit_row(&self.pool, app_id, row)
            .await
            .map_err(|m| DbError::Internal { message: m })
    }

    async fn update_audit_status(
        &self,
        app_id: &str,
        id: i64,
        new_status: TerminalStatus,
        error: Option<&str>,
    ) -> Result<bool, DbError> {
        audit::update_audit_status(&self.pool, app_id, id, new_status, error)
            .await
            .map_err(|m| DbError::Internal { message: m })
    }

    async fn find_latest_backfill_row(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<Option<BackfillLookup>, DbError> {
        audit::find_latest_backfill_row(client, app_id, collection, name)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }

    async fn find_latest_backfill_row_pool(
        &self,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<Option<BackfillLookup>, DbError> {
        audit::find_latest_backfill_row(self.pool.as_ref(), app_id, collection, name)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }

    async fn set_backfill_running(
        &self,
        client: &Self::Client,
        app_id: &str,
        id: i64,
    ) -> Result<(), DbError> {
        audit::set_backfill_running(client, app_id, id)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }

    async fn insert_backfill_running(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
        dry_run: bool,
        deploy_id: &str,
        schema_version: i32,
    ) -> Result<i64, DbError> {
        audit::insert_backfill_running(
            client,
            app_id,
            collection,
            name,
            dry_run,
            deploy_id,
            schema_version,
        )
        .await
        .map_err(|e| DbError::from_pg(&e))
    }

    async fn reset_backfill_row_pool(
        &self,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<(), DbError> {
        audit::reset_backfill_row(self.pool.as_ref(), app_id, collection, name)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }

    async fn reset_backfill_row_client(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<(), DbError> {
        audit::reset_backfill_row(client, app_id, collection, name)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }

    async fn peek_latest_backfill_status(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<Option<String>, DbError> {
        audit::peek_latest_backfill_status(client, app_id, collection, name)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }

    async fn heartbeat_backfill(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<(), DbError> {
        audit::heartbeat_backfill(client, app_id, collection, name)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }

    async fn lock_audit_row_for_update(
        &self,
        client: &Self::Client,
        app_id: &str,
        id: i64,
    ) -> Result<Option<LockedAuditRow>, DbError> {
        audit::lock_audit_row_for_update(client, app_id, id)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }

    async fn update_backfill_progress(
        &self,
        client: &Self::Client,
        app_id: &str,
        id: i64,
        next_cursor: i64,
        dead_letter_pks: &Value,
        processed_total: i64,
    ) -> Result<(), DbError> {
        audit::update_backfill_progress(
            client,
            app_id,
            id,
            next_cursor,
            dead_letter_pks,
            processed_total,
        )
        .await
        .map_err(|e| DbError::from_pg(&e))
    }

    async fn finalise_backfill(
        &self,
        client: &Self::Client,
        app_id: &str,
        id: i64,
        terminal: TerminalStatus,
        error_message: Option<&str>,
    ) -> Result<(), DbError> {
        audit::finalise_backfill(client, app_id, id, terminal, error_message)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }

    async fn cancel_backfill_row_pool(
        &self,
        app_id: &str,
        id: i64,
    ) -> Result<(), DbError> {
        audit::cancel_backfill_row(self.pool.as_ref(), app_id, id)
            .await
            .map_err(|e| DbError::from_pg(&e))
    }
}
