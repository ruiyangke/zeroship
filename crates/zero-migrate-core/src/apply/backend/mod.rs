//! What is left of the `MigrationBackend` dialect seam's old home: the optional
//! capability traits, and the re-exports.
//!
//! The seam itself is [`zero_migrate_backend::backend::MigrationBackend`],
//! re-exported below at the `apply::backend::MigrationBackend` path it has always
//! been reached by. It is declared in the contract crate rather than here for the
//! reason every other item there is: a vendor crate cannot implement a trait that
//! lives in the engine, because the engine already depends on every vendor.
//!
//! NO VENDOR IS LEFT IN THIS MODULE. All three execution halves live in
//! `zero-migrate-postgres`, `zero-migrate-sqlite` and `zero-migrate-mysql`, and core
//! re-exports none of them, because a `pub use zero_migrate_postgres::...` here would
//! be core NAMING a vendor outside the registry - the thing
//! `tests/dialect_matrix/core_names_no_vendor_crate.rs` exists to forbid. A caller
//! that wants `PostgresBackend` names the vendor crate, exactly as the registry
//! composition does. PostgreSQL was the last one inside, and the governing rule
//! asked for exactly this: the core is neutral, and that is the hard limit.
//!
//! The executor's apply/rollback **orchestration** - partition versioned vs
//! repeatable, the drift/tamper gate, squash/expand gates, `order_pending`, the
//! FIRST/SECOND pass, the repeatable phase, rollback selection + reverse-topo
//! ordering - is dialect-agnostic and stays single-sourced in
//! [`crate::apply::executor`]. Everything the orchestration touches that is
//! **dialect-coupled** lives behind the trait:
//!
//! - **connection / session I/O** - the project lock
//!   (`pg_advisory_lock(hashtext($1))`), the GUC snapshot/restore
//!   (`current_setting`/`set_config`), the unconditional `RESET ROLE`, and
//!   transaction begin/commit/rollback;
//! - **the per-migration confined apply** - the txn path (`BEGIN; SET LOCAL ...;
//!   SET LOCAL ROLE migrator; <up>; RESET ROLE; INSERT journal; COMMIT`), the
//!   non-txn two-phase path, the non-txn crash recovery, and the rollback `down`;
//! - **journal row I/O** - the shared-sequence net-state reads
//!   (`applied`/`superseded_versions`/`latest_completed_checksums`), the
//!   immutability bootstrap, and the event inserts - exposed as **dialect-neutral
//!   owned row structs** (`AppliedEntry`, ...), never a `compio_postgres::Row`;
//! - **parse-time non-txn idempotency validation** - PG calls `pg_query::parse`
//!   directly; a SQLite backend rejects `transaction:false` at the dialect
//!   boundary instead, so this MUST sit behind the trait (no raw `pg_query::parse`
//!   in the generic `apply_locked` body);
//! - **drift schema introspection** - `snapshot_schema` over
//!   `information_schema`/`pg_catalog` (PG) vs `sqlite_master` + PRAGMAs (SQLite);
//!   the checksum/tamper comparison itself is dialect-agnostic and stays generic
//!   ([`check_checksum_drift`](crate::apply::backend::MigrationBackend::check_checksum_drift)).
//!
//! `zero_migrate_postgres::PostgresBackend`, `zero_migrate_sqlite::SqliteBackend`
//! and `zero_migrate_mysql::MysqlBackend` are the live implementations. Postgres
//! remains the richest regression bar; SQLite and MySQL provide dialect-specific
//! session, journal, drift, and DML behavior behind the same orchestration trait,
//! without forking the generic executor.
//!
//! The trait is used through **static dispatch** (`<B: MigrationBackend>`), so
//! native `async fn` in trait (Rust >= 1.75) is used directly - no boxing, no
//! `dyn`, no `async-trait` allocation on the apply hot path.

pub mod capability;
// PostgreSQL used to sit here, and SQLite and MySQL beside it. All three are
// `zero_migrate_postgres::backend`, `zero_migrate_sqlite::backend` and
// `zero_migrate_mysql::backend` now, and none is re-exported from here.

pub use capability::{
    BackendCapability, BackfillError, BackfillOutcome, BackfillSpec, DryRunError, DryRunReport,
    MigrationResult, OnlineSchemaChange, ShadowConfig, ShadowDryRun,
};
// The progress row a resumable backfill reads back. It moved down beside the
// `BackfillSpec` it describes progress THROUGH; re-exported so
// `apply::backend::BackfillProgressEntry` resolves unchanged.
pub use zero_migrate_backend::backfill::BackfillProgressEntry;
// THE SEAM ITSELF, and the vocabulary its signatures name. `MigrationBackend` is
// declared in `zero-migrate-backend` now - the crate every vendor already depends
// on - so a backend crate can implement it without depending on the engine that
// depends on every backend. That inversion is the whole point of the contract
// crate, and this module is what is left of the trait's old home: the optional
// capability traits and these re-exports.
//
// Re-exported so every historical `apply::backend::...` and
// `zero_migrate::apply::backend::...` path resolves unchanged.
pub use zero_migrate_backend::backend::{
    CrossDeployObligations, JournalFuture, MigrationBackend, PlaceholderStyle,
    PlanPreconditionVerdict, ProjectLockAcquisition, ProjectLockHolder,
};

// The non-blocking project-lock retry budget moved down beside
// `ProjectLockAcquisition`, the type it budgets. All three backends read the same
// two values so their busy verdicts cannot drift apart, and every backend has now
// left this crate.
