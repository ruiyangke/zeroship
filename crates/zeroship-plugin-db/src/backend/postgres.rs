//! `PostgresBackend` — the single concrete impl of [`super::Backend`].
//!
//! Wraps the `compio_postgres::Pool` and the configured URL. Every PG-
//! flavoured call moves here so consumer files such as `transaction/*` can stay
//! free of `compio_postgres::Client` direct
//! references and go through the trait instead.
//!
//! The methods are intentionally thin — they forward to PG-tier helpers
//! beside this module or to vendor-neutral builders in `crate::query`.
//! The point of this file is to *name the seam*: PostgreSQL catalog SQL
//! lives in `pg_introspect`, not in the schema floor.
//!
//! **No method here emits DDL.** Schema belongs to `zeroship-migrate`,
//! which authors the pgvector `USING ivfflat` and PostGIS `USING gist`
//! indexes from the declared `t.vector()` / `t.geoPoint()` fields
//! (`zeroship-migrate-core/src/render/declarative.rs`, `vector_index_snapshot`
//! / `geo_index_snapshot`). The two `pg_extension` capability probes below
//! are the only production catalog reads that survive.

use std::cell::RefCell;
use std::rc::Rc;

#[cfg(any(test, feature = "test-helpers"))]
use crate::diff::LiveSchema;
use zeroship_data_core::error::{CleanupAck, DbError, SettleIntent, TerminalResult};

#[cfg(any(test, feature = "test-helpers"))]
use super::Backend;
#[cfg(any(test, feature = "test-helpers"))]
use super::pg_introspect;
use super::{
    DialectBuilder, GeoPoint, LockManager, SpatialIndex, SqlExecutor, VectorIndex, VectorMetric,
};
#[cfg(any(test, feature = "test-helpers"))]
use super::{PgLockManager, PgSqlExecutor, SchemaIntrospect};
use super::{pg_autocommit, pg_error};

/// Single concrete impl of `Backend` backed by `compio_postgres`.
///
/// Holds the `Rc<Pool>` for the configured URL. The pool itself is
/// created by [`crate::init_pool_async`] and stashed in the per-isolate
/// context; this wrapper just provides the trait facade.
pub struct PostgresBackend {
    pool: Rc<compio_postgres::Pool>,
    /// Configured URL — used by [`Self::acquire_dedicated_client`] to
    /// open a fresh connection outside the pool (for the
    /// `db.transaction(fn)` and `migrationBegin` paths that need a
    /// connection that survives across pool-return points).
    url: String,
    /// Per-backend column-key cache. Lazily resolves
    /// `(app_id, key_id) → AeadKey` from this isolate's in-process root
    /// key source -- roots the host supplied, else
    /// `ZEROSHIP_COLUMN_KEY_<KEYID>` env vars. No database round-trip is
    /// involved and none is wanted (see `crate::encryption::keys`).
    /// Single-threaded (`RefCell` inside `KeyStore`) since every
    /// `PostgresBackend` is owned by a single compio thread.
    key_store: crate::encryption::KeyStore,
    /// Cached pgvector extension presence probe.
    ///
    /// `None` before the first [`VectorIndex::vector_search`] call;
    /// `Some(true)` / `Some(false)`
    /// after the first `SELECT 1 FROM pg_extension WHERE extname='vector'`
    /// round-trip. The probe is per-backend (so per-isolate, since each
    /// isolate carries its own `PostgresBackend` Rc) and stays cached
    /// for the life of the backend — pgvector is provisioned at admin
    /// time and never disappears mid-process. `RefCell` (not `Mutex`)
    /// because every `PostgresBackend` is owned by a single
    /// compio thread.
    pgvector_available: RefCell<Option<bool>>,
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
    postgis_available: RefCell<Option<bool>>,
}

impl std::fmt::Debug for PostgresBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresBackend").finish()
    }
}

impl PostgresBackend {
    /// Build a backend handle around an already-initialised pool.
    ///
    /// Column keys resolve through whatever local source this isolate
    /// carries - the roots a host installed, else
    /// `ZEROSHIP_COLUMN_KEY_<KEYID>`. A caller that wants to pin the
    /// source regardless of the isolate goes through
    /// [`Self::new_with_key_source`].
    ///
    /// Do not call this from inside a `context::with` / `with_mut`
    /// closure - it takes a context borrow of its own.
    /// `ThreadDbContext::set_pool` uses `new_with_key_source` for
    /// exactly that reason.
    pub fn new(pool: Rc<compio_postgres::Pool>, url: String) -> Self {
        let key_source = crate::context::isolate_key_source();
        Self::new_with_key_source(pool, url, key_source)
    }

    /// Build a backend handle with an explicit column-key source.
    ///
    /// The isolate context uses this to hand a backend the root keys the
    /// host installed (`ThreadDbContext::local_key_source`) instead of
    /// the process environment.
    pub fn new_with_key_source(
        pool: Rc<compio_postgres::Pool>,
        url: String,
        key_source: crate::encryption::LocalKeySource,
    ) -> Self {
        Self {
            pool,
            url,
            pgvector_available: RefCell::new(None),
            postgis_available: RefCell::new(None),
            key_store: crate::encryption::KeyStore::new(key_source),
        }
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
/// [`crate::backend::pg_autocommit`].
impl PostgresBackend {
    /// Run `sql` under this app's role and render the rows as JSON objects.
    ///
    /// # Errors
    ///
    /// Propagates pool checkout, session setup, statement and COMMIT failures.
    pub(crate) async fn query_roled_json(
        &self,
        app_id: &str,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<serde_json::Value>, DbError> {
        pg_autocommit::roled_json(&self.pool, app_id, sql, params).await
    }

    /// Read column 0 of the first row as raw bytes, under this app's role.
    ///
    /// # Errors
    ///
    /// As [`Self::query_roled_json`], plus a decode failure if column 0 is not
    /// byte-typed.
    pub(crate) async fn read_roled_scalar_bytes(
        &self,
        app_id: &str,
        sql: &str,
        params: &[&str],
    ) -> Result<pg_autocommit::ScalarRead<Vec<u8>>, DbError> {
        pg_autocommit::roled_scalar_bytes(&self.pool, app_id, sql, params).await
    }

    /// Read column 0 of the first row as text, under this app's role.
    ///
    /// # Errors
    ///
    /// As [`Self::query_roled_json`], plus a decode failure if column 0 is not
    /// text-typed. A BYTEA column is refused rather than mis-parsed; use
    /// [`Self::read_roled_scalar_bytes`].
    pub(crate) async fn read_roled_scalar_text(
        &self,
        app_id: &str,
        sql: &str,
        params: &[&str],
    ) -> Result<pg_autocommit::ScalarRead<String>, DbError> {
        pg_autocommit::roled_scalar_text(&self.pool, app_id, sql, params).await
    }

    /// Run a statement under this app's role, discarding any result rows.
    ///
    /// # Errors
    ///
    /// As [`Self::query_roled_json`].
    pub(crate) async fn execute_roled(
        &self,
        app_id: &str,
        sql: &str,
        params: &[&str],
    ) -> Result<(), DbError> {
        pg_autocommit::roled_statement(&self.pool, app_id, sql, params).await
    }

    /// Run `sql` under this app's role and render the rows as JSON, keeping the
    /// `compio_postgres::Row` inside this tier.
    ///
    /// The missing fifth sibling of the four above until 2026-09-02. Because it
    /// did not exist, `crate::exec` fetched the pool itself - an
    /// `ensure_postgres_pool_for_shared_sql` returning `Rc<Pool>` - and called
    /// `pg_autocommit::roled_rows` directly, which put two vendor signatures in
    /// an ENGINE-tiered file for want of a method that every neighbouring call
    /// already had.
    ///
    /// The JSON conversion happens HERE rather than at the caller for the same
    /// reason: `row_to_json` is this tier's business, and the engine wants
    /// `Vec<Value>` either way - it is what the SQLite arm has always returned.
    ///
    /// # Errors
    ///
    /// As [`Self::query_roled_json`].
    pub(crate) async fn query_roled_rows_as_json(
        &self,
        app_id: &str,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<serde_json::Value>, DbError> {
        let rows = pg_autocommit::roled_rows(&self.pool, app_id, sql, params).await?;
        Ok(super::pg_row_json::rows_to_json_value(&rows))
    }
}

// Capability impls -- four blocks, one per sub-trait:
//
//   1. `impl SqlExecutor for PostgresBackend`      -- 3 methods.
//   2. `impl LockManager for PostgresBackend`      -- 3 methods.
//   3. `impl SchemaIntrospect for PostgresBackend` -- 2 methods +
//      `type LiveSchema`.
//   4. `impl PgSqlExecutor for PostgresBackend`    -- 1 method --
//      the PG-only `pool_handle()` accessor that lets free-function
//      helpers reach `&Pool` without naming `PostgresBackend`.
//
// `impl Backend for PostgresBackend {}` below is a one-line composition
// marker -- every operation lives on the sub-trait impls above.
// ---------------------------------------------------------------------------

impl SqlExecutor for PostgresBackend {
    type Client = compio_postgres::OwnedPooledClient;

    /// Check a connection out of the pool for the caller's own use.
    ///
    /// **This is a capacity-model change, not a refactor.** Until 2026-08-27
    /// this opened a brand new TCP connection per transaction with
    /// `compio_postgres::connect` and a detached task per connection - it never
    /// touched the pool. The concurrent-transaction ceiling was therefore
    /// *unbounded*, and a worker multiplexing ~200 apps that each open a
    /// transaction opened ~200 backends. That is a way to exhaust a cluster's
    /// `max_connections` from one worker, which takes every tenant down rather
    /// than slowing one. Operator decision, 2026-08-27: connections always come
    /// from a pool.
    ///
    /// The inversion is real and is stated rather than discovered: a
    /// transaction that used to get a connection now **queues**, and the pool
    /// size is the ceiling for the whole worker.
    ///
    /// ## Policy chosen here, and OWED to a later decision
    ///
    /// Three questions are deliberately not settled by the design, and this
    /// code takes the conservative option on each rather than inventing an
    /// answer. Each is owed a decision before this ships to a real tenant:
    ///
    /// 1. **Ceiling** - the shared data pool's `max_size` (`lib.rs`:
    ///    `Pool::connect(&url, 8)`). Conservative because it adds no new
    ///    connections to what the process already opens. A dedicated
    ///    transaction pool would raise the total and needs a sizing decision.
    /// 2. **Exhaustion** - queue on the pool's existing `acquire_timeout`
    ///    rather than refuse immediately. Conservative because a bounded wait
    ///    degrades to the previous behaviour under light load and refuses only
    ///    when the wait genuinely expires. It is NOT yet tied to SC-1's
    ///    deadline; when that lands, the acquire wait must be inside it.
    /// 3. **Starvation** - one app CAN now starve others: the pool is shared,
    ///    the queue is FIFO across apps, and nothing here is per-app. The
    ///    unbounded model made this impossible. No per-app fairness is
    ///    implemented, because inventing one would settle a question the
    ///    design explicitly left open.
    /// 4. **Autocommit work shares the same slots, and this is the one that
    ///    bites first.** The three questions above are the design's; this one
    ///    is not a refinement of the third, it is a fourth. A transaction does
    ///    not merely compete with other *transactions* for the pool - it
    ///    competes with every co-resident autocommit operation, because those
    ///    take a SECOND checkout from the very same pool:
    ///    `exec::query_postgres_pool_with_autocommit_role`, `pool_exec` just
    ///    below, the unmask path in `crud::unmask` and `audit::write_audit_row`
    ///    all call `pool.get()` / `pool.query_*`. `TxRoute` routes an operation
    ///    with no transaction in its call chain to the pool *deliberately*,
    ///    including while that app holds a transaction open, so this is the
    ///    designed path and not a leak.
    ///
    ///    With `max_size = 8`, eight concurrent transactions on one worker
    ///    thread therefore pin every slot, and each co-resident autocommit op
    ///    blocks for the full `acquire_timeout` before failing - while a
    ///    transaction may hold its slot idle for `DB_IDLE_IN_TX_TIMEOUT_MS`
    ///    plus a statement timeout per statement. The unbounded model made
    ///    this shape impossible, because a transaction never touched the pool.
    ///    Nothing here reserves headroom for autocommit work, and adding a
    ///    reservation is a sizing decision - like (1) - that this change does
    ///    not get to take on its own.
    ///
    /// `_app_id` is unused here and that is correct rather than lazy: a pooled
    /// checkout is app-agnostic, and the per-app constraint the PostgreSQL arm
    /// does enforce is the role, applied separately by
    /// `transaction::apply_per_app_role` on the connection this returns.
    async fn acquire_dedicated_client(&self, _app_id: &str) -> Result<Self::Client, DbError> {
        self.pool.get_owned().await.map_err(|e| {
            // Walk the source chain. The pool renders an exhausted acquire as
            // the generic "error connecting to server" wrapper and puts
            // "connection timeout after 400ms (pool: 0/1 idle, 1/1 total)" in
            // its source - so the bare Display tells an operator the server is
            // unreachable when what actually happened is that this worker hit
            // its own ceiling. Those need different responses.
            let mut message = format!("db: pooled checkout for a dedicated client failed: {e}");
            let mut cur: &dyn std::error::Error = &e;
            while let Some(source) = std::error::Error::source(cur) {
                message.push_str(&format!(" - caused by: {source}"));
                cur = source;
            }
            DbError::Transient { message }
        })
    }

    async fn pool_exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        let rows = self
            .pool
            .query_text_params(sql, params)
            .await
            .map_err(|e| pg_error::classify(&e))?;
        Ok(rows.len() as u64)
    }

    #[cfg(any(test, feature = "test-helpers"))]
    async fn pool_exec_ddl(&self, sql: &str) -> Result<(), DbError> {
        // Multi-statement DDL (CREATE TABLE + implicit system-field
        // CREATE INDEXes + `COMMENT ON COLUMN` mask sentinels) must use
        // the simple query protocol — `query_text_params` (extended
        // protocol) rejects it with `cannot insert multiple commands
        // into a prepared statement`. `batch_execute` issues a single
        // `Query` message and runs the `;`-separated statements in one
        // implicit transaction.
        let client = self.pool.get().await.map_err(|e| pg_error::classify(&e))?;
        client
            .batch_execute(sql)
            .await
            .map_err(|e| pg_error::classify(&e))
    }

    async fn client_exec(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[&str],
    ) -> Result<u64, DbError> {
        let rows = client
            .query_text_params(sql, params)
            .await
            .map_err(|e| pg_error::classify(&e))?;
        Ok(rows.len() as u64)
    }
}

impl LockManager for PostgresBackend {
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

#[cfg(any(test, feature = "test-helpers"))]
impl SchemaIntrospect for PostgresBackend {
    type LiveSchema = LiveSchema;

    async fn introspect_schema(&self, app_id: &str) -> Result<Self::LiveSchema, DbError> {
        // The PG-tier reader returns its local `SchemaError`; the sibling
        // translator re-creates the exact `coded_sql("diff: …", e)` shape, so
        // SQLSTATE classification and the operator-facing message stay
        // unchanged while the schema snapshot remains vendor-neutral.
        pg_introspect::read_live_schema(&self.pool, app_id)
            .await
            .map_err(pg_error::classify_schema_error)
    }

    async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError> {
        pg_introspect::estimate_row_count(&self.pool, app_id, collection)
            .await
            .map_err(pg_error::classify_schema_error)
    }
}

#[cfg(any(test, feature = "test-helpers"))]
impl PgSqlExecutor for PostgresBackend {
    fn pool_handle(&self) -> &Rc<compio_postgres::Pool> {
        &self.pool
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

impl PostgresBackend {
    /// Check (and cache) whether the `vector` extension is installed on
    /// the connected database. The probe runs at most once per backend
    /// instance — pgvector is provisioned at admin time and stays present
    /// for the life of the process.
    ///
    /// Returns `Ok(())` when present; `Err(DbError::Configuration)` with
    /// code `vector_extension_missing` otherwise. Connection failures
    /// during the probe surface as `DbError::Transient` so callers can
    /// distinguish "extension missing" from "database unreachable".
    async fn ensure_pgvector_available(&self) -> Result<(), DbError> {
        // Fast path: cached result.
        if let Some(present) = *self.pgvector_available.borrow() {
            if present {
                return Ok(());
            }
            return Err(DbError::config_hinted(
                "vector_extension_missing",
                "pgvector is not installed on this database",
                "run `CREATE EXTENSION vector;` (Postgres superuser) or \
                 swap the database image to `pgvector/pgvector:pg16` \
                 (see docs/runbooks/docker-compose.md)",
            ));
        }

        let empty: Vec<&str> = Vec::new();
        let rows = self
            .pool
            .query_text_params("SELECT 1 FROM pg_extension WHERE extname='vector'", &empty)
            .await
            .map_err(|e| pg_error::classify(&e))?;
        let present = !rows.is_empty();
        *self.pgvector_available.borrow_mut() = Some(present);
        if present {
            Ok(())
        } else {
            Err(DbError::config_hinted(
                "vector_extension_missing",
                "pgvector is not installed on this database",
                "run `CREATE EXTENSION vector;` (Postgres superuser) or \
                 swap the database image to `pgvector/pgvector:pg16` \
                 (see docs/runbooks/docker-compose.md)",
            ))
        }
    }
}

impl VectorIndex for PostgresBackend {
    async fn vector_search(
        &self,
        binding: &zeroship_data_core::binding::DbBinding,
        collection: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: VectorMetric,
        filter: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        // Probe so a missing extension surfaces with the same typed
        // error shape the capability probe produces — the SDK branches
        // on `e.code === "vector_extension_missing"` regardless of
        // which entry point fired.
        self.ensure_pgvector_available().await?;

        let app_id = binding.app_id();
        // The projection allowlist and the `column` identifier check both come
        // off the descriptor. A collection this deploy does not declare is
        // refused here rather than searched with an unbounded projection.
        let schema_hint = crate::descriptor::collection_schema(binding, collection)?;
        let bq = crate::query::build_vector_search(
            app_id,
            collection,
            column,
            query,
            k,
            metric,
            filter,
            &schema_hint,
        )
        .map_err(DbError::from)?;

        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        self.query_roled_json(app_id, &bq.sql, &param_refs).await
    }
}

// ---------------------------------------------------------------------------
// SpatialIndex — PostGIS adapter
// ---------------------------------------------------------------------------
//
// One method: `spatial_near` — `WHERE ST_DWithin(col, ST_MakePoint(lng, lat)::
// geography, radius) ORDER BY ST_Distance(...) LIMIT $4`.
//
// The GiST index it reads is NOT created here. `zeroship-migrate` authors it
// from the declared `t.geoPoint()` field
// (`zeroship-migrate-core/src/render/declarative.rs::geo_index_snapshot`,
// emitted as `USING gist ("col")`).
//
// Both probe `pg_extension WHERE extname='postgis'` on first call and
// cache on `postgis_available`. Absence surfaces as
// `DbError::Configuration { code: "postgis_extension_missing", ... }`.
// ---------------------------------------------------------------------------

impl PostgresBackend {
    /// Check (and cache) whether the `postgis` extension is installed on
    /// the connected database. Mirrors [`Self::ensure_pgvector_available`]
    /// — the probe runs at most once per backend; PostGIS is
    /// provisioned at admin time and stays present.
    async fn ensure_postgis_available(&self) -> Result<(), DbError> {
        if let Some(present) = *self.postgis_available.borrow() {
            if present {
                return Ok(());
            }
            return Err(DbError::config_hinted(
                "postgis_extension_missing",
                "PostGIS is not installed on this database",
                "run `CREATE EXTENSION postgis;` (Postgres superuser) or \
                 swap the database image to a PostGIS-bundled variant \
                 (see docs/runbooks/docker-compose.md)",
            ));
        }

        let empty: Vec<&str> = Vec::new();
        let rows = self
            .pool
            .query_text_params("SELECT 1 FROM pg_extension WHERE extname='postgis'", &empty)
            .await
            .map_err(|e| pg_error::classify(&e))?;
        let present = !rows.is_empty();
        *self.postgis_available.borrow_mut() = Some(present);
        if present {
            Ok(())
        } else {
            Err(DbError::config_hinted(
                "postgis_extension_missing",
                "PostGIS is not installed on this database",
                "run `CREATE EXTENSION postgis;` (Postgres superuser) or \
                 swap the database image to a PostGIS-bundled variant \
                 (see docs/runbooks/docker-compose.md)",
            ))
        }
    }
}

impl SpatialIndex for PostgresBackend {
    async fn spatial_near(
        &self,
        binding: &zeroship_data_core::binding::DbBinding,
        collection: &str,
        column: &str,
        point: GeoPoint,
        radius_m: f64,
        filter: &serde_json::Value,
        limit: Option<usize>,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        self.ensure_postgis_available().await?;

        let app_id = binding.app_id();
        let schema_hint = crate::descriptor::collection_schema(binding, collection)?;
        let bq = crate::query::build_spatial_near(
            app_id,
            collection,
            column,
            point,
            radius_m,
            filter,
            limit,
            &schema_hint,
        )
        .map_err(DbError::from)?;
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        self.query_roled_json(app_id, &bq.sql, &param_refs).await
    }
}

#[cfg(any(test, feature = "test-helpers"))]
impl PgLockManager for PostgresBackend {
    async fn acquire_pooled_client_for_lock(
        &self,
    ) -> Result<compio_postgres::OwnedPooledClient, DbError> {
        // Keep one typed pool-checkout error so operator log lines stay
        // grep-able across test-helper callers.
        self.pool.get_owned().await.map_err(|e| DbError::Transient {
            message: format!("db: failed to acquire orchestrator client: {e}"),
        })
    }
}

// ---------------------------------------------------------------------------
// PgDialect — the Postgres flavour of `DialectBuilder`. The trait impl
// lands on both backends so `query.rs`'s free-function string
// builders can be retargeted onto a dialect-typed entry point
// without re-shaping their call sites.
//
// The hooks are pure functions of their inputs (ZST has no state). We
// `impl DialectBuilder for PostgresBackend` directly — there is no
// reason to carry a `PgDialect` field on the backend struct because
// the ZST has nothing to store. The `PgDialect` type is kept around
// only as the documentation anchor; consumers reach the impl through
// `&PostgresBackend`.
// ---------------------------------------------------------------------------

/// Postgres-flavoured dialect. Zero-sized — every method is pure.
///
/// Not instantiated by production code today; the matching trait
/// behaviour lives on `impl DialectBuilder for PostgresBackend` below.
/// Kept as a documentation anchor + so the test module can name the
/// ZST when asserting per-hook output without holding a `Pool`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PgDialect;

impl DialectBuilder for PgDialect {
    #[cfg(any(test, feature = "test-helpers"))]
    fn sql_dialect(&self) -> crate::query::SqlDialect {
        crate::query::SqlDialect::Postgres
    }

    /// Double-quote with embedded-quote escape. Matches the existing
    /// `crate::query::quote_ident` helper byte-for-byte.
    fn quote_ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    /// PG mapping for the type vocabulary. Each branch is a single
    /// `&'static str` — matches the column-type names PG accepts in a
    /// `CREATE TABLE` DDL.
    fn map_zs_type(&self, zs_type: &str, _opts: &serde_json::Value) -> String {
        match zs_type {
            "text" => "TEXT",
            "bigint" | "int8" => "BIGINT",
            "integer" | "int4" | "int" => "INTEGER",
            "double" => "DOUBLE PRECISION",
            "real" => "REAL",
            "bytes" | "blob" => "BYTEA",
            "numeric" | "decimal" => "NUMERIC",
            "boolean" | "bool" => "BOOLEAN",
            "timestamp" => "TIMESTAMP",
            "timestamptz" => "TIMESTAMPTZ",
            "json" => "JSON",
            "jsonb" => "JSONB",
            other => {
                tracing::debug!(
                    zs_type = other,
                    "PgDialect::map_zs_type: unknown type — defaulting to TEXT"
                );
                "TEXT"
            }
        }
        .to_string()
    }

    /// PG's "now" function. PG also accepts `CURRENT_TIMESTAMP`, but
    /// `NOW()` is the idiomatic form used elsewhere in the codebase.
    fn now_fn(&self) -> &'static str {
        "NOW()"
    }

    // `last_insert_rowid_sql` defaults to `None` on the trait — PG
    // routes through `RETURNING id` instead. No override needed.
}

/// Direct `DialectBuilder` impl on `PostgresBackend` so consumers can
/// hold an `&PostgresBackend` and reach the dialect without naming a
/// separate field. The bodies delegate to the `PgDialect` ZST; rustc
/// inlines the value away because every method is `&self`.
impl DialectBuilder for PostgresBackend {
    #[cfg(any(test, feature = "test-helpers"))]
    fn sql_dialect(&self) -> crate::query::SqlDialect {
        PgDialect.sql_dialect()
    }

    fn quote_ident(&self, name: &str) -> String {
        PgDialect.quote_ident(name)
    }

    fn map_zs_type(&self, zs_type: &str, opts: &serde_json::Value) -> String {
        PgDialect.map_zs_type(zs_type, opts)
    }

    fn now_fn(&self) -> &'static str {
        PgDialect.now_fn()
    }

    fn last_insert_rowid_sql(&self) -> Option<&'static str> {
        PgDialect.last_insert_rowid_sql()
    }
}

// `Backend` is a pure composition marker -- every method
// lives on a sub-trait impl above.
#[cfg(any(test, feature = "test-helpers"))]
impl Backend for PostgresBackend {}

// ===========================================================================
// EncryptedColumn + Backup impls on PostgresBackend
// ===========================================================================
//
// Both impls are unconditional on the PG arm:
//   * `EncryptedColumn` -- PG key sourcing is in-process only, the same
//     `LocalKeySource` the SQLite arm uses. The database-backed variant
//     was deleted on 2026-08-27 with the admin schema it read.
//   * `Backup` -- the PITR placeholder writes to
//     `__zeroship_admin.pitr_targets`, a table with no installer since
//     that same deletion, so `pitr_replay` always errors. Snapshot /
//     restore do not touch it. The impl is `cfg(feature =
//     "test-helpers")` and nothing outside this crate reaches it, so
//     nothing observes the failure. Whether PITR gets a real home or is
//     deleted is an open operator decision -- it was NOT covered by the
//     2026-08-27 decisions that removed per-column keys and the durable
//     mask-policy store, so it is the one admin-schema reference this
//     crate still issues.

// Real `EncryptedColumn` body. Delegates to the workspace
// `crate::encryption::aead` module (mode-dispatch on encrypt; mode-
// agnostic on decrypt because the wire format carries the nonce). Key
// resolution goes through `self.key_store`, which reads this isolate's
// in-process root key source.
impl crate::backend::EncryptedColumn for PostgresBackend {
    type KeyHandle = crate::encryption::aead::AeadKey;

    async fn resolve_key(&self, app_id: &str, key_id: &str) -> Result<Self::KeyHandle, DbError> {
        self.key_store.resolve(app_id, key_id).await
    }

    fn encrypt(
        &self,
        key: &Self::KeyHandle,
        mode: crate::backend::EncryptionMode,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, DbError> {
        match mode {
            crate::backend::EncryptionMode::Randomised => {
                crate::encryption::aead::encrypt_randomised(key, plaintext, aad)
            }
            crate::backend::EncryptionMode::Deterministic => {
                crate::encryption::aead::encrypt_deterministic(key, plaintext, aad)
            }
        }
    }

    fn decrypt(
        &self,
        key: &Self::KeyHandle,
        _mode: crate::backend::EncryptionMode,
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, DbError> {
        // Decrypt is mode-agnostic: the wire format carries the nonce,
        // and AES-GCM verifies the tag regardless of how the nonce was
        // produced on the write side. The caller picks the
        // mode-appropriate AAD (Camp A: row_pk in AAD for Randomised,
        // omitted for Deterministic) — see
        // `crate::crud::encryption_pass`.
        crate::encryption::aead::decrypt(key, ciphertext, aad)
    }
}

// ===========================================================================
// Real `Backup` impl on PostgresBackend
// ===========================================================================
//
// `snapshot` → `pg_dump --schema=<app_id> --format=custom --no-owner
// --no-privileges`, stream the dump to a file URI, hash the bytes
// on the fly. `restore` → drop-and-recreate schema then `pg_restore`.
// `pitr_replay` → records the target row in `__zeroship_admin.pitr_targets`;
// the operator runs the actual recovery via `recovery.conf`.
//
// Notes:
//   1. The PITR placeholder writes to the `__zeroship_admin` schema,
//      which NO LONGER EXISTS - the auth/bootstrap subtree that
//      provisioned it was deleted on 2026-08-27. The INSERT below
//      therefore fails on every database. It is left in place because
//      picking a new home for PITR targets is a design decision, not a
//      deletion.
//   2. `Backup` is admin-tier surface — app code never reaches it. Nor does
//      anything else: there is no accessor and no consumer, so today the impl
//      is exercised only by this crate's tests through the trait. Whether the
//      capability ships or goes is an open operator decision, not a claim this
//      comment should keep making on its behalf.
//
// Both `pg_dump` and `pg_restore` need to be on `PATH` in the deployment
// environment. The integration tests `#[ignore]` themselves when the
// binaries aren't reachable so CI doesn't hard-fail on minimal images.
//
// The child process is driven via `std::process::Command` wrapped in
// `compio::runtime::spawn_blocking` — keeps `compio`'s workspace feature
// set unchanged (no `process` feature dep) and matches the shell-out
// pattern used elsewhere in the codebase (e.g. `sandbox-agent/src/exec.rs`).

#[cfg(feature = "test-helpers")]
impl crate::backend::Backup for PostgresBackend {
    async fn snapshot(
        &self,
        app_id: &str,
        dest_uri: &str,
        opts: crate::backend::SnapshotOpts,
    ) -> Result<crate::backend::SnapshotHandle, DbError> {
        backup_pg::snapshot_impl(self, app_id, dest_uri, opts).await
    }

    async fn restore(
        &self,
        app_id: &str,
        snapshot: &crate::backend::SnapshotHandle,
    ) -> Result<(), DbError> {
        backup_pg::restore_impl(self, app_id, snapshot).await
    }

    async fn pitr_replay(
        &self,
        app_id: &str,
        target: crate::backend::PitrTarget,
    ) -> Result<(), DbError> {
        backup_pg::pitr_replay_impl(self, app_id, target).await
    }
}

/// Inner module so the helpers stay grouped and the surrounding file
/// keeps the "thin trait facade + per-capability impl block" shape.
/// `pub(super)` so the trait methods above can call in; everything
/// else stays private.
#[cfg(feature = "test-helpers")]
mod backup_pg {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::PostgresBackend;
    use crate::backend::SNAPSHOT_RESTORE_LOCK_TAG;
    use crate::backend::{
        BusyPolicy, LockGuard, LockScope, PgLockManager, PitrTarget, SnapshotHandle, SnapshotOpts,
    };
    use zeroship_data_core::error::DbError;

    /// Parse a `file:///abs/path` URI into the underlying filesystem
    /// path. Returns a typed `Configuration` error for unsupported
    /// schemes (e.g. `s3://`) so the SDK can branch on `.code`.
    ///
    /// Only the `file://` scheme is supported -- S3 / R2 land alongside
    /// the production BlobStore wire-through later. The path
    /// is the on-disk address of the dump artifact; SHA-256 hashing
    /// happens on write (snapshot) and re-verification on read
    /// (restore). The handle's `uri` field echoes the caller-supplied
    /// string so dashboards and operators can correlate the snapshot
    /// back to wherever they asked for it to land.
    fn parse_dest_path(dest_uri: &str) -> Result<PathBuf, DbError> {
        if let Some(rest) = dest_uri.strip_prefix("file://") {
            // RFC 8089: `file:///abs/path` — the empty authority leaves
            // `rest` starting with `/`. We accept both `file:///x` and
            // `file://x` here since callers in tests sometimes elide
            // the empty authority.
            Ok(PathBuf::from(rest))
        } else if dest_uri.starts_with("s3://") || dest_uri.starts_with("https://") {
            Err(DbError::Configuration {
                code: "backup_dest_uri_unsupported",
                message: format!(
                    "snapshot destination URI {dest_uri:?} uses an unsupported scheme; \
                     PR 4 ships `file://` only — S3/HTTPS land alongside the production \
                     BlobStore wire-through in a later PR"
                ),
                hint: Some(
                    "use `file:///abs/path/to/snapshot.dump` in P5 PR 4; \
                     S3/R2 destinations are deferred"
                        .to_string(),
                ),
            })
        } else {
            // Treat anything else as a bare filesystem path so the
            // operator can pass either form. `file://` is the documented
            // shape per `SnapshotHandle::uri` rustdoc.
            Ok(PathBuf::from(dest_uri))
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Stream `path` through SHA-256 and return the 32-byte digest.
    /// Reads in 64 KiB chunks; runs entirely on the compio thread (no
    /// `spawn_blocking`) — the snapshot path is operator-driven and not
    /// hot enough to warrant offloading. The std-fs blocking reads
    /// dominate only for multi-GiB snapshots, at which point the whole
    /// op is already gated by pg_dump latency.
    fn sha256_file(path: &Path) -> Result<[u8; 32], std::io::Error> {
        use sha2::Digest;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = sha2::Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().into())
    }

    /// Classify a `pg_dump` / `pg_restore` failure by stderr substring
    /// match. The set of patterns is intentionally narrow — only the
    /// shapes the SDK needs to branch on get a coded variant; the rest
    /// pass through as `Internal` with the raw stderr in the message
    /// so the operator can read it from the log.
    fn classify_pg_tool_failure(op: &'static str, stderr: &str, if_busy: BusyPolicy) -> DbError {
        // `pg_dump: error: connection to server ... failed: …` —
        // transient infra issue. When the caller asked for Retry we
        // mark it retryable; otherwise propagate the same code so the
        // SDK can branch on `.code === "backup_busy"`.
        if stderr.contains("connection to server")
            || stderr.contains("connection to database failed")
            || stderr.contains("could not connect to server")
        {
            return DbError::Coded {
                code: "backup_busy".to_string(),
                message: format!(
                    "{op} could not reach Postgres: {}",
                    stderr.trim().lines().next().unwrap_or(stderr.trim())
                ),
                hint: match if_busy {
                    BusyPolicy::Retry => Some(
                        "transient — retry after the database accepts connections again"
                            .to_string(),
                    ),
                    BusyPolicy::Abort => None,
                },
            };
        }
        // `pg_dump: error: relation "<app>.<table>" does not exist` /
        // schema not found.
        if stderr.contains("does not exist") || stderr.contains("no matching schemas were found") {
            return DbError::Configuration {
                code: "snapshot_app_unknown",
                message: format!(
                    "{op}: source app schema is missing — {}",
                    stderr.trim().lines().next().unwrap_or(stderr.trim())
                ),
                hint: Some(
                    "verify the app_id matches a schema that exists on this database".to_string(),
                ),
            };
        }
        // Everything else: Internal with the raw stderr so the
        // operator can debug. We keep the message bounded so a verbose
        // `pg_restore --verbose` dump doesn't flood the JS console.
        let mut truncated = stderr.trim().to_string();
        if truncated.len() > 4096 {
            truncated.truncate(4096);
            truncated.push_str("\n…[truncated]");
        }
        DbError::Internal {
            message: format!("{op} failed: {truncated}"),
        }
    }

    /// Run a `pg_dump` / `pg_restore` subprocess on a blocking worker
    /// and return its output. The connection URL is passed via the
    /// `--dbname=` long arg so it doesn't show up in `ps` output
    /// (modern pg tools mask the password component, but we still
    /// prefer the explicit form).
    async fn run_pg_tool(
        binary: &'static str,
        args: Vec<String>,
    ) -> Result<std::process::Output, std::io::Error> {
        compio::runtime::spawn_blocking(move || {
            std::process::Command::new(binary).args(&args).output()
        })
        .await
        .map_err(|_| std::io::Error::other(format!("{binary}: spawn_blocking task panicked")))?
    }

    pub(super) async fn snapshot_impl(
        backend: &PostgresBackend,
        app_id: &str,
        dest_uri: &str,
        opts: SnapshotOpts,
    ) -> Result<SnapshotHandle, DbError> {
        // Pre-flight: hold the per-app snapshot/restore lock for the duration
        // of pg_dump so another backup operation cannot replace the database
        // while we capture it. Contention surfaces as
        // `migration_in_progress` regardless of `opts.if_busy` — the
        // SDK branches on `.code` and the caller chooses to retry.
        let lock_client = backend.acquire_pooled_client_for_lock().await?;
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: SNAPSHOT_RESTORE_LOCK_TAG.to_string(),
        };
        let guard = match LockGuard::acquire(backend, lock_client, &scope).await {
            Ok(g) => g,
            Err(DbError::LockContention { message }) => {
                return Err(DbError::Coded {
                    code: "migration_in_progress".to_string(),
                    message: format!(
                        "snapshot: another deploy / migration is in progress for app {app_id:?}: \
                         {message}"
                    ),
                    hint: Some(
                        "retry the snapshot once the in-flight snapshot / restore completes"
                            .to_string(),
                    ),
                });
            }
            Err(other) => return Err(other),
        };

        // Parse destination + ensure parent dir exists.
        let dest_path = parse_dest_path(dest_uri)?;
        if let Some(parent) = dest_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| DbError::Internal {
                    message: format!("snapshot: create parent dir {parent:?} failed: {e}"),
                })?;
            }
        }

        // pg_dump invocation. `--no-owner --no-privileges` makes the
        // dump restore-portable across operator roles; `--format=custom`
        // is the input format `pg_restore` consumes.
        let url = backend.url().to_string();
        let app_id_owned = app_id.to_string();
        let dest_path_str = dest_path.to_string_lossy().into_owned();
        let args = vec![
            format!("--dbname={url}"),
            format!("--schema={app_id_owned}"),
            "--format=custom".to_string(),
            "--no-owner".to_string(),
            "--no-privileges".to_string(),
            format!("--file={dest_path_str}"),
        ];

        let output = match run_pg_tool("pg_dump", args).await {
            Ok(o) => o,
            Err(e) => {
                // Best-effort lock release on Err. Drop logs a leak
                // notice on failure; the session-scoped lock auto-
                // releases when the pool recycles the connection.
                let _ = guard.release().await;
                return Err(DbError::Configuration {
                    code: "pg_dump_unavailable",
                    message: format!("pg_dump spawn failed: {e}"),
                    hint: Some(
                        "ensure `pg_dump` is on PATH in the deployment environment \
                         (matches the server major version)"
                            .to_string(),
                    ),
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let _ = guard.release().await;
            // Cleanup partial file so a re-run doesn't see stale bytes.
            let _ = std::fs::remove_file(&dest_path);
            return Err(classify_pg_tool_failure("pg_dump", &stderr, opts.if_busy));
        }

        // Compute the content hash over the persisted file.
        let content_hash = match sha256_file(&dest_path) {
            Ok(h) => h,
            Err(e) => {
                let _ = guard.release().await;
                let _ = std::fs::remove_file(&dest_path);
                return Err(DbError::Internal {
                    message: format!("snapshot: SHA-256 of {dest_path_str:?} failed: {e}"),
                });
            }
        };

        // Release the lock now that the dump is committed to disk.
        // From this point onwards concurrent deploys can proceed; the
        // returned handle is enough for a later restore to verify
        // integrity independent of the live schema.
        let _ = guard.release().await;

        Ok(SnapshotHandle {
            uri: dest_uri.to_string(),
            content_hash,
            created_at_ms: now_ms(),
        })
    }

    pub(super) async fn restore_impl(
        backend: &PostgresBackend,
        app_id: &str,
        snapshot: &SnapshotHandle,
    ) -> Result<(), DbError> {
        // Hold the per-app snapshot/restore lock for the whole restore so
        // another backup operation cannot observe the drop-and-recreate
        // sequence.
        let lock_client = backend.acquire_pooled_client_for_lock().await?;
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: SNAPSHOT_RESTORE_LOCK_TAG.to_string(),
        };
        let guard = match LockGuard::acquire(backend, lock_client, &scope).await {
            Ok(g) => g,
            Err(DbError::LockContention { message }) => {
                return Err(DbError::Coded {
                    code: "migration_in_progress".to_string(),
                    message: format!(
                        "restore: another deploy / migration is in progress for app {app_id:?}: \
                         {message}"
                    ),
                    hint: Some(
                        "retry the restore once the in-flight snapshot / restore completes"
                            .to_string(),
                    ),
                });
            }
            Err(other) => return Err(other),
        };

        // Resolve the on-disk path; only file:// is supported.
        let src_path = match parse_dest_path(&snapshot.uri) {
            Ok(p) => p,
            Err(e) => {
                let _ = guard.release().await;
                return Err(e);
            }
        };

        // Verify the content hash matches what `snapshot` recorded.
        // A mismatch means either the file was truncated/corrupted in
        // transit or the operator pointed us at the wrong dump. Refuse
        // before touching the live schema.
        match sha256_file(&src_path) {
            Ok(observed) if observed == snapshot.content_hash => { /* ok */ }
            Ok(_) => {
                let _ = guard.release().await;
                return Err(DbError::Coded {
                    code: "snapshot_hash_mismatch".to_string(),
                    message: format!(
                        "restore: SHA-256 of {:?} does not match the SnapshotHandle's \
                         recorded hash — snapshot is corrupt or this is the wrong file",
                        src_path
                    ),
                    hint: Some(
                        "re-fetch the snapshot from the original source; do NOT run \
                         restore against a dump whose hash has drifted"
                            .to_string(),
                    ),
                });
            }
            Err(e) => {
                let _ = guard.release().await;
                return Err(DbError::Internal {
                    message: format!("restore: SHA-256 of {src_path:?} failed: {e}"),
                });
            }
        }

        // Drop the live schema so pg_restore can rebuild it from
        // the dump's TOC. This is a simplification of the
        // load-bearing safety step: a full `swap_schema_atomic`
        // SECURITY DEFINER function is the hardened replacement. The
        // simple sequence is destructive — if pg_restore fails after the
        // DROP, the schema is gone and the operator has to re-
        // restore. The lock above keeps concurrent deploys out of
        // the window; the snapshot hash above keeps wrong dumps out.
        //
        // We DROP but do NOT pre-CREATE the schema: pg_dump's custom
        // format emits its own `CREATE SCHEMA "<app_id>"` statement
        // in the TOC, and pre-creating would trip pg_restore with
        // `schema … already exists`. The CASCADE drop kills the
        // schema's tables / indexes / sequences; pg_restore rebuilds
        // the full graph (schema + objects).
        //
        // Run via the pool (not the locked client) so a SQL error
        // doesn't drop the lock. We use raw quoted identifiers; app
        // ids reaching this surface are platform-controlled (typed_id
        // entity prefixes), not user input.
        let drop_sql = format!(r#"DROP SCHEMA IF EXISTS "{app_id}" CASCADE"#);
        let empty: Vec<&str> = Vec::new();
        if let Err(e) = backend.pool().query_text_params(&drop_sql, &empty).await {
            let _ = guard.release().await;
            return Err(DbError::Internal {
                message: format!(
                    "restore: DROP SCHEMA failed: {}",
                    crate::backend::pg_error::classify(&e)
                ),
            });
        }

        // pg_restore. The dump's TOC includes a `CREATE SCHEMA` so
        // we don't pre-create. `--no-owner --no-privileges` matches
        // the dump-side flags so role-rewriting doesn't trip.
        let url = backend.url().to_string();
        let src_path_str = src_path.to_string_lossy().into_owned();
        let args = vec![
            format!("--dbname={url}"),
            "--no-owner".to_string(),
            "--no-privileges".to_string(),
            "--exit-on-error".to_string(),
            src_path_str.clone(),
        ];

        let output = match run_pg_tool("pg_restore", args).await {
            Ok(o) => o,
            Err(e) => {
                let _ = guard.release().await;
                return Err(DbError::Configuration {
                    code: "pg_restore_unavailable",
                    message: format!("pg_restore spawn failed: {e}"),
                    hint: Some(
                        "ensure `pg_restore` is on PATH in the deployment environment \
                         (matches the server major version)"
                            .to_string(),
                    ),
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let _ = guard.release().await;
            // The schema is empty at this point — the operator needs
            // to know that the partial restore left no data.
            let base = classify_pg_tool_failure("pg_restore", &stderr, BusyPolicy::Abort);
            return Err(match base {
                DbError::Internal { message } => DbError::Coded {
                    code: "restore_failed".to_string(),
                    message: format!(
                        "{message}\n\
                         NOTE: schema {app_id:?} is EMPTY after partial restore — \
                         operator must re-run restore to reconstruct state"
                    ),
                    hint: Some(
                        "P6a hardening lands the swap_schema_atomic SECURITY DEFINER \
                         function that makes the restore non-destructive on failure"
                            .to_string(),
                    ),
                },
                other => other,
            });
        }

        let _ = guard.release().await;
        Ok(())
    }

    pub(super) async fn pitr_replay_impl(
        backend: &PostgresBackend,
        app_id: &str,
        target: PitrTarget,
    ) -> Result<(), DbError> {
        // This ships the API surface only. PITR replay requires
        // PG-server-level configuration (recovery.conf,
        // archive_command); the platform can't initiate WAL replay
        // from a client connection. We record the target so the
        // operator dashboard / maintenance cron can surface
        // "PITR recovery pending for <app_id>"; the operator runs
        // the actual recovery out-of-band.
        let target_str = match &target {
            PitrTarget::Lsn(s) => format!("LSN:{s}"),
            PitrTarget::TimeMillis(ms) => format!("TIME_MS:{ms}"),
        };
        let sql = "INSERT INTO __zeroship_admin.pitr_targets \
                   (app_id, target, recorded_at) \
                   VALUES ($1, $2, NOW()) \
                   ON CONFLICT (app_id) DO UPDATE \
                     SET target = EXCLUDED.target, \
                         recorded_at = EXCLUDED.recorded_at";
        backend
            .pool()
            .query_text_params(sql, &[app_id, target_str.as_str()])
            .await
            .map_err(|e| crate::backend::pg_error::classify(&e))?;
        Ok(())
    }
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
pub(crate) fn terminal_from_tag(intent: SettleIntent, tag: Option<&str>) -> TerminalResult {
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
pub(crate) async fn cleanup(client: &compio_postgres::Client) -> CleanupAck {
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
pub(crate) const fn terminal_from_status(
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
    //! its per-capability impls (`SqlExecutor` / `LockManager` /
    //! `SchemaIntrospect`) either calls the `Rc<Pool>` directly or
    //! forwards into [`crate::diff`] / [`crate::query`] free functions.
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
    //! AND a real `Client`; they're exercised by `tests/integration.rs`.
    //!
    //! That leaves *compile-time* tests as the highest-signal coverage
    //! we can add in `--lib`:
    //!
    //! 1. `PostgresBackend: Backend` — proves the trait impl is wired
    //!    up so any future bound change to `Backend` (adding a method,
    //!    tightening a lifetime, swapping an associated type) fails
    //!    compilation here, not at a distant call site.
    //! 2. Associated-type identities — pin `Client = compio_postgres::OwnedPooledClient`
    //!    and `LiveSchema = crate::diff::LiveSchema` so a refactor that
    //!    accidentally swaps either is caught here.
    //! 3. The `Backend: 'static` bound on the trait — re-asserted at
    //!    the impl site.
    //!
    //! These are runtime no-ops (the bodies never execute) — they exist
    //! so `cargo build -p zeroship-plugin-db --tests` fails fast on a
    //! seam break.

    use super::*;
    use crate::backend::{
        Backend, DialectBuilder, LockManager, PgLockManager, PgSqlExecutor, SchemaIntrospect,
        SqlExecutor,
    };

    /// Compile-time: `PostgresBackend` must satisfy the `Backend` trait
    /// (a pure composition marker over five sub-traits).
    /// The function is never called; the bound is checked at type-check
    /// time.
    fn assert_postgres_backend_impls_backend() {
        fn assert_impl<T: Backend>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: each carved capability trait is impl'd directly on
    /// `PostgresBackend` (not just visible through the
    /// `Backend` super-bound). A regression that pulls one back onto
    /// the omnibus trait or detaches the impl block fails here at
    /// build time.
    fn assert_postgres_backend_impls_sub_traits() {
        fn impls_sql_executor<T: SqlExecutor<Client = compio_postgres::OwnedPooledClient>>() {}
        fn impls_lock_manager<T: LockManager<Client = compio_postgres::OwnedPooledClient>>() {}
        fn impls_schema_introspect<T: SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>>() {}
        fn impls_pg_sql_executor<T: PgSqlExecutor>() {}
        fn impls_pg_lock_manager<T: PgLockManager>() {}
        impls_sql_executor::<PostgresBackend>();
        impls_lock_manager::<PostgresBackend>();
        impls_schema_introspect::<PostgresBackend>();
        impls_pg_sql_executor::<PostgresBackend>();
        impls_pg_lock_manager::<PostgresBackend>();

        // `DialectBuilder` impl lands directly on the backend
        // (not on the `Backend` super-trait -- the trait composition
        // stays unchanged). The bound here pins the impl so a future
        // refactor that detaches the impl block fails at type-check.
        fn impls_dialect_builder<T: DialectBuilder>() {}
        impls_dialect_builder::<PostgresBackend>();
    }

    // ---------------------------------------------------------------------
    // PgDialect hook unit tests. ZST has no I/O -- each test
    // is a string-compare against the expected SQL fragment.
    // ---------------------------------------------------------------------

    #[test]
    fn pg_dialect_quote_ident_doubles_embedded_quote() {
        let d = PgDialect;
        assert_eq!(d.quote_ident("plain"), "\"plain\"");
        assert_eq!(d.quote_ident("with\"quote"), "\"with\"\"quote\"");
    }

    #[test]
    fn pg_dialect_map_zs_type_covers_p1_vocabulary() {
        let d = PgDialect;
        let no_opts = serde_json::json!({});
        assert_eq!(d.map_zs_type("text", &no_opts), "TEXT");
        assert_eq!(d.map_zs_type("bigint", &no_opts), "BIGINT");
        assert_eq!(d.map_zs_type("int8", &no_opts), "BIGINT");
        assert_eq!(d.map_zs_type("integer", &no_opts), "INTEGER");
        assert_eq!(d.map_zs_type("int4", &no_opts), "INTEGER");
        assert_eq!(d.map_zs_type("double", &no_opts), "DOUBLE PRECISION");
        assert_eq!(d.map_zs_type("real", &no_opts), "REAL");
        assert_eq!(d.map_zs_type("bytes", &no_opts), "BYTEA");
        assert_eq!(d.map_zs_type("blob", &no_opts), "BYTEA");
        assert_eq!(d.map_zs_type("numeric", &no_opts), "NUMERIC");
        assert_eq!(d.map_zs_type("decimal", &no_opts), "NUMERIC");
        assert_eq!(d.map_zs_type("boolean", &no_opts), "BOOLEAN");
        assert_eq!(d.map_zs_type("timestamp", &no_opts), "TIMESTAMP");
        assert_eq!(d.map_zs_type("timestamptz", &no_opts), "TIMESTAMPTZ");
        assert_eq!(d.map_zs_type("json", &no_opts), "JSON");
        assert_eq!(d.map_zs_type("jsonb", &no_opts), "JSONB");
        // Unknown types fall through to TEXT.
        assert_eq!(d.map_zs_type("nonsense_type", &no_opts), "TEXT");
    }

    #[test]
    fn pg_dialect_now_fn_and_last_insert_rowid() {
        let d = PgDialect;
        assert_eq!(d.now_fn(), "NOW()");
        // PG routes through `RETURNING id` for last-inserted rowid —
        // the trait default of `None` is the correct PG shape.
        assert_eq!(d.last_insert_rowid_sql(), None);
    }

    /// Compile-time: the associated types must remain wired to the
    /// concrete `compio_postgres` / `crate::diff` types. Swapping
    /// either accidentally would silently change the `B::Client` /
    /// `B::LiveSchema` shape every consumer sees. `LiveSchema` is owned
    /// by [`SchemaIntrospect`] -- the `Backend` super-bound
    /// `SchemaIntrospect<LiveSchema = LiveSchema>` re-anchors it so
    /// `Backend<LiveSchema = …>` still resolves here.
    fn assert_postgres_backend_assoc_types() {
        fn same_client<T: Backend<Client = compio_postgres::OwnedPooledClient>>() {}
        fn same_live_schema<T: Backend<LiveSchema = crate::diff::LiveSchema>>() {}
        same_client::<PostgresBackend>();
        same_live_schema::<PostgresBackend>();
    }

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
        let src = include_str!("postgres.rs");
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
        let _ = assert_postgres_backend_impls_backend as fn();
        let _ = assert_postgres_backend_impls_sub_traits as fn();
        let _ = assert_postgres_backend_assoc_types as fn();
        let _ = assert_postgres_backend_is_static as fn();
    }
}
#[cfg(test)]
mod terminal_projection_tests {
    use super::{terminal_from_status, terminal_from_tag};
    use compio_postgres::TransactionStatus;
    use zeroship_data_core::error::{SettleIntent, TerminalResult};

    /// **L8, without a database.** PostgreSQL answers `COMMIT` with the tag
    /// `ROLLBACK` when the transaction is in the failed state, and reading that
    /// as success reports discarded writes as durable.
    ///
    /// This rule was only reachable through a live server until 2026-09-02,
    /// when the projection was split out of `terminal`. The live arm that
    /// covered it - `commit_that_postgres_rolled_back_must_not_report_success_l8`
    /// in `tests/native_transaction.rs` - had ALSO been failing for an unrelated
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
