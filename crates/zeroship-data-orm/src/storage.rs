//! Host storage capabilities outside query execution.
use crate::{
    capability::{LockScope, SnapshotHandle, SnapshotOpts},
    error::DbError,
};
pub trait LockManager: 'static {
    type Client;
    /// Try to acquire a lock for the scope; return `Ok(false)` on contention.
    /// PostgreSQL locks belong to the supplied session; SQLite uses a backend-local registry.
    #[allow(async_fn_in_trait)]
    async fn try_acquire(&self, client: &Self::Client, scope: &LockScope) -> Result<bool, DbError> {
        let (k1, k2) = scope.to_keys();
        self.try_acquire_advisory_lock(client, &k1, &k2).await
    }

    /// Release a lock using the same scope and client as acquisition.
    #[allow(async_fn_in_trait)]
    async fn release(&self, client: &Self::Client, scope: &LockScope) -> Result<(), DbError> {
        let (k1, k2) = scope.to_keys();
        self.release_advisory_lock(client, &k1, &k2).await
    }

    /// Wait for a lock using backend keys. Runtime callers should use
    /// `BoundedLockAcquire::acquire` to bound contention waits.
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

    /// Try to acquire a lock using backend keys. Prefer [`Self::try_acquire`].
    #[doc(hidden)]
    #[allow(async_fn_in_trait)]
    async fn try_acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<bool, DbError>;

    /// Release a lock using backend keys. Prefer [`Self::release`].
    /// A PostgreSQL session whose unlock fails must be discarded before pool reuse.
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

    /// Restore a snapshot taken by [`Self::snapshot`], checking its integrity first.
    #[allow(async_fn_in_trait)]
    async fn restore(&self, app_id: &str, snapshot: &SnapshotHandle) -> Result<(), DbError>;
}
