//! SQL execution helpers — the only consumer of `compio_postgres::Pool`
//! on the CRUD hot path.
//!
//! Every CRUD dispatch helper (in `crate::crud`) lowers its
//! `BuiltQuery` through one of:
//!
//! - `exec_query` — read path, returns rows as `Vec<serde_json::Value>`
//!   (one `Value::Object` per row); the CRUD resolver serialises once
//!   at the V8 boundary.
//! - `exec_count` — read path that extracts a single `count` column.
//! - `exec_mutation` — write path, returns RETURNING rows as
//!   `Vec<serde_json::Value>`.
//! - `exec_mutation_with_emit` — write path + broker wakeup on backends
//!   that still need SDK-local publication.
//!
//! All four route through `run_sql`, which transparently uses the
//! per-isolate TX client (`IsolateDbContext::tx_conn`) when an explicit
//! transaction is active and the pool otherwise.
//!
//! ## Error rail
//!
//! Every fallible helper here returns [`crate::error::DbError`] — the
//! `dispatch_*` layer in `crate::crud` calls
//! [`crate::error::DbError::to_op_error`] at the V8 boundary so each
//! throw carries `.code` for the SDK to branch on (replaces the
//! pre-stage-8b `Result<_, String>` rail).
//!
//! The transaction-emit deferral (Gap B closure) lives here:
//! `queue_or_emit` decides between immediate emit and TX-pending
//! queueing; `drain_pending_emits_on_commit` fires the queue on
//! COMMIT; `clear_pending_emits` discards it on ROLLBACK.

use std::rc::Rc;

use serde_json::Value;

use crate::backend::BackendHandle;
use crate::context::TxConnection;
use crate::context;
use crate::error::DbError;
use crate::query::BuiltQuery;
use crate::v8_bridge::rows_to_json_value;

fn sqlite_shared_crud_unavailable() -> DbError {
    DbError::Configuration {
        code: "backend_unsupported",
        message: "this operation requires the Postgres pool-backed backend".to_string(),
        hint: Some(
            "The SQLite backend does not expose a compio_postgres::Pool. Route row reads/writes through the backend handle instead."
                .to_string(),
        ),
    }
}

async fn ensure_backend_for_shared_sql() -> Result<BackendHandle, DbError> {
    if context::with(|c| c.backend().is_none()) {
        crate::init_pool_async()
            .await
            .map_err(|e| DbError::config("lazy_init_failed", format!("db: lazy init failed: {e}")))?;
    }

    context::with(|c| c.backend()).ok_or_else(|| {
        DbError::config("not_configured", "db: backend not initialized".to_string())
    })
}

async fn ensure_postgres_pool_for_shared_sql() -> Result<Rc<compio_postgres::Pool>, DbError> {
    if matches!(
        ensure_backend_for_shared_sql().await?,
        crate::backend::BackendHandle::Sqlite(_)
    ) {
        return Err(sqlite_shared_crud_unavailable());
    }

    context::with(|c| c.pool()).ok_or_else(|| {
        DbError::config("not_configured", "db: pool not initialized".to_string())
    })
}

/// Execute SQL with text params — uses TX connection if active, otherwise pool.
pub(crate) async fn run_sql(
    app_id: &str,
    sql: &str,
    params: &[&str],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    // Check if there's an active transaction *owned by this app*.
    // SEC-1: a tx parked by a co-resident app must NOT capture this
    // app's SQL — `has_tx_for(app_id)` reads `false` for another app's
    // slot, so we fall through to this app's own autocommit path.
    let has_tx = context::with(|c| c.has_tx_for(app_id));
    if has_tx {
        // Use this app's transaction connection
        let client = context::with_mut(|c| c.take_tx_client_for(app_id))
            .ok_or_else(|| DbError::internal("db: transaction connection lost"))?;
        let result = match &client {
            TxConnection::Postgres(client) => client.query_text_params(sql, params).await,
            TxConnection::Sqlite(_) => {
                context::with_mut(|c| c.put_tx_client_for(app_id, client));
                return Err(sqlite_shared_crud_unavailable());
            }
        };
        // Put it back
        context::with_mut(|c| c.put_tx_client_for(app_id, client));
        return result.map_err(|e| DbError::from_pg(&e));
    }

    // No transaction — use pool. On the SQLite arm the shared CRUD
    // row-returning path is not wired yet; surface a typed error
    // instead of falling through to a misleading `pool not initialized`.
    exec_postgres_autocommit_with_role(app_id, sql, params).await
}

/// Execute a built query via pool (or TX conn) and return the
/// per-row JSON values.
///
/// Returning `Vec<Value>` (rather than a pre-serialised JSON array
/// string) lets the CRUD resolver chain in `crate::crud` inspect or
/// take a single row without paying for an intermediate serialise +
/// reparse round-trip. The final JSON string is materialised once at
/// the V8 boundary (`ResolveValue::Json`).
pub(crate) async fn exec_query(app_id: &str, bq: BuiltQuery) -> Result<Vec<Value>, DbError> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    if let BackendHandle::Sqlite(sq) = ensure_backend_for_shared_sql().await? {
        return exec_sqlite_json(app_id, &sq, &bq.sql, &param_refs).await;
    }
    let rows = run_sql(app_id, &bq.sql, &param_refs).await?;
    Ok(rows_to_json_value(&rows))
}

/// Execute a built query expecting a count result.
///
/// Returns the raw integer; callers wrap into the appropriate
/// `OpResult` shape (typically `ResolveValue::F64` so JS sees a real
/// `number`).
pub(crate) async fn exec_count(app_id: &str, bq: BuiltQuery) -> Result<i64, DbError> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    if let BackendHandle::Sqlite(sq) = ensure_backend_for_shared_sql().await? {
        let rows = exec_sqlite_json(app_id, &sq, &bq.sql, &param_refs).await?;
        return Ok(rows
            .first()
            .and_then(|row| row.get("count"))
            .and_then(Value::as_i64)
            .unwrap_or(0));
    }
    let rows = run_sql(app_id, &bq.sql, &param_refs).await?;

    Ok(rows
        .first()
        .map(|r| r.get::<_, i64>("count"))
        .unwrap_or(0))
}

/// Execute an insert/update/delete query, returning the affected
/// rows as `Vec<serde_json::Value>` (one `Value::Object` per row).
///
/// Returning the typed intermediate (instead of a pre-serialised JSON
/// string) lets [`exec_mutation_with_emit`] iterate the live `Value`s
/// to build broker events without paying for a JSON parse of its own
/// output; the CRUD resolver chain then serialises once at the V8
/// boundary.
pub(crate) async fn exec_mutation(app_id: &str, bq: BuiltQuery) -> Result<Vec<Value>, DbError> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    if let BackendHandle::Sqlite(sq) = ensure_backend_for_shared_sql().await? {
        return exec_sqlite_json(app_id, &sq, &bq.sql, &param_refs).await;
    }
    let rows = run_sql(app_id, &bq.sql, &param_refs).await?;
    Ok(rows_to_json_value(&rows))
}

async fn exec_postgres_autocommit_with_role(
    app_id: &str,
    sql: &str,
    params: &[&str],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    let pool = ensure_postgres_pool_for_shared_sql().await?;
    query_postgres_pool_with_autocommit_role(&pool, app_id, sql, params).await
}

pub(crate) async fn query_postgres_pool_with_autocommit_role(
    pool: &Rc<compio_postgres::Pool>,
    app_id: &str,
    sql: &str,
    params: &[&str],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    let mut client = pool.get().await.map_err(|e| DbError::from_pg(&e))?;

    // P2-C1: run the per-app role + DB-1 timeout guards via `SET LOCAL`
    // inside an explicit transaction, exactly like the explicit-tx path
    // (`tx_session_setup_sql`). `SET LOCAL` auto-reverts at COMMIT and at
    // the implicit ROLLBACK the `compio_postgres::Transaction` issues on
    // drop — so a setup error, a query error, OR a cancellation between
    // setup and the would-be reset can no longer leave the pooled
    // connection carrying this tenant's role + timeouts for the next
    // checkout. The previous shape ran a session-level `SET ROLE` + a
    // separate `RESET` that was skipped entirely when the future was
    // cancelled mid-flight (no RAII guard, and the pool's Drop is
    // synchronous so it cannot issue async RESET SQL).
    let tx = client.transaction().await.map_err(|e| {
        let mut err = DbError::from_pg(&e);
        crate::error::prefix_message(
            &mut err,
            "db: autocommit BEGIN (per-app §17.5 + DB-1 guards): ",
        );
        err
    })?;

    let setup_sql = crate::auth::bootstrap::autocommit_local_session_setup_sql(app_id);
    tx.simple_query(&setup_sql).await.map_err(|e| {
        let mut err = DbError::from_pg(&e);
        crate::error::prefix_message(
            &mut err,
            "db: autocommit session setup (per-app §17.5 + DB-1 guards): ",
        );
        err
    })?;

    let rows = tx
        .query_text_params(sql, params)
        .await
        .map_err(|e| DbError::from_pg(&e))?;

    // COMMIT reverts the SET LOCAL state and releases the connection
    // clean. On any early return above, `tx` is dropped instead, which
    // rolls back (also reverting the SET LOCAL state) and marks the
    // connection dirty so the pool drains it before the next checkout.
    tx.commit().await.map_err(|e| {
        let mut err = DbError::from_pg(&e);
        crate::error::prefix_message(
            &mut err,
            "db: autocommit COMMIT (per-app §17.5 + DB-1 guards): ",
        );
        err
    })?;

    Ok(rows)
}

async fn exec_sqlite_json(
    app_id: &str,
    backend: &crate::backend::sqlite::SqliteBackend,
    sql: &str,
    params: &[&str],
) -> Result<Vec<Value>, DbError> {
    // SEC-1: only this app's parked tx routes its SQL through the tx
    // client; a co-resident app's tx is invisible here and we use the
    // shared autocommit path instead.
    let has_tx = context::with(|c| c.has_tx_for(app_id));
    if !has_tx {
        #[cfg(test)]
        tests::record_sqlite_shared_route();
        return backend.query_json(sql, params).await;
    }

    let client = context::TxClientSlotGuard::take(app_id)?;
    let result = match client.client() {
        TxConnection::Sqlite(client) => {
            #[cfg(test)]
            tests::record_sqlite_tx_route();
            let typed = client.query_typed_internal(sql, params).await?;
            Ok(crate::v8_bridge::typed_rows_to_json_value(&typed))
        }
        TxConnection::Postgres(_) => Err(DbError::internal(
            "db: sqlite backend active with postgres transaction connection",
        )),
    };
    result
}

/// Execute a mutation, then emit a [`crate::wal_consumer::emit_local`]
/// event into the in-process broker on success.
///
/// This is the P8a coarse-grained reactive-query bridge: every
/// successful INSERT/UPDATE/DELETE produces one or more events on
/// `(app_id, collection)` that wake any matching subscribers in the
/// same isolate.
///
/// On error the broker is untouched — partial writes produce no
/// events. The error message is forwarded verbatim.
///
/// `op` selects the [`crate::broker::ChangeOp`] tagged on the event;
/// the caller knows whether it called `build_insert`, `build_update_one`,
/// `build_delete_one`, etc. so we don't try to infer it from the SQL.
///
/// Future read-set narrowing (P8b) extends this helper to populate
/// `changed_columns` from the SET clause and the logical `id` from the
/// RETURNING row. For P8a we collect what's already in the result
/// `Value`.
pub(crate) async fn exec_mutation_with_emit(
    bq: BuiltQuery,
    app_id: &str,
    collection: &str,
    op: crate::broker::ChangeOp,
) -> Result<Vec<Value>, DbError> {
    // `exec_mutation` returns the typed `Vec<Value>` already decoded
    // from `compio_postgres::Row`. Pre-fix we re-parsed our own JSON
    // string here just to extract the PK + changed-column set; now we
    // iterate the live `Value`s directly. The CRUD resolver in
    // `crud.rs` does the final `Value::Array(rows).to_string()` once
    // at the V8 boundary.
    let rows = exec_mutation(app_id, bq).await?;
    emit_for_rows(&rows, app_id, collection, op);
    Ok(rows)
}

fn backend_publishes_committed_changes() -> bool {
    context::with(|c| matches!(c.backend(), Some(BackendHandle::Sqlite(_))))
}

/// Build and queue/emit broker events for a mutation's RETURNING rows.
///
/// Split out of [`exec_mutation_with_emit`] so the gating logic can be
/// unit-tested without a Postgres connection: callers pass the already-
/// fetched `Vec<Value>` and we run the same gate + per-row build the
/// production path runs.
///
/// Perf CRITICAL N3-C1 (mirrors `wal_consumer::emit_for_tuple`'s R2 N-C1
/// fix on the cross-worker path): short-circuit the per-row
/// `(columns, tuple)` build before allocating anything the broker will
/// discard.
///
/// Two gates, both cheap:
///
///   1. `wal_consumer::is_app_suppressed(app_id)` — when the WAL
///      consumer is running for this app, it owns the publish path for
///      events this isolate writes. The corresponding `emit_local` call
///      would be a no-op, so building the tuple is pure waste. In
///      production with the consumer active, EVERY mutation previously
///      paid the build cost only to discard the result inside
///      `emit_local`.
///
///   2. `broker::has_subscribers(app_id, collection)` — a single
///      `HashMap::get` on the thread-local broker. On a table with no
///      reactive subscribers (the common case for most mutations) the
///      event would otherwise be built only for `broker::publish` to
///      drop it. Conservative-true semantics: a subscriber added
///      between this check and a subsequent mutation will see THAT
///      mutation's event — no race, broker and exec helpers run on the
///      same compio thread.
///
/// Both gates have to pass to do the work. We skip the in-tx queue
/// path too: when in a transaction with no subscribers, queueing for
/// the COMMIT-time drain would just defer the discard. Subscribers
/// added mid-transaction would miss the event, mirroring the WAL
/// consumer's same conservative-true contract.
fn emit_for_rows(
    rows: &[Value],
    app_id: &str,
    collection: &str,
    op: crate::broker::ChangeOp,
) {
    if rows.is_empty() {
        // No rows affected — no broker event. UPDATE with a non-
        // matching filter falls here; subscribers should not see a
        // spurious change.
        return;
    }
    if backend_publishes_committed_changes() {
        // SQLite has a commit-time CDC publisher wired through the writer
        // actor's preupdate/commit hooks. The old SDK-local emit was kept
        // for the Postgres/no-WAL-consumer path; on SQLite it races the CDC
        // publisher and produces duplicate identical live snapshots.
        return;
    }
    if crate::wal_consumer::is_app_suppressed(app_id)
        || !crate::broker::has_subscribers(app_id, collection)
    {
        return;
    }
    for row in rows {
        let pk = row
            .get("id")
            .and_then(value_to_logical_id)
            .or_else(|| row.get("_id").and_then(value_to_logical_id));
        // changed_columns: the keys present in the returned row,
        // minus the system columns we never want to report. For
        // INSERT this is "every declared column" — for UPDATE it's
        // the post-image, which is a superset of what changed.
        // Filtering down to "what changed" requires a before/after
        // diff that we don't have here; P8b will compute it from the
        // mutation's SET clause directly.
        let (columns, tuple): (Vec<String>, std::collections::HashMap<String, String>) = match row {
            Value::Object(m) => {
                let cols = m
                    .keys()
                    .filter(|k| !matches!(k.as_str(), "created_at" | "updated_at"))
                    .cloned()
                    .collect();
                // P8b: render the full RETURNING row into a
                // `column → text` map for the broker's predicate
                // evaluation. Numbers / bools are stringified to
                // match the WAL-consumer path's text encoding so the
                // predicate-eval rules collapse to a single
                // comparison code path.
                let tuple = m
                    .iter()
                    .map(|(k, v)| {
                        let s = match v {
                            Value::String(s) => s.clone(),
                            Value::Null => "NULL".to_string(),
                            other => other.to_string(),
                        };
                        (k.clone(), s)
                    })
                    .collect();
                (cols, tuple)
            }
            _ => (Vec::new(), std::collections::HashMap::new()),
        };
        #[cfg(test)]
        tests::record_tuple_built();
        queue_or_emit(app_id, collection, op, pk, columns, tuple);
    }
}

/// If a transaction is active on this thread, queue the event in
/// the per-isolate context's `pending_emits` slot for the settle
/// path to drain on COMMIT. Otherwise (autocommit), fire it
/// immediately. Closes Gap B — subscribers no longer observe
/// pre-commit state.
fn queue_or_emit(
    app_id: &str,
    collection: &str,
    op: crate::broker::ChangeOp,
    pk: Option<String>,
    changed_columns: Vec<String>,
    new_tuple: std::collections::HashMap<String, String>,
) {
    // SEC-1: queue only while THIS app's tx is open. If a co-resident
    // app holds the only parked tx, this app is effectively in
    // autocommit and must emit immediately (its event would otherwise
    // sit unfired — there is no settle path for it).
    let in_tx = context::with(|c| c.has_tx_for(app_id));
    if !in_tx {
        crate::wal_consumer::emit_local(app_id, collection, op, pk, changed_columns, new_tuple);
        return;
    }
    let ev = crate::broker::ChangeEvent {
        app_id: app_id.to_string(),
        collection: collection.to_string(),
        op,
        pk,
        changed_columns,
        new_tuple,
        old_tuple: None,
    };
    context::with_mut(|c| c.push_pending_emit(ev));
}

fn value_to_logical_id(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Drain `app_id`'s `pending_emits` queue and fire every queued event
/// through the broker. Called by the transaction settle path on COMMIT.
/// SEC-1: scoped to the committing app so one app's COMMIT can never
/// fire a co-resident app's pre-commit events.
pub(crate) fn drain_pending_emits_on_commit(app_id: &str) {
    let queued: Vec<crate::broker::ChangeEvent> =
        context::with_mut(|c| c.drain_pending_emits_for(app_id));
    for ev in queued {
        crate::wal_consumer::emit_local(
            &ev.app_id,
            &ev.collection,
            ev.op,
            ev.pk,
            ev.changed_columns,
            ev.new_tuple,
        );
    }
}

/// Clear `app_id`'s `pending_emits` queue without firing any events.
/// Called by the transaction settle path on ROLLBACK (and by
/// `exec_begin` to drop any stale residue from an interrupted prior
/// run). SEC-1: scoped to the app so a ROLLBACK never drops a
/// co-resident app's queued events.
pub(crate) fn clear_pending_emits(app_id: &str) {
    context::with_mut(|c| c.clear_pending_emits_for(app_id));
}

/// Lazy pool accessor shared by every async helper that needs the
/// pooled connection. First call kicks off `init_pool_async` (Postgres
/// connect + warm-up); subsequent calls clone the `Rc<Pool>` out of
/// the per-thread cell.
pub(crate) async fn ensure_pool() -> Result<Rc<compio_postgres::Pool>, DbError> {
    ensure_postgres_pool_for_shared_sql().await
}

/// **Test-only**: end-to-end wrapper around [`exec_mutation_with_emit`]
/// so integration tests can drive the queue/drain machinery against a
/// real Postgres connection without spinning up a V8 isolate.
///
/// The caller is responsible for setting
/// [`crate::context::IsolateDbContext::tx_conn`] (via
/// [`crate::install_tx_marker_for_tests`]) when the test wants the
/// queueing path to fire.
#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn exec_mutation_with_emit_for_tests(
    bq: crate::query::BuiltQuery,
    app_id: &str,
    collection: &str,
    op: crate::broker::ChangeOp,
) -> Result<Vec<Value>, String> {
    exec_mutation_with_emit(bq, app_id, collection, op)
        .await
        .map_err(DbError::into_string)
}

/// **Test-only**: exec a read query through the same shared
/// pool-or-tx path production CRUD uses, including the Postgres
/// autocommit per-app role fence.
#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn exec_query_for_tests(
    app_id: &str,
    bq: crate::query::BuiltQuery,
) -> Result<Vec<Value>, String> {
    exec_query(app_id, bq).await.map_err(DbError::into_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NamespaceManager as _;
    use crate::backend::sqlite::SqliteBackend;
    use crate::backend::SqlExecutor as _;
    use crate::broker::ChangeOp;
    use std::path::PathBuf;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;

    thread_local! {
        /// Counter incremented every time the production code path
        /// finishes building one `(columns, tuple)` pair inside
        /// [`emit_for_rows`]. Wired in via the `#[cfg(test)]`
        /// `tests::record_tuple_built()` call at the bottom of the
        /// per-row loop.
        static TUPLE_BUILT_COUNT: Cell<usize> = const { Cell::new(0) };
        static SQLITE_ROUTE: Cell<u8> = const { Cell::new(0) };
    }

    /// Hook invoked from [`emit_for_rows`] when the per-row build
    /// actually runs (i.e. both gates passed). Crate-private; only the
    /// test build references it.
    pub(super) fn record_tuple_built() {
        TUPLE_BUILT_COUNT.with(|c| c.set(c.get() + 1));
    }

    fn tuple_built_count() -> usize {
        TUPLE_BUILT_COUNT.with(|c| c.get())
    }

    fn reset_counter() {
        TUPLE_BUILT_COUNT.with(|c| c.set(0));
    }

    pub(super) fn record_sqlite_shared_route() {
        SQLITE_ROUTE.with(|c| c.set(1));
    }

    pub(super) fn record_sqlite_tx_route() {
        SQLITE_ROUTE.with(|c| c.set(2));
    }

    fn reset_sqlite_route() {
        SQLITE_ROUTE.with(|c| c.set(0));
    }

    fn sqlite_route() -> u8 {
        SQLITE_ROUTE.with(|c| c.get())
    }

    /// Reset every piece of thread-local state the gate inspects:
    ///   - broker subscriptions (`drop_app(None)`),
    ///   - WAL suppression set (clear the legacy sentinel and any test
    ///     keys we know we register below),
    ///   - the per-test build counter.
    fn reset_world() {
        crate::broker::drop_app(None);
        context::with_mut(|c| c.clear_pool());
        // Clear every suppression key any test in this module sets so
        // ordering between tests on the same thread doesn't leak.
        for key in ["app_suppressed", "app_no_subs", "app_active"] {
            crate::wal_consumer::unsuppress_app(key);
        }
        reset_counter();
        reset_sqlite_route();
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    /// One synthetic RETURNING row with the columns a real mutation
    /// would surface; the gate doesn't care about the content, only
    /// that the row is a `Value::Object`.
    fn synthetic_row() -> Value {
        Value::Object(
            [
                ("id".to_string(), Value::from(7i64)),
                ("title".to_string(), Value::from("hi")),
            ]
            .into_iter()
            .collect::<serde_json::Map<_, _>>(),
        )
    }

    fn synthetic_typed_id_row() -> Value {
        Value::Object(
            [
                ("id".to_string(), Value::from("usr_02HXTESTSUBSCRIPTIONID")),
                ("title".to_string(), Value::from("hi")),
            ]
            .into_iter()
            .collect::<serde_json::Map<_, _>>(),
        )
    }

    #[test]
    fn exec_mutation_with_emit_skips_build_when_app_suppressed() {
        reset_world();
        // Register a subscriber so the only thing keeping us out of
        // the build is the suppression flag.
        let sub = crate::broker::subscribe("app_suppressed", "messages");
        crate::wal_consumer::suppress_app("app_suppressed");

        let rows = vec![synthetic_row()];
        emit_for_rows(&rows, "app_suppressed", "messages", ChangeOp::Insert);

        assert_eq!(
            tuple_built_count(),
            0,
            "suppressed app must skip the (columns, tuple) build entirely",
        );
        assert!(
            sub.pop().is_none(),
            "no broker event must be queued when the WAL consumer is active",
        );
        reset_world();
    }

    #[test]
    fn exec_mutation_with_emit_skips_build_when_no_subscribers() {
        reset_world();
        // No subscribers, no suppression — the build should still
        // short-circuit because the broker would discard the event.
        let rows = vec![synthetic_row()];
        emit_for_rows(&rows, "app_no_subs", "ghosts", ChangeOp::Insert);

        assert_eq!(
            tuple_built_count(),
            0,
            "no subscribers must skip the (columns, tuple) build",
        );
        // `pending_emits` only fills when in_tx; we're not in a tx
        // here, so emit_local was the only candidate consumer and it
        // would have published to the empty broker bucket. Verify the
        // broker really has no bucket for this key.
        assert!(
            !crate::broker::has_subscribers("app_no_subs", "ghosts"),
            "sanity: precondition for the gate",
        );
        reset_world();
    }

    // -------------------------------------------------------------------
    // I13 — queue_or_emit / drain_pending_emits_on_commit / clear_pending_emits
    // -------------------------------------------------------------------
    //
    // The in-tx branch of `queue_or_emit` is gated on
    // `context::with(|c| c.has_tx())`, which is `self.tx_conn.is_some()`.
    // The slot can only hold a real `compio_postgres::Client`, which the
    // context-module test docs explicitly note is not constructible
    // outside `compio-postgres`. So these tests cover three of the four
    // branches:
    //
    //  (a) queue_or_emit in autocommit (no tx) → immediate emit_local
    //  (b) drain_pending_emits_on_commit fires every queued event
    //  (c) clear_pending_emits drops the queue without firing
    //
    // The in-tx queue branch is exercised by the integration suite
    // (gap_b_subscriber_does_not_observe_pre_commit_state in
    // tests/integration.rs).

    /// In autocommit mode (`has_tx() == false`), `queue_or_emit` must
    /// route the event directly to `wal_consumer::emit_local`, which
    /// publishes to the broker. The subscriber's queue receives the
    /// event without any explicit drain.
    #[test]
    fn queue_or_emit_no_tx_emits_immediately() {
        reset_world();
        // Defensive: make sure no tx is parked for this app from an
        // earlier test on the same OS thread.
        context::with(|c| assert!(!c.has_tx_for("app_active"), "precondition: no tx"));

        let sub = crate::broker::subscribe("app_active", "messages");

        let mut tuple = HashMap::new();
        tuple.insert("id".to_string(), "9".to_string());
        queue_or_emit(
            "app_active",
            "messages",
            ChangeOp::Insert,
            Some("9".to_string()),
            vec!["id".to_string()],
            tuple,
        );

        match sub.pop() {
            Some(crate::broker::SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.pk.as_deref(), Some("9"));
                assert_eq!(ev.op, ChangeOp::Insert);
            }
            other => panic!("expected immediate Change event, got {other:?}"),
        }
        reset_world();
    }

    /// `drain_pending_emits_on_commit` must publish every event sitting
    /// on the per-isolate `pending_emits` queue. We seed the queue
    /// directly via the context accessor (bypassing the `has_tx` gate
    /// the production path uses) since the slot is the unit under test
    /// here — the gate's role is verified by the integration suite.
    #[test]
    fn drain_pending_emits_on_commit_fires_every_queued_event() {
        reset_world();
        let sub = crate::broker::subscribe("app_active", "messages");

        let mk_event = |pk: i64| crate::broker::ChangeEvent {
            app_id: "app_active".to_string(),
            collection: "messages".to_string(),
            op: ChangeOp::Insert,
            pk: Some(pk.to_string()),
            changed_columns: vec!["id".to_string()],
            new_tuple: {
                let mut m = HashMap::new();
                m.insert("id".to_string(), pk.to_string());
                m
            },
            old_tuple: None,
        };
        context::with_mut(|c| {
            c.push_pending_emit(mk_event(1));
            c.push_pending_emit(mk_event(2));
            c.push_pending_emit(mk_event(3));
        });
        // Sanity: nothing has been delivered before drain.
        assert!(sub.pop().is_none(), "drain must not have happened yet");

        drain_pending_emits_on_commit("app_active");

        let mut pks = Vec::new();
        while let Some(msg) = sub.pop() {
            if let crate::broker::SubscriptionMessage::Change(ev) = msg {
                pks.push(ev.pk.as_deref().unwrap().to_string());
            }
        }
        pks.sort();
        assert_eq!(pks, vec!["1", "2", "3"], "drain must publish every queued event");

        // Drain a second time → nothing left (queue is consumed, not
        // copied).
        drain_pending_emits_on_commit("app_active");
        assert!(sub.pop().is_none(), "second drain must be a no-op");
        reset_world();
    }

    /// `clear_pending_emits` must drop the queue WITHOUT publishing
    /// anything — the ROLLBACK path relies on this so subscribers
    /// never observe aborted mutations.
    #[test]
    fn clear_pending_emits_drops_without_firing() {
        reset_world();
        let sub = crate::broker::subscribe("app_active", "messages");

        let ev = crate::broker::ChangeEvent {
            app_id: "app_active".to_string(),
            collection: "messages".to_string(),
            op: ChangeOp::Insert,
            pk: Some("99".to_string()),
            changed_columns: vec!["id".to_string()],
            new_tuple: HashMap::new(),
            old_tuple: None,
        };
        context::with_mut(|c| c.push_pending_emit(ev));

        clear_pending_emits("app_active");

        assert!(
            sub.pop().is_none(),
            "ROLLBACK path must drop queued events silently",
        );
        // After clear, drain must also be a no-op (queue is empty).
        drain_pending_emits_on_commit("app_active");
        assert!(sub.pop().is_none(), "post-clear drain must publish nothing");
        reset_world();
    }

    #[test]
    fn exec_mutation_with_emit_builds_when_active_subscriber() {
        reset_world();
        let sub = crate::broker::subscribe("app_active", "messages");

        let rows = vec![synthetic_row()];
        emit_for_rows(&rows, "app_active", "messages", ChangeOp::Insert);

        assert_eq!(
            tuple_built_count(),
            1,
            "with a live subscriber and no WAL suppression we MUST build the tuple",
        );
        // And the event must actually have been delivered: prove the
        // subscriber's queue holds the change.
        match sub.pop() {
            Some(crate::broker::SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.app_id, "app_active");
                assert_eq!(ev.collection, "messages");
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.pk.as_deref(), Some("7"));
                // changed_columns excludes `created_at`/`updated_at`
                // (none here) and surfaces every other RETURNING
                // column; order isn't part of the contract.
                let mut cols = ev.changed_columns.clone();
                cols.sort();
                assert_eq!(cols, vec!["id".to_string(), "title".to_string()]);
                // The tuple map renders Numbers as their JSON string
                // form per the comment in `emit_for_rows`.
                let mut expected = HashMap::new();
                expected.insert("id".to_string(), "7".to_string());
                expected.insert("title".to_string(), "hi".to_string());
                assert_eq!(ev.new_tuple, expected);
            }
            other => panic!("expected Change variant, got {other:?}"),
        }
        reset_world();
    }

    #[test]
    fn exec_mutation_with_emit_skips_local_emit_when_sqlite_cdc_publishes() {
        reset_world();
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                SqliteBackend::new(PathBuf::from(dir.path())).expect("open sqlite backend"),
            );
            context::with_mut(|c| c.set_sqlite_backend(Rc::clone(&backend)));
            let sub = crate::broker::subscribe("app_active", "messages");

            let rows = vec![synthetic_row()];
            emit_for_rows(&rows, "app_active", "messages", ChangeOp::Insert);

            assert_eq!(
                tuple_built_count(),
                0,
                "SQLite CDC owns committed-change publication; SDK-local emit must not build"
            );
            assert!(
                sub.pop().is_none(),
                "SQLite SDK-local emit must not publish a duplicate broker event"
            );
        });
        reset_world();
    }

    #[test]
    fn exec_mutation_with_emit_uses_logical_typed_id_for_pk() {
        reset_world();
        let sub = crate::broker::subscribe("app_active", "messages");

        let rows = vec![synthetic_typed_id_row()];
        emit_for_rows(&rows, "app_active", "messages", ChangeOp::Insert);

        match sub.pop() {
            Some(crate::broker::SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.pk.as_deref(), Some("usr_02HXTESTSUBSCRIPTIONID"));
                assert_eq!(
                    ev.new_tuple.get("id").map(String::as_str),
                    Some("usr_02HXTESTSUBSCRIPTIONID")
                );
            }
            other => panic!("expected Change variant, got {other:?}"),
        }
        reset_world();
    }

    #[test]
    fn sqlite_exec_helpers_use_tx_connection_when_present() {
        reset_world();
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                SqliteBackend::new(PathBuf::from(dir.path())).expect("open sqlite backend"),
            );
            backend
                .ensure_app_schema("app_exec")
                .await
                .expect("ensure app schema");
            backend
                .pool_exec(
                    r#"CREATE TABLE "app_exec"."notes" (
                           id INTEGER PRIMARY KEY,
                           title TEXT NOT NULL
                       )"#,
                    &[],
                )
                .await
                .expect("CREATE TABLE notes");

            context::with_mut(|c| {
                c.clear_pool();
                c.set_sqlite_backend(Rc::clone(&backend));
            });

            let client = backend
                .acquire_dedicated_client()
                .await
                .expect("acquire tx client");
            backend
                .client_exec(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            context::with_mut(|c| {
                let prev = c.install_tx_client("app_exec", TxConnection::Sqlite(client));
                assert!(prev.is_none(), "tx slot should start empty");
            });

            reset_sqlite_route();
            let inserted = exec_mutation("app_exec", BuiltQuery {
                sql: r#"INSERT INTO "app_exec"."notes" (id, title)
                        VALUES (1, 'tx-row') RETURNING *"#
                    .to_string(),
                params: vec![],
            })
            .await
            .expect("exec_mutation through tx");
            assert_eq!(
                sqlite_route(),
                2,
                "exec_mutation must route through TxConnection::Sqlite",
            );
            assert_eq!(
                inserted[0].get("title").and_then(Value::as_str),
                Some("tx-row"),
            );

            reset_sqlite_route();
            let count = exec_count("app_exec", BuiltQuery {
                sql: r#"SELECT COUNT(*) AS count FROM "app_exec"."notes""#.to_string(),
                params: vec![],
            })
            .await
            .expect("exec_count through tx");
            assert_eq!(
                sqlite_route(),
                2,
                "exec_count must route through TxConnection::Sqlite",
            );
            assert_eq!(count, 1);

            reset_sqlite_route();
            let rows = exec_query("app_exec", BuiltQuery {
                sql: r#"SELECT title FROM "app_exec"."notes" WHERE id = 1"#.to_string(),
                params: vec![],
            })
            .await
            .expect("exec_query through tx");
            assert_eq!(
                sqlite_route(),
                2,
                "exec_query must route through TxConnection::Sqlite",
            );
            assert_eq!(rows[0].get("title").and_then(Value::as_str), Some("tx-row"));

            if let Some(TxConnection::Sqlite(client)) =
                context::with_mut(|c| c.take_tx_client_for("app_exec"))
            {
                let _ = client.exec("ROLLBACK", &[]).await;
            } else {
                panic!("sqlite tx client should still be parked for cleanup");
            }
            context::with_mut(|c| c.clear_pool());
        });
        reset_world();
    }

    // -------------------------------------------------------------------
    // SEC-1 — cross-tenant transaction hijack via the thread-shared slot
    // -------------------------------------------------------------------
    //
    // The worker multiplexes ~200 isolates (apps) per OS thread. When
    // app A's `env.db.transaction(async () => await fetch(slow))` parks
    // its tx client across the await, a co-resident app B's plain
    // `env.db.*` call lands on the same thread-local context. `run_sql`
    // / `exec_sqlite_json` must route B onto B's OWN autocommit path —
    // never onto A's pinned transaction connection (A's snapshot, A's
    // open tx, and — on Postgres — A's per-app role).

    #[test]
    fn sec1_app_b_query_must_not_route_through_app_a_parked_tx() {
        reset_world();
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                SqliteBackend::new(PathBuf::from(dir.path())).expect("open sqlite backend"),
            );
            context::with_mut(|c| {
                c.clear_pool();
                c.set_sqlite_backend(Rc::clone(&backend));
            });

            // app_a opens an explicit transaction; its dedicated client
            // is parked in the per-isolate slot — exactly the state a
            // creator callback leaves behind across an `await`.
            let client = backend
                .acquire_dedicated_client()
                .await
                .expect("acquire tx client");
            backend
                .client_exec(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            context::with_mut(|c| {
                let prev = c.install_tx_client("app_a", TxConnection::Sqlite(client));
                assert!(prev.is_none(), "tx slot must start empty");
            });

            // Co-resident app_b now runs a plain (non-transactional)
            // query on the same thread.
            reset_sqlite_route();
            let rows = exec_query("app_b", BuiltQuery {
                sql: "SELECT 'b' AS title".to_string(),
                params: vec![],
            })
            .await
            .expect("app_b query");
            assert_eq!(rows[0].get("title").and_then(Value::as_str), Some("b"));
            assert_eq!(
                sqlite_route(),
                1,
                "SEC-1: app_b's query must take its own shared/autocommit \
                 route (1) — not app_a's parked tx connection (2)",
            );

            // app_a's parked transaction must still be present and
            // untouched after app_b's access.
            assert!(
                context::with(|c| c.has_tx_for("app_a")),
                "app_a's parked tx must survive app_b's access",
            );

            // Cleanup: roll app_a's tx back and drop the client.
            if let Some(TxConnection::Sqlite(client)) =
                context::with_mut(|c| c.take_tx_client_for("app_a"))
            {
                let _ = client.exec("ROLLBACK", &[]).await;
            } else {
                panic!("app_a's tx client should still be parked for cleanup");
            }
            context::with_mut(|c| c.clear_pool());
        });
        reset_world();
    }

    #[test]
    fn dropping_in_flight_sqlite_query_restores_tx_slot() {
        reset_world();
        run(async {
            use crate::backend::sqlite::session::arm_next_command_gate_for_tests;
            use std::time::Duration;

            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                SqliteBackend::new(PathBuf::from(dir.path())).expect("open sqlite backend"),
            );
            backend
                .ensure_app_schema("app_exec_cancel")
                .await
                .expect("ensure app schema");
            backend
                .pool_exec(
                    r#"CREATE TABLE "app_exec_cancel"."notes" (
                           id INTEGER PRIMARY KEY,
                           title TEXT NOT NULL
                       )"#,
                    &[],
                )
                .await
                .expect("CREATE TABLE notes");
            backend
                .pool_exec(
                    r#"INSERT INTO "app_exec_cancel"."notes" (id, title)
                        VALUES (1, 'persisted')"#,
                    &[],
                )
                .await
                .expect("seed row");

            context::with_mut(|c| {
                c.clear_pool();
                c.set_sqlite_backend(Rc::clone(&backend));
            });

            let client = backend
                .acquire_dedicated_client()
                .await
                .expect("acquire tx client");
            backend
                .client_exec(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            context::with_mut(|c| {
                let prev = c.install_tx_client("app_exec_cancel", TxConnection::Sqlite(client));
                assert!(prev.is_none(), "tx slot should start empty");
            });

            let gate = arm_next_command_gate_for_tests();
            let task = compio::runtime::spawn(async {
                exec_query("app_exec_cancel", BuiltQuery {
                    sql: r#"SELECT title FROM "app_exec_cancel"."notes" WHERE id = 1"#.to_string(),
                    params: vec![],
                })
                .await
            });
            gate.wait_until_blocked()
                .await
                .expect("worker must block on test gate");
            drop(task);
            gate.release();
            compio::time::sleep(Duration::from_millis(20)).await;

            assert!(
                context::with(|c| c.has_tx_for("app_exec_cancel")),
                "dropping the in-flight future must restore the tx slot"
            );

            let rows = exec_query("app_exec_cancel", BuiltQuery {
                sql: r#"SELECT title FROM "app_exec_cancel"."notes" WHERE id = 1"#.to_string(),
                params: vec![],
            })
            .await
            .expect("subsequent query must reuse restored tx slot");
            assert_eq!(rows[0].get("title").and_then(Value::as_str), Some("persisted"));

            if let Some(TxConnection::Sqlite(client)) =
                context::with_mut(|c| c.take_tx_client_for("app_exec_cancel"))
            {
                let _ = client.exec("ROLLBACK", &[]).await;
            } else {
                panic!("sqlite tx client should still be parked for cleanup");
            }
            context::with_mut(|c| c.clear_pool());
        });
        reset_world();
    }

    // -------------------------------------------------------------------
    // P2-C1 — non-tx (autocommit) path must NOT leak role/timeout on a
    // cancelled query.
    // -------------------------------------------------------------------
    //
    // PG-REQUIRED. Gated like the crate's other live-Postgres tests:
    // connects to `PG_TEST_URL` (default `postgres://postgres:test@
    // localhost:5434/postgres`) and SKIPS (prints + returns) when no
    // Postgres is reachable. No zeroship test PG is configured in the
    // fix-authoring environment, so this is verified by compile + code
    // reasoning here, not observed red/green.
    //
    // Real path: drives `query_postgres_pool_with_autocommit_role` (the
    // single funnel every autocommit CRUD op flows through) against a
    // pool of size 1 so the SAME backend is reused on the next checkout.
    // We force the leak window by running `pg_sleep` and cancelling the
    // future mid-statement (after `SET LOCAL ROLE` + timeouts are
    // applied, before any reset/commit). Pre-fix (session-level `SET
    // ROLE` + a separate `RESET` that the cancellation skips), the next
    // checkout inherited the app role + `statement_timeout`. Post-fix
    // (SET LOCAL inside an explicit transaction), the rollback-on-drop
    // reverts both, so the next checkout sees the clean login role and
    // default timeout.

    fn pg_test_url() -> String {
        std::env::var("PG_TEST_URL")
            .unwrap_or_else(|_| "postgres://postgres:test@localhost:5434/postgres".to_string())
    }

    #[test]
    fn autocommit_cancelled_query_does_not_leak_role_or_timeout_to_pool() {
        use compio_postgres::{NoTls, Pool};
        use std::time::Duration;

        reset_world();
        run(async {
            let url = pg_test_url();
            // Skip when no Postgres is reachable — same convention as
            // tests/integration.rs `require_pg`.
            match compio_postgres::connect(&url, NoTls).await {
                Ok((client, connection)) => {
                    compio::runtime::spawn(async move {
                        let _ = connection.run().await;
                    })
                    .detach();
                    drop(client);
                }
                Err(e) => {
                    eprintln!("Skipping autocommit-leak test — Postgres not reachable: {e}");
                    return;
                }
            }

            // Pool of ONE so the cancelled-then-reused checkout lands on
            // the same backend whose session state we want to inspect.
            let pool = Rc::new(Pool::connect(&url, 1).await.expect("pool connect"));

            let app_id = "p2c1leak";
            let role = crate::auth::bootstrap::per_app_role_name(app_id);
            let role_ident = crate::query::quote_ident(&role);

            // Discover the login role so we can (a) GRANT it membership
            // in the app role (required for SET LOCAL ROLE) and (b)
            // assert the connection returns to it after cancellation.
            let login_user = {
                let c = pool.get().await.expect("checkout for setup");
                let rows = c
                    .query_text_params("SELECT current_user AS u", &[])
                    .await
                    .expect("current_user");
                rows[0].get::<_, &str>("u").to_string()
            };

            // Provision the per-app role directly (NOLOGIN) and grant the
            // login user membership so `SET LOCAL ROLE` succeeds.
            {
                let c = pool.get().await.expect("checkout for role setup");
                let _ = c
                    .simple_query(&format!("DROP ROLE IF EXISTS {role_ident}"))
                    .await;
                c.simple_query(&format!("CREATE ROLE {role_ident} NOLOGIN"))
                    .await
                    .expect("create app role");
                c.simple_query(&format!(
                    "GRANT {role_ident} TO {}",
                    crate::query::quote_ident(&login_user)
                ))
                .await
                .expect("grant membership");
            }

            // Force the leak window: run a server-side sleep through the
            // funnel and cancel the future before it can reset/commit.
            // `pg_sleep(1)` + a 100ms timeout drops the future while the
            // SET LOCAL role + timeouts are live on the backend.
            let cancelled = compio::time::timeout(
                Duration::from_millis(100),
                query_postgres_pool_with_autocommit_role(
                    &pool,
                    app_id,
                    "SELECT pg_sleep(1)",
                    &[],
                ),
            )
            .await;
            assert!(
                cancelled.is_err(),
                "the pg_sleep query must be cancelled by the timeout to exercise the leak window",
            );

            // Re-check out (size-1 pool → same backend). The pool's dirty
            // barrier drains the rolled-back tx; assert NO residual state.
            let c = pool.get().await.expect("re-checkout after cancel");
            let user_after = {
                let rows = c
                    .query_text_params("SELECT current_user AS u", &[])
                    .await
                    .expect("current_user after");
                rows[0].get::<_, &str>("u").to_string()
            };
            let timeout_after = {
                let rows = c
                    .query_text_params("SHOW statement_timeout", &[])
                    .await
                    .expect("show statement_timeout");
                rows[0].get::<_, &str>("statement_timeout").to_string()
            };

            assert_eq!(
                user_after, login_user,
                "cancelled autocommit query must NOT leave the per-app role on the pooled \
                 connection (saw {user_after}, want login role {login_user})",
            );
            assert_eq!(
                timeout_after, "0",
                "cancelled autocommit query must NOT leave a residual statement_timeout \
                 on the pooled connection (saw {timeout_after}, want default 0)",
            );

            // Cleanup: revoke + drop the throwaway role.
            let _ = c
                .simple_query(&format!(
                    "REVOKE {role_ident} FROM {}",
                    crate::query::quote_ident(&login_user)
                ))
                .await;
            let _ = c
                .simple_query(&format!("DROP ROLE IF EXISTS {role_ident}"))
                .await;
        });
        reset_world();
    }
}
