//! Shared optional file-overlay configuration.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use tracing_subscriber::EnvFilter;

/// Development-only stash signing key used by web binaries when insecure dev is explicit.
pub const DEV_STASH_SIGNING_KEY: &str = "dev-stash-key-please-rotate";

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
    #[serde(rename = "rust_log")]
    pub log_filter: Option<String>,
    /// Tracing output format.
    pub log_format: Option<String>,
}

/// CLI/environment observability overrides shared by server binaries.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ObservabilityFlags {
    /// `RUST_LOG` / `EnvFilter` directive.
    #[arg(long = "log-filter", env = "RUST_LOG")]
    pub log_filter: Option<String>,
    /// Tracing output format.
    #[arg(long = "log-format", env = "ZEROSHIP_LOG_FORMAT")]
    pub log_format: Option<String>,
}

/// Resolve observability settings with flag/env values overriding the file.
#[must_use]
pub fn resolve_observability(
    flags: &ObservabilityFlags,
    file: &ObsSection,
    default_filter: &str,
) -> (String, Option<String>) {
    (
        resolve_log_filter(
            flags.log_filter.clone().or(file.log_filter.clone()),
            default_filter,
        ),
        flags.log_format.clone().or(file.log_format.clone()),
    )
}

/// Resolve an optional string override with precedence CLI/env > file > fallback.
///
/// Passing `None` for `fallback` resolves to an empty string when neither the
/// CLI/environment nor file overlay supplied a value.
#[must_use]
pub fn resolve_overlay_string(
    cli: Option<String>,
    file: Option<String>,
    fallback: Option<&str>,
) -> String {
    cli.or(file)
        .or_else(|| fallback.map(str::to_owned))
        .unwrap_or_default()
}

/// Return true when environment variable `key` is exactly `expected`.
///
/// Web binaries use a `SetTrue` flag plus skipped env field so `--dev-insecure`
/// and `--trust-proxy` preserve exact `"1"` environment truthiness. Bootstrap
/// flags intentionally use [`env_is_truthy`] to also accept `"true"`, while the
/// auth binary uses clap's normal boolean env parsing.
#[must_use]
pub fn env_is_exact(key: &str, expected: &str) -> bool {
    std::env::var(key).is_ok_and(|value| value == expected)
}

/// Return true when environment variable `key` is `"1"` or case-insensitive `"true"`.
///
/// This intentionally differs from [`env_is_exact`] for bootstrap-style flags
/// that accept both common truthy spellings.
#[must_use]
pub fn env_is_truthy(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Require `value` to be non-empty unless insecure development mode is explicit.
///
/// # Errors
///
/// Returns a startup-facing message naming `label` when the value is missing
/// outside `--dev-insecure`.
pub fn require_unless_dev(label: &str, value: &str, insecure_dev: bool) -> Result<(), String> {
    if insecure_dev || !value.is_empty() {
        Ok(())
    } else {
        Err(format!("{label} is required outside --dev-insecure"))
    }
}

/// Resolve a tracing filter candidate, warning and falling back to the default on parse error.
#[must_use]
pub fn resolve_log_filter(candidate: Option<String>, default_filter: &str) -> String {
    let Some(candidate) = candidate else {
        return default_filter.to_string();
    };

    match EnvFilter::try_new(candidate.as_str()) {
        Ok(_) => candidate,
        Err(err) => {
            eprintln!(
                "invalid tracing filter {candidate:?}: {err}; falling back to default filter {default_filter:?}"
            );
            default_filter.to_string()
        }
    }
}

/// Load an optional file overlay or exit the current process with a uniform startup error.
pub fn load_overlay_or_exit(path: Option<&Path>, binary: &str) -> FileConfig {
    match FileConfig::load(path) {
        Ok(file) => file,
        Err(err) => {
            let message = format!("{binary}: failed to load config file: {err}");
            tracing::error!("{message}");
            eprintln!("{message}");
            std::process::exit(1);
        }
    }
}

impl FileConfig {
    /// Load an optional TOML overlay from `path`.
    ///
    /// Passing `None` returns an all-default configuration. Passing `Some`
    /// reads the file and parses it as TOML. Absent a path the overlay is
    /// empty; there is no well-known-path auto-discovery.
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

/// Validate the shared production requirement for a stash signing key.
///
/// # Errors
///
/// Returns an explanatory error when `value` is the development sentinel, empty,
/// or shorter than 32 bytes, unless `insecure_dev` is enabled.
pub fn validate_stash_key(value: &str, insecure_dev: bool) -> Result<(), String> {
    if insecure_dev {
        return Ok(());
    }

    if value == DEV_STASH_SIGNING_KEY {
        return Err(
            "STASH_SIGNING_KEY is the dev default; refusing to boot without --dev-insecure"
                .to_owned(),
        );
    }

    if value.is_empty() {
        return Err(
            "STASH_SIGNING_KEY is required outside --dev-insecure; set a strong (>=32 byte) value"
                .to_owned(),
        );
    }

    if value.len() < 32 {
        return Err(format!(
            "STASH_SIGNING_KEY is too short ({} bytes); minimum 32 bytes",
            value.len()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{ConfigError, FileConfig, ObsSection, ObservabilityFlags, resolve_observability};

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
            config.auth.trusted_oauth_clients,
            ["zeroship-builder", "zeroship-console"]
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
        assert!(config.auth.trusted_oauth_clients.is_empty());
        assert_eq!(config.observability.log_filter.as_deref(), Some("debug"));
    }

    #[test]
    fn resolve_observability_prefers_flag_then_file_then_default_filter() {
        let file = ObsSection {
            log_filter: Some("info,zeroship_file=debug".to_string()),
            log_format: None,
        };

        let flags = ObservabilityFlags {
            log_filter: Some("warn,zeroship_flag=trace".to_string()),
            log_format: None,
        };
        let (filter, _) = resolve_observability(&flags, &file, "info,zeroship_default=debug");
        assert_eq!(filter, "warn,zeroship_flag=trace");

        let flags = ObservabilityFlags::default();
        let (filter, _) = resolve_observability(&flags, &file, "info,zeroship_default=debug");
        assert_eq!(filter, "info,zeroship_file=debug");

        let file = ObsSection::default();
        let (filter, _) = resolve_observability(&flags, &file, "info,zeroship_default=debug");
        assert_eq!(filter, "info,zeroship_default=debug");
    }

    #[test]
    fn resolve_observability_prefers_flag_then_file_then_none_format() {
        let file = ObsSection {
            log_filter: None,
            log_format: Some("json".to_string()),
        };

        let flags = ObservabilityFlags {
            log_filter: None,
            log_format: Some("compact".to_string()),
        };
        let (_, format) = resolve_observability(&flags, &file, "info");
        assert_eq!(format.as_deref(), Some("compact"));

        let flags = ObservabilityFlags::default();
        let (_, format) = resolve_observability(&flags, &file, "info");
        assert_eq!(format.as_deref(), Some("json"));

        let file = ObsSection::default();
        let (_, format) = resolve_observability(&flags, &file, "info");
        assert!(format.is_none());
    }

    #[test]
    fn resolve_observability_falls_back_to_default_for_malformed_filter() {
        let file = ObsSection {
            log_filter: Some("info,zeroship_file=debug".to_string()),
            log_format: None,
        };
        let flags = ObservabilityFlags {
            log_filter: Some("zeroship_core=definitely-not-a-level".to_string()),
            log_format: None,
        };

        let (filter, _) = resolve_observability(&flags, &file, "info,zeroship_default=debug");
        assert_eq!(filter, "info,zeroship_default=debug");

        let file = ObsSection {
            log_filter: Some("zeroship_core=definitely-not-a-level".to_string()),
            log_format: None,
        };
        let flags = ObservabilityFlags::default();

        let (filter, _) = resolve_observability(&flags, &file, "info,zeroship_default=debug");
        assert_eq!(filter, "info,zeroship_default=debug");
    }

    #[test]
    fn resolve_overlay_string_prefers_cli_then_file_then_fallback() {
        assert_eq!(
            super::resolve_overlay_string(
                Some("cli".to_string()),
                Some("file".to_string()),
                Some("fallback")
            ),
            "cli"
        );
        assert_eq!(
            super::resolve_overlay_string(None, Some("file".to_string()), Some("fallback")),
            "file"
        );
        assert_eq!(
            super::resolve_overlay_string(None, None, Some("fallback")),
            "fallback"
        );
        assert_eq!(super::resolve_overlay_string(None, None, None), "");
    }

    #[test]
    fn env_is_exact_matches_only_expected_value() {
        let key = format!("ZEROSHIP_TEST_ENV_EXACT_{}", std::process::id());

        std::env::remove_var(&key);
        assert!(!super::env_is_exact(&key, "1"));

        std::env::set_var(&key, "true");
        assert!(!super::env_is_exact(&key, "1"));

        std::env::set_var(&key, "1");
        assert!(super::env_is_exact(&key, "1"));

        std::env::remove_var(&key);
    }

    #[test]
    fn env_is_truthy_accepts_one_and_true() {
        let key = format!("ZEROSHIP_TEST_ENV_TRUTHY_{}", std::process::id());

        std::env::remove_var(&key);
        assert!(!super::env_is_truthy(&key));

        std::env::set_var(&key, "1");
        assert!(super::env_is_truthy(&key));

        std::env::set_var(&key, "true");
        assert!(super::env_is_truthy(&key));

        std::env::set_var(&key, "TRUE");
        assert!(super::env_is_truthy(&key));

        std::env::set_var(&key, "yes");
        assert!(!super::env_is_truthy(&key));

        std::env::remove_var(&key);
    }

    #[test]
    fn require_unless_dev_rejects_missing_only_outside_dev() {
        let err = super::require_unless_dev("CONTROL_KEY / --control-key", "", false)
            .expect_err("missing secret");
        assert_eq!(
            err,
            "CONTROL_KEY / --control-key is required outside --dev-insecure"
        );

        super::require_unless_dev("CONTROL_KEY / --control-key", "", true)
            .expect("dev bypass");
        super::require_unless_dev("CONTROL_KEY / --control-key", "secret", false)
            .expect("present");
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
    fn validate_stash_key_rejects_dev_default() {
        let err = super::validate_stash_key(super::DEV_STASH_SIGNING_KEY, false)
            .expect_err("dev default");

        assert!(err.contains("dev default"));
    }

    #[test]
    fn validate_stash_key_rejects_empty_value() {
        let err = super::validate_stash_key("", false).expect_err("empty key");

        assert!(err.contains("required"));
    }

    #[test]
    fn validate_stash_key_rejects_short_value() {
        let err = super::validate_stash_key("short", false).expect_err("short key");

        assert!(err.contains("too short"));
    }

    #[test]
    fn validate_stash_key_accepts_strong_value() {
        super::validate_stash_key("0123456789abcdef0123456789abcdef", false)
            .expect("strong key");
    }
}
