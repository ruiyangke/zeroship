//! PostgreSQL pooled execution under the backend's authority and timeout guards.

use std::rc::Rc;

use crate::value::Value;

use crate::backend::postgres::pg_error;
use crate::backend::postgres::pg_session_sql::autocommit_local_session_setup_sql;
use crate::connection::SessionAuthority;
use crate::binding::DbBinding;
use zeroship_data_orm::error::DbError;

/// Read driver rows under the binding's authority.
pub(crate) async fn scoped_rows(
    pool: &Rc<compio_postgres::Pool>,
    binding: &DbBinding,
    authority: SessionAuthority,
    sql: &str,
    params: &[Value],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    with_scoped_transaction(pool, binding, authority, None, async |tx| {
        let bindings: Vec<_> = params.iter().map(super::params::Parameter).collect();
        let refs: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            bindings.iter().map(|value| value as _).collect();
        tx.query(sql, &refs)
            .await
            .map_err(|e| pg_error::classify(&e))
    })
    .await
}

/// Read a masked field's real value on a short transaction of this pool's own.
///
/// **There is no restore half here, and its absence is the point.** The setup
/// batch narrows to the binding and this appends the assumption of the unmask
/// role to it, so the elevated statement is the only one the transaction runs;
/// `SET LOCAL` is discarded at the commit or rollback that ends it, and the
/// lease is returned afterwards. Narrowing back would have nothing left to
/// protect. The creator's OWN transaction is the case that does, and it is
/// bracketed where it runs
/// (`crate::backend::postgres::executor::read_unmasked_on_session`).
pub(crate) async fn scoped_elevated_rows(
    pool: &Rc<compio_postgres::Pool>,
    binding: &DbBinding,
    authority: SessionAuthority,
    assume_role: Option<&str>,
    sql: &str,
    params: &[Value],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    with_scoped_transaction(pool, binding, authority, assume_role, async |tx| {
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
pub(crate) async fn scoped_execute(
    pool: &Rc<compio_postgres::Pool>,
    binding: &DbBinding,
    authority: SessionAuthority,
    sql: &str,
    params: &[Value],
) -> Result<u64, DbError> {
    with_scoped_transaction(pool, binding, authority, None, async |tx| {
        let bindings: Vec<_> = params.iter().map(super::params::Parameter).collect();
        let refs: Vec<&(dyn compio_postgres::types::ToSql + Sync)> =
            bindings.iter().map(|value| value as _).collect();
        tx.execute(sql, &refs)
            .await
            .map_err(|e| pg_error::classify(&e))
    })
    .await
}

/// Run one operation on a short transaction of this pool's own.
///
/// `assume_role` is appended to the setup batch, after the narrowing and the
/// budgets, for the one caller whose statement needs a privilege the binding
/// role does not carry. It rides the SAME batch so its failure is classified as
/// a session-setup failure rather than as the statement's, and so PostgreSQL's
/// abort-at-first-failure leaves the operation unreached.
async fn with_scoped_transaction<T>(
    pool: &Rc<compio_postgres::Pool>,
    binding: &DbBinding,
    authority: SessionAuthority,
    assume_role: Option<&str>,
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
        zeroship_data_orm::error::prefix_message(&mut err, "db: autocommit BEGIN: ");
        err
    })?;

    let mut setup_sql = autocommit_local_session_setup_sql(binding, authority)?;
    if let Some(assume_role) = assume_role {
        setup_sql.push_str("; ");
        setup_sql.push_str(assume_role);
    }
    tx.simple_query(&setup_sql).await.map_err(|e| {
        let mut classified = match authority {
            SessionAuthority::PerBindingRole => {
                pg_error::classify_pg_binding_session_setup(&e)
            }
            SessionAuthority::Connection => {
                zeroship_data_orm::error::SessionSetupError::failed(pg_error::classify(&e))
            }
        };
        zeroship_data_orm::error::prefix_message(classified.error_mut(), "db: session setup: ");
        classified.into_db_error()
    })?;

    let result = operation(&tx).await?;

    tx.commit().await.map_err(|e| {
        let mut err = pg_error::classify(&e);
        zeroship_data_orm::error::prefix_message(&mut err, "db: autocommit COMMIT: ");
        err
    })?;

    Ok(result)
}

/// Run `sql` under the backend's configured authority and decode native records.
///
/// # Errors
///
/// Propagates any error from [`scoped_rows`].
pub(crate) async fn scoped_json(
    pool: &Rc<compio_postgres::Pool>,
    binding: &DbBinding,
    authority: SessionAuthority,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Value>, DbError> {
    let rows = scoped_rows(pool, binding, authority, sql, params).await?;
    crate::backend::postgres::pg_row_json::rows_to_values(&rows)
}
