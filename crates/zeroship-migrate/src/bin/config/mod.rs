//! DX-parity config resolution for the `zeroship-migrate` CLI: a project config
//! file + env conventions, layered under a single, explicit precedence rule.
//!
//! **Precedence (highest → lowest): CLI flag > env var > config file > built-in
//! default.** This module owns ONLY the file + env layers (the lowest two of the
//! four); the CLI-flag and built-in-default layers live in the bin (`run_config`),
//! which calls [`FileEnvLayer::resolve`] to fold the file + env, then lets an
//! explicit flag win on top. Factored out so the precedence logic is unit-testable
//! in isolation (no clap, no process).
//!
//! Discovery: a `zeroship-migrate.toml` is searched from the CWD walking UP to the
//! filesystem root (so running from a subdirectory of a project still finds the
//! project config — the same ergonomics dbmate/cargo give). The FIRST file found
//! wins; a missing file is fine (file layer is empty). A malformed file is a hard
//! error (never silently ignored).

use std::path::{Path, PathBuf};

/// The config-file name discovered in the project tree.
pub const CONFIG_FILE_NAME: &str = "zeroship-migrate.toml";

/// Env var names (analogous to the existing `DATABASE_URL`). `DATABASE_URL` itself
/// is NOT renamed — it remains the DSN env, read by clap's `env=` on the bin.
pub const ENV_MIGRATIONS_DIR: &str = "ZEROSHIP_MIGRATE_MIGRATIONS_DIR";
pub const ENV_SCHEMA_FILE: &str = "ZEROSHIP_MIGRATE_SCHEMA_FILE";
pub const ENV_ENGINE: &str = "ZEROSHIP_MIGRATE_ENGINE";
pub const ENV_PROFILE: &str = "ZEROSHIP_MIGRATE_PROFILE";

/// The parsed `zeroship-migrate.toml`. Every key is optional; an absent key leaves
/// that field `None` so the next-lower precedence layer supplies it.
///
/// `serde(deny_unknown_fields)` makes a typo'd key a hard parse error rather than a
/// silently-ignored setting — the operator gets told, not surprised.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub migrations_dir: Option<String>,
    pub schema_file: Option<String>,
    pub database_url: Option<String>,
    /// `pg` | `sqlite` (validated when folded into the effective engine).
    pub engine: Option<String>,
    /// `trusted` | `platform` | `confined` (validated when folded into the profile).
    pub profile: Option<String>,
}

/// A failure resolving the file/env layer (printed by the bin; exits non-zero).
#[derive(Debug)]
pub enum ConfigError {
    /// The config file exists but could not be read.
    Read { path: PathBuf, source: std::io::Error },
    /// The config file exists but is not valid TOML / has an unknown key.
    Parse { path: PathBuf, message: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "read config {}: {source}", path.display())
            }
            Self::Parse { path, message } => {
                write!(f, "malformed config {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Discover + read the project config file, starting at `start` and walking UP to
/// the filesystem root. Returns:
/// - `Ok(None)` if no `zeroship-migrate.toml` exists anywhere up the tree (fine);
/// - `Ok(Some(cfg))` for the first one found (parsed);
/// - `Err(..)` if a found file cannot be read or is malformed.
///
/// # Errors
/// [`ConfigError::Read`] / [`ConfigError::Parse`] for an unreadable / malformed
/// discovered file.
pub fn discover_file_config(start: &Path) -> Result<Option<FileConfig>, ConfigError> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return load_file_config(&candidate).map(Some);
        }
        dir = d.parent();
    }
    Ok(None)
}

/// Parse a specific config-file path (used by [`discover_file_config`] and tests).
///
/// # Errors
/// [`ConfigError::Read`] if the file cannot be read; [`ConfigError::Parse`] if it
/// is not valid TOML or carries an unknown key.
pub fn load_file_config(path: &Path) -> Result<FileConfig, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        message: e.message().to_string(),
    })
}

/// The resolved file + env layer for ONE setting: env wins over file (the two
/// lowest-but-one and lowest layers). The bin folds an explicit CLI flag on top
/// (highest), then a built-in default below (lowest) — those two layers stay in the
/// bin so this module is clap-free.
fn env_over_file(env_var: &str, file_value: Option<&str>) -> Option<String> {
    // A *present* env var wins, even if empty? No — an empty env var is treated as
    // unset (a common shell footgun: `FOO=` should not blank a config value).
    match std::env::var(env_var) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => file_value.map(str::to_string),
    }
}

/// The folded file+env values the bin layers CLI flags / defaults on top of. Each
/// field is `Some` iff the env var or the config file supplied it (env winning).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileEnvLayer {
    pub migrations_dir: Option<String>,
    pub schema_file: Option<String>,
    pub database_url: Option<String>,
    pub engine: Option<String>,
    pub profile: Option<String>,
}

impl FileEnvLayer {
    /// Fold a (possibly absent) file config with the process env into the
    /// file+env layer (env wins over file per key). `DATABASE_URL` is read here too
    /// for parity, but the bin's clap `env=` already covers it — the bin treats the
    /// clap value as authoritative and only consults `database_url` from the file.
    #[must_use]
    pub fn resolve(file: Option<&FileConfig>) -> Self {
        let f = |get: fn(&FileConfig) -> Option<&str>, env_var: &str| {
            env_over_file(env_var, file.and_then(get))
        };
        Self {
            migrations_dir: f(|c| c.migrations_dir.as_deref(), ENV_MIGRATIONS_DIR),
            schema_file: f(|c| c.schema_file.as_deref(), ENV_SCHEMA_FILE),
            // database_url's env is DATABASE_URL (unchanged), handled by the bin's
            // clap layer; here we only surface the FILE value for the bin to fall
            // back to when neither --database-url nor DATABASE_URL is set.
            database_url: file.and_then(|c| c.database_url.clone()),
            engine: f(|c| c.engine.as_deref(), ENV_ENGINE),
            profile: f(|c| c.profile.as_deref(), ENV_PROFILE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- file parsing ----

    #[test]
    fn parse_full_config() {
        let toml = r#"
            migrations_dir = "db/m"
            schema_file = "db/s.sql"
            database_url = "sqlite:/tmp/a.db"
            engine = "sqlite"
            profile = "platform"
        "#;
        let cfg: FileConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.migrations_dir.as_deref(), Some("db/m"));
        assert_eq!(cfg.schema_file.as_deref(), Some("db/s.sql"));
        assert_eq!(cfg.database_url.as_deref(), Some("sqlite:/tmp/a.db"));
        assert_eq!(cfg.engine.as_deref(), Some("sqlite"));
        assert_eq!(cfg.profile.as_deref(), Some("platform"));
    }

    #[test]
    fn empty_config_is_all_none() {
        let cfg: FileConfig = toml::from_str("").expect("parse empty");
        assert_eq!(cfg, FileConfig::default());
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = toml::from_str::<FileConfig>("migrations_dirr = \"x\"\n").unwrap_err();
        assert!(
            err.message().contains("unknown field"),
            "deny_unknown_fields must reject a typo'd key: {err}"
        );
    }

    #[test]
    fn malformed_toml_surfaces_as_parse_error() {
        let dir = std::env::temp_dir().join(format!("zsmig_cfg_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CONFIG_FILE_NAME);
        std::fs::write(&path, "not = = valid [[[\n").unwrap();
        let err = load_file_config(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
        assert!(err.to_string().contains("malformed config"));
    }

    // ---- discovery (walks up) ----

    #[test]
    fn discover_walks_up_to_find_config() {
        let root = std::env::temp_dir().join(format!("zsmig_cfg_disc_{}", uniq()));
        let sub = root.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            root.join(CONFIG_FILE_NAME),
            "migrations_dir = \"from_root\"\n",
        )
        .unwrap();
        let found = discover_file_config(&sub).expect("discover").expect("some");
        assert_eq!(found.migrations_dir.as_deref(), Some("from_root"));
    }

    #[test]
    fn discover_missing_is_ok_none() {
        let root = std::env::temp_dir().join(format!("zsmig_cfg_none_{}", uniq()));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(discover_file_config(&root).expect("ok"), None);
    }

    // ---- precedence: env over file ----

    #[test]
    fn env_overrides_file_value() {
        let var = "ZSMIG_TEST_PRECEDENCE_ONE";
        // SAFETY: single-threaded test; set then read then clear.
        std::env::set_var(var, "from_env");
        assert_eq!(env_over_file(var, Some("from_file")), Some("from_env".into()));
        std::env::remove_var(var);
        assert_eq!(env_over_file(var, Some("from_file")), Some("from_file".into()));
        // An empty env var is treated as unset (shell footgun guard).
        std::env::set_var(var, "");
        assert_eq!(env_over_file(var, Some("from_file")), Some("from_file".into()));
        std::env::remove_var(var);
        assert_eq!(env_over_file(var, None), None);
    }

    #[test]
    fn resolve_folds_file_and_env() {
        let file = FileConfig {
            migrations_dir: Some("file_dir".into()),
            engine: Some("sqlite".into()),
            ..FileConfig::default()
        };
        let var = ENV_MIGRATIONS_DIR;
        std::env::set_var(var, "env_dir");
        let layer = FileEnvLayer::resolve(Some(&file));
        std::env::remove_var(var);
        // env wins for migrations_dir; engine falls through from the file.
        assert_eq!(layer.migrations_dir.as_deref(), Some("env_dir"));
        assert_eq!(layer.engine.as_deref(), Some("sqlite"));
        assert_eq!(layer.schema_file, None);
    }

    fn uniq() -> String {
        format!(
            "{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
}
