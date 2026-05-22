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
//! - `exec_mutation_with_emit` — write path + broker emit (or queue
//!   when inside a transaction).
//!
//! All four route through `run_sql`, which transparently uses the
//! per-isolate TX client (`IsolateDbContext::tx_conn`) when an explicit
//! `Transaction` / auto-tx is active and the pool otherwise.
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

use crate::context;
use crate::error::DbError;
use crate::query::BuiltQuery;
use crate::v8_bridge::rows_to_json_value;

/// Execute SQL with text params — uses TX connection if active, otherwise pool.
pub(crate) async fn run_sql(
    sql: &str,
    params: &[&str],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    // Check if there's an active transaction
    let has_tx = context::with(|c| c.has_tx());
    if has_tx {
        // Use transaction connection
        let client = context::with_mut(|c| c.take_tx_client())
            .ok_or_else(|| DbError::internal("db: transaction connection lost"))?;
        let result = client.query_text_params(sql, params).await;
        // Put it back
        context::with_mut(|c| c.put_tx_client(client));
        return result.map_err(|e| DbError::from_pg(&e));
    }

    // No transaction — use pool
    let has_pool = context::with(|c| c.pool_initialised());
    if !has_pool {
        crate::init_pool_async()
            .await
            .map_err(|e| DbError::config("lazy_init_failed", format!("db: lazy init failed: {e}")))?;
    }
    let pool = context::with(|c| c.pool());
    let pool = pool.ok_or_else(|| {
        DbError::config("not_configured", "db: pool not initialized".to_string())
    })?;
    pool.query_text_params(sql, params)
        .await
        .map_err(|e| DbError::from_pg(&e))
}

/// Execute a built query via pool (or TX conn) and return the
/// per-row JSON values.
///
/// Returning `Vec<Value>` (rather than a pre-serialised JSON array
/// string) lets the CRUD resolver chain in `crate::crud` inspect or
/// take a single row without paying for an intermediate serialise +
/// reparse round-trip. The final JSON string is materialised once at
/// the V8 boundary (`ResolveValue::Json`).
pub(crate) async fn exec_query(bq: BuiltQuery) -> Result<Vec<Value>, DbError> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = run_sql(&bq.sql, &param_refs).await?;
    Ok(rows_to_json_value(&rows))
}

/// Execute a built query expecting a count result.
///
/// Returns the raw integer; callers wrap into the appropriate
/// `OpResult` shape (typically `ResolveValue::F64` so JS sees a real
/// `number`).
pub(crate) async fn exec_count(bq: BuiltQuery) -> Result<i64, DbError> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = run_sql(&bq.sql, &param_refs).await?;

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
pub(crate) async fn exec_mutation(bq: BuiltQuery) -> Result<Vec<Value>, DbError> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = run_sql(&bq.sql, &param_refs).await?;
    Ok(rows_to_json_value(&rows))
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
/// `changed_columns` from the SET clause and `pk` from the RETURNING
/// row. For P8a we collect what's already in the result `Value`.
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
    let rows = exec_mutation(bq).await?;
    emit_for_rows(&rows, app_id, collection, op);
    Ok(rows)
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
    if crate::wal_consumer::is_app_suppressed(app_id)
        || !crate::broker::has_subscribers(app_id, collection)
    {
        return;
    }
    for row in rows {
        let pk = row
            .get("id")
            .and_then(|v| v.as_i64())
            .or_else(|| row.get("_id").and_then(|v| v.as_i64()));
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
    pk: Option<i64>,
    changed_columns: Vec<String>,
    new_tuple: std::collections::HashMap<String, String>,
) {
    let in_tx = context::with(|c| c.has_tx());
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

/// Drain the per-isolate `pending_emits` queue and fire every queued
/// event through the broker. Called by the transaction settle path
/// on COMMIT.
pub(crate) fn drain_pending_emits_on_commit() {
    let queued: Vec<crate::broker::ChangeEvent> =
        context::with_mut(|c| c.drain_pending_emits());
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

/// Clear the per-isolate `pending_emits` queue without firing any
/// events. Called by the transaction settle path on ROLLBACK (and by
/// `exec_begin` to drop any stale residue from an interrupted prior
/// run).
pub(crate) fn clear_pending_emits() {
    context::with_mut(|c| c.clear_pending_emits());
}

/// Lazy pool accessor shared by every async helper that needs the
/// pooled connection. First call kicks off `init_pool_async` (Postgres
/// connect + warm-up); subsequent calls clone the `Rc<Pool>` out of
/// the per-thread cell.
pub(crate) async fn ensure_pool() -> Result<Rc<compio_postgres::Pool>, DbError> {
    let has_pool = context::with(|c| c.pool_initialised());
    if !has_pool {
        crate::init_pool_async()
            .await
            .map_err(|e| DbError::config("lazy_init_failed", format!("db: lazy init failed: {e}")))?;
    }
    context::with(|c| c.pool())
        .ok_or_else(|| {
            DbError::config("not_configured", "db: pool not initialized".to_string())
        })
}

/// **Test-only**: end-to-end wrapper around [`exec_mutation_with_emit`]
/// so integration tests can drive the queue/drain machinery against a
/// real Postgres connection without spinning up a V8 isolate.
///
/// The caller is responsible for setting
/// [`crate::context::IsolateDbContext::tx_conn`] (via
/// [`crate::install_tx_marker_for_tests`]) when the test wants the
/// queueing path to fire.
#[cfg(any(test, feature = "test-helpers"))]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::ChangeOp;
    use std::cell::Cell;
    use std::collections::HashMap;

    thread_local! {
        /// Counter incremented every time the production code path
        /// finishes building one `(columns, tuple)` pair inside
        /// [`emit_for_rows`]. Wired in via the `#[cfg(test)]`
        /// `tests::record_tuple_built()` call at the bottom of the
        /// per-row loop.
        static TUPLE_BUILT_COUNT: Cell<usize> = const { Cell::new(0) };
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

    /// Reset every piece of thread-local state the gate inspects:
    ///   - broker subscriptions (`drop_app(None)`),
    ///   - WAL suppression set (clear the legacy sentinel and any test
    ///     keys we know we register below),
    ///   - the per-test build counter.
    fn reset_world() {
        crate::broker::drop_app(None);
        // Clear every suppression key any test in this module sets so
        // ordering between tests on the same thread doesn't leak.
        for key in ["app_suppressed", "app_no_subs", "app_active"] {
            crate::wal_consumer::unsuppress_app(key);
        }
        reset_counter();
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
                assert_eq!(ev.pk, Some(7));
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
}
