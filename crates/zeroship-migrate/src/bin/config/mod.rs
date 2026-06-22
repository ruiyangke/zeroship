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

/// The DSN env var (unchanged name). Read HERE through the same empty-is-unset rule
/// as the `ZEROSHIP_MIGRATE_*` vars (MED-1) — NOT via clap's `env=`, which would
/// surface a present-but-empty `DATABASE_URL=` as `Some("")` and defeat the config
/// fallback.
pub const ENV_DATABASE_URL: &str = "DATABASE_URL";

/// Env var names (analogous to the existing `DATABASE_URL`).
pub const ENV_MIGRATIONS_DIR: &str = "ZEROSHIP_MIGRATE_MIGRATIONS_DIR";
pub const ENV_SCHEMA_FILE: &str = "ZEROSHIP_MIGRATE_SCHEMA_FILE";
pub const ENV_ENGINE: &str = "ZEROSHIP_MIGRATE_ENGINE";
pub const ENV_PROFILE: &str = "ZEROSHIP_MIGRATE_PROFILE";
pub const ENV_DUMP_SCHEMA: &str = "ZEROSHIP_MIGRATE_DUMP_SCHEMA";

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
    /// Auto-refresh `schema.sql` after a successful `migrate`/`up`/`rollback`/`down`
    /// (dbmate parity, default ON). `false` disables the auto-dump. The CLI
    /// `--no-dump-schema` flag can only turn it OFF (one-directional override).
    pub dump_schema: Option<bool>,
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

/// A discovered + parsed project config, paired with the absolute-ish path it was
/// loaded FROM (so the bin can echo `using config: <path>` — LOW-3). The path is the
/// `start`-relative join walked up to where the file was found; the bin canonicalizes
/// it for the echo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredConfig {
    /// The path the config was loaded from (the discovered `zeroship-migrate.toml`).
    pub path: PathBuf,
    /// The parsed config.
    pub config: FileConfig,
}

/// Discover + read the project config file, starting at `start` and walking UP to
/// the filesystem root. Returns:
/// - `Ok(None)` if no `zeroship-migrate.toml` exists anywhere up the tree (fine);
/// - `Ok(Some(discovered))` for the first one found (parsed), carrying the PATH it
///   was loaded from (so the bin can echo it — an ancestor-dir config must never be
///   adopted invisibly);
/// - `Err(..)` if a found file cannot be read or is malformed.
///
/// # Errors
/// [`ConfigError::Read`] / [`ConfigError::Parse`] for an unreadable / malformed
/// discovered file.
pub fn discover_file_config(start: &Path) -> Result<Option<DiscoveredConfig>, ConfigError> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            let config = load_file_config(&candidate)?;
            return Ok(Some(DiscoveredConfig {
                path: candidate,
                config,
            }));
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
        // `Display` (`to_string`) carries the line/column span + source snippet;
        // `.message()` strips the location, leaving the operator with no pointer to
        // the offending line in a multi-line config. Keep the span (LOW-2).
        message: e.to_string(),
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
    /// Auto-dump `schema.sql` after a schema-changing command (env wins over file;
    /// `None` ⇒ neither layer set it, so the built-in default ON applies). A
    /// present-but-EMPTY env var is treated as unset; a non-empty env value is
    /// parsed as a bool (`true`/`1`/`yes`/`on` ⇒ true; `false`/`0`/`no`/`off` ⇒
    /// false; anything else falls through to the file value).
    pub dump_schema: Option<bool>,
}

impl FileEnvLayer {
    /// Fold a (possibly absent) file config with the process env into the
    /// file+env layer (env wins over file per key, with a present-but-EMPTY env var
    /// treated as unset). `database_url` is folded the SAME way (MED-1): a non-empty
    /// `DATABASE_URL` env wins over the file; an empty `DATABASE_URL=` falls through
    /// to the file's `database_url`. The bin then lets the `--database-url` flag win
    /// on top of this folded value.
    #[must_use]
    pub fn resolve(file: Option<&FileConfig>) -> Self {
        let f = |get: fn(&FileConfig) -> Option<&str>, env_var: &str| {
            env_over_file(env_var, file.and_then(get))
        };
        Self {
            migrations_dir: f(|c| c.migrations_dir.as_deref(), ENV_MIGRATIONS_DIR),
            schema_file: f(|c| c.schema_file.as_deref(), ENV_SCHEMA_FILE),
            database_url: f(|c| c.database_url.as_deref(), ENV_DATABASE_URL),
            engine: f(|c| c.engine.as_deref(), ENV_ENGINE),
            profile: f(|c| c.profile.as_deref(), ENV_PROFILE),
            dump_schema: resolve_dump_schema(file.and_then(|c| c.dump_schema)),
        }
    }
}

/// Parse a boolean env string (`true`/`1`/`yes`/`on` vs `false`/`0`/`no`/`off`,
/// case-insensitive). `None` for an unrecognised value (so it falls through to the
/// file/default rather than silently flipping the setting).
fn parse_bool_env(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Fold `ZEROSHIP_MIGRATE_DUMP_SCHEMA` (non-empty, parseable) over the file value.
/// A present-but-empty / unparseable env var is treated as unset.
fn resolve_dump_schema(file_value: Option<bool>) -> Option<bool> {
    match std::env::var(ENV_DUMP_SCHEMA) {
        Ok(v) if !v.is_empty() => parse_bool_env(&v).or(file_value),
        _ => file_value,
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
    fn parse_dump_schema_bool_from_config() {
        let cfg: FileConfig = toml::from_str("dump_schema = false\n").expect("parse");
        assert_eq!(cfg.dump_schema, Some(false));
        let cfg: FileConfig = toml::from_str("dump_schema = true\n").expect("parse");
        assert_eq!(cfg.dump_schema, Some(true));
        // Absent ⇒ None (so the bin's ON default applies).
        let cfg: FileConfig = toml::from_str("migrations_dir = \"x\"\n").expect("parse");
        assert_eq!(cfg.dump_schema, None);
    }

    #[test]
    fn parse_bool_env_accepts_the_usual_truthy_falsy_set() {
        for t in ["true", "1", "yes", "on", "TRUE", "On"] {
            assert_eq!(parse_bool_env(t), Some(true), "{t} should be true");
        }
        for f in ["false", "0", "no", "off", "FALSE", "Off"] {
            assert_eq!(parse_bool_env(f), Some(false), "{f} should be false");
        }
        // An unrecognised value falls through (None) rather than silently flipping.
        assert_eq!(parse_bool_env("maybe"), None);
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

    /// LOW-2 RED: the parse error must PRESERVE the TOML span (line/column), not just
    /// the bare message — so an operator with a multi-line config gets pointed at the
    /// offending line. `toml::de::Error`'s `Display` carries the span; `.message()`
    /// strips it. Pre-fix (using `.message()`) this assertion fails.
    #[test]
    fn malformed_toml_error_preserves_line_column_span() {
        let dir = std::env::temp_dir().join(format!("zsmig_cfg_span_{}", uniq()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CONFIG_FILE_NAME);
        // A multi-line config whose error is on line 3, so a line indication is
        // meaningful (not trivially line 1).
        std::fs::write(
            &path,
            "migrations_dir = \"db/m\"\nschema_file = \"db/s.sql\"\nengine = = broken\n",
        )
        .unwrap();
        let err = load_file_config(&path).unwrap_err();
        let s = err.to_string();
        // `toml`'s Display embeds a `line N, column M` (and a TOML source span). The
        // bare `.message()` form does not. Match the location wording robustly.
        let lc = s.to_lowercase();
        assert!(
            lc.contains("line ") && lc.contains("column "),
            "parse error must carry a line/column span, got: {s}"
        );
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
        assert_eq!(found.config.migrations_dir.as_deref(), Some("from_root"));
        // The discovered PATH is the ancestor's config (LOW-3: the bin echoes this).
        assert_eq!(found.path, root.join(CONFIG_FILE_NAME));
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
