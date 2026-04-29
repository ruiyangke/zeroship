//! Edge LRU caches for blob bytes, keyed by content hash.
//!
//! - `BlobCache` is the in-memory tier (Phase A of the zero-copy plan;
//!   see `docs/architecture/blob-store.md`). Bytes are still copied
//!   once on socket write — `Bytes` is `Arc`-refcounted so concurrent
//!   requests for the same hash share the buffer.
//! - `DiskBlobCache` is the on-disk tier (Phase B). Larger blobs that
//!   don't fit in the memory budget land on disk; on serve, the gateway
//!   `mmap`s the file and hands the pointer to ntex via
//!   `Bytes::from_owner`, so the kernel page cache → socket path is
//!   zero-copy from the userspace side.
//! - Phase C will swap the page-cache → socket copy for `sendfile(2)`
//!   or `IORING_OP_SPLICE`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bytes::Bytes;
use lru::LruCache;

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

    /// Look up a blob, promoting the entry to most-recently-used on hit.
    pub fn get(&self, hash: &str) -> Option<Bytes> {
        let mut state = self.state.lock().expect("BlobCache mutex poisoned");
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
        let mut state = self.state.lock().expect("BlobCache mutex poisoned");
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
        self.state.lock().expect("BlobCache mutex poisoned").current_bytes
    }

    /// Number of entries currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state.lock().expect("BlobCache mutex poisoned").lru.len()
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
// Disk-backed LRU (Phase B)
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
        let mut state = self.state.lock().expect("DiskBlobCache mutex poisoned");
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
        let mut state = self.state.lock().expect("DiskBlobCache mutex poisoned");
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
                            eprintln!(
                                "[gate] disk cache evict: failed to remove {p:?}: {e}"
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

    /// Total bytes currently tracked across all entries.
    #[must_use]
    pub fn current_bytes(&self) -> u64 {
        self.state
            .lock()
            .expect("DiskBlobCache mutex poisoned")
            .current_bytes
    }

    /// Number of entries currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .expect("DiskBlobCache mutex poisoned")
            .lru
            .len()
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

// ---------------------------------------------------------------------------
// mmap helper — Phase B zero-copy
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
}
