//! The sealed test-key enum for compio-postgres's gated tests and benches, and
//! the one default for `PG_TEST_URL`.
//!
//! `compio-postgres` is a standalone, publishable driver with NO zeroship
//! dependency, so it cannot use the typed keys in `zeroship_core::config` that
//! the rest of the workspace reads the environment through. Exactly one
//! substitute is permitted: a dependency-free sealed key enum whose variants map
//! to literal names and whose raw access lives in one accessor.
//!
//! The integration tests reach this through `support::env` and the benchmark
//! targets depend on the crate directly, so all of them resolve one server
//! address from one place instead of each target carrying a spelling of its own.

/// Every environment name compio-postgres's targets are permitted to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestEnvKey {
    /// Connection string for the PostgreSQL integration suites. Absent means
    /// [`DEFAULT_TEST_URL`], never "do not run".
    PgTestUrl,
    /// Set on the child in the descriptor-budget probe, which re-executes its
    /// own test binary so the `/proc/self/fd` count is taken in a process that
    /// ran nothing else. Present means "you are the child".
    ///
    /// Prefixed because, unlike `PG_TEST_URL`, it names no shared service and
    /// exists only inside one test's re-exec.
    FdProbeChild,
}

impl TestEnvKey {
    /// The literal environment spelling. Literal arms only, by rule.
    ///
    /// Public because a key can be WRITTEN as well as read: the probe above
    /// passes this to `Command::env` when it spawns its child. Handing out the
    /// name opens no bypass - `std::env::var` is denied at every call site in
    /// the workspace, so the only way to read one back is [`get`] below.
    pub const fn name(self) -> &'static str {
        match self {
            Self::PgTestUrl => "PG_TEST_URL",
            Self::FdProbeChild => "CPG_FD_PROBE_CHILD",
        }
    }
}

/// The one raw environment read for compio-postgres's targets.
///
/// It takes the sealed key, never a `&str`, so no caller can name a variable
/// this crate has not declared. Non-Unicode reads as absent.
#[expect(
    clippy::disallowed_methods,
    reason = "the one raw environment read compio-postgres's test and bench targets are permitted"
)]
pub fn get(key: TestEnvKey) -> Option<String> {
    std::env::var(key.name()).ok()
}

/// The server used when [`TestEnvKey::PgTestUrl`] is unset: the one
/// `tests/provision_test_backends.sh` provisions with its default `PG_HOST`,
/// `PG_PORT`, `PG_USER`, `PG_PASS` and `PG_DB`, so a developer who runs that
/// script exports nothing.
///
/// ONE DEFINITION, beside the key it defaults. The test targets reach it
/// through `support::test_url` and `support::plaintext_url`, and the benchmark
/// targets through this crate, so no target carries a second spelling of the
/// address.
pub const DEFAULT_TEST_URL: &str = "postgres://postgres:zeroship@127.0.0.1:5440/zeroship";
