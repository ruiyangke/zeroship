//! `--blob-store` URL parsing shared by control, worker, and gateway.
//!
//! One grammar, one builder, so all three services resolve the same store
//! from the same flag/env. The deploy ingest (control) writes through exactly
//! the store the gateway and worker read.
//!
//! Grammar:
//!
//! - `s3://bucket/prefix?region=...` -> [`S3BlobStore`], built from an
//!   [`S3Runtime`] the CALLING process resolved. This crate reads no
//!   environment of its own; see [`S3Runtime`] for why.
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

/// Everything an S3-backed blob store needs that a process resolves for it.
///
/// This crate must NOT read `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
/// `AWS_SESSION_TOKEN` or `ZEROSHIP_BLOB_UPLOAD_CONCURRENCY` itself, and that is
/// a STRUCTURAL reason rather than a stylistic one:
/// `zeroship-core` depends on `zeroship-bundle`, so bundle cannot use the typed
/// environment keys that make a read enumerable, and a raw read here would be
/// exactly the invisible read Step 4 of
/// `docs/proposals/2026-08-11-config-name-alignment.md` removes.
///
/// The rule that resolves it is the one Section 4.5 states for publishable
/// libraries: production library APIs accept resolved options, and platform
/// processes declare and read any external credentials before injecting them.
/// Bundle is not in `libs/`, but the dependency direction puts it in the same
/// position, so it gets the same shape.
#[derive(Debug, Clone)]
pub struct S3Runtime {
    /// Static credentials. There is no provider chain and no metadata-service
    /// lookup; the `compio-s3` client is static-credential only.
    pub credentials: S3Credentials,
    /// In-flight multipart part uploads, already clamped by
    /// [`crate::limits::resolve_upload_concurrency`].
    pub upload_concurrency: usize,
}

impl S3Runtime {
    /// Assemble from values a caller has already read.
    ///
    /// # Errors
    /// Returns [`BlobStoreConfigError::Credentials`] if either the access key
    /// id or the secret access key is absent or empty. An empty value is
    /// treated as absent because that is how a Compose file spells "unset".
    pub fn from_resolved(
        access_key_id: Option<String>,
        secret_access_key: Option<String>,
        session_token: Option<String>,
        upload_concurrency: usize,
    ) -> Result<Self, BlobStoreConfigError> {
        let access = access_key_id.filter(|v| !v.is_empty()).ok_or_else(|| {
            BlobStoreConfigError::Credentials("AWS_ACCESS_KEY_ID is unset".into())
        })?;
        let secret = secret_access_key.filter(|v| !v.is_empty()).ok_or_else(|| {
            BlobStoreConfigError::Credentials("AWS_SECRET_ACCESS_KEY is unset".into())
        })?;
        let session = session_token.filter(|v| !v.is_empty());
        Ok(Self {
            credentials: S3Credentials::new(access, secret, session),
            upload_concurrency,
        })
    }
}

/// Build an `Arc<dyn BlobStore>` from a parsed [`StoreUrl`].
///
/// `s3` is required for an `s3://` location and ignored for a local one. It is
/// an `Option` rather than a second function so a caller that does not yet know
/// which kind of location it has still has one call site.
///
/// # Errors
/// Returns [`BlobStoreConfigError::Credentials`] when an `s3://` location is
/// built without resolved runtime values, and propagates local-store
/// construction failures.
pub fn build_blob_store(
    url: &StoreUrl,
    s3: Option<&S3Runtime>,
) -> Result<Arc<dyn BlobStore>, BlobStoreConfigError> {
    match url {
        StoreUrl::Local(root) => {
            let store = LocalDiskBlobStore::new(root.clone())?;
            Ok(Arc::new(store))
        }
        StoreUrl::S3(cfg) => {
            let runtime = s3.ok_or_else(|| {
                BlobStoreConfigError::Credentials(
                    "an s3:// blob store needs resolved credentials from its caller".into(),
                )
            })?;
            Ok(Arc::new(S3BlobStore::new(
                cfg.clone(),
                runtime.credentials.clone(),
                runtime.upload_concurrency,
            )))
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
        assert!(matches!(u, StoreUrl::Local(p) if p == std::path::Path::new("/var/lib/zeroship/blobs")));
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
        let store = build_blob_store(&u, None).expect("local build");
        assert!(store.local_path(&"a".repeat(64)).is_some());
        assert!(root.join("blobs").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
