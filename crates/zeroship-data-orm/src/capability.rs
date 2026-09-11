//! Value types shared by ORM storage capability contracts.
//!
//! These types describe scope, locking and snapshots without carrying database
//! connections or runtime state. The consuming traits live in [`crate::storage`].

/// Scope used to derive advisory-lock keys. Actual visibility depends on the backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LockScope {
    /// An app lock across database sessions. PostgreSQL coordinates through the server;
    /// SQLite coordinates only callers sharing the backend instance.
    GlobalApp {
        /// App identifier — first half of the key namespace.
        app_id: String,
        /// Scope name tag, e.g. `"snapshot_restore"`,
        /// `"mig:add_archived_flag"`. Both halves of the underlying
        /// `(key1, key2)` advisory-lock pair derive from this field
        /// (see [`Self::to_keys`]).
        name: String,
    },
    /// An app lock requested for local coordination.
    #[allow(
        dead_code,
        reason = "LocalApp remains part of the lock model for test-helper coverage even though the release build only constructs GlobalApp."
    )]
    LocalApp {
        /// App identifier — first half of the key namespace.
        app_id: String,
        /// Scope name tag.
        name: String,
    },
}

impl LockScope {
    /// Derive lock keys from the app and scope name, independent of visibility.
    pub fn to_keys(&self) -> (String, String) {
        match self {
            Self::GlobalApp { app_id, name } | Self::LocalApp { app_id, name } => {
                (format!("{app_id}:{name}"), name.clone())
            }
        }
    }

    /// App identifier for this scope.
    pub fn app_id(&self) -> &str {
        match self {
            Self::GlobalApp { app_id, .. } | Self::LocalApp { app_id, .. } => app_id.as_str(),
        }
    }

    /// Name of this scope.
    pub fn name(&self) -> &str {
        match self {
            Self::GlobalApp { name, .. } | Self::LocalApp { name, .. } => name.as_str(),
        }
    }
}

/// Name of the per-app lock shared by snapshot and restore.
pub const SNAPSHOT_RESTORE_LOCK_TAG: &str = "snapshot_restore";

/// Options for [`Backup::snapshot`](crate::storage::Backup::snapshot).
#[derive(Debug, Clone)]
pub struct SnapshotOpts {
    pub if_busy: BusyPolicy,
}

/// Policy when a snapshot can't be taken immediately (e.g. SQLite
/// `VACUUM INTO` hitting `SQLITE_BUSY` on a schema-change race).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPolicy {
    /// Surface a typed `Configuration { code: "backup_busy" }` error
    /// to the caller immediately. The caller decides whether to retry.
    Abort,
    /// Retry with bounded backoff.
    Retry,
}

/// Handle returned by [`Backup::snapshot`](crate::storage::Backup::snapshot) -
/// the address + integrity metadata needed to restore.
#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    /// Where the snapshot lives. Convention:
    /// `s3://<bucket>/snapshots/<app>/<ts>-<hash>.<ext>` or
    /// `file:///<path>`.
    pub uri: String,
    /// SHA-256 over the snapshot bytes, computed streamingly on the
    /// way out. The restore path re-hashes the download and refuses
    /// on mismatch.
    pub content_hash: [u8; 32],
    /// Wall-clock time of snapshot start, milliseconds since UNIX
    /// epoch.
    pub created_at_ms: u64,
}

/// Result of selecting a cell, distinguishing a missing row from SQL NULL.
#[derive(Debug)]
pub enum ScalarRead<T> {
    /// The query matched no row.
    NoRow,
    /// The row exists and the selected column is SQL NULL.
    Null,
    /// The row exists and the column holds a value.
    Value(T),
}

/// An unmask audit record. Named fields prevent accidentally swapping text values.
#[derive(Debug)]
pub struct UnmaskAuditRow<'a> {
    /// Trusted actor identity, or empty when the call carried none.
    pub actor_id: &'a str,
    /// Trusted actor role, or empty. Never populated from a claim the DB-3
    /// sanitiser rejected - that goes in `claimed_actor`.
    pub actor_role: &'a str,
    /// The REFUSED claim, serialised whole and untrusted. Kept apart from the
    /// trusted columns so an operator reading the row cannot confuse what a
    /// handler sent with what the runtime established.
    pub claimed_actor: &'a str,
    pub collection: &'a str,
    pub row_pk: &'a str,
    pub column: &'a str,
    pub classification: &'a str,
    pub reason: &'a str,
    pub outcome: &'a str,
}
