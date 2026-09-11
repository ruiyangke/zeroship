//! PostgreSQL adapter state and database operations.
//!
//! Composes the pool with catalog, key and extension state. The sibling
//! `driver` module owns physical leases, `executor` applies authority, and
//! `search` plans search operations. Schema creation belongs to migrations.

use std::cell::RefCell;
use std::rc::Rc;

use zeroship_data_orm::error::{BeginIntent, CleanupAck, DbError, SettleIntent, TerminalResult};

#[cfg(test)]
use super::PgLockManager;
use super::{pg_autocommit, pg_error};
use zeroship_data_orm::storage::LockManager;

/// PostgreSQL adapter backed by an owned compio pool.
///
/// Holds the local pool, keys and catalog state for the configured URL.
/// The ORM connection factory constructs this backend on its compio thread.
pub struct PostgresBackend {
    pool: Rc<compio_postgres::Pool>,
    /// Configured URL, retained for backend configuration accessors.
    url: String,
    /// Project encryption keys supplied by the trusted host.
    key_store: zeroship_data_orm::encryption::KeyStore,
    /// Cached pgvector extension presence probe.
    ///
    /// `None` before the first [`crate::search::Search::vector_search`] call;
    /// `Some(true)` / `Some(false)`
    /// after the first `SELECT 1 FROM pg_extension WHERE extname='vector'`
    /// round-trip. The probe is per-backend (so per-isolate, since each
    /// isolate carries its own `PostgresBackend` Rc) and stays cached
    /// for the life of the backend — pgvector is provisioned at admin
    /// time and never disappears mid-process. `RefCell` (not `Mutex`)
    /// because every `PostgresBackend` is owned by a single
    /// compio thread.
    pub(super) pgvector_available: RefCell<Option<bool>>,
    /// Cached PostGIS extension presence probe.
    ///
    /// Same shape and lifetime semantics as [`Self::pgvector_available`]:
    /// `None` until the first `SpatialIndex::spatial_near` call probes
    /// `pg_extension WHERE
    /// extname='postgis'`; `Some(true)` / `Some(false)` after. Cached
    /// for the life of the backend (PostGIS is provisioned at admin
    /// time and stays present). Absence surfaces as
    /// `DbError::Configuration { code: "postgis_extension_missing", … }`
    /// from both entry points so the SDK can branch on `.code`.
    pub(super) postgis_available: RefCell<Option<bool>>,
}

impl std::fmt::Debug for PostgresBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresBackend").finish()
    }
}

impl PostgresBackend {
    /// Build a backend handle around an already-initialised pool, with the
    /// column-key source supplied by the caller.
    ///
    /// **The key source is a parameter, not a lookup, and that is a tier
    /// boundary rather than a style preference.** This constructor used to have
    /// a sibling `new()` that called `crate::backend::postgres::context::isolate_key_source()` -
    /// the vendor reaching up into the ENGINE's per-isolate thread-local. Once
    /// this file is `zeroship-data-postgres`, that call cannot compile: the
    /// engine depends on the vendor, so the vendor may not name the engine.
    ///
    /// The lookup did not disappear; it moved up to the composer that always
    /// owned the context,
    /// `zeroship_data_orm::backend_selection`. The old `new()`
    /// even documented the hazard it created - "do not call this from inside a
    /// `context::with` closure, it takes a context borrow of its own" - which
    /// is what a fetch buried in a constructor costs.
    pub fn new(
        pool: Rc<compio_postgres::Pool>,
        url: String,
        key_source: zeroship_data_orm::encryption::ProjectKeySource,
    ) -> Self {
        Self {
            pool,
            url,
            pgvector_available: RefCell::new(None),
            postgis_available: RefCell::new(None),
            key_store: zeroship_data_orm::encryption::KeyStore::new(key_source),
        }
    }

    /// Connect a pool and wrap it, in one call.
    ///
    /// **This exists so that no crate above this one has to name
    /// `compio_postgres::Pool`.** Until 2026-09-02 the adapter connected the
    /// pool itself and handed it to [`Self::new`], which put the vendor type in
    /// `ThreadDbContext::set_pool`'s signature - flagged by
    /// `tests/lib/tier_signature_census.sh` as the adapter embedding a vendor
    /// type. Pushing the composer DOWN instead of up is the only direction that
    /// works: an `open_postgres_backend` in the engine's `backend_selection` was
    /// tried the same day and refused by `xtask/tests/data_architecture.rs`,
    /// because taking `Rc<Pool>` names the vendor from a non-vendor crate just
    /// as surely. Inside this crate the name is simply local.
    ///
    /// The key source stays a PARAMETER for the reason [`Self::new`] documents:
    /// the vendor may not reach up into the engine to look it up.
    ///
    /// # Errors
    ///
    /// [`DbError::config`] with code `db_connect_failed`, carrying the driver's
    /// whole `source` chain. The chain walk is here rather than at the call site
    /// because the root cause - `ECONNREFUSED`, a TLS handshake failure - is
    /// otherwise hidden behind the driver's generic wrapper by the time a caller
    /// sees it.
    pub async fn connect(
        url: &str,
        max_size: usize,
        key_source: zeroship_data_orm::encryption::ProjectKeySource,
    ) -> Result<Self, DbError> {
        let pool = compio_postgres::Pool::connect(url, max_size)
            .await
            .map_err(|e| {
                let mut msg = format!("db: failed to connect: {e}");
                let mut cur: &dyn std::error::Error = &e;
                while let Some(src) = std::error::Error::source(cur) {
                    msg.push_str(&format!(" - caused by: {src}"));
                    cur = src;
                }
                DbError::config("db_connect_failed", msg)
            })?;
        Ok(Self::new(Rc::new(pool), url.to_string(), key_source))
    }

    /// Borrow the inner pool. Provided for the few places that still
    /// need the raw `Pool` (e.g. the v8_classes layer's `ensure_pool`
    /// shim until the consumer migration completes).
    pub fn pool(&self) -> &Rc<compio_postgres::Pool> {
        &self.pool
    }

    /// Borrow the configured URL.
    pub fn url(&self) -> &str {
        &self.url
    }
}

// ---------------------------------------------------------------------------
/// Pooled execution under this app's role fence.
///
/// These are the ONLY way a caller outside `backend/` reaches a pooled
/// connection. Each one narrows the connection to `app_id`'s role before the
/// statement runs and reverts at COMMIT; there is deliberately no entry point
/// that hands out `&Pool`, because a caller holding the pool can open a bare
/// checkout carrying the shared login role and reach a tenant schema unfenced.
/// The tree records one occasion that happened - see the comment above the
/// audit INSERT in `crud/unmask.rs`.
///
/// The three shapes exist because the callers want three different things; the
/// reasoning, including why the byte reader cannot go through JSON, is in
/// [`crate::backend::postgres::pg_autocommit`].
impl PostgresBackend {
    /// Run `sql` under this app's role and render the rows as JSON objects.
    ///
    /// `pub` rather than `pub(crate)`: it is the AUTOCOMMIT arm of the engine's
    /// routed search entry points, whose transaction arm issues the same
    /// statement on the parked lane instead. Both arms have to be written where
    /// the routing decision is, and that is `zeroship-data-orm`.
    ///
    /// # Errors
    ///
    /// Propagates pool checkout, session setup, statement and COMMIT failures.
    pub async fn query_roled_values(
        &self,
        schema: &zeroship_data_sql::SchemaName,
        sql: &str,
        params: &[zeroship_data_sql::value::Value],
    ) -> Result<Vec<zeroship_data_sql::value::Value>, DbError> {
        pg_autocommit::roled_json(&self.pool, schema, sql, params).await
    }

    /// Read column 0 of the first row as raw bytes, under this app's role.
    ///
    /// # Errors
    ///
    /// As [`Self::query_roled_values`], plus a decode failure if column 0 is not
    /// byte-typed.
    pub async fn read_roled_scalar_bytes(
        &self,
        schema: &zeroship_data_sql::SchemaName,
        sql: &str,
        params: &[zeroship_data_sql::value::Value],
    ) -> Result<pg_autocommit::ScalarRead<Vec<u8>>, DbError> {
        pg_autocommit::roled_scalar_bytes(&self.pool, schema, sql, params).await
    }

    /// Read column 0 of the first row as text, under this app's role.
    ///
    /// # Errors
    ///
    /// As [`Self::query_roled_values`], plus a decode failure if column 0 is not
    /// text-typed. A BYTEA column is refused rather than mis-parsed; use
    /// [`Self::read_roled_scalar_bytes`].
    pub async fn read_roled_scalar_text(
        &self,
        schema: &zeroship_data_sql::SchemaName,
        sql: &str,
        params: &[zeroship_data_sql::value::Value],
    ) -> Result<pg_autocommit::ScalarRead<String>, DbError> {
        pg_autocommit::roled_scalar_text(&self.pool, schema, sql, params).await
    }

    /// Run a statement under this app's role, discarding any result rows.
    ///
    /// # Errors
    ///
    /// As [`Self::query_roled_values`].
    pub async fn execute_roled(
        &self,
        schema: &zeroship_data_sql::SchemaName,
        sql: &str,
        params: &[zeroship_data_sql::value::Value],
    ) -> Result<(), DbError> {
        pg_autocommit::roled_statement(&self.pool, schema, sql, params).await
    }

    /// Run `sql` under this app's role and render the rows as JSON, keeping the
    /// `compio_postgres::Row` inside this tier.
    ///
    /// The missing fifth sibling of the four above until 2026-09-02. Because it
    /// did not exist, `crate::backend::postgres::exec` fetched the pool itself - an
    /// `ensure_postgres_pool_for_shared_sql` returning `Rc<Pool>` - and called
    /// `pg_autocommit::roled_rows` directly, which put two vendor signatures in
    /// an ENGINE-tiered file for want of a method that every neighbouring call
    /// already had.
    ///
    /// The JSON conversion happens HERE rather than at the caller for the same
    /// reason: `row_to_value` is this tier's business, and the engine wants
    /// `Vec<Value>` either way - it is what the SQLite arm has always returned.
    ///
    /// # Errors
    ///
    /// As [`Self::query_roled_values`].
    pub async fn query_roled_rows_as_json(
        &self,
        schema: &zeroship_data_sql::SchemaName,
        sql: &str,
        params: &[zeroship_data_sql::value::Value],
    ) -> Result<Vec<zeroship_data_sql::value::Value>, DbError> {
        let rows = pg_autocommit::roled_rows(&self.pool, schema, sql, params).await?;
        super::pg_row_json::rows_to_values(&rows)
    }
}

// Capability impls -- one block per sub-trait:
//
//   1. `impl DatabaseFixture for PostgresBackend`      -- 3 methods.
//   2. `impl LockManager for PostgresBackend`      -- 3 methods.
//   3. `impl Catalog for PostgresBackend` -- 2 methods +
//      `type LiveSchema`.
//
// A fourth, `impl PgDatabaseFixture`, carried a `pool_handle()` accessor handing
// out the raw pool. A bare checkout off that pool runs as the shared
// `zeroship_worker` login role with no `SET LOCAL ROLE`, so it was a standing
// way around the per-app role fence. Its last caller went with
// `crud/mask_drift.rs`; trait, impl and witnesses were deleted 2026-09-09.
// Reaching a tenant schema needs a roled entry point, not a pool handle.
//
// `impl Backend for PostgresBackend {}` below is a one-line composition
// marker -- every operation lives on the sub-trait impls above.
// ---------------------------------------------------------------------------

#[cfg(test)]
impl crate::fixtures::DatabaseFixture for PostgresBackend {
    type Client = compio_postgres::PoolConnection;

    async fn fixture_session(&self, _app_id: &str) -> Result<Self::Client, DbError> {
        self.pool
            .acquire()
            .await
            .map_err(|error| pg_error::classify(&error))
    }

    async fn execute_fixture(&self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
        let client = self
            .pool
            .acquire()
            .await
            .map_err(|e| pg_error::classify(&e))?;
        if params.is_empty() {
            let tag = client
                .batch_execute_reporting_tag(sql)
                .await
                .map_err(|e| pg_error::classify(&e))?;
            Ok(tag
                .and_then(|tag| tag.split_whitespace().last()?.parse().ok())
                .unwrap_or(0))
        } else {
            super::params::execute(&client, sql, params).await
        }
    }

    async fn execute_fixture_on(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        super::params::execute(client, sql, params).await
    }
}

impl LockManager for PostgresBackend {
    type Client = compio_postgres::PoolConnection;
    async fn acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        // Two-key advisory lock on `(hashtext(key1)::int4,
        // hashtext(key2)::int4)`. Session-scoped — held until the
        // backend session ends or `release_advisory_lock` runs.
        let sql = "SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)";
        client
            .query_text_params(sql, &[key1, key2])
            .await
            .map_err(|e| {
                let mut err = pg_error::classify(&e);
                // Decorate the message so operators can see which key
                // failed (the bare SQLSTATE message often doesn't show
                // the hash inputs).
                if let DbError::Internal { message }
                | DbError::Transient { message }
                | DbError::LockContention { message } = &mut err
                {
                    *message = format!("db: pg_advisory_lock({key1}, {key2}) failed: {message}");
                }
                err
            })?;
        Ok(())
    }

    async fn try_acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<bool, DbError> {
        let sql = "SELECT pg_try_advisory_lock(hashtext($1)::int4, hashtext($2)::int4) AS got";
        let rows = client
            .query_text_params(sql, &[key1, key2])
            .await
            .map_err(|e| pg_error::classify(&e))?;
        let got: bool = rows
            .first()
            .map(|r| r.try_get::<_, bool>("got").unwrap_or(false))
            .unwrap_or(false);
        Ok(got)
    }

    async fn release_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        let sql = "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
        client
            .query_text_params(sql, &[key1, key2])
            .await
            .map_err(|e| pg_error::classify(&e))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// VectorIndex — pgvector adapter
// ---------------------------------------------------------------------------
//
// One method: `vector_search` — `SELECT *, col <op> $1::vector AS _distance
// FROM ... ORDER BY col <op> $1::vector LIMIT $2` via `build_vector_search`.
//
// The ivfflat index it reads is NOT created here. `zeroship-migrate` authors
// it from the declared `t.vector(dims, { metric })` field
// (`zeroship-migrate-core/src/render/declarative.rs::vector_index_snapshot`,
// emitted by `zeroship-migrate-postgres/src/ddl.rs::create_index` as
// `USING ivfflat ("col" vector_<metric>_ops) WITH (lists = 100)`), and the
// engine's drift pass compares `access_method` so it round-trips. A search
// against a table whose migration has not been applied is a missing-index
// sequential scan, not a correctness failure.
//
// The probe of `pg_extension WHERE extname='vector'` runs on first call and
// caches on `pgvector_available`. Absence surfaces as
// `DbError::Configuration { code: "vector_extension_missing", ... }`.
// ---------------------------------------------------------------------------

#[cfg(test)]
impl PgLockManager for PostgresBackend {
    async fn acquire_pooled_client_for_lock(
        &self,
    ) -> Result<compio_postgres::PoolConnection, DbError> {
        // Keep one typed pool-checkout error so operator log lines stay
        // grep-able across test-helper callers.
        self.pool.acquire().await.map_err(|e| DbError::Transient {
            message: format!("db: failed to acquire orchestrator client: {e}"),
        })
    }
}

impl PostgresBackend {
    /// Borrow this isolate's column-encryption key store.
    ///
    /// This is all that remains of the `EncryptedColumn` impl deleted on
    /// 2026-09-02: key SOURCING was the only part of column encryption a
    /// backend ever contributed, and PG stopped differing from SQLite on it
    /// when the admin-schema `get_column_key` getter went on 2026-08-27. The
    /// AEAD is `zeroship_data_orm::encryption::aead` for both, so the CRUD passes call it
    /// directly rather than through a per-vendor trait.
    pub fn key_store(&self) -> &zeroship_data_orm::encryption::KeyStore {
        &self.key_store
    }
}

// ===========================================================================
#[cfg(test)]
#[path = "snapshot_fixture.rs"]
mod snapshot_fixture;

/// Render SC-1's [`BeginIntent`] as PostgreSQL's `BEGIN` statement.
///
/// The dialect lives here, not in the protocol. PostgreSQL spells the ANSI
/// levels verbatim, so this is a `format!` today; a backend that did not would
/// still only have to change its own renderer.
pub fn render_begin(intent: BeginIntent) -> String {
    match intent {
        BeginIntent::Default => "BEGIN".to_string(),
        BeginIntent::Isolation(level) => {
            format!("BEGIN ISOLATION LEVEL {}", level.ansi_name())
        }
    }
}

/// Apply the §17.5 per-app PG role to a transaction's dedicated client.
///
/// Issues `SET LOCAL ROLE "<per-app role>"` on `client` so every
/// statement in the surrounding transaction executes under the
/// constrained per-app role rather than the platform login role. `SET
/// LOCAL` auto-reverts at COMMIT / ROLLBACK, so a pooled / dedicated
/// connection can never leak the role to a later use.
///
/// The per-app role is provisioned by the migration service. The WAL
/// consumer + §17.6 watchdog + §17.7 drop step 3 deliberately do NOT
/// call this - they stay on the platform role (the only connection
/// crossing the per-app trust boundary).
///
/// **Lives here, not in the SC-1 driver, because SQLite has no roles.** The
/// protocol says "narrow the session's authority before the creator's first
/// statement"; `SET LOCAL ROLE` is one dialect's answer to that, and the engine
/// asking for it by name was the last thing making `transaction/mod.rs` name
/// `compio_postgres`.
pub async fn apply_per_app_role(
    client: &compio_postgres::Client,
    schema: &zeroship_data_sql::SchemaName,
) -> Result<(), zeroship_data_orm::error::SessionSetupError> {
    // SET LOCAL ROLE + the DB-1 timeout guards (statement / idle-in-tx / lock)
    // in one simple-query batch - all SET LOCAL, so they revert at the tx end.
    // The idle-in-tx guard is the load-bearing defense: a creator callback that
    // never resolves can no longer pin this dedicated connection forever and
    // exhaust the shared Postgres for other tenants.
    let sql = crate::backend::postgres::pg_session_sql::tx_session_setup_sql(schema)
        .map_err(zeroship_data_orm::error::SessionSetupError::failed)?;
    client.simple_query(&sql).await.map_err(|e| {
        // THE SAME `schema`, not a second variable that happens to hold the same
        // characters. The classifier derives the role it expects to see named in
        // the failure; handing it a different identity than the setup batch used
        // is what degrades SCHEMA_NOT_PROVISIONED into a generic failure.
        let mut classified =
            crate::backend::postgres::pg_error::classify_pg_per_app_session_setup(&e, schema);
        zeroship_data_orm::error::prefix_message(
            classified.error_mut(),
            "db: tx session setup (per-app section 17.5 + DB-1 guards): ",
        );
        classified
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// SC-1 terminal projection
// ---------------------------------------------------------------------------
//
// What PostgreSQL did, in the protocol's vocabulary. The vendor decides what
// happened; SC-1 decides what it means. Both are pure functions of what the
// server answered, so they are testable without a connection - which is the
// whole reason they are functions rather than arms inlined in the driver.

/// Project PostgreSQL's command tag onto SC-1's terminal result.
///
/// **L8: a `COMMIT` answered `ROLLBACK` is a FAILED transaction.** PostgreSQL
/// replies with the tag `ROLLBACK` when the transaction is in the failed state,
/// and a driver reading only "did it error" reports a discarded transaction as
/// committed - which is what published change events for writes that never
/// landed. The check is deliberately scoped to the `Commit` arm: `RELEASE`
/// answers with the tag `RELEASE`, so "anything but COMMIT is a failure" would
/// reject every healthy nested commit.
pub fn terminal_from_tag(intent: SettleIntent, tag: Option<&str>) -> TerminalResult {
    match (intent, tag) {
        (SettleIntent::Commit, Some("ROLLBACK")) => TerminalResult::RolledBack,
        (SettleIntent::Commit, _) => TerminalResult::Committed,
        (SettleIntent::Rollback, _) => TerminalResult::RolledBack,
    }
}

/// **Cleanup `ROLLBACK` first, health oracle second.**
///
/// `transaction_status()` returns `None` whenever a request is in flight, and a
/// failed statement's trailing `ReadyForQuery` is not consumed when its `await`
/// returns. Inside a poisoned block every data statement fails with `25P02`, so
/// no retry makes the oracle answer - and `None` is indeterminate, which
/// withdraws. Sampling on entry to `Cancelling` therefore destroys a healthy
/// connection on **every** forced cleanup of a poisoned transaction.
///
/// `ROLLBACK` is accepted from a poisoned block, and answering it resolves the
/// status byte. That is why the two lines below are in this order and must stay
/// in it.
pub async fn cleanup(client: &compio_postgres::Client) -> CleanupAck {
    let rolled_back = client.batch_execute("ROLLBACK").await;
    match client.transaction_status() {
        Some(compio_postgres::TransactionStatus::Idle) => {
            if rolled_back.is_ok() {
                CleanupAck::RolledBack
            } else {
                // The statement errored but the session is provably out of any
                // transaction block. Nothing is open; report the weaker proof.
                CleanupAck::NoOpenTransaction
            }
        }
        // Still in a block, or the oracle cannot say. Either way the cleanup is
        // unproved.
        Some(
            compio_postgres::TransactionStatus::InTransaction
            | compio_postgres::TransactionStatus::Failed,
        )
        | None => CleanupAck::Indeterminate,
    }
}

/// Project the post-failure transaction status onto SC-1's terminal result.
///
/// Reached only when the terminal statement itself failed, where the tag never
/// arrived. `Idle` means the server ended the transaction, so the outcome is
/// known: rolled back. Anything else - still in a transaction, in a FAILED
/// transaction, or a status that could not be read at all - is DBR-03
/// territory: not knowing is not the same as knowing it ended, so it settles
/// `Indeterminate` and the session is withdrawn.
pub const fn terminal_from_status(
    status: Option<compio_postgres::TransactionStatus>,
) -> TerminalResult {
    match status {
        Some(compio_postgres::TransactionStatus::Idle) => TerminalResult::RolledBack,
        _ => TerminalResult::Indeterminate,
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`PostgresBackend`].
    //!
    //! ## What this layer can — and cannot — test in isolation
    //!
    //! `PostgresBackend` is, by design, a thin facade: every method in
    //! its per-capability impls (`DatabaseFixture` / `LockManager` /
    //! `Catalog`) either calls the `Rc<Pool>` directly or
    //! forwards into [`crate::backend::postgres::diff`] / [`crate::backend::postgres::query`] free functions.
    //! `impl Backend for PostgresBackend` is a one-line composition
    //! marker -- every method body lives on a sub-trait impl. The only
    //! non-async logic in this file is:
    //!
    //! * [`PostgresBackend::new`] — captures the `pool` + `url` fields.
    //! * [`PostgresBackend::pool`] / [`PostgresBackend::url`] — getters.
    //! * The [`std::fmt::Debug`] impl — deliberately opaque
    //!   ("PostgresBackend").
    //!
    //! Even those need an `Rc<compio_postgres::Pool>` to construct, and
    //! `Pool::connect` requires a live Postgres listener. There is no
    //! stub / no-IO constructor. The async methods need both a Pool
    //! AND a real `Client`; they're exercised by `crates/zeroship-data-orm/src/tests/integration.rs`.
    //!
    //! That leaves *compile-time* tests as the highest-signal coverage
    //! we can add in `--lib`:
    //!
    //! 1. `PostgresBackend: Backend` — proves the trait impl is wired
    //!    up so any future bound change to `Backend` (adding a method,
    //!    tightening a lifetime, swapping an associated type) fails
    //!    compilation here, not at a distant call site.
    //! 2. Associated-type identities — pin `Client = compio_postgres::PoolConnection`
    //!    and `LiveSchema = zeroship_data_sql::catalog::LiveSchema` so a refactor that
    //!    accidentally swaps either is caught here.
    //! 3. The `Backend: 'static` bound on the trait — re-asserted at
    //!    the impl site.
    //!
    //! These are runtime no-ops (the bodies never execute) — they exist
    //! so `cargo build -p zeroship-data-v8 --tests` fails fast on a
    //! seam break.

    use super::*;
    use crate::backend::postgres::PgLockManager;
    use crate::fixtures::DatabaseFixture;
    use zeroship_data_orm::protection::Catalog;
    use zeroship_data_orm::storage::LockManager;

    // The `Backend` conformance assertion is NOT here: that trait is the
    // adapter's own marker, so both `impl Backend for PostgresBackend` and the
    // assertion pinning it live in `zeroship-data-v8`.

    /// Compile-time: each carved capability trait is impl'd directly on
    /// `PostgresBackend` (not just visible through the
    /// `Backend` super-bound). A regression that pulls one back onto
    /// the omnibus trait or detaches the impl block fails here at
    /// build time.
    fn assert_postgres_backend_impls_sub_traits() {
        fn impls_sql_executor<T: DatabaseFixture<Client = compio_postgres::PoolConnection>>() {}
        fn impls_lock_manager<T: LockManager<Client = compio_postgres::PoolConnection>>() {}
        fn impls_schema_introspect<T: Catalog>() {}
        fn impls_pg_lock_manager<T: PgLockManager>() {}
        impls_sql_executor::<PostgresBackend>();
        impls_lock_manager::<PostgresBackend>();
        impls_schema_introspect::<PostgresBackend>();
        impls_pg_lock_manager::<PostgresBackend>();
    }

    // ---------------------------------------------------------------------
    // PgDialect hook unit tests. ZST has no I/O -- each test
    // is a string-compare against the expected SQL fragment.
    // ---------------------------------------------------------------------

    /// Compile-time: the associated types must remain wired to the
    /// concrete `compio_postgres` / `crate::backend::postgres::diff` types. Swapping
    /// either accidentally would silently change the `B::Client` /
    /// `B::LiveSchema` shape every consumer sees. `LiveSchema` is owned
    /// by [`Catalog`] -- the `Backend` super-bound
    /// `Catalog<LiveSchema = LiveSchema>` re-anchors it so
    /// `Backend<LiveSchema = …>` still resolves here.
    // `assert_postgres_backend_assoc_types` is not here: it is stated in terms
    // of `Backend`, which this crate cannot name. `zeroship-data-v8`'s
    // `assert_associated_types_pinned` pins the same two associated types.
    fn assert_postgres_backend_assoc_types() {}

    /// Compile-time: the `Backend: 'static` bound carries through to
    /// the impl. The per-isolate context relies on this to park
    /// `Rc<PostgresBackend>` in a thread-local without explicit lifetime
    /// gymnastics.
    fn assert_postgres_backend_is_static() {
        fn assert_static<T: 'static>() {}
        assert_static::<PostgresBackend>();
    }

    // Runtime side: the only Pool-free observation we can make is on
    // the `Debug` impl shape. It must remain opaque ("PostgresBackend")
    // so accidentally adding a field that exposes the URL or pool
    // internals via #[derive(Debug)] would be caught here.

    #[test]
    fn debug_impl_is_opaque_source_check() {
        // We can't construct a real `PostgresBackend` without a Pool
        // (Pool::connect needs a live Postgres listener). Instead we
        // verify the *source* of the Debug impl: it must render a
        // bare struct name with no fields, so the URL (which may
        // carry credentials) is never printed.
        //
        // Regression guard: if someone switches to `#[derive(Debug)]`,
        // the rendered string would include
        // `pool: Rc { ... }, url: "postgres://..."` and break the
        // assertion below.
        let src = include_str!("implementation.rs");
        let debug_block = src
            .split("impl std::fmt::Debug for PostgresBackend")
            .nth(1)
            .expect("Debug impl present");
        // First `}` that closes the impl block (the impl body has only
        // one inner `fn fmt` whose own braces match).
        let body_end = debug_block
            .find("\n}\n")
            .expect("Debug impl block has a closing brace");
        let body = &debug_block[..body_end];
        assert!(
            body.contains("debug_struct(\"PostgresBackend\")"),
            "Debug impl must use a `debug_struct(\"PostgresBackend\")` builder"
        );
        assert!(
            body.contains(".finish()"),
            "Debug impl must close with `.finish()` (no fields)"
        );
        assert!(
            !body.contains(".field("),
            "Debug impl must NOT expose internal fields — `url` may contain secrets"
        );
    }

    #[test]
    fn compile_time_trait_assertions_link() {
        // Calling the asserter functions ensures rustc keeps them
        // alive and the `unused` lints don't fire. The compile-time
        // checks happen at type-check time on the function body
        // regardless of whether we call them, but the explicit
        // `_ = ...` documents intent and silences `dead_code`.
        let _ = assert_postgres_backend_impls_sub_traits as fn();
        let _ = assert_postgres_backend_assoc_types as fn();
        let _ = assert_postgres_backend_is_static as fn();
    }
}
#[cfg(test)]
mod begin_render_tests {
    use super::render_begin;
    use zeroship_data_orm::error::{BeginIntent, IsolationLevel};

    /// The dialect half of what `build_begin_sql` used to do in the engine.
    /// Its validation half is now `IsolationLevel::parse`, tested in data-core -
    /// the split is the point: a typo cannot reach here, because the only way
    /// in is a variant.
    #[test]
    fn every_intent_renders_its_postgres_statement() {
        assert_eq!(render_begin(BeginIntent::Default), "BEGIN");
        assert_eq!(
            render_begin(BeginIntent::Isolation(IsolationLevel::Serializable)),
            "BEGIN ISOLATION LEVEL SERIALIZABLE"
        );
        assert_eq!(
            render_begin(BeginIntent::Isolation(IsolationLevel::ReadCommitted)),
            "BEGIN ISOLATION LEVEL READ COMMITTED"
        );
        assert_eq!(
            render_begin(BeginIntent::Isolation(IsolationLevel::ReadUncommitted)),
            "BEGIN ISOLATION LEVEL READ UNCOMMITTED"
        );
        assert_eq!(
            render_begin(BeginIntent::Isolation(IsolationLevel::RepeatableRead)),
            "BEGIN ISOLATION LEVEL REPEATABLE READ"
        );
    }
}

#[cfg(test)]
mod terminal_projection_tests {
    use super::{terminal_from_status, terminal_from_tag};
    use compio_postgres::TransactionStatus;
    use zeroship_data_orm::error::{SettleIntent, TerminalResult};

    /// **L8, without a database.** PostgreSQL answers `COMMIT` with the tag
    /// `ROLLBACK` when the transaction is in the failed state, and reading that
    /// as success reports discarded writes as durable.
    ///
    /// This rule was only reachable through a live server until 2026-09-02,
    /// when the projection was split out of `terminal`. The live arm that
    /// covered it - `commit_that_postgres_rolled_back_must_not_report_success_l8`
    /// in `crates/zeroship-data-v8/src/tests/native_transaction.rs` - had ALSO been failing for an unrelated
    /// reason (it duplicated a platform-assigned `id`, so it never poisoned the
    /// transaction at all), which means this rule went unbound in practice for
    /// as long as that test was red. A pure arm cannot rot that way.
    #[test]
    fn a_commit_answered_rollback_is_a_rollback() {
        assert_eq!(
            terminal_from_tag(SettleIntent::Commit, Some("ROLLBACK")),
            TerminalResult::RolledBack
        );
    }

    /// The control the L8 rule needs: a healthy commit differs from the case
    /// above in the TAG ALONE, and must not be swept up by it.
    #[test]
    fn a_commit_answered_commit_is_a_commit() {
        assert_eq!(
            terminal_from_tag(SettleIntent::Commit, Some("COMMIT")),
            TerminalResult::Committed
        );
    }

    /// **The scoping that keeps nested commits working.** `RELEASE` answers with
    /// the tag `RELEASE`, so a rule shaped "anything but COMMIT is a failure"
    /// would reject every healthy savepoint release. Only the literal `ROLLBACK`
    /// tag means failure.
    #[test]
    fn a_commit_answered_release_is_not_treated_as_failure() {
        assert_eq!(
            terminal_from_tag(SettleIntent::Commit, Some("RELEASE")),
            TerminalResult::Committed
        );
        assert_eq!(
            terminal_from_tag(SettleIntent::Commit, None),
            TerminalResult::Committed
        );
    }

    /// A rollback is a rollback whatever the server called it - including when
    /// the server says `COMMIT`, which `outcome_error` then reports as a
    /// `settle_result_mismatch` rather than quietly accepting.
    #[test]
    fn a_rollback_is_a_rollback_for_every_tag() {
        for tag in [Some("ROLLBACK"), Some("COMMIT"), Some("RELEASE"), None] {
            assert_eq!(
                terminal_from_tag(SettleIntent::Rollback, tag),
                TerminalResult::RolledBack,
                "rollback misclassified for tag {tag:?}"
            );
        }
    }

    /// **DBR-03: not knowing is not the same as knowing it ended.** Only an
    /// `Idle` status proves the server ended the transaction. Every other
    /// reading - still in a transaction, in a failed transaction, or a status we
    /// could not read at all - settles indeterminate and withdraws the session.
    #[test]
    fn only_idle_proves_a_failed_terminal_actually_rolled_back() {
        assert_eq!(
            terminal_from_status(Some(TransactionStatus::Idle)),
            TerminalResult::RolledBack
        );
        assert_eq!(
            terminal_from_status(Some(TransactionStatus::InTransaction)),
            TerminalResult::Indeterminate
        );
        assert_eq!(
            terminal_from_status(Some(TransactionStatus::Failed)),
            TerminalResult::Indeterminate
        );
        assert_eq!(terminal_from_status(None), TerminalResult::Indeterminate);
    }
}

#[cfg(test)]
use zeroship_data_sql::value::Value;
