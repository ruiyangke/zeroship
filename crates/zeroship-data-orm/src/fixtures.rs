//! Host-level database setup helpers for conformance fixtures.
use crate::error::DbError;
pub trait DatabaseFixture: 'static {
    type Client;

    #[allow(async_fn_in_trait)]
    async fn fixture_session(&self, app_id: &str) -> Result<Self::Client, DbError>;

    #[allow(async_fn_in_trait)]
    async fn execute_fixture(
        &self,
        sql: &str,
        params: &[zeroship_data_sql::value::Value],
    ) -> Result<u64, DbError>;

    #[allow(async_fn_in_trait)]
    async fn execute_fixture_on(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[zeroship_data_sql::value::Value],
    ) -> Result<u64, DbError>;
}
