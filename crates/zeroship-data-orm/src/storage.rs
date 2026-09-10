//! The storage-capability traits: what a backend must be able to do.
//!
//! Eight narrow traits rather than one monolithic `Backend`. Each is a CONTRACT
//! and nothing more - an associated type or two, and method signatures over the
//! vocabulary in [`crate::capability`]. None of them names a database driver, a
//! runtime or V8, and none carries a default body that does; that is what rank 0
//! means, and it is why they can sit below every tier that implements them.
//!
//! Two shapes are excluded on purpose, because either would break rank 0:
//!
//! * A DEFAULT METHOD BODY that awaits. A default body travels with its trait,
//!   so a backoff schedule built on `compio::time::sleep` would put an async
//!   executor in a crate that declares no runtime. Bounded acquisition lives in
//!   `zeroship-data-v8`'s `lock_policy::BoundedLockAcquire` extension trait,
//!   blanket-implemented over every `LockManager`.
//! * A method handing out `BrokerPauseGuard` or `SchemaPendingGuard`, whose
//!   `Drop` impls drive the engine's broker registries. That is a
//!   `data-core -> data-engine` edge against the dependency that already runs
//!   the other way, and it is not vendor behaviour in the first place: every
//!   implementation would ignore `self`.
//!
//! # Implementations stay above
//!
//! Every `impl Trait for T` lives in the crate that owns `T` - nineteen of them
//! across `zeroship-data-v8` today, including two on its `BackendHandle`
//! dispatch enum. The orphan rule permits exactly that shape: a local type may
//! implement a foreign trait.
//!
//! # NO `cfg(feature)` ON A TRAIT OR A TRAIT MEMBER IN THIS FILE
//!
//! This file declares CONTRACTS, and a contract whose shape depends on a
//! feature is not one. No trait or trait member here carries a
//! `#[cfg(feature = ...)]`. `test-helpers` gates HELPERS AND FIXTURES -
//! `DbBinding::cold_start`, the broker's per-thread test isolation,
//! `schema_cache::reset_for_tests` - and never the shape of a capability.
//!
//! It is not a style rule. `test-helpers` is a DEV-dependency feature of every
//! crate above this one, so `--all-targets`, `--all-features`, clippy and every
//! `cargo test` invocation unify it ON and report a shape no shipped binary
//! has. Three distinct breakages hide in that blind spot:
//!
//! * A REQUIRED member behind the gate (`DialectBuilder::sql_dialect`) is
//!   `error[E0046]: not all trait items implemented` in any build that enables
//!   this crate's feature without the vendor's, and feature unification makes
//!   that reachable from a single dependent's manifest.
//! * An OVERRIDE behind the gate (`SqlExecutor::pool_exec_ddl` on the PG arm)
//!   is `error[E0407]` in the mirror configuration, and worse than an error in
//!   the one that compiles: the default body silently takes over, sending
//!   multi-statement DDL down the extended protocol PostgreSQL rejects.
//! * A whole TRAIT behind the gate (`SchemaIntrospect`) stops the shipped worker
//!   and CLI binaries from compiling the moment a production caller reaches it,
//!   while every test configuration stays green.
//!
//! The invariant is checkable rather than remembered:
//!
//! ```text
//! grep -nE '^[[:space:]]*#\[cfg' crates/zeroship-data-orm/src/storage.rs \
//!                                crates/zeroship-data-orm/src/capability.rs
//! ```
//!
//! must print nothing. The anchor is load-bearing - the prose in both files
//! quotes the attribute repeatedly, so an unanchored `grep 'cfg(feature'`
//! matches six comment lines and reports a violation that is not one.
//!
//! Two gates enforce it, and each is blind to what the other sees:
//!
//! * `tests/contract_feature_invariance_gate.sh` reads every `pub trait` here
//!   and refuses a `cfg` attribute on one, and separately BUILDS each
//!   dependent with this crate's feature on and its own off - the
//!   configuration this workspace does not contain and the one the E0046 came
//!   from.
//! * `tests/shipped_config_gate.sh` builds the lib/bins configuration that
//!   actually ships, which is what catches a production caller of anything
//!   still gated.

use zeroship_data_sql::compile::SqlDialect;
use zeroship_data_sql::descriptors::{GeoPoint, VectorMetric};

use crate::binding::DbBinding;
use crate::capability::{LockScope, SnapshotHandle, SnapshotOpts};
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
/// on `BackendHandle` (the engine tier's dispatch enum, in
/// `zeroship-data-orm`) (`as_postgres`, `as_encrypted_column_pg`)
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
    /// Migration fixtures may execute a script containing a table, its indexes
    /// and protection sentinels. PostgreSQL uses the simple-query protocol for
    /// scripts because a prepared statement accepts only a single command.
    ///
    /// The default impl forwards to [`Self::pool_exec`] — correct for
    /// the SQLite arm (whose `pool_exec` routes through `sqlite3_exec`,
    /// natively multi-statement) and for mocks. The Postgres backend
    /// overrides it to use `batch_execute`.
    ///
    /// **KEEP THIS UNGATED.** A defaulted member is the WORST place to put a
    /// feature gate, because only one of its two failure modes is an error.
    /// Gate the member and leave the PG override ungated and you get `error[E0407]:
    /// method `pool_exec_ddl` is not a member of trait `SqlExecutor``. Gate
    /// both, then enable this crate's feature without the vendor's - which
    /// feature unification does from a single dependent's manifest - and it
    /// COMPILES, with the default body quietly taking over: PG would send a
    /// `;`-separated CREATE TABLE script through `query_text_params`, which is
    /// the extended protocol, and PostgreSQL answers `cannot insert multiple
    /// commands into a prepared statement` at runtime. That is exactly the
    /// divergence the override exists to prevent, reintroduced by a cfg.
    /// Measured 2026-09-04 by restoring the gate on this member alone:
    /// `cargo check -p zeroship-data-postgres` (default features) reports
    /// `error[E0407]: method `pool_exec_ddl` is not a member of trait
    /// `SqlExecutor``.
    ///
    /// The member has no caller today - schema belongs to `zeroship-migrate`,
    /// which issues its own DDL - and that is not a reason to gate it. It is
    /// the DDL half of the SQL-execution contract, it is the one member whose
    /// default is wrong for a shipped vendor, and both facts are properties of
    /// the contract rather than of the current call graph.
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
/// implemented for every `LockManager`. It is deliberately not a DEFAULT METHOD
/// BODY here: a default body travels with its trait, and this trait sits in a
/// crate that declares no runtime. Call `acquire` with
/// `use crate::lock_policy::BoundedLockAcquire;` in scope.
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
///   `hashtext()` SQL lives at this layer. `acquire_advisory_lock` itself is
///   **unreachable from the typed surface, and must stay that way**: it waits
///   indefinitely, which lets a malicious app holding its own lock stall every
///   other caller. The typed API dispatches through the bounded retry loop
///   instead. All three raw methods stay `#[doc(hidden)]`; nothing blocks
///   deleting `acquire_advisory_lock` outright.
pub trait LockManager: SqlExecutor {
    /// Try to acquire a session-scoped advisory lock for the given
    /// [`LockScope`]; `Ok(false)` if another holder already owns it.
    /// Typed wrapper over [`Self::try_acquire_advisory_lock`].
    ///
    /// Takes `&LockScope` — see
    /// [`BoundedLockAcquire::acquire`](crate::lock_policy::BoundedLockAcquire::acquire).
    #[allow(async_fn_in_trait)]
    async fn try_acquire(&self, client: &Self::Client, scope: &LockScope) -> Result<bool, DbError> {
        let (k1, k2) = scope.to_keys();
        self.try_acquire_advisory_lock(client, &k1, &k2).await
    }

    /// Release a session-scoped advisory lock previously acquired via
    /// [`BoundedLockAcquire::acquire`](crate::lock_policy::BoundedLockAcquire::acquire)
    /// / [`Self::try_acquire`]. Typed wrapper over
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
    /// `crates/zeroship-data-v8/tests/integration.rs` still calls `pg_advisory_lock` SQL
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
/// A narrow capability rather than part of an omnibus super-trait (see
/// `docs/archive/p0-implementation-plan.md` and
/// `docs/archive/db-system-design.md` §7). The trait owns the `LiveSchema`
/// associated type, so test helpers and conformance assertions bind to this
/// instead of to `Backend`. `Backend` re-anchors the same associated type via
/// the `SchemaIntrospect<LiveSchema = LiveSchema>` super-bound below, so the
/// constraint is identical for callers that do want the omnibus trait.
///
/// **KEEP THIS UNGATED.** A catalog read is a PRODUCTION SECURITY FENCE on the
/// write path: `zeroship_data_orm::crud::protection_floor` refuses a write
/// whose descriptor dropped a mask or an encryption block the database still
/// records, and it recovers those records through this trait. A fence that
/// compiles only into test builds is not a fence. Shipping it costs nothing -
/// both impls read their own vendor's catalog with the driver the crate already
/// depends on, so there is no new dependency in either tier.
///
/// The same reasoning covers [`Backup`], which is a capability CONTRACT and is
/// ungated for the same reason. What sits behind the feature there are its two
/// vendor IMPLS - a separate decision with a separate cost; see that trait's own
/// rustdoc. "Only tests call it today" is a fact about the call graph, never
/// about the shape, and it is not a reason to gate a contract.
///
/// What is genuinely test-only, and named as a code SPAN rather than an
/// intra-doc link: the `Backend` conformance marker, now plain `cfg(test)` in
/// `zeroship-data-orm`. `tests/run_doc_gate.sh` requires zero unresolved
/// links in the default and `--all-features` doc builds alike, so a link to a
/// cfg-gated item is red in one of them whichever way it is written.
/// `PgSqlExecutor`'s raw-pool escape hatch was the other example and was
/// deleted on 2026-09-09.
pub trait SchemaIntrospect: 'static {
    /// Concrete live-schema snapshot returned by
    /// [`Self::introspect_schema`]. The Postgres impl uses
    /// [`zeroship_data_sql::catalog::LiveSchema`]; each vendor tier populates that
    /// neutral shape from its own catalog.
    ///
    type LiveSchema;

    /// Introspect the live schema for an app into the vendor's catalog snapshot.
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
/// backend would re-use `zeroship_data_orm::backend::postgres::implementation::PostgresBackend`'s pool +
/// `SqlExecutor` impl but share a single `PgDialect`. Pinning
/// `DialectBuilder` as its own trait (and composing into the
/// per-backend struct) is the canonical shape.
pub trait DialectBuilder: 'static {
    /// Concrete SQL dialect this builder targets.
    ///
    /// **UNGATED, and it was `#[cfg(feature = "test-helpers")]` until
    /// 2026-09-04 - the most dangerous gate in this file.** A REQUIRED member
    /// with no default does not merely disappear when the feature is off; it
    /// changes what every implementor must write. Enable this crate's feature
    /// without a vendor's - which one dependent's manifest does through feature
    /// unification, with no source change anywhere - and both vendors stop
    /// compiling:
    ///
    /// ```text
    /// cargo check -p zeroship-data-postgres --features zeroship-data-core/test-helpers
    ///   error[E0046]: not all trait items implemented, missing: `sql_dialect`
    ///     x2 - `impl DialectBuilder for PgDialect` and `for PostgresBackend`
    /// cargo check -p zeroship-data-sqlite --features zeroship-data-core/test-helpers
    ///   error[E0046]: ... x2 - `for SqliteDialect` and `for SqliteBackend`
    /// ```
    ///
    /// Measured 2026-09-04 at 26e996ef5, where the gate was still on. Line
    /// numbers are deliberately not quoted: they were `postgres.rs:809` /
    /// `:863` then, they moved when this comment was written, and a citation
    /// that rots inside its own commit is worse than the impl names.
    ///
    /// Deleting the configuration that trips a latent hazard does not retire
    /// the hazard, and those four `impl` blocks are the whole reason: they were
    /// never protected by anything except nobody having written that manifest
    /// line yet. `tests/contract_feature_invariance_gate.sh` builds all four
    /// dependents in exactly that resolution now.
    ///
    /// A `DialectBuilder` that cannot say which dialect it builds is not one -
    /// this is the trait's identity, not a probe. Its ungated peers
    /// ([`Self::quote_ident`], [`Self::map_zs_type`]) already answer
    /// per-dialect questions; this answers WHICH dialect, and it was the only
    /// member of the five that a release build could not ask.
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
    fn map_zs_type(&self, zs_type: &str, opts: &zeroship_data_sql::value::Value) -> String;

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
/// `BackendHandle::as_postgres` / `BackendHandle::as_sqlite`. The
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

    // Broker pause and schema-pending engagement do NOT belong on this trait.
    // They are not vendor behaviour - the PG and SQLite bodies would be the same
    // two lines, ignoring `self` and touching no backend state - and they live
    // as `broker::BrokerPauseGuard::new` / `SchemaPendingGuard::new`, in the
    // module owning the registries they mutate. Returning those guards from here
    // would also force them to rank 0 while their `Drop` drives the engine,
    // which is a Cargo cycle that cannot build.
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
/// `BackendHandle::as_postgres` / `BackendHandle::as_sqlite` —
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
    /// **A parameter, not a lookup.** An impl calling
    /// `descriptor::collection_schema(binding, collection)` itself would reach
    /// the ENGINE's per-isolate context - an edge that cannot compile from a
    /// vendor crate. The caller already holds the descriptor; `binding` and
    /// `collection` stay because the SQL still names the schema and table.
    async fn vector_search(
        &self,
        binding: &DbBinding,
        collection: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: VectorMetric,
        filter: &zeroship_data_sql::value::Value,
        schema: &zeroship_data_sql::value::Value,
    ) -> Result<Vec<zeroship_data_sql::value::Value>, DbError>;
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
        filter: &zeroship_data_sql::value::Value,
        limit: Option<usize>,
        schema: &zeroship_data_sql::value::Value,
    ) -> Result<Vec<zeroship_data_sql::value::Value>, DbError>;
}

/// Snapshot + restore for the per-app data store.
///
/// **Admin surface** — like [`ChangeStream`] / [`VectorIndex`], this
/// trait sits beside the `Backend` super-trait rather than joining it.
/// App code never reaches it.
///
/// # The CONTRACT ships; the IMPLEMENTATIONS do not
///
/// **The trait is UNGATED and must stay that way**, for the reason at the top of
/// this file: a contract whose shape depends on a feature is not a contract.
/// This one is the sibling of [`ChangeStream`], [`VectorIndex`] and
/// [`SpatialIndex`], which are ungated for the same reason. A trait declaration
/// with no implementor generates no code, so shipping it costs zero and not
/// merely little.
///
/// What is STILL out of a release build is everything that does work:
///
/// * both impls - `zeroship-data-postgres`'s `backup_pg` (a `pg_dump` /
///   `pg_restore` shell-out and a destructive `DROP SCHEMA ... CASCADE` on the
///   restore path) and `zeroship-data-sqlite`'s `VACUUM INTO` + atomic rename;
/// * the `sha2` both use to hash dump artifacts, which is `optional = true` in
///   each vendor manifest and pulled in by `test-helpers`;
/// * the three compile-time assertions in `zeroship-data-orm`'s
///   `backend::tests`.
///
/// The list is an inventory of what is gated, so every entry must name a symbol
/// that exists. One that does not reads as gated surface and is worse than an
/// omission.
///
/// That line is drawn deliberately and is not the same line the gate on the
/// trait drew. A capability contract costs nothing to ship and is what a future
/// backup orchestrator would be written against. An uncallable body that
/// `DROP SCHEMA ... CASCADE`es a tenant is not something to link into every
/// worker, gateway and CLI binary in order to satisfy a symmetry argument.
/// Whoever gives this capability a caller ungates the impls in the same change,
/// and `tests/shipped_config_gate.sh` makes that a compile error rather than a
/// discovery: an ungated caller of a gated impl fails the lib/bins build.
///
/// # Why there is no `pitr_replay`, and what it would take
///
/// There is no `pitr_replay` here, and the reason is a shape problem rather
/// than unfinished work - which is why it is written down. Without it, PITR
/// reads as an obvious gap for the next author to fill.
///
/// PostgreSQL point-in-time recovery is driven by server-level configuration -
/// `restore_command`, `recovery_target_*`, an `archive_command` that must
/// already have been running before the window being recovered. None of that can
/// be initiated over a client connection, so a PG arm here could only record a
/// requested target for an operator to act on out of band, and there is nowhere
/// for a worker-reachable method to durably record it: AGENTS.md's privilege
/// invariant reserves that kind of state for a schema a separate service writes.
/// SQLite has no WAL-archive substrate to replay from at all.
///
/// So PITR is an operator capability with a database-server contract, not a
/// per-app data-store method. Whoever gives it a home should start from where an
/// operator's request is durably recorded and who is allowed to write there -
/// not from this trait, whose two methods are both things a client connection
/// can actually perform.
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
}
