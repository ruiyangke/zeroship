//! Content-addressed blob store. The storage layer that backs `.zship`.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use uuid::Uuid;
use zeroship_id::AppId;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob or manifest exceeds its byte budget")]
    TooLarge,
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
        self.put_blob_stream(hash, data.len() as u64, &mut cursor)
            .await
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

    /// Cheapest call that distinguishes "this backend is reachable and usable"
    /// from "it is not". Backs the worker's and control plane's `/readyz`.
    ///
    /// There is deliberately NO default implementation. A default `Ok(())`
    /// would make every future backend's readiness probe vacuously green while
    /// still reading like a check, which is the failure mode readiness
    /// endpoints exist to avoid.
    ///
    /// `has_blob` is NOT a substitute: a local store answers `Ok(false)` for a
    /// missing blob and cannot tell that apart from a blob root that has been
    /// unmounted out from under it.
    async fn probe(&self) -> Result<(), BlobError>;

    /// Stream a blob by hash into an already-open temp file while hashing,
    /// returning the verified byte count. This is the gateway's hot-path
    /// refill primitive: the caller (`DiskBlobCache::reserve_temp`) owns an
    /// open `compio::fs::File` created with `create_new`, and the store
    /// streams the object's bytes into it WITHOUT buffering the whole object
    /// in memory.
    ///
    /// Contract:
    /// - `out` is positioned at offset 0 and is the sole writer.
    /// - bytes are size-checked against `expected_size` (when `Some`) and
    ///   `max_bytes`; the stream is aborted the moment either is exceeded.
    /// - SHA-256 of the streamed bytes is verified against `hash` BEFORE
    ///   returning `Ok`. A mismatch is [`BlobError::HashMismatch`]; nothing
    ///   the caller publishes can be corrupt.
    /// - On any error the temp file's contents are meaningless and the
    ///   caller MUST discard it (the `DiskBlobTemp` guard unlinks on drop).
    ///
    /// There is intentionally no default implementation: every backend must
    /// provide a real, byte-verified streaming refill (no hidden buffering).
    async fn get_blob_to_file(
        &self,
        hash: &str,
        out: &compio::fs::File,
        expected_size: Option<u64>,
        max_bytes: u64,
    ) -> Result<u64, BlobError>;

    /// Manifest storage — separate keyspace from blobs.
    ///
    /// A manifest's bytes are a function of its key: `deploy_hash` is the
    /// hash of the manifest itself, so a key has exactly one right content.
    /// That is why a write never replaces a manifest already under its key.
    /// The same bytes again are success, so concurrent writers of one manifest
    /// all succeed; anything else under the key - other content, a torn or
    /// empty file - is refused with [`BlobError::Backend`] for an operator to
    /// look at, since the store cannot tell which copy is right.
    async fn put_manifest(
        &self,
        app_id: &AppId,
        deploy_hash: &str,
        json: &[u8],
    ) -> Result<(), BlobError>;

    /// Read a manifest, enforcing `MAX_MANIFEST_BYTES` before allocating its body.
    async fn get_manifest(&self, app_id: &AppId, deploy_hash: &str) -> Result<Bytes, BlobError>;

    /// Delete one per-deploy manifest object. Returns `true` when an object was
    /// present and removed, `false` when it was already absent.
    ///
    /// Content-addressed blobs under `blobs/` are NOT app-owned and are not
    /// deleted here; this only removes the manifest handle
    /// `manifests/<app_id>/<deploy_hash>.json`.
    async fn delete_manifest(&self, app_id: &AppId, deploy_hash: &str) -> Result<bool, BlobError>;

    /// Delete every manifest object owned by `app_id` (the
    /// `manifests/<app_id>/` keyspace). App archive deliberately does not call
    /// this: retained manifests are required for reversible restore.
    ///
    /// Idempotent: an empty/absent prefix, a repeated call after a full
    /// success, and objects disappearing between list and delete all return
    /// `Ok(())`. Content-addressed blobs under `blobs/` are NOT app-owned and
    /// are never deleted here (shared-blob GC is a separate design). A
    /// partial failure after bounded retries is [`BlobError::Backend`].
    async fn delete_app_manifests(&self, app_id: &AppId) -> Result<(), BlobError>;
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
///
/// A write that returns `Ok` is durable: the file is synced before it is
/// published, and every directory whose entries the write created or changed
/// is synced before the write returns. A write that finds its bytes already
/// there (a deduplicated blob, the same manifest again) syncs their directory
/// too, because the writer that put them there may not have yet. `rename` and
/// `link` are atomic but not durable, and without the directory syncs a power
/// loss can drop an entry after the caller has recorded the write as done.
///
/// Writes never recreate `blobs/`, a shard under it, or `manifests/`: those
/// exist from [`Self::new`] on, and a write that finds one missing fails. A
/// store whose volume has been unmounted then refuses writes instead of
/// filling the empty mountpoint beneath it.
#[derive(Debug, Clone)]
pub struct LocalDiskBlobStore {
    root: PathBuf,
}

impl LocalDiskBlobStore {
    /// Create the store: `<root>/blobs/` with every shard beneath it, and
    /// `<root>/manifests/`, synced so they survive a crash. Idempotent.
    ///
    /// Every shard exists before any blob is written, so a blob write only
    /// ever adds an entry to a shard that is already durable.
    ///
    /// # Errors
    ///
    /// Refuses a root whose filesystem cannot hard-link. Manifests are
    /// published with `hard_link`, the only no-clobber publish that never
    /// exposes a partial file, so a store on such a filesystem (SMB and many
    /// FUSE mounts) could never write one; it refuses here, at startup, rather
    /// than at the first deploy.
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        Self::open(root, |original, link| std::fs::hard_link(original, link))
    }

    /// [`Self::new`], with the hard link it probes the filesystem with handed in.
    fn open(
        root: PathBuf,
        hard_link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    ) -> std::io::Result<Self> {
        let blobs = root.join("blobs");
        let manifests = root.join("manifests");
        for shard in 0..=u8::MAX {
            std::fs::create_dir_all(blobs.join(format!("{shard:02x}")))?;
        }
        std::fs::create_dir_all(&manifests)?;
        probe_hard_links(&manifests, hard_link)?;
        for dir in [&blobs, &manifests, &root] {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(Self { root })
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        // Sharded: <root>/blobs/<hash[0..2]>/<hash[2..]>
        let (shard, rest) = hash.split_at(2);
        self.root.join("blobs").join(shard).join(rest)
    }

    fn manifest_path(&self, app_id: &AppId, deploy_hash: &str) -> PathBuf {
        self.root
            .join("manifests")
            .join(app_id.as_str())
            .join(format!("{deploy_hash}.json"))
    }

    /// Write `json` to a scratch file beside `path` that belongs to this
    /// writer alone. Nothing is visible under the manifest's key until
    /// [`Self::publish_manifest`].
    ///
    /// Two deploys of one artifact write the same key at the same time, so the
    /// scratch file cannot be named after the key: a shared one lets a later
    /// writer truncate the file an earlier writer is about to publish, and lets
    /// the earlier writer's publish take the file out from under the later one.
    /// `create_new` refuses a scratch file that already exists.
    async fn stage_manifest(path: &Path, json: &[u8]) -> Result<Scratch, BlobError> {
        use compio::io::AsyncWriteAtExt;

        let (scratch, mut file) = Scratch::create(
            path.with_extension(format!("json.tmp-{}", Uuid::new_v4().simple())),
        )
        .await?;
        let compio::BufResult(written, _) = file.write_all_at(json.to_vec(), 0).await;
        written?;
        // Synced before it is published, so the key never names bytes that a
        // crash can take back.
        file.sync_all().await?;
        file.close().await?;
        Ok(scratch)
    }

    /// Make sure `manifests/<app_id>/` exists and its entry in `manifests/`
    /// is durable, and return it. `manifests/` is synced even when another
    /// writer created the directory, because that writer may not have synced
    /// it yet.
    async fn app_manifest_dir(&self, app_id: &AppId) -> Result<PathBuf, BlobError> {
        let manifests = self.root.join("manifests");
        let dir = manifests.join(app_id.as_str());
        match compio::fs::create_dir(&dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(BlobError::Io(error)),
        }
        sync_dir(&manifests).await?;
        Ok(dir)
    }

    /// Publish a staged manifest under `path` without replacing anything
    /// already there: the local form of S3's conditional create.
    ///
    /// `hard_link` fails when the key exists, and never exposes a partial
    /// file, because the scratch file is complete before it gets a second
    /// name. When the key is taken, [`check_existing_manifest`] decides: the
    /// same bytes are success, so writers of one manifest all succeed, and
    /// anything else is refused. Nothing here ever removes what a key holds.
    ///
    /// The scratch file is dropped on every path out, which unlinks its name.
    async fn publish_manifest(scratch: Scratch, path: &Path, json: &[u8]) -> Result<(), BlobError> {
        match compio::fs::hard_link(scratch.path(), path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                check_existing_manifest(path, json).await
            }
            Err(error) => Err(BlobError::Io(error)),
        }
    }
}

/// Hard-link a probe file inside `dir` and remove both names, refusing with a
/// message that names the requirement when the filesystem cannot link.
fn probe_hard_links(
    dir: &Path,
    hard_link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let original = dir.join(format!(".hard-link-probe-{}", Uuid::new_v4().simple()));
    let link = original.with_extension("link");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&original)?;
    let linked = hard_link(&original, &link);
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&original);
    linked.map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "blob store directory {} is on a filesystem that cannot hard-link ({error}); \
                 manifests are published by hard link, so put the root on a filesystem \
                 that supports hard links, such as ext4, xfs, btrfs, NFS or CephFS",
                dir.display()
            ),
        )
    })
}

/// A publish found `path` taken: succeed only when it holds exactly `json`.
///
/// A manifest's bytes are a function of its key, so a key holding anything
/// else - other content, a torn or empty file, something that is not a file -
/// is a store that needs an operator, and is refused naming the key. A failure
/// to read the key is reported as I/O. Neither case removes anything: the blob
/// root is the system of record, and this writer cannot tell which copy is
/// right. The read is capped at the length of `json`.
async fn check_existing_manifest(path: &Path, json: &[u8]) -> Result<(), BlobError> {
    use compio::io::AsyncReadAtExt;

    let file = compio::fs::File::open(path).await?;
    let meta = file.metadata().await?;
    if !meta.is_file() {
        return Err(BlobError::Backend(format!(
            "manifest key {} is not a regular file",
            path.display()
        )));
    }
    let divergent = || {
        BlobError::Backend(format!(
            "manifest key {} already holds different bytes",
            path.display()
        ))
    };
    if meta.len() != json.len() as u64 {
        return Err(divergent());
    }
    let (read, existing) = file.read_exact_at(vec![0; json.len()], 0).await.into();
    read?;
    if existing == json {
        Ok(())
    } else {
        Err(divergent())
    }
}

/// A scratch file that only this writer uses, beside the key it will publish.
///
/// Dropping it removes the file, so every way out of a write cleans up: a
/// failed write, a publish that did not happen, and a caller that drops the
/// future between staging and publishing. `Drop` cannot await, so the unlink
/// is synchronous; it removes one name nothing else knows. After a manifest's
/// `hard_link` the key keeps the file and only the scratch name goes; after a
/// blob's `rename` the name is already gone.
///
/// Nothing sweeps abandoned scratch files at startup. Several processes share
/// one blob root, and a sweep in one would remove another's file in flight.
struct Scratch {
    path: PathBuf,
    /// False once the open that would have created the file has failed: the
    /// name then holds nothing this writer made.
    created: bool,
}

impl Scratch {
    /// Create `path` as a new file for writing. `create_new` refuses a name
    /// that exists, so the file is this writer's alone.
    ///
    /// The guard exists before the open is issued, so a future dropped once
    /// the kernel has created the file removes it. A drop that lands before
    /// the kernel runs the open can still leave the file: the open completes
    /// after the guard has already run. Such a file is inert, because nothing
    /// reads scratch names, and nothing removes it later.
    async fn create(path: PathBuf) -> std::io::Result<(Self, compio::fs::File)> {
        let mut scratch = Self {
            path,
            created: true,
        };
        match compio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&scratch.path)
            .await
        {
            Ok(file) => Ok((scratch, file)),
            Err(error) => {
                scratch.created = false;
                Err(error)
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.created {
            let _ = std::fs::remove_file(&self.path);
        }
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

        // Idempotent: pre-existing blob → consume the reader (so the
        // caller's stream cursor is advanced past the entry) and report
        // dedup. Two separate checks are needed, because they answer two
        // different questions.
        //
        // `verify_local_blob` asks whether the STORE holds the right bytes
        // for this hash; a truncated or corrupt local file must not
        // silently dedup.
        //
        // Hashing the supplied stream asks whether THIS CALLER has those
        // bytes, and that is the one the deploy path depends on: `unpack`
        // records a hash as satisfied on any `Ok` from here, so crediting a
        // caller who shipped something else would let a deploy claim a
        // blob it never possessed and then point an `anon` asset at
        // another tenant's content. The gateway keys its caches on the
        // bare hash with no app partition, which is sound only while this
        // check holds.
        //
        // The hash is not a secret that could stand in for the bytes: it
        // is returned as the ETag on every 200/206/304, so any reader who
        // was ever authorized keeps it, and blobs are never deleted.
        if let Ok(meta) = compio::fs::metadata(&path).await {
            if meta.is_file() {
                verify_local_blob(&path, hash).await?;
                let supplied = hash_reader_to_end(reader)?;
                if supplied != hash {
                    return Err(BlobError::HashMismatch {
                        expected: hash.to_string(),
                        got: supplied,
                    });
                }
                // The writer that put the blob here may not have synced the
                // shard yet, and this caller will record the blob as present.
                if let Some(shard) = path.parent() {
                    sync_dir(shard).await?;
                }
                return Ok(PutOutcome::Deduped);
            }
        }

        // Unique tmp suffix so concurrent writes of the same hash from
        // different deploys don't trample each other. `create_new`
        // ensures we never overwrite a partial tmp from another caller.
        let (tmp, file) =
            Scratch::create(path.with_extension(format!("tmp-{}", Uuid::new_v4().simple())))
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
                let compio::BufResult(res, _returned) = (&file).write_all_at(chunk, offset).await;
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

        // `tmp` removes its file when it drops, on the error paths and when
        // the rename fails; after a rename it names nothing.
        result?;
        compio::fs::rename(tmp.path(), &path).await?;
        if let Some(shard) = path.parent() {
            sync_dir(shard).await?;
        }
        Ok(PutOutcome::Wrote)
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

    /// Stat the `blobs/` directory `new()` created. This catches the failure
    /// this backend actually has - the blob root deleted, unmounted, or on a
    /// filesystem that has gone read-only/EIO - and costs one stat.
    async fn probe(&self) -> Result<(), BlobError> {
        let dir = self.root.join("blobs");
        let meta = compio::fs::metadata(&dir).await.map_err(BlobError::Io)?;
        if meta.is_dir() {
            Ok(())
        } else {
            Err(BlobError::Io(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "blob root is not a directory",
            )))
        }
    }

    async fn get_blob_to_file(
        &self,
        hash: &str,
        out: &compio::fs::File,
        expected_size: Option<u64>,
        max_bytes: u64,
    ) -> Result<u64, BlobError> {
        use compio::io::{AsyncReadAt, AsyncWriteAtExt};
        use sha2::Digest;

        if !validate_hash_format(hash) {
            return Err(BlobError::Backend(format!(
                "malformed blob hash {hash:?}: expected 64-char lowercase hex"
            )));
        }
        let src = self.blob_path(hash);
        // Open the source blob. The local store does NOT hard-link through
        // the supplied handle (the contract: it copies bytes into `out`),
        // so a future hard-link fast path would need a separate
        // path-publish API.
        let file = match compio::fs::File::open(&src).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BlobError::NotFound(hash.to_string()));
            }
            Err(e) => return Err(BlobError::Io(e)),
        };

        let mut hasher = sha2::Sha256::new();
        let mut offset: u64 = 0;
        let chunk_size: usize = 256 * 1024;
        loop {
            let buf: Vec<u8> = Vec::with_capacity(chunk_size);
            let compio::BufResult(res, buf) = file.read_at(buf, offset).await;
            let n = res.map_err(BlobError::Io)?;
            if n == 0 {
                break;
            }
            offset += n as u64;
            if offset > max_bytes {
                return Err(BlobError::TooLarge);
            }
            if let Some(exp) = expected_size {
                if offset > exp {
                    return Err(BlobError::Backend(format!(
                        "blob exceeds expected size {exp}"
                    )));
                }
            }
            hasher.update(&buf[..n]);
            let mut chunk: Vec<u8> = Vec::with_capacity(n);
            chunk.extend_from_slice(&buf[..n]);
            // `write_all_at` is `&mut self` on `&File`; bind a fresh shared
            // ref and borrow it mutably (the OS file offset is irrelevant —
            // positional writes).
            let mut wref: &compio::fs::File = out;
            let compio::BufResult(wres, _) = wref.write_all_at(chunk, offset - n as u64).await;
            wres.map_err(BlobError::Io)?;
        }

        if let Some(exp) = expected_size {
            if offset != exp {
                return Err(BlobError::Backend(format!(
                    "size mismatch: expected {exp}, observed {offset}"
                )));
            }
        }
        let computed = hex::encode(hasher.finalize());
        if computed != hash {
            return Err(BlobError::HashMismatch {
                expected: hash.to_string(),
                got: computed,
            });
        }
        out.sync_all().await?;
        Ok(offset)
    }

    async fn put_manifest(
        &self,
        app_id: &AppId,
        deploy_hash: &str,
        json: &[u8],
    ) -> Result<(), BlobError> {
        let path = self.manifest_path(app_id, deploy_hash);
        let dir = self.app_manifest_dir(app_id).await?;
        let scratch = Self::stage_manifest(&path, json).await?;
        Self::publish_manifest(scratch, &path, json).await?;
        // The scratch name is gone by now, so this one sync covers both the
        // key's entry and the scratch entry's removal. It runs when the key
        // already held these bytes too, because the writer that put them there
        // may not have synced the directory yet.
        sync_dir(&dir).await?;
        Ok(())
    }

    async fn get_manifest(&self, app_id: &AppId, deploy_hash: &str) -> Result<Bytes, BlobError> {
        use compio::io::AsyncReadAtExt;
        let path = self.manifest_path(app_id, deploy_hash);
        let file = match compio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BlobError::NotFound(format!("{}/{deploy_hash}", app_id.as_str())));
            }
            Err(e) => return Err(BlobError::Io(e)),
        };
        let size = file.metadata().await?.len();
        if size > crate::MAX_MANIFEST_BYTES {
            return Err(BlobError::TooLarge);
        }
        let size = usize::try_from(size).map_err(|_| BlobError::TooLarge)?;
        let (result, bytes) = file.read_exact_at(vec![0; size], 0).await.into();
        result?;
        Ok(Bytes::from(bytes))
    }

    async fn delete_manifest(&self, app_id: &AppId, deploy_hash: &str) -> Result<bool, BlobError> {
        let path = self.manifest_path(app_id, deploy_hash);
        match compio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(BlobError::Io(e)),
        }
    }

    async fn delete_app_manifests(&self, app_id: &AppId) -> Result<(), BlobError> {
        let dir = self.root.join("manifests").join(app_id.as_str());
        // `remove_dir_all` removes the whole `manifests/<app_id>/` subtree.
        // An absent directory is success (idempotent). Content-addressed
        // blobs live under `blobs/` and are untouched. compio::fs has no
        // recursive remove; std::fs is fine here — purge is a control-plane
        // op over a small manifest subtree, not a hot path (mirrors
        // `LocalFs::delete`).
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(BlobError::Io(e)),
        }
    }
}

/// Sync a directory, so the entries created, renamed or removed in it survive
/// a crash. The same shape as `sync_dir` in `zeroship-storage`'s local
/// backend, returning the `std::io::Error` this crate converts from.
async fn sync_dir(dir: &Path) -> std::io::Result<()> {
    compio::fs::File::open(dir).await?.sync_all().await
}

/// Size/hash-verify a local blob file before trusting a dedup hit. Returns
/// `HashMismatch` (or a backend error) on any divergence so a truncated or
/// tampered local file never silently dedups.
/// Read `reader` to EOF and return the hex SHA-256 of everything it
/// yielded. Streams in fixed chunks so a caller-supplied length can never
/// drive the allocation.
pub(crate) fn hash_reader_to_end(reader: &mut dyn std::io::Read) -> Result<String, BlobError> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let mut scratch = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut scratch).map_err(BlobError::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&scratch[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

async fn verify_local_blob(path: &std::path::Path, hash: &str) -> Result<(), BlobError> {
    let data = match compio::fs::read(path).await {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read the app's manifest directory")
            .map(|entry| entry.expect("entry").file_name().into_string().expect("utf-8"))
            .collect();
        names.sort();
        names
    }

    /// The interleaving two identical deploys produce when both reach
    /// `put_manifest`: each writer stages the manifest before either publishes
    /// it. The writer that publishes second still succeeds, the key holds the
    /// complete manifest after each publish, and neither scratch file remains.
    #[compio::test]
    async fn two_writers_staged_before_either_publishes_both_publish() {
        let root = tempfile::tempdir().expect("blob root");
        let store = LocalDiskBlobStore::new(root.path().to_path_buf()).expect("blob store");
        let app_id = AppId::mint();
        let deploy_hash = sha256_hex(b"raced-deploy");
        let json = br#"{"version":1,"rules":[],"assets":{}}"#;
        let path = store.manifest_path(&app_id, &deploy_hash);
        store.app_manifest_dir(&app_id).await.expect("the app's manifest directory");

        let first = LocalDiskBlobStore::stage_manifest(&path, json)
            .await
            .expect("the first writer stages");
        let second = LocalDiskBlobStore::stage_manifest(&path, json)
            .await
            .expect("the second writer stages");
        assert!(
            matches!(
                store.get_manifest(&app_id, &deploy_hash).await,
                Err(BlobError::NotFound(_))
            ),
            "a staged manifest is not visible under its key"
        );

        LocalDiskBlobStore::publish_manifest(first, &path, json)
            .await
            .expect("the first writer publishes");
        let got = store.get_manifest(&app_id, &deploy_hash).await.unwrap();
        assert_eq!(got.as_ref(), json);

        LocalDiskBlobStore::publish_manifest(second, &path, json)
            .await
            .expect("the second writer publishes after the first");
        let got = store.get_manifest(&app_id, &deploy_hash).await.unwrap();
        assert_eq!(got.as_ref(), json);
        assert_eq!(
            entries(path.parent().expect("app directory")),
            vec![format!("{deploy_hash}.json")]
        );
    }

    /// A staged manifest whose publish never happens takes its scratch file
    /// with it, and nothing appears under the key.
    #[compio::test]
    async fn a_staged_manifest_dropped_unpublished_leaves_nothing() {
        let root = tempfile::tempdir().expect("blob root");
        let store = LocalDiskBlobStore::new(root.path().to_path_buf()).expect("blob store");
        let app_id = AppId::mint();
        let deploy_hash = sha256_hex(b"abandoned-deploy");
        let json = br#"{"version":1,"rules":[],"assets":{}}"#;
        let path = store.manifest_path(&app_id, &deploy_hash);
        store.app_manifest_dir(&app_id).await.expect("the app's manifest directory");

        let staged = LocalDiskBlobStore::stage_manifest(&path, json)
            .await
            .expect("stages");
        let app_dir = path.parent().expect("app directory");
        assert_eq!(entries(app_dir).len(), 1, "the scratch file exists while staged");
        drop(staged);
        assert!(entries(app_dir).is_empty(), "the scratch file outlived its writer");
    }

    /// A scratch file is only ever created new. A name that already holds a
    /// file is refused, and the refused guard leaves that file alone: it
    /// belongs to whoever made it, not to this writer.
    #[compio::test]
    async fn a_scratch_file_never_takes_over_an_existing_name() {
        let root = tempfile::tempdir().expect("scratch directory");
        let taken = root.path().join("key.json.tmp-taken");
        std::fs::write(&taken, b"another writer's bytes").expect("plant a file");

        let refused = Scratch::create(taken.clone()).await;
        assert_eq!(
            refused.map(|_| ()).map_err(|e| e.kind()),
            Err(std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(
            std::fs::read(&taken).expect("the planted file survives"),
            b"another writer's bytes"
        );
    }

    /// A root whose filesystem refuses hard links is refused when the store
    /// opens, with a message naming the requirement, and the probe leaves no
    /// file behind. The refusal is driven through the seam `open` takes,
    /// because no filesystem without hard links (SMB, many FUSE mounts) can be
    /// mounted by an unprivileged test. EPERM is what Linux returns for a
    /// filesystem that does not support links.
    #[test]
    fn a_root_that_cannot_hard_link_is_refused_at_startup() {
        let root = tempfile::tempdir().expect("blob root");
        let refused = LocalDiskBlobStore::open(root.path().to_path_buf(), |_, _| {
            Err(std::io::Error::from_raw_os_error(1))
        })
        .expect_err("a root without hard links is refused");
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            refused.to_string().contains("cannot hard-link"),
            "the refusal names the requirement: {refused}"
        );
        assert!(entries(&root.path().join("manifests")).is_empty(), "the probe left a file");

        // The control: the real filesystem under the same root links, and
        // opening leaves nothing behind either.
        LocalDiskBlobStore::new(root.path().to_path_buf()).expect("a local filesystem links");
        assert!(entries(&root.path().join("manifests")).is_empty(), "the probe left a file");
    }
}
