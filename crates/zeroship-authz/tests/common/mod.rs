//! What the two live-database targets in this crate share: the one place a DSN
//! is resolved, and the refusal that replaces a skip.
//!
//! WHY THIS IS NOT A SKIP. Both targets used to open with
//!
//! ```text
//! let Some(dsn) = zeroship_core::config::test_database_url_opt() else {
//!     zeroship_test_support::skip("skipping (no test database; set PG_TEST_URL)");
//!     return;
//! };
//! ```
//!
//! which made an unconfigured `cargo test -p zeroship-authz` print a full green
//! while every database test returned before its first assertion. Measured with
//! a one-variable control: with the DSN set the tests resolve and run; unset,
//! each prints its skip marker to the real stderr and libtest still reports
//! `ok`, so without `--nocapture` the two runs differ only in elapsed time.
//! That is the failure this repository names most often - a guard that examined
//! nothing looks exactly like a clean one - sitting on the crate that owns the
//! organization authority ladder, where "verification ran green" is the whole
//! claim anyone wants from it.
//!
//! THE SHAPE THE GATES USE. A shell gate under `tests/` emits one line per arm
//! carrying the number of items that arm ruled on, and a floor that number must
//! clear; an arm that can rule on nothing refuses and emits NO arm line rather
//! than printing what a clean arm prints. A cargo test binary has the same two
//! outcomes available - a verdict, or the statement that no verdict was
//! reachable - and `zeroship_testkit::live_db` already draws that line for the
//! control and gateway targets. This uses it. There is no skip path left here,
//! so the count of database tests that ran is either all of them or none, and
//! none is loud: the process exits with `REFUSED_EXIT_CODE`, distinct from
//! libtest's own status for a failing test, having printed a block that says NO
//! TEST RAN.
//!
//! NO NEW ENVIRONMENT VARIABLE, deliberately. `PG_TEST_URL` and the generated
//! overlay already answer "which database", and a second name deciding "is a
//! missing one fatal" would put the answer back in the hands of whoever
//! remembered to export it. It is not a setting; it is a property of these
//! tests.

use std::sync::OnceLock;

/// The DSN both live targets dial, or a refusal that ends the process.
///
/// Memoised, so the preflight costs one connection per test binary rather than
/// one per test - and so the refusal, when it comes, is printed once.
///
/// The schema pair is [`zeroship_testkit::live_db::PLATFORM_SCHEMAS`], which is
/// also what turns the journal-currency stage on. These tests read
/// `zeroship.organization_members`, `zeroship.projects` and `zeroship.apps`; a
/// database migrated by an older checkout answers their queries with a missing
/// column, which is the same void run wearing different clothes.
pub fn live_dsn() -> String {
    static DSN: OnceLock<String> = OnceLock::new();
    DSN.get_or_init(|| {
        zeroship_testkit::live_db::require_configured(
            zeroship_core::config::test_database_url_opt(),
            zeroship_testkit::live_db::PLATFORM_SCHEMAS,
        )
    })
    .clone()
}
