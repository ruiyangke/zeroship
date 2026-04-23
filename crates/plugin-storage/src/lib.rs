//! Storage plugin — `zeroship.storage.*` native primitives.
//!
//! Provides object-store CRUD backed (for now) by the local filesystem.
//! S3 / R2 / GCS backends go behind the same `Backend` trait so switching
//! is a one-liner at deploy time.
//!
//! Native API surface (what @zeroship/storage SDK wraps):
//! - `zeroship.storage.put(bucket, key, bytesBase64, contentType?)` → Promise<{ bucket, key, size }>
//! - `zeroship.storage.get(bucket, key)` → Promise<{ bytesBase64, contentType, size } | null>
//! - `zeroship.storage.delete(bucket, key)` → Promise<{ deleted: bool }>
//! - `zeroship.storage.list(bucket, prefix)` → Promise<[{ key, size, modifiedAt }]>
//!
//! Each app gets its own directory under the storage root: `<root>/<app_id>/<bucket>/<key>`.
//! Keys may contain `/` — treated as subpaths. Path-traversal (`..`) is rejected.

use std::cell::RefCell;
use std::path::PathBuf;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

pub mod backend;
pub mod callbacks;

// ---------------------------------------------------------------------------
// Thread-local state
// ---------------------------------------------------------------------------

thread_local! {
    /// Root directory for the local filesystem backend. Poisoned during
    /// `register()` so callbacks can find it without threading state
    /// through every op.
    pub(crate) static STORAGE_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// StoragePlugin
// ---------------------------------------------------------------------------

/// The storage plugin — registers `zeroship.storage.*` methods.
pub struct StoragePlugin {
    root: PathBuf,
}

impl std::fmt::Debug for StoragePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoragePlugin").field("root", &self.root).finish()
    }
}

impl StoragePlugin {
    /// Create a new `StoragePlugin` backed by a local filesystem directory.
    ///
    /// The directory is created on first use if it doesn't exist. For
    /// production, swap this out for an S3-backed constructor once the
    /// `Backend` trait has an S3 impl.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl NativePlugin for StoragePlugin {
    fn namespace(&self) -> &str {
        "storage"
    }

    fn name(&self) -> &str {
        "storage"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        STORAGE_ROOT.with(|cell| {
            *cell.borrow_mut() = Some(self.root.clone());
        });
        r.add("put", callbacks::put);
        r.add("get", callbacks::get);
        r.add("delete", callbacks::delete);
        r.add("list", callbacks::list);
    }
}
