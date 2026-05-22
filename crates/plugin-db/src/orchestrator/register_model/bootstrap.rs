//! Stage 1 — Bootstrap.
//!
//! Idempotent setup that has to happen before any diff/apply work:
//!
//! 1. Acquire the session-scoped advisory lock on `(app_id,
//!    'register_model')` via a dedicated pool client. The lock survives
//!    the non-transactional `CREATE INDEX CONCURRENTLY` phase because
//!    [`apply`](super::apply) drops the client (releasing the lock)
//!    between pass 1 and pass 2.
//! 2. `CREATE SCHEMA IF NOT EXISTS` for the app.
//! 3. `CREATE TABLE IF NOT EXISTS __zeroship_migrations` (delegated to
//!    `crate::audit::ensure_audit_table_exists`).
//! 4. Compute `schema_version` from `MAX(schema_version) + 1` over the
//!    audited DDL history.
//! 5. Expand declared inline indexes + named indexes into a single
//!    `Vec<IndexSpec>` the plan stage will diff against `pg_catalog`.
//!
//! Returns a [`RegisterContext`] the later stages thread through.

use compio_postgres::{Pool, PooledClient};
use serde_json::Value;

use crate::query;

/// Threaded context produced by `bootstrap`. Owns the lock client so
/// `apply` can drop it (releasing the advisory lock) between transactional
/// DDL and CIC.
///
/// Lifetime `'p` ties the held `PooledClient` to the borrowed `Pool`
/// the caller passed in; the context cannot outlive the pool reference.
pub(crate) struct RegisterContext<'p> {
    /// App identifier — schema name, audit-row key.
    pub app_id: String,
    /// Deploy identifier — audit-row grouping key. `'cold_start'` for
    /// pre-deploy bootstrap.
    pub deploy_id: String,
    /// `schema_version` snapshot captured AFTER `ensure_audit_table_exists`
    /// runs, so each DDL row gets a monotonic version. Set once at
    /// bootstrap and shared across every audit write in this run.
    pub schema_version: i32,
    /// Strictness from `schema._meta.strictness` (defaults to `strict`).
    /// Read once at bootstrap so the validate stage can branch without
    /// re-walking the JSON.
    pub strictness: String,
    /// Expanded index list — declared inline + named — passed into
    /// `compute_diff` so the plan correctly identifies which need
    /// `CREATE INDEX CONCURRENTLY`.
    pub declared_indexes: Vec<query::IndexSpec>,
    /// Dedicated pool client holding the session advisory lock. Moves
    /// into `apply`; dropped between pass 1 (transactional) and pass 2
    /// (CIC) so the second pass runs unlocked.
    pub lock_client: PooledClient<'p>,
}

/// Run stage 1.
///
/// Order is load-bearing: the schema must exist before
/// `ensure_audit_table_exists` runs, the audit table must exist before
/// `next_schema_version` reads from it, the lock must be held before
/// any diff/apply work to serialise concurrent deploys per-app.
pub(crate) async fn bootstrap<'p>(
    pool: &'p Pool,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    deploy_id: &str,
) -> Result<RegisterContext<'p>, String> {
    // Strictness — proposal A2 line 122. Read from schema._meta.strictness
    // if present; default is 'strict'.
    let strictness = schema
        .get("_meta")
        .and_then(|m| m.get("strictness"))
        .and_then(Value::as_str)
        .unwrap_or("strict")
        .to_string();

    let empty: Vec<&str> = Vec::new();

    // -------------------------------------------------------------------
    // Concurrent-deploy serialisation: proposal A2 line 202.
    //
    // Two-key advisory lock keyed on (app_id, register_model). Held at
    // session scope on a dedicated pool client so the lock survives the
    // CREATE INDEX CONCURRENTLY phases (which can't run in a transaction).
    // Released when `apply` drops the client at the end of pass 1.
    //
    // The proposal calls for `pg_advisory_xact_lock` (transaction scope);
    // because registerModel spans non-transactional CONCURRENTLY DDL, we
    // use the session-scoped equivalent `pg_advisory_lock` on a dedicated
    // connection. Functionally identical for our serialisation goal: a
    // second worker calling the same function blocks on the same key.
    let lock_client = pool
        .get()
        .await
        .map_err(|e| format!("db: failed to acquire orchestrator client: {e}"))?;
    let lock_sql =
        "SELECT pg_advisory_lock(hashtext('zs_reg:' || $1)::int4, hashtext('register_model')::int4)";
    lock_client
        .query_text_params(lock_sql, &[app_id])
        .await
        .map_err(|e| format!("db: pg_advisory_lock failed: {e}"))?;
    // From here on, any other orchestrator call against the same app_id
    // blocks until lock_client is dropped.

    // -------------------------------------------------------------------
    // Schema + audit table.
    // -------------------------------------------------------------------
    let create_schema = query::build_create_schema(app_id);
    pool.query_text_params(&create_schema, &empty)
        .await
        .map_err(|e| format!("db: create schema failed: {e}"))?;

    crate::audit::ensure_audit_table_exists(pool, app_id).await?;

    let schema_version = crate::audit::next_schema_version(pool, app_id).await?;

    let mut declared_indexes = query::build_create_indexes(app_id, collection, schema)
        .map_err(|e| format!("db: {e}"))?;
    let named_indexes = query::build_named_indexes(app_id, collection, indexes)
        .map_err(|e| format!("db: {e}"))?;
    declared_indexes.extend(named_indexes);

    Ok(RegisterContext {
        app_id: app_id.to_string(),
        deploy_id: deploy_id.to_string(),
        schema_version,
        strictness,
        declared_indexes,
        lock_client,
    })
}
