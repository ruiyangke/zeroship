//! Host storage capabilities outside query execution.
use crate::{
    capability::{LockScope, SnapshotHandle, SnapshotOpts},
    error::DbError,
};
pub trait LockManager: 'static {
    type Client;
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
    /// `crates/zeroship-data-orm/src/tests/postgres/mod.rs` still calls `pg_advisory_lock` SQL
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
