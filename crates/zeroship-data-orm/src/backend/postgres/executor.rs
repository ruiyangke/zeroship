//! Host database routing and transaction authority setup.
use super::PostgresBackend;
use crate::binding::DbBinding;
use crate::value::Value;
use crate::{
    driver::{Driver, LeaseKind, Session},
    error::*,
    executor::ScopedExecutor,
};
use async_trait::async_trait;

#[async_trait(?Send)]
impl ScopedExecutor for PostgresBackend {
    fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        self.connection_driver().pool_counts()
    }
    async fn prepare_for_app(&self, _binding: &DbBinding) -> Result<(), DbError> {
        Ok(())
    }
    async fn query(
        &self,
        binding: &DbBinding,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError> {
        self.query_scoped_values(binding, sql, params).await
    }
    async fn exec(
        &self,
        binding: &DbBinding,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        super::pg_autocommit::scoped_execute(
            self.pool(),
            binding,
            self.session_authority(),
            sql,
            params,
        )
        .await
    }
    async fn read_unmasked(
        &self,
        binding: &DbBinding,
        session: Option<&Session>,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError> {
        let elevation =
            super::pg_session_sql::unmask_elevation_sql(binding, self.session_authority())?;
        match session {
            Some(session) => {
                let client = session
                    .get::<compio_postgres::PoolConnection>()
                    .ok_or_else(|| {
                        DbError::internal("the audited raw read received a non-PostgreSQL session")
                    })?;
                read_unmasked_on_session(client, elevation.as_ref(), sql, params).await
            }
            None => {
                let rows = super::pg_autocommit::scoped_elevated_rows(
                    self.pool(),
                    binding,
                    self.session_authority(),
                    elevation.as_ref().map(|e| e.assume.as_str()),
                    sql,
                    params,
                )
                .await?;
                super::pg_row_json::rows_to_values(&rows)
            }
        }
    }
    async fn check_connection(&self) -> Result<(), DbError> {
        // A pooled lease and a protocol Sync: no table, no authority setup and
        // no transaction lane, so an open transaction on this app's lane does
        // not delay the answer. The wait is the pool's acquire timeout.
        let client = self
            .pool()
            .acquire()
            .await
            .map_err(|error| super::pg_error::classify(&error))?;
        client
            .check_connection()
            .await
            .map_err(|error| super::pg_error::classify(&error))
    }
    async fn open_tx_session(
        &self,
        binding: &DbBinding,
        begin: BeginIntent,
    ) -> Result<Session, OpenSessionError> {
        let session = self
            .connection_driver()
            .acquire(LeaseKind::Transaction)
            .await?;
        let client = session
            .get::<compio_postgres::PoolConnection>()
            .expect("PostgresDriver returns a PostgreSQL lease");
        client
            .batch_execute(&super::render_begin(begin))
            .await
            .map_err(|e| super::pg_error::classify(&e))?;
        super::apply_session_authority(client, binding, self.session_authority()).await?;
        Ok(session)
    }
}
impl PostgresBackend {
    pub fn connection_driver(&self) -> super::driver::PostgresDriver {
        super::driver::PostgresDriver::new(self.pool().clone())
    }
}

/// Read the real value on the CREATOR's open transaction, bracketed by the
/// elevation.
///
/// **The restore is unconditional, and that is what this function exists for.**
/// The session is the creator's own and is handed straight back to the lane, so
/// every statement the creator issues for the rest of that transaction runs
/// under whatever role this one left behind.
///
/// **The decode is inside the bracket, not after it.** A statement the SERVER
/// refuses aborts the transaction, and the creator's next statement is refused
/// `25P02` whatever role it would have run as - so that failure cannot leak the
/// elevation. The one that can is a read the server ANSWERED and the row
/// decoder then refused: the transaction is still live and the creator carries
/// on. `driver::tests::invalid_native_results_are_errors_and_the_session_remains_usable`
/// exhibits exactly that pair, and it is the reason the bracket spans producing
/// the values rather than only the round trip.
///
/// Which error is reported when both halves fail is decided once here: the
/// READ's, because a restore refused inside an already-aborted transaction is
/// that failure's consequence and naming it would hide the cause. A restore
/// that fails after a SUCCESSFUL read is the opposite case and must be raised,
/// because the rows are worthless beside a session left elevated.
async fn read_unmasked_on_session(
    client: &compio_postgres::PoolConnection,
    elevation: Option<&super::pg_session_sql::UnmaskElevation>,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Value>, DbError> {
    let decoded = async |client| {
        let rows = super::params::query(client, sql, params).await?;
        super::pg_row_json::rows_to_values(&rows)
    };
    let Some(elevation) = elevation else {
        return decoded(client).await;
    };
    client
        .simple_query(&elevation.assume)
        .await
        .map_err(|e| super::pg_error::classify(&e))?;
    let read = decoded(client).await;
    let restored = client
        .simple_query(&elevation.restore)
        .await
        .map(|_| ())
        .map_err(|e| super::pg_error::classify(&e));
    match (read, restored) {
        (Ok(values), Ok(())) => Ok(values),
        (Ok(_), Err(restore)) => Err(restore),
        (Err(read), _) => Err(read),
    }
}
