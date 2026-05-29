//! Shared optional file-overlay configuration.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Error returned while loading an optional zeroship configuration file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("read {path}: {source}")]
    Io {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },

    /// The configuration file could not be parsed as TOML.
    #[error("parse {path}: {source}")]
    Parse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying TOML parser error.
        #[source]
        source: toml::de::Error,
    },
}

/// Optional cross-binary domain configuration loaded from `ops/zeroship.toml`.
// No deny_unknown_fields: future sections must be tolerated by older binaries.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FileConfig {
    /// Auth-domain configuration shared by binaries that integrate with Hydra.
    #[serde(default)]
    pub auth: AuthSection,
    /// Observability configuration shared by platform binaries.
    #[serde(default)]
    pub observability: ObsSection,
}

/// Auth-domain values that can be supplied by the shared file overlay.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AuthSection {
    /// Hydra admin API base URL.
    pub hydra_admin_url: Option<String>,
    /// Hydra public issuer/base URL.
    pub hydra_public_url: Option<String>,
    /// First-party OAuth client IDs trusted by the platform.
    #[serde(default)]
    pub trusted_oauth_clients: Vec<String>,
}

/// Observability values that can be supplied by the shared file overlay.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ObsSection {
    /// `RUST_LOG` / `EnvFilter` directive.
    pub rust_log: Option<String>,
    /// Tracing output format.
    pub log_format: Option<String>,
}

impl FileConfig {
    /// Load an optional TOML overlay from `path`.
    ///
    /// Passing `None` returns an all-default configuration. Passing `Some`
    /// reads the file and parses it as TOML.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] when the file cannot be read, or
    /// [`ConfigError::Parse`] when the file is not valid TOML for this shape.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let Some(path) = path else {
            return Ok(Self::default());
        };

        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Validate the generic strength requirement for a stash signing key.
///
/// Binary-specific development default sentinels are checked by the binaries
/// that own those defaults. This helper only enforces empty and length checks.
///
/// # Errors
///
/// Returns an explanatory error when `value` is empty or shorter than 32 bytes,
/// unless `insecure_dev` is enabled.
pub fn validate_stash_key(value: &str, insecure_dev: bool) -> Result<(), String> {
    if insecure_dev {
        return Ok(());
    }

    if value.is_empty() {
        return Err("stash signing key must not be empty".to_owned());
    }

    if value.len() < 32 {
        return Err("stash signing key must be at least 32 bytes".to_owned());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{ConfigError, FileConfig};

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn write(name: &str, contents: &str) -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

            let path = std::env::temp_dir().join(format!(
                "zeroship-core-config-{name}-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(path.as_path(), contents).expect("write temp config");
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.path.as_path());
        }
    }

    #[test]
    fn load_none_returns_defaults() {
        let config = FileConfig::load(None).expect("load default config");

        assert!(config.auth.hydra_admin_url.is_none());
        assert!(config.auth.hydra_public_url.is_none());
        assert!(config.auth.trusted_oauth_clients.is_empty());
        assert!(config.observability.rust_log.is_none());
        assert!(config.observability.log_format.is_none());
    }

    #[test]
    fn load_full_config_populates_fields() {
        let file = TempFile::write(
            "full.toml",
            r#"
[auth]
hydra_admin_url = "http://hydra:4445"
hydra_public_url = "https://auth.zeroship.ai"
trusted_oauth_clients = ["zeroship-builder", "zeroship-console"]

[observability]
rust_log = "info,zeroship_=debug"
log_format = "json"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert_eq!(
            config.auth.hydra_admin_url.as_deref(),
            Some("http://hydra:4445")
        );
        assert_eq!(
            config.auth.hydra_public_url.as_deref(),
            Some("https://auth.zeroship.ai")
        );
        assert_eq!(
            config.auth.trusted_oauth_clients,
            ["zeroship-builder", "zeroship-console"]
        );
        assert_eq!(
            config.observability.rust_log.as_deref(),
            Some("info,zeroship_=debug")
        );
        assert_eq!(config.observability.log_format.as_deref(), Some("json"));
    }

    #[test]
    fn load_auth_only_defaults_observability() {
        let file = TempFile::write(
            "auth-only.toml",
            r#"
[auth]
hydra_admin_url = "http://hydra:4445"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert_eq!(
            config.auth.hydra_admin_url.as_deref(),
            Some("http://hydra:4445")
        );
        assert!(config.observability.rust_log.is_none());
        assert!(config.observability.log_format.is_none());
    }

    #[test]
    fn load_observability_only_defaults_auth() {
        let file = TempFile::write(
            "observability-only.toml",
            r#"
[observability]
rust_log = "debug"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");

        assert!(config.auth.hydra_admin_url.is_none());
        assert!(config.auth.hydra_public_url.is_none());
        assert!(config.auth.trusted_oauth_clients.is_empty());
        assert_eq!(config.observability.rust_log.as_deref(), Some("debug"));
    }

    #[test]
    fn malformed_toml_returns_parse_error() {
        let file = TempFile::write("malformed.toml", "[auth");

        let err = FileConfig::load(Some(&file.path)).expect_err("parse error");

        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(err.to_string().contains(file.path.to_str().expect("utf-8 path")));
    }

    #[test]
    fn nonexistent_path_returns_io_error() {
        let path = std::env::temp_dir().join(format!(
            "zeroship-core-config-missing-{}",
            std::process::id()
        ));

        let err = FileConfig::load(Some(&path)).expect_err("io error");

        assert!(matches!(err, ConfigError::Io { .. }));
        assert!(err.to_string().contains(path.to_str().expect("utf-8 path")));
    }

    #[test]
    fn validate_stash_key_allows_insecure_dev_bypass() {
        super::validate_stash_key("", true).expect("insecure dev bypass");
    }

    #[test]
    fn validate_stash_key_rejects_empty_value() {
        let err = super::validate_stash_key("", false).expect_err("empty key");

        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn validate_stash_key_rejects_short_value() {
        let err = super::validate_stash_key("short", false).expect_err("short key");

        assert!(err.contains("at least 32"));
    }

    #[test]
    fn validate_stash_key_accepts_strong_value() {
        super::validate_stash_key("0123456789abcdef0123456789abcdef", false)
            .expect("strong key");
    }
}
