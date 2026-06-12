//! Backend abstraction for storage operations.
//!
//! Multiple implementations slot in behind the `Backend` trait:
//! - `LocalFs` — filesystem-backed, always available (the dev default)
//! - `S3` — S3/R2/MinIO/Spaces/B2 via S3-API (behind the `s3` feature)
//!
//! The trait methods are async + `Send + Sync` so they can live behind
//! `Arc<dyn Backend>` and survive the compio executor's work-stealing.
//!
//! Error type is `String` — every op ultimately surfaces to JS via the
//! callback layer's `OpResult::Failed { error: String }`, so anything
//! richer would get flattened there anyway.
//!
//! ## Streaming is the kernel; buffered is a convenience
//!
//! `put_stream` / `get_stream` are the primitive ops. The whole-object
//! `put` / `get` are thin wrappers built on top of them (no per-backend
//! whole-object buffering ceiling — memory is bounded by the part size
//! on upload and by the consumer's pull rate on download). The S3 backend
//! turns `put_stream` into an S3 multipart upload (bounded `PART_SIZE`
//! parts), so an arbitrarily large object never lands fully in RAM.

use std::time::SystemTime;

use bytes::Bytes;

pub mod local;
pub use local::LocalFs;

#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "s3")]
pub use s3::S3;

// ---------------------------------------------------------------------------
// Types shared by all backends
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub size: u64,
    pub content_type: Option<String>,
    pub modified_at: SystemTime,
}

#[derive(Debug, Clone)]
pub struct ListEntry {
    pub key: String,
    pub size: u64,
    pub modified_at: SystemTime,
}

// ---------------------------------------------------------------------------
// Chunk streams — the dyn-compatible streaming seam
// ---------------------------------------------------------------------------

/// One chunk of a streamed object, or a terminal error. `None` from
/// [`ChunkSource::next_chunk`] means clean EOF.
pub type ChunkResult = Result<Bytes, String>;

/// A single-pass, single-threaded async source of object bytes.
///
/// This is the input to [`Backend::put_stream`] and the output of
/// [`Backend::get_stream`]. We use a boxed trait object rather than the
/// proposal's loose `impl Stream<Item = …>` because `Backend` is consumed
/// as `Arc<dyn Backend>`; `impl Trait` / generic method params are not
/// object-safe. `next_chunk` is the only operation any backend needs.
///
/// `?Send`: every storage op runs on the worker thread that owns the V8
/// isolate + compio runtime, mirroring the `Backend(?Send)` contract.
#[async_trait::async_trait(?Send)]
pub trait ChunkSource {
    /// Yield the next chunk, `None` at EOF, or `Some(Err(_))` on a fatal
    /// upstream error (e.g. the V8 ReadableStream rejected mid-read).
    async fn next_chunk(&mut self) -> Option<ChunkResult>;
}

/// A [`ChunkSource`] that replays a single in-memory buffer once. Lets the
/// buffered convenience `put` flow through the streaming path without a
/// second code path on each backend.
#[derive(Debug)]
pub struct OnceChunk(Option<Bytes>);

impl OnceChunk {
    #[must_use]
    pub fn new(bytes: Bytes) -> Self {
        Self(if bytes.is_empty() { None } else { Some(bytes) })
    }
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for OnceChunk {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        self.0.take().map(Ok)
    }
}

/// Boxed source handed to [`Backend::put_stream`].
pub type BoxChunkSource = Box<dyn ChunkSource>;

/// Boxed source returned by [`Backend::get_stream`].
pub type BoxByteStream = Box<dyn ChunkSource>;

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// Pluggable object-storage backend. All implementations see the same
/// namespacing contract — the kernel enforces `<app_id>/<bucket>/<key>`
/// separation before the backend sees anything, so implementations only
/// worry about their own I/O, not multi-tenancy.
///
/// Path-traversal rejection lives in `validate_object_coords` below, not
/// per-backend — every implementation gets it for free by calling the
/// helper at the top of each op.
///
/// **Streaming is the kernel.** `put_stream` / `get_stream` are the
/// required ops; `put` / `get` have default impls that drive the streaming
/// path (so adding a backend means implementing exactly two streaming
/// methods plus `delete` / `list`).
// Trait is `Send + Sync` so it can live behind `Arc<dyn Backend>` on a
// `NativePlugin` (which requires Send+Sync). Methods return `!Send`
// futures via `(?Send)` because compio's async fs ops hold thread-local
// state. That's fine — storage ops are always awaited on the same thread
// that owns the worker's V8 isolate.
#[async_trait::async_trait(?Send)]
pub trait Backend: Send + Sync + std::fmt::Debug {
    /// Stream an object in. The backend pulls `body` chunk-by-chunk; nothing
    /// requires the whole object to be resident. Returns total bytes written.
    async fn put_stream(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        body: BoxChunkSource,
        content_type: Option<&str>,
    ) -> Result<u64, String>;

    /// Stream an object out. `Ok(None)` if the key is absent; otherwise the
    /// metadata plus a [`ChunkSource`] the caller pulls to EOF.
    async fn get_stream(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, String>;

    async fn delete(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<bool, String>;

    async fn list(
        &self,
        app_id: &str,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<ListEntry>, String>;

    // -- Buffered conveniences, built on the streaming path ----------------

    /// Buffered put — wraps `bytes` in a [`OnceChunk`] and drives
    /// [`Backend::put_stream`]. Used by the base64 convenience callback for
    /// small objects; large objects go straight through `put_stream`.
    async fn put(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> Result<u64, String> {
        let src: BoxChunkSource = Box::new(OnceChunk::new(Bytes::copy_from_slice(bytes)));
        self.put_stream(app_id, bucket, key, src, content_type).await
    }

    /// Buffered get — drains [`Backend::get_stream`] into a single `Vec<u8>`.
    /// The convenience callback uses this for the base64 return shape.
    async fn get(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, ObjectMeta)>, String> {
        let Some((meta, mut stream)) = self.get_stream(app_id, bucket, key).await? else {
            return Ok(None);
        };
        let mut buf = Vec::with_capacity(meta.size as usize);
        while let Some(chunk) = stream.next_chunk().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(Some((buf, meta)))
    }
}

// ---------------------------------------------------------------------------
// Shared validation — every backend calls this before any I/O.
// ---------------------------------------------------------------------------

/// Reject inputs that would escape an app's keyspace. Used by every
/// `Backend` impl; shipping it centrally means adding a new backend
/// can't accidentally forget the check.
pub fn validate_object_coords(app_id: &str, bucket: &str, key: &str) -> Result<(), String> {
    if app_id.is_empty() || bucket.is_empty() || key.is_empty() {
        return Err("storage: app_id/bucket/key must all be non-empty".into());
    }
    if app_id.contains('/') || app_id.contains("..") {
        return Err(format!("storage: invalid app_id '{app_id}'"));
    }
    if bucket.contains('/') || bucket == "." || bucket == ".." {
        return Err(format!("storage: invalid bucket name '{bucket}'"));
    }
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("storage: invalid key '{key}'"));
        }
    }
    Ok(())
}

/// The slimmer app_id/bucket check used by `list` (which allows an empty
/// key — that's what "list all" means). Shared so `LocalFs` and `S3`
/// reject the same inputs.
pub fn validate_list_coords(app_id: &str, bucket: &str) -> Result<(), String> {
    if app_id.contains('/') || app_id.contains("..") || app_id.is_empty() {
        return Err(format!("storage: invalid app_id '{app_id}'"));
    }
    if bucket.contains('/') || bucket == "." || bucket == ".." || bucket.is_empty() {
        return Err(format!("storage: invalid bucket name '{bucket}'"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_object_coords("app", "uploads", "../escape").is_err());
        assert!(validate_object_coords("app", "uploads", "a/../b").is_err());
        assert!(validate_object_coords("app", "uploads", ".").is_err());
        assert!(validate_object_coords("app", "uploads", "").is_err());
    }

    #[test]
    fn rejects_bad_buckets() {
        assert!(validate_object_coords("app", "a/b", "key").is_err());
        assert!(validate_object_coords("app", "..", "key").is_err());
    }

    #[test]
    fn rejects_bad_app_id() {
        assert!(validate_object_coords("a/b", "uploads", "key").is_err());
        assert!(validate_object_coords("..", "uploads", "key").is_err());
    }

    #[test]
    fn allows_nested_keys() {
        assert!(validate_object_coords("app", "uploads", "avatars/2026/u.png").is_ok());
    }

    #[test]
    fn list_coords_rejects_bad_inputs() {
        assert!(validate_list_coords("a/b", "uploads").is_err());
        assert!(validate_list_coords("app", "a/b").is_err());
        assert!(validate_list_coords("app", "uploads").is_ok());
    }

    #[compio::test]
    async fn once_chunk_yields_once() {
        let mut src = OnceChunk::new(Bytes::from_static(b"hello"));
        assert_eq!(src.next_chunk().await.unwrap().unwrap().as_ref(), b"hello");
        assert!(src.next_chunk().await.is_none());
    }

    #[compio::test]
    async fn once_chunk_empty_is_immediate_eof() {
        let mut src = OnceChunk::new(Bytes::new());
        assert!(src.next_chunk().await.is_none());
    }
}
