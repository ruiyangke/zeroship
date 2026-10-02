//! The migrate-node integration suites, linked into one test executable.
//!
//! Cargo links one executable per `tests/*.rs`; `Cargo.toml` sets `autotests =
//! false` and registers this file as the `integration` target, so a suite is
//! compiled only once it is declared here. Add `mod <name>;` with the file or its
//! tests never run. The suites share `support`, declared once here and reached as
//! `crate::support::...`.

mod support;

mod collection_export_round_trip;
mod gen_artifacts_dialectal_report;
mod gen_artifacts_domain_column;
mod gen_artifacts_enum_column;
mod gen_artifacts_exports_collections;
mod gen_artifacts_reserved_identifiers;
mod mock_apply;
mod named_relations;
mod rollback_over_completed_rename;
mod rollback_sqlite;
mod status_ir_host;
