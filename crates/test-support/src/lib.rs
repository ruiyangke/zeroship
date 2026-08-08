//! Test-only helpers shared across the workspace.
//!
//! Consumed as a `dev-dependency`; never linked into a shipping binary.

use std::io::Write;

/// The token every skip announcement carries, and the only thing a gate has to
/// look for.
///
/// It is deliberately not a word. `tests/run_auth_suite.sh` used to search the
/// run log for "skipping", which missed the 13 lines reading
/// `[anchors] skip <name> (no GATEWAY_ANCHORS_DB_URL)` and reported "0 skipped"
/// with them sitting in its own log. Widening that search to "skip" does not
/// work either, and the reason is measured: of the 98 lines containing "skip"
/// in one full run, 13 were real announcements, 5 were the harness's own
/// `test <name> ... ok` lines for tests whose names contain "skips"/"skipped"
/// (`pg_or_skip`, `dunning_tick_skips_when_advisory_lock_held`,
/// `persisted_skip_consent`), and the remaining ~80 were driver debug output
/// echoing `INSERT INTO zeroship.oauth_clients (..., skip_consent, ...)`. The
/// word occurs incidentally in identifiers, in SQL, and in log noise, so it
/// cannot discriminate.
///
/// The hyphens are what make this token safe: they are not legal in a Rust
/// identifier, so no test name can ever produce it, and nothing in the schema
/// or the SQL the drivers log is spelled this way. A repo-wide search found
/// zero occurrences before it was introduced here.
pub const SKIP_MARKER: &str = "ZEROSHIP-TEST-SKIPPED";

/// Announce that a test did nothing because the backend it needs is absent.
///
/// `reason` is carried through verbatim after the marker and should name what
/// was missing, usually the environment variable that was not set. A gate can
/// count the marker; a human reading the log still needs to know which knob to
/// turn.
///
/// The channel is a direct handle write and `println!`/`eprintln!` would not
/// do. The harness captures output by swapping the thread-local target those
/// macros write through, and replays that buffer only for a FAILING test, so an
/// announcement made through a macro is invisible on a pass, which is exactly
/// the run where it matters. A write straight to the `Stderr` handle never
/// enters the buffer. Measured in one passing test with no `--nocapture`:
/// `println!` and `eprintln!` vanished, `stderr().write_all` printed.
///
/// The write is best-effort. A test that cannot reach stderr is not a test
/// worth failing over, and a panicking announcer would turn a skip into a
/// failure with a misleading cause.
pub fn skip(reason: &str) {
    let _ = std::io::stderr().write_all(format!("{SKIP_MARKER}: {reason}\n").as_bytes());
}
