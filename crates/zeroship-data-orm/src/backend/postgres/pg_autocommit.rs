//! PostgreSQL pooled execution under the per-app role and timeout guards.

use std::rc::Rc;

use crate::value::Value;

use crate::backend::postgres::pg_error;
use crate::backend::postgres::pg_session_sql::autocommit_local_session_setup_sql;
use zeroship_data_orm::error::DbError;
use crate::sql::SchemaName;

pub use zeroship_data_orm::capability::ScalarRead;

/// Read driver rows under the binding's authority.
pub async fn roled_rows(
    pool: &Rc<compio_postgres::Pool>,
    schema: &SchemaName,
    sql: &str,
    params: &[Value],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    with_roled_transaction(pool, schema, async |tx| {
        let bindings: Vec<_> = params.iter().map(super::params::Parameter).collect();
        let refs: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            bindings.iter().map(|value| value as _).collect();
        tx.query(sql, &refs)
            .await
            .map_err(|e| pg_error::classify(&e))
    })
    .await
}

/// Execute without fetching rows under the binding's authority.
pub(crate) async fn roled_execute(
    pool: &Rc<compio_postgres::Pool>,
    schema: &SchemaName,
    sql: &str,
    params: &[Value],
) -> Result<u64, DbError> {
    with_roled_transaction(pool, schema, async |tx| {
        let bindings: Vec<_> = params.iter().map(super::params::Parameter).collect();
        let refs: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            bindings.iter().map(|value| value as _).collect();
        tx.execute(sql, &refs)
            .await
            .map_err(|e| pg_error::classify(&e))
    })
    .await
}

async fn with_roled_transaction<T>(
    pool: &Rc<compio_postgres::Pool>,
    schema: &SchemaName,
    operation: impl for<'a, 'conn> AsyncFnOnce(
        &'a compio_postgres::Transaction<'conn>,
    ) -> Result<T, DbError>,
) -> Result<T, DbError> {
    use crate::driver::{Driver, LeaseKind};
    let mut lease = super::driver::PostgresDriver::new(pool.clone())
        .acquire(LeaseKind::Transaction)
        .await?;
    let client = lease
        .get_mut::<compio_postgres::PoolConnection>()
        .expect("PostgresDriver returns a PostgreSQL lease");

    // SET LOCAL is reverted on commit or the transaction guard's rollback,
    // including when setup or execution is cancelled.
    let tx = client.transaction().await.map_err(|e| {
        let mut err = pg_error::classify(&e);
        zeroship_data_orm::error::prefix_message(
            &mut err,
            "db: autocommit BEGIN (per-app §17.5 + DB-1 guards): ",
        );
        err
    })?;

    let setup_sql = autocommit_local_session_setup_sql(schema)?;
    tx.simple_query(&setup_sql).await.map_err(|e| {
        let mut classified = pg_error::classify_pg_per_app_session_setup(&e, schema);
        zeroship_data_orm::error::prefix_message(
            classified.error_mut(),
            "db: per-app session setup: ",
        );
        classified.into_db_error()
    })?;

    let result = operation(&tx).await?;

    tx.commit().await.map_err(|e| {
        let mut err = pg_error::classify(&e);
        zeroship_data_orm::error::prefix_message(
            &mut err,
            "db: autocommit COMMIT (per-app §17.5 + DB-1 guards): ",
        );
        err
    })?;

    Ok(result)
}

/// Run `sql` under the per-app role and decode native records.
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
    crate::backend::postgres::pg_row_json::rows_to_values(&rows)
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
/// Split out of `roled_scalar_bytes` because the AUTOCOMMIT lane is not the
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
/// `roled_scalar_bytes` there.
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

/// Run a statement under the per-app role without fetching rows.
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
    roled_execute(pool, schema, sql, params).await?;
    Ok(())
}
