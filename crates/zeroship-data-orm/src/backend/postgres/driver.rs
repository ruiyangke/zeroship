//! PostgreSQL implementation of the ORM execution contracts.
use crate::{driver::*, error::*};
use async_trait::async_trait;
use compio_postgres::{CancelToken, Pool, PoolConnection};
use std::rc::Rc;
use crate::value::Value;
use crate::sql::{compile::SqlDialect};

#[derive(Debug)]
struct PgCancellation {
    pool: Pool,
    token: CancelToken,
}
#[async_trait(?Send)]
impl Cancellation for PgCancellation {
    async fn cancel(&self) -> Result<CancelDelivery, DbError> {
        self.pool.cancel_query(&self.token).await.map_err(|error| {
            DbError::internal(format!(
                "db.transaction: could not deliver a cancellation request: {error}"
            ))
        })?;
        Ok(CancelDelivery::Requested)
    }
}

#[async_trait(?Send)]
impl DriverSession for PoolConnection {
    fn server_process_id(&self) -> Option<i32> {
        Some(self.process_id())
    }
    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Value>, DbError> {
        super::params::query(self, sql, params)
            .await
            .and_then(|rows| super::pg_row_json::rows_to_values(&rows))
    }
    async fn exec(&self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
        super::params::execute(self, sql, params).await
    }
    async fn settle(&self, intent: SettleIntent) -> (TerminalResult, Option<DbError>) {
        match self.batch_execute_reporting_tag(intent.verb()).await {
            Ok(tag) => (super::terminal_from_tag(intent, tag.as_deref()), None),
            Err(error) => (
                super::terminal_from_status(self.transaction_status()),
                Some(super::pg_error::classify(&error)),
            ),
        }
    }
    async fn cleanup(&self) -> CleanupAck {
        super::cleanup(self).await
    }
    fn canceller(&self) -> Option<CancellationHandle> {
        Some(CancellationHandle::new(PgCancellation {
            pool: self.pool().clone(),
            token: self.cancel_token(),
        }))
    }
    fn discard(self: Box<Self>) {
        PoolConnection::discard(*self);
    }
}

/// PostgreSQL connection source. It owns no application services.
#[derive(Clone, Debug)]
pub struct PostgresDriver {
    pool: Rc<Pool>,
}
impl PostgresDriver {
    pub fn new(pool: Rc<Pool>) -> Self {
        Self { pool }
    }
}
#[async_trait(?Send)]
impl Driver for PostgresDriver {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }
    fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        Some((
            self.pool.idle_count(),
            self.pool.active_count(),
            self.pool.total_count(),
        ))
    }
    async fn acquire(&self, _kind: LeaseKind) -> Result<Session, DbError> {
        let connection = self
            .pool
            .acquire()
            .await
            .map_err(|e| super::pg_error::classify(&e))?;
        Ok(Session::new(connection))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[compio::test]
    async fn native_exec_reports_command_counts() {
        let postgres = crate::tests::fixtures::postgres::Postgres::start();
        let pool = Pool::connect(&postgres.url(), 2)
            .await
            .unwrap();
        crate::driver::tests::native_commands(PostgresDriver::new(Rc::new(pool))).await;
    }

    #[compio::test]
    async fn invalid_native_results_are_errors_and_the_session_remains_usable() {
        let postgres = crate::tests::fixtures::postgres::Postgres::start();
        let pool = Pool::connect(&postgres.url(), 1)
            .await
            .unwrap();
        let driver = PostgresDriver::new(Rc::new(pool));
        let session = driver.acquire(LeaseKind::Autocommit).await.unwrap();
        for expression in [
            "'infinity'::timestamp",
            "'-infinity'::timestamptz",
            "'infinity'::date",
            "'-infinity'::date",
            "'NaN'::float4",
            "'Infinity'::float8",
            "'-Infinity'::float8",
            "'NaN'::numeric",
            "'Infinity'::numeric",
            "INTERVAL '1 day'",
        ] {
            let sql = format!("SELECT {expression} AS invalid_result");
            let error = session.query(&sql, &[]).await.expect_err(&sql);
            assert!(error.to_string().contains("invalid_result"), "{error}");
            let rows = session.query("SELECT 1 AS healthy", &[]).await.unwrap();
            assert_eq!(rows[0]["healthy"], Value::from(1));
        }
        let rows = session
            .query("SELECT NULL::timestamp AS stamp, NULL::float8 AS number, NULL::numeric AS decimal, 'null'::jsonb AS document", &[])
            .await
            .unwrap();
        for field in ["stamp", "number", "decimal", "document"] {
            assert_eq!(rows[0][field], Value::Null);
        }
    }

    #[compio::test]
    async fn timestamp_rounding_is_consistent_across_epochs() {
        let postgres = crate::tests::fixtures::postgres::Postgres::start();
        let pool = Pool::connect(&postgres.url(), 1)
            .await
            .unwrap();
        let driver = PostgresDriver::new(Rc::new(pool));
        let session = driver.acquire(LeaseKind::Autocommit).await.unwrap();
        for timestamp in [
            "1969-12-31 23:59:59.999999+00",
            "1970-01-01 00:00:00.000001+00",
            "1999-12-31 23:59:59.999999+00",
            "2000-01-01 00:00:00.000001+00",
        ] {
            let rows = session
                .query("SELECT $1::timestamptz AS stamp, floor(extract(epoch FROM $1::timestamptz) * 1000)::bigint AS expected", &[timestamp.into()])
                .await
                .unwrap();
            assert_eq!(
                rows[0]["stamp"].as_i64(),
                rows[0]["expected"].as_i64(),
                "{timestamp}"
            );
        }
    }
}
