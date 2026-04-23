//! Local filesystem `Backend` — the dev default, always available.
//!
//! Storage layout: `<root>/<app_id>/<bucket>/<key>`. The key may contain
//! `/` (nested paths); each segment is validated by
//! `super::validate_object_coords` before any filesystem op.
//!
//! Uses compio's `AsyncWriteAt` / `AsyncReadAt` for positional I/O on
//! io_uring. Zero tokio.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use compio::fs;
use compio::io::{AsyncReadAtExt, AsyncWriteAtExt};

use super::{Backend, ListEntry, ObjectMeta};

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
    async fn put(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        bytes: &[u8],
        _content_type: Option<&str>,
    ) -> Result<u64, String> {
        super::validate_object_coords(app_id, bucket, key)?;
        let full = self.object_path(app_id, bucket, key);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).await
                .map_err(|e| format!("storage: mkdir: {e}"))?;
        }
        let mut f = fs::File::create(&full).await
            .map_err(|e| format!("storage: create '{}': {e}", full.display()))?;
        let buf = bytes.to_vec();
        let (res, _buf): (std::io::Result<()>, Vec<u8>) = f.write_all_at(buf, 0).await.into();
        res.map_err(|e| format!("storage: write: {e}"))?;
        f.sync_all().await.map_err(|e| format!("storage: fsync: {e}"))?;
        // TODO: content_type sidecar metadata file (v1 returns None on get)
        Ok(bytes.len() as u64)
    }

    async fn get(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, ObjectMeta)>, String> {
        super::validate_object_coords(app_id, bucket, key)?;
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
        let f = fs::File::open(&full).await
            .map_err(|e| format!("storage: open: {e}"))?;
        let buf = Vec::with_capacity(size as usize);
        let (res, bytes): (std::io::Result<usize>, Vec<u8>) =
            f.read_to_end_at(buf, 0).await.into();
        res.map_err(|e| format!("storage: read: {e}"))?;
        Ok(Some((bytes, ObjectMeta { size, content_type: None, modified_at })))
    }

    async fn delete(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<bool, String> {
        super::validate_object_coords(app_id, bucket, key)?;
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
        // Listing still needs app_id/bucket validation but allows empty key
        // (that's what "list all" means). We duplicate a slimmer check here
        // because validate_object_coords rejects empty keys.
        if app_id.contains('/') || app_id.contains("..") || app_id.is_empty() {
            return Err(format!("storage: invalid app_id '{app_id}'"));
        }
        if bucket.contains('/') || bucket == "." || bucket == ".." || bucket.is_empty() {
            return Err(format!("storage: invalid bucket name '{bucket}'"));
        }
        let dir = self.bucket_dir(app_id, bucket);
        let mut results = Vec::new();
        walk(&dir, &dir, prefix, &mut results).await?;
        results.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(results)
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
        let meta = entry.metadata()
            .map_err(|e| format!("storage: stat entry: {e}"))?;
        if meta.is_dir() {
            Box::pin(walk(base, &path, prefix, out)).await?;
        } else if meta.is_file() {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let key = rel.components()
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
