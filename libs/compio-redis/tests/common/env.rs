//! The sealed test-key enum for this crate's gated tests.
//!
//! `compio-redis` is a standalone, publishable library with NO zeroship
//! dependency (`AGENTS.md`, the `libs/` boundary), so it cannot use the typed
//! keys in `zeroship_core::config` that the rest of the workspace reads the
//! environment through. Section 4.5 of
//! `docs/proposals/2026-08-11-config-name-alignment.md` allows exactly one
//! substitute: a dependency-free sealed key enum, in exactly
//! `libs/<crate>/tests/common/env.rs`, whose variants map to literal names and
//! whose raw access lives in one accessor.
//!
//! The workspace source gate recognizes this file BY PATH and by shape. It
//! permits no raw read anywhere else in `libs/compio-redis`, including this
//! crate's production sources: a published library takes resolved options from
//! its caller and does not read process configuration.
//!
//! The names stay enumerable, because they are literals in a sealed enum a
//! scanner can lift. What they do not get is a linked read SITE: the
//! `DECLARED_ENV_READS` slice lives in `zeroship-core`, which this crate must
//! not depend on. Location comes from the source scan, not from the binary.

/// Every environment name this crate's tests are permitted to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestEnvKey {
    /// Connection string for the single-node Redis suites. Absent means
    /// `common::DEFAULT_REDIS_URL`, never "do not run".
    RedisTestUrl,
    /// Comma-separated seed list for the gated cluster suite.
    DragonflyClusterSeeds,
}

impl TestEnvKey {
    /// The literal environment spelling. Literal arms only, by rule.
    const fn name(self) -> &'static str {
        match self {
            Self::RedisTestUrl => "REDIS_TEST_URL",
            Self::DragonflyClusterSeeds => "DRAGONFLY_CLUSTER_SEEDS",
        }
    }
}

/// The one raw environment read in `libs/compio-redis`.
///
/// It takes the sealed key, never a `&str`, so no caller can name a variable
/// this module has not declared. Non-Unicode reads as absent, matching the
/// `std::env::var(..).ok()` every call site used before.
#[allow(clippy::disallowed_methods)]
pub fn get(key: TestEnvKey) -> Option<String> {
    std::env::var(key.name()).ok()
}
