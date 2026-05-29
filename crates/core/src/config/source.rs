//! Overlay discovery: where a `FileConfig` came from and how it was resolved.

use std::fmt;
use std::path::{Path, PathBuf};

use super::file::{ConfigError, FileConfig};

/// Fixed well-known path probed when no explicit `--config` / `ZEROSHIP_CONFIG` is given.
pub const SYSTEM_CONFIG_PATH: &str = "/etc/zeroship/zeroship.toml";

/// Where the resolved overlay came from. Replaces the old
/// `ResolvedConfig { source: Option<PathBuf>, discovered: bool }`, which could
/// represent the impossible `{None, true}` state (M4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// No overlay applied; all-default configuration.
    None,
    /// Overlay loaded from an explicitly requested `--config` / `ZEROSHIP_CONFIG` path.
    Explicit(PathBuf),
    /// Overlay loaded from the well-known system path via auto-discovery.
    Discovered(PathBuf),
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("(none)"),
            Self::Explicit(path) => write!(f, "{}", path.display()),
            Self::Discovered(path) => write!(f, "{} (auto-discovered)", path.display()),
        }
    }
}

/// A loaded overlay paired with the source it came from.
#[derive(Debug, Clone)]
pub struct LoadedOverlay {
    /// Parsed overlay (all-default when `source` is [`ConfigSource::None`]).
    pub config: FileConfig,
    /// Where the overlay was resolved from.
    pub source: ConfigSource,
}

impl LoadedOverlay {
    fn defaults() -> Self {
        Self {
            config: FileConfig::default(),
            source: ConfigSource::None,
        }
    }
}

impl FileConfig {
    /// Resolve the overlay: explicit path if given, else (when `allow_discovery`)
    /// probe the system well-known path.
    ///
    /// # Errors
    ///
    /// Propagates [`ConfigError`] for an explicit path that fails, OR for a
    /// *present* well-known file that fails to read/parse. A *missing*
    /// well-known path is not an error, and a `try_exists` failure on the
    /// never-requested well-known path is downgraded to a warning + defaults.
    pub fn resolve(
        explicit: Option<&Path>,
        allow_discovery: bool,
    ) -> Result<LoadedOverlay, ConfigError> {
        Self::resolve_with_well_known(explicit, allow_discovery, Path::new(SYSTEM_CONFIG_PATH))
    }

    fn resolve_with_well_known(
        explicit: Option<&Path>,
        allow_discovery: bool,
        well_known: &Path,
    ) -> Result<LoadedOverlay, ConfigError> {
        if let Some(path) = explicit {
            // Explicit path: missing/broken is fatal.
            return Ok(LoadedOverlay {
                config: Self::load(Some(path))?,
                source: ConfigSource::Explicit(path.to_path_buf()),
            });
        }

        if !allow_discovery {
            // The --no-config path: compiled defaults even if the well-known file exists.
            return Ok(LoadedOverlay::defaults());
        }

        match well_known.try_exists() {
            Ok(true) => Ok(LoadedOverlay {
                config: Self::load(Some(well_known))?,
                source: ConfigSource::Discovered(well_known.to_path_buf()),
            }),
            Ok(false) => Ok(LoadedOverlay::defaults()),
            // S5: never fatal for a path nobody requested. A flaky/locked-down
            // /etc must not DoS the whole fleet — warn and fall back to defaults.
            Err(source) => {
                tracing::warn!(
                    path = %well_known.display(),
                    error = %source,
                    "config: could not probe well-known path; continuing with defaults"
                );
                Ok(LoadedOverlay::defaults())
            }
        }
    }
}

/// Load an optional file overlay, returning a [`Result`].
///
/// This is the library-level loader: it never calls `process::exit`. Binaries
/// wanting the exit-on-error behaviour use `bootstrap::bootstrap_or_exit`.
///
/// # Errors
///
/// Propagates any [`ConfigError`] from [`FileConfig::resolve`].
pub fn load_overlay(
    explicit: Option<&Path>,
    allow_discovery: bool,
    _binary: &str,
) -> Result<LoadedOverlay, ConfigError> {
    FileConfig::resolve(explicit, allow_discovery)
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

/// Emit a startup log line naming the resolved overlay source. Call AFTER tracing init.
pub fn log_overlay_source(source: &ConfigSource) {
    match source {
        ConfigSource::Discovered(path) => {
            tracing::info!(path = %path.display(), "config: loaded overlay (auto-discovered)");
        }
        ConfigSource::Explicit(path) => {
            tracing::info!(path = %path.display(), "config: loaded overlay");
        }
        ConfigSource::None => {
            tracing::debug!(default_path = SYSTEM_CONFIG_PATH, "config: no overlay found");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{ConfigSource, FileConfig, SYSTEM_CONFIG_PATH};
    use crate::config::file::ConfigError;

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn write(name: &str, contents: &str) -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

            let path = std::env::temp_dir().join(format!(
                "zeroship-core-config-src-{name}-{}-{}",
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

    fn missing_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zeroship-core-config-src-missing-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn config_source_display_variants() {
        assert_eq!(ConfigSource::None.to_string(), "(none)");
        assert_eq!(
            ConfigSource::Explicit(PathBuf::from("/tmp/explicit.toml")).to_string(),
            "/tmp/explicit.toml"
        );
        assert_eq!(
            ConfigSource::Discovered(PathBuf::from("/etc/zeroship/zeroship.toml")).to_string(),
            "/etc/zeroship/zeroship.toml (auto-discovered)"
        );
    }

    #[test]
    fn system_config_path_is_etc() {
        assert_eq!(SYSTEM_CONFIG_PATH, "/etc/zeroship/zeroship.toml");
    }

    #[test]
    fn resolve_explicit_loads_and_marks_explicit() {
        let file = TempFile::write(
            "resolve-explicit.toml",
            r#"
[auth]
hydra_admin_url = "http://hydra:4445"
"#,
        );

        let overlay =
            FileConfig::resolve_with_well_known(Some(&file.path), true, Path::new("/dev/null"))
                .expect("resolve explicit");

        assert_eq!(
            overlay.source,
            ConfigSource::Explicit(file.path.clone())
        );
        assert_eq!(
            overlay.config.auth.hydra_admin_url.as_deref(),
            Some("http://hydra:4445")
        );
    }

    #[test]
    fn resolve_explicit_missing_is_error() {
        let path = missing_path("explicit");

        let err = FileConfig::resolve_with_well_known(Some(&path), true, Path::new("/dev/null"))
            .expect_err("explicit missing is error");

        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn resolve_discovers_present_well_known() {
        let file = TempFile::write(
            "resolve-well-known.toml",
            r#"
[observability]
rust_log = "info,zeroship_=debug"
"#,
        );

        let overlay = FileConfig::resolve_with_well_known(None, true, &file.path)
            .expect("resolve well-known");

        assert_eq!(overlay.source, ConfigSource::Discovered(file.path.clone()));
        assert_eq!(
            overlay.config.observability.log_filter.as_deref(),
            Some("info,zeroship_=debug")
        );
    }

    #[test]
    fn resolve_missing_well_known_returns_none() {
        let path = missing_path("well-known");

        let overlay =
            FileConfig::resolve_with_well_known(None, true, &path).expect("missing well-known is ok");

        assert_eq!(overlay.source, ConfigSource::None);
        assert!(overlay.config.auth.hydra_admin_url.is_none());
        assert!(overlay.config.observability.log_filter.is_none());
    }

    #[test]
    fn resolve_no_discovery_ignores_present_well_known() {
        // --no-config path: even a present well-known file is ignored.
        let file = TempFile::write(
            "resolve-no-config.toml",
            r#"
[observability]
rust_log = "info,zeroship_=debug"
"#,
        );

        let overlay = FileConfig::resolve_with_well_known(None, false, &file.path)
            .expect("no-config is ok");

        assert_eq!(overlay.source, ConfigSource::None);
        assert!(overlay.config.observability.log_filter.is_none());
    }

    #[test]
    fn resolve_overlay_string_prefers_cli_then_file_then_fallback() {
        use super::resolve_overlay_string;
        assert_eq!(
            resolve_overlay_string(Some("cli".to_string()), Some("file".to_string()), Some("fb")),
            "cli"
        );
        assert_eq!(
            resolve_overlay_string(None, Some("file".to_string()), Some("fb")),
            "file"
        );
        assert_eq!(resolve_overlay_string(None, None, Some("fb")), "fb");
        assert_eq!(resolve_overlay_string(None, None, None), "");
    }

    #[test]
    fn resolve_present_but_malformed_well_known_is_error() {
        let file = TempFile::write("resolve-malformed.toml", "[auth");

        let err = FileConfig::resolve_with_well_known(None, true, &file.path)
            .expect_err("malformed well-known is error");

        assert!(matches!(err, ConfigError::Parse { .. }));
    }
}
