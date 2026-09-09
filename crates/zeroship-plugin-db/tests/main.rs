//! The integration-test target that needs NO cargo feature.
//!
//! WHY THIS FILE EXISTS
//! --------------------
//! Cargo links one executable per `tests/*.rs` file, and every one of those
//! executables statically links this crate plus its whole dependency graph. The
//! cost is not one binary per file: cargo mints a fresh binary for each distinct
//! dependency-graph fingerprint and never collects the old one, so a crate whose
//! files are spread across three feature resolutions pays for each file in each
//! resolution. Collapsing to one target per resolution is the standard answer,
//! and `zeroship-auth` and `zeroship-control` already took it here; this follows
//! their shape.
//!
//! WHY THREE TARGETS AND NOT ONE
//! -----------------------------
//! `required-features` is a property of the TARGET, so two files needing
//! different features cannot share one. That is the whole reason the collapse
//! stops at three:
//!
//!   main              no `required-features`. Runs under a bare
//!                     `cargo test -p zeroship-plugin-db` and must keep doing
//!                     so - no module below reaches a `test-helpers` symbol or a
//!                     database.
//!   test_helpers      `required-features = ["test-helpers"]`. Everything that
//!                     reaches a `*_for_tests` symbol, a live PostgreSQL, or
//!                     both. See `tests/test_helpers.rs`.
//!   distributed_live  `required-features = ["live-db-tests"]`. One file, kept
//!                     as its own target because that feature is narrower still.
//!
//! HOW TO ADD A TEST FILE
//! ----------------------
//! `autotests = false`, so a new `tests/<name>.rs` is compiled by NOTHING until
//! a `mod <name>;` line names it here or in `tests/test_helpers.rs`. Add that
//! line in the same commit as the file, or its tests never run and nothing says
//! so. That is the one way this arrangement can lose coverage silently, which is
//! why both entry files say it.
//!
//! HOW TO RUN A SUBSET
//! -------------------
//! `cargo test -p zeroship-plugin-db --test <file>` no longer resolves for the
//! files below. The module path is a prefix of every test name, so filter on it:
//!
//!   cargo test -p zeroship-plugin-db --test main capability::
//!
//! A libtest filter is a SUBSTRING match with no anchor, so a module name that
//! is a suffix of another selects both - `integration::` also picks up
//! `sqlite_integration::`. Pair the filter with `--skip` when that matters.

mod audit_table_parity;
mod capability;
mod db_v8_class;
mod platform_fence;
mod subscription_finalizer;
