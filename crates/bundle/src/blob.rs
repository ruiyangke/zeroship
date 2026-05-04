//! Content-addressed blob store. The storage layer that backs `.zsapp`.
//!
//! See `docs/architecture/blob-store.md` for the full design.

use std::path::PathBuf;

use bytes::Bytes;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob not found: {0}")]
    NotFound(String),
    #[error("hash mismatch: expected {expected}, got {got}")]
    HashMismatch { expected: String, got: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend: {0}")]
    Backend(String),
}

// ---------------------------------------------------------------------------
// Outcome of a put — fresh write or content-addressed dedup.
// ---------------------------------------------------------------------------

/// What happened on a successful `put_blob` / `put_blob_stream`. The
/// ingest pipeline uses this to count `blobs_uploaded` vs.
/// `blobs_deduped` without doing a separate `has_blob` round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The bytes were written to the store as a fresh blob.
    Wrote,
    /// The blob already existed (content-addressed dedup hit). For
    /// streaming puts the reader was drained to advance the caller's
    /// stream cursor; bytes were not re-persisted.
    Deduped,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Content-addressed blob store. Implementations: `LocalDiskBlobStore`
/// (single-host / dev), and later `S3BlobStore` + `CachedBlobStore`.
///
/// Futures are not `Send` — compio is thread-per-core and tasks stay on
/// their origin thread. The trait itself is `Send + Sync` so an
/// `Arc<dyn BlobStore>` can be shared across threads (each thread runs
/// its own compio runtime and serves requests to the shared store).
#[async_trait::async_trait(?Send)]
pub trait BlobStore: Send + Sync + std::fmt::Debug {
    /// Fetch a blob by hash. Allocates.
    async fn get_blob(&self, hash: &str) -> Result<Bytes, BlobError>;

    /// Local on-disk path of a blob, if file-backed. Used by the gateway
    /// for mmap / sendfile zero-copy. Returns None for purely remote
    /// backends.
    fn local_path(&self, hash: &str) -> Option<PathBuf>;

    /// Single-shot convenience for in-memory bytes. Default impl wraps
    /// in a `Cursor` and delegates to `put_blob_stream`. Implementations
    /// MAY override for a buffered fast path, but the default is correct
    /// for any backend that has a working streaming put.
    async fn put_blob(&self, hash: &str, data: &[u8]) -> Result<PutOutcome, BlobError> {
        let mut cursor = std::io::Cursor::new(data);
        self.put_blob_stream(hash, data.len() as u64, &mut cursor).await
    }

    /// Stream a blob into storage. The reader is consumed up to
    /// `expected_size` bytes; SHA-256 is computed during the read, and
    /// the persisted blob is committed atomically only if the computed
    /// hash matches `hash` AND the byte count matches `expected_size`.
    /// On size or hash mismatch, the partial write is removed.
    ///
    /// Idempotent: pre-existing blob → drain the reader (so the caller's
    /// stream cursor advances past the entry) and return
    /// `Ok(PutOutcome::Deduped)` without rewriting.
    async fn put_blob_stream(
        &self,
        hash: &str,
        expected_size: u64,
        reader: &mut dyn std::io::Read,
    ) -> Result<PutOutcome, BlobError>;

    async fn has_blob(&self, hash: &str) -> Result<bool, BlobError>;

    /// Manifest storage — separate keyspace from blobs.
    async fn put_manifest(
        &self,
        app_id: &Uuid,
        deploy_hash: &str,
        json: &[u8],
    ) -> Result<(), BlobError>;

    async fn get_manifest(
        &self,
        app_id: &Uuid,
        deploy_hash: &str,
    ) -> Result<Bytes, BlobError>;
}

// ---------------------------------------------------------------------------
// Hash helpers
// ---------------------------------------------------------------------------

/// SHA-256 of `data`, lowercase hex.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    hex::encode(digest)
}

/// True iff `hash` is a 64-char lowercase hex string.
#[must_use]
pub fn validate_hash_format(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

// ---------------------------------------------------------------------------
// LocalDiskBlobStore
// ---------------------------------------------------------------------------

/// File-backed blob store. Layout:
///
/// ```text
/// <root>/blobs/<hash[0..2]>/<hash[2..]>
/// <root>/manifests/<app_id>/<deploy_hash>.json
/// ```
///
/// `local_path` always returns `Some(path)` regardless of whether the
/// file currently exists on disk — callers that care should `get_blob`
/// or `has_blob`. This keeps the accessor cheap (no syscall).
#[derive(Debug, Clone)]
pub struct LocalDiskBlobStore {
    root: PathBuf,
}

impl LocalDiskBlobStore {
    /// Create the store, ensuring `<root>/blobs/` and `<root>/manifests/`
    /// exist. Idempotent.
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(root.join("blobs"))?;
        std::fs::create_dir_all(root.join("manifests"))?;
        Ok(Self { root })
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        // Sharded: <root>/blobs/<hash[0..2]>/<hash[2..]>
        let (shard, rest) = hash.split_at(2);
        self.root.join("blobs").join(shard).join(rest)
    }

    fn manifest_path(&self, app_id: &Uuid, deploy_hash: &str) -> PathBuf {
        self.root
            .join("manifests")
            .join(app_id.to_string())
            .join(format!("{deploy_hash}.json"))
    }
}

#[async_trait::async_trait(?Send)]
impl BlobStore for LocalDiskBlobStore {
    async fn get_blob(&self, hash: &str) -> Result<Bytes, BlobError> {
        if !validate_hash_format(hash) {
            return Err(BlobError::Backend(format!(
                "malformed blob hash {hash:?}: expected 64-char lowercase hex"
            )));
        }
        let path = self.blob_path(hash);
        let data = match compio::fs::read(&path).await {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BlobError::NotFound(hash.to_string()));
            }
            Err(e) => return Err(BlobError::Io(e)),
        };
        let actual = sha256_hex(&data);
        if actual != hash {
            return Err(BlobError::HashMismatch {
                expected: hash.to_string(),
                got: actual,
            });
        }
        Ok(Bytes::from(data))
    }

    fn local_path(&self, hash: &str) -> Option<PathBuf> {
        if !validate_hash_format(hash) {
            return None;
        }
        Some(self.blob_path(hash))
    }

    async fn put_blob_stream(
        &self,
        hash: &str,
        expected_size: u64,
        reader: &mut dyn std::io::Read,
    ) -> Result<PutOutcome, BlobError> {
        if !validate_hash_format(hash) {
            return Err(BlobError::Backend(format!(
                "malformed blob hash {hash:?}: expected 64-char lowercase hex"
            )));
        }
        let path = self.blob_path(hash);

        // Idempotent: pre-existing blob → drain the reader (so the
        // caller's stream cursor is advanced past the entry) and
        // report dedup. Content-addressing means the bytes on disk are
        // identical to whatever the caller would have written.
        if let Ok(meta) = compio::fs::metadata(&path).await {
            if meta.is_file() {
                std::io::copy(reader, &mut std::io::sink())
                    .map_err(BlobError::Io)?;
                return Ok(PutOutcome::Deduped);
            }
        }

        if let Some(parent) = path.parent() {
            compio::fs::create_dir_all(parent).await?;
        }

        // Unique tmp suffix so concurrent writes of the same hash from
        // different deploys don't trample each other. `create_new`
        // ensures we never overwrite a partial tmp from another caller.
        let tmp = path.with_extension(format!("tmp-{}", Uuid::new_v4().simple()));

        let file = compio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await?;

        let result: Result<(), BlobError> = async {
            use compio::io::AsyncWriteAtExt;
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            let mut total: u64 = 0;
            let chunk_size: usize = 64 * 1024;
            let mut scratch: Vec<u8> = vec![0u8; chunk_size];
            let mut offset: u64 = 0;

            loop {
                // Sync read: tar/zstd are CPU-only over in-memory bytes.
                let n = reader.read(&mut scratch).map_err(BlobError::Io)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                if total > expected_size {
                    return Err(BlobError::Backend(format!(
                        "blob exceeds declared size {expected_size}"
                    )));
                }
                hasher.update(&scratch[..n]);

                // compio's write_at consumes the buffer and hands it
                // back via BufResult; allocate a fresh owned chunk per
                // write (64 KiB allocations are cheap, ~hundreds of ns).
                let mut chunk: Vec<u8> = Vec::with_capacity(n);
                chunk.extend_from_slice(&scratch[..n]);
                let compio::BufResult(res, _returned) =
                    (&file).write_all_at(chunk, offset).await;
                res.map_err(BlobError::Io)?;
                offset += n as u64;
            }

            if total != expected_size {
                return Err(BlobError::Backend(format!(
                    "size mismatch: expected {expected_size}, observed {total}"
                )));
            }
            let computed = hex::encode(hasher.finalize());
            if computed != hash {
                return Err(BlobError::HashMismatch {
                    expected: hash.to_string(),
                    got: computed,
                });
            }
            file.sync_all().await?;
            Ok(())
        }
        .await;

        // Drop the file handle before the rename; compio::fs::File
        // closes on drop.
        drop(file);

        match result {
            Ok(()) => {
                compio::fs::rename(&tmp, &path).await?;
                Ok(PutOutcome::Wrote)
            }
            Err(e) => {
                let _ = compio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }

    async fn has_blob(&self, hash: &str) -> Result<bool, BlobError> {
        if !validate_hash_format(hash) {
            return Ok(false);
        }
        let path = self.blob_path(hash);
        match compio::fs::metadata(&path).await {
            Ok(m) => Ok(m.is_file()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(BlobError::Io(e)),
        }
    }

    async fn put_manifest(
        &self,
        app_id: &Uuid,
        deploy_hash: &str,
        json: &[u8],
    ) -> Result<(), BlobError> {
        let path = self.manifest_path(app_id, deploy_hash);
        if let Some(parent) = path.parent() {
            compio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("json.tmp");
        let owned = json.to_vec();
        let (res, _buf): (std::io::Result<()>, Vec<u8>) =
            compio::fs::write(&tmp, owned).await.into();
        res?;
        compio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    async fn get_manifest(
        &self,
        app_id: &Uuid,
        deploy_hash: &str,
    ) -> Result<Bytes, BlobError> {
        let path = self.manifest_path(app_id, deploy_hash);
        match compio::fs::read(&path).await {
            Ok(v) => Ok(Bytes::from(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(BlobError::NotFound(
                format!("{app_id}/{deploy_hash}"),
            )),
            Err(e) => Err(BlobError::Io(e)),
        }
    }
}
