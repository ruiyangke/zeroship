//! Storage plugin — `zeroship.storage.*` native primitives.
//!
//! Object-store CRUD dispatched through a pluggable `Backend` trait.
//! Ships with `LocalFs` (filesystem) today; `S3` (S3/R2/MinIO/Spaces/B2
//! via the S3 API) is behind the `s3` feature flag.
//!
//! Native API surface (what `@zeroship/storage` SDK wraps):
//!
//! Buffered (small objects):
//! - `zeroship.storage.put(bucket, key, bytesBase64, contentType?)` → Promise<{ bucket, key, size }>
//! - `zeroship.storage.get(bucket, key)` → Promise<{ bytesBase64, contentType, size } | null>
//! - `zeroship.storage.delete(bucket, key)` → Promise<{ deleted: bool }>
//! - `zeroship.storage.list(bucket, prefix)` → Promise<[{ key, size, modifiedAt }]>
//!
//! Streaming (no whole-object buffering — see `callbacks` and the proposal's
//! "env.storage streaming through V8" section):
//! - `zeroship.storage.putStream(bucket, key, ReadableStream, contentType?)`
//!   → Promise<{ bucket, key, size }>
//! - `zeroship.storage.getStream(bucket, key)`
//!   → Promise<{ streamId, contentType, size } | null>
//! - `zeroship.storage.readChunk(streamId)` → Promise<Uint8Array | undefined>
//! - `zeroship.storage.cancelStream(streamId)` → Promise<undefined>
//!
//! `put_stream` / `get_stream` on the `Backend` trait are the kernel ops;
//! buffered `put` / `get` are conveniences built on the streaming path.
//! The S3 backend turns `put_stream` into an S3 multipart upload, so memory
//! is bounded by the part size, never the object size.
//!
//! Multi-tenancy: each app's keyspace is namespaced by app_id. Backends
//! never see raw user input — `backend::validate_object_coords` gates
//! every op so path traversal can't leak across apps.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

pub mod backend;
pub mod callbacks;

pub use backend::{Backend, LocalFs};
#[cfg(feature = "s3")]
pub use backend::S3;

// ---------------------------------------------------------------------------
// Thread-local backend handle
// ---------------------------------------------------------------------------
//
// V8 isolates are single-threaded, and each worker thread owns its own
// isolate + compio runtime. Storing the backend in a thread_local avoids
// threading it through every callback signature while still being
// sound — every access is from the same thread that registered it.
//
// Use `Arc<dyn Backend>` everywhere. The plugin crosses threads during
// worker init (Arc allows that); the thread-local slot on each worker
// holds its own clone. Atomic refcount cost is paid once per register
// — not on the per-request hot path.

thread_local! {
    pub(crate) static STORAGE_BACKEND: RefCell<Option<Arc<dyn Backend>>> =
        const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// StoragePlugin
// ---------------------------------------------------------------------------

/// The storage plugin — registers `zeroship.storage.*` methods.
///
/// Construct with either:
/// - `StoragePlugin::local(path)` — filesystem backend (dev default)
/// - `StoragePlugin::with_backend(backend)` — any `Backend` impl (S3, R2, etc.)
pub struct StoragePlugin {
    backend: Arc<dyn Backend>,
}

impl std::fmt::Debug for StoragePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoragePlugin")
            .field("backend", &self.backend)
            .finish()
    }
}

impl StoragePlugin {
    /// Convenience: local filesystem backend rooted at `path`. The
    /// directory is created lazily on first write.
    #[must_use]
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self { backend: Arc::new(LocalFs::new(path)) }
    }

    /// Use any `Backend` impl. Takes `Arc<dyn Backend>` so multiple plugin
    /// instances (different namespaces/features) can share a backend
    /// without double-allocating connection pools / configs. The plugin
    /// itself crosses threads (runtime init hands it to worker threads)
    /// but the futures it produces stay thread-local.
    #[must_use]
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    /// Back-compat alias so existing CLI code (`StoragePlugin::new(path)`)
    /// keeps working during the refactor. Equivalent to `::local(path)`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::local(path)
    }
}

impl NativePlugin for StoragePlugin {
    fn namespace(&self) -> &str { "storage" }
    fn name(&self) -> &str { "storage" }

    fn register(&self, r: &mut NativeRegistrar) {
        STORAGE_BACKEND.with(|cell| {
            *cell.borrow_mut() = Some(Arc::clone(&self.backend));
        });
        r.add("put", callbacks::put);
        r.add("get", callbacks::get);
        r.add("delete", callbacks::delete);
        r.add("list", callbacks::list);
        // Streaming surface (proposal "env.storage streaming through V8"):
        // the @zeroship/storage SDK wraps these into streaming put/get.
        r.add("putStream", callbacks::put_stream);
        r.add("getStream", callbacks::get_stream);
        r.add("readChunk", callbacks::read_chunk);
        r.add("cancelStream", callbacks::cancel_stream);
    }
}
