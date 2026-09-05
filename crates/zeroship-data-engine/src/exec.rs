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
//! All four dispatch on [`BackendHandle`] with an exhaustive `match`. The
//! `Postgres` arm is `run_sql`, which uses the per-isolate TX client
//! (`ThreadDbContext::tx_conns`) when the DISPATCH THAT STARTED THIS OP was
//! issued inside the app's own `db.transaction(fn)` callback, and the pool
//! otherwise; the `Sqlite` arm is `exec_sqlite_json`.
//!
//! ## Why the entry points take `&TxRoute` and not `app_id: &str`
//!
//! Until 2026-08-10 the three routing sites here read ambient state —
//! `crate::tx_lanes::with(|l| l.has_tx_for(app_id))`, "does this app have a
//! transaction open RIGHT NOW". That is a temporal test standing in for a
//! structural one. An ORDINARY write with no transaction anywhere in its
//! call chain, merely overlapping a stranger's transaction on the same
//! isolate, was routed onto that stranger's connection and destroyed by
//! its ROLLBACK — while reporting success. Measured on both tiers by
//! `tests/e2e_dev_vs_deployed_db.sh` (`cxPlain`).
//!
//! The decision is now [`crate::tx_route::TxRoute`], captured
//! synchronously in the `dispatch_*` prelude while `scope` is still live
//! (it reads V8's continuation-preserved slot, which is the structural
//! test) and moved into the spawned future. `TxRoute`'s only production
//! constructor takes `&mut v8::PinScope`, so a dispatch site that forgets
//! to capture has nothing to pass here and fails to compile rather than
//! silently defaulting to the pool.
//!
//! ## Error rail
//!
//! Every fallible helper here returns [`zeroship_data_core::error::DbError`] — the
//! `dispatch_*` layer in `crate::crud` calls
//! the adapter tier's `op_error::ToOpError::to_op_error` at the V8 boundary so each
//! throw carries `.code` for the SDK to branch on (replacing the
//! earlier `Result<_, String>` rail).
//!
//! The transaction-emit deferral lives here:
//! `queue_or_emit` decides between immediate emit and TX-pending
//! queueing; `drain_pending_emits_on_commit` fires the queue on
//! COMMIT; `clear_pending_emits` discards it on ROLLBACK.

use std::rc::Rc;

use serde_json::Value;

use crate::backend::BackendHandle;
use crate::backend::pg_error;
use crate::backend::pg_row_json::rows_to_json_value;
// THE ADAPTER IMPORT THAT USED TO SIT HERE IS GONE, and it had to be. It was
// `#[cfg(any(test, feature = "test-helpers"))] use crate::context;`, read only
// by `ambient_route_for_tests`, and the note above it said the gate kept the
// edge off `tests/lib/tier_direction_census.sh` "but cargo will not once the
// engine is its own crate". That day arrived on 2026-09-03: `test-helpers` is a
// normal feature, so a `--features test-helpers` build compiles the read into
// the LIB, which would need `zeroship-plugin-db` as a normal dependency of the
// crate the adapter depends on. Only a DEV-dependency cycle is legal, and even
// that would link two copies of this crate and give every `thread_local!` here
// two instances.
//
// The fix is the one the crate has taken three times already: the backend is a
// PARAMETER. See `ambient_route_for_tests` below.
use crate::tx_lanes::TxConnection;
use zeroship_data_core::error::DbError;
use crate::query::BuiltQuery;
use crate::tx_route::TxRoute;

// The metric names and the emit point moved to `crate::metrics` on 2026-09-01.
// They were private to this file, which made the BILLED surface accidentally
// equal to "whatever flows through `run_sql` / `exec_mutation`" - and the search
// family and every unmask statement do not.
use crate::metrics::{DB_READS, DB_ROWS_WRITTEN, DB_WRITES, emit_db_metric};

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

// There is deliberately no `ensure_backend_for_shared_sql` here any more.
//
// It was the last ENGINE-to-ADAPTER edge in this crate: an engine file reading
// `crate::context` and calling `crate::init_pool_async`, the one dependency
// direction the crate split forbids. It moved VERBATIM to
// `crate::tx_scope::ensure_backend`, which is adapter-side, where the thread
// context and the lazy init both already live; the cold-init arm went with it
// and is still load-bearing for `installSchema`'s boot-time `setMaskPolicy`.
//
// Nothing in this file resolves a backend now. The routed helpers below take it
// off `route.backend()`, which `tx_scope::bind_route` bound for them at the
// dispatch frame, so the handle a statement runs on is the handle its routing
// decision was made against - one read, not two.

// There is deliberately no `ensure_postgres_backend_for_shared_sql` any more.
//
// It resolved a backend of its own and then narrowed it, which meant it could
// not take the route's already-bound handle - it took no arguments at all. Its
// two callers now narrow where they stand:
// `exec_postgres_autocommit_with_role` against `route.backend()` (ENGINE), and
// `v8_classes/replication.rs` against `tx_scope::ensure_backend()` (ADAPTER to
// ADAPTER, and correct there: a slot diagnostic is never in a transaction).

/// The op was dispatched inside a `db.transaction(fn)` callback whose
/// transaction has since settled — a continuation that outlived its
/// transaction (typically a promise the callback started and never
/// awaited).
///
/// Refused rather than silently autocommitted on a pooled connection.
/// Falling back to the pool would let work the creator wrote INSIDE a
/// transaction commit on its own after that transaction rolled back,
/// which is the mirror image of the defect `TxRoute` fixes. Mirrors
/// `transaction::transaction_dispatch`'s `transaction_scope_expired`.
fn tx_scope_expired() -> DbError {
    DbError::validation_hinted(
        "transaction_scope_expired",
        "db: this operation was issued inside a transaction that has already settled".to_string(),
        "Every env.db call started inside a db.transaction(...) callback must be awaited before \
         the callback returns; work left running past the callback has no transaction to run on.",
    )
}

/// The route says "in transaction" and the transaction is still open, but
/// its connection is checked out by another in-flight op for the same
/// app. A transaction owns exactly ONE connection, so two of its
/// operations cannot be in flight at once.
///
/// Split from [`tx_scope_expired`] deliberately: an empty tx slot has two
/// causes and they call for opposite fixes (await your calls vs. do not
/// leak work past the callback). Before this split both arrived as
/// `DbError::internal("db: transaction connection lost")`, which named
/// neither. `tx_claimed_by` is the discriminator — the top-level claim is
/// held for the whole transaction, including the window where the client
/// is checked out.
fn tx_connection_busy() -> DbError {
    DbError::validation_hinted(
        "transaction_connection_busy",
        "db: another operation is already using this transaction's connection".to_string(),
        "A transaction has one connection, so its operations cannot overlap. Await each env.db \
         call inside the db.transaction(...) callback before starting the next — a Promise.all \
         over several tx operations runs them concurrently on that one connection.",
    )
}

/// Which of the two empty-slot causes applies for `app_id`.
fn tx_slot_unavailable(app_id: &str) -> DbError {
    if crate::tx_lanes::with(|l| l.tx_claimed_by(app_id)) {
        tx_connection_busy()
    } else {
        tx_scope_expired()
    }
}

/// Take this dispatch's parked transaction client, or refuse the way every
/// other routed statement refuses.
///
/// `TxClientSlotGuard::take` reports an empty slot as a bare internal error,
/// which is not what a creator should see: a route that says "in transaction"
/// and finds no session means either that the transaction has settled under a
/// continuation that outlived it, or that another op is holding its one
/// connection. [`tx_slot_unavailable`] tells those apart and mints the coded
/// errors documented on [`TxRoute::in_tx`].
///
/// `pub(crate)` because the routed raw-column reads in
/// [`crate::backend_handle`] need the same claim, and they must not re-derive
/// the classification.
pub(crate) fn take_tx_lane(route: &TxRoute) -> Result<crate::tx_lanes::TxClientSlotGuard, DbError> {
    crate::tx_lanes::TxClientSlotGuard::take(route.app_id())
        .map_err(|_| tx_slot_unavailable(route.app_id()))
}

/// Execute SQL with text params — uses the app's TX connection when this
/// dispatch was issued inside that transaction, otherwise the pool.
pub async fn run_sql(
    route: &TxRoute,
    sql: &str,
    params: &[&str],
) -> Result<Vec<Value>, DbError> {
    let app_id = route.app_id();
    // Structural, not temporal: `route.in_tx()` was frozen at the V8
    // dispatch frame from the continuation-preserved transaction scope,
    // so it is true only for ops issued INSIDE this app's own
    // `db.transaction(fn)` callback. SEC-1 falls out of the same
    // comparison: a co-resident app's callback plants ITS app_id, so this
    // app reads `false` and takes its own autocommit path.
    if route.in_tx() {
        // Use this app's transaction connection
        let client = crate::tx_lanes::with_mut(|l| l.take_tx_client_for(app_id))
            .ok_or_else(|| tx_slot_unavailable(app_id))?;
        let result = match &client {
            TxConnection::Postgres(client) => client.query_text_params(sql, params).await,
            TxConnection::Sqlite(_) => {
                crate::tx_lanes::with_mut(|l| l.put_tx_client_for(app_id, client));
                return Err(sqlite_shared_crud_unavailable());
            }
        };
        // Put it back
        crate::tx_lanes::with_mut(|l| l.put_tx_client_for(app_id, client));
        return result
            .map(|rows| rows_to_json_value(&rows))
            .map_err(|e| pg_error::classify(&e));
    }

    // No transaction — use pool. On the SQLite arm the shared CRUD
    // row-returning path is not wired yet; surface a typed error
    // instead of falling through to a misleading `pool not initialized`.
    exec_postgres_autocommit_with_role(route, sql, params).await
}

/// Execute a built query via pool (or TX conn) and return the
/// per-row JSON values.
///
/// Returning `Vec<Value>` (rather than a pre-serialised JSON array
/// string) lets the CRUD resolver chain in `crate::crud` inspect or
/// take a single row without paying for an intermediate serialise +
/// reparse round-trip. The final JSON string is materialised once at
/// the V8 boundary (`ResolveValue::Json`).
///
/// # The backend dispatch below is an exhaustive `match`, and stays one
///
/// This and its two siblings ([`exec_count`], [`exec_mutation`]) each opened
/// with `if let BackendHandle::Sqlite(sq) = route.backend() { ...; return }` and
/// fell through to [`run_sql`] until 2026-09-04. That made `Postgres` the
/// engine's DEFAULT execution route rather than one arm of a choice, at three
/// sites. It was caught, but only downstream and only by accident: the
/// fall-through reaches the exhaustive `match` in
/// [`exec_postgres_autocommit_with_role`], so a third backend broke the build at
/// a function whose name does not mention the branch the author had to write.
/// Spelled as a `match` here, the error lands where the routing is decided.
/// **Do not reopen any of the three as an `if let`.**
pub async fn exec_query(route: &TxRoute, bq: BuiltQuery) -> Result<Vec<Value>, DbError> {
    let app_id = route.app_id();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = match route.backend() {
        BackendHandle::Sqlite(sq) => exec_sqlite_json(route, sq, &bq.sql, &param_refs).await?,
        BackendHandle::Postgres(_) => run_sql(route, &bq.sql, &param_refs).await?,
    };
    // Success arm only: one read op. Unforgeable (emitted by the primitive).
    emit_db_metric(app_id, DB_READS, 1);
    Ok(rows)
}

/// Execute a built query expecting a count result.
///
/// Returns the raw integer; callers wrap into the appropriate
/// `OpResult` shape (typically `ResolveValue::F64` so JS sees a real
/// `number`).
///
/// Exhaustive backend dispatch, for the reason on [`exec_query`].
pub async fn exec_count(route: &TxRoute, bq: BuiltQuery) -> Result<i64, DbError> {
    let app_id = route.app_id();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = match route.backend() {
        BackendHandle::Sqlite(sq) => exec_sqlite_json(route, sq, &bq.sql, &param_refs).await?,
        BackendHandle::Postgres(_) => run_sql(route, &bq.sql, &param_refs).await?,
    };
    // Success arm only: a count is a read op.
    emit_db_metric(app_id, DB_READS, 1);

    // ONE extraction for both dialects, which is the point: this was written
    // twice, byte-identically, once per arm, back when each arm returned on its
    // own. Both arms hand back JSON rather than a `compio_postgres::Row`, so the
    // two dialects agree on the shape a count comes back in. PostgreSQL renders
    // `count(*)` as INT8 (OID 20), which `row_to_json` maps to an exact
    // `Number::from(i64)` - so `as_i64` reads it back losslessly rather than
    // going via `f64` the way the FLOAT arms do.
    Ok(rows
        .first()
        .and_then(|row| row.get("count"))
        .and_then(Value::as_i64)
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
///
/// Exhaustive backend dispatch, for the reason on [`exec_query`].
pub async fn exec_mutation(route: &TxRoute, bq: BuiltQuery) -> Result<Vec<Value>, DbError> {
    let app_id = route.app_id();
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = match route.backend() {
        BackendHandle::Sqlite(sq) => exec_sqlite_json(route, sq, &bq.sql, &param_refs).await?,
        BackendHandle::Postgres(_) => run_sql(route, &bq.sql, &param_refs).await?,
    };
    // Success arm only: one write op + the affected/RETURNING row count.
    emit_db_metric(app_id, DB_WRITES, 1);
    emit_db_metric(app_id, DB_ROWS_WRITTEN, rows.len() as u64);
    Ok(rows)
}

/// Run `sql` on the shared Postgres backend under `app_id`'s role.
///
/// The role fence, the `SET LOCAL` batch and the surrounding transaction are
/// PostgreSQL dialect and live in [`crate::backend::pg_autocommit`]. They sat
/// here until 2026-09-01, which put them one tier ABOVE the Postgres backend
/// that called into them - the upward half of the `PG <-> ENGINE` cycle. On
/// 2026-09-02 the last half of that reach went too: this took the pool out of
/// the backend and called `pg_autocommit` itself, so the `Row` type came back
/// here to be converted. Now the backend does both and returns JSON.
async fn exec_postgres_autocommit_with_role(
    route: &TxRoute,
    sql: &str,
    params: &[&str],
) -> Result<Vec<Value>, DbError> {
    // The Postgres narrowing happens here, against the backend the adapter
    // already bound onto the route, rather than in a wrapper that resolved one
    // of its own. Both arms are ENGINE types, so this whole path is
    // engine-internal.
    match route.backend() {
        BackendHandle::Postgres(pg) => {
            // SCHEMA: the roled autocommit funnel derives the per-app role from it.
            pg.query_roled_rows_as_json(route.schema(), sql, params)
                .await
        }
        BackendHandle::Sqlite(_) => Err(sqlite_shared_crud_unavailable()),
    }
}

async fn exec_sqlite_json(
    route: &TxRoute,
    backend: &crate::backend::sqlite::SqliteBackend,
    sql: &str,
    params: &[&str],
) -> Result<Vec<Value>, DbError> {
    // Bind this app's file into the session before addressing it. The SQL
    // below qualifies its tables as `"<app_id>"."<table>"`, and that alias
    // exists only because of an ATTACH.
    //
    // This is the data plane's own job. A row read must not depend on an
    // earlier metadata callback having attached the file on this thread.
    //
    // Cheap to repeat. `attach_app_file` returns on a cache hit before issuing
    // any SQL, so every call after the first is a set lookup. Placing it above
    // the tx/autocommit split covers both: SQLite has ONE session actor (see
    // `SqlExecutor::acquire_dedicated_client` for SqliteBackend), so a
    // "dedicated" tx client is the same connection and inherits the ATTACH.
    backend.attach_app_file(route.app_id()).await?;

    // Same discriminator as `run_sql`: the route was frozen at the V8
    // dispatch frame, so only ops issued inside THIS app's own
    // `db.transaction(fn)` callback take the tx client. SEC-1 falls out of
    // it, and so does the `cxPlain` case (an ordinary overlapping write
    // stays on the shared autocommit path).
    if !route.in_tx() {
        #[cfg(test)]
        tests::record_sqlite_shared_route();
        return backend.query_json(sql, params).await;
    }

    // `take` has exactly one failure mode — an empty slot — which under a
    // route that says "in transaction" means either the transaction has
    // settled or another op holds its connection. Re-typed so the creator
    // sees the same coded errors the Postgres arm produces.
    let client = crate::tx_lanes::TxClientSlotGuard::take(route.app_id())
        .map_err(|_| tx_slot_unavailable(route.app_id()))?;
    let result = match client.client() {
        TxConnection::Sqlite(client) => {
            #[cfg(test)]
            tests::record_sqlite_tx_route();
            let typed = client.query_typed_internal(sql, params).await?;
            Ok(crate::backend::sqlite::row_json::typed_rows_to_json_value(
                &typed,
            ))
        }
        TxConnection::Postgres(_) => Err(DbError::internal(
            "db: sqlite backend active with postgres transaction connection",
        )),
    };
    result
}

/// Execute a mutation, then emit a [`crate::broker::emit_local`]
/// event into the in-process broker on success.
///
/// This is the coarse-grained reactive-query bridge: every
/// successful INSERT/UPDATE/DELETE produces one or more events on
/// `(app_id, collection)` that wake any matching subscribers in the
/// same isolate.
///
/// On error the broker is untouched — partial writes produce no
/// events. The error message is forwarded verbatim.
///
/// `op` selects the [`zeroship_core::change_event::ChangeOp`] tagged on the event;
/// the caller knows whether it called `build_insert`, `build_update_one`,
/// `build_delete_one`, etc. so we don't try to infer it from the SQL.
///
/// A future read-set narrowing pass could extend this helper to
/// populate `changed_columns` from the SET clause and the logical `id`
/// from the RETURNING row. For now this collects what's already in
/// the result `Value`.
pub async fn exec_mutation_with_emit(
    bq: BuiltQuery,
    route: &TxRoute,
    collection: &str,
    op: zeroship_core::change_event::ChangeOp,
) -> Result<Vec<Value>, DbError> {
    // `exec_mutation` returns the typed `Vec<Value>` already decoded
    // from `compio_postgres::Row`. Pre-fix we re-parsed our own JSON
    // string here just to extract the PK + changed-column set; now we
    // iterate the live `Value`s directly. The CRUD resolver in
    // `crud.rs` does the final `Value::Array(rows).to_string()` once
    // at the V8 boundary.
    let rows = exec_mutation(route, bq).await?;
    emit_for_rows(
        &rows,
        route.app_id(),
        route.in_tx(),
        backend_publishes_committed_changes(route.backend()),
        collection,
        op,
    );
    Ok(rows)
}

/// Does the backend publish committed changes on its own?
///
/// SQLite does, through the writer actor's commit hook. PostgreSQL does not on
/// this path - the WAL consumer is a separate process concern - so the local
/// emit below is what feeds subscribers there.
///
/// **A function of the backend, taken as an argument.** This read
/// `context::with(|c| ...)` until 2026-09-03, which is how an ENGINE file came
/// to depend on the ADAPTER's thread state, and the census could not see it:
/// the unqualified `context::` call does not match the extractor's
/// `crate::`-prefixed pattern, so only the `use` at the top of this file kept
/// the edge visible at all.
///
/// # This `match` is the guard, and it is the only guard there can be
///
/// It was `matches!(backend, BackendHandle::Sqlite(_))` until 2026-09-04 -
/// equivalent while [`BackendHandle`] has exactly two variants, so the defect
/// was LATENT, not live. It goes live at the event decision 4 of
/// `docs/proposals/2026-08-31-data-crate-shape.md` governs: a third backend
/// reads `false` here, [`emit_for_rows`] then publishes on its behalf, and a
/// backend that publishes its OWN commits delivers every change to every
/// subscriber twice. Nothing names the branch the author failed to write - no
/// compile error, no failing test.
///
/// So this is written as a `match` with one arm per variant and NO wildcard,
/// and the compile error a third variant produces here IS the regression test.
/// **Do not reopen it as `matches!`, an `if let`, or a `_ =>` arm.** The
/// question is not "is this the `Sqlite` arm" (which a new backend can answer
/// `false` to safely); it is "does this backend publish its own committed
/// changes", whose answer for a backend nobody has written yet is UNKNOWN, and
/// `false` is an assumption rather than a default.
///
/// What that buys, stated plainly: the compiler refuses a new variant that does
/// not answer HERE. What it does not buy: it cannot check that the answer is
/// CORRECT, it says nothing about the other non-exhaustive `BackendHandle`
/// sites in this workspace, and nothing in `tests/` prevents a future edit from
/// collapsing it back into a `matches!`. Only a source gate can hold that last
/// one; see the report attached to tracker #189.
fn backend_publishes_committed_changes(backend: &BackendHandle) -> bool {
    match backend {
        // The writer actor's commit hook publishes committed changes itself;
        // an SDK-local emit here would race it and duplicate every event.
        BackendHandle::Sqlite(_) => true,
        // The WAL consumer is a separate process concern and is not always
        // running, so the local emit below is what feeds subscribers here.
        BackendHandle::Postgres(_) => false,
    }
}

/// Build and queue/emit broker events for a mutation's RETURNING rows.
///
/// Split out of [`exec_mutation_with_emit`] so the gating logic can be
/// unit-tested without a Postgres connection: callers pass the already-
/// fetched `Vec<Value>` and we run the same gate + per-row build the
/// production path runs.
///
/// Mirrors `wal_consumer::emit_for_tuple`'s fix on the cross-worker
/// path: short-circuit the per-row `(columns, tuple)` build before
/// allocating anything the broker will discard.
///
/// Two gates, both cheap:
///
///   1. `broker::is_app_suppressed(app_id)` — when the WAL
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
    in_tx: bool,
    backend_publishes: bool,
    collection: &str,
    op: zeroship_core::change_event::ChangeOp,
) {
    if rows.is_empty() {
        // No rows affected — no broker event. UPDATE with a non-
        // matching filter falls here; subscribers should not see a
        // spurious change.
        return;
    }
    if backend_publishes {
        // SQLite has a commit-time CDC publisher wired through the writer
        // actor's preupdate/commit hooks. The old SDK-local emit was kept
        // for the Postgres/no-WAL-consumer path; on SQLite it races the CDC
        // publisher and produces duplicate identical live snapshots.
        return;
    }
    if crate::broker::is_app_suppressed(app_id)
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
        // diff that we don't have here; a future pass could compute it
        // from the mutation's SET clause directly.
        let (columns, tuple): (Vec<String>, std::collections::HashMap<String, String>) = match row {
            Value::Object(m) => {
                let cols = m
                    .keys()
                    .filter(|k| !matches!(k.as_str(), "created_at" | "updated_at"))
                    .cloned()
                    .collect();
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
        queue_or_emit(app_id, in_tx, collection, op, pk, columns, tuple);
    }
}

/// If a transaction is active on this thread, queue the event in
/// the per-isolate context's `pending_emits` slot for the settle
/// path to drain on COMMIT. Otherwise (autocommit), fire it
/// immediately. Subscribers no longer observe pre-commit state.
fn queue_or_emit(
    app_id: &str,
    in_tx: bool,
    collection: &str,
    op: zeroship_core::change_event::ChangeOp,
    pk: Option<String>,
    changed_columns: Vec<String>,
    new_tuple: std::collections::HashMap<String, String>,
) {
    // Queue only when THIS write actually ran on THIS app's transaction —
    // the same route that decided which connection the SQL used, so the
    // event's fate cannot disagree with the row's. A write that merely
    // overlapped someone else's transaction is in autocommit and must emit
    // immediately; queueing it would park the event on a settle path that
    // belongs to a different unit of work (previously it was both routed
    // onto and queued behind a stranger's transaction).
    if !in_tx {
        crate::broker::emit_local(app_id, collection, op, pk, changed_columns, new_tuple);
        return;
    }
    let ev = zeroship_core::change_event::ChangeEvent {
        app_id: app_id.to_string(),
        collection: collection.to_string(),
        op,
        pk,
        changed_columns,
        new_tuple,
        old_tuple: None,
    };
    crate::tx_lanes::with_mut(|l| l.push_pending_emit(ev));
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
pub fn drain_pending_emits_on_commit(app_id: &str) {
    let queued: Vec<zeroship_core::change_event::ChangeEvent> =
        crate::tx_lanes::with_mut(|l| l.drain_pending_emits_for(app_id));
    for ev in queued {
        crate::broker::emit_local(
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
pub fn clear_pending_emits(app_id: &str) {
    crate::tx_lanes::with_mut(|l| l.clear_pending_emits_for(app_id));
}

/// Reconstruct a [`TxRoute`] from the ambient parked-tx slot and the backend
/// the caller already holds, for the no-isolate test helpers only.
///
/// **The backend is a parameter, and that is the whole point.** This used to
/// read `crate::context::with(|c| c.backend())` - an ENGINE file reaching into
/// the ADAPTER's per-isolate context. The gate kept it off
/// `tests/lib/tier_direction_census.sh` and its own comment said cargo would not
/// be as forgiving once the engine became a crate. It did not: `test-helpers` is
/// a normal feature, so that read would have needed the adapter as a normal
/// dependency of its own dependency.
///
/// The two `*_for_tests` wrappers that used to sit either side of this function
/// moved to the adapter with it - `zeroship_plugin_db::exec_mutation_with_emit_for_tests`
/// and `zeroship_plugin_db::exec_query_for_tests`, which resolve the backend
/// through `tx_scope::ensure_backend()` where the thread context lives.
///
/// **The dialect is DERIVED from that backend, not assumed.** The two test
/// constructors below it stamped `SqlDialect::Postgres` unconditionally until
/// 2026-09-03, which made every SQLite harness that reaches this helper -
/// `test_support::unit_route` and `crates/zeroship-plugin-db/tests/sqlite_integration.rs` among them -
/// carry a route claiming a dialect its connection does not speak. Nothing read
/// it, so nothing failed; that is luck, not containment. Production reads the
/// configured dialect BEFORE a backend exists and must not re-derive it (see
/// [`crate::tx_route::TxRoute::dialect`]), but this helper is handed the open
/// backend up front, so here the handle is the best answer available and asking
/// it is strictly better than picking one.
#[cfg(any(test, feature = "test-helpers"))]
pub fn ambient_route_for_tests(app_id: &str, backend: crate::backend::BackendHandle) -> TxRoute {
    let dialect = match &backend {
        crate::backend::BackendHandle::Postgres(_) => crate::query::SqlDialect::Postgres,
        crate::backend::BackendHandle::Sqlite(_) => crate::query::SqlDialect::Sqlite,
    };
    let captured = if crate::tx_lanes::with(|l| l.has_tx_for(app_id)) {
        crate::tx_route::CapturedRoute::tx_for_tests(app_id, dialect)
    } else {
        crate::tx_route::CapturedRoute::pool_for_tests(app_id, dialect)
    };
    // Sync, and it can be: only the COLD path needs to await, and a harness
    // driving exec directly has already opened a backend. Production binds
    // through `tx_scope::bind_route`, which owns the cold arm.
    captured.bind(backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SqlExecutor as _;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::rc::Rc;
    use zeroship_core::change_event::ChangeOp;

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

    /// Reset the state one test owns, scoped to `app_id`:
    ///   - that app's broker subscriptions,
    ///   - that app's WAL suppression entry,
    ///   - the per-test build counter and SQLite route (both thread-local).
    ///
    /// **Scoped on purpose.** This used to call `drop_app(None)` - dropping
    /// EVERY app's subscriptions process-wide - and to decrement the
    /// suppression refcount for a hardcoded list of keys belonging to other
    /// tests. Its doc comment described all of that as "thread-local state",
    /// but `broker`'s registry and `wal_consumer::SUPPRESSED_APPS` are
    /// process-global statics, and cargo runs these tests on parallel threads.
    /// So one test's cleanup silently tore down a concurrently-running test's
    /// world: that is what made
    /// `exec_mutation_with_emit_skips_build_when_app_suppressed` fail in 2 of 9
    /// consecutive runs on unmodified code.
    ///
    /// Scoping also makes these tests able to SEE cross-app leakage rather than
    /// hiding it - a global reset erases the evidence of exactly the bug the
    /// suppression refcount exists to prevent.
    fn reset_world(app_id: &str) {
        crate::broker::drop_app(Some(app_id));
        crate::broker::unsuppress_app(app_id);
        reset_counter();
        reset_sqlite_route();
    }

    /// One test's cleanup must not tear down another's world.
    ///
    /// The broker registry and `wal_consumer::SUPPRESSED_APPS` are
    /// process-global statics and cargo runs these tests on parallel threads,
    /// so a cleanup with global blast radius corrupts whatever else is running.
    /// `reset_world` used to call `drop_app(None)`, which is what this arm
    /// pins: it stands in for a concurrent test that owns `theirs` and checks
    /// that our cleanup leaves it intact. On the pre-fix helper the
    /// subscription assertion fails.
    #[test]
    fn reset_world_leaves_other_apps_untouched() {
        let mine = "app_reset_scope_mine";
        let theirs = "app_reset_scope_theirs";

        reset_world(mine);

        // Stand in for a test running concurrently on another thread.
        let their_sub = crate::broker::subscribe(theirs, "messages");
        crate::broker::suppress_app(theirs);

        // Our cleanup fires while they are mid-test.
        reset_world(mine);

        assert!(
            crate::broker::is_app_suppressed(theirs),
            "reset_world cleared another app's suppression",
        );
        assert!(
            crate::broker::has_subscribers(theirs, "messages"),
            "reset_world dropped another app's broker subscription",
        );

        crate::broker::unsuppress_app(theirs);
        drop(their_sub);
        reset_world(theirs);
        reset_world(mine);
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    /// A test route must speak the dialect of the connection it is bound to.
    ///
    /// [`ambient_route_for_tests`] stamped `SqlDialect::Postgres` on every route
    /// it minted until 2026-09-03, because that is what
    /// `CapturedRoute::pool_for_tests` hardcoded. Every SQLite harness that
    /// reaches this helper - `test_support::unit_route` and the whole of
    /// `crates/zeroship-plugin-db/tests/sqlite_integration.rs` - therefore carried a route claiming
    /// PostgreSQL over a rusqlite connection. It did no damage only because no
    /// path those fixtures take reads the dialect off the route; the 34
    /// `route.dialect()` reads in `crud/mod.rs` are one fixture away.
    ///
    /// **There is no PostgreSQL arm here and that is not an omission**: a
    /// `BackendHandle::Postgres` needs a live server, which no unit in this
    /// module opens. `crates/zeroship-plugin-db/tests/unmask_tx_lane.rs` is the Postgres-side harness, and
    /// it now names its dialect at the two `CapturedRoute` constructors rather
    /// than inheriting one. What stands in for that arm below is a control that
    /// needs no server: the same SQLite handle, bound to a route captured with
    /// `Postgres` explicitly, must still answer `Postgres` - so the SQLite
    /// answer above came from the derivation in [`ambient_route_for_tests`] and
    /// not from `bind` quietly inspecting the handle.
    #[test]
    fn an_ambient_test_route_speaks_the_dialect_of_the_backend_it_was_handed() {
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let sqlite = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    crate::encryption::LocalKeySource::env_var(),
                )
                .expect("open sqlite backend"),
            );
            let handle = BackendHandle::Sqlite(Rc::clone(&sqlite));

            let derived = ambient_route_for_tests("app_route_dialect", handle.clone());
            assert_eq!(
                derived.dialect(),
                crate::query::SqlDialect::Sqlite,
                "a route bound to a SQLite handle must not claim PostgreSQL: \
                 every builder it reaches would emit the wrong SQL",
            );

            let stated = crate::tx_route::CapturedRoute::pool_for_tests(
                "app_route_dialect",
                crate::query::SqlDialect::Postgres,
            )
            .bind(handle);
            assert_eq!(
                stated.dialect(),
                crate::query::SqlDialect::Postgres,
                "the dialect is the constructor's input; `bind` must not \
                 re-derive it from the handle",
            );
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
        reset_world("app_suppressed");
        // Register a subscriber so the only thing keeping us out of
        // the build is the suppression flag.
        let sub = crate::broker::subscribe("app_suppressed", "messages");
        crate::broker::suppress_app("app_suppressed");

        let rows = vec![synthetic_row()];
        emit_for_rows(
            &rows,
            "app_suppressed",
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
            "app_no_subs",
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
            !crate::broker::has_subscribers("app_no_subs", "ghosts"),
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
    // crates/zeroship-plugin-db/tests/integration.rs).

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
                !l.has_tx_for("app_active_queue_or_emit_no_tx_emits_immediately"),
                "precondition: no tx"
            )
        });

        let sub = crate::broker::subscribe(
            "app_active_queue_or_emit_no_tx_emits_immediately",
            "messages",
        );

        let mut tuple = HashMap::new();
        tuple.insert("id".to_string(), "9".to_string());
        queue_or_emit(
            "app_active_queue_or_emit_no_tx_emits_immediately",
            false,
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
        let sub = crate::broker::subscribe(
            "app_active_drain_pending_emits_on_commit_fires_every_queued_event",
            "messages",
        );

        let mk_event = |pk: i64| zeroship_core::change_event::ChangeEvent {
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
        crate::tx_lanes::with_mut(|l| {
            l.push_pending_emit(mk_event(1));
            l.push_pending_emit(mk_event(2));
            l.push_pending_emit(mk_event(3));
        });
        // Sanity: nothing has been delivered before drain.
        assert!(sub.pop().is_none(), "drain must not have happened yet");

        drain_pending_emits_on_commit(
            "app_active_drain_pending_emits_on_commit_fires_every_queued_event",
        );

        let mut pks = Vec::new();
        while let Some(msg) = sub.pop() {
            if let crate::broker::SubscriptionMessage::Change(ev) = msg {
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
        drain_pending_emits_on_commit(
            "app_active_drain_pending_emits_on_commit_fires_every_queued_event",
        );
        assert!(sub.pop().is_none(), "second drain must be a no-op");
        reset_world("app_active_drain_pending_emits_on_commit_fires_every_queued_event");
    }

    /// `clear_pending_emits` must drop the queue WITHOUT publishing
    /// anything — the ROLLBACK path relies on this so subscribers
    /// never observe aborted mutations.
    #[test]
    fn clear_pending_emits_drops_without_firing() {
        reset_world("app_active_clear_pending_emits_drops_without_firing");
        let sub = crate::broker::subscribe(
            "app_active_clear_pending_emits_drops_without_firing",
            "messages",
        );

        let ev = zeroship_core::change_event::ChangeEvent {
            app_id: "app_active_clear_pending_emits_drops_without_firing".to_string(),
            collection: "messages".to_string(),
            op: ChangeOp::Insert,
            pk: Some("99".to_string()),
            changed_columns: vec!["id".to_string()],
            new_tuple: HashMap::new(),
            old_tuple: None,
        };
        crate::tx_lanes::with_mut(|l| l.push_pending_emit(ev));

        clear_pending_emits("app_active_clear_pending_emits_drops_without_firing");

        assert!(
            sub.pop().is_none(),
            "ROLLBACK path must drop queued events silently",
        );
        // After clear, drain must also be a no-op (queue is empty).
        drain_pending_emits_on_commit("app_active_clear_pending_emits_drops_without_firing");
        assert!(sub.pop().is_none(), "post-clear drain must publish nothing");
        reset_world("app_active_clear_pending_emits_drops_without_firing");
    }

    #[test]
    fn exec_mutation_with_emit_builds_when_active_subscriber() {
        reset_world("app_active_exec_mutation_with_emit_builds_when_active_subscriber");
        let sub = crate::broker::subscribe(
            "app_active_exec_mutation_with_emit_builds_when_active_subscriber",
            "messages",
        );

        let rows = vec![synthetic_row()];
        emit_for_rows(
            &rows,
            "app_active_exec_mutation_with_emit_builds_when_active_subscriber",
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
            Some(crate::broker::SubscriptionMessage::Change(ev)) => {
                assert_eq!(
                    ev.app_id,
                    "app_active_exec_mutation_with_emit_builds_when_active_subscriber"
                );
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
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()), crate::encryption::LocalKeySource::env_var())
                    .expect("open sqlite backend"),
            );
            let handle = BackendHandle::Sqlite(Rc::clone(&backend));
            let sub = crate::broker::subscribe(
                "app_active_exec_mutation_with_emit_skips_local_emit_when_sqlite_cdc_publishes",
                "messages",
            );

            let rows = vec![synthetic_row()];
            emit_for_rows(
                &rows,
                "app_active_exec_mutation_with_emit_skips_local_emit_when_sqlite_cdc_publishes",
                /* in_tx */ false,
                // The SQLite arm: its commit hook publishes, so the local emit
                // must short-circuit. This is the one site that passes `true`.
                /* backend_publishes */ true,
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
        let sub = crate::broker::subscribe(
            "app_active_exec_mutation_with_emit_uses_logical_typed_id_for_pk",
            "messages",
        );

        let rows = vec![synthetic_typed_id_row()];
        emit_for_rows(
            &rows,
            "app_active_exec_mutation_with_emit_uses_logical_typed_id_for_pk",
            /* in_tx */ false,
            /* backend_publishes */ false,
            "messages",
            ChangeOp::Insert,
        );

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
        reset_world("app_active_exec_mutation_with_emit_uses_logical_typed_id_for_pk");
    }

    #[test]
    fn sqlite_exec_helpers_use_tx_connection_when_present() {
        reset_world("app_exec");
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()), crate::encryption::LocalKeySource::env_var())
                    .expect("open sqlite backend"),
            );
            backend
                .attach_app_file("app_exec")
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

            // The handle is held HERE rather than parked in the adapter's
            // per-isolate context. These tests only ever used that context as a
            // carrier for a backend they had just opened themselves, and the
            // engine cannot name it any more.
            let handle = BackendHandle::Sqlite(Rc::clone(&backend));

            let client = backend
                .acquire_dedicated_client("app_exec")
                .await
                .expect("acquire tx client");
            backend
                .client_exec(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            crate::tx_lanes::with_mut(|l| {
                let prev = l.install_tx_client("app_exec", TxConnection::Sqlite(client));
                assert!(prev.is_none(), "tx slot should start empty");
            });

            reset_sqlite_route();
            let inserted = exec_mutation(
                &ambient_route_for_tests("app_exec", handle.clone()),
                BuiltQuery {
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
                "exec_mutation must route through TxConnection::Sqlite",
            );
            assert_eq!(
                inserted[0].get("title").and_then(Value::as_str),
                Some("tx-row"),
            );

            reset_sqlite_route();
            let count = exec_count(
                &ambient_route_for_tests("app_exec", handle.clone()),
                BuiltQuery {
                    sql: r#"SELECT COUNT(*) AS count FROM "app_exec"."notes""#.to_string(),
                    params: vec![],
                },
            )
            .await
            .expect("exec_count through tx");
            assert_eq!(
                sqlite_route(),
                2,
                "exec_count must route through TxConnection::Sqlite",
            );
            assert_eq!(count, 1);

            reset_sqlite_route();
            let rows = exec_query(
                &ambient_route_for_tests("app_exec", handle.clone()),
                BuiltQuery {
                    sql: r#"SELECT title FROM "app_exec"."notes" WHERE id = 1"#.to_string(),
                    params: vec![],
                },
            )
            .await
            .expect("exec_query through tx");
            assert_eq!(
                sqlite_route(),
                2,
                "exec_query must route through TxConnection::Sqlite",
            );
            assert_eq!(rows[0].get("title").and_then(Value::as_str), Some("tx-row"));

            if let Some(TxConnection::Sqlite(client)) =
                crate::tx_lanes::with_mut(|l| l.take_tx_client_for("app_exec"))
            {
                let _ = client.exec("ROLLBACK", &[]).await;
            } else {
                panic!("sqlite tx client should still be parked for cleanup");
            }
        });
        reset_world("app_exec");
    }

    // -------------------------------------------------------------------
    // Metering-as-infrastructure — the exec boundary emits a
    // raw usage metric in the SUCCESS arm, scoped to app_id, and emits
    // NOTHING on a failed op. Faithful: drives the REAL `exec_query` /
    // `exec_mutation` / `exec_count` path against a live SqliteBackend with
    // a `Meter` stamped into the per-isolate context (the same slot
    // `DbPlugin::register` populates in production).
    // -------------------------------------------------------------------

    #[test]
    fn metering_db_exec_emits_reads_writes_rows_and_skips_failures() {
        use std::sync::Arc;
        reset_world("app_metering_db_exec_emits_reads_writes_rows_and_skips_failures");
        run(async {
            let app_id = "00000000-0000-7000-8000-0000000000e5";
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()), crate::encryption::LocalKeySource::env_var())
                    .expect("open sqlite backend"),
            );
            backend
                .attach_app_file(app_id)
                .await
                .expect("ensure app schema");
            backend
                .pool_exec(
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

            // Stamp a real Meter into this thread's metric slot - exactly what
            // `DbPlugin::register` does in production. The slot is
            // `crate::metrics`' own thread-local, not the adapter's context;
            // the two used to be written in one closure, which is the only
            // reason this line ever looked like an adapter read.
            let meter = Arc::new(zeroship_metering::Meter::new());
            let handle = BackendHandle::Sqlite(Rc::clone(&backend));
            crate::metrics::stamp(Some(Arc::clone(&meter)));

            // 1 mutation returning 1 row → db_writes +1, db_rows_written +1.
            exec_mutation(
                &ambient_route_for_tests(app_id, handle.clone()),
                BuiltQuery {
                    sql: format!(
                        r#"INSERT INTO "{app_id}"."notes" (id, title) VALUES (1, 'a') RETURNING *"#
                    ),
                    params: vec![],
                },
            )
            .await
            .expect("insert");

            // 1 query (read) → db_reads +1.
            exec_query(
                &ambient_route_for_tests(app_id, handle.clone()),
                BuiltQuery {
                    sql: format!(r#"SELECT title FROM "{app_id}"."notes" WHERE id = 1"#),
                    params: vec![],
                },
            )
            .await
            .expect("select");

            // 1 count (read) → db_reads +1.
            exec_count(
                &ambient_route_for_tests(app_id, handle.clone()),
                BuiltQuery {
                    sql: format!(r#"SELECT COUNT(*) AS count FROM "{app_id}"."notes""#),
                    params: vec![],
                },
            )
            .await
            .expect("count");

            // A FAILED op (bad SQL) must emit NOTHING.
            let bad = exec_query(
                &ambient_route_for_tests(app_id, handle.clone()),
                BuiltQuery {
                    sql: format!(r#"SELECT nope FROM "{app_id}"."no_such_table""#),
                    params: vec![],
                },
            )
            .await;
            assert!(bad.is_err(), "the bad query must fail");

            let events = meter.drain();
            let id = uuid::Uuid::parse_str(app_id).unwrap();
            assert_eq!(
                usage_value(&events, id, "db_writes"),
                Some(1),
                "one mutation = 1 db_writes; got {events:?}"
            );
            assert_eq!(
                usage_value(&events, id, "db_rows_written"),
                Some(1),
                "the insert returned 1 row; got {events:?}"
            );
            assert_eq!(
                usage_value(&events, id, "db_reads"),
                Some(2),
                "one query + one count = 2 db_reads (the FAILED query did NOT bill); got {events:?}"
            );

            crate::metrics::stamp(None);
        });
        reset_world("app_metering_db_exec_emits_reads_writes_rows_and_skips_failures");
    }

    fn usage_value(
        events: &[zeroship_core::usage_event::UsageEvent],
        app_id: uuid::Uuid,
        meter: &str,
    ) -> Option<u64> {
        events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .map(|event| event.value)
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
        reset_world("app_a");
        reset_world("app_b");
        run(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()), crate::encryption::LocalKeySource::env_var())
                    .expect("open sqlite backend"),
            );
            // The handle is held HERE rather than parked in the adapter's
            // per-isolate context. These tests only ever used that context as a
            // carrier for a backend they had just opened themselves, and the
            // engine cannot name it any more.
            let handle = BackendHandle::Sqlite(Rc::clone(&backend));

            // app_a opens an explicit transaction; its dedicated client
            // is parked in the per-isolate slot — exactly the state a
            // creator callback leaves behind across an `await`.
            let client = backend
                .acquire_dedicated_client("app_a")
                .await
                .expect("acquire tx client");
            backend
                .client_exec(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            crate::tx_lanes::with_mut(|l| {
                let prev = l.install_tx_client("app_a", TxConnection::Sqlite(client));
                assert!(prev.is_none(), "tx slot must start empty");
            });

            // Co-resident app_b now runs a plain (non-transactional)
            // query on the same thread.
            reset_sqlite_route();
            let rows = exec_query(
                &ambient_route_for_tests("app_b", handle.clone()),
                BuiltQuery {
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
                crate::tx_lanes::with(|l| l.has_tx_for("app_a")),
                "app_a's parked tx must survive app_b's access",
            );

            // Cleanup: roll app_a's tx back and drop the client.
            if let Some(TxConnection::Sqlite(client)) =
                crate::tx_lanes::with_mut(|l| l.take_tx_client_for("app_a"))
            {
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
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()), crate::encryption::LocalKeySource::env_var())
                    .expect("open sqlite backend"),
            );
            backend
                .attach_app_file("app_exec_cancel")
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

            // The handle is held HERE rather than parked in the adapter's
            // per-isolate context. These tests only ever used that context as a
            // carrier for a backend they had just opened themselves, and the
            // engine cannot name it any more.
            let handle = BackendHandle::Sqlite(Rc::clone(&backend));

            let client = backend
                .acquire_dedicated_client("app_exec_cancel")
                .await
                .expect("acquire tx client");
            backend
                .client_exec(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            crate::tx_lanes::with_mut(|l| {
                let prev = l.install_tx_client("app_exec_cancel", TxConnection::Sqlite(client));
                assert!(prev.is_none(), "tx slot should start empty");
            });

            let gate = backend.arm_next_command_gate_for_tests();
            let spawned_handle = handle.clone();
            let task = compio::runtime::spawn(async move {
                exec_query(
                    &ambient_route_for_tests("app_exec_cancel", spawned_handle),
                    BuiltQuery {
                        sql: r#"SELECT title FROM "app_exec_cancel"."notes" WHERE id = 1"#
                            .to_string(),
                        params: vec![],
                    },
                )
                .await
            });
            gate.wait_until_blocked()
                .await
                .expect("worker must block on test gate");
            drop(task);
            gate.release();
            compio::time::sleep(Duration::from_millis(20)).await;

            assert!(
                crate::tx_lanes::with(|l| l.has_tx_for("app_exec_cancel")),
                "dropping the in-flight future must restore the tx slot"
            );

            let rows = exec_query(
                &ambient_route_for_tests("app_exec_cancel", handle.clone()),
                BuiltQuery {
                    sql: r#"SELECT title FROM "app_exec_cancel"."notes" WHERE id = 1"#.to_string(),
                    params: vec![],
                },
            )
            .await
            .expect("subsequent query must reuse restored tx slot");
            assert_eq!(
                rows[0].get("title").and_then(Value::as_str),
                Some("persisted")
            );

            if let Some(TxConnection::Sqlite(client)) =
                crate::tx_lanes::with_mut(|l| l.take_tx_client_for("app_exec_cancel"))
            {
                let _ = client.exec("ROLLBACK", &[]).await;
            } else {
                panic!("sqlite tx client should still be parked for cleanup");
            }
        });
        reset_world("app_exec_cancel");
    }

    /// A gate armed on one session must leave every other session running.
    ///
    /// The slot used to be a process-global one-shot, taken in **every**
    /// actor's run loop by whichever actor happened to receive the next
    /// command. So arming it here stalled an unrelated, concurrently running
    /// test's actor on `release_rx.recv()`, and satisfied the arming test's
    /// `wait_until_blocked()` with that foreign command - leaving the arming
    /// test to assert against a query that was never gated. That is a race
    /// between tests, so it only bit when the timing lined up.
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
            let backend_a =
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir_a.path()), crate::encryption::LocalKeySource::env_var())
                    .expect("open backend a");
            let backend_b =
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir_b.path()), crate::encryption::LocalKeySource::env_var())
                    .expect("open backend b");

            let _gate = backend_a.arm_next_command_gate_for_tests();

            let ran = compio::time::timeout(
                Duration::from_secs(5),
                backend_b.pool_exec("CREATE TABLE probe (id INTEGER PRIMARY KEY)", &[]),
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
    // PG-REQUIRED. Connects to the database the overlay names (or
    // `PG_TEST_URL`) and PANICS, naming the provisioning command, when there
    // is none. It used to fall back to `postgres://postgres:test@localhost:
    // 5434/postgres` - a different server with different credentials - so a
    // run with no overlay measured whatever was listening there.
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

    fn pg_test_url() -> String {
        zeroship_core::config::test_database_url()
    }

    #[test]
    fn autocommit_cancelled_query_does_not_leak_role_or_timeout_to_pool() {
        use compio_postgres::{NoTls, Pool};
        use std::time::Duration;

        reset_world("app_autocommit_cancelled_query_does_not_leak_role_or_timeout_to_pool");
        run(async {
            let url = pg_test_url();
            match compio_postgres::connect(&url, NoTls).await {
                Ok((client, connection)) => {
                    compio::runtime::spawn(async move {
                        let _ = connection.run().await;
                    })
                    .detach();
                    drop(client);
                }
                Err(e) => {
                    // A Postgres this test cannot dial is a FAILURE, not a
                    // skip: the role/timeout leak this test guards against
                    // only reproduces against a real backend, and a return
                    // here used to let that leak go unchecked while the
                    // suite still reported green.
                    panic!("autocommit-leak test could not connect to {url}: {e}");
                }
            }

            // Pool of ONE so the cancelled-then-reused checkout lands on
            // the same backend whose session state we want to inspect.
            let pool = Rc::new(Pool::connect(&url, 1).await.expect("pool connect"));

            let app_id = "p2c1leak";
            let role = zeroship_core::database_role::per_app_role_name(app_id)
                .expect("test app role name");
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
                crate::backend::pg_autocommit::roled_rows(
                    &pool,
                    &zeroship_schema::SchemaName::new(app_id).expect("fixture schema"),
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
        reset_world("app_autocommit_cancelled_query_does_not_leak_role_or_timeout_to_pool");
    }
}
