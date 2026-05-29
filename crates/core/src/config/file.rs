//! File-overlay schema: the TOML sections and their load/parse error type.

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

    /// An observability `log_format` value from the file overlay was not a
    /// recognised format.
    #[error("invalid log_format {value:?}; expected one of pretty, compact, json, logfmt, bunyan")]
    InvalidLogFormat {
        /// The offending value as written in the file.
        value: String,
    },
}

/// Optional cross-binary domain configuration loaded from `ops/zeroship.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    /// Hydra admin API base URL.
    pub hydra_admin_url: Option<String>,
    /// Hydra public issuer/base URL.
    pub hydra_public_url: Option<String>,
    /// First-party OAuth client IDs trusted by the platform.
    ///
    /// `None` (key absent) means "use the compiled-in default set"; `Some(vec)`
    /// means exactly that set, where an empty vec is "no trusted clients".
    pub trusted_oauth_clients: Option<Vec<String>>,
}

/// Observability values that can be supplied by the shared file overlay.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ObsSection {
    /// `RUST_LOG` / `EnvFilter` directive.
    #[serde(rename = "rust_log")]
    pub log_filter: Option<String>,
    /// Tracing output format, as a raw string (TOML carries strings; it is
    /// parsed into a `LogFormat` by `resolve_observability`, which errors on
    /// invalid values).
    pub log_format: Option<String>,
}

impl FileConfig {
    /// Load an optional TOML overlay from `path`.
    ///
    /// This is the explicit-only primitive: passing `None` returns an
    /// all-default configuration and does *not* probe any well-known path.
    /// Passing `Some` reads the file and parses it as TOML. Callers that want
    /// system-path auto-discovery use [`FileConfig::resolve`], which is built
    /// on top of this primitive.
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
        assert!(config.auth.trusted_oauth_clients.is_none());
        assert!(config.observability.log_filter.is_none());
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
            config.auth.trusted_oauth_clients.as_deref(),
            Some(["zeroship-builder".to_string(), "zeroship-console".to_string()].as_slice())
        );
        assert_eq!(
            config.observability.log_filter.as_deref(),
            Some("info,zeroship_=debug")
        );
        assert_eq!(config.observability.log_format.as_deref(), Some("json"));
    }

    #[test]
    fn load_ops_zeroship_toml_parses() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ops/zeroship.toml");

        let config = FileConfig::load(Some(&path)).expect("load ops/zeroship.toml");

        assert_eq!(
            config.observability.log_filter.as_deref(),
            Some("info,zeroship_=debug")
        );
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
        assert!(config.observability.log_filter.is_none());
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
        assert!(config.auth.trusted_oauth_clients.is_none());
        assert_eq!(config.observability.log_filter.as_deref(), Some("debug"));
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

    // S7: unknown keys now fail loudly instead of being silently ignored.
    #[test]
    fn deny_unknown_fields_in_auth_section_is_parse_error() {
        let file = TempFile::write(
            "unknown-auth-key.toml",
            r#"
[auth]
hydra_pubic_url = "https://typo.example"
"#,
        );

        let err = FileConfig::load(Some(&file.path)).expect_err("unknown key rejected");

        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    // M4: trusted_oauth_clients distinguishes absent / empty / populated.
    #[test]
    fn trusted_oauth_clients_absent_is_none() {
        let file = TempFile::write(
            "tcl-absent.toml",
            r#"
[auth]
hydra_admin_url = "http://hydra:4445"
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");
        assert!(config.auth.trusted_oauth_clients.is_none());
    }

    #[test]
    fn trusted_oauth_clients_empty_is_some_empty() {
        let file = TempFile::write(
            "tcl-empty.toml",
            r#"
[auth]
trusted_oauth_clients = []
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");
        assert_eq!(
            config.auth.trusted_oauth_clients,
            Some(Vec::<String>::new())
        );
    }

    #[test]
    fn trusted_oauth_clients_populated_is_some_vec() {
        let file = TempFile::write(
            "tcl-populated.toml",
            r#"
[auth]
trusted_oauth_clients = ["a", "b"]
"#,
        );

        let config = FileConfig::load(Some(&file.path)).expect("load config");
        assert_eq!(
            config.auth.trusted_oauth_clients,
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }
}
