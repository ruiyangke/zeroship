//! Native SQL execution on the dispatch's captured transaction route.
//! Shared CRUD uses the registered driver for autocommit and the owned session
//! for transaction work. Successful operations emit usage and queued effects.

use crate::value::Value;

use crate::backend::BackendHandle;
use crate::sql::compiler::CompiledQuery;

use crate::tx_route::TxRoute;
use zeroship_data_orm::error::DbError;

use crate::metrics::{emit_db_metric, DB_READS, DB_ROWS_WRITTEN, DB_WRITES};

/// The op was dispatched inside a `db.transaction(fn)` callback whose
/// transaction has since settled — a continuation that outlived its
/// transaction (typically a promise the callback started and never
/// awaited).
///
/// It must be refused instead of falling back to autocommit.
fn tx_scope_expired() -> DbError {
    crate::transaction::scope::expired()
}

/// The route says "in transaction" and the transaction is still open, but
/// its connection is checked out by another in-flight operation for the same
/// app. A transaction serializes operations on its owned connection.
fn tx_connection_busy() -> DbError {
    DbError::validation_hinted(
        "transaction_connection_busy",
        "db: another operation is already using this transaction's connection".to_string(),
        "A transaction has one connection, so its operations cannot overlap. Await each env.db \
         call inside the db.transaction(...) callback before starting the next — a Promise.all \
         over several tx operations runs them concurrently on that one connection.",
    )
}

/// Which of the two empty-slot causes applies for this route.
fn tx_slot_unavailable(route: &crate::binding::DbRoute) -> DbError {
    if crate::tx_lanes::with(|l| l.tx_claimed_by(route)) {
        tx_connection_busy()
    } else {
        tx_scope_expired()
    }
}

/// Take this dispatch's parked transaction client, or refuse the way every
/// other routed statement refuses.
///
/// An empty slot is classified as either an expired scope or concurrent use of
/// the transaction connection.
pub(crate) fn take_tx_lane(route: &TxRoute) -> Result<crate::tx_lanes::TxClientSlotGuard, DbError> {
    route.check_scope()?;
    crate::tx_lanes::TxClientSlotGuard::take(&route.key())
        .map_err(|_| tx_slot_unavailable(&route.key()))
}

/// Read catalog evidence on the captured binding and transaction lease.
pub(crate) async fn read_catalog(
    route: &TxRoute,
) -> Result<crate::sql::catalog::LiveSchema, DbError> {
    if route.in_tx() {
        route.check_scope()?;
        return crate::transaction::driver::execute_operation(&route.key(), async {
            let lane = take_tx_lane(route)?;
            route.backend().validate_session(lane.client())?;
            route
                .backend()
                .introspect_schema(route.binding(), Some(lane.client()))
                .await
        })
        .await;
    }
    route
        .backend()
        .introspect_schema(route.binding(), None)
        .await
}

/// Execute SQL with text params — uses the app's TX connection when this
/// dispatch was issued inside that transaction, otherwise the pool.
pub async fn run_sql(route: &TxRoute, sql: &str, params: &[Value]) -> Result<Vec<Value>, DbError> {
    if route.in_tx() {
        route.check_scope()?;
        return crate::transaction::driver::execute_operation(&route.key(), async {
            let lane = take_tx_lane(route)?;
            route.backend().validate_session(lane.client())?;
            #[cfg(test)]
            tests::record_sqlite_tx_route();
            lane.client().query(sql, params).await
        })
        .await;
    }
    #[cfg(test)]
    tests::record_sqlite_shared_route();
    route
        .backend()
        .query(route.binding(), sql, params)
        .await
}

/// Execute without fetching rows on the captured transaction route.
pub async fn run_statement(route: &TxRoute, sql: &str, params: &[Value]) -> Result<u64, DbError> {
    if route.in_tx() {
        route.check_scope()?;
        return crate::transaction::driver::execute_operation(&route.key(), async {
            let lane = take_tx_lane(route)?;
            route.backend().validate_session(lane.client())?;
            #[cfg(test)]
            tests::record_sqlite_tx_route();
            lane.client().exec(sql, params).await
        })
        .await;
    }
    #[cfg(test)]
    tests::record_sqlite_shared_route();
    route
        .backend()
        .exec(route.binding(), sql, params)
        .await
}

/// Execute a compiled read and meter a successful operation.
pub async fn exec_query(route: &TxRoute, bq: CompiledQuery) -> Result<Vec<Value>, DbError> {
    let param_refs = &bq.params;
    let rows = run_sql(route, &bq.sql, param_refs).await?;
    // Success arm only: one read op. Unforgeable (emitted by the primitive).
    emit_db_metric(route.usage(), DB_READS, 1);
    Ok(rows)
}

/// Execute a built query expecting a count result.
///
/// Returns the raw integer; callers wrap into the appropriate
/// `OpResult` shape (typically `ResolveValue::F64` so JS sees a real
/// `number`).
///
pub async fn exec_count(route: &TxRoute, bq: CompiledQuery) -> Result<i64, DbError> {
    let param_refs = &bq.params;
    let rows = run_sql(route, &bq.sql, param_refs).await?;
    emit_db_metric(route.usage(), DB_READS, 1);
    let count = rows
        .first()
        .and_then(|row| row.get("count"))
        .and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            _ => None,
        })
        .ok_or_else(|| DbError::internal("count result did not return an integer"))?;
    Ok(count)
}

/// Execute an insert/update/delete query, returning the affected
/// rows as `Vec<crate::value::Value>` (one `Value::Object` per row).
///
/// Native records feed the protection passes and adapters. Broker events
/// encode their explicit wire contract separately.
///
pub async fn exec_mutation(route: &TxRoute, bq: CompiledQuery) -> Result<Vec<Value>, DbError> {
    let param_refs = &bq.params;
    let rows = run_sql(route, &bq.sql, param_refs).await?;
    // Success arm only: one write op + the affected/RETURNING row count.
    emit_db_metric(route.usage(), DB_WRITES, 1);
    emit_db_metric(route.usage(), DB_ROWS_WRITTEN, rows.len() as u64);
    Ok(rows)
}

/// Execute a mutation, then emit a [`crate::cdc::broker::emit_local`]
/// event into the in-process broker on success.
///
/// This is the coarse-grained reactive-query bridge: every
/// successful INSERT/UPDATE/DELETE produces one or more events on
/// `(app_id, collection)` that wake any matching subscribers in the
/// same isolate.
///
/// On error the broker is untouched. The caller supplies the operation kind;
/// returned rows supply the logical identity and changed columns.
pub async fn exec_mutation_with_emit(
    bq: CompiledQuery,
    route: &TxRoute,
    collection: &str,
    op: zeroship_data_orm::cdc::ChangeOp,
    binding: &crate::binding::DbBinding,
) -> Result<Vec<Value>, DbError> {
    crate::descriptor::collection_schema(binding, collection)?;
    let rows = exec_mutation(route, bq).await?;
    emit_mutation_rows(&rows, route, collection, op);
    Ok(rows)
}

pub(crate) fn emit_mutation_rows(
    rows: &[Value],
    route: &TxRoute,
    collection: &str,
    op: zeroship_data_orm::cdc::ChangeOp,
) {
    emit_for_rows(
        rows,
        &route.key(),
        route.in_tx(),
        backend_publishes_committed_changes(route.backend()),
        collection,
        op,
    );
}

/// Execute a count-only mutation and queue a collection invalidation on success.
pub async fn exec_mutation_count_with_emit(
    bq: CompiledQuery,
    route: &TxRoute,
    collection: &str,
    op: zeroship_data_orm::cdc::ChangeOp,
) -> Result<u64, DbError> {
    let affected = run_statement(route, &bq.sql, &bq.params).await?;
    emit_db_metric(route.usage(), DB_WRITES, 1);
    emit_db_metric(route.usage(), DB_ROWS_WRITTEN, affected);
    emit_mutation_count(route, collection, op, affected);
    Ok(affected)
}

pub(crate) fn emit_mutation_count(
    route: &TxRoute,
    collection: &str,
    op: zeroship_data_orm::cdc::ChangeOp,
    affected: u64,
) {
    let app_id = route.app_id();
    if affected != 0
        && !backend_publishes_committed_changes(route.backend())
        && !crate::cdc::broker::is_app_suppressed(app_id)
        && crate::cdc::broker::has_subscribers(app_id, collection)
    {
        queue_or_emit(
            &route.key(),
            route.in_tx(),
            collection,
            op,
            None,
            Vec::new(),
            std::collections::HashMap::new(),
        );
    }
}

/// Does the backend publish committed changes on its own?
///
/// SQLite does, through the writer actor's commit hook. PostgreSQL does not on
/// this path - the WAL consumer is a separate process concern - so the local
/// emit below is what feeds subscribers there.
///
/// The registered driver explicitly declares its committed-event source.
fn backend_publishes_committed_changes(backend: &BackendHandle) -> bool {
    backend.publishes_committed_changes()
}

/// Build and queue/emit broker events for a mutation's RETURNING rows.
///
/// Tuple construction is skipped when the backend publishes committed changes,
/// WAL delivery owns the app, or the collection has no subscribers.
fn emit_for_rows(
    rows: &[Value],
    route: &crate::binding::DbRoute,
    in_tx: bool,
    backend_publishes: bool,
    collection: &str,
    op: zeroship_data_orm::cdc::ChangeOp,
) {
    if rows.is_empty() {
        // No rows affected — no broker event. UPDATE with a non-
        // matching filter falls here; subscribers should not see a
        // spurious change.
        return;
    }
    if backend_publishes {
        // The backend's commit publisher owns delivery for this write.
        return;
    }
    if crate::cdc::broker::is_app_suppressed(route.app_id())
        || !crate::cdc::broker::has_subscribers(route.app_id(), collection)
    {
        return;
    }
    for row in rows {
        let pk = row.get("id").and_then(value_to_logical_id);
        // The returned post-image conservatively reports every projected field.
        let (columns, tuple): (Vec<String>, std::collections::HashMap<String, String>) = match row {
            Value::Object(m) => {
                let cols = m.keys().cloned().collect();
                // Render the full RETURNING row into a
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
        queue_or_emit(route, in_tx, collection, op, pk, columns, tuple);
    }
}

/// If a transaction is active on this thread, queue the event in
/// the per-isolate context's `pending_emits` slot for the settle
/// path to drain on COMMIT. Otherwise (autocommit), fire it
/// immediately. Subscribers no longer observe pre-commit state.
fn queue_or_emit(
    route: &crate::binding::DbRoute,
    in_tx: bool,
    collection: &str,
    op: zeroship_data_orm::cdc::ChangeOp,
    pk: Option<String>,
    changed_columns: Vec<String>,
    new_tuple: std::collections::HashMap<String, String>,
) {
    // Queue only when THIS write actually ran on THIS app's transaction —
    // the same route that decided which connection the SQL used, so the
    // event's fate cannot disagree with the row's. A write that merely
    // overlapped someone else's transaction is in autocommit and must emit
    // immediately; queueing it would park the event on a settle path that
    // belongs to a different unit of work.
    if !in_tx {
        crate::cdc::broker::emit_local(
            route.app_id(),
            collection,
            op,
            pk,
            changed_columns,
            new_tuple,
        );
        return;
    }
    let ev = zeroship_data_orm::cdc::ChangeEvent {
        app_id: route.app_id().to_string(),
        collection: collection.to_string(),
        op,
        pk,
        changed_columns,
        new_tuple,
        old_tuple: None,
    };
    crate::tx_lanes::with_mut(|l| l.push_pending_emit(route, ev));
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
/// Scoped to the committing app so one app's COMMIT can never
/// fire a co-resident app's pre-commit events.
pub fn drain_pending_emits_on_commit(route: &crate::binding::DbRoute) {
    let queued: Vec<zeroship_data_orm::cdc::ChangeEvent> =
        crate::tx_lanes::with_mut(|l| l.drain_pending_emits_for(route));
    for ev in queued {
        crate::cdc::broker::emit_local(
            &ev.app_id,
            &ev.collection,
            ev.op,
            ev.pk,
            ev.changed_columns,
            ev.new_tuple,
        );
    }
}

/// Clear the route's `pending_emits` queue without firing any events.
/// Called by the transaction settle path on ROLLBACK (and by
/// `exec_begin` to drop any stale residue from an interrupted prior
/// run). Scoped to the app so a ROLLBACK never drops a
/// co-resident app's queued events.
pub fn clear_pending_emits(route: &crate::binding::DbRoute) {
    crate::tx_lanes::with_mut(|l| l.clear_pending_emits_for(route));
}

#[cfg(test)]
pub fn ambient_route_for_tests(
    binding: &crate::binding::DbBinding,
    backend: crate::backend::BackendHandle,
) -> TxRoute {
    let registration = backend.sql_registration().clone();
    let captured = if crate::tx_lanes::with(|l| l.has_tx_for(&binding.route())) {
        crate::tx_route::CapturedRoute::tx_on_binding_for_tests(binding, registration)
    } else {
        crate::tx_route::CapturedRoute::pool_on_binding_for_tests(binding, registration)
    };
    // Sync, and it can be: only the COLD path needs to await, and a harness
    // driving exec directly has already opened a backend. Production binds
    // through `tx_scope::bind_route`, which owns the cold arm.
    captured
        .bind(backend)
        .expect("test route registration matches backend")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Session;
    use crate::tests::fixtures::DatabaseFixture;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::rc::Rc;
    use zeroship_data_orm::cdc::ChangeOp;

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

    /// Reset only the state owned by one test app.
    fn reset_world(app_id: &str) {
        crate::cdc::broker::drop_app(app_id);
        crate::cdc::broker::unsuppress_app(app_id);
        reset_counter();
        reset_sqlite_route();
    }

    /// One test's cleanup must leave another app's broker state intact.
    #[test]
    fn reset_world_leaves_other_apps_untouched() {
        let mine = "app_reset_scope_mine";
        let theirs = "app_reset_scope_theirs";

        reset_world(mine);

        // Stand in for a test running concurrently on another thread.
        let their_sub = crate::cdc::broker::subscribe(theirs, "messages");
        crate::cdc::broker::suppress_app(theirs);

        // Our cleanup fires while they are mid-test.
        reset_world(mine);

        assert!(
            crate::cdc::broker::is_app_suppressed(theirs),
            "reset_world cleared another app's suppression",
        );
        assert!(
            crate::cdc::broker::has_subscribers(theirs, "messages"),
            "reset_world dropped another app's broker subscription",
        );

        crate::cdc::broker::unsuppress_app(theirs);
        drop(their_sub);
        reset_world(theirs);
        reset_world(mine);
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    /// A captured compiler registration cannot be rebound to another backend.
    #[test]
    fn an_ambient_route_captures_and_verifies_the_sql_registration() {
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let sqlite = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    crate::encryption::ProjectKeySource::unavailable(),
                )
                .expect("open sqlite backend"),
            );
            let handle = BackendHandle::new(Rc::clone(&sqlite));

            let derived = ambient_route_for_tests(&crate::tests::fixtures::harness_binding("app_route_dialect"), handle.clone());
            assert_eq!(
                derived.sql_registration().family(),
                crate::sql::registration::SQLITE_FAMILY,
            );

            let mismatch = crate::tx_route::CapturedRoute::pool_for_tests(
                "app_route_dialect",
                crate::sql::registration::SqlRegistration::postgres(),
            )
            .bind(handle);
            assert!(mismatch.is_err());
        });
    }

    #[test]
    fn exec_count_rejects_malformed_driver_results() {
        run(async {
            let (backend, dir) = crate::tests::fixtures::unit_backend();
            let route = ambient_route_for_tests(&crate::tests::fixtures::harness_binding("app_count_result"), backend);
            for sql in [
                "SELECT 1 AS other",
                "SELECT 'one' AS count",
                "SELECT 1.5 AS count",
            ] {
                let error = exec_count(
                    &route,
                    CompiledQuery {
                        sql: sql.to_owned(),
                        params: Vec::new(),
                    },
                )
                .await
                .unwrap_err();
                assert!(
                    error.to_string().contains("count result"),
                    "unexpected error: {error}"
                );
            }
            drop(route);
            drop(dir);
        });
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
            .collect::<crate::value::Map<_, _>>(),
        )
    }

    fn synthetic_typed_id_row() -> Value {
        Value::Object(
            [
                ("id".to_string(), Value::from("usr_02hxtestsubscriptionid000")),
                ("title".to_string(), Value::from("hi")),
            ]
            .into_iter()
            .collect::<crate::value::Map<_, _>>(),
        )
    }

    #[test]
    fn exec_mutation_with_emit_skips_build_when_app_suppressed() {
        reset_world("app_suppressed");
        // Register a subscriber so the only thing keeping us out of
        // the build is the suppression flag.
        let sub = crate::cdc::broker::subscribe("app_suppressed", "messages");
        crate::cdc::broker::suppress_app("app_suppressed");

        let rows = vec![synthetic_row()];
        emit_for_rows(
            &rows,
            &crate::tests::fixtures::harness_route("app_suppressed"),
            /* in_tx */ false,
            /* backend_publishes */ false,
            "messages",
            ChangeOp::Insert,
        );

        assert_eq!(
            tuple_built_count(),
            0,
            "suppressed app must skip the (columns, tuple) build entirely",
        );
        assert!(
            sub.pop().is_none(),
            "no broker event must be queued when the WAL consumer is active",
        );
        reset_world("app_suppressed");
    }

    #[test]
    fn exec_mutation_with_emit_skips_build_when_no_subscribers() {
        reset_world("app_no_subs");
        // No subscribers, no suppression — the build should still
        // short-circuit because the broker would discard the event.
        let rows = vec![synthetic_row()];
        emit_for_rows(
            &rows,
            &crate::tests::fixtures::harness_route("app_no_subs"),
            /* in_tx */ false,
            /* backend_publishes */ false,
            "ghosts",
            ChangeOp::Insert,
        );

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
            !crate::cdc::broker::has_subscribers("app_no_subs", "ghosts"),
            "sanity: precondition for the gate",
        );
        reset_world("app_no_subs");
    }

    // -------------------------------------------------------------------
    // queue_or_emit / drain_pending_emits_on_commit / clear_pending_emits
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
    // crates/zeroship-data-orm/src/tests/postgres/cdc.rs).

    /// In autocommit mode (`has_tx() == false`), `queue_or_emit` must
    /// route the event directly to `broker::emit_local`, which
    /// publishes to the broker. The subscriber's queue receives the
    /// event without any explicit drain.
    #[test]
    fn queue_or_emit_no_tx_emits_immediately() {
        reset_world("app_active_queue_or_emit_no_tx_emits_immediately");
        // Defensive: make sure no tx is parked for this app from an
        // earlier test on the same OS thread.
        crate::tx_lanes::with(|l| {
            assert!(
                !l.has_tx_for(&crate::tests::fixtures::harness_route("app_active_queue_or_emit_no_tx_emits_immediately")),
                "precondition: no tx"
            )
        });

        let sub = crate::cdc::broker::subscribe(
            "app_active_queue_or_emit_no_tx_emits_immediately",
            "messages",
        );

        let mut tuple = HashMap::new();
        tuple.insert("id".to_string(), "9".to_string());
        queue_or_emit(
            &crate::tests::fixtures::harness_route("app_active_queue_or_emit_no_tx_emits_immediately"),
            false,
            "messages",
            ChangeOp::Insert,
            Some("9".to_string()),
            vec!["id".to_string()],
            tuple,
        );

        match sub.pop() {
            Some(crate::cdc::broker::SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.pk.as_deref(), Some("9"));
                assert_eq!(ev.op, ChangeOp::Insert);
            }
            other => panic!("expected immediate Change event, got {other:?}"),
        }
        reset_world("app_active_queue_or_emit_no_tx_emits_immediately");
    }

    /// `drain_pending_emits_on_commit` must publish every event sitting
    /// on the per-isolate `pending_emits` queue. We seed the queue
    /// directly via the context accessor (bypassing the `has_tx` gate
    /// the production path uses) since the slot is the unit under test
    /// here — the gate's role is verified by the integration suite.
    #[test]
    fn drain_pending_emits_on_commit_fires_every_queued_event() {
        reset_world("app_active_drain_pending_emits_on_commit_fires_every_queued_event");
        let sub = crate::cdc::broker::subscribe(
            "app_active_drain_pending_emits_on_commit_fires_every_queued_event",
            "messages",
        );

        let mk_event = |pk: i64| zeroship_data_orm::cdc::ChangeEvent {
            app_id: "app_active_drain_pending_emits_on_commit_fires_every_queued_event".to_string(),
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
        let route = crate::tests::fixtures::harness_route(
            "app_active_drain_pending_emits_on_commit_fires_every_queued_event",
        );
        crate::tx_lanes::with_mut(|l| {
            l.push_pending_emit(&route, mk_event(1));
            l.push_pending_emit(&route, mk_event(2));
            l.push_pending_emit(&route, mk_event(3));
        });
        // Sanity: nothing has been delivered before drain.
        assert!(sub.pop().is_none(), "drain must not have happened yet");

        drain_pending_emits_on_commit(&crate::tests::fixtures::harness_route("app_active_drain_pending_emits_on_commit_fires_every_queued_event"));

        let mut pks = Vec::new();
        while let Some(msg) = sub.pop() {
            if let crate::cdc::broker::SubscriptionMessage::Change(ev) = msg {
                pks.push(ev.pk.as_deref().unwrap().to_string());
            }
        }
        pks.sort();
        assert_eq!(
            pks,
            vec!["1", "2", "3"],
            "drain must publish every queued event"
        );

        // Drain a second time → nothing left (queue is consumed, not
        // copied).
        drain_pending_emits_on_commit(&crate::tests::fixtures::harness_route("app_active_drain_pending_emits_on_commit_fires_every_queued_event"));
        assert!(sub.pop().is_none(), "second drain must be a no-op");
        reset_world("app_active_drain_pending_emits_on_commit_fires_every_queued_event");
    }

    /// `clear_pending_emits` must drop the queue WITHOUT publishing
    /// anything — the ROLLBACK path relies on this so subscribers
    /// never observe aborted mutations.
    #[test]
    fn clear_pending_emits_drops_without_firing() {
        reset_world("app_active_clear_pending_emits_drops_without_firing");
        let sub = crate::cdc::broker::subscribe(
            "app_active_clear_pending_emits_drops_without_firing",
            "messages",
        );

        let ev = zeroship_data_orm::cdc::ChangeEvent {
            app_id: "app_active_clear_pending_emits_drops_without_firing".to_string(),
            collection: "messages".to_string(),
            op: ChangeOp::Insert,
            pk: Some("99".to_string()),
            changed_columns: vec!["id".to_string()],
            new_tuple: HashMap::new(),
            old_tuple: None,
        };
        crate::tx_lanes::with_mut(|l| {
            l.push_pending_emit(
                &crate::tests::fixtures::harness_route(
                    "app_active_clear_pending_emits_drops_without_firing",
                ),
                ev,
            );
        });

        clear_pending_emits(&crate::tests::fixtures::harness_route("app_active_clear_pending_emits_drops_without_firing"));

        assert!(
            sub.pop().is_none(),
            "ROLLBACK path must drop queued events silently",
        );
        // After clear, drain must also be a no-op (queue is empty).
        drain_pending_emits_on_commit(&crate::tests::fixtures::harness_route(
            "app_active_clear_pending_emits_drops_without_firing",
        ));
        assert!(sub.pop().is_none(), "post-clear drain must publish nothing");
        reset_world("app_active_clear_pending_emits_drops_without_firing");
    }

    #[test]
    fn exec_mutation_with_emit_builds_when_active_subscriber() {
        reset_world("app_active_exec_mutation_with_emit_builds_when_active_subscriber");
        let sub = crate::cdc::broker::subscribe(
            "app_active_exec_mutation_with_emit_builds_when_active_subscriber",
            "messages",
        );

        let rows = vec![synthetic_row()];
        emit_for_rows(
            &rows,
            &crate::tests::fixtures::harness_route("app_active_exec_mutation_with_emit_builds_when_active_subscriber"),
            /* in_tx */ false,
            /* backend_publishes */ false,
            "messages",
            ChangeOp::Insert,
        );

        assert_eq!(
            tuple_built_count(),
            1,
            "with a live subscriber and no WAL suppression we MUST build the tuple",
        );
        // And the event must actually have been delivered: prove the
        // subscriber's queue holds the change.
        match sub.pop() {
            Some(crate::cdc::broker::SubscriptionMessage::Change(ev)) => {
                assert_eq!(
                    ev.app_id,
                    "app_active_exec_mutation_with_emit_builds_when_active_subscriber"
                );
                assert_eq!(ev.collection, "messages");
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.pk.as_deref(), Some("7"));
                // Every returned column is reported; order is not part of the contract.
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
        reset_world("app_active_exec_mutation_with_emit_builds_when_active_subscriber");
    }

    #[test]
    fn exec_mutation_with_emit_skips_local_emit_when_sqlite_cdc_publishes() {
        reset_world(
            "app_active_exec_mutation_with_emit_skips_local_emit_when_sqlite_cdc_publishes",
        );
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    crate::encryption::ProjectKeySource::unavailable(),
                )
                .expect("open sqlite backend"),
            );
            let handle = BackendHandle::new(Rc::clone(&backend));
            let sub = crate::cdc::broker::subscribe(
                "app_active_exec_mutation_with_emit_skips_local_emit_when_sqlite_cdc_publishes",
                "messages",
            );

            let rows = vec![synthetic_row()];
            emit_for_rows(
                &rows,
                &crate::tests::fixtures::harness_route("app_active_exec_mutation_with_emit_skips_local_emit_when_sqlite_cdc_publishes"),
                /* in_tx */ false,
                backend_publishes_committed_changes(&handle),
                "messages",
                ChangeOp::Insert,
            );

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
        reset_world(
            "app_active_exec_mutation_with_emit_skips_local_emit_when_sqlite_cdc_publishes",
        );
    }

    #[test]
    fn exec_mutation_with_emit_uses_logical_typed_id_for_pk() {
        reset_world("app_active_exec_mutation_with_emit_uses_logical_typed_id_for_pk");
        let sub = crate::cdc::broker::subscribe(
            "app_active_exec_mutation_with_emit_uses_logical_typed_id_for_pk",
            "messages",
        );

        let rows = vec![synthetic_typed_id_row()];
        emit_for_rows(
            &rows,
            &crate::tests::fixtures::harness_route("app_active_exec_mutation_with_emit_uses_logical_typed_id_for_pk"),
            /* in_tx */ false,
            /* backend_publishes */ false,
            "messages",
            ChangeOp::Insert,
        );

        match sub.pop() {
            Some(crate::cdc::broker::SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.pk.as_deref(), Some("usr_02hxtestsubscriptionid000"));
                assert_eq!(
                    ev.new_tuple.get("id").map(String::as_str),
                    Some("usr_02hxtestsubscriptionid000")
                );
            }
            other => panic!("expected Change variant, got {other:?}"),
        }
        reset_world("app_active_exec_mutation_with_emit_uses_logical_typed_id_for_pk");
    }

    #[test]
    fn sqlite_exec_helpers_use_tx_connection_when_present() {
        reset_world("app_exec");
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    crate::encryption::ProjectKeySource::unavailable(),
                )
                .expect("open sqlite backend"),
            );
            backend
                .attach_app_file("app_exec")
                .await
                .expect("ensure app schema");
            backend
                .execute_fixture(
                    r#"CREATE TABLE "app_exec"."notes" (
                           id INTEGER PRIMARY KEY,
                           title TEXT NOT NULL
                       )"#,
                    &[],
                )
                .await
                .expect("CREATE TABLE notes");

            // The handle is held HERE rather than parked in the adapter's
            // per-isolate context. These tests only ever used that context as a
            // carrier for a backend they had just opened themselves, and the
            // engine cannot name it any more.
            let handle = BackendHandle::new(Rc::clone(&backend));

            let admission = crate::transaction::TxAdmission::acquire(crate::tests::fixtures::harness_route("app_exec"))
                .await
                .expect("the fixture claims a free lane");
            crate::transaction::exec_begin_or_savepoint(
                false,
                None,
                &crate::tests::fixtures::harness_binding("app_exec"),
                handle.clone(),
            )
            .await
            .expect("begin through the transaction protocol");
            admission.handed_to_reducer();

            reset_sqlite_route();
            let inserted = exec_mutation(
                &ambient_route_for_tests(&crate::tests::fixtures::harness_binding("app_exec"), handle.clone()),
                CompiledQuery {
                    sql: r#"INSERT INTO "app_exec"."notes" (id, title)
                        VALUES (1, 'tx-row') RETURNING *"#
                        .to_string(),
                    params: vec![],
                },
            )
            .await
            .expect("exec_mutation through tx");
            assert_eq!(
                sqlite_route(),
                2,
                "exec_mutation must route through Session::Sqlite",
            );
            assert_eq!(
                inserted[0].get("title").and_then(Value::as_str),
                Some("tx-row"),
            );

            reset_sqlite_route();
            let count = exec_count(
                &ambient_route_for_tests(&crate::tests::fixtures::harness_binding("app_exec"), handle.clone()),
                CompiledQuery {
                    sql: r#"SELECT COUNT(*) AS count FROM "app_exec"."notes""#.to_string(),
                    params: vec![],
                },
            )
            .await
            .expect("exec_count through tx");
            assert_eq!(
                sqlite_route(),
                2,
                "exec_count must route through Session::Sqlite",
            );
            assert_eq!(count, 1);

            reset_sqlite_route();
            let rows = exec_query(
                &ambient_route_for_tests(&crate::tests::fixtures::harness_binding("app_exec"), handle.clone()),
                CompiledQuery {
                    sql: r#"SELECT title FROM "app_exec"."notes" WHERE id = 1"#.to_string(),
                    params: vec![],
                },
            )
            .await
            .expect("exec_query through tx");
            assert_eq!(
                sqlite_route(),
                2,
                "exec_query must route through Session::Sqlite",
            );
            assert_eq!(rows[0].get("title").and_then(Value::as_str), Some("tx-row"));

            assert!(matches!(
                crate::transaction::exec_settle(&crate::tests::fixtures::harness_route("app_exec"), false, None).await,
                crate::transaction::SettleOutcome::Ok
            ));
        });
        reset_world("app_exec");
    }

    // -------------------------------------------------------------------
    // Usage reporting - the exec boundary reports each successful operation
    // to the sink its route carries and reports NOTHING for a failed op or
    // for a route without a sink. Drives the real `exec_query` /
    // `exec_mutation` / `exec_count` path against a live SqliteBackend.
    // -------------------------------------------------------------------

    /// Collects what the ORM reports, in order.
    #[derive(Debug, Default)]
    struct RecordingSink(std::sync::Mutex<Vec<(String, u64)>>);

    impl crate::metrics::UsageSink for RecordingSink {
        fn record(&self, metric: &str, amount: u64) {
            self.0
                .lock()
                .expect("recording sink")
                .push((metric.to_owned(), amount));
        }
    }

    impl RecordingSink {
        fn records(&self) -> Vec<(String, u64)> {
            self.0.lock().expect("recording sink").clone()
        }

        fn totals(&self) -> std::collections::BTreeMap<String, u64> {
            let mut totals = std::collections::BTreeMap::new();
            for (metric, amount) in self.records() {
                *totals.entry(metric).or_default() += amount;
            }
            totals
        }
    }

    fn metered_route(
        app_id: &str,
        backend: BackendHandle,
        sink: &std::sync::Arc<RecordingSink>,
    ) -> TxRoute {
        crate::tx_route::CapturedRoute::pool_for_tests(app_id, backend.sql_registration().clone())
            .with_usage_for_tests(std::sync::Arc::clone(sink) as _)
            .bind(backend)
            .expect("test route registration matches backend")
    }

    /// Each route reports to its own sink, and a route without one reports
    /// nothing whatever its app id is.
    #[test]
    fn usage_reaches_only_the_sink_its_route_carries() {
        use std::sync::Arc;

        run(async {
            let dir = tempfile::tempdir().unwrap();
            let backend = BackendHandle::new(Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    dir.path().to_path_buf(),
                    crate::encryption::ProjectKeySource::unavailable(),
                )
                .unwrap(),
            ));
            let select = || CompiledQuery {
                sql: "SELECT 1 AS one".to_owned(),
                params: Vec::new(),
            };
            let first = Arc::new(RecordingSink::default());
            let second = Arc::new(RecordingSink::default());

            exec_query(
                &metered_route("app_usage_first", backend.clone(), &first),
                select(),
            )
            .await
            .expect("metered read");
            exec_query(
                &metered_route("app_usage_second", backend.clone(), &second),
                select(),
            )
            .await
            .expect("second metered read");
            assert_eq!(first.records(), vec![(DB_READS.to_owned(), 1)]);
            assert_eq!(second.records(), vec![(DB_READS.to_owned(), 1)]);

            // Not an app id, and no sink: the route binds and serves the read.
            let unmetered = ambient_route_for_tests(&crate::tests::fixtures::harness_binding("platform"), backend);
            assert!(unmetered.usage().is_none());
            exec_query(&unmetered, select())
                .await
                .expect("a route without a sink is never refused on attribution");
            assert_eq!(first.records(), vec![(DB_READS.to_owned(), 1)]);
            assert_eq!(second.records(), vec![(DB_READS.to_owned(), 1)]);
        });
    }

    #[test]
    fn usage_counts_reads_writes_rows_and_skips_failures() {
        use std::sync::Arc;
        let app_id = "app_usage_counts";
        reset_world(app_id);
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    crate::encryption::ProjectKeySource::unavailable(),
                )
                .expect("open sqlite backend"),
            );
            backend
                .attach_app_file(app_id)
                .await
                .expect("ensure app schema");
            backend
                .execute_fixture(
                    &format!(
                        r#"CREATE TABLE "{app_id}"."notes" (
                               id INTEGER PRIMARY KEY,
                               title TEXT NOT NULL
                           )"#
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE notes");

            let sink = Arc::new(RecordingSink::default());
            let handle = BackendHandle::new(Rc::clone(&backend));

            // 1 mutation returning 1 row -> db_writes +1, db_rows_written +1.
            exec_mutation(
                &metered_route(app_id, handle.clone(), &sink),
                CompiledQuery {
                    sql: format!(
                        r#"INSERT INTO "{app_id}"."notes" (id, title) VALUES (1, 'a') RETURNING *"#
                    ),
                    params: vec![],
                },
            )
            .await
            .expect("insert");

            // 1 query (read) -> db_reads +1.
            exec_query(
                &metered_route(app_id, handle.clone(), &sink),
                CompiledQuery {
                    sql: format!(r#"SELECT title FROM "{app_id}"."notes" WHERE id = 1"#),
                    params: vec![],
                },
            )
            .await
            .expect("select");

            // 1 count (read) -> db_reads +1.
            exec_count(
                &metered_route(app_id, handle.clone(), &sink),
                CompiledQuery {
                    sql: format!(r#"SELECT COUNT(*) AS count FROM "{app_id}"."notes""#),
                    params: vec![],
                },
            )
            .await
            .expect("count");

            // A FAILED op (bad SQL) must report NOTHING.
            let bad = exec_query(
                &metered_route(app_id, handle.clone(), &sink),
                CompiledQuery {
                    sql: format!(r#"SELECT nope FROM "{app_id}"."no_such_table""#),
                    params: vec![],
                },
            )
            .await;
            assert!(bad.is_err(), "the bad query must fail");

            for (filter, expected) in [("id = 1", 1), ("id = 2", 0)] {
                let affected = exec_mutation_count_with_emit(
                    CompiledQuery {
                        sql: format!(
                            r#"UPDATE "{app_id}"."notes" SET title = 'changed' WHERE {filter}"#
                        ),
                        params: vec![],
                    },
                    &metered_route(app_id, handle.clone(), &sink),
                    "notes",
                    ChangeOp::Update,
                )
                .await
                .expect("count-only update");
                assert_eq!(affected, expected);
            }
            assert!(exec_mutation_count_with_emit(
                CompiledQuery {
                    sql: format!(r#"DELETE FROM "{app_id}"."missing""#),
                    params: vec![],
                },
                &metered_route(app_id, handle.clone(), &sink),
                "missing",
                ChangeOp::Delete,
            )
            .await
            .is_err());

            assert_eq!(
                sink.totals(),
                std::collections::BTreeMap::from([
                    (DB_READS.to_owned(), 2),
                    (DB_ROWS_WRITTEN.to_owned(), 2),
                    (DB_WRITES.to_owned(), 3),
                ]),
                "one query + one count read, three successful mutation statements, and \
                 their returned or affected rows; the failed statements report nothing"
            );
            assert!(
                sink.records().iter().all(|(_, amount)| *amount != 0),
                "an update matching no row reports no rows: {:?}",
                sink.records()
            );
        });
        reset_world(app_id);
    }

    // -------------------------------------------------------------------
    // Cross-tenant transaction hijack via the thread-shared slot
    // -------------------------------------------------------------------
    //
    // The worker multiplexes many isolates (apps) per OS thread. When
    // app A's `env.db.transaction(async () => await fetch(slow))` parks
    // its tx client across the await, a co-resident app B's plain
    // `env.db.*` call lands on the same thread-local context. `run_sql`
    // / `exec_sqlite_values` must route B onto B's OWN autocommit path —
    // never onto A's pinned transaction connection (A's snapshot, A's
    // open tx, and — on Postgres — A's per-app role).

    #[test]
    fn sec1_app_b_query_must_not_route_through_app_a_parked_tx() {
        reset_world("app_a");
        reset_world("app_b");
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    crate::encryption::ProjectKeySource::unavailable(),
                )
                .expect("open sqlite backend"),
            );
            // The handle is held HERE rather than parked in the adapter's
            // per-isolate context. These tests only ever used that context as a
            // carrier for a backend they had just opened themselves, and the
            // engine cannot name it any more.
            let handle = BackendHandle::new(Rc::clone(&backend));

            // app_a opens an explicit transaction; its dedicated client
            // is parked in the per-isolate slot — exactly the state a
            // creator callback leaves behind across an `await`.
            let client = backend
                .fixture_session("app_a")
                .await
                .expect("acquire tx client");
            backend
                .execute_fixture_on(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            crate::tx_lanes::with_mut(|l| {
                let prev = l.install_tx_client(&crate::tests::fixtures::harness_route("app_a"), Session::new(client));
                assert!(prev.is_none(), "tx slot must start empty");
            });

            // Co-resident app_b now runs a plain (non-transactional)
            // query on the same thread.
            reset_sqlite_route();
            let rows = exec_query(
                &ambient_route_for_tests(&crate::tests::fixtures::harness_binding("app_b"), handle.clone()),
                CompiledQuery {
                    sql: "SELECT 'b' AS title".to_string(),
                    params: vec![],
                },
            )
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
                crate::tx_lanes::with(|l| l.has_tx_for(&crate::tests::fixtures::harness_route("app_a"))),
                "app_a's parked tx must survive app_b's access",
            );

            // Cleanup: roll app_a's tx back and drop the client.
            if let Some(client) = crate::tx_lanes::with_mut(|l| l.take_tx_client_for(&crate::tests::fixtures::harness_route("app_a"))) {
                let _ = client.exec("ROLLBACK", &[]).await;
            } else {
                panic!("app_a's tx client should still be parked for cleanup");
            }
        });
        reset_world("app_a");
        reset_world("app_b");
    }

    #[test]
    fn dropping_in_flight_sqlite_query_restores_tx_slot() {
        reset_world("app_exec_cancel");
        run(async {
            use std::time::Duration;

            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    crate::encryption::ProjectKeySource::unavailable(),
                )
                .expect("open sqlite backend"),
            );
            backend
                .attach_app_file("app_exec_cancel")
                .await
                .expect("ensure app schema");
            backend
                .execute_fixture(
                    r#"CREATE TABLE "app_exec_cancel"."notes" (
                           id INTEGER PRIMARY KEY,
                           title TEXT NOT NULL
                       )"#,
                    &[],
                )
                .await
                .expect("CREATE TABLE notes");
            backend
                .execute_fixture(
                    r#"INSERT INTO "app_exec_cancel"."notes" (id, title)
                        VALUES (1, 'persisted')"#,
                    &[],
                )
                .await
                .expect("seed row");

            // The handle is held HERE rather than parked in the adapter's
            // per-isolate context. These tests only ever used that context as a
            // carrier for a backend they had just opened themselves, and the
            // engine cannot name it any more.
            let handle = BackendHandle::new(Rc::clone(&backend));

            let admission = crate::transaction::TxAdmission::acquire(crate::tests::fixtures::harness_route("app_exec_cancel"))
                .await
                .expect("the fixture claims a free lane");
            crate::transaction::exec_begin_or_savepoint(
                false,
                None,
                &crate::tests::fixtures::harness_binding("app_exec_cancel"),
                handle.clone(),
            )
            .await
            .expect("begin through the transaction protocol");
            admission.handed_to_reducer();

            let gate = backend.arm_next_command_gate_for_tests();
            let spawned_handle = handle.clone();
            let task = crate::orm_context::spawn(async move {
                exec_query(
                    &ambient_route_for_tests(&crate::tests::fixtures::harness_binding("app_exec_cancel"), spawned_handle),
                    CompiledQuery {
                        sql: r#"SELECT title FROM "app_exec_cancel"."notes" WHERE id = 1"#
                            .to_string(),
                        params: vec![],
                    },
                )
                .await
            });
            compio::time::timeout(Duration::from_secs(5), gate.wait_until_blocked())
                .await
                .expect("the operation must reach the gate")
                .expect("worker must block on test gate");
            drop(task);
            gate.release();
            compio::time::sleep(Duration::from_millis(20)).await;

            assert!(
                crate::tx_lanes::with(|l| l.has_tx_for(&crate::tests::fixtures::harness_route("app_exec_cancel"))),
                "dropping the in-flight future must restore the tx slot"
            );

            assert!(matches!(
                crate::transaction::exec_settle(&crate::tests::fixtures::harness_route("app_exec_cancel"), false, None).await,
                crate::transaction::SettleOutcome::Ok
            ));

            let rows = exec_query(
                &ambient_route_for_tests(&crate::tests::fixtures::harness_binding("app_exec_cancel"), handle.clone()),
                CompiledQuery {
                    sql: r#"SELECT title FROM "app_exec_cancel"."notes" WHERE id = 1"#.to_string(),
                    params: vec![],
                },
            )
            .await
            .expect("subsequent query must work after rollback");
            assert_eq!(
                rows[0].get("title").and_then(Value::as_str),
                Some("persisted")
            );
        });
        reset_world("app_exec_cancel");
    }

    /// A gate armed on one session must leave every other session running.
    ///
    /// The slot must be per-session, not a process-global one-shot that any
    /// actor's run loop can take. A global slot lets arming it here stall an
    /// unrelated, concurrently running test's actor on `release_rx.recv()` and
    /// satisfy the arming test's `wait_until_blocked()` with that foreign
    /// command, leaving the test to assert against a query that was never gated -
    /// a race between tests that only bites when the timing lines up.
    ///
    /// The gate is held for the whole probe on purpose: dropping it closes
    /// `release_tx`, which would release a wrongly-gated actor and hide the
    /// very stall this test is looking for.
    #[test]
    fn next_command_gate_does_not_stall_another_session() {
        run(async {
            use std::time::Duration;

            let dir_a = tempfile::tempdir().expect("tempdir a");
            let dir_b = tempfile::tempdir().expect("tempdir b");
            let backend_a = crate::backend_selection::new_sqlite_backend(
                PathBuf::from(dir_a.path()),
                crate::encryption::ProjectKeySource::unavailable(),
            )
            .expect("open backend a");
            let backend_b = crate::backend_selection::new_sqlite_backend(
                PathBuf::from(dir_b.path()),
                crate::encryption::ProjectKeySource::unavailable(),
            )
            .expect("open backend b");

            let _gate = backend_a.arm_next_command_gate_for_tests();

            let ran = compio::time::timeout(
                Duration::from_secs(5),
                backend_b.execute_fixture("CREATE TABLE probe (id INTEGER PRIMARY KEY)", &[]),
            )
            .await;

            assert!(
                ran.is_ok(),
                "a gate armed on backend A must not stall backend B's actor"
            );
            ran.expect("not stalled").expect("backend B command");
        });
    }

    // -------------------------------------------------------------------
    // The non-tx (autocommit) path must NOT leak role/timeout on a
    // cancelled query.
    // -------------------------------------------------------------------
    //
    // The test owns a PostgreSQL container. Docker or startup failure is a
    // failed test; the live cancellation assertion always runs.
    //
    // The leak window this
    // test guards can only be observed against a real backend, so a run
    // that quietly returned on a connect failure would let the leak go
    // unchecked while the suite still reported green.
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

    #[test]
    fn autocommit_cancelled_query_does_not_leak_role_or_timeout_to_pool() {
        use compio_postgres::{NoTls, Pool};
        use std::time::Duration;

        reset_world("app_autocommit_cancelled_query_does_not_leak_role_or_timeout_to_pool");
        run(async {
            let postgres = crate::tests::fixtures::postgres::Postgres::start();
            let url = postgres.url();
            match compio_postgres::connect(&url, NoTls).await {
                Ok((client, connection)) => {
                    crate::orm_context::spawn(async move {
                        let _ = connection.run().await;
                    })
                    .detach();
                    drop(client);
                }
                Err(e) => {
                    // A Postgres this test cannot dial is a FAILURE, not a
                    // skip: the role/timeout leak this test guards against
                    // only reproduces against a real backend, and returning
                    // here would let that leak go unchecked while the suite
                    // still reported green.
                    panic!("autocommit-leak test could not connect to {url}: {e}");
                }
            }

            // Pool of ONE so the cancelled-then-reused checkout lands on
            // the same backend whose session state we want to inspect.
            let pool = Rc::new(Pool::connect(&url, 1).await.expect("pool connect"));

            let app_id = "p2c1leak";
            let binding = crate::tests::fixtures::harness_binding(app_id);
            let role = binding
                .session_role()
                .expect("a harness binding narrows")
                .to_owned();
            let role_ident = crate::sql::mapping::quote_ident(&role);

            // Discover the login role so we can (a) GRANT it membership
            // in the app role (required for SET LOCAL ROLE) and (b)
            // assert the connection returns to it after cancellation.
            let login_user = {
                let c = pool.acquire().await.expect("checkout for setup");
                let rows = c
                    .query_text_params("SELECT current_user AS u", &[])
                    .await
                    .expect("current_user");
                rows[0].get::<_, &str>("u").to_string()
            };

            // Provision the per-app role directly (NOLOGIN) and grant the
            // login user membership so `SET LOCAL ROLE` succeeds.
            {
                let c = pool.acquire().await.expect("checkout for role setup");
                let _ = c
                    .simple_query(&format!("DROP ROLE IF EXISTS {role_ident}"))
                    .await;
                c.simple_query(&format!("CREATE ROLE {role_ident} NOLOGIN"))
                    .await
                    .expect("create app role");
                c.simple_query(&format!(
                    "GRANT {role_ident} TO {}",
                    crate::sql::mapping::quote_ident(&login_user)
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
                crate::backend::pg_autocommit::scoped_rows(
                    &pool,
                    &binding,
                    crate::connection::SessionAuthority::PerBindingRole,
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
            let c = pool.acquire().await.expect("re-checkout after cancel");
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
                    crate::sql::mapping::quote_ident(&login_user)
                ))
                .await;
            let _ = c
                .simple_query(&format!("DROP ROLE IF EXISTS {role_ident}"))
                .await;
        });
        reset_world("app_autocommit_cancelled_query_does_not_leak_role_or_timeout_to_pool");
    }
}
