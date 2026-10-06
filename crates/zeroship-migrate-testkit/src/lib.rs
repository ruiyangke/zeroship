//! # `zeroship-migrate-testkit` - canned `SqlSession` doubles for the migration backends
//!
//! One dev-only crate holding the two vendors' canned
//! [`SqlSession`](zeroship_migrate_backend::driver::SqlSession) recorders as sibling
//! modules: [`mysql::RecordingSession`] records the MySQL backend's SQL and returns
//! its canned `information_schema` rows, and [`postgres::RecordingSession`] does the
//! same for the PostgreSQL backend. The vendor's own unit tests and the engine's
//! integration tests in `zeroship-migrate/tests/integration/` both name this crate
//! under `[dev-dependencies]`.
//!
//! # Why the recorders are modules of a shared crate
//!
//! Two suites drive each recorder, and they cannot share a `#[cfg(test)]` module.
//! What a recorder proves about a vendor - which SQL it emits, in which order, with
//! which binds - belongs beside that code, as unit tests in the vendor crate. What
//! it proves about the ENGINE (`apply_with_lock_backend`, `MigrationEngine`,
//! `diff_snapshots`, `fold_ops`, `AppliedPlan`, `ops::status::history_via_backend`)
//! cannot live in a vendor crate, because `zeroship-migrate` depends on the vendor
//! and the edge back is a cycle Cargo refuses.
//!
//! A copy of a recorder on each side is the real hazard: its canned rows are the
//! shared premise of both suites, and two copies drift silently - one suite would go
//! on asserting against a shape the other had already corrected. So each recorder is
//! ONE object in this crate, and both sides name it under `[dev-dependencies]`.
//!
//! This crate reaches the contract ([`zeroship_migrate_backend`]) and the IR
//! ([`zeroship_migrate_ir`]) and neither a vendor nor the engine, so the dependency
//! edges run one way from every user and nothing shipped can link it.

pub mod mysql;
pub mod postgres;
