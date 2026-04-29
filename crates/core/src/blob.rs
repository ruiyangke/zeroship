//! Content-addressed blob store. The storage layer that backs `.zsdeploy`.
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

    /// Insert a blob. Idempotent — repeated puts of the same hash are
    /// no-ops.
    async fn put_blob(&self, hash: &str, data: &[u8]) -> Result<(), BlobError>;

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

    async fn put_blob(&self, hash: &str, data: &[u8]) -> Result<(), BlobError> {
        if !validate_hash_format(hash) {
            return Err(BlobError::Backend(format!(
                "malformed blob hash {hash:?}: expected 64-char lowercase hex"
            )));
        }
        let actual = sha256_hex(data);
        if actual != hash {
            return Err(BlobError::HashMismatch {
                expected: hash.to_string(),
                got: actual,
            });
        }
        let path = self.blob_path(hash);
        // Idempotent: blob already on disk → no-op. Content-addressing
        // means the bytes are identical by definition.
        if let Ok(meta) = compio::fs::metadata(&path).await {
            if meta.is_file() {
                return Ok(());
            }
        }
        if let Some(parent) = path.parent() {
            compio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("tmp");
        let owned = data.to_vec();
        let (res, _buf): (std::io::Result<()>, Vec<u8>) =
            compio::fs::write(&tmp, owned).await.into();
        res?;
        compio::fs::rename(&tmp, &path).await?;
        Ok(())
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
