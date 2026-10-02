//! The migration server's integration suites, linked into one test executable.
//!
//! Cargo links one executable per `tests/*.rs`; `Cargo.toml` sets `autotests =
//! false` and registers this file as the `integration` target, so a suite is
//! compiled only once it is declared here. Add `mod <name>;` with the file or its
//! tests never run. The suites share `fixture`, declared once here and reached as
//! `crate::fixture::...`, plus the tenant-cluster fixture it carries.

mod fixture;

#[path = "fixture/tenant.rs"]
mod tenant;

mod apply_api_test;
mod apply_database_target_pg;
mod author_and_apply_pg;
mod compio_pg_conformance;
mod datastore_reconciler_pg;
mod domain_boundary;
mod health_endpoints_test;
mod smoke_apply_pg;
mod typed_id_parity;
