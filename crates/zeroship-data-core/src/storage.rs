//! The storage-capability traits: what a backend must be able to do.
//!
//! Eight traits, carved out of what used to be one monolithic `Backend`. Each
//! is a CONTRACT and nothing more - an associated type or two, and method
//! signatures over the vocabulary in [`crate::capability`]. None of them names
//! a database driver, a runtime or V8, and none carries a default body that
//! does; that is what rank 0 means, and it is why they can sit below every
//! tier that implements them.
//!
//! Moved from `zeroship-plugin-db`'s `backend/mod.rs` on 2026-09-02, after the
//! two things that would have made the move unbuildable were dealt with first:
//!
//! * `LockManager::try_acquire_with_backoff` was a DEFAULT METHOD BODY holding
//!   a `compio::time::sleep` schedule. A default body travels with its trait,
//!   so this would have put an async executor in a crate that declares no
//!   runtime. It is now `zeroship-plugin-db`'s `lock_policy::BoundedLockAcquire`
//!   extension trait, blanket-implemented over every `LockManager`.
//! * `ChangeStream` used to hand out `BrokerPauseGuard` and
//!   `SchemaPendingGuard`, whose `Drop` impls drive the engine's broker
//!   registries - a `data-core -> data-engine` edge against the dependency that
//!   already runs the other way. Those two methods were deleted rather than
//!   moved: all four implementations were the same two lines and ignored
//!   `self`, so they were never vendor behaviour at all.
//!
//! # Implementations stay above
//!
//! Every `impl Trait for T` lives in the crate that owns `T` - nineteen of them
//! across `zeroship-plugin-db` today, including two on its `BackendHandle`
//! dispatch enum. The orphan rule permits exactly that shape: a local type may
//! implement a foreign trait.


use zeroship_schema::descriptors::{GeoPoint, VectorMetric};
#[cfg(feature = "test-helpers")]
use zeroship_schema::query::SqlDialect;

use crate::binding::DbBinding;
use crate::capability::LockScope;
// `Backup` carries `#[cfg(feature = "test-helpers")]` and is the only consumer
// of these three, so the import carries the same gate. An ungated import here
// names items that do not exist in a default build.
#[cfg(feature = "test-helpers")]
use crate::capability::{PitrTarget, SnapshotHandle, SnapshotOpts};
use crate::error::DbError;

/// SQL execution capability — the "connection lifecycle + run a
/// statement" slice of the data-store boundary.
///
/// Carved out of the monolithic `Backend` trait (see
/// `docs/archive/p0-implementation-plan.md` and the
/// converged design at `docs/archive/db-system-design.md` §7).
/// Consumer bounds narrow onto this trait (and [`LockManager`])
/// instead of the omnibus `Backend` super-trait. That carve is
/// finished: nothing takes `dyn Backend`, and the `&PostgresBackend`
/// parameters that remain are the deliberate Postgres-only accessors
/// on [`BackendHandle`] (`as_postgres`, `as_encrypted_column_pg`)
/// plus their private callers. Those sit
/// outside the capability traits by design - replication and WAL
/// consumption are Postgres-specific - rather than being a migration
/// someone left half-done.
///
/// Not `Send + Sync` on purpose — the compio runtime is
/// single-threaded per worker, so we don't pay for atomics or
/// thread-safety bounds we don't use (Open Q4 in the implementation
/// plan).
pub trait SqlExecutor: 'static {
    /// Concrete connection / client handle. The orchestrator threads
    /// this through audit helpers and advisory-lock acquisition without
    /// naming the underlying SQL driver.
    type Client;

    /// Acquire a dedicated (non-pooled) connection for `app_id`. Caller owns
    /// the lifetime — used by the migration lock and the native
    /// `db.transaction(fn)` orchestrator, which need a
    /// connection that survives across pool-return points.
    ///
    /// **`app_id` is the admission key, not a label.** SC-1 admits one
    /// top-level transaction per `(runtime_instance_id, app_id)`, and a backend
    /// that cannot see the app cannot enforce that key: the SQLite arm handed
    /// out one shared transaction connection and so refused app B while app A
    /// held one (defect L22b). Postgres checks out from a pool and needs no
    /// per-app routing, so it ignores the argument; that asymmetry is the
    /// point, not an oversight.
    ///
    /// For Postgres this is a pooled checkout; for SQLite it opens (or reuses)
    /// that app's own transaction connection.
    #[allow(async_fn_in_trait)]
    async fn acquire_dedicated_client(&self, app_id: &str) -> Result<Self::Client, DbError>;

    /// Execute a SQL statement against the pool with text-encoded
    /// parameters. Returns the count of affected rows (read paths
    /// usually ignore the return).
    ///
    /// Implementations free to fan out to a connection pool internally
    /// — this is the "no transaction, no lock client, just run it"
    /// path. The orchestrator and audit helpers do not park clients
    /// across awaits when they use this.
    #[allow(async_fn_in_trait)]
    async fn pool_exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError>;

    /// Execute a parameterless, possibly multi-statement DDL script
    /// against the pool.
    ///
    /// CREATE TABLE emission bundles the table definition with its
    /// implicit system-field indexes (and, on PG, `COMMENT ON COLUMN`
    /// mask sentinels) into one `;`-separated script — see
    /// [`crate::query::build_create_table_with_fks_for_dialect`]. The
    /// Postgres extended/prepared protocol used by [`Self::pool_exec`]
    /// (`query_text_params` issues `Parse`/`Bind`/`Execute`) rejects
    /// multi-statement strings with `cannot insert multiple commands
    /// into a prepared statement`, so DDL must ride the **simple query
    /// protocol** (`batch_execute`) instead, which executes a sequence
    /// of `;`-separated statements in one implicit transaction.
    ///
    /// The default impl forwards to [`Self::pool_exec`] — correct for
    /// the SQLite arm (whose `pool_exec` routes through `sqlite3_exec`,
    /// natively multi-statement) and for mocks. The Postgres backend
    /// overrides it to use `batch_execute`.
    #[cfg(feature = "test-helpers")]
    #[allow(async_fn_in_trait)]
    async fn pool_exec_ddl(&self, sql: &str) -> Result<(), DbError> {
        self.pool_exec(sql, &[]).await.map(|_| ())
    }

    /// Execute a SQL statement against a specific client.
    ///
    /// Used by the migration lock + apply-pass paths that need every
    /// statement to land on the same backend session as the advisory
    /// lock. Returns the count of affected rows.
    #[allow(async_fn_in_trait)]
    async fn client_exec(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[&str],
    ) -> Result<u64, DbError>;
}

/// Advisory-lock capability — session-scoped `(key1, key2)` locks held
/// on a [`SqlExecutor::Client`].
///
/// Carved out of the monolithic `Backend` trait (see
/// `docs/archive/p0-implementation-plan.md` and
/// `docs/archive/db-system-design.md` §7). The `: SqlExecutor`
/// super-bound is load-bearing — every method takes a `&Self::Client`
/// and that associated type lives on [`SqlExecutor`].
///
/// **This trait is a CONTRACT and carries no policy.** Everything below is
/// either a primitive a backend must supply or a wrapper that does nothing but
/// derive `(key1, key2)` from a [`LockScope`]. Nothing here reaches for a
/// runtime, which is what makes it rank-0 vocabulary bound for
/// `zeroship-data-core`.
///
/// The bounded-retry acquisition policy - the schedule, the
/// `compio::time::sleep` between attempts, the per-retry tracing - is
/// [`BoundedLockAcquire`](crate::lock_policy::BoundedLockAcquire), blanket-
/// implemented for every `LockManager`. It lived here as a DEFAULT METHOD BODY
/// until 2026-09-02, and a default body travels with its trait: moving this
/// trait down would have carried an async executor into a crate that declares
/// no runtime. Call `acquire` with `use crate::lock_policy::BoundedLockAcquire;`
/// in scope.
///
/// **Two-tier surface**:
///
/// - The **typed API** ([`Self::try_acquire`] / [`Self::release`], plus
///   `BoundedLockAcquire::acquire`) takes a [`LockScope`] enum. This is the
///   shape every new call site should adopt — it carries an explicit
///   classification of the lock's visibility (cluster-wide vs in-process) and
///   centralises key derivation per §10.
///
/// - The **legacy string-key API**
///   ([`Self::acquire_advisory_lock`] /
///   [`Self::try_acquire_advisory_lock`] /
///   [`Self::release_advisory_lock`]) takes raw `(key1, key2)`
///   strings. The `try_*` / `release_*` halves remain the underlying
///   primitives the typed API dispatches through, and the PG impl's
///   `hashtext()` SQL lives at this layer. `acquire_advisory_lock`
///   itself is **no longer routed through** by the typed surface —
///   a security fix replaced the indefinite-wait dispatch with the bounded
///   retry loop, since the indefinite wait let a malicious app holding its own
///   lock stall every other caller. All three legacy methods stay
///   `#[doc(hidden)]`; eager removal of `acquire_advisory_lock` is unblocked
///   but not done.
pub trait LockManager: SqlExecutor {
    /// Try to acquire a session-scoped advisory lock for the given
    /// [`LockScope`]; `Ok(false)` if another holder already owns it.
    /// Typed wrapper over [`Self::try_acquire_advisory_lock`].
    ///
    /// Takes `&LockScope` — see [`Self::acquire`].
    #[allow(async_fn_in_trait)]
    async fn try_acquire(&self, client: &Self::Client, scope: &LockScope) -> Result<bool, DbError> {
        let (k1, k2) = scope.to_keys();
        self.try_acquire_advisory_lock(client, &k1, &k2).await
    }

    /// Release a session-scoped advisory lock previously acquired via
    /// [`Self::acquire`] / [`Self::try_acquire`]. Typed wrapper over
    /// [`Self::release_advisory_lock`].
    ///
    /// Takes `&LockScope` so the release site can reuse the same
    /// binding the acquisition used — the §10.5 key-derivation
    /// invariant lives in the single
    /// `LockScope` value, not in textual identity across two struct
    /// literals.
    #[allow(async_fn_in_trait)]
    async fn release(&self, client: &Self::Client, scope: &LockScope) -> Result<(), DbError> {
        let (k1, k2) = scope.to_keys();
        self.release_advisory_lock(client, &k1, &k2).await
    }

    /// **Legacy string-key primitive — DO NOT CALL FROM NEW CODE.**
    /// Acquire a session-scoped advisory lock on `(key1, key2)` against
    /// the given client. Blocks (server-side, indefinitely) if another
    /// holder exists; the lock releases when the client is dropped or
    /// the backend session ends.
    ///
    /// Postgres maps this to
    /// `SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)`.
    /// Future backends would map to their per-engine equivalent (e.g.
    /// sqlite has no advisory locks — that backend would need a
    /// `BEGIN EXCLUSIVE` or a sentinel table).
    ///
    /// **Security**: the indefinite-wait shape is a within-app DoS
    /// vector: a malicious app holding its own session-scoped advisory lock
    /// stalls every subsequent operation using that scope. The typed surface no
    /// longer dispatches through this method; it routes via
    /// [`BoundedLockAcquire::try_acquire_with_backoff`](crate::lock_policy::BoundedLockAcquire::try_acquire_with_backoff)
    /// instead. This method is
    /// retained as the trait primitive only because (a) some future
    /// backend may want to expose the indefinite-wait shape behind a
    /// feature gate, and (b) the integration test
    /// `b1_advisory_lock_prevents_concurrent_runs` at
    /// `tests/integration.rs` still calls `pg_advisory_lock` SQL
    /// directly to exercise the contended branch. No production
    /// caller invokes it.
    ///
    /// Prefer
    /// [`BoundedLockAcquire::acquire`](crate::lock_policy::BoundedLockAcquire::acquire)
    /// at every call site.
    #[doc(hidden)]
    #[allow(async_fn_in_trait)]
    #[allow(
        dead_code,
        reason = "The blocking advisory-lock primitive is retained for lock-manager tests; production code routes through try_acquire/backoff."
    )]
    async fn acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError>;

    /// **Legacy string-key primitive**: try to acquire the same
    /// session-scoped advisory lock; return `Ok(false)` if the lock
    /// is already held by a different session, so a second acquirer
    /// observes "already held" instead of blocking.
    ///
    /// Prefer [`Self::try_acquire`] at new call sites.
    #[doc(hidden)]
    #[allow(async_fn_in_trait)]
    async fn try_acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<bool, DbError>;

    /// **Legacy string-key primitive**: release a session-scoped
    /// advisory lock. The lock auto-releases on session end, so
    /// callers can treat an `Err` as observability-only
    /// (warn-and-continue) — but returning the typed error lets them
    /// emit a structured log instead of silently swallowing it.
    /// Mirrors the pattern `LockGuard::release` adopted.
    ///
    /// Prefer [`Self::release`] at new call sites.
    #[doc(hidden)]
    #[allow(async_fn_in_trait)]
    async fn release_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError>;
}

/// Live-schema introspection capability — "read the catalog and return
/// a typed snapshot the diff engine can consume".
///
/// Carved out of the monolithic `Backend` trait (see
/// `docs/archive/p0-implementation-plan.md` and
/// `docs/archive/db-system-design.md` §7). The trait owns the
/// `LiveSchema` associated type that used to live on `Backend` —
/// pinning it here means test helpers and conformance assertions use a narrow
/// capability trait instead of the omnibus super-trait. `Backend` re-anchors the same
/// associated type via the `SchemaIntrospect<LiveSchema = LiveSchema>`
/// super-bound below so the constraint is unchanged for existing
/// callers.
///
/// **UNGATED, and it was `#[cfg(feature = "test-helpers")]` until 2026-09-04.**
/// The gate was correct while the only consumers were conformance assertions
/// and the migration-facing diff. It stopped being correct the moment
/// `zeroship_data_engine::crud::protection_floor` made a catalog read a
/// PRODUCTION SECURITY FENCE on the write path: the floor refuses a write whose
/// descriptor dropped a mask or an encryption block the database still records,
/// and it recovers those records through this trait. A fence that compiles only
/// into test builds is not a fence, so the capability ships.
///
/// It cost nothing to ship. Both impls read their own vendor's catalog with the
/// driver the crate already depends on; ungating pulled in no new dependency in
/// either tier. What stays gated is what is genuinely test-only: `Backup` (and
/// the `sha2` it hashes dumps with), `PgSqlExecutor`'s raw-pool escape hatch,
/// and the `Backend` conformance marker. Those three are code SPANS and not
/// intra-doc links on purpose - each is cfg-gated out of a default build, and
/// `tests/run_doc_gate.sh` requires zero unresolved links in the default and
/// `--all-features` doc builds alike, so a link here would be red in one of
/// them whichever way it was written.
pub trait SchemaIntrospect: 'static {
    /// Concrete live-schema snapshot returned by
    /// [`Self::introspect_schema`]. The Postgres impl uses
    /// [`zeroship_schema::diff::LiveSchema`]; each vendor tier populates that
    /// neutral shape from its own catalog.
    ///
    /// Both links said `crate::diff::…` until 2026-09-04 and resolved to
    /// nothing: this crate has no `diff` module, and never has - the path is a
    /// leftover from when these traits lived in `zeroship-plugin-db`. It went
    /// unnoticed because the trait was `cfg(feature = "test-helpers")`, so a
    /// DEFAULT doc build never rendered it; ungating the trait is what put the
    /// two dead links in front of `tests/run_doc_gate.sh`.
    type LiveSchema;

    /// Introspect the live schema for an app. Returns the typed
    /// snapshot the diff engine consumes via
    /// [`zeroship_schema::diff::compute_diff`].
    #[allow(async_fn_in_trait)]
    async fn introspect_schema(&self, app_id: &str) -> Result<Self::LiveSchema, DbError>;

    /// Estimate the row count for a single collection. Used by the
    /// classifier to decide "ADD NOT NULL on empty table" — cheap
    /// `reltuples`-style estimate is fine.
    #[allow(async_fn_in_trait)]
    async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError>;
}

/// SQL-dialect strategy — the seam every per-engine SQL-string
/// builder route through.
///
/// See `docs/archive/p1-sqlite-implementation-plan.md` §5. The six
/// methods listed below are the minimum-viable hook set; additional
/// hooks (RETURNING/upsert/JSON/vector) fill in alongside
/// the consumers that need them.
///
/// **No production caller yet** — the trait + ZST impls
/// (`SqliteDialect` here; `PgDialect` in the PG arm) exist so the
/// `query.rs` free-function builders can be retargeted onto a
/// dialect-typed entry point without re-shaping their call sites.
/// Until that retarget lands, `quote_ident` etc. continue to
/// live as free `quote_ident_pg(...)`-style functions inside `query.rs`.
///
/// **Why on the backend, not on `SqlExecutor`**: dialect choice is a
/// property of the *engine*, not the connection — a future PG-replica
/// backend would re-use [`crate::backend::PostgresBackend`]'s pool +
/// `SqlExecutor` impl but share a single `PgDialect`. Pinning
/// `DialectBuilder` as its own trait (and composing into the
/// per-backend struct) is the canonical shape.
pub trait DialectBuilder: 'static {
    /// Concrete SQL dialect this builder targets.
    #[cfg(feature = "test-helpers")]
    fn sql_dialect(&self) -> SqlDialect;

    /// Quote an identifier (column / table / schema name) per the
    /// engine's lexical rules. PG: doubled `"`; SQLite: doubled `"`
    /// with embedded-NUL rejection.
    fn quote_ident(&self, name: &str) -> String;

    /// Map a Zeroship-level type string (`"string"`, `"int"`,
    /// `"timestamp"`, …) to the engine's column-type vocabulary.
    /// `opts` is the per-field option object the SDK passes alongside
    /// the type (e.g. `{ length: 256 }`).
    #[allow(
        dead_code,
        reason = "These dialect hooks are still covered by unit/integration tests while the production query builders route through free functions."
    )]
    fn map_zs_type(&self, zs_type: &str, opts: &serde_json::Value) -> String;

    /// SQL fragment that evaluates to "now" on the server. PG: `NOW()`;
    /// SQLite: `CURRENT_TIMESTAMP`. Returned as a `&'static str` so
    /// callers can splice it into a query string without an alloc.
    #[allow(
        dead_code,
        reason = "These dialect hooks are still covered by unit/integration tests while the production query builders route through free functions."
    )]
    fn now_fn(&self) -> &'static str;

    /// Engine-side SQL that returns the last-inserted rowid for a
    /// non-RETURNING insert, if the engine supports the concept.
    /// PG returns `None` (it routes through `RETURNING` instead).
    /// SQLite returns `Some("SELECT last_insert_rowid()")`. Default
    /// `None` so the PG impl doesn't need to override.
    #[allow(
        dead_code,
        reason = "These dialect hooks are still covered by unit/integration tests while the production query builders route through free functions."
    )]
    fn last_insert_rowid_sql(&self) -> Option<&'static str> {
        None
    }
}

/// Change-stream capability — the "produce CDC events for an app" slice
/// of the data-store boundary.
///
/// Introduced as the cross-backend surface for SQLite's
/// `preupdate_hook`-driven CDC arm. PG and SQLite both implement this on adapter types
/// (`change_stream_pg::PgChangeStream` and
/// `backend::sqlite::cdc::SqliteChangeStream`) rather than on the
/// backend itself so the `ConsumerHandle` associated type can diverge
/// across arms without bleeding into the per-isolate `BackendHandle`
/// enum.
///
/// **Why not on `Backend` super-bound** (plan §2.1): consumers route
/// via `BackendHandle::as_change_stream_pg(...)` /
/// `as_change_stream_sqlite(...)` accessors that mirror
/// [`BackendHandle::as_postgres`] / [`BackendHandle::as_sqlite`]. The
/// associated `ConsumerHandle` type (concrete `WalConsumerHandle` for
/// PG, `SqliteConsumerHandle` for SQLite) is the load-bearing reason
/// not to dyn-erase — `async fn` + an associated type is not object-safe
/// without `Box<dyn Future>` per call, and the consumer surface (a
/// detached `compio::runtime::spawn` task on PG, an actor-driven flume
/// channel on SQLite) doesn't naturally share an erased shape.
///
/// The process-wide CDC lifecycle invokes `spawn_consumer` when the first
/// subscription opens. App deletion invokes `deprovision`; migration and
/// schema coordination own the pause guards.
#[allow(dead_code)]
pub trait ChangeStream: 'static {
    /// Concrete handle representing a spawned-but-still-running
    /// consumer. PG: a task handle / supervisor handle; SQLite: a
    /// session marker the actor uses to track that hooks are armed.
    /// Type erased per-impl (associated type) so we don't pay the
    /// `Box<dyn Future>` price the dyn-safe shape would force.
    type ConsumerHandle: 'static;

    /// Idempotently tear down the CDC infrastructure for `app_id`.
    /// Used during app deletion; PG drops the publication and every worker
    /// slot, while SQLite disarms hooks.
    #[allow(async_fn_in_trait)]
    async fn deprovision(&self, app_id: &str) -> Result<(), DbError>;

    /// Provision and spawn the long-running consumer for `(app_id,
    /// worker_id)`. This is the sole provisioning path so a slot cannot be
    /// created without an owned task. The returned handle controls explicit
    /// shutdown and completion.
    #[allow(async_fn_in_trait)]
    async fn spawn_consumer(
        &self,
        app_id: &str,
        worker_id: &str,
    ) -> Result<Self::ConsumerHandle, DbError>;

    // `pause_broker` and `engage_schema_pending` were here until 2026-09-02.
    // They were not vendor behaviour: all four impls - PG and SQLite - were the
    // same two lines, ignored `self`, and touched no backend state. They now
    // live as `broker::BrokerPauseGuard::new` / `SchemaPendingGuard::new`, in
    // the module owning the registries they mutate. Returning them from here
    // would also force those guards to rank 0 while their `Drop` drives the
    // engine, which is a Cargo cycle the split cannot build.
}

/// Vector-index capability — the "build an ANN index over a `float[]`
/// column and run a top-k nearest-neighbour query" slice of the
/// data-store boundary.
///
/// See `docs/archive/p4-search-implementation-plan.md`
/// §2. The PG impl wraps `pgvector` (`CREATE INDEX … USING
/// ivfflat`, `<->` / `<#>` / `<=>` operators by metric). The SQLite
/// impl is a pure-Rust flat scan over a `BLOB` column holding
/// little-endian `[f32]` payloads — dev tier only, ≤50k rows, ≤1024
/// dims, HNSW deferred (riskiest-decision Q-P4-D, plan §10).
///
/// ## Why not on `Backend` super-bound
///
/// Same rationale as [`ChangeStream`] (plan §2):
/// consumers route via concrete-backend accessors —
/// [`BackendHandle::as_postgres`] / [`BackendHandle::as_sqlite`] —
/// because `async fn` in trait position is dyn-incompatible. Adding
/// `VectorIndex` to the omnibus `Backend` super-trait would force
/// every backend to implement it (including hypothetical future
/// arms that have no vector primitive), and the consumer migration
/// path goes through the same `as_*()?.vector_search(...)`
/// shape every other capability consumer already uses.
///
/// ## Method signatures
///
/// The one method is `async`, takes `&self`, and returns
/// `Result<…, DbError>` — the same shape as the other capability
/// traits. `collection` / `column` are unquoted identifiers; impls
/// call through their dialect's `quote_ident` before splicing into SQL.
///
/// **Search only — the index is not this trait's to create.** The
/// pgvector ivfflat index is authored by `zeroship-migrate` from the
/// declared `t.vector(dims, { metric })` field; the SQLite `vec0`
/// shadow relation is named by the runtime descriptor
/// (`AuxiliaryObject::ShadowTable`) and is likewise not created here.
///
/// `dims` is the declared vector dimensionality (per SDK
/// `t.vector(dims)`); impls fail-fast on a dim mismatch at insert
/// time via a `vector_dimension_mismatch` typed error. `metric`
/// selects the distance function — see [`VectorMetric`] for the
/// canonical operator mapping.
///
/// `filter` is the standard `$op`-shaped JSON predicate the rest of
/// the crate already understands (passes through `query.rs` builders).
/// The vector search returns rows ordered by `_distance` ASC; the
/// synthetic `_distance` column is `f64` and lives on the returned
/// JSON `Value`s. The SDK strips the leading `_` from user-visible
/// columns (Q-P4-I) so this prefix is reserved for engine annotations.
///
/// Not `Send + Sync` — same Open Q4 reasoning as the rest of the
/// capability traits.
pub trait VectorIndex: 'static {
    /// Return the top-`k` rows ordered by distance ASC. `query` is the
    /// query vector (length must match the column's declared `dims`
    /// or impls return a `vector_dimension_mismatch` typed error).
    /// `filter` is composed via `AND` with the distance ordering.
    /// Each returned `Value` is an object including a synthetic
    /// `"_distance"` field (`f64`).
    #[allow(async_fn_in_trait)]
    #[allow(clippy::too_many_arguments)] // mirrors the SDK's flat vector-search call shape; a params struct would just move the fields
    /// `schema` is the caller-resolved descriptor slice for `collection`.
    ///
    /// **A parameter, not a lookup.** Both impls used to call
    /// `descriptor::collection_schema(binding, collection)` themselves, which
    /// reaches the ENGINE's per-isolate context - an edge that cannot compile
    /// once the backends are their own crates. The caller already holds the
    /// descriptor; `binding` and `collection` stay because the SQL still names
    /// the schema and table.
    async fn vector_search(
        &self,
        binding: &DbBinding,
        collection: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: VectorMetric,
        filter: &serde_json::Value,
        schema: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, DbError>;
}

/// Spatial-index capability — the "build an R-tree-like index over a
/// `geography(POINT)` column and run a within-radius point query"
/// slice of the data-store boundary.
///
/// See plan §2. The PG impl wraps
/// PostGIS (`geography(POINT, 4326)` column type, `GIST` index,
/// `ST_DWithin` / `ST_MakePoint` operators). The SQLite impl
/// is a pure-Rust haversine within-radius post-filter over a `BLOB`
/// column packed as `(lat, lng)` little-endian `f64` × 2 = 16 bytes —
/// dev tier only, no R-tree (Q-P4-C: polygon ops PG-only).
///
/// `point` is the query centre. `radius_m` is in metres on both
/// backends (PG geography type works in metres; SQLite haversine
/// returns metres directly). `filter` composes with the
/// within-radius predicate via `AND`. Each returned `Value` includes
/// a synthetic `"_distance_m"` field (`f64`).
///
/// **`spatial_near` only** (Q-P4-C): polygon ops (`within`,
/// `intersects`) are deferred. The SQLite impl rejects polygon
/// input with `Configuration { code: "polygon_ops_pg_only" }`.
pub trait SpatialIndex: 'static {
    /// Return rows within `radius_m` of `point` ordered by distance
    /// ASC. `limit` of `None` defers to the impl's default.
    #[allow(async_fn_in_trait)]
    #[allow(clippy::too_many_arguments)] // mirrors the SDK's flat spatial-near call shape; a params struct would just move the fields
    /// `schema` is the caller-resolved descriptor slice - see
    /// [`VectorIndex::vector_search`] for why it is a parameter.
    async fn spatial_near(
        &self,
        binding: &DbBinding,
        collection: &str,
        column: &str,
        point: GeoPoint,
        radius_m: f64,
        filter: &serde_json::Value,
        limit: Option<usize>,
        schema: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, DbError>;
}

/// Snapshot + restore + PITR for the per-app data store.
///
/// **Admin surface** — like [`ChangeStream`] / [`VectorIndex`], this
/// trait sits beside the `Backend` super-trait rather than joining it.
/// App code never reaches it.
///
/// # This capability is NOT in a release build
///
/// The `#[cfg]` below is the whole story: the trait, both impls
/// (`PostgresBackend` in `backend/postgres.rs`, `SqliteBackend` in
/// `backend/sqlite/mod.rs`), both `BackendHandle::as_backup_*`
/// accessors, and the three compile-time assertions in `mod tests` are
/// each gated on `feature = "test-helpers"`. Nothing in the workspace
/// enables that feature outside this crate's own integration targets,
/// so there is no backup orchestrator calling in and no shipped path
/// that can take a snapshot. Read the bodies as a design placeholder
/// exercised by tests, not as operable backup.
///
/// The two `as_backup_*` accessors have no caller at all - the
/// integration tests reach the impls through the trait directly.
///
/// Bodies, for what they will do: PG shells out to
/// `pg_dump`/`pg_restore` and writes a PITR placeholder row; SQLite
/// does `VACUUM INTO` + atomic-rename restore and refuses PITR with
/// `pitr_pg_only`.
#[cfg(feature = "test-helpers")]
pub trait Backup: 'static {
    /// Take a snapshot of the per-app data store and stream it to
    /// `dest_uri`. Returns a handle with the content hash for
    /// integrity verification on restore.
    #[allow(async_fn_in_trait)]
    async fn snapshot(
        &self,
        app_id: &str,
        dest_uri: &str,
        opts: SnapshotOpts,
    ) -> Result<SnapshotHandle, DbError>;

    /// Restore a snapshot taken by [`Self::snapshot`]. PG:
    /// downloads + `pg_restore` + atomic schema swap. SQLite:
    /// downloads + atomic rename + isolate evict.
    #[allow(async_fn_in_trait)]
    async fn restore(&self, app_id: &str, snapshot: &SnapshotHandle) -> Result<(), DbError>;

    /// Replay WAL up to `target`. PG: writes the target to a
    /// `__zeroship_admin.pitr_targets` table NOTHING NOW CREATES, so
    /// the call fails; operator runs `recovery.conf`. The impl has no
    /// caller - see `backend/postgres.rs`.
    /// SQLite: returns `Configuration { code: "pitr_pg_only" }`
    /// — SQLite has no WAL-archive PITR story.
    #[allow(async_fn_in_trait)]
    async fn pitr_replay(&self, app_id: &str, target: PitrTarget) -> Result<(), DbError>;
}

