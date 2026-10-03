//! The migrate-node integration suites, linked into one test executable.
//!
//! `Cargo.toml` sets `autotests = false` and registers `tests/main.rs` as the
//! `main` target, so a suite is compiled only once it is declared here. Add
//! `mod <name>;` with the file or its tests never run. The suites reach the
//! shared helpers at the target root as `crate::support::...`.

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
