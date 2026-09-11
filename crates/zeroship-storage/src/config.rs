//! Backend selection shared by Rust hosts, the worker and the CLI.
//!
//! Bare paths and `file://` locations select the local filesystem. An
//! `s3://` location selects the S3-compatible backend. Credentials resolve
//! through declared AWS environment variables; code can inject a backend with
//! explicit credentials through `StorageStore::from_backend`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::{Backend, LocalFs};

/// A parsed object-storage location for Rust hosts.
#[derive(Debug, Clone)]
pub enum StorageBackendConfig {
    /// Local filesystem root (dev default).
    Local(PathBuf),
    /// Remote S3-compatible store (parsed + validated config).
    #[cfg(feature = "s3")]
    S3(compio_s3::S3Config),
}

/// Errors building a storage backend from `--storage-url`.
#[derive(Debug, thiserror::Error)]
pub enum StorageConfigError {
    /// The storage location failed validation.
    #[error("invalid storage location: {0}")]
    Url(String),
    /// An `s3://` URL was supplied but the crate was built without the `s3`
    /// feature, so no S3 backend exists to construct.
    #[error("s3:// storage-url requires the zeroship-storage `s3` feature")]
    S3FeatureDisabled,
    /// S3 credentials were not resolvable from the environment.
    #[error("missing S3 credentials: {0}")]
    Credentials(String),
}

impl StorageBackendConfig {
    /// Classify a raw `--storage-url` value. An `s3://` scheme parses +
    /// validates the full S3 config now (so misconfiguration fails fast at
    /// startup). Bare paths and `file://` select local storage; other schemes
    /// and empty locations are rejected.
    ///
    /// # Errors
    /// Returns [`StorageConfigError::Url`] for an invalid location, or [`StorageConfigError::S3FeatureDisabled`] when an
    /// `s3://` URL is given but the `s3` feature is off.
    pub fn parse(raw: &str) -> Result<Self, StorageConfigError> {
        let raw = raw.strip_prefix("file://").unwrap_or(raw);
        if raw.is_empty() {
            return Err(StorageConfigError::Url("location is empty".into()));
        }
        if raw.starts_with("s3://") {
            #[cfg(feature = "s3")]
            {
                let cfg = compio_s3::S3Config::parse_url(raw)
                    .map_err(|e| StorageConfigError::Url(e.to_string()))?;
                Ok(Self::S3(cfg))
            }
            #[cfg(not(feature = "s3"))]
            {
                Err(StorageConfigError::S3FeatureDisabled)
            }
        } else if raw.contains("://") {
            Err(StorageConfigError::Url("unsupported storage URL scheme".into()))
        } else {
            Ok(Self::Local(PathBuf::from(raw)))
        }
    }

    /// True for a remote (S3) location. Used by `--check-config` reporting.
    #[must_use]
    pub const fn is_remote(&self) -> bool {
        match self {
            Self::Local(_) => false,
            #[cfg(feature = "s3")]
            Self::S3(_) => true,
        }
    }

    /// Backend kind label for `--check-config` (`local` | `s3`).
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Local(_) => "local",
            #[cfg(feature = "s3")]
            Self::S3(_) => "s3",
        }
    }
}

/// Resolve static S3 credentials from the conventional AWS environment
/// variables, matching the deploy blob store's resolver so one process
/// identity covers both deploy blobs and `env.storage`.
///
/// # Errors
/// Returns [`StorageConfigError::Credentials`] if either the access key id or
/// the secret access key is absent/empty.
#[cfg(feature = "s3")]
pub fn s3_credentials_from_env() -> Result<compio_s3::S3Credentials, StorageConfigError> {
    // Class `external`: the `AWS_*` trio is Amazon's contract, honoured by
    // every S3-compatible tool in the deployment, not a zeroship name.
    let access = zeroship_core::declared_env!(
        external,
        "AWS_ACCESS_KEY_ID",
        crate::StorageConsumer
    )
        .filter(|v| !v.is_empty())
        .ok_or_else(|| StorageConfigError::Credentials("AWS_ACCESS_KEY_ID is unset".into()))?;
    let secret = zeroship_core::declared_env!(
        external,
        "AWS_SECRET_ACCESS_KEY",
        crate::StorageConsumer
    )
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            StorageConfigError::Credentials("AWS_SECRET_ACCESS_KEY is unset".into())
        })?;
    let session = zeroship_core::declared_env!(
        external,
        "AWS_SESSION_TOKEN",
        crate::StorageConsumer
    )
    .filter(|v| !v.is_empty());
    Ok(compio_s3::S3Credentials::new(access, secret, session))
}

/// Build an `Arc<dyn Backend>` from a parsed [`StorageBackendConfig`]. The S3
/// leg pulls credentials from the environment via [`s3_credentials_from_env`].
///
/// # Errors
/// Propagates credential-resolution failures for the S3 leg.
pub fn build_backend(
    cfg: &StorageBackendConfig,
) -> Result<Arc<dyn Backend>, StorageConfigError> {
    match cfg {
        StorageBackendConfig::Local(root) => Ok(Arc::new(LocalFs::new(root.clone()))),
        #[cfg(feature = "s3")]
        StorageBackendConfig::S3(s3cfg) => {
            let creds = s3_credentials_from_env()?;
            Ok(Arc::new(crate::backend::S3::new(s3cfg.clone(), creds)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_path_is_local() {
        let c = StorageBackendConfig::parse("./storage").unwrap();
        assert!(matches!(c, StorageBackendConfig::Local(_)));
        assert!(!c.is_remote());
        assert_eq!(c.kind(), "local");
    }

    #[test]
    fn absolute_path_is_local() {
        let c = StorageBackendConfig::parse("/var/lib/zeroship/storage").unwrap();
        assert!(
            matches!(c, StorageBackendConfig::Local(p) if p == std::path::Path::new("/var/lib/zeroship/storage"))
        );
    }

    #[test]
    fn file_urls_share_the_path_parser() {
        for (url, path) in [("file:///var/lib/storage", "/var/lib/storage"), ("file://.zeroship/storage", ".zeroship/storage")] {
            assert!(matches!(StorageBackendConfig::parse(url).unwrap(), StorageBackendConfig::Local(root) if root == std::path::Path::new(path)));
        }
        for invalid in ["", "file://", "https://storage", "redis://storage"] {
            assert!(StorageBackendConfig::parse(invalid).is_err());
        }
    }

    #[cfg(feature = "s3")]
    #[test]
    fn s3_url_parses_to_remote() {
        let c = StorageBackendConfig::parse(
            "s3://bucket/storage?provider=minio&endpoint=http://127.0.0.1:9000&region=us-east-1&style=path&dev_http=true",
        )
        .unwrap();
        assert!(c.is_remote());
        assert_eq!(c.kind(), "s3");
    }

    #[cfg(feature = "s3")]
    #[test]
    fn malformed_s3_url_is_rejected() {
        // s3:// scheme but missing required region → fail fast.
        assert!(matches!(
            StorageBackendConfig::parse("s3://bucket/storage?provider=minio&endpoint=http://127.0.0.1:9000&style=path&dev_http=true"),
            Err(StorageConfigError::Url(_))
        ));
    }

    #[cfg(not(feature = "s3"))]
    #[test]
    fn s3_url_without_feature_is_rejected() {
        assert!(matches!(
            StorageBackendConfig::parse("s3://bucket/storage?region=us-east-1"),
            Err(StorageConfigError::S3FeatureDisabled)
        ));
    }
}
