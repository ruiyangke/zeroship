//! The test suites' view of the shared TOML overlay.
//!
//! WHAT THIS REPLACES. Test code named the one test PostgreSQL under eight
//! different environment variables - `AUTH_DB_URL`, `CONTROL_TEST_DB`,
//! `GATEWAY_ANCHORS_DB_URL`, `GATEWAY_POOL_SMOKE_URL`, `LIVE_DB_TEST_URL`,
//! `MIGRATED_TEST_DB`, `ZERO_MIGRATE_TEST_PG_URL`, `ZEROSHIP_SCHEDULER_TEST_DB`
//! - each read by one crate's tests and exported by whichever suite happened to
//! remember it. Nothing related them, so a name that was never exported meant
//! the tests behind it did not run, and a name pointed at the wrong server meant
//! they ran against it silently. Both happened.
//!
//! The services already solved this. They take a TOML overlay parsed by
//! [`FileConfig`] with `deny_unknown_fields`, resolved explicit-path-first by
//! [`FileConfig::resolve`]. This module points test code at the same file
//! through the same parser, so the test topology is one document with one
//! schema and a typo in it is an error rather than a silence.
//!
//! WHY THE FILE IS GENERATED AND GITIGNORED, which is the one place this
//! diverges from the obvious shape. Every DSN leaf in the schema is
//! `secret`-classed - `auth.database_url`, `control.database_url`,
//! `gateway.database_url`, `migrated.database_url`,
//! `migrated.provision_database_url`, `worker.database_url`, `worker.kv_url`,
//! `workflow_scheduler.database_url`, verified against the compiled contract
//! dump. Check 8 of `tests/config_name_alignment_gate.sh` fails any TRACKED
//! `*.toml` holding a literal at a secret-classed leaf, and it says in terms
//! that it will never carry an exception list. A committed
//! `control.database_url = "postgres://postgres:zeroship@..."` is therefore
//! rejected, and was: planting exactly that line produced
//!
//!   deploy/ops/zeroship.test.toml:2 control.database_url is secret-classed
//!       and holds a plaintext literal
//!
//! The same gate exempts UNTRACKED overlays deliberately, because in a real
//! deployment the overlay may itself be a mounted secret. The test overlay is
//! the same thing: `tests/provision_test_backends.sh` writes it next to the
//! backends it stands up, so the coordinates have one definition, the file has
//! the real schema, and no credential enters git.
//!
//! PRECEDENCE is the services' own, and the surviving environment names are the
//! overlay tier rather than a parallel system: an explicit `PG_TEST_URL` /
//! `REDIS_TEST_URL` wins, else the generated file, and there is no third tier -
//! a missing file is a hard failure naming the provisioning command rather than
//! a compiled default that would let a suite pass against nothing.

use std::path::{Path, PathBuf};

use super::file::FileConfig;

/// Repository-relative location of the generated test overlay.
pub const TEST_OVERLAY_PATH: &str = "deploy/ops/zeroship.test.toml";

/// The command that writes it. Named in every failure this module can raise.
pub const PROVISION_COMMAND: &str = "tests/provision_test_backends.sh";

/// Absolute path to the generated test overlay.
///
/// Derived from this crate's own `CARGO_MANIFEST_DIR` rather than the current
/// directory, because `cargo test` runs each target with the CWD of the crate
/// under test and there are twenty of those. `crates/core` is always two
/// levels below the workspace root.
#[must_use]
pub fn overlay_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(TEST_OVERLAY_PATH)
}

/// Load the generated test overlay through the real parser.
///
/// # Panics
///
/// When the file is absent or does not parse. Both are fatal on purpose: this
/// is the same decision `tests/provision_test_backends.sh` made for the
/// backends themselves when `ZEROSHIP_REQUIRE_LIVE_BACKENDS` was deleted. A
/// test suite that silently falls back to a compiled default when its
/// configuration is missing is a suite that reports passes for work it did not
/// do.
#[must_use]
pub fn load() -> FileConfig {
    let path = overlay_path();
    match FileConfig::resolve(Some(path.as_path()), false) {
        Ok(overlay) => overlay.config,
        Err(error) => panic!(
            "test overlay {} could not be loaded: {error}\n\
             Provision the test backends and their configuration with:\n    {PROVISION_COMMAND}",
            path.display()
        ),
    }
}

/// The one PostgreSQL every test in this workspace dials.
///
/// `PG_TEST_URL` overrides it. That is how a suite hands its per-run scratch
/// database name down (`tests/lib/scratch_db.sh`), and how two concurrent runs
/// stay disjoint - a per-run value cannot live in a tracked or generated file
/// shared by both.
///
/// # Panics
///
/// When neither `PG_TEST_URL` nor the overlay supplies one.
#[must_use]
pub fn database_url() -> String {
    if let Some(url) = crate::test_env!("PG_TEST_URL").filter(|url| !url.is_empty()) {
        return url;
    }
    load().control.database_url.unwrap_or_else(|| {
        panic!(
            "neither PG_TEST_URL nor control.database_url in {} names a PostgreSQL.\n\
             Provision the test backends and their configuration with:\n    {PROVISION_COMMAND}",
            overlay_path().display()
        )
    })
}

/// The one Redis every test in this workspace dials.
///
/// `REDIS_TEST_URL` overrides it, for the same reason `PG_TEST_URL` overrides
/// the DSN.
///
/// # Panics
///
/// When neither `REDIS_TEST_URL` nor the overlay supplies one.
#[must_use]
pub fn kv_url() -> String {
    if let Some(url) = crate::test_env!("REDIS_TEST_URL").filter(|url| !url.is_empty()) {
        return url;
    }
    load().worker.kv_url.unwrap_or_else(|| {
        panic!(
            "neither REDIS_TEST_URL nor worker.kv_url in {} names a Redis.\n\
             Provision the test backends and their configuration with:\n    {PROVISION_COMMAND}",
            overlay_path().display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{overlay_path, TEST_OVERLAY_PATH};
    use crate::config::file::FileConfig;

    #[test]
    fn overlay_path_points_at_the_workspace_root() {
        let path = overlay_path();
        let text = path.to_string_lossy().replace('\\', "/");
        assert!(
            text.ends_with(TEST_OVERLAY_PATH),
            "overlay path {text} does not end with {TEST_OVERLAY_PATH}"
        );
        assert!(
            !text.contains("crates/core/deploy"),
            "overlay path {text} was resolved relative to this crate, not the workspace root"
        );
    }

    /// The contract bites: an unknown key is an ERROR, not a silence.
    ///
    /// This is the property the whole move is for. A test topology in a file
    /// nothing validates is the environment-variable sprawl again with fewer
    /// files - a misspelled `databse_url` would configure nothing and say
    /// nothing, which is exactly how `gateway.broker_secret` sat in the schema
    /// for weeks while the gateway declared `broker_secret_file`.
    ///
    /// Both directions are asserted, because only the pair discriminates:
    /// without the accepted arm a parser that rejected EVERYTHING would pass
    /// this test.
    #[test]
    fn an_unknown_overlay_key_is_rejected_and_the_correct_one_is_not() {
        let dir = std::env::temp_dir();
        let stamp = std::process::id();

        let good_path = dir.join(format!("zeroship-test-overlay-good-{stamp}.toml"));
        std::fs::write(
            good_path.as_path(),
            "[control]\ndatabase_url = \"postgres://u@h:5440/d\"\n",
        )
        .expect("write good overlay");
        let good = FileConfig::load(Some(good_path.as_path())).expect("the real key parses");
        assert_eq!(
            good.control.database_url.as_deref(),
            Some("postgres://u@h:5440/d")
        );
        let _ = std::fs::remove_file(good_path.as_path());

        let typo_path = dir.join(format!("zeroship-test-overlay-typo-{stamp}.toml"));
        std::fs::write(
            typo_path.as_path(),
            "[control]\ndatabse_url = \"postgres://u@h:5440/d\"\n",
        )
        .expect("write typo overlay");
        let error =
            FileConfig::load(Some(typo_path.as_path())).expect_err("a misspelled key is rejected");
        assert!(
            error.to_string().contains("databse_url"),
            "the error must name the offending key; got {error}"
        );
        let _ = std::fs::remove_file(typo_path.as_path());
    }
}
