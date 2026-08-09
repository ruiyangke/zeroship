//! Skip announcer for this crate's gated tests.
//!
//! This is a deliberate copy of `crates/test-support`, which is where the
//! reasoning behind the marker and the direct-handle write is written down.
//! `compio-s3` is a standalone, publishable library with no zeroship
//! dependency, and it does not grow one for a test helper. What must stay
//! identical is the MARKER TEXT: one search over a run log has to find every
//! skip in the workspace, whichever side of that line it came from.

use std::io::Write;

pub const SKIP_MARKER: &str = "ZEROSHIP-TEST-SKIPPED";

/// Opt-in strictness, kept identical to `crates/test-support` for the same
/// reason the marker text is: a run that declares its backends are provisioned
/// must be held to that on BOTH sides of the standalone/workspace line, or the
/// strict gate silently exempts whichever crates copied the helper.
pub const REQUIRE_LIVE_BACKENDS_ENV: &str = "ZEROSHIP_REQUIRE_LIVE_BACKENDS";

pub fn require_live_backends() -> bool {
    matches!(
        std::env::var(REQUIRE_LIVE_BACKENDS_ENV).ok().as_deref(),
        Some("1")
    )
}

pub fn skip(reason: &str) {
    let _ = std::io::stderr().write_all(format!("{SKIP_MARKER}: {reason}\n").as_bytes());
    if require_live_backends() {
        panic!(
            "{REQUIRE_LIVE_BACKENDS_ENV}=1 declares the live backends are provisioned, \
             but this test skipped: {reason}"
        );
    }
}
