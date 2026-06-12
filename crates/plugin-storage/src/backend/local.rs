//! Local filesystem `Backend` — the dev default, always available.
//!
//! Storage layout: `<root>/<app_id>/<bucket>/<key>`. The key may contain
//! `/` (nested paths); each segment is validated by
//! `super::validate_object_coords` before any filesystem op.
//!
//! Uses compio's `AsyncWriteAt` / `AsyncReadAt` for positional I/O on
//! io_uring. Zero tokio.
//!
//! Streaming: `put_stream` writes chunks to a sibling temp file and
//! `rename`s it into place (atomic on POSIX — a reader never observes a
//! half-written object). `get_stream` reads the file back in bounded
//! chunks so a large object never lands fully in RAM.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bytes::Bytes;
use compio::fs;
use compio::io::{AsyncReadAtExt, AsyncWriteAtExt};

use super::{
    validate_list_coords, validate_object_coords, Backend, BoxByteStream, BoxChunkSource,
    ChunkResult, ChunkSource, ListEntry, ObjectMeta,
};

/// Bytes read per `get_stream` chunk. 256 KiB balances syscall count
/// against per-chunk allocation; the consumer pulls these one at a time.
const READ_CHUNK: u64 = 256 * 1024;

#[derive(Debug, Clone)]
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn object_path(&self, app_id: &str, bucket: &str, key: &str) -> PathBuf {
        // Safety: caller already ran validate_object_coords, so no path-
        // traversal segments can reach here. We still `push` segment-by-
        // segment (not via Path::new) so a '/' in a key component — which
        // *would* have been rejected — can never be interpreted as a
        // directory separator by the OS path parser.
        let mut path = self.root.join(app_id).join(bucket);
        for segment in key.split('/') {
            path.push(segment);
        }
        path
    }

    fn bucket_dir(&self, app_id: &str, bucket: &str) -> PathBuf {
        self.root.join(app_id).join(bucket)
    }
}

#[async_trait::async_trait(?Send)]
impl Backend for LocalFs {
    async fn put_stream(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        mut body: BoxChunkSource,
        _content_type: Option<&str>,
    ) -> Result<u64, String> {
        validate_object_coords(app_id, bucket, key)?;
        let full = self.object_path(app_id, bucket, key);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)
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
        let write_result: Result<(), String> = async {
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
            Ok(())
        }
        .await;

        if let Err(e) = write_result {
            drop(f);
            let _ = fs::remove_file(&tmp).await;
            return Err(e);
        }
        drop(f);

        fs::rename(&tmp, &full).await.map_err(|e| {
            // Best-effort cleanup; the object slot is untouched on failure.
            let tmp2 = tmp.clone();
            compio::runtime::spawn(async move {
                let _ = fs::remove_file(&tmp2).await;
            })
            .detach();
            format!("storage: rename temp into place: {e}")
        })?;
        // TODO: content_type sidecar metadata file (v1 returns None on get)
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
        let meta = match fs::metadata(&full).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("storage: stat: {e}")),
        };
        if !meta.is_file() {
            return Ok(None);
        }
        let size = meta.len();
        let modified_at = meta.modified().unwrap_or_else(|_| SystemTime::now());
        let f = fs::File::open(&full)
            .await
            .map_err(|e| format!("storage: open: {e}"))?;
        let object_meta = ObjectMeta {
            size,
            content_type: None,
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
        match fs::remove_file(&full).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("storage: delete: {e}")),
        }
    }

    async fn list(
        &self,
        app_id: &str,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<ListEntry>, String> {
        validate_list_coords(app_id, bucket)?;
        let dir = self.bucket_dir(app_id, bucket);
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

// Directory walk is sync — listing isn't on the per-request hot path, and
// compio's read_dir API churned across versions. std::fs::read_dir is the
// stable choice; a dedicated blocking-task offload would be overkill here.
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
