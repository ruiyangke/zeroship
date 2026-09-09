// Most of the modules below used to be their own crate ROOT and each carried its
// own copy of this attribute: a fixture that chains several compio-postgres
// futures in one async block builds a generated state machine deep enough to
// overflow rustc's default layout query. `recursion_limit` is per crate root, so
// one declaration here covers all of them. A compile-budget knob, not a
// behaviour change.
#![recursion_limit = "256"]

//! Shared database integration fixtures, included by ordinary package tests.
//! PostgreSQL is required; unavailable databases and extensions fail the run.
//!
//! See `tests/main.rs` for adding a module or selecting a test subset.
//!
//! THE SHARED FIXTURES ARE DECLARED HERE, ONCE
//! -------------------------------------------
//! `support`, `schema_fixture` and `parity` are declared by THIS file and
//! reached from the modules below through `crate::`. Each file used to declare
//! its own `#[path]` copy, which was free while every file was its own process
//! and is not free now: a second `mod` of the same source under a second parent
//! compiles a second, independent copy of every `static` in it. Three of those
//! statics only work as singletons:
//!
//!   `support::sweep_prior_run_residue_once`  a `Once` guarding a sweep that
//!       DROPS this suite's schemas, roles, replication slots and publications.
//!       A second copy is a second sweep, and a sweep is only correct when no
//!       sibling is holding anything.
//!   `support::init_test_tracing`             a `Once` around the global
//!       tracing-subscriber install.
//!   `parity::MATRIX_COUNTER`                 hands every matrix run a
//!       collection name no sibling uses, so each case stays its own witness for
//!       what reached the disk. A second copy hands the same names out twice.
//!
//! `parity` also owns a thread-local compio runtime; its header explains why a
//! per-dispatch runtime is invisible on SQLite and fatal on Postgres.
//!
//! WHAT SHARING ONE PROCESS DOES NOT PROTECT AGAINST
//! -------------------------------------------------
//! Until this collapse, cargo ran these files as separate processes, one at a
//! time, and the PROCESS was what kept `sweep_prior_run_residue_once` away from
//! a live sibling. Only `integration` and `native_transaction` call it - the
//! other modules never reach the `Once` - so under `--test-threads` greater than
//! one they can now be running while it sweeps.
//!
//! What bounds that is the sweep's own scoping, not the merge. Its tenancy half
//! touches only namespaces carrying `support::TEST_APP_PREFIX` and the per-app
//! roles wrapping them, and those two modules are the only ones here that mint
//! an app id with that prefix; count them with
//! `grep -rln test_app_id crates/zeroship-plugin-db/tests` rather than trusting
//! this sentence. Its replication half touches only the platform's own slot and
//! publication prefix, and no other module here opens a logical-decoding
//! consumer.
//!
//! `tests/run_plugin_db_live_suite.sh` runs this target with `--test-threads=1`,
//! and has always had to for the older reason that these tests share one
//! database. Under that flag the window above does not exist.

// Declared once, for every module below. `support` also defines the exported
// `test_app_id!` macro, whose expansion names `$crate::support::test_app_id_from`
// - so it resolves against this declaration and would silently follow a second
// one.
mod parity;
#[path = "support/schema.rs"]
mod schema_fixture;
mod support;

// The PostgreSQL query-builder, CRUD, encryption and CDC suite. One of the two
// modules that mint `zst_`-prefixed app ids, and the only one that drives
// logical decoding.
mod integration;

// The same ground on SQLite, plus the `SqliteSession` actor in isolation.
mod sqlite_integration;

// The native `Db.transaction(fn)` orchestrator, end to end through a real
// `Runtime` and the `DbPlugin`. The other module that mints `zst_` app ids.
mod native_transaction;

// The missing-per-app-role classification (`schema_not_provisioned`). It needs a
// live server because `compio_postgres::Error` has no public constructor, so the
// SQLSTATE it classifies can only be produced by PostgreSQL itself.
mod missing_role;

// The `DbPlan` search family, EXECUTED. `zeroship-data-query-builder` declares no
// dependencies, so its own tests can only compare rendered SQL against a string;
// these run the same statements against a server carrying vector and postgis.
mod search_ir_live;

// The masking storage flip. Every fixture builds its table from the PLATFORM'S
// OWN DDL emitter and writes through the real write pipeline, because the risk
// is the emitter and the data plane disagreeing about which physical column
// holds what - and a hand-written fixture agrees with whichever one its author
// had in mind. It FAILS rather than skips without a database: a skipping run of
// a security suite is indistinguishable from a passing one.
mod mask_flip;

// The write builders' `RETURNING` projection, executed by a role holding
// COLUMN-level grants: one that may WRITE a masked field's raw column and may
// not READ it. That grant is unusable while any write verb stars, so this is
// what says whether the projection is real rather than whether the SQL text
// changed. It FAILS rather than skips without a database.
mod column_grants;

// Which CONNECTION a `find({ unmask })` uses inside `db.transaction(fn)`. Every
// fixture parks a real transaction connection in the per-isolate slot; the
// question is about lanes rather than about masking storage. FAILS rather than
// skips without a database.
mod unmask_tx_lane;

// The same question asked of the SEARCH FAMILY. It needs a server carrying BOTH
// vector and postgis, which the unmask fixtures do not. FAILS rather than skips
// without a database or without an extension.
mod search_tx_lane;
