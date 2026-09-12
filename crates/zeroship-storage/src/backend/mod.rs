//! Backend abstraction for storage operations.
//!
//! Multiple implementations slot in behind the `Backend` trait:
//! - `LocalFs` — filesystem-backed, always available (the dev default)
//! - `S3` — S3/R2/MinIO/Spaces/B2 via S3-API (behind the `s3` feature)
//!
//! Backends are shareable across threads; operation futures stay on the caller's
//! compio runtime. Rust callers and language bindings receive typed errors.
//!
//! ## Streaming is the kernel; buffered is a convenience
//!
//! `put_stream` / `get_stream` are the primitive ops. Buffered operations build
//! on them; `get` checks the caller's cap before buffering. Scoped `Storage`
//! handles enforce buffered and streamed-upload limits across backends. S3
//! uploads bound working memory by part size and concurrency, while downloads
//! advance at the consumer's pull rate.

use crate::StorageError;

use std::time::SystemTime;

use bytes::Bytes;

pub mod local;
pub use local::LocalFs;

#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "s3")]
pub use s3::{S3UploadTuning, S3};

/// The content type an object is advertised with when the writer supplied
/// none. Every backend applies it at its own `put_stream` boundary, so a
/// `put(.., None)` reads back identically whichever backend is configured —
/// this is the S3/HTTP convention, and `LocalFs` matches it rather than
/// inventing a `None` that only one backend can produce.
pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

// ---------------------------------------------------------------------------
// Types shared by all backends
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub size: u64,
    /// The object's content type. Backends resolve an absent writer-supplied
    /// type to [`DEFAULT_CONTENT_TYPE`] on the way in, so a *stored* object
    /// always reports `Some(_)`. `None` is reserved for callers that
    /// synthesise an `ObjectMeta` without going through a backend.
    pub content_type: Option<String>,
    pub modified_at: SystemTime,
}

#[derive(Debug, Clone)]
pub struct ListEntry {
    pub key: String,
    pub size: u64,
    pub modified_at: SystemTime,
}

/// One page request for [`Backend::list`].
///
/// `list` is paginated on every backend because the alternative — hand back
/// whatever the bucket happens to hold — makes the cost of a single app
/// request scale with that app's stored-object count, on a thread shared with
/// every co-resident app.
#[derive(Debug, Clone, Copy)]
pub struct ListRequest<'a> {
    /// Literal key prefix (not a glob). Empty lists the whole app-bucket.
    pub prefix: &'a str,
    /// Resume strictly AFTER this key. `None` starts at the beginning.
    /// Always a key a previous page returned as [`ListPage::cursor`].
    pub cursor: Option<&'a str>,
    /// Maximum entries this page may contain. Backends MUST NOT exceed it.
    /// Scoped handles apply the configured pagination bounds before dispatch.
    pub limit: usize,
}

/// One page of a listing.
///
/// The `cursor` is what makes a truncated listing distinguishable from a
/// complete one. A silent cap would be worse than the unbounded listing it
/// replaced: it looks like a complete answer.
#[derive(Debug, Clone)]
pub struct ListPage {
    /// Entries in ascending key order, at most `ListRequest::limit` of them.
    pub entries: Vec<ListEntry>,
    /// `Some(key)` iff MORE entries exist beyond this page — pass it back as
    /// [`ListRequest::cursor`]. `None` means the listing is COMPLETE.
    ///
    /// It is the last key of this page rather than an opaque token so that
    /// both backends can resume from it natively (a lexicographic walk
    /// position on `LocalFs`, S3's `start-after` on the S3 leg) and so paging
    /// is not tied to one backend's continuation format.
    pub cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// Chunk streams — the dyn-compatible streaming seam
// ---------------------------------------------------------------------------

/// One chunk of a streamed object, or a terminal error. `None` from
/// [`ChunkSource::next_chunk`] means clean EOF.
pub type ChunkResult = Result<Bytes, StorageError>;

/// A single-pass, single-threaded async source of object bytes.
///
/// This is the input to [`Backend::put_stream`] and the output of
/// [`Backend::get_stream`]. We use a boxed trait object rather than the
/// proposal's loose `impl Stream<Item = …>` because `Backend` is consumed
/// as `Arc<dyn Backend>`; `impl Trait` / generic method params are not
/// object-safe. `next_chunk` is the only operation any backend needs.
///
/// `?Send`: operations stay on the caller's compio thread, mirroring the
/// `Backend(?Send)` contract.
#[async_trait::async_trait(?Send)]
pub trait ChunkSource {
    /// Yield the next chunk, `None` at EOF, or `Some(Err(_))` on a fatal
    /// upstream error (for example, a producer failing during an upload).
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

/// Low-level object storage for trusted hosts and backend implementers.
/// Application callers receive a namespace-bound [`crate::Storage`] handle,
/// which validates coordinates and limits before invoking this trait.
///
/// Implementations own streaming upload/download, deletion and pagination.
/// Buffered methods drain the streaming path. The backend is shareable across
/// threads, while each operation's future runs on its calling compio runtime.
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
    ) -> Result<u64, StorageError>;

    /// Stream an object out. `Ok(None)` if the key is absent; otherwise the
    /// metadata plus a [`ChunkSource`] the caller pulls to EOF.
    async fn get_stream(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, StorageError>;

    async fn delete(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<bool, StorageError>;

    /// List one page of keys. Entries come back in ascending key order, and
    /// never more than `req.limit` of them; [`ListPage::cursor`] tells the
    /// caller whether more remain. There is deliberately no "list everything"
    /// entry point — see [`ListRequest`].
    async fn list(
        &self,
        app_id: &str,
        bucket: &str,
        req: ListRequest<'_>,
    ) -> Result<ListPage, StorageError>;

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
    ) -> Result<u64, StorageError> {
        let src: BoxChunkSource = Box::new(OnceChunk::new(Bytes::copy_from_slice(bytes)));
        self.put_stream(app_id, bucket, key, src, content_type).await
    }

    /// Buffered get — drains [`Backend::get_stream`] into a single `Vec<u8>`,
    /// capped at `max_bytes`. The convenience callback uses this for the base64
    /// return shape; it materialises the whole object in RAM (and base64-encodes
    /// it, ~2.3× peak), so an uncapped buffered `get` driven by an
    /// attacker-controlled `Content-Length` is an OOM-DoS. The cap is enforced
    /// against BOTH the advertised `meta.size` (rejected before allocating) AND
    /// the running total (rejected if the body streams past the cap despite a
    /// smaller/absent advertised size). The capacity hint is clamped to the cap
    /// so a lying `Content-Length` cannot pre-allocate gigabytes.
    ///
    /// The streaming `get_stream` path stays unbounded by design — only this
    /// buffered convenience is capped.
    async fn get(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        max_bytes: u64,
    ) -> Result<Option<(Vec<u8>, ObjectMeta)>, StorageError> {
        let Some((meta, mut stream)) = self.get_stream(app_id, bucket, key).await? else {
            return Ok(None);
        };
        if meta.size > max_bytes {
            return Err(StorageError::LimitExceeded(format!(
                "storage: object size {} exceeds buffered-get cap {max_bytes} \
                 (use streaming getStream for large objects)",
                meta.size
            )));
        }
        // Clamp the capacity hint to the cap — never trust the advertised size
        // to pre-allocate beyond what we are willing to buffer.
        let cap_hint = usize::try_from(meta.size.min(max_bytes)).unwrap_or(usize::MAX);
        let mut buf = Vec::with_capacity(cap_hint);
        while let Some(chunk) = stream.next_chunk().await {
            let chunk = chunk?;
            if buf.len() as u64 + chunk.len() as u64 > max_bytes {
                return Err(StorageError::LimitExceeded(format!(
                    "storage: object body exceeds buffered-get cap {max_bytes} \
                     (use streaming getStream for large objects)"
                )));
            }
            buf.extend_from_slice(&chunk);
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
pub fn validate_object_coords(app_id: &str, bucket: &str, key: &str) -> Result<(), StorageError> {
    validate_list_coords(app_id, bucket)?;
    validate_key(key)
}

fn validate_key(key: &str) -> Result<(), StorageError> {
    if key.contains(['\\', '\0']) {
        return Err(StorageError::InvalidArgument("storage: invalid key".into()));
    }
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(StorageError::InvalidArgument(format!("storage: invalid key '{key}'")));
        }
    }
    Ok(())
}

/// The slimmer app_id/bucket check used by `list` (which allows an empty
/// key — that's what "list all" means). Shared so `LocalFs` and `S3`
/// reject the same inputs.
pub fn validate_list_coords(app_id: &str, bucket: &str) -> Result<(), StorageError> {
    if app_id.contains(['/', '\\', '\0']) || app_id == "." || app_id.contains("..") || app_id.is_empty() {
        return Err(StorageError::InvalidArgument(format!("storage: invalid app_id '{app_id}'")));
    }
    if bucket.contains(['/', '\\', '\0']) || bucket == "." || bucket == ".." || bucket.is_empty() {
        return Err(StorageError::InvalidArgument(format!("storage: invalid bucket name '{bucket}'")));
    }
    Ok(())
}

/// Validate list selectors without treating their contents as a filesystem path.
/// A prefix may be empty or end at a directory separator.
pub fn validate_list_request(request: &ListRequest<'_>) -> Result<(), StorageError> {
    if !request.prefix.is_empty() {
        validate_key(request.prefix.strip_suffix('/').unwrap_or(request.prefix))?;
    }
    if let Some(cursor) = request.cursor {
        validate_key(cursor)?;
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
