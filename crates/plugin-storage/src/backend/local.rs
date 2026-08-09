//! Local filesystem `Backend` — the dev default, always available.
//!
//! Storage layout, per app-bucket:
//!
//! ```text
//! <root>/<app_id>/<bucket>/o/<key>   object bytes
//! <root>/<app_id>/<bucket>/m/<key>   metadata sidecar (content type)
//! ```
//!
//! The key may contain `/` (nested paths); each segment is validated by
//! `super::validate_object_coords` before any filesystem op.
//!
//! Objects and their metadata live in two disjoint subtrees on purpose.
//! Sidecars kept as *siblings* of the objects they describe (`.<name>.meta`)
//! would collide with a creator key spelled exactly that way — a creator who
//! stored `.avatar.png.meta` would have it silently destroyed by a later
//! `put` of `avatar.png`, and `list` would have to hide sidecars by name
//! filter. Rooting objects under `o/` makes every creator key land inside
//! `o/`, where no metadata file can ever be addressed, and makes `list` walk
//! a subtree that structurally contains no metadata rather than one it has to
//! filter.
//!
//! Uses compio's `AsyncWriteAt` / `AsyncReadAt` for positional I/O on
//! io_uring. Zero tokio.
//!
//! Streaming: `put_stream` writes chunks to a temp file and `rename`s it into
//! place (atomic on POSIX — a reader never observes a half-written object).
//! `get_stream` reads the file back in bounded chunks so a large object never
//! lands fully in RAM.
//!
//! ## Metadata can never contradict the bytes
//!
//! Two files cannot be renamed into place in one atomic step, so there is
//! necessarily an instant where the object has been replaced and its sidecar
//! has not (and, after a crash or a race between two writers of the same key,
//! that state can persist). Rather than try to close that window, every
//! sidecar carries a **fingerprint** of the exact object instance it
//! describes — `(dev, ino, mtime, size)`, captured from the temp file *before*
//! the rename, all of which `rename` preserves. A reader fingerprints the
//! object it actually opened and ignores any sidecar that does not match.
//!
//! So a mismatched sidecar is inert, not wrong: the object degrades to
//! [`DEFAULT_CONTENT_TYPE`] — the same answer a `put` with no content type
//! gives — and never inherits some other writer's type. "Unknown" is a safe
//! answer; "confidently wrong" is not.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bytes::Bytes;
use compio::fs;
use compio::io::{AsyncReadAtExt, AsyncWriteAtExt};

use super::{
    validate_list_coords, validate_object_coords, Backend, BoxByteStream, BoxChunkSource,
    ChunkResult, ChunkSource, ListEntry, ObjectMeta, DEFAULT_CONTENT_TYPE,
};

/// Bytes read per `get_stream` chunk. 256 KiB balances syscall count
/// against per-chunk allocation; the consumer pulls these one at a time.
const READ_CHUNK: u64 = 256 * 1024;

/// Subtree holding object bytes, under `<root>/<app_id>/<bucket>/`.
const OBJECTS_SUBDIR: &str = "o";

/// Subtree holding metadata sidecars, mirroring the object key path.
const META_SUBDIR: &str = "m";

/// Sidecar schema version. A sidecar that does not carry exactly this
/// version is ignored (the object reads back as [`DEFAULT_CONTENT_TYPE`]),
/// so the payload shape can change without a reader ever misreading an old
/// one as a new one.
const SIDECAR_VERSION: u64 = 1;

#[derive(Debug, Clone)]
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Path to the object bytes for a key (inside the `o/` subtree).
    fn object_path(&self, app_id: &str, bucket: &str, key: &str) -> PathBuf {
        self.keyed_path(app_id, bucket, OBJECTS_SUBDIR, key)
    }

    /// Path to the metadata sidecar for a key (inside the `m/` subtree,
    /// mirroring the object's key path).
    fn meta_path(&self, app_id: &str, bucket: &str, key: &str) -> PathBuf {
        self.keyed_path(app_id, bucket, META_SUBDIR, key)
    }

    fn keyed_path(&self, app_id: &str, bucket: &str, subdir: &str, key: &str) -> PathBuf {
        // Safety: caller already ran validate_object_coords, so no path-
        // traversal segments can reach here. We still `push` segment-by-
        // segment (not via Path::new) so a '/' in a key component — which
        // *would* have been rejected — can never be interpreted as a
        // directory separator by the OS path parser.
        let mut path = self.root.join(app_id).join(bucket).join(subdir);
        for segment in key.split('/') {
            path.push(segment);
        }
        path
    }

    /// Root of the object subtree for an app-bucket — the base `list` walks.
    /// Metadata lives outside it, so no sidecar can ever be listed as an
    /// object.
    fn objects_dir(&self, app_id: &str, bucket: &str) -> PathBuf {
        self.root.join(app_id).join(bucket).join(OBJECTS_SUBDIR)
    }

    /// Read the content type recorded for the object instance identified by
    /// `fingerprint`, or [`DEFAULT_CONTENT_TYPE`] when there is no sidecar,
    /// it is unreadable/malformed, or it describes a *different* instance of
    /// this key (a crash between the two renames, or a racing writer).
    ///
    /// Never fails: an unreadable sidecar degrades the content type, it does
    /// not fail the read of an object whose bytes are perfectly fine.
    async fn read_content_type(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        fingerprint: ObjectFingerprint,
    ) -> String {
        let path = self.meta_path(app_id, bucket, key);
        let Ok(raw) = fs::read(&path).await else {
            return DEFAULT_CONTENT_TYPE.to_string();
        };
        decode_sidecar(&raw, fingerprint).unwrap_or_else(|| DEFAULT_CONTENT_TYPE.to_string())
    }
}

/// Identity of one specific *instance* of an object file: which filesystem
/// object it is, and which write produced it.
///
/// `rename` preserves all four fields, so a fingerprint taken from the temp
/// file before the rename describes the bytes that become live. Together they
/// make a stale sidecar detectable: `(dev, ino)` catches a different file,
/// and `(mtime, size)` catches the case where the inode number was recycled
/// by a later object at the same key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObjectFingerprint {
    dev: u64,
    ino: u64,
    mtime: i64,
    mtime_nsec: i64,
    size: u64,
}

impl ObjectFingerprint {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            size: meta.size(),
        }
    }
}

/// Serialise a sidecar: the content type plus the fingerprint of the object
/// instance it describes.
fn encode_sidecar(fingerprint: ObjectFingerprint, content_type: &str) -> Vec<u8> {
    serde_json::json!({
        "v": SIDECAR_VERSION,
        "dev": fingerprint.dev,
        "ino": fingerprint.ino,
        "mtime": fingerprint.mtime,
        "mtime_nsec": fingerprint.mtime_nsec,
        "size": fingerprint.size,
        "content_type": content_type,
    })
    .to_string()
    .into_bytes()
}

/// Parse a sidecar and return its content type ONLY if it describes exactly
/// `expected`. Returns `None` for anything else — wrong version, malformed
/// JSON, missing field, or a fingerprint from a different write. Callers
/// substitute [`DEFAULT_CONTENT_TYPE`].
fn decode_sidecar(raw: &[u8], expected: ObjectFingerprint) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(raw).ok()?;
    if value.get("v")?.as_u64()? != SIDECAR_VERSION {
        return None;
    }
    let found = ObjectFingerprint {
        dev: value.get("dev")?.as_u64()?,
        ino: value.get("ino")?.as_u64()?,
        mtime: value.get("mtime")?.as_i64()?,
        mtime_nsec: value.get("mtime_nsec")?.as_i64()?,
        size: value.get("size")?.as_u64()?,
    };
    if found != expected {
        return None;
    }
    Some(value.get("content_type")?.as_str()?.to_string())
}

/// Write a sidecar to `path` and `fsync` it, so the rename that publishes it
/// cannot expose a file whose contents have not reached disk.
async fn write_sidecar(
    path: &Path,
    fingerprint: ObjectFingerprint,
    content_type: &str,
) -> Result<(), String> {
    let payload = encode_sidecar(fingerprint, content_type);
    let mut file = fs::File::create(path)
        .await
        .map_err(|e| format!("storage: create metadata temp '{}': {e}", path.display()))?;
    let (res, _buf): (std::io::Result<()>, Vec<u8>) = file.write_all_at(payload, 0).await.into();
    res.map_err(|e| format!("storage: write metadata: {e}"))?;
    file.sync_all()
        .await
        .map_err(|e| format!("storage: fsync metadata: {e}"))
}

/// `fsync` a directory so a `rename` into it is durable. On POSIX the rename
/// is atomic but NOT durable: without this, a power loss can lose the
/// directory entry after `put` has already reported success (and already
/// billed `storage_bytes`), leaving the caller believing in an object that no
/// longer exists.
async fn sync_dir(dir: &Path) -> Result<(), String> {
    let handle = fs::File::open(dir)
        .await
        .map_err(|e| format!("storage: open dir '{}' for fsync: {e}", dir.display()))?;
    handle
        .sync_all()
        .await
        .map_err(|e| format!("storage: fsync dir '{}': {e}", dir.display()))
}

#[async_trait::async_trait(?Send)]
impl Backend for LocalFs {
    async fn put_stream(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        mut body: BoxChunkSource,
        content_type: Option<&str>,
    ) -> Result<u64, String> {
        validate_object_coords(app_id, bucket, key)?;
        let content_type = content_type.unwrap_or(DEFAULT_CONTENT_TYPE);
        let full = self.object_path(app_id, bucket, key);
        let meta_full = self.meta_path(app_id, bucket, key);
        let obj_dir = full
            .parent()
            .ok_or_else(|| "storage: object path has no parent".to_string())?
            .to_path_buf();
        let meta_dir = meta_full
            .parent()
            .ok_or_else(|| "storage: meta path has no parent".to_string())?
            .to_path_buf();
        for dir in [&obj_dir, &meta_dir] {
            fs::create_dir_all(dir)
                .await
                .map_err(|e| format!("storage: mkdir: {e}"))?;
        }

        // Write to a unique temp sibling, then atomically rename into place.
        // A crash or mid-stream error leaves only the temp file (cleaned up
        // on the error path), never a torn object at `full`.
        let tmp = temp_sibling(&full);
        let mut f = fs::File::create(&tmp)
            .await
            .map_err(|e| format!("storage: create temp '{}': {e}", tmp.display()))?;

        let mut offset: u64 = 0;
        let mut total: u64 = 0;
        // Yields the fingerprint of the finished temp file. Taken here, from
        // the still-open fd AFTER the final fsync, so it describes exactly the
        // bytes about to be renamed live — `rename` changes none of the four
        // fields it is built from.
        let write_result: Result<ObjectFingerprint, String> = async {
            while let Some(chunk) = body.next_chunk().await {
                let chunk = chunk?;
                if chunk.is_empty() {
                    continue;
                }
                let n = chunk.len() as u64;
                let buf = chunk.to_vec();
                let (res, _buf): (std::io::Result<()>, Vec<u8>) =
                    f.write_all_at(buf, offset).await.into();
                res.map_err(|e| format!("storage: write: {e}"))?;
                offset += n;
                total += n;
            }
            f.sync_all().await.map_err(|e| format!("storage: fsync: {e}"))?;
            let stat = f
                .metadata()
                .await
                .map_err(|e| format!("storage: stat temp: {e}"))?;
            Ok(ObjectFingerprint::of(&stat))
        }
        .await;

        let fingerprint = match write_result {
            Ok(fp) => fp,
            Err(e) => {
                drop(f);
                let _ = fs::remove_file(&tmp).await;
                return Err(e);
            }
        };
        drop(f);

        // Stage the sidecar BEFORE publishing the object, so a failure here
        // leaves the whole put un-published rather than half-applied.
        let meta_tmp = temp_sibling(&meta_full);
        if let Err(e) = write_sidecar(&meta_tmp, fingerprint, content_type).await {
            let _ = fs::remove_file(&tmp).await;
            let _ = fs::remove_file(&meta_tmp).await;
            return Err(e);
        }

        // Publish the bytes. From here the object is live; the sidecar that
        // currently sits beside it (if any) belongs to the PREVIOUS write, so
        // its fingerprint no longer matches and readers landing in this window
        // see DEFAULT_CONTENT_TYPE rather than the stale type.
        if let Err(e) = fs::rename(&tmp, &full).await {
            let _ = fs::remove_file(&tmp).await;
            let _ = fs::remove_file(&meta_tmp).await;
            return Err(format!("storage: rename temp into place: {e}"));
        }

        // Publish the metadata.
        if let Err(e) = fs::rename(&meta_tmp, &meta_full).await {
            let _ = fs::remove_file(&meta_tmp).await;
            return Err(format!("storage: rename metadata into place: {e}"));
        }

        // Make both renames durable. `rename` is atomic on POSIX but NOT
        // durable: without these, a power failure can lose the directory
        // entries AFTER this call has reported success and the caller has
        // already billed `storage_bytes` for the object.
        //
        // Object directory first, so the bytes are on stable storage before
        // the metadata that describes them. Losing the sidecar to a crash in
        // between costs the content type (the object reads back as
        // DEFAULT_CONTENT_TYPE); losing the object while keeping a sidecar
        // would leave a file describing nothing.
        sync_dir(&obj_dir).await?;
        sync_dir(&meta_dir).await?;
        Ok(total)
    }

    async fn get_stream(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, String> {
        validate_object_coords(app_id, bucket, key)?;
        let full = self.object_path(app_id, bucket, key);
        // Open FIRST, then `fstat` the open fd. Stat-then-open would let a
        // concurrent writer swap the object in between, so the advertised size
        // and the fingerprint could describe a different file than the one the
        // returned stream reads. Everything below describes exactly the fd the
        // caller is handed.
        let f = match fs::File::open(&full).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("storage: open: {e}")),
        };
        let meta = f
            .metadata()
            .await
            .map_err(|e| format!("storage: stat: {e}"))?;
        if !meta.is_file() {
            return Ok(None);
        }
        let size = meta.len();
        let modified_at = meta.modified().unwrap_or_else(|_| SystemTime::now());
        let content_type = self
            .read_content_type(app_id, bucket, key, ObjectFingerprint::of(&meta))
            .await;
        let object_meta = ObjectMeta {
            size,
            content_type: Some(content_type),
            modified_at,
        };
        let stream: BoxByteStream = Box::new(FileChunks {
            file: f,
            offset: 0,
            remaining: size,
        });
        Ok(Some((object_meta, stream)))
    }

    async fn delete(&self, app_id: &str, bucket: &str, key: &str) -> Result<bool, String> {
        validate_object_coords(app_id, bucket, key)?;
        let full = self.object_path(app_id, bucket, key);
        let existed = match fs::remove_file(&full).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(format!("storage: delete: {e}")),
        };
        // Drop the sidecar too. Correctness does not depend on it — a sidecar
        // outliving its object cannot match the fingerprint of whatever is
        // written at the key next — but leaving them behind would grow the
        // metadata tree without bound as keys churn.
        match fs::remove_file(self.meta_path(app_id, bucket, key)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("storage: delete metadata: {e}")),
        }
        Ok(existed)
    }

    async fn list(
        &self,
        app_id: &str,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<ListEntry>, String> {
        validate_list_coords(app_id, bucket)?;
        // Walks the `o/` subtree only. Metadata lives in a sibling subtree, so
        // it is not merely filtered out of the listing — it is not reachable
        // from the walk at all.
        let dir = self.objects_dir(app_id, bucket);
        let mut results = Vec::new();
        walk(&dir, &dir, prefix, &mut results).await?;
        results.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(results)
    }
}

/// A [`ChunkSource`] over an open file, read positionally in `READ_CHUNK`
/// slices until `remaining` is exhausted.
struct FileChunks {
    file: fs::File,
    offset: u64,
    remaining: u64,
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for FileChunks {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        if self.remaining == 0 {
            return None;
        }
        let want = READ_CHUNK.min(self.remaining) as usize;
        let buf = vec![0u8; want];
        // `read_exact_at` fills the whole buffer; we sized it to the bytes
        // we know remain, so a clean object always satisfies it. An error
        // here (e.g. the file was truncated under us) terminates the stream.
        let (res, bytes): (std::io::Result<()>, Vec<u8>) =
            self.file.read_exact_at(buf, self.offset).await.into();
        match res {
            Ok(()) => {
                self.offset += want as u64;
                self.remaining = self.remaining.saturating_sub(want as u64);
                Some(Ok(Bytes::from(bytes)))
            }
            Err(e) => {
                self.remaining = 0;
                Some(Err(format!("storage: read: {e}")))
            }
        }
    }
}

/// A unique temp sibling path next to the target object. The PID + a
/// monotonic counter keep concurrent writers to the same key from
/// colliding on the temp file before the atomic rename.
fn temp_sibling(full: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = full
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "obj".to_string());
    let tmp_name = format!(".{file_name}.tmp.{pid}.{n}");
    match full.parent() {
        Some(parent) => parent.join(tmp_name),
        None => PathBuf::from(tmp_name),
    }
}

// Directory walk is sync, and that is a known defect rather than a design
// choice: `list` is a registered native op (`lib.rs`, `r.add("list", ...)`)
// that app code calls per request through `env.storage.list`, so this
// blocking `std::fs::read_dir` runs on the thread that also drives V8 and
// every co-resident app's requests. A worker thread multiplexes many app
// isolates, so one app's walk over a large bucket stalls the others.
//
// It is also unbounded: no pagination, no max-keys, and the caller
// serialises the whole result into a single JSON string, so the cost scales
// with the bucket rather than with what the app asked for.
//
// `std::fs::read_dir` was chosen because compio's read_dir API churned
// across versions. That is a real constraint on the fix, not a reason the
// blocking call is acceptable - the options are compio's own filesystem
// API, an explicit blocking-task offload, or a bounded incremental walk.
async fn walk(
    base: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<ListEntry>,
) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("storage: readdir '{}': {e}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("storage: readdir entry: {e}"))?;
        let path = entry.path();
        // Skip in-flight temp files so a concurrent streaming put isn't
        // surfaced as a phantom listed object.
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with('.') && n.contains(".tmp."))
        {
            continue;
        }
        let meta = entry
            .metadata()
            .map_err(|e| format!("storage: stat entry: {e}"))?;
        if meta.is_dir() {
            Box::pin(walk(base, &path, prefix, out)).await?;
        } else if meta.is_file() {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let key = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if !prefix.is_empty() && !key.starts_with(prefix) {
                continue;
            }
            out.push(ListEntry {
                key,
                size: meta.len(),
                modified_at: meta.modified().unwrap_or_else(|_| SystemTime::now()),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "app_local";
    const BUCKET: &str = "uploads";

    /// A temp root that removes itself, so a failing assertion cannot leak a
    /// directory into `/tmp` that a later run then reads as existing state.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "zs-localfs-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fp(dev: u64, ino: u64, mtime: i64, mtime_nsec: i64, size: u64) -> ObjectFingerprint {
        ObjectFingerprint { dev, ino, mtime, mtime_nsec, size }
    }

    async fn put_bytes(be: &LocalFs, key: &str, body: &[u8], ct: Option<&str>) {
        be.put(APP, BUCKET, key, body, ct)
            .await
            .unwrap_or_else(|e| panic!("put {key}: {e}"));
    }

    async fn content_type_of(be: &LocalFs, key: &str) -> Option<String> {
        be.get(APP, BUCKET, key, 1 << 20)
            .await
            .expect("get")
            .expect("object present")
            .1
            .content_type
    }

    // -- sidecar codec ------------------------------------------------------

    #[test]
    fn sidecar_round_trips_under_a_matching_fingerprint() {
        let f = fp(1, 2, 3, 4, 5);
        let raw = encode_sidecar(f, "text/html");
        assert_eq!(decode_sidecar(&raw, f).as_deref(), Some("text/html"));
    }

    #[test]
    fn sidecar_is_rejected_when_any_fingerprint_field_differs() {
        let f = fp(1, 2, 3, 4, 5);
        let raw = encode_sidecar(f, "text/html");
        // Each field on its own must be enough to reject: this is what makes
        // the sidecar inert after a crash between the two renames, and what
        // covers inode reuse (same ino, different mtime/size).
        for other in [
            fp(9, 2, 3, 4, 5),
            fp(1, 9, 3, 4, 5),
            fp(1, 2, 9, 4, 5),
            fp(1, 2, 3, 9, 5),
            fp(1, 2, 3, 4, 9),
        ] {
            assert_eq!(
                decode_sidecar(&raw, other),
                None,
                "a sidecar for {f:?} must not be applied to {other:?}"
            );
        }
    }

    #[test]
    fn malformed_or_wrong_version_sidecars_are_ignored() {
        let f = fp(1, 2, 3, 4, 5);
        assert_eq!(decode_sidecar(b"", f), None, "empty");
        assert_eq!(decode_sidecar(b"not json", f), None, "garbage");
        assert_eq!(decode_sidecar(b"{}", f), None, "no fields");
        let bumped = serde_json::json!({
            "v": SIDECAR_VERSION + 1,
            "dev": 1, "ino": 2, "mtime": 3, "mtime_nsec": 4, "size": 5,
            "content_type": "text/html",
        })
        .to_string();
        assert_eq!(
            decode_sidecar(bumped.as_bytes(), f),
            None,
            "a future schema version must not be read as this one"
        );
    }

    // -- the crash / race window -------------------------------------------

    /// THE window this design exists to make safe. `put_stream` renames the
    /// object and then the sidecar; a crash (or a racing writer of the same
    /// key) can leave the NEW object next to an OLD sidecar. Reconstruct that
    /// state exactly — overwrite the object, then restore the previous
    /// sidecar over the new one — and assert the reader reports the neutral
    /// default rather than the previous writer's type.
    ///
    /// Does NOT catch: anything about how likely that interleaving is, or a
    /// torn/partial sidecar *write* (the sidecar is fsync'd before its rename,
    /// so a partial one is not reachable through `put_stream`; the malformed
    /// case is covered by the codec test above).
    #[test]
    fn a_stale_sidecar_never_lends_its_type_to_newer_bytes() {
        let root = TempRoot::new("stale");
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let be = LocalFs::new(&root.0);
            let key = "report.html";
            let meta_path = be.meta_path(APP, BUCKET, key);

            put_bytes(&be, key, b"<h1>first</h1>", Some("text/html")).await;
            assert_eq!(content_type_of(&be, key).await.as_deref(), Some("text/html"));
            let old_sidecar = std::fs::read(&meta_path).expect("sidecar written");

            // New bytes, new type — then wind the sidecar back to the old one,
            // which is precisely the on-disk state a crash between the two
            // renames leaves behind.
            put_bytes(&be, key, b"{\"second\":true}", Some("application/json")).await;
            std::fs::write(&meta_path, &old_sidecar).expect("restore old sidecar");

            assert_eq!(
                content_type_of(&be, key).await.as_deref(),
                Some(DEFAULT_CONTENT_TYPE),
                "a sidecar describing an older write must be ignored, not applied"
            );
        });
    }

    /// The same protection with the sidecar missing entirely (crash before it
    /// was ever renamed in): the object still reads, with the default type.
    #[test]
    fn a_missing_sidecar_degrades_the_type_but_not_the_object() {
        let root = TempRoot::new("missing");
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let be = LocalFs::new(&root.0);
            let key = "orphan.bin";
            put_bytes(&be, key, b"payload", Some("image/png")).await;
            std::fs::remove_file(be.meta_path(APP, BUCKET, key)).expect("drop sidecar");

            let (body, meta) = be.get(APP, BUCKET, key, 1 << 20).await.unwrap().unwrap();
            assert_eq!(body, b"payload", "bytes must still be readable");
            assert_eq!(meta.content_type.as_deref(), Some(DEFAULT_CONTENT_TYPE));
        });
    }

    // -- keyspace / layout --------------------------------------------------

    /// The reason metadata lives in its own subtree. A creator key spelled
    /// like a sibling-style sidecar (`.<name>.meta`) must be an ordinary
    /// object: storable, listable, readable, and NOT destroyed by writing the
    /// object whose sidecar it would have shadowed.
    ///
    /// Does NOT catch: a creator key colliding with the `o/`/`m/` split
    /// itself, which is impossible by construction — every key is rooted
    /// inside `o/`.
    #[test]
    fn a_key_shaped_like_a_sidecar_is_just_an_object() {
        let root = TempRoot::new("collide");
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let be = LocalFs::new(&root.0);
            put_bytes(&be, ".avatar.png.meta", b"creator bytes", Some("text/plain")).await;
            put_bytes(&be, "avatar.png", b"PNG", Some("image/png")).await;

            let (body, meta) = be
                .get(APP, BUCKET, ".avatar.png.meta", 1 << 20)
                .await
                .unwrap()
                .expect("the sidecar-shaped key must survive the neighbouring put");
            assert_eq!(body, b"creator bytes");
            assert_eq!(meta.content_type.as_deref(), Some("text/plain"));
            assert_eq!(
                content_type_of(&be, "avatar.png").await.as_deref(),
                Some("image/png")
            );

            let keys: Vec<String> =
                be.list(APP, BUCKET, "").await.unwrap().into_iter().map(|e| e.key).collect();
            assert_eq!(keys, vec![".avatar.png.meta".to_string(), "avatar.png".to_string()]);
        });
    }

    /// Nested keys put their metadata in the mirrored `m/` path, and listing
    /// still reports the object keys only.
    #[test]
    fn nested_keys_round_trip_their_type_and_list_cleanly() {
        let root = TempRoot::new("nested");
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let be = LocalFs::new(&root.0);
            put_bytes(&be, "a/b/c.svg", b"<svg/>", Some("image/svg+xml")).await;
            assert_eq!(
                content_type_of(&be, "a/b/c.svg").await.as_deref(),
                Some("image/svg+xml")
            );
            let keys: Vec<String> =
                be.list(APP, BUCKET, "").await.unwrap().into_iter().map(|e| e.key).collect();
            assert_eq!(keys, vec!["a/b/c.svg".to_string()]);

            // A key that names an intermediate DIRECTORY is not an object.
            // `get_stream` opens before it stats (so the size and fingerprint
            // describe the same fd the caller reads), and `open` succeeds on a
            // directory — the `is_file` check on the fstat is what keeps this
            // `None` instead of handing back a directory fd.
            for dir_key in ["a", "a/b"] {
                assert!(
                    be.get_stream(APP, BUCKET, dir_key).await.unwrap().is_none(),
                    "a directory key ({dir_key}) must not read back as an object"
                );
            }
        });
    }
}
