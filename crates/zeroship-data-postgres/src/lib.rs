#![recursion_limit = "256"]
//
// The `env.db` async chains nest deeply enough to pass rustc's default depth of
// 128 when a pooled request awaits the connection task's own future. The adapter
// carries the same attribute for the same code.

//! The PostgreSQL vendor tier of the data plane.
//!
//! # What this crate is for
//!
//! It is the only crate below `zeroship-plugin-db` allowed to name
//! `compio_postgres`. Everything above it speaks the contracts in
//! [`zeroship_data_core::storage`] - `SqlExecutor`, `LockManager`,
//! `SchemaIntrospect` and the rest - so a second backend is a sibling crate
//! rather than a set of `match` arms.
//!
//! `tests/vendor_embedding_gate.sh` enforces that mechanically: a
//! `compio_postgres::` type appearing in a signature outside this crate and its
//! SQLite peer is a build failure, not a review comment.
//!
//! # What lives here
//!
//! [`postgres::PostgresBackend`] and the five files it leans on - catalog
//! introspection, row-to-JSON decoding, SQLSTATE classification, per-app session
//! SQL, and the roled autocommit funnel - plus [`lock_guard::LockGuard`], which
//! travels with the backend because its `acquire` is bound to
//! `LockManager<Client = compio_postgres::OwnedPooledClient>`.
//!
//! # What deliberately does NOT live here
//!
//! `Backend`, the composition marker, stays in `zeroship-plugin-db`: it is that
//! crate's own `pub(crate)` trait, so by the orphan rule
//! `impl Backend for PostgresBackend` can only be written there. Its
//! compile-time conformance assertion stays with it.
//!
//! The bounded-retry lock policy is likewise absent by design - it is
//! [`zeroship_data_core::lock_policy::BoundedLockAcquire`], because the SQLite
//! backend needs it too and a policy both vendors call cannot live in either.

// These four serve only the two extension traits below, which are themselves
// gated: without the gate a default build warns on all four.
#[cfg(any(test, feature = "test-helpers"))]
use std::rc::Rc;

#[cfg(any(test, feature = "test-helpers"))]
use zeroship_data_core::error::DbError;
#[cfg(any(test, feature = "test-helpers"))]
use zeroship_data_core::storage::{LockManager, SqlExecutor};

#[cfg(any(test, feature = "test-helpers"))]
pub mod lock_guard;
pub mod pg_autocommit;
pub mod pg_error;
// The whole introspection chain follows `SchemaIntrospect`'s gate in data-core.
#[cfg(feature = "test-helpers")]
pub mod pg_introspect;
pub mod pg_row_json;
pub mod pg_session_sql;
pub mod postgres;

pub use postgres::PostgresBackend;


/// Postgres-specific extension trait exposing the underlying pool
/// handle so free-function consumers — chiefly the audit helpers in
/// `zeroship_plugin_db::audit` — can reach an `&compio_postgres::Pool` without
/// naming the concrete backend type.
///
/// **Open Q1 resolution**: the 16 audit-table operations
/// that used to live as methods on `Backend` were deleted; the
/// helpers stay as free functions in `crate::audit::*` taking
/// `&Pool` / `&Client`, and generic consumers reach the pool through
/// `backend.pool_handle()`. See `docs/archive/p0-implementation-plan.md`
/// §3 Q1 and `docs/archive/db-system-design.md` §7.
///
/// **Feature gating: TEST-ONLY as of 2026-09-01, and that is the point.**
///
/// This trait hands out the raw pool, and a bare checkout carries the shared
/// `zeroship_worker` login role with NO `SET LOCAL ROLE` - so every call is a
/// chance to reach a tenant schema outside the per-app role fence. The tree
/// records one occasion the option was taken: `crud/unmask.rs`'s audit INSERT
/// used `pool_handle()` until 2026-09-01 and was the single ungated production
/// path in this crate reaching a tenant schema unfenced.
///
/// It was unconditional until 2026-09-01, which made the escape hatch
/// DISCOURAGED rather than IMPOSSIBLE. Its only four callers were in
/// `crud::mask_drift`, which was itself `#[cfg(any(test, feature =
/// "test-helpers"))]`, so gating the trait removed it from release builds and
/// changed no behaviour.
///
/// **THAT MODULE WAS DELETED ON 2026-09-03, SO THIS TRAIT NOW HAS NO CALLERS
/// AT ALL** - what remains is this definition, the `impl` in `postgres.rs`, and
/// two `assert_impl`-style witnesses that only prove the impl exists. The
/// reason it was gated rather than deleted was that three of those four callers
/// issued DDL the per-app role is not granted, so they could not be routed
/// through the roled funnel; with the callers gone, that reason is gone too.
/// Deleting the trait is the open follow-up, tracked in
/// `docs/proposals/2026-08-28-app-database-decoupling.md`. It is left standing
/// here only because removing a role-fence escape hatch is a decision worth
/// making on its own rather than as a side effect of deleting a drift checker.
///
/// If a production path ever needs the pool, that is a design question, not a
/// feature-flag question: route it through `PostgresBackend`'s roled entry
/// points, or argue why the fence should not apply.
///
/// The `impl` side is PG-only; a hypothetical `SqliteBackend` would not
/// implement this trait — it would have its own audit-helper signatures (a
/// `SqliteExecutor` accessor returning `&sqlite::Connection`, etc.).
#[cfg(any(test, feature = "test-helpers"))]
pub trait PgSqlExecutor: SqlExecutor<Client = compio_postgres::OwnedPooledClient> {
    /// Borrow the underlying `compio_postgres::Pool`. Free-function
    /// audit helpers in `zeroship_plugin_db::audit` take `&Pool` directly; this
    /// accessor lets generic consumers (e.g.
    /// `<B: PgSqlExecutor>`) reach the pool without naming
    /// `PostgresBackend`.
    fn pool_handle(&self) -> &Rc<compio_postgres::Pool>;
}

/// Postgres-specific extension trait carrying the
/// `acquire_pooled_client_for_lock` primitive - a pool checkout whose lease
/// [`crate::lock_guard::LockGuard`] holds for the life of the
/// advisory lock.
///
/// **Open Q5 is now moot, and this paragraph is kept as history rather than as
/// a live trade-off.** It read: the primitive had to return a
/// `PooledClient<'p>` whose `'p` borrow lifetime threaded through `LockGuard`,
/// and the alternative was a GAT on [`LockManager`](zeroship_data_core::storage::LockManager) of the form
/// `type PooledLockClient<'p>: 'p where Self: 'p` — workable with
/// async-fn-in-trait but fighting the trait solver at consumer sites, so the
/// PG extension trait was taken and cross-backend lifetime threading deferred.
/// `OwnedPooledClient` removes the lifetime outright: the lease owns an `Rc` of
/// the pool and still returns on drop, so neither branch of that choice is
/// needed. See `docs/archive/p0-implementation-plan.md` §3 Q5 and
/// `docs/archive/db-system-design.md` §7 for the original framing.
///
/// The `: LockManager<Client = compio_postgres::OwnedPooledClient>` super-bound
/// is load-bearing: the returned lease is the [`SqlExecutor::Client`](zeroship_data_core::storage::SqlExecutor::Client) that
/// [`LockManager::acquire_advisory_lock`](zeroship_data_core::storage::LockManager::acquire_advisory_lock) takes, so the orchestrator can hand
/// it straight into `LockGuard::acquire` without an adapter.
#[cfg(any(test, feature = "test-helpers"))]
pub trait PgLockManager: LockManager<Client = compio_postgres::OwnedPooledClient> {
    /// Acquire a pool-leased client for advisory-lock duty.
    ///
    /// Returns an [`compio_postgres::OwnedPooledClient`]: the lease owns an
    /// `Rc` of the pool and still returns on drop, so
    /// [`crate::lock_guard::LockGuard`] no longer has to thread a
    /// `'p` borrow lifetime through itself. The Q5 note above is therefore
    /// historical - the GAT alternative it weighs was solving a lifetime
    /// problem the owned lease removes outright.
    ///
    /// Postgres impl wraps `self.pool().get_owned().await` and maps the
    /// pool error to [`DbError::Transient`](zeroship_data_core::error::DbError::Transient) with the same operator-
    /// facing message the bootstrap call site used to emit inline.
    #[allow(async_fn_in_trait)]
    async fn acquire_pooled_client_for_lock(
        &self,
    ) -> Result<compio_postgres::OwnedPooledClient, DbError>;
}
