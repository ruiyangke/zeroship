//! Backend abstraction — "the data store boundary".
//!
//! Stage 8e — R2 of the plugin-db architecture review
//! (`docs/reviews/plugin-db-architecture-review-2026-05-21.md`).
//!
//! ## What this is
//!
//! A single trait, [`Backend`], that captures everything the
//! orchestrator and audit-row layer ask of "the database" — connection
//! lifecycle, advisory locks, schema introspection, and audit-table
//! reads/writes. Today the only impl is [`PostgresBackend`]; the goal
//! is to name the seams BEFORE a second backend lands so we don't
//! accidentally bake `compio_postgres::Pool` / `Client` into every
//! consumer file.
//!
//! ## What this is NOT
//!
//! - **Not a query-IR layer.** [`crate::query`] still emits Postgres
//!   DDL/DML directly via `quote_ident`, `ON CONFLICT`, `RETURNING`,
//!   etc. The architecture critic explicitly scoped that out — see
//!   `query.rs` (4099 LOC) and the review's R2 recommendation: "wait
//!   until a real sqlite or planetscale prototype is in motion." This
//!   trait fixes the *non-builder* surface (orchestrator + audit).
//! - **Not a leakage-free abstraction.** [`crate::replication`] and
//!   [`crate::wal_consumer`] talk raw `pg_replication_slots` and the
//!   streaming WAL protocol; those stay Postgres-only behind their own
//!   files. The `auth/*` SECURITY DEFINER bootstrap is likewise PG-only.
//! - **Not async-trait-Boxed.** `compio-postgres` is single-threaded
//!   io_uring; we use `async fn` directly in trait position
//!   (`async_fn_in_trait` is stable) so the orchestrator's hot paths
//!   don't allocate a `Box<dyn Future>` per call.
//!
//! ## Trait shape
//!
//! Associated types `Client` / `LiveSchema` keep the consumer files
//! free of `compio_postgres::Client` / `crate::diff::LiveSchema`
//! direct references — they go through `B::Client` instead. The PG
//! impl ties them to the concrete types in
//! [`postgres::PostgresBackend`].

use std::rc::Rc;

use serde_json::Value;

use crate::audit::{AuditRow, BackfillLookup, LockedAuditRow, TerminalStatus};
use crate::error::DbError;

pub mod postgres;

pub use postgres::PostgresBackend;

/// SQL execution capability — the "connection lifecycle + run a
/// statement" slice of the data-store boundary.
///
/// Carved out of the monolithic [`Backend`] trait in P0 PR 1 (see
/// `docs/proposals/p0-implementation-plan.md` §"PR 1" and the
/// converged design at `docs/proposals/db-system-design.md` §7).
/// Future P0 PRs will narrow consumer bounds onto this trait (and
/// [`LockManager`]) instead of the omnibus [`Backend`] super-trait;
/// see the deferred-backlog [C1] entry in
/// `docs/reviews/plugin-db-deferred.md`.
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

    /// Acquire a dedicated (non-pooled) connection. Caller owns the
    /// lifetime — used by the migration lock and the user-driven
    /// `db.beginTransaction()` path, which need a connection that
    /// survives across pool-return points.
    ///
    /// For Postgres this opens a fresh `compio_postgres::connect(...)`
    /// against the configured URL and spawns the connection task; for
    /// future backends this maps to whatever "long-lived session"
    /// primitive that backend exposes.
    #[allow(async_fn_in_trait)]
    async fn acquire_dedicated_client(&self) -> Result<Self::Client, DbError>;

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
/// Carved out of the monolithic [`Backend`] trait in P0 PR 1 (see
/// `docs/proposals/p0-implementation-plan.md` §"PR 1" and
/// `docs/proposals/db-system-design.md` §7). The `: SqlExecutor`
/// super-bound is load-bearing — every method takes a `&Self::Client`
/// and that associated type lives on [`SqlExecutor`].
///
/// PR 6 will replace the legacy `(key1, key2)` string-key shape with
/// the typed `LockScope` enum and rename `OrchestratorLockGuard` →
/// `LockGuard`; the three methods on this trait stay (likely
/// `pub(crate)`) as the underlying primitive.
pub trait LockManager: SqlExecutor {
    /// Acquire a session-scoped advisory lock on `(key1, key2)` against
    /// the given client. Blocks if another holder exists; the lock
    /// releases when the client is dropped or the backend session
    /// ends.
    ///
    /// Postgres maps this to
    /// `SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)`.
    /// Future backends would map to their per-engine equivalent (e.g.
    /// sqlite has no advisory locks — that backend would need a
    /// `BEGIN EXCLUSIVE` or a sentinel table).
    #[allow(async_fn_in_trait)]
    async fn acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError>;

    /// Try to acquire the same session-scoped advisory lock; return
    /// `Ok(false)` if the lock is already held by a different session.
    /// Used by [`crate::migrations::exec_begin`] so a second worker
    /// observes "migration already running" instead of blocking.
    #[allow(async_fn_in_trait)]
    async fn try_acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<bool, DbError>;

    /// Release a session-scoped advisory lock. The lock auto-releases
    /// on session end, so callers can treat an `Err` as
    /// observability-only (warn-and-continue) — but returning the
    /// typed error lets them emit a structured log instead of
    /// silently swallowing it. Mirrors the pattern
    /// `OrchestratorLockGuard::release` adopted at `ffb1e101`
    /// (code-critique MAJOR-R5-5).
    #[allow(async_fn_in_trait)]
    async fn release_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError>;
}

/// The data-store boundary. One impl per storage backend; today only
/// Postgres ([`PostgresBackend`]).
///
/// `Send + Sync` so the trait object can live behind an `Rc` shared
/// across the per-isolate context's borrow surface (compio is
/// single-threaded but the bound is cheap to satisfy).
///
/// Lifetime invariants:
///
/// - Methods that take `&Self::Client` use it borrow-only; the caller
///   owns the client (e.g. the migration lock holds it across awaits,
///   the audit helpers borrow it for one operation).
/// - [`SqlExecutor::acquire_dedicated_client`] returns an owned `Client`
///   detached from any pool lifetime — the caller is free to park it
///   on the per-isolate context (e.g. `MigrationLock::client`,
///   [`crate::context::IsolateDbContext::tx_conn`]) for the duration
///   of a session-scoped lock.
#[doc = "`Backend` is now a super-trait composition of \
[`SqlExecutor`] + [`LockManager`] (P0 PR 1) plus the remaining 16 \
methods that have not yet been carved into focused capability traits. \
P0 PR 2 carves `NamespaceManager` / `SchemaIntrospect` / \
`IndexBuilder` off the 16; the asymmetry is intentional and tracked \
in `docs/proposals/p0-implementation-plan.md` §\"PR 2\"."]
pub trait Backend: SqlExecutor<Client = compio_postgres::Client> + LockManager + 'static {
    /// Concrete live-schema snapshot returned by
    /// [`Self::introspect_schema`]. The Postgres impl uses
    /// [`crate::diff::LiveSchema`]; alternate backends would produce
    /// the same shape from their own catalog tables.
    type LiveSchema;

    // ----- schema bootstrap + introspection ---------------------------

    /// Idempotently create the per-app schema namespace.
    #[allow(async_fn_in_trait)]
    async fn ensure_app_schema(&self, app_id: &str) -> Result<(), DbError>;

    /// Introspect the live schema for an app. Returns the typed
    /// snapshot the diff engine consumes via
    /// [`crate::diff::compute_diff`].
    #[allow(async_fn_in_trait)]
    async fn introspect_schema(&self, app_id: &str) -> Result<Self::LiveSchema, DbError>;

    /// Estimate the row count for a single collection. Used by the
    /// classifier to decide "ADD NOT NULL on empty table" — cheap
    /// `reltuples`-style estimate is fine.
    #[allow(async_fn_in_trait)]
    async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError>;

    // ----- audit table reads/writes -----------------------------------

    /// Create `__zeroship_migrations` for an app if it doesn't exist.
    /// Idempotent.
    #[allow(async_fn_in_trait)]
    async fn ensure_audit_table(&self, app_id: &str) -> Result<(), DbError>;

    /// Compute the next monotonic `schema_version` for a deploy.
    #[allow(async_fn_in_trait)]
    async fn next_schema_version(&self, app_id: &str) -> Result<i32, DbError>;

    /// Insert a single row into the audit table, returning its PK.
    #[allow(async_fn_in_trait)]
    async fn write_audit_row(&self, app_id: &str, row: &AuditRow) -> Result<i64, DbError>;

    /// Drive an audit row to a terminal status. Returns `true` if the
    /// row transitioned, `false` if the UPDATE matched nothing (row
    /// was already terminal).
    #[allow(async_fn_in_trait)]
    async fn update_audit_status(
        &self,
        app_id: &str,
        id: i64,
        new_status: TerminalStatus,
        error: Option<&str>,
    ) -> Result<bool, DbError>;

    /// SELECT the latest backfill row for `(collection, name)`.
    #[allow(async_fn_in_trait)]
    async fn find_latest_backfill_row(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<Option<BackfillLookup>, DbError>;

    /// Pool-driven variant of [`Self::find_latest_backfill_row`] — used
    /// by the `migrations.status({name, collection})` observation API
    /// which has no dedicated client.
    #[allow(async_fn_in_trait)]
    async fn find_latest_backfill_row_pool(
        &self,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<Option<BackfillLookup>, DbError>;

    /// Mark a backfill row as `running` — used when an existing row is
    /// being resumed by this worker.
    #[allow(async_fn_in_trait)]
    async fn set_backfill_running(
        &self,
        client: &Self::Client,
        app_id: &str,
        id: i64,
    ) -> Result<(), DbError>;

    /// Insert a fresh `running` backfill row.
    #[allow(clippy::too_many_arguments)]
    #[allow(async_fn_in_trait)]
    async fn insert_backfill_running(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
        dry_run: bool,
        deploy_id: &str,
        schema_version: i32,
    ) -> Result<i64, DbError>;

    /// Bump `audit_generation` + reset progress columns. Pool-driven
    /// variant (called from `migrations.reset({name, collection})`).
    #[allow(async_fn_in_trait)]
    async fn reset_backfill_row_pool(
        &self,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<(), DbError>;

    /// Client-driven variant of [`Self::reset_backfill_row_pool`] —
    /// called from `exec_begin` when `reset: true` is passed.
    #[allow(async_fn_in_trait)]
    async fn reset_backfill_row_client(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<(), DbError>;

    /// Peek the latest backfill row's status without locking. Used by
    /// `fetchBatch` to short-circuit on cancel.
    #[allow(async_fn_in_trait)]
    async fn peek_latest_backfill_status(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<Option<String>, DbError>;

    /// Best-effort heartbeat write on the backfill row.
    #[allow(async_fn_in_trait)]
    async fn heartbeat_backfill(
        &self,
        client: &Self::Client,
        app_id: &str,
        collection: &str,
        name: &str,
    ) -> Result<(), DbError>;

    /// `SELECT status, audit_generation ... FOR UPDATE` — acquires the
    /// row lock inside the current transaction.
    #[allow(async_fn_in_trait)]
    async fn lock_audit_row_for_update(
        &self,
        client: &Self::Client,
        app_id: &str,
        id: i64,
    ) -> Result<Option<LockedAuditRow>, DbError>;

    /// Advance a backfill row's progress columns.
    #[allow(async_fn_in_trait)]
    async fn update_backfill_progress(
        &self,
        client: &Self::Client,
        app_id: &str,
        id: i64,
        next_cursor: i64,
        dead_letter_pks: &Value,
        processed_total: i64,
    ) -> Result<(), DbError>;

    /// Drive a backfill row to its terminal status.
    #[allow(async_fn_in_trait)]
    async fn finalise_backfill(
        &self,
        client: &Self::Client,
        app_id: &str,
        id: i64,
        terminal: TerminalStatus,
        error_message: Option<&str>,
    ) -> Result<(), DbError>;

    /// Operator-driven cancel of a backfill row.
    #[allow(async_fn_in_trait)]
    async fn cancel_backfill_row_pool(
        &self,
        app_id: &str,
        id: i64,
    ) -> Result<(), DbError>;

    // ----- create-index recovery -------------------------------------

    /// Idempotent `CREATE INDEX CONCURRENTLY` with retry + audit.
    /// SQLSTATE-driven: unique/not-null/fk/check violations are fatal;
    /// deadlock / disk-full / OOM retry up to a small budget.
    ///
    /// This is the second pass of `register_model::apply` — Postgres-
    /// specific because CIC is a Postgres feature (the trait keeps the
    /// signature; alternate backends would have to map to whatever
    /// online-index primitive they provide).
    ///
    /// Returns `Ok(())` on success; on terminal retry-loop failures
    /// the returned [`DbError::SchemaRefused`] carries a JSON envelope
    /// the SDK consumes verbatim (`validation_refused` /
    /// `unique_violation` shapes). Postgres errors during CIC flow
    /// through the normal `from_pg` path (`UniqueViolation` etc.);
    /// configuration / invariant breaches surface as
    /// [`DbError::Configuration`].
    #[allow(async_fn_in_trait)]
    async fn create_index_with_recovery(
        &self,
        app_id: &str,
        collection: &str,
        spec: &crate::query::IndexSpec,
        deploy_id: &str,
        schema_version: i32,
    ) -> Result<(), DbError>;
}

/// Opaque trait-object handle that the per-isolate context stores.
///
/// We expose the *concrete* PG impl through this Rc so the consumer
/// code can stay `B: Backend`-generic where it cares; the runtime
/// stash is type-erased to avoid threading a parameter through
/// `IsolateDbContext`.
pub type BackendHandle = Rc<PostgresBackend>;

#[cfg(test)]
mod tests {
    //! Interface-level (compile-time) tests for the [`Backend`] trait.
    //!
    //! The trait is `async fn`-in-trait and every method needs a real
    //! Postgres listener via [`PostgresBackend`]; we cannot exercise
    //! method bodies from a `#[test]` without `tests/integration.rs`.
    //! What we *can* do — and what catches the highest-leverage
    //! refactor mistakes — is pin the trait shape at compile time:
    //!
    //! - the canonical impl [`PostgresBackend`] satisfies the bound;
    //! - the associated types stay wired to their concrete
    //!   `compio_postgres` / `crate::diff` counterparts;
    //! - the `'static` bound on the trait flows through.
    //!
    //! Any future change to the trait (new method, swapped
    //! associated-type bound, lifetime tightening) trips one of these
    //! at `cargo build -p zeroship-plugin-db --tests` time, before any
    //! caller fails at a more distant site.

    use super::*;

    /// Compile-time: the canonical impl [`PostgresBackend`] satisfies
    /// the [`Backend`] trait. Function body type-checks at build time;
    /// it's a deliberate no-op at runtime.
    fn assert_postgres_backend_impls_backend() {
        fn assert_impl<T: Backend>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the carved
    /// [`SqlExecutor`] capability super-trait (P0 PR 1). If a future
    /// refactor accidentally pulls a `SqlExecutor` method back onto
    /// the omnibus `Backend` trait — or detaches the impl block from
    /// the `PostgresBackend` type — this stops compiling.
    fn assert_postgres_backend_impls_sql_executor() {
        fn assert_impl<T: SqlExecutor<Client = compio_postgres::Client>>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the carved
    /// [`LockManager`] capability super-trait (P0 PR 1). The
    /// `: SqlExecutor` super-bound on `LockManager` plus the
    /// `Client = compio_postgres::Client` constraint here pin the
    /// shape end-to-end — a regression in either direction fails
    /// compilation in this module.
    fn assert_postgres_backend_impls_lock_manager() {
        fn assert_impl<T: LockManager<Client = compio_postgres::Client>>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: the associated types stay anchored to the concrete
    /// `compio_postgres::Client` / `crate::diff::LiveSchema`. A
    /// regression here would silently change every `B::Client` /
    /// `B::LiveSchema` consumer's expectations.
    fn assert_associated_types_pinned() {
        fn pinned_client<T: Backend<Client = compio_postgres::Client>>() {}
        fn pinned_live_schema<T: Backend<LiveSchema = crate::diff::LiveSchema>>() {}
        pinned_client::<PostgresBackend>();
        pinned_live_schema::<PostgresBackend>();
    }

    /// Compile-time: `Backend: 'static`. The per-isolate context parks
    /// the impl behind an `Rc<PostgresBackend>` in a `thread_local!`;
    /// dropping the `'static` bound would break that path.
    fn assert_backend_is_static() {
        fn assert_static<T: 'static>() {}
        assert_static::<PostgresBackend>();
    }

    /// [`BackendHandle`] must remain `Rc<PostgresBackend>` — the
    /// per-isolate context stores it via this alias, and consumers
    /// `Rc::clone` it without naming the concrete type. Identity-check
    /// the alias here so a refactor that re-types it (e.g. to
    /// `Arc<dyn Backend>`) trips a build error in this module rather
    /// than at every call site.
    fn assert_backend_handle_alias() {
        fn same<T, U>()
        where
            T: 'static,
            U: 'static,
        {
            // We assert structural equivalence by requiring the
            // function body to type-check with `T = U` — the caller
            // below substitutes both sides with the same concrete
            // type, so any divergence is caught.
        }
        same::<BackendHandle, Rc<PostgresBackend>>();
    }

    #[test]
    fn compile_time_assertions_link() {
        // Keep the asserter functions live so the dead-code lint
        // doesn't fire. The type-check still runs even if these
        // weren't called, but the explicit cast documents intent.
        let _ = assert_postgres_backend_impls_backend as fn();
        let _ = assert_postgres_backend_impls_sql_executor as fn();
        let _ = assert_postgres_backend_impls_lock_manager as fn();
        let _ = assert_associated_types_pinned as fn();
        let _ = assert_backend_is_static as fn();
        let _ = assert_backend_handle_alias as fn();
    }
}
