//! The sealed test-key enum for this crate's gated tests.
//!
//! `compio-postgres` is a standalone, publishable driver with NO zeroship
//! dependency (`AGENTS.md`, the `libs/` boundary), so it cannot use the typed
//! keys in `zeroship_core::config` that the rest of the workspace reads the
//! environment through. Section 4.5 of
//! `docs/proposals/2026-08-11-config-name-alignment.md` allows exactly one
//! substitute: a dependency-free sealed key enum, in exactly
//! `libs/<crate>/tests/common/env.rs`, whose variants map to literal names and
//! whose raw access lives in one accessor.
//!
//! The workspace source gate recognizes this file BY PATH and by shape. It
//! permits no raw read anywhere else in `libs/compio-postgres`, including this
//! crate's production sources: a published library takes resolved options from
//! its caller and does not read process configuration. Adding a name here is
//! adding a variant, which is a visible edit in a file the gate already reads.
//!
//! WHAT THIS DOES NOT GIVE UP, AND WHAT IT DOES: the names are still
//! enumerable, because they are literals in a sealed enum a scanner can lift.
//! What it does not give is a linked read SITE - there is no
//! `DECLARED_ENV_READS` entry for a `libs/` test, because that slice lives in
//! `zeroship-core`. Location therefore comes from the source scan, not from the
//! binary.

/// Every environment name this crate's tests are permitted to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestEnvKey {
    /// Connection string for the PostgreSQL integration suites. Absent means
    /// the default DSN in each target, never "do not run".
    PgTestUrl,
}

impl TestEnvKey {
    /// The literal environment spelling. Literal arms only, by rule.
    const fn name(self) -> &'static str {
        match self {
            Self::PgTestUrl => "PG_TEST_URL",
        }
    }
}

/// The one raw environment read in `libs/compio-postgres`.
///
/// It takes the sealed key, never a `&str`, so no caller can name a variable
/// this module has not declared. Non-Unicode reads as absent, matching the
/// `std::env::var(..).ok()` every call site used before.
#[allow(clippy::disallowed_methods)]
pub fn get(key: TestEnvKey) -> Option<String> {
    std::env::var(key.name()).ok()
}
