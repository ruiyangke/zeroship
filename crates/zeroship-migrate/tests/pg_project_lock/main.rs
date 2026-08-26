//! The PostgreSQL PROJECT LOCK: who holds it, what a peer sees, and what a
//! cancelled acquisition leaves behind.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.
//!
//! WHY THIS SUBJECT IS ITS OWN BINARY rather than part of `pg_engine`.
//! `pg_project_lock_grant` aims a peer release at the instant a 3ms
//! `statement_timeout` fires. That window is measured against a WALL CLOCK, so it is
//! sensitive to how much other work hits the same server at the same moment. Folded
//! in with the other 116 live-PostgreSQL tests it failed 1 run in 5 -
//! `pg_advisory_unlock_all()` itself overran the 3ms budget. Run beside its own
//! subject only it passed every attempt. Keeping the lock tests here is what
//! preserves the quiet server the race needs; it is not a naming preference.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod pg_project_lock_grant;
mod pg_status_project_lock;
