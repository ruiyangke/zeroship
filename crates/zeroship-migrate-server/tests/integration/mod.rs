//! The migration server's integration suites.
//!
//! `Cargo.toml` sets `autotests = false` and registers `tests/main.rs` as the
//! `main` target, so a suite is compiled only once it is declared here. Add
//! `mod <name>;` with the file or its tests never run. The suites reach the
//! shared fixtures at the target root as `crate::support::fixture::...`.

mod apply_api_test;
mod apply_database_target_pg;
mod author_and_apply_pg;
mod compio_pg_conformance;
mod datastore_reconciler_pg;
mod domain_boundary;
mod gen_artifacts_byte_identical;
mod health_endpoints_test;
mod smoke_apply_pg;
mod typed_id_parity;
