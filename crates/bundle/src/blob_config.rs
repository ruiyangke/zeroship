//! `--blob-store` URL parsing shared by control, worker, and gateway.
//!
//! One grammar, one builder, so all three services resolve the same store
//! from the same flag/env. The deploy ingest (control) writes through exactly
//! the store the gateway and worker read.
//!
//! Grammar:
//!
//! - `s3://bucket/prefix?region=…` → [`S3BlobStore`], credentials resolved
//!   from the standard AWS environment variables.
//! - any other value → a bare local filesystem path → [`LocalDiskBlobStore`]
//!   (the dev default, unchanged).

use std::path::PathBuf;
use std::sync::Arc;

use compio_s3::{S3Config, S3Credentials};

use crate::blob::{BlobStore, LocalDiskBlobStore};
use crate::s3_blob::S3BlobStore;

/// A parsed `--blob-store` location.
#[derive(Debug, Clone)]
pub enum StoreUrl {
    /// Local filesystem root (dev default).
    Local(PathBuf),
    /// Remote S3-compatible store.
    S3(S3Config),
}

/// Errors building a blob store from `--blob-store`.
#[derive(Debug, thiserror::Error)]
pub enum BlobStoreConfigError {
    /// The `s3://…` URL failed to parse/validate.
    #[error("invalid s3:// blob-store url: {0}")]
    Url(String),
    /// S3 credentials were not resolvable from the environment.
    #[error("missing S3 credentials: {0}")]
    Credentials(String),
    /// Building the local store failed (I/O).
    #[error("local blob store: {0}")]
    Local(#[from] std::io::Error),
}

impl StoreUrl {
    /// Classify a raw `--blob-store` value. An `s3://` scheme parses+validates
    /// the full S3 config now (so misconfiguration fails fast at startup);
    /// anything else is treated as a local filesystem path.
    ///
    /// # Errors
    /// Returns [`BlobStoreConfigError::Url`] when an `s3://` value fails to
    /// parse or validate.
    pub fn parse(raw: &str) -> Result<Self, BlobStoreConfigError> {
        if raw.starts_with("s3://") {
            let cfg = S3Config::parse_url(raw)
                .map_err(|e| BlobStoreConfigError::Url(e.to_string()))?;
            Ok(Self::S3(cfg))
        } else {
            Ok(Self::Local(PathBuf::from(raw)))
        }
    }

    /// True for a remote (S3) location. Used by `--check-config` reporting.
    #[must_use]
    pub const fn is_remote(&self) -> bool {
        matches!(self, Self::S3(_))
    }
}

/// Resolve static S3 credentials from the conventional AWS environment
/// variables. There is no provider chain and no metadata-service lookup (the
/// `compio-s3` client is static-credential only).
///
/// # Errors
/// Returns [`BlobStoreConfigError::Credentials`] if either the access key id
/// or secret access key is absent/empty.
pub fn s3_credentials_from_env() -> Result<S3Credentials, BlobStoreConfigError> {
    let access = std::env::var("AWS_ACCESS_KEY_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            BlobStoreConfigError::Credentials("AWS_ACCESS_KEY_ID is unset".into())
        })?;
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            BlobStoreConfigError::Credentials("AWS_SECRET_ACCESS_KEY is unset".into())
        })?;
    let session = std::env::var("AWS_SESSION_TOKEN").ok().filter(|v| !v.is_empty());
    Ok(S3Credentials::new(access, secret, session))
}

/// Build an `Arc<dyn BlobStore>` from a parsed [`StoreUrl`]. S3 stores pull
/// credentials from the environment via [`s3_credentials_from_env`].
///
/// # Errors
/// Propagates credential-resolution and local-store construction failures.
pub fn build_blob_store(
    url: &StoreUrl,
) -> Result<Arc<dyn BlobStore>, BlobStoreConfigError> {
    match url {
        StoreUrl::Local(root) => {
            let store = LocalDiskBlobStore::new(root.clone())?;
            Ok(Arc::new(store))
        }
        StoreUrl::S3(cfg) => {
            let creds = s3_credentials_from_env()?;
            Ok(Arc::new(S3BlobStore::new(cfg.clone(), creds)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_path_is_local() {
        let u = StoreUrl::parse("./bundles").unwrap();
        assert!(matches!(u, StoreUrl::Local(_)));
        assert!(!u.is_remote());
    }

    #[test]
    fn absolute_path_is_local() {
        let u = StoreUrl::parse("/var/lib/zeroship/blobs").unwrap();
        assert!(matches!(u, StoreUrl::Local(p) if p == PathBuf::from("/var/lib/zeroship/blobs")));
    }

    #[test]
    fn s3_url_parses_to_remote() {
        let u = StoreUrl::parse("s3://bucket/prefix?region=us-east-1").unwrap();
        assert!(u.is_remote());
        match u {
            StoreUrl::S3(cfg) => {
                assert_eq!(cfg.bucket, "bucket");
                assert_eq!(cfg.prefix.as_deref(), Some("prefix"));
            }
            StoreUrl::Local(_) => panic!("expected S3"),
        }
    }

    #[test]
    fn malformed_s3_url_is_rejected() {
        // s3:// scheme but missing required region → fail fast.
        assert!(StoreUrl::parse("s3://bucket/prefix").is_err());
    }

    #[test]
    fn build_local_store_creates_root() {
        let mut root = std::env::temp_dir();
        root.push(format!("zsblobcfg-{}", uuid::Uuid::new_v4().simple()));
        let u = StoreUrl::Local(root.clone());
        let store = build_blob_store(&u).expect("local build");
        assert!(store.local_path(&"a".repeat(64)).is_some());
        assert!(root.join("blobs").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
