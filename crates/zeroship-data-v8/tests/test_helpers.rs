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
//! Fixtures and tracing helpers are declared here and shared by the modules.
//! Each PostgreSQL test retains its own container guard. SQLite tests retain
//! their temporary files. Nextest isolates processes and bounds simultaneous
//! database tests through `.config/nextest.toml`.
//!
//! `parity` also owns a thread-local compio runtime; its header explains the
//! runtime lifetime required by repeated V8 dispatches.

// Declared once, for every module below. `support` also defines the exported
// `test_app_id!` macro, whose expansion names `$crate::support::test_app_id_from`
// - so it resolves against this declaration and would silently follow a second
// one.
mod parity;
#[path = "support/schema.rs"]
mod schema_fixture;
mod support;

// PostgreSQL query-builder, CRUD and encryption tests use suite-prefixed app ids.
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

// The `DbPlan` search family, EXECUTED. `zeroship-data-sql` declares no
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
