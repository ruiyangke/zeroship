//! Edge LRU caches for blob bytes, keyed by content hash.
//!
//! - `BlobCache` is the in-memory tier (see
//!   `docs/architecture/blob-store.md`). Bytes are still copied
//!   once on socket write — `Bytes` is `Arc`-refcounted so concurrent
//!   requests for the same hash share the buffer.
//! - `DiskBlobCache` is the on-disk tier. Larger blobs that do not fit
//!   in the memory budget land on disk; on serve, the gateway `mmap`s
//!   the file and hands the pointer to ntex via
//!   `Bytes::from_owner`, so the kernel page cache → socket path is
//!   zero-copy from the userspace side.
//! - A future follow-up can swap the page-cache → socket copy for
//!   `sendfile(2)` or `IORING_OP_SPLICE`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use bytes::Bytes;
use lru::LruCache;
use sha2::{Digest, Sha256};

/// Bounded-by-bytes LRU cache keyed by blob hash. Insert of a single
/// entry larger than the budget is a no-op so one giant asset can't
/// evict everything.
pub struct BlobCache {
    state: Mutex<CacheState>,
    max_bytes: usize,
}

struct CacheState {
    lru: LruCache<String, Bytes>,
    current_bytes: usize,
}

impl BlobCache {
    /// Create a cache with the given byte budget. The cache is
    /// internally unbounded by entry-count — eviction is byte-driven.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(CacheState {
                lru: LruCache::unbounded(),
                current_bytes: 0,
            }),
            max_bytes,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            tracing::error!("BlobCache mutex poisoned; recovering cache state");
            poisoned.into_inner()
        })
    }

    /// Look up a blob, promoting the entry to most-recently-used on hit.
    pub fn get(&self, hash: &str) -> Option<Bytes> {
        let mut state = self.lock_state();
        state.lru.get(hash).cloned()
    }

    /// Insert (or replace) a blob. Evicts oldest entries until the new
    /// entry fits inside `max_bytes`. If the new entry alone exceeds
    /// `max_bytes`, this is a silent no-op — one big asset must not
    /// evict everything else.
    pub fn insert(&self, hash: String, bytes: Bytes) {
        let new_size = bytes.len();
        if new_size > self.max_bytes {
            return;
        }
        let mut state = self.lock_state();
        // Replacing an existing key needs to subtract the old size first
        // so the byte accounting tracks the delta, not the gross sum.
        if let Some(old) = state.lru.pop(&hash) {
            state.current_bytes = state.current_bytes.saturating_sub(old.len());
        }
        while state.current_bytes + new_size > self.max_bytes {
            match state.lru.pop_lru() {
                Some((_, evicted)) => {
                    state.current_bytes = state.current_bytes.saturating_sub(evicted.len());
                }
                None => break,
            }
        }
        state.current_bytes += new_size;
        state.lru.put(hash, bytes);
    }

    /// Total bytes currently held across all entries.
    #[must_use]
    pub fn current_bytes(&self) -> usize {
        self.lock_state().current_bytes
    }

    /// Number of entries currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock_state().lru.len()
    }

    /// True iff no entries are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for BlobCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        let (len, current) = match &state {
            Ok(s) => (s.lru.len(), s.current_bytes),
            Err(p) => (p.get_ref().lru.len(), p.get_ref().current_bytes),
        };
        f.debug_struct("BlobCache")
            .field("max_bytes", &self.max_bytes)
            .field("current_bytes", &current)
            .field("len", &len)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Disk-backed LRU
// ---------------------------------------------------------------------------

/// Bounded-by-bytes LRU cache that stores blob bytes on the local
/// filesystem. The gateway uses this as a second tier behind
/// [`BlobCache`]: blobs that don't fit in the memory budget land here,
/// and on serve we `mmap` the file and hand the pointer to ntex via
/// `Bytes::from_owner` so the kernel page cache → socket path is
/// zero-copy from the userspace side.
///
/// The cache is empty on startup (a warm-start that walked the
/// directory could be added later — for v1 we let the first hit refill
/// the in-memory bookkeeping). Inserts are atomic: bytes go to a
/// `<path>.tmp` file that is renamed into place, so concurrent gets
/// never see partial files.
///
/// Layout: `<root>/<hash[0..2]>/<hash[2..]>` — the same 2-char shard
/// prefix used by `LocalDiskBlobStore`, which keeps directory sizes
/// manageable on millions of entries.
pub struct DiskBlobCache {
    root: PathBuf,
    state: Mutex<DiskState>,
    max_bytes: u64,
    /// Per-process singleflight registry keyed by blob hash. A cold refill
    /// registers a sender here; concurrent refills of the SAME hash clone the
    /// receiver and await it instead of launching their own download. When
    /// the leader finishes it drops the sender, waking every follower, which
    /// then re-checks the disk cache. Duplicate downloads remain safe
    /// (temp-uniqueness + no-clobber publish + verify); this just avoids them
    /// on the common concurrent-miss path.
    inflight: InflightMap,
}

/// Shared singleflight registry. Held both by the cache and (cloned) by each
/// `RefillLeader` so the leader can deregister itself on drop without owning
/// the whole cache.
type InflightMap =
    std::sync::Arc<Mutex<std::collections::HashMap<String, flume::Receiver<()>>>>;

/// Guard returned to a refill LEADER. Holds the sender open for the duration
/// of the refill; dropping it (on success, error, or panic) wakes every
/// follower and removes the inflight entry.
pub struct RefillLeader {
    inflight: InflightMap,
    hash: String,
    _sender: flume::Sender<()>,
}

impl Drop for RefillLeader {
    fn drop(&mut self) {
        let mut inflight = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inflight.remove(&self.hash);
        // `_sender` drops after this, closing the channel and waking
        // followers blocked on `recv_async`.
    }
}

/// Outcome of `begin_refill`: either this caller is the LEADER (and must do
/// the download), or a FOLLOWER that should await the leader then re-check.
pub enum RefillRole {
    /// This caller owns the refill; drop the guard when done.
    Leader(RefillLeader),
    /// Another caller is already refilling; await this then re-check disk.
    Follower(flume::Receiver<()>),
}

/// A reserved, already-open temp file under the cache root, handed to the
/// blob store's streaming refill (`BlobStore::get_blob_to_file`). The store
/// streams + byte-verifies into `file`; the gateway then publishes it under
/// the content-addressed final path.
///
/// Unlinks its temp file on drop UNLESS it was published (`publish_temp`
/// takes ownership and clears `path`). This is what guarantees a dropped /
/// failed refill never leaves an orphan in the cache root.
pub struct DiskBlobTemp {
    /// Unique temp path under the cache root. `None` once published/disarmed.
    path: Option<PathBuf>,
    /// The open compio file the store writes into.
    file: compio::fs::File,
}

impl DiskBlobTemp {
    /// Borrow the open file to hand to `BlobStore::get_blob_to_file`. The
    /// store + S3 client never receive a raw path.
    #[must_use]
    pub const fn file(&self) -> &compio::fs::File {
        &self.file
    }

    /// The temp path, for the no-clobber publish primitive.
    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("temp path consumed before use")
    }
}

impl Drop for DiskBlobTemp {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            // Best-effort unlink of an unpublished temp. A failure here only
            // leaves a stray temp file (no correctness impact — temp names
            // are unique and never serve as a final path).
            if let Err(e) = std::fs::remove_file(&p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = ?p, error = %e, "gateway: temp blob unlink failed");
                }
            }
        }
    }
}

struct DiskState {
    /// hash → on-disk file size in bytes. LRU order is the access
    /// order; `get`/`promote` move entries to most-recently-used,
    /// `pop_lru` evicts the oldest.
    lru: LruCache<String, u64>,
    current_bytes: u64,
}

impl DiskBlobCache {
    /// Build a disk cache rooted at `root` with the given byte budget.
    /// Creates the root if missing. The cache starts empty in-memory;
    /// any pre-existing files in `root` are ignored until a future
    /// warm-start pass walks them.
    pub fn new(root: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            state: Mutex::new(DiskState {
                lru: LruCache::unbounded(),
                current_bytes: 0,
            }),
            max_bytes,
            inflight: std::sync::Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    fn lock_state(&self) -> MutexGuard<'_, DiskState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            tracing::error!("DiskBlobCache mutex poisoned; recovering cache state");
            poisoned.into_inner()
        })
    }

    /// On-disk path for a hash, regardless of whether the blob is
    /// currently cached. Used internally for read/write; callers
    /// should prefer [`Self::local_path`] which only returns paths
    /// for entries we know are present (and promotes the entry on
    /// access).
    fn path_for(&self, hash: &str) -> PathBuf {
        // Real hashes are 64 hex chars. For anything shorter than 3
        // chars (or when splitting would produce an empty `rest`) we
        // fall back to a flat layout — it keeps the function total
        // for malformed keys (caller validates upstream) and for
        // synthetic test keys.
        if hash.len() < 3 {
            return self.root.join(hash);
        }
        let (prefix, rest) = hash.split_at(2);
        self.root.join(prefix).join(rest)
    }

    /// If the blob is cached locally, returns the on-disk path AND
    /// promotes the entry to most-recently-used. The caller is
    /// responsible for opening / mmap'ing the path. Returns `None` if
    /// the entry isn't present in the LRU (a cold path triggers a
    /// backend fetch + insert).
    pub fn local_path(&self, hash: &str) -> Option<PathBuf> {
        let mut state = self.lock_state();
        // `LruCache::get` promotes on hit.
        state.lru.get(hash)?;
        Some(self.path_for(hash))
    }

    /// Insert bytes for `hash`. Atomic: we write to `<path>.tmp` and
    /// rename — a concurrent `local_path` call either sees the old
    /// state (returns None or a fully-written previous file) or the
    /// new one, never partial. Evicts oldest entries (deleting their
    /// on-disk files) until the new entry fits inside `max_bytes`.
    /// If the entry alone exceeds the budget, the insert is a no-op
    /// — one fat blob must not flush everything else.
    pub fn insert(&self, hash: &str, bytes: &[u8]) -> std::io::Result<()> {
        let new_size = bytes.len() as u64;
        if new_size > self.max_bytes {
            return Ok(());
        }
        let path = self.path_for(hash);

        // Materialise the file outside the lock — fs I/O is the slow
        // part and we don't want to serialise concurrent inserts of
        // distinct hashes.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Use a unique tmp suffix per write so concurrent inserts of
        // the same hash don't trip over each other's rename. The
        // suffix combines the thread id with a process-wide counter
        // — uniqueness inside the process, plus the PID makes
        // multi-process scenarios (no current caller, but cheap)
        // safe too.
        let tmp = path.with_extension(unique_tmp_suffix());
        // O_CREAT|O_TRUNC|O_WRONLY semantics — std::fs::write does
        // exactly that. Truncating ensures a previous failed insert
        // (lingering .tmp) doesn't corrupt the new write.
        std::fs::write(&tmp, bytes)?;
        // rename(2) is atomic on POSIX for same-filesystem moves —
        // any concurrent reader either opens the previous file (or
        // fails ENOENT) or the new one, never a partial.
        std::fs::rename(&tmp, &path)?;

        // Now update the bookkeeping. The lock is taken last so the
        // common case (different hashes) doesn't contend.
        let mut state = self.lock_state();
        if let Some(old_size) = state.lru.pop(hash) {
            state.current_bytes = state.current_bytes.saturating_sub(old_size);
        }
        while state.current_bytes + new_size > self.max_bytes {
            match state.lru.pop_lru() {
                Some((evicted_hash, evicted_size)) => {
                    state.current_bytes = state.current_bytes.saturating_sub(evicted_size);
                    let p = self.path_for(&evicted_hash);
                    // Best-effort: a failed unlink doesn't block the
                    // insert (the file is already orphaned from the
                    // LRU) — log and move on.
                    if let Err(e) = std::fs::remove_file(&p) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(
                                path = ?p,
                                error = %e,
                                "gateway: disk cache evict — failed to remove"
                            );
                        }
                    }
                }
                None => break,
            }
        }
        state.current_bytes += new_size;
        state.lru.put(hash.to_string(), new_size);
        Ok(())
    }

    /// Reserve a unique, already-open temp file under the cache root for a
    /// streaming refill. The caller hands `temp.file()` to
    /// `BlobStore::get_blob_to_file`, which streams + byte-verifies the blob
    /// into it WITHOUT buffering the whole object. On success the caller
    /// publishes via [`Self::publish_temp`]; on any error it drops the
    /// `DiskBlobTemp`, which unlinks the temp.
    ///
    /// The temp lives directly under the hash shard directory so the eventual
    /// hard-link publish is same-directory (never cross-device).
    pub async fn reserve_temp(&self, hash: &str) -> std::io::Result<DiskBlobTemp> {
        let final_path = self.path_for(hash);
        if let Some(parent) = final_path.parent() {
            compio::fs::create_dir_all(parent).await?;
        }
        let tmp = final_path.with_extension(unique_tmp_suffix());
        let file = compio::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&tmp)
            .await?;
        Ok(DiskBlobTemp {
            path: Some(tmp),
            file,
        })
    }

    /// Publish a verified temp file under the content-addressed final path,
    /// returning the path the static response should serve.
    ///
    /// The temp's bytes were already size/hash-verified by the store, so this
    /// only has to commit them with a no-clobber primitive and keep the LRU
    /// byte accounting correct:
    ///
    /// 1. `sync_all` + close the temp.
    /// 2. If the hash is already a live LRU entry whose final file exists,
    ///    promote it and discard the temp (a concurrent refill won).
    /// 3. Otherwise hard-link temp → final (same directory, so no
    ///    cross-device), then unlink temp. If hard-link is unsupported, copy
    ///    into a freshly `create_new`'d final file. NEVER overwrite-rename.
    /// 4. If the final path already exists, size/hash-verify it: valid ⇒
    ///    trust it, discard temp; invalid ⇒ unlink the corrupt final and
    ///    retry the no-clobber publish.
    /// 5. Update byte accounting exactly once for the file that wins,
    ///    subtracting any replaced corrupt entry first.
    pub async fn publish_temp(
        &self,
        hash: &str,
        temp: DiskBlobTemp,
        verified_size: u64,
    ) -> std::io::Result<PathBuf> {
        // 1. Durably flush + close the temp before we link it into place.
        temp.file.sync_all().await?;
        // Take ownership of the temp path so its Drop does NOT unlink the
        // file out from under the publish; we manage it explicitly here.
        let mut temp = temp;
        let temp_path = temp.path.take().expect("temp path present at publish");
        // Closing the file: drop the compio handle now that it's synced.
        drop(temp);

        let final_path = self.path_for(hash);

        // 2. Fast path: another refill already published this hash.
        {
            let mut state = self.lock_state();
            if state.lru.get(hash).is_some() && final_path.exists() {
                drop(state);
                let _ = std::fs::remove_file(&temp_path);
                return Ok(final_path);
            }
        }

        // 3/4. No-clobber publish with final-file verification.
        let mut size_delta_old: Option<u64> = None;
        loop {
            match std::fs::hard_link(&temp_path, &final_path) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&temp_path);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Final already exists — verify it.
                    match verify_file(&final_path, hash) {
                        Ok(()) => {
                            // Valid existing file wins; discard our temp.
                            let _ = std::fs::remove_file(&temp_path);
                            break;
                        }
                        Err(_) => {
                            // Corrupt final — drop its bytes from accounting,
                            // unlink it, and retry the no-clobber publish.
                            let mut state = self.lock_state();
                            if let Some(old) = state.lru.pop(hash) {
                                state.current_bytes = state.current_bytes.saturating_sub(old);
                                size_delta_old = Some(old);
                            }
                            drop(state);
                            let _ = std::fs::remove_file(&final_path);
                            continue;
                        }
                    }
                }
                Err(e) if is_hardlink_unsupported(&e) => {
                    // Hard-link unsupported on this FS — copy into a freshly
                    // created final file (no-clobber via create_new).
                    match copy_no_clobber(&temp_path, &final_path) {
                        Ok(()) => {
                            let _ = std::fs::remove_file(&temp_path);
                            break;
                        }
                        Err(ce) if ce.kind() == std::io::ErrorKind::AlreadyExists => {
                            match verify_file(&final_path, hash) {
                                Ok(()) => {
                                    let _ = std::fs::remove_file(&temp_path);
                                    break;
                                }
                                Err(_) => {
                                    let _ = std::fs::remove_file(&final_path);
                                    continue;
                                }
                            }
                        }
                        Err(ce) => {
                            let _ = std::fs::remove_file(&temp_path);
                            return Err(ce);
                        }
                    }
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&temp_path);
                    return Err(e);
                }
            }
        }

        // 5. Byte accounting — exactly once for the winning final file.
        // Evict to fit, subtracting any corrupt entry we already removed.
        self.account_published(hash, verified_size, size_delta_old);
        Ok(final_path)
    }

    /// Claim singleflight leadership for refilling `hash`, or return a
    /// receiver to await an in-progress refill. The first caller becomes the
    /// `Leader` and must perform the download; concurrent callers become
    /// `Follower`s that await the leader and then re-check the disk cache.
    pub fn begin_refill(&self, hash: &str) -> RefillRole {
        let mut inflight = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(rx) = inflight.get(hash) {
            return RefillRole::Follower(rx.clone());
        }
        // Bounded(0): we never send; the channel is a drop-to-wake signal.
        let (tx, rx) = flume::bounded(0);
        inflight.insert(hash.to_string(), rx);
        RefillRole::Leader(RefillLeader {
            inflight: std::sync::Arc::clone(&self.inflight),
            hash: hash.to_string(),
            _sender: tx,
        })
    }

    /// Insert/promote LRU bookkeeping for a freshly published final file,
    /// evicting oldest entries to stay within budget. `already_subtracted`
    /// is `Some(old_size)` if a corrupt prior entry's bytes were already
    /// removed during the publish retry (so we don't double-subtract).
    fn account_published(&self, hash: &str, size: u64, already_subtracted: Option<u64>) {
        let mut state = self.lock_state();
        // If the entry is still tracked (and wasn't the corrupt one we
        // already popped), subtract its old size before re-adding.
        if already_subtracted.is_none() {
            if let Some(old) = state.lru.pop(hash) {
                state.current_bytes = state.current_bytes.saturating_sub(old);
            }
        }
        while state.current_bytes + size > self.max_bytes {
            match state.lru.pop_lru() {
                Some((evicted_hash, evicted_size)) => {
                    if evicted_hash == hash {
                        // Don't evict the entry we're publishing.
                        state.lru.put(evicted_hash, evicted_size);
                        break;
                    }
                    state.current_bytes = state.current_bytes.saturating_sub(evicted_size);
                    let p = self.path_for(&evicted_hash);
                    if let Err(e) = std::fs::remove_file(&p) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(path = ?p, error = %e, "gateway: disk cache evict — failed to remove");
                        }
                    }
                }
                None => break,
            }
        }
        state.current_bytes += size;
        state.lru.put(hash.to_string(), size);
    }

    /// Total bytes currently tracked across all entries.
    #[must_use]
    pub fn current_bytes(&self) -> u64 {
        self.lock_state().current_bytes
    }

    /// Number of entries currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock_state().lru.len()
    }

    /// True iff no entries are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for DiskBlobCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        let (len, current) = match &state {
            Ok(s) => (s.lru.len(), s.current_bytes),
            Err(p) => (p.get_ref().lru.len(), p.get_ref().current_bytes),
        };
        f.debug_struct("DiskBlobCache")
            .field("root", &self.root)
            .field("max_bytes", &self.max_bytes)
            .field("current_bytes", &current)
            .field("len", &len)
            .finish()
    }
}

/// Generate a unique extension for `<path>.tmp` writes so concurrent
/// inserts of the same hash don't collide on a shared tmp filename.
/// We need uniqueness only inside this process — `rename(2)` is the
/// commit point, and once a tmp file is renamed it's no longer
/// referenced by its tmp name.
fn unique_tmp_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("tmp.{pid}.{n}")
}

/// Size/hash-verify an existing final file before trusting it during a
/// publish race. Returns an error on any divergence so a corrupt or
/// truncated final file is never promoted.
fn verify_file(path: &Path, hash: &str) -> std::io::Result<()> {
    let data = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let actual = hex::encode(hasher.finalize());
    if actual == hash {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("blob hash mismatch: expected {hash}, got {actual}"),
        ))
    }
}

/// Copy `src` into a freshly created `dst` (no-clobber: fails with
/// `AlreadyExists` if `dst` exists). Used only when hard-link is unsupported.
fn copy_no_clobber(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let bytes = std::fs::read(src)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Whether a `hard_link` error means the filesystem doesn't support links
/// (EPERM/ENOSYS/Unsupported), in which case we fall back to a copy.
fn is_hardlink_unsupported(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::Unsupported {
        return true;
    }
    // EPERM (1) and ENOSYS (38 on Linux) are the portable "links not allowed
    // here" signals. Matching by raw code keeps this libc-free.
    matches!(e.raw_os_error(), Some(1) | Some(38))
}

// ---------------------------------------------------------------------------
// mmap helper for the disk-cache serve path
// ---------------------------------------------------------------------------

/// Open `path` and return a `Bytes` view backed by an mmap of the
/// file. The `Bytes` owns the mmap, so the mapping lives until every
/// outstanding `Bytes` clone is dropped — this is what makes the path
/// safe even if the on-disk file is unlinked under us afterwards (the
/// mmap pins the inode).
///
/// SAFETY: blobs are content-addressed and immutable once written. The
/// disk cache writes via `<path>.tmp` + rename, so a successful
/// `File::open` either sees a complete file or fails. Concurrent
/// readers get distinct mappings of the same inode. The unsafety is
/// localised to this function.
#[allow(unsafe_code)]
pub fn mmap_to_bytes(path: &Path) -> std::io::Result<Bytes> {
    let file = std::fs::File::open(path)?;
    // SAFETY: see function-level docs. The file is read-only,
    // content-addressed, and mmap2's `Mmap` holds the descriptor for
    // the duration of the mapping.
    let mmap = unsafe { memmap2::Mmap::map(&file)? };
    Ok(Bytes::from_owner(mmap))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn b(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    #[test]
    fn miss_returns_none() {
        let cache = BlobCache::new(1024);
        assert!(cache.get("deadbeef").is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_bytes(), 0);
    }

    #[test]
    fn hit_returns_inserted_bytes() {
        let cache = BlobCache::new(1024);
        let payload = Bytes::from_static(b"hello");
        cache.insert("a".into(), payload.clone());
        assert_eq!(cache.get("a"), Some(payload));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.current_bytes(), 5);
    }

    #[test]
    fn evicts_oldest_when_over_budget() {
        let cache = BlobCache::new(10);
        cache.insert("a".into(), b(4));
        cache.insert("b".into(), b(4));
        // Both fit (8 bytes used, 10 budget).
        assert_eq!(cache.len(), 2);
        // Third 4-byte entry pushes us to 12; oldest ("a") is evicted.
        cache.insert("c".into(), b(4));
        assert!(cache.get("a").is_none(), "oldest entry should be evicted");
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.current_bytes(), 8);
    }

    #[test]
    fn get_promotes_entry_to_most_recent() {
        let cache = BlobCache::new(10);
        cache.insert("a".into(), b(4));
        cache.insert("b".into(), b(4));
        // Touch "a" — now "b" is the LRU.
        assert!(cache.get("a").is_some());
        // Insert "c" — should evict "b", not "a".
        cache.insert("c".into(), b(4));
        assert!(cache.get("a").is_some(), "promoted entry must survive");
        assert!(cache.get("b").is_none(), "untouched entry should be evicted");
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn rejects_entries_larger_than_budget() {
        let cache = BlobCache::new(4);
        cache.insert("big".into(), b(5));
        assert!(cache.get("big").is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_bytes(), 0);
    }

    #[test]
    fn replacing_existing_key_accounts_for_delta() {
        let cache = BlobCache::new(10);
        cache.insert("a".into(), b(3));
        assert_eq!(cache.current_bytes(), 3);
        // Replace with a larger payload — accounting should track the delta,
        // not double-count.
        cache.insert("a".into(), b(7));
        assert_eq!(cache.current_bytes(), 7);
        assert_eq!(cache.len(), 1);
        // And shrink — also tracked.
        cache.insert("a".into(), b(2));
        assert_eq!(cache.current_bytes(), 2);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn blob_cache_recovers_after_state_lock_poison() {
        let cache = BlobCache::new(10);
        let poisoned = std::panic::catch_unwind(|| {
            let _guard = cache.state.lock().unwrap();
            panic!("poison blob cache");
        });
        assert!(poisoned.is_err());

        cache.insert("a".into(), b(4));
        assert!(cache.get("a").is_some());
        assert_eq!(cache.current_bytes(), 4);
        assert_eq!(cache.len(), 1);
    }

    // -----------------------------------------------------------------------
    // DiskBlobCache tests
    // -----------------------------------------------------------------------

    fn tmp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "zsdisk-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        p
    }

    /// 64-char lowercase sha256-ish stand-in for tests. The cache is
    /// keyed by string, so any stable 64-char hex works.
    fn h(prefix: u8) -> String {
        let mut s = format!("{prefix:02x}");
        s.push_str(&"0".repeat(62));
        s
    }

    #[test]
    fn disk_round_trip() {
        let root = tmp_root("round-trip");
        let cache = DiskBlobCache::new(root.clone(), 1024).expect("new");
        let hash = h(0xab);
        let payload = b"hello disk";

        cache.insert(&hash, payload).expect("insert");
        let path = cache.local_path(&hash).expect("present");
        // Sharded layout: <root>/ab/<rest>
        assert!(path.starts_with(root.join("ab")), "sharded layout");
        let bytes = std::fs::read(&path).expect("read back");
        assert_eq!(bytes, payload, "round-trip bytes");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.current_bytes(), payload.len() as u64);

        // mmap helper produces the same bytes.
        let via_mmap = mmap_to_bytes(&path).expect("mmap");
        assert_eq!(&via_mmap[..], payload);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn disk_evicts_oldest_when_over_budget() {
        let root = tmp_root("evict");
        // Budget for 8 bytes total.
        let cache = DiskBlobCache::new(root.clone(), 8).expect("new");
        let a = h(0xa1);
        let b1 = h(0xb1);
        let c = h(0xc1);

        cache.insert(&a, &[0u8; 4]).expect("a");
        cache.insert(&b1, &[0u8; 4]).expect("b");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.current_bytes(), 8);

        // Third insert pushes us to 12B; oldest (a) gets evicted.
        cache.insert(&c, &[0u8; 4]).expect("c");
        assert!(cache.local_path(&a).is_none(), "oldest evicted from LRU");
        assert!(
            !cache.path_for(&a).exists(),
            "evicted file removed from disk"
        );
        assert!(cache.local_path(&b1).is_some());
        assert!(cache.local_path(&c).is_some());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.current_bytes(), 8);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn disk_atomic_write() {
        // A concurrent insert of the same hash must leave the final
        // file matching one of the inserts in full — never partial.
        // We simulate concurrency from multiple threads.
        let root = tmp_root("atomic");
        let cache = std::sync::Arc::new(
            DiskBlobCache::new(root.clone(), 1024 * 1024).expect("new"),
        );
        let hash = h(0xde);
        let payload_a = vec![0xAAu8; 4096];
        let payload_b = vec![0xBBu8; 4096];

        let mut handles = Vec::new();
        for _ in 0..4 {
            let c = cache.clone();
            let h_ = hash.clone();
            let pa = payload_a.clone();
            handles.push(std::thread::spawn(move || {
                c.insert(&h_, &pa).expect("insert a");
            }));
            let c = cache.clone();
            let h_ = hash.clone();
            let pb = payload_b.clone();
            handles.push(std::thread::spawn(move || {
                c.insert(&h_, &pb).expect("insert b");
            }));
        }
        for j in handles {
            j.join().expect("thread");
        }

        let path = cache.local_path(&hash).expect("present");
        let final_bytes = std::fs::read(&path).expect("read");
        // The final file's bytes match exactly one of the two inserts
        // — never a half-and-half mix.
        assert!(
            final_bytes == payload_a || final_bytes == payload_b,
            "final file bytes must match one full payload"
        );
        assert_eq!(final_bytes.len(), 4096);
        assert_eq!(cache.len(), 1, "single LRU entry per hash");
        assert_eq!(cache.current_bytes(), 4096);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn disk_promotes_on_get() {
        // Touching A then inserting C with a 2-entry budget should
        // evict B (the LRU), not A. Mirrors the in-memory cache test.
        let root = tmp_root("promote");
        let cache = DiskBlobCache::new(root.clone(), 8).expect("new");
        let a = h(0xa2);
        let b1 = h(0xb2);
        let c = h(0xc2);

        cache.insert(&a, &[0u8; 4]).expect("a");
        cache.insert(&b1, &[0u8; 4]).expect("b");
        // Touch A — promotes it.
        assert!(cache.local_path(&a).is_some());
        // Insert C — should evict B (oldest after the promotion).
        cache.insert(&c, &[0u8; 4]).expect("c");
        assert!(cache.local_path(&a).is_some(), "promoted entry survives");
        assert!(cache.local_path(&b1).is_none(), "untouched entry evicted");
        assert!(cache.local_path(&c).is_some());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn disk_path_layout() {
        let root = tmp_root("layout");
        let cache = DiskBlobCache::new(root.clone(), 1024).expect("new");
        let hash = format!(
            "ab12{}",
            "c".repeat(60)
        );
        cache.insert(&hash, b"x").expect("insert");
        let path = cache.local_path(&hash).expect("present");
        // <root>/ab/12cccc... — first two hex chars are the shard prefix,
        // the rest is the file name.
        assert_eq!(
            path,
            root.join("ab").join(format!("12{}", "c".repeat(60))),
            "sharded path layout"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn disk_oversized_entry_is_no_op() {
        let root = tmp_root("oversized");
        let cache = DiskBlobCache::new(root.clone(), 4).expect("new");
        let hash = h(0x01);
        cache.insert(&hash, &[0u8; 5]).expect("oversized");
        assert!(cache.local_path(&hash).is_none(), "no entry");
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_bytes(), 0);
        std::fs::remove_dir_all(&root).ok();
    }

    // -----------------------------------------------------------------------
    // Streaming refill: reserve_temp / publish_temp / begin_refill
    // -----------------------------------------------------------------------

    /// Real sha256 hex of `data` — needed for the publish verify-on-conflict
    /// path (synthetic `h()` hashes wouldn't verify).
    fn real_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    /// Stream `bytes` into a reserved temp via direct writes (stands in for
    /// `BlobStore::get_blob_to_file`), then return the temp ready to publish.
    async fn fill_temp(cache: &DiskBlobCache, hash: &str, bytes: &[u8]) -> DiskBlobTemp {
        use compio::io::AsyncWriteAtExt;
        let temp = cache.reserve_temp(hash).await.expect("reserve");
        let mut fref: &compio::fs::File = temp.file();
        let compio::BufResult(res, _) = fref.write_all_at(bytes.to_vec(), 0).await;
        res.expect("write temp");
        temp
    }

    #[compio::test]
    async fn reserve_publish_round_trip() {
        let root = tmp_root("reserve-publish");
        let cache = DiskBlobCache::new(root.clone(), 1024 * 1024).expect("new");
        let payload = b"streamed blob bytes";
        let hash = real_hash(payload);

        let temp = fill_temp(&cache, &hash, payload).await;
        let final_path = cache
            .publish_temp(&hash, temp, payload.len() as u64)
            .await
            .expect("publish");
        assert!(final_path.exists(), "final published");
        assert_eq!(std::fs::read(&final_path).unwrap(), payload, "bytes match");
        // Published entry is in the LRU and serves via local_path.
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.current_bytes(), payload.len() as u64);
        assert_eq!(cache.local_path(&hash), Some(final_path));

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn dropped_temp_unlinks_and_does_not_publish() {
        let root = tmp_root("drop-temp");
        let cache = DiskBlobCache::new(root.clone(), 1024 * 1024).expect("new");
        let hash = real_hash(b"abandoned");
        let temp = fill_temp(&cache, &hash, b"abandoned").await;
        let temp_path = temp.path().to_path_buf();
        assert!(temp_path.exists(), "temp present before drop");
        drop(temp);
        assert!(!temp_path.exists(), "temp unlinked on drop");
        assert_eq!(cache.len(), 0, "nothing published");

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn publish_no_clobber_trusts_valid_existing_final() {
        // Simulate a duplicate download: a valid final file already exists
        // (no LRU entry — e.g. another process wrote it). publish_temp must
        // verify + trust it, discard our temp, and NOT corrupt the file.
        let root = tmp_root("no-clobber-valid");
        let cache = DiskBlobCache::new(root.clone(), 1024 * 1024).expect("new");
        let payload = b"shared content-addressed bytes";
        let hash = real_hash(payload);

        // Pre-create the final file out-of-band (valid bytes), no LRU entry.
        let final_path = cache.path_for(&hash);
        std::fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        std::fs::write(&final_path, payload).unwrap();

        let temp = fill_temp(&cache, &hash, payload).await;
        let temp_path = temp.path().to_path_buf();
        let published = cache
            .publish_temp(&hash, temp, payload.len() as u64)
            .await
            .expect("publish over valid existing");
        assert_eq!(published, final_path);
        assert!(!temp_path.exists(), "our temp discarded");
        assert_eq!(std::fs::read(&final_path).unwrap(), payload, "final intact");
        // Byte accounting added once for the winning file.
        assert_eq!(cache.current_bytes(), payload.len() as u64);
        assert_eq!(cache.len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn publish_replaces_corrupt_existing_final() {
        // A corrupt final file (wrong bytes for the hash) must be unlinked
        // and replaced by the verified temp.
        let root = tmp_root("no-clobber-corrupt");
        let cache = DiskBlobCache::new(root.clone(), 1024 * 1024).expect("new");
        let payload = b"the real bytes for this hash";
        let hash = real_hash(payload);

        let final_path = cache.path_for(&hash);
        std::fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        std::fs::write(&final_path, b"CORRUPT DIFFERENT BYTES").unwrap();

        let temp = fill_temp(&cache, &hash, payload).await;
        let published = cache
            .publish_temp(&hash, temp, payload.len() as u64)
            .await
            .expect("publish over corrupt existing");
        assert_eq!(published, final_path);
        assert_eq!(
            std::fs::read(&final_path).unwrap(),
            payload,
            "corrupt final replaced with verified bytes"
        );
        assert_eq!(cache.current_bytes(), payload.len() as u64, "accounting correct");

        std::fs::remove_dir_all(&root).ok();
    }

    #[compio::test]
    async fn begin_refill_leader_then_follower() {
        let root = tmp_root("singleflight");
        let cache = DiskBlobCache::new(root.clone(), 1024 * 1024).expect("new");
        let hash = h(0x7f);

        // First caller leads.
        let role1 = cache.begin_refill(&hash);
        assert!(matches!(role1, RefillRole::Leader(_)), "first caller leads");
        // Concurrent caller for the SAME hash follows.
        let role2 = cache.begin_refill(&hash);
        let rx = match role2 {
            RefillRole::Follower(rx) => rx,
            RefillRole::Leader(_) => panic!("second concurrent caller must follow"),
        };
        // A DIFFERENT hash leads independently.
        let other = h(0x80);
        assert!(matches!(cache.begin_refill(&other), RefillRole::Leader(_)));

        // Dropping the leader wakes the follower (recv resolves with Err).
        drop(role1);
        let woke = rx.recv_async().await;
        assert!(woke.is_err(), "follower woken by leader drop");
        // After the leader finished, a fresh refill leads again.
        assert!(matches!(cache.begin_refill(&hash), RefillRole::Leader(_)));

        std::fs::remove_dir_all(&root).ok();
    }

    /// Two independent caches over the SAME root (stand-in for two gateway
    /// processes): both reserve distinct temps and publish; the no-clobber
    /// primitive ensures exactly one final file with the correct bytes, and
    /// no partial reads.
    #[compio::test]
    async fn two_process_same_root_no_clobber() {
        let root = tmp_root("two-process");
        let payload = vec![0x5Au8; 4096];
        let hash = real_hash(&payload);

        let cache_a = DiskBlobCache::new(root.clone(), 1024 * 1024).expect("a");
        let cache_b = DiskBlobCache::new(root.clone(), 1024 * 1024).expect("b");

        let temp_a = fill_temp(&cache_a, &hash, &payload).await;
        let temp_b = fill_temp(&cache_b, &hash, &payload).await;

        // A publishes first (wins via hard-link).
        let path_a = cache_a
            .publish_temp(&hash, temp_a, payload.len() as u64)
            .await
            .expect("a publish");
        // B publishes second; the final already exists with valid bytes, so
        // B verifies + trusts it (no clobber, no partial).
        let path_b = cache_b
            .publish_temp(&hash, temp_b, payload.len() as u64)
            .await
            .expect("b publish");
        assert_eq!(path_a, path_b, "same content-addressed final path");
        assert_eq!(std::fs::read(&path_a).unwrap(), payload, "final bytes correct");

        std::fs::remove_dir_all(&root).ok();
    }
}
