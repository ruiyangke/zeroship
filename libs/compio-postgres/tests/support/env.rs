//! The sealed test-key enum for compio-postgres's targets.
//!
//! The rest of the workspace reads the environment through the typed keys in
//! `zeroship_core::config`, which this standalone driver cannot depend on.
//! Exactly one substitute is permitted: a dependency-free sealed key enum whose
//! variants map to literal names and whose raw access lives in one accessor.
//!
//! No key here names a server. A suite finds its database through
//! `zeroship_testkit_server::compio_postgres::server`, never through a
//! variable.

/// Every environment name compio-postgres's targets are permitted to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestEnvKey {
    /// Set on the child in the descriptor-budget probe, which re-executes its
    /// own test binary so the `/proc/self/fd` count is taken in a process that
    /// ran nothing else. Present means "you are the child".
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
