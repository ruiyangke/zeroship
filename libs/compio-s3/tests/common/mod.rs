//! Skip announcer for this crate's gated tests.
//!
//! This is a deliberate copy of `crates/test-support`, which is where the
//! reasoning behind the marker and the direct-handle write is written down.
//! `compio-s3` is a standalone, publishable library with no zeroship
//! dependency, and it does not grow one for a test helper. What must stay
//! identical is the MARKER TEXT: one search over a run log has to find every
//! skip in the workspace, whichever side of that line it came from.
//!
//! NO `env` MODULE, and its absence is deliberate. This crate had a sealed
//! `tests/common/env.rs` whose enum carried exactly ONE variant,
//! `ZEROSHIP_REQUIRE_LIVE_BACKENDS`. With that flag deleted the enum had no
//! variants left, so the sealed accessor was reading nothing; the file went
//! rather than lingering as an empty frame that the next name to come along
//! would be dropped into unexamined. `libs/compio-s3` now reads no environment
//! at all, in tests or in production, which is the strongest form of the rule
//! that `crates/core/tests/config_env_access_gate.rs` enforces.
//!
//! WHY THIS CRATE STILL SKIPS while `compio-postgres` and `compio-redis` no
//! longer do. What `minio_smoke.rs` announces is a missing DOCKER DAEMON and a
//! MinIO container that would not start - not a backend anything provisions
//! ahead of the run. The operator decision that removed the flag names Postgres
//! and Redis; `tests/provision_test_backends.sh` stands up those two and not
//! this. A skip here is an honest absence, not a hidden pass, and the suite
//! gates count it: `tests/lib/skip_census.sh` reads this marker.

use std::io::Write;

/// The skip token, declared here rather than taken from `crates/test-support`.
///
/// THIS DUPLICATION IS DELIBERATE AND MUST STAY, for the reason spelled out at
/// `libs/compio-redis/tests/common/mod.rs`: `compio-s3` is a standalone,
/// publishable driver with no zeroship dependency, and `cargo test` builds
/// dev-dependencies, so sharing four lines from `zeroship-test-support` would
/// put a zeroship crate in this one's build graph.
///
/// The copies stay honest because the CONSUMER is shared:
/// `tests/lib/skip_census.sh` greps for this exact string, so a copy that
/// drifted would stop being counted and the suite gates would notice.
pub const SKIP_MARKER: &str = "ZEROSHIP-TEST-SKIPPED";

pub fn skip(reason: &str) {
    let _ = std::io::stderr().write_all(format!("{SKIP_MARKER}: {reason}\n").as_bytes());
}
