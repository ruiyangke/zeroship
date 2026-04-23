//! Backend abstraction for storage operations.
//!
//! Today: local filesystem only. The `Backend` trait exists so an S3
//! implementation can slot in without touching the callback or SDK layers.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use compio::fs;
use compio::io::{AsyncReadAtExt, AsyncWriteAtExt};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ObjectMeta {
    pub size: u64,
    pub content_type: Option<String>,
    pub modified_at: SystemTime,
}

#[derive(Debug)]
pub struct ListEntry {
    pub key: String,
    pub size: u64,
    pub modified_at: SystemTime,
}

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

/// Resolve `<root>/<app_id>/<bucket>/<key>` while rejecting path traversal.
///
/// Errors when the key contains `..` or absolute components that would
/// escape the app's storage dir. Multi-level keys (e.g. `avatars/2026/u.png`)
/// are allowed — each component is validated individually.
pub fn resolve_object_path(
    root: &Path,
    app_id: &str,
    bucket: &str,
    key: &str,
) -> Result<PathBuf, String> {
    if app_id.is_empty() || bucket.is_empty() || key.is_empty() {
        return Err("storage: app_id/bucket/key must all be non-empty".into());
    }
    // Bucket names: single segment only, no slashes.
    if bucket.contains('/') || bucket == "." || bucket == ".." {
        return Err(format!("storage: invalid bucket name '{bucket}'"));
    }
    // App id: single segment only.
    if app_id.contains('/') || app_id.contains("..") {
        return Err(format!("storage: invalid app_id '{app_id}'"));
    }
    let mut path = root.join(app_id).join(bucket);
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("storage: invalid key '{key}'"));
        }
        path.push(segment);
    }
    Ok(path)
}

fn bucket_dir(root: &Path, app_id: &str, bucket: &str) -> Result<PathBuf, String> {
    if app_id.contains('/') || app_id.contains("..") || app_id.is_empty() {
        return Err(format!("storage: invalid app_id '{app_id}'"));
    }
    if bucket.contains('/') || bucket == "." || bucket == ".." || bucket.is_empty() {
        return Err(format!("storage: invalid bucket name '{bucket}'"));
    }
    Ok(root.join(app_id).join(bucket))
}

// ---------------------------------------------------------------------------
// Local filesystem operations
// ---------------------------------------------------------------------------

pub async fn put(
    root: &Path,
    app_id: &str,
    bucket: &str,
    key: &str,
    bytes: &[u8],
    _content_type: Option<&str>,
) -> Result<u64, String> {
    let full = resolve_object_path(root, app_id, bucket, key)?;
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
    // TODO: persist content_type once we have a sidecar metadata format (v1 just returns None on get)
    Ok(bytes.len() as u64)
}

pub async fn get(
    root: &Path,
    app_id: &str,
    bucket: &str,
    key: &str,
) -> Result<Option<(Vec<u8>, ObjectMeta)>, String> {
    let full = resolve_object_path(root, app_id, bucket, key)?;
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
    let (res, bytes): (std::io::Result<usize>, Vec<u8>) = f.read_to_end_at(buf, 0).await.into();
    res.map_err(|e| format!("storage: read: {e}"))?;
    Ok(Some((
        bytes,
        ObjectMeta {
            size,
            content_type: None,
            modified_at,
        },
    )))
}

pub async fn delete(
    root: &Path,
    app_id: &str,
    bucket: &str,
    key: &str,
) -> Result<bool, String> {
    let full = resolve_object_path(root, app_id, bucket, key)?;
    match fs::remove_file(&full).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("storage: delete: {e}")),
    }
}

pub async fn list(
    root: &Path,
    app_id: &str,
    bucket: &str,
    prefix: &str,
) -> Result<Vec<ListEntry>, String> {
    let dir = bucket_dir(root, app_id, bucket)?;
    let mut results = Vec::new();
    walk(&dir, &dir, prefix, &mut results).await?;
    // Stable sort by key so clients see a deterministic order.
    results.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(results)
}

async fn walk(
    base: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<ListEntry>,
) -> Result<(), String> {
    // Compio's fs::read_dir isn't available in all versions — fall back to
    // std::fs for the directory walk. Each entry then uses compio fs::metadata.
    // Listing is not on the per-request hot path, so the sync walk is fine.
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
            // Normalise to forward slashes so the key matches the SDK's contract.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        let root = Path::new("/tmp/zs-storage-test");
        assert!(resolve_object_path(root, "app", "uploads", "../escape").is_err());
        assert!(resolve_object_path(root, "app", "uploads", "a/../b").is_err());
        assert!(resolve_object_path(root, "app", "uploads", ".").is_err());
        assert!(resolve_object_path(root, "app", "uploads", "").is_err());
    }

    #[test]
    fn rejects_bad_buckets() {
        let root = Path::new("/tmp/zs-storage-test");
        assert!(resolve_object_path(root, "app", "a/b", "key").is_err());
        assert!(resolve_object_path(root, "app", "..", "key").is_err());
    }

    #[test]
    fn allows_nested_keys() {
        let root = Path::new("/tmp/zs-storage-test");
        let p = resolve_object_path(root, "app", "uploads", "avatars/2026/u.png").unwrap();
        assert!(p.ends_with("app/uploads/avatars/2026/u.png"));
    }
}
