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

pub fn skip(reason: &str) {
    let _ = std::io::stderr().write_all(format!("{SKIP_MARKER}: {reason}\n").as_bytes());
}
