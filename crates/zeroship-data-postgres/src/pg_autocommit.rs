//! PostgreSQL pooled ("autocommit") execution under the per-app role fence.
//!
//! **PG TIER.** Every function here is PostgreSQL dialect and vendor types:
//! `SET LOCAL ROLE`, an explicit `BEGIN`/`COMMIT` around a single statement,
//! and `compio_postgres` rows. The policy these encode - which timeouts, which
//! role - lives one tier down in [`zeroship_data_core::budgets`], and the SQL that renders
//! it is [`crate::pg_session_sql`].
//!
//! # Why this module exists
//!
//! The funnel below lived in `crate::exec` (ENGINE tier) until 2026-09-01, and
//! `backend/postgres.rs` reached UP into it from two search methods. That was
//! the whole of the `PG <-> ENGINE` tier cycle: a cycle is not a bad edge, it
//! is a pair of crates that cannot be separated at all, because cargo cannot
//! express mutual dependency. Sinking the funnel into the tier that owns its
//! dialect removes the upward half; what remains is `ENGINE -> PG`, which is
//! rank 3 -> 2 and legal.
//!
//! # Why there is no single "row" return
//!
//! The five production callers want three different things, and one neutral row
//! type would have to lie to at least one of them:
//!
//! - the two search methods want JSON, and get it via
//!   [`crate::pg_row_json::rows_to_values`];
//! - the two unmask readers want ONE CELL, and one of them wants it as RAW
//!   BYTES. Routing that through JSON is not an option:
//!   `pg_row_json::column_to_value` base64-encodes BYTEA, so an encrypted
//!   ciphertext would arrive as text and have to be decoded back before it
//!   could be decrypted - re-introducing the text round-trip that made every
//!   PostgreSQL unmask of an encrypted column fail until 2026-09-01;
//! - the audit INSERT wants nothing at all.
//!
//! So each shape gets its own entry point. `roled_rows` remains for
//! `crate::exec::run_sql`, whose callers are the whole CRUD surface and whose
//! row vocabulary is a separate, open question.

use std::rc::Rc;

use zeroship_data_query_builder::value::Value;

use crate::pg_error;
use crate::pg_session_sql::autocommit_local_session_setup_sql;
use zeroship_data_core::error::DbError;
use zeroship_data_query_builder::SchemaName;

// `ScalarRead` is a shared return type - SQLite returns it too, now that the
// unmask reads dispatch through `BackendHandle` - so it is data-core's, not the
// PostgreSQL module's. That is the shape #119 was about. Re-exported here
// because this module's own signatures return it.
pub use zeroship_data_core::capability::ScalarRead;

/// Run `sql` on a pooled connection narrowed to `schema`'s runtime role,
/// returning the
/// raw driver rows.
///
/// This is the primitive the rest of the module is built on, and the one
/// `crate::exec::run_sql` still needs because its own callers have not settled
/// on a row vocabulary. Prefer one of the shaped wrappers below.
///
/// # Errors
///
/// Returns a typed database error if the pool checkout, the `BEGIN`, the
/// per-app session setup, the statement itself, or the `COMMIT` fails.
pub async fn roled_rows(
    pool: &Rc<compio_postgres::Pool>,
    schema: &SchemaName,
    sql: &str,
    params: &[Value],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    let mut client = pool.acquire().await.map_err(|e| pg_error::classify(&e))?;

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
        let mut err = pg_error::classify(&e);
        zeroship_data_core::error::prefix_message(
            &mut err,
            "db: autocommit BEGIN (per-app §17.5 + DB-1 guards): ",
        );
        err
    })?;

    let setup_sql = autocommit_local_session_setup_sql(schema)?;
    tx.simple_query(&setup_sql).await.map_err(|e| {
        let mut classified = pg_error::classify_pg_per_app_session_setup(&e, schema);
        zeroship_data_core::error::prefix_message(
            classified.error_mut(),
            "db: per-app session setup: ",
        );
        classified.into_db_error()
    })?;

    let bindings: Vec<_> = params.iter().map(crate::params::Parameter).collect();
    let refs: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
        bindings.iter().map(|value| value as _).collect();
    let rows = tx
        .query(sql, &refs)
        .await
        .map_err(|e| pg_error::classify(&e))?;

    // COMMIT reverts the SET LOCAL state and releases the connection
    // clean. On any early return above, `tx` is dropped instead, which
    // rolls back (also reverting the SET LOCAL state) and marks the
    // connection dirty so the pool drains it before the next checkout.
    tx.commit().await.map_err(|e| {
        let mut err = pg_error::classify(&e);
        zeroship_data_core::error::prefix_message(
            &mut err,
            "db: autocommit COMMIT (per-app §17.5 + DB-1 guards): ",
        );
        err
    })?;

    Ok(rows)
}

/// Run `sql` under the per-app role and render the rows as JSON objects.
///
/// # Errors
///
/// Propagates any error from [`roled_rows`].
pub(crate) async fn roled_json(
    pool: &Rc<compio_postgres::Pool>,
    schema: &SchemaName,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Value>, DbError> {
    let rows = roled_rows(pool, schema, sql, params).await?;
    Ok(crate::pg_row_json::rows_to_values(&rows))
}

/// Read column 0 of the first row as raw bytes, under the per-app role.
///
/// The bytes are returned uninterpreted. This is the entry point for reading an
/// encrypted column's BYTEA sibling: the ciphertext must reach the decryptor
/// exactly as stored, and `query_text_params` binds every result in BINARY
/// format (`libs/compio-postgres/src/query.rs:186`), so no rendering happens.
///
/// # Errors
///
/// Propagates any error from [`roled_rows`], or reports a decode failure if
/// column 0 is not a byte-typed column.
pub(crate) async fn roled_scalar_bytes(
    pool: &Rc<compio_postgres::Pool>,
    schema: &SchemaName,
    sql: &str,
    params: &[Value],
) -> Result<ScalarRead<Vec<u8>>, DbError> {
    scalar_bytes(&roled_rows(pool, schema, sql, params).await?)
}

/// Decode column 0 of the first row as raw bytes.
///
/// Split out of [`roled_scalar_bytes`] because the AUTOCOMMIT lane is not the
/// only lane a scalar read can run on. An unmask issued inside
/// `db.transaction(fn)` has to read the ciphertext on the app's parked
/// transaction connection - a pooled checkout cannot see a row that
/// transaction has not committed - and that lane's rows come back from
/// `Client::query_text_params` rather than from [`roled_rows`]. The decode is
/// the same either way and must stay the same, so it lives here once instead of
/// being written a second time at the routed call site.
///
/// # Errors
///
/// Reports a decode failure if column 0 is not a byte-typed column.
pub fn scalar_bytes(rows: &[compio_postgres::Row]) -> Result<ScalarRead<Vec<u8>>, DbError> {
    let Some(row) = rows.first() else {
        return Ok(ScalarRead::NoRow);
    };
    let value: Option<&[u8]> = row
        .try_get::<_, Option<&[u8]>>(0)
        .map_err(|e| DbError::internal(format!("db: read scalar bytes: {e}")))?;
    Ok(match value {
        Some(bytes) => ScalarRead::Value(bytes.to_vec()),
        None => ScalarRead::Null,
    })
}

/// Read column 0 of the first row as text, under the per-app role.
///
/// # Errors
///
/// Propagates any error from [`roled_rows`], or reports a decode failure if
/// column 0 is not a text-typed column. `&str: FromSql::accepts` covers
/// VARCHAR/TEXT/BPCHAR/NAME/UNKNOWN plus citext and ltree and nothing else, and
/// `Row::get_inner` consults it BEFORE decoding, even for NULL - so pointing
/// this at a BYTEA column is refused outright rather than mis-parsed. Use
/// [`roled_scalar_bytes`] there.
pub(crate) async fn roled_scalar_text(
    pool: &Rc<compio_postgres::Pool>,
    schema: &SchemaName,
    sql: &str,
    params: &[Value],
) -> Result<ScalarRead<String>, DbError> {
    scalar_text(&roled_rows(pool, schema, sql, params).await?)
}

/// Decode column 0 of the first row as text.
///
/// The text twin of [`scalar_bytes`], split out for the same reason and used by
/// the same routed reader.
///
/// # Errors
///
/// Reports a decode failure if column 0 is not a text-typed column.
pub fn scalar_text(rows: &[compio_postgres::Row]) -> Result<ScalarRead<String>, DbError> {
    let Some(row) = rows.first() else {
        return Ok(ScalarRead::NoRow);
    };
    let value: Option<&str> = row
        .try_get::<_, Option<&str>>(0)
        .map_err(|e| DbError::internal(format!("db: read scalar text: {e}")))?;
    Ok(match value {
        Some(text) => ScalarRead::Value(text.to_string()),
        None => ScalarRead::Null,
    })
}

/// Run a statement under the per-app role and discard any result rows.
///
/// # Errors
///
/// Propagates any error from [`roled_rows`].
pub(crate) async fn roled_statement(
    pool: &Rc<compio_postgres::Pool>,
    schema: &SchemaName,
    sql: &str,
    params: &[Value],
) -> Result<(), DbError> {
    roled_rows(pool, schema, sql, params).await?;
    Ok(())
}
