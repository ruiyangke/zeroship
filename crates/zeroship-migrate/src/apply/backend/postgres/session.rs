//! The `PgSession` in-session Postgres driver seam.
//!
//! The migrate apply path (`PostgresBackend` + its `journal`/`drift`/`baseline`/
//! `precondition`/`backfill` collaborators and the executor PG leaves) is generic
//! over this trait rather than threading a concrete `&compio_postgres::Client`.
//! The single impl this milestone ships forwards each verb one-to-one to today's
//! `compio_postgres::Client` methods, so the default (`native-pg`) build is
//! byte-for-byte behaviorally unchanged.
//!
//! The trait's signatures name `compio_postgres::{Row, Error, types::ToSql}`, so
//! the whole module lives behind `native-pg` (a deliberate zero-churn choice: no
//! associated `type Error`, no local row type, no `ToSql`-annotation churn at the
//! ~29 param call sites). A future compio-free variant (associated `type Error`,
//! a local `SeamRow`, dropped concrete `ToSql`) is future work — it lands with the
//! second (Node/napi) driver impl.
//!
//! Connect / lifecycle is a per-impl free function (`crate::conn::connect`), NOT
//! part of this trait: the compio impl detaches a run-loop `JoinHandle`; a Node
//! impl has no run-loop. Transaction control (`BEGIN`/`COMMIT`/`ROLLBACK`),
//! advisory locks (`pg_advisory_lock`), and confinement `SET`s are SQL strings
//! issued through `batch_execute`/`execute` — they are engine logic, not driver
//! methods, so the trait carries no transaction-object or lock abstraction.

use compio_postgres::{types::ToSql, Error, Row};

/// The in-session Postgres driver surface the migrate apply path is generic over.
///
/// Exactly the five in-session verbs the engine issues on a live session:
/// `batch_execute` (DDL / txn control / multi-statement session setup), `execute`
/// / `execute_text_params` (parameterized DML → rows affected), and `query` /
/// `query_one` (catalog / journal introspection → rows).
#[allow(async_fn_in_trait)] // !Send is by design on the single-thread compio runtime
pub trait PgSession {
    /// DDL / txn control / multi-statement session setup. Simple-query protocol:
    /// one `&str`, may contain `;`-separated statements, no params, no rows.
    async fn batch_execute(&self, sql: &str) -> Result<(), Error>;

    /// Parameterized DML → rows affected.
    async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, Error>;

    /// Schema-blind op.* DML: text-format params with server-inferred types.
    /// (Distinct from [`PgSession::execute`]: a concrete-OID binary bind would make
    /// PG refuse `text → timestamptz`; the assembler needs text-format coercion.)
    async fn execute_text_params(
        &self,
        sql: &str,
        params: &[Option<String>],
    ) -> Result<u64, Error>;

    /// Parameterized SELECT → all rows.
    async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, Error>;

    /// Parameterized SELECT → exactly one row (errors otherwise).
    async fn query_one(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Row, Error>;
}

/// The native, compio-postgres [`PgSession`] impl — one forward per verb, so the
/// default build's SQL, txn boundaries, and behavior are identical to pre-seam.
impl PgSession for compio_postgres::Client {
    async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        compio_postgres::Client::batch_execute(self, sql).await
    }

    async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, Error> {
        compio_postgres::Client::execute(self, sql, params).await
    }

    async fn execute_text_params(
        &self,
        sql: &str,
        params: &[Option<String>],
    ) -> Result<u64, Error> {
        compio_postgres::Client::execute_text_params(self, sql, params).await
    }

    async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, Error> {
        compio_postgres::Client::query(self, sql, params).await
    }

    async fn query_one(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Row, Error> {
        compio_postgres::Client::query_one(self, sql, params).await
    }
}
