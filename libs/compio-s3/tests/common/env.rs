//! The sealed test-key enum for this crate's gated tests.
//!
//! `compio-s3` is a standalone, publishable client with NO zeroship dependency
//! (`AGENTS.md`, the `libs/` boundary), so it cannot use the typed keys in
//! `zeroship_core::config` that the rest of the workspace reads the environment
//! through. Section 4.5 of
//! `docs/proposals/2026-08-11-config-name-alignment.md` allows exactly one
//! substitute: a dependency-free sealed key enum, in exactly
//! `libs/<crate>/tests/common/env.rs`, whose variants map to literal names and
//! whose raw access lives in one accessor.
//!
//! The workspace source gate recognizes this file BY PATH and by shape. It
//! permits no raw read anywhere else in `libs/compio-s3`, including this
//! crate's production sources: the client takes static credentials from its
//! caller and never resolves them from the process environment. That is the
//! same rule stated positively in Section 4.5 - "platform processes declare and
//! read any external credentials before injecting them into a library".

/// Every environment name this crate's tests are permitted to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestEnvKey {
    /// Opt-in strictness: a skip becomes a failure when this is `1`.
    RequireLiveBackends,
}

impl TestEnvKey {
    /// The literal environment spelling. Literal arms only, by rule.
    const fn name(self) -> &'static str {
        match self {
            Self::RequireLiveBackends => "ZEROSHIP_REQUIRE_LIVE_BACKENDS",
        }
    }
}

/// The one raw environment read in `libs/compio-s3`.
///
/// It takes the sealed key, never a `&str`, so no caller can name a variable
/// this module has not declared. Non-Unicode reads as absent, matching the
/// `std::env::var(..).ok()` every call site used before.
#[allow(clippy::disallowed_methods)]
pub fn get(key: TestEnvKey) -> Option<String> {
    std::env::var(key.name()).ok()
}
