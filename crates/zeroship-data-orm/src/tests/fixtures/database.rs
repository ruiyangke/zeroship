//! Host-level database setup helpers for conformance fixtures.
use crate::error::DbError;
pub(crate) trait DatabaseFixture: 'static {
    type Client;

    /// A client on the lane `alias` names.
    ///
    /// `alias` is the physical qualifier, not the tenant: on SQLite the lane
    /// resolves its own `ATTACH` by that name, so a fixture that then runs
    /// qualified SQL must hand in the same string its tables live under.
    #[allow(async_fn_in_trait)]
    async fn fixture_session(&self, alias: &str) -> Result<Self::Client, DbError>;

    #[allow(async_fn_in_trait)]
    async fn execute_fixture(
        &self,
        sql: &str,
        params: &[crate::value::Value],
    ) -> Result<u64, DbError>;

    #[allow(async_fn_in_trait)]
    async fn execute_fixture_on(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[crate::value::Value],
    ) -> Result<u64, DbError>;
}
