//! The test suites' view of the shared TOML overlay.
//!
//! WHAT THIS REPLACES. Test code named the one test PostgreSQL under eight
//! different environment variables - `AUTH_DB_URL`, `CONTROL_TEST_DB`,
//! `GATEWAY_ANCHORS_DB_URL`, `GATEWAY_POOL_SMOKE_URL`, `LIVE_DB_TEST_URL`,
//! `MIGRATED_TEST_DB`, `ZERO_MIGRATE_TEST_PG_URL`, `ZEROSHIP_SCHEDULER_TEST_DB` -
//! each read by one crate's tests and exported by whichever suite happened to
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
//! WHY THE FILE IS GENERATED AND GITIGNORED. It contains the live credentials
//! for the backend instance `tests/provision_test_backends.sh` owns. The
//! provisioner writes it next to the backends it starts, so their coordinates
//! have one definition, the file has the real schema, and no credential enters
//! git.
//!
//! PRECEDENCE is the services' own, and the surviving environment names are the
//! overlay tier rather than a parallel system: an explicit `PG_TEST_URL` wins, else the generated file, and there is no third tier -
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

/// Load the overlay if it has been generated, else `None`.
///
/// The `_opt` accessors below use this so a checkout that has never run the
/// provisioner keeps whatever each call site already did about an absent
/// database, instead of every one of them turning into a panic in the same
/// commit that renamed them.
fn load_opt() -> Option<FileConfig> {
    let path = overlay_path();
    if !path.exists() {
        return None;
    }
    // A file that EXISTS and does not parse is fatal. That is the whole value
    // of putting the test topology under `deny_unknown_fields`: a misspelled
    // key must not read as "no configuration".
    match FileConfig::resolve(Some(path.as_path()), false) {
        Ok(overlay) => Some(overlay.config),
        Err(error) => panic!(
            "test overlay {} exists but does not parse: {error}\n\
             It is generated - fix {PROVISION_COMMAND} rather than the file.",
            path.display()
        ),
    }
}

/// The one PostgreSQL every test in this workspace dials, or `None`.
///
/// THIS IS THE DROP-IN for the eight `test_env!("...")` chains it replaced, and
/// it returns an `Option` for exactly that reason: those call sites decide for
/// themselves what an absent database means. Collapsing eight names into one is
/// a naming change; deciding on their behalf what happens when there is no
/// database is not, and belongs to whoever owns each test.
///
/// WHAT THE CALLERS ACTUALLY DECIDED, and why the answer is no longer "some of
/// them substitute a default". This doc used to say that most announce a skip,
/// a few panic, and `crates/zeroship-data-v8/tests/distributed_live.rs` substitutes a
/// default. The census on 2026-08-21 found 26 substituting a default, not one:
/// eighteen in `crates/control` alone, all naming `zeroship_billing_test`
/// regardless of what the file was about, and four in `crates/plugin-db`
/// naming a DIFFERENT SERVER (`localhost:5434`, password `test`). Every one of
/// them is gone. A caller that cannot proceed without a database now takes
/// [`database_url`] (which panics naming the provisioner) or
/// `zeroship_testkit::live_db::require_configured` (which refuses the whole run
/// and is what a multi-module target wants); an `_opt` caller that still wants
/// to skip still skips.
///
/// The 12 remaining are all in `libs/`, which by the `libs/` boundary cannot
/// see this module at all - they read `PG_TEST_URL` and nothing else, and their
/// compiled default is the shared `zeroship` database. That is how 84 `cpg_*`
/// schemas came to be sitting in it.
///
/// `PG_TEST_URL` wins over the overlay. That is how a suite hands its per-run
/// scratch database name down (`tests/lib/scratch_db.sh`) and how two
/// concurrent runs stay disjoint - a per-run value cannot live in a file both
/// of them read.
/// THE RESOLUTION ANNOUNCES ITSELF, and that is the whole of what the printing
/// below is for. `env -u PG_TEST_URL cargo test ...` LOOKS like a no-database
/// arm and is not: it falls through to the overlay, which points at the auth
/// suite's server, and that suite DROPS A FIXED-NAME DATABASE. A run that
/// believes it is testing "no database" is instead pointed at a server another
/// suite is actively destroying, and nothing said so.
///
/// MEASURED, so nobody re-derives it: the empty-string arm is already taken.
/// `.filter(|url| !url.is_empty())` runs BEFORE the `or_else`, so
/// `PG_TEST_URL=""` behaves exactly like unset. THERE IS NO VALUE OF
/// `PG_TEST_URL` THAT MEANS "NO DATABASE".
///
/// The fallback is NOT the bug and must not be deleted - see the paragraph
/// above it. Making the choice audible is the fix a trap deserves: a trap that
/// announces itself is not a trap.
///
/// ONCE PER PROCESS, not per call. This is called from test setup paths that
/// run per test; a per-call print would bury the signal it exists to give.
///
/// THE DSN IS NEVER PRINTED. It carries a password, so the announcement has the
/// SOURCE and a redacted authority: enough to see which server answered, never
/// the credential.
#[must_use]
pub fn database_url_opt() -> Option<String> {
    let from_env = crate::test_env!("PG_TEST_URL").filter(|url| !url.is_empty());
    let source = if from_env.is_some() {
        "PG_TEST_URL"
    } else {
        TEST_OVERLAY_PATH
    };
    let resolved = from_env
        .or_else(|| load_opt().and_then(|config| config.control.database_url))
        .filter(|url| !url.is_empty());

    static ANNOUNCE: std::sync::Once = std::sync::Once::new();
    ANNOUNCE.call_once(|| match resolved.as_deref() {
        Some(url) => eprintln!("test PG DSN resolved from {source} -> {}", redact_dsn(url)),
        None => eprintln!(
            "test PG DSN: none configured (PG_TEST_URL unset or empty, and no {TEST_OVERLAY_PATH})"
        ),
    });

    resolved
}

/// Everything after the credential, or the whole authority when there is none.
///
/// Deliberately crude: this exists to make a wrong SERVER visible, not to parse
/// URLs. A string with no `@` and no `//` is returned unchanged, because a value
/// this function cannot understand is one the reader should see verbatim rather
/// than have silently emptied.
fn redact_dsn(url: &str) -> &str {
    if let Some((_, after_credential)) = url.rsplit_once('@') {
        return after_credential;
    }
    if let Some((_, after_scheme)) = url.split_once("//") {
        return after_scheme;
    }
    url
}

/// [`database_url_opt`], for a caller that cannot proceed without one.
///
/// # Panics
///
/// When neither `PG_TEST_URL` nor the overlay supplies one, naming the
/// provisioning command.
#[must_use]
pub fn database_url() -> String {
    database_url_opt().unwrap_or_else(|| {
        panic!(
            "no PostgreSQL: neither PG_TEST_URL nor control.database_url in {}.\n\
             Provision the test backends and their configuration with:\n    {PROVISION_COMMAND}",
            overlay_path().display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{overlay_path, redact_dsn, TEST_OVERLAY_PATH};
    use crate::config::file::FileConfig;

    /// The announcement must never carry the credential.
    ///
    /// This is the arm that matters: every DSN leaf in the schema is
    /// `secret`-classed because it holds a password, so a resolution notice that
    /// printed the raw URL would put one on every test run's stderr and into
    /// every CI log. The assertions below are written as "the password is
    /// ABSENT" rather than "the output equals X", because an equality check
    /// passes for the wrong reason the day the format changes.
    #[test]
    fn the_resolution_notice_shows_the_server_and_never_the_password() {
        let with_credential = "postgres://postgres:hunter2@127.0.0.1:5440/zeroship";
        let shown = redact_dsn(with_credential);
        assert!(
            !shown.contains("hunter2"),
            "the password reached the notice: {shown}"
        );
        assert!(
            !shown.contains("postgres:"),
            "the credential pair reached the notice: {shown}"
        );
        assert_eq!(
            shown, "127.0.0.1:5440/zeroship",
            "the server must stay visible - seeing WHICH host answered is the \
             entire point of the notice"
        );

        // No credential at all: the authority still has to survive, or a run
        // against the wrong host would be announced as nothing.
        assert_eq!(
            redact_dsn("postgres://127.0.0.1:5490/dbbind_corpus_18"),
            "127.0.0.1:5490/dbbind_corpus_18"
        );

        // Something this function cannot parse is returned VERBATIM rather than
        // silently emptied. A notice that prints "" for an unrecognised value is
        // worse than no notice, because it reads as "nothing configured".
        assert_eq!(redact_dsn("not-a-url"), "not-a-url");
    }

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
