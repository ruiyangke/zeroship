//! Backend abstraction for storage operations.
//!
//! Multiple implementations slot in behind the `Backend` trait:
//! - `LocalFs` — filesystem-backed, always available (the dev default)
//! - `S3` — S3/R2/MinIO/Spaces/B2 via S3-API (behind the `s3` feature)
//!
//! The trait methods are async + `Send + Sync` so they can live behind
//! `Arc<dyn Backend>` and survive the compio executor's work-stealing.
//!
//! Error type is `String` — every op ultimately surfaces to JS via the
//! callback layer's `OpResult::Failed { error: String }`, so anything
//! richer would get flattened there anyway.

use std::time::SystemTime;

pub mod local;
pub use local::LocalFs;

// ---------------------------------------------------------------------------
// Types shared by all backends
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub size: u64,
    pub content_type: Option<String>,
    pub modified_at: SystemTime,
}

#[derive(Debug, Clone)]
pub struct ListEntry {
    pub key: String,
    pub size: u64,
    pub modified_at: SystemTime,
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// Pluggable object-storage backend. All implementations see the same
/// namespacing contract — the kernel enforces `<app_id>/<bucket>/<key>`
/// separation before the backend sees anything, so implementations only
/// worry about their own I/O, not multi-tenancy.
///
/// Path-traversal rejection lives in `validate_object_coords` below, not
/// per-backend — every implementation gets it for free by calling the
/// helper at the top of each op.
// Trait is `Send + Sync` so it can live behind `Arc<dyn Backend>` on a
// `NativePlugin` (which requires Send+Sync). Methods return `!Send`
// futures via `(?Send)` because compio's async fs ops hold thread-local
// state. That's fine — storage ops are always awaited on the same thread
// that owns the worker's V8 isolate.
#[async_trait::async_trait(?Send)]
pub trait Backend: Send + Sync + std::fmt::Debug {
    async fn put(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> Result<u64, String>;

    async fn get(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, ObjectMeta)>, String>;

    async fn delete(
        &self,
        app_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<bool, String>;

    async fn list(
        &self,
        app_id: &str,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<ListEntry>, String>;
}

// ---------------------------------------------------------------------------
// Shared validation — every backend calls this before any I/O.
// ---------------------------------------------------------------------------

/// Reject inputs that would escape an app's keyspace. Used by every
/// `Backend` impl; shipping it centrally means adding a new backend
/// can't accidentally forget the check.
pub fn validate_object_coords(app_id: &str, bucket: &str, key: &str) -> Result<(), String> {
    if app_id.is_empty() || bucket.is_empty() || key.is_empty() {
        return Err("storage: app_id/bucket/key must all be non-empty".into());
    }
    if app_id.contains('/') || app_id.contains("..") {
        return Err(format!("storage: invalid app_id '{app_id}'"));
    }
    if bucket.contains('/') || bucket == "." || bucket == ".." {
        return Err(format!("storage: invalid bucket name '{bucket}'"));
    }
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("storage: invalid key '{key}'"));
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
}
