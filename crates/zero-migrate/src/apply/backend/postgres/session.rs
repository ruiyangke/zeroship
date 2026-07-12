//! The `PgSession` in-session Postgres driver seam.
//!
//! The migrate apply path (`PostgresBackend` + its `journal`/`drift`/`baseline`/
//! `precondition`/`backfill` collaborators and the executor PG leaves) is generic
//! over this trait rather than threading a concrete `&compio_postgres::Client`.
//!
//! The trait is typed in DRIVER-NEUTRAL types ([`SeamBind`]/[`SeamRow`]/
//! [`SeamError`], see [`seam`](super::seam)), NOT `compio_postgres::{Row, Error,
//! types::ToSql}` (design §A). Those compio types have private constructors, so a
//! host (napi) driver could neither be *called* (it cannot extract cells from
//! `&[&dyn ToSql]`) nor *return* rows/errors. The neutral types close both sides.
//!
//! The native (`native-pg`) impl below is the FIRST producer of every neutral
//! type: it maps `SeamBind → ToSql` on the way in, and `compio_postgres::Row →
//! SeamRow` / `compio_postgres::Error → SeamError` on the way out. It is the ONLY
//! place still touching `compio_postgres::{types::ToSql, types::FromSql, Row,
//! Error}`. The mapping forwards the exact same wire operations, so the default
//! build's SQL, txn boundaries, journal/lock ordering, and decoded domain values
//! are byte-for-byte unchanged — only the intermediate Rust representation becomes
//! neutral.
//!
//! Connect / lifecycle is a per-impl free function (`crate::conn::connect`), NOT
//! part of this trait: the compio impl detaches a run-loop `JoinHandle`; a Node
//! impl has no run-loop. Transaction control (`BEGIN`/`COMMIT`/`ROLLBACK`),
//! advisory locks (`pg_advisory_lock`), and confinement `SET`s are SQL strings
//! issued through `batch_execute`/`execute` — they are engine logic, not driver
//! methods, so the trait carries no transaction-object or lock abstraction.

use super::seam::{SeamBind, SeamError, SeamRow};

/// The in-session Postgres driver surface the migrate apply path is generic over.
///
/// Exactly the five in-session verbs the engine issues on a live session:
/// `batch_execute` (DDL / txn control / multi-statement session setup), `execute`
/// / `execute_text_params` (parameterized DML → rows affected), and `query` /
/// `query_one` (catalog / journal introspection → rows).
///
/// Every verb's error is the neutral [`SeamError`] (§A.8: uniform across all 5).
/// The bind params are neutral [`SeamBind`] on `execute`/`query`/`query_one` (3 of
/// 5); `execute_text_params` keeps its already-neutral `&[Option<String>]` shape
/// (PG refuses concrete-OID binary for `text → timestamptz`); `batch_execute`
/// takes no params.
#[allow(async_fn_in_trait)] // !Send is by design on the single-thread compio runtime
pub trait PgSession {
    /// DDL / txn control / multi-statement session setup. Simple-query protocol:
    /// one `&str`, may contain `;`-separated statements, no params, no rows.
    async fn batch_execute(&self, sql: &str) -> Result<(), SeamError>;

    /// Parameterized DML → rows affected.
    async fn execute(&self, sql: &str, params: &[SeamBind]) -> Result<u64, SeamError>;

    /// Schema-blind op.* DML: text-format params with server-inferred types.
    /// (Distinct from [`PgSession::execute`]: a concrete-OID binary bind would make
    /// PG refuse `text → timestamptz`; the assembler needs text-format coercion.
    /// Its BIND side is already neutral (`&[Option<String>]`), but its ERROR side
    /// widens to [`SeamError`] uniformly with the other four verbs — a host impl
    /// cannot construct a `compio_postgres::Error`.)
    async fn execute_text_params(
        &self,
        sql: &str,
        params: &[Option<String>],
    ) -> Result<u64, SeamError>;

    /// Parameterized SELECT → all rows.
    async fn query(&self, sql: &str, params: &[SeamBind]) -> Result<Vec<SeamRow>, SeamError>;

    /// Parameterized SELECT → exactly one row (errors otherwise).
    async fn query_one(&self, sql: &str, params: &[SeamBind]) -> Result<SeamRow, SeamError>;
}

// ---------------------------------------------------------------------------
// The native, compio-postgres impl — the FIRST producer of every neutral type.
// `native-pg`-only: it is the ONLY place still touching `compio_postgres::{types::
// ToSql, types::FromSql, Row, Error}`. The `PgSession` trait above is neutral, so
// it compiles on the whole PG seam; a `host-pg`-only build gets the trait but no
// compio adapter (the addon supplies its own host `PgSession` in Phase C, and the
// in-crate recording driver proves genericity in the meantime).
// ---------------------------------------------------------------------------













#[cfg(test)]
mod element_null_parity {
    //! §A.8 dual-adapter parity: a synthetic element-NULL `reloptions`-shaped
    //! `text[]` yields the LEGACY `[]` outcome through BOTH the compio adapter and
    //! a canned host `SeamRow`. Pins parity on the one raw, nullable, unfiltered
    //! catalog array cell so native and host stay interchangeable.

    use super::super::seam::{SeamRow, SeamValue};

    /// A canned host row carrying a `text[]` with an element-NULL, exactly as a
    /// napi `pg` driver would build it (JS `null` inside the array).
    fn host_row_with_element_null() -> SeamRow {
        SeamRow::new(
            vec!["reloptions".to_string()],
            vec![SeamValue::TextArray(vec![
                Some("fillfactor=90".to_string()),
                None, // a SQL NULL element
            ])],
        )
    }

    #[test]
    fn host_element_null_reloptions_coerces_to_empty_via_unwrap_or_default() {
        let row = host_row_with_element_null();
        // The `.unwrap_or_default()` idiom (drift.rs:899/900/902/1114 shape): a
        // NULL element makes the Vec<String> decode err, coerced to [].
        let cols: Vec<String> = row.try_get::<_, Vec<String>>("reloptions").unwrap_or_default();
        assert!(cols.is_empty(), "element-NULL text[] must coerce to [] on host adapter");
    }

    #[test]
    fn host_element_null_reloptions_coerces_to_none_via_ok_flatten() {
        let row = host_row_with_element_null();
        // The `.ok().flatten()` idiom (drift.rs:901 reloptions shape): a NULL
        // element makes the inner Vec<String> decode err → None.
        let opt: Option<Vec<String>> = row
            .try_get::<_, Option<Vec<String>>>("reloptions")
            .ok()
            .flatten();
        assert!(opt.is_none(), "element-NULL text[] must coerce to None on host adapter");
    }


}
