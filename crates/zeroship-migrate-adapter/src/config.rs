//! `zeroship-platform-migrate`'s command definition plus its generated controls.
//!
//! This module exists so the platform migrate one-shot is configured by the
//! SAME generator as the six servers. Until it existed, this binary was the one
//! target Cargo metadata classifies `platform` whose configuration nothing could
//! see: `crates/config-contract` links a generated registry per binary, and a
//! binary with no declaration contributes no `ConfigSpec` and no `ReadSite`, so
//! every collision, projection and undeclared-read check simply had one fewer
//! subject and still reported green.
//!
//! It lives in the LIBRARY, not in `src/bin/`, and it is NOT behind
//! `platform-cli`. Both are load-bearing. The contract checker links this crate
//! to reach `PlatformMigrateSettings::SPECS`, and the declaration carries no V8:
//! keeping it out of the feature keeps `zeroship-runtime` off the checker's
//! dependency graph while still putting the one-shot inside the contract.
//!
//! THE DSN IS `Secret<String>`, WHICH IS WHY THERE IS NO `--database-url`.
//! A secret field generates exactly one clap carrier - `--<name>-file PATH` -
//! and the generator has no arm that emits a value flag for it
//! (`crates/config-macros/src/zeroship_config.rs`, `flag_projection` +
//! `source_fields`). The hand-rolled parser this replaced had both spellings,
//! and the value one put a postgres SUPERUSER DSN in the argv of every caller
//! that used it, where `ps`, `docker inspect`, `docker ps --no-trunc` and
//! /proc/<pid>/cmdline publish it to anything sharing the PID namespace. The
//! deletion is now structural rather than policed: restoring `--database-url`
//! would mean declaring the DSN as something other than a secret.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector, Secret,
};
use zeroship_core::observability::LogFormat;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_migrate_adapter=debug";

/// The default project schema, project id and cluster-lock database.
///
/// Named constants rather than literals inside the attribute: the attribute's
/// `default =` expression is also what `--check-config` prints, so a shared name
/// keeps the printed default and the compiled one the same thing.
pub const DEFAULT_PROJECT_SCHEMA: &str = "zeroship";
/// The advisory-lock / journal project id.
pub const DEFAULT_PROJECT_ID: &str = "zeroship";

/// The database every concurrent migrate run coordinates through.
///
/// `postgres` is the maintenance database `initdb` creates and the official
/// container image ships; it is the conventional "connect to the cluster, not to
/// a database" target. Overridable per run because it CAN be dropped on a
/// hardened cluster, in which case the acquisition fails loudly and names the
/// override rather than silently skipping the lock. The lock itself is
/// `platform::cluster_lock`.
///
/// It lives here rather than beside that lock because the lock module is behind
/// `platform-cli` and this default has to be spellable from the declaration
/// below, which is not.
pub const DEFAULT_CLUSTER_LOCK_DATABASE: &str = "postgres";

/// The controls one platform-migrate run resolves before it connects.
#[zeroship_config(binary = "zeroship-platform-migrate", scope = "platform_migrate")]
#[derive(Debug)]
pub struct PlatformMigrateSettings {
    /// Optional shared config overlay path.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known config overlay (compiled defaults only).
    #[config(shared = NO_CONFIG)]
    pub no_config: BootstrapControl<bool>,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret
    /// config, then exit without reading the DSN file, connecting or migrating.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,

    /// Output format for `--check-config`.
    #[config(shared = CHECK_CONFIG_FORMAT, default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,

    /// `EnvFilter` directive for the tracing subscriber.
    #[config(shared = OBSERVABILITY_LOG_FILTER, default = DEFAULT_LOG_FILTER.to_owned())]
    pub log_filter: Operational<String>,

    /// Tracing output format; `auto` picks pretty on a TTY and json otherwise.
    #[config(shared = OBSERVABILITY_LOG_FORMAT, default = LogFormat::Auto)]
    pub log_format: Operational<LogFormat>,

    /// Directory holding the `db/migrations-ts/*.ts` platform migrations.
    #[config(name = "platform_migrate.migrations_dir", default = default_migrations_dir())]
    pub migrations_dir: Operational<PathBuf>,

    /// The primary platform schema.
    #[config(name = "platform_migrate.project_schema", default = DEFAULT_PROJECT_SCHEMA.to_owned())]
    pub project_schema: Operational<String>,

    /// The advisory-lock / journal project id.
    #[config(name = "platform_migrate.project_id", default = DEFAULT_PROJECT_ID.to_owned())]
    pub project_id: Operational<String>,

    /// The database on the SAME cluster that concurrent migrate runs coordinate
    /// through while applying cluster-global objects (roles, databases,
    /// tablespaces).
    ///
    /// A separate knob from the DSN on purpose: a `PostgreSQL` advisory lock is
    /// database-scoped, so two runs migrating two different databases have
    /// nowhere else to exclude each other. Only needs setting on a cluster where
    /// the maintenance database was dropped.
    #[config(
        name = "platform_migrate.cluster_lock_database",
        default = DEFAULT_CLUSTER_LOCK_DATABASE.to_owned()
    )]
    pub cluster_lock_database: Operational<String>,

    // Secrets last within the table, by convention. This one generates ONE
    // `--database-url-file` path flag and no value flag, so it cannot reach a
    // process argument list.
    /// Admin `PostgreSQL` DSN for platform DDL (roles, grants, RLS, functions).
    ///
    /// Secret-classed by grammar: a DSN admits userinfo, so the type cannot
    /// depend on whether a particular deployment's value happens to carry a
    /// password. This deployment's does - it is the cluster superuser.
    #[config(name = "platform_migrate.database_url")]
    pub database_url: Secret<String>,
}

/// The default `db/migrations-ts` directory, resolved relative to the repo root
/// (the crate is two levels below it: `crates/zeroship-migrate-adapter`).
///
/// A compiled default that only makes sense in a source checkout, which is why
/// the deploy compose file passes `--migrations-dir` explicitly against a
/// read-only mount. It stays a default so a developer running the binary from
/// the tree needs one flag fewer, not because a container should rely on it.
#[must_use]
pub fn default_migrations_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.ancestors().nth(2).map_or_else(
        || PathBuf::from("db/migrations-ts"),
        |root| root.join("db").join("migrations-ts"),
    )
}

impl OverlaySelector for PlatformMigrateSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for PlatformMigrateSettings {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }

    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use clap::Parser;
    use zeroship_core::config::GeneratedConfig;

    use super::{PlatformMigrateSettings, PlatformMigrateSettingsSources};

    fn argv(items: &[&str]) -> Vec<String> {
        std::iter::once("zeroship-platform-migrate".to_owned())
            .chain(items.iter().map(|item| (*item).to_string()))
            .collect()
    }

    /// A DSN file at `mode`, which is the ONE variable the pair below differ in.
    fn dsn_file(dir: &std::path::Path, contents: &str, mode: u32) -> String {
        let path = dir.join("migrate-dsn");
        let mut file = std::fs::File::create(&path).expect("create dsn file");
        file.write_all(contents.as_bytes()).expect("write dsn");
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .expect("set dsn file mode");
        path.to_string_lossy().into_owned()
    }

    /// THE DELETION, ASSERTED AT THE PARSER. `--database-url <dsn>` was the
    /// surviving way to put a postgres SUPERUSER DSN in this binary's argv, and
    /// 29 in-repo files spelled it. A flag that is merely unused still works, so
    /// this checks the refusal itself - and that the refusal NAMES the
    /// replacement, because a bare "unexpected argument" would send whoever
    /// trips it looking for a missing feature rather than a renamed flag.
    ///
    /// Does not cover: whether any caller still passes it. That is a text
    /// search, not a compiled assertion.
    #[test]
    fn the_deleted_value_flag_is_refused_and_names_the_file_flag() {
        let error = PlatformMigrateSettingsSources::try_parse_from(argv(&[
            "--database-url",
            "postgres://postgres:zeroship@postgres:5432/zeroship",
        ]))
        .expect_err("the value form must not parse");
        let rendered = error.to_string();
        assert!(
            rendered.contains("--database-url-file"),
            "the refusal must name the replacement; got {rendered}"
        );
    }

    /// The one-variable partner: the SAME DSN behind the `-file` flag resolves.
    /// Without it the test above would also pass on a parser that rejects
    /// everything.
    #[test]
    fn the_dsn_is_read_from_the_file_the_path_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dsn_file(
            dir.path(),
            "postgres://postgres:zeroship@postgres:5432/zeroship\n",
            0o600,
        );
        let sources = PlatformMigrateSettingsSources::try_parse_from(argv(&[
            "--no-config",
            "--database-url-file",
            &path,
        ]))
        .expect("a DSN supplied as a path must parse");
        let settings = PlatformMigrateSettings::resolve_config(sources, None)
            .expect("resolution must succeed");
        assert_eq!(
            settings.database_url.expose_secret().map(String::as_str),
            Some("postgres://postgres:zeroship@postgres:5432/zeroship"),
            "the trailing newline must be trimmed by the shared secret reader"
        );
    }

    /// Same flag, same DSN, same everything except the MODE. The shared reader
    /// enforces owner-only permissions and REFUSES rather than warns, so a
    /// harness that writes its DSN file at 0644 fails here and not at connect
    /// time.
    #[test]
    fn a_world_readable_dsn_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dsn_file(
            dir.path(),
            "postgres://postgres:zeroship@postgres:5432/zeroship\n",
            0o644,
        );
        let sources = PlatformMigrateSettingsSources::try_parse_from(argv(&[
            "--no-config",
            "--database-url-file",
            &path,
        ]))
        .expect("the flag parses; the file is what is rejected");
        let error = PlatformMigrateSettings::resolve_config(sources, None)
            .expect_err("a group-or-other-readable secret file must refuse");
        let rendered = error.to_string();
        assert!(
            rendered.contains("100644") && rendered.contains("chmod 600"),
            "the refusal must state the mode and the fix; got {rendered}"
        );
    }

    /// No DSN at all resolves ABSENT rather than to a compiled default. The
    /// refusal that names the flag is the binary's, because "required" is a
    /// property of the consumer and not of the secret class.
    #[test]
    fn a_missing_dsn_resolves_absent() {
        let sources = PlatformMigrateSettingsSources::try_parse_from(argv(&["--no-config"]))
            .expect("every other field has a default");
        let settings = PlatformMigrateSettings::resolve_config(sources, None)
            .expect("an absent secret is not a resolution error");
        assert!(
            !settings.database_url.is_configured(),
            "no source supplied the DSN, so it must be absent"
        );
    }

    /// A flag with no value must not silently swallow the next flag.
    #[test]
    fn a_dangling_dsn_path_flag_is_refused() {
        PlatformMigrateSettingsSources::try_parse_from(argv(&["--database-url-file"]))
            .expect_err("a dangling flag must refuse");
    }
}
