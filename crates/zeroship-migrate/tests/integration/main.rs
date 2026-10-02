//! The migration engine's integration suites, linked into one test executable.
//!
//! Cargo links one executable per `tests/*.rs`; `Cargo.toml` sets `autotests =
//! false` and registers this file as the `integration` target, so a suite is
//! compiled only once it is declared here. Add `mod <name>;` with the file or its
//! tests never run. The suites share `support` and the dialect corpus, declared
//! once here and reached as `crate::support::...` and `crate::dialect_corpus::...`.

mod support;
mod dialect_corpus;

/// Test-only composition used to compare the generated review artifact with the
/// policies production resolves through its private registry. It lives at this
/// crate root because the generated dialect table names it as `crate::...`.
static SHIPPING_VENDORS: &[&zeroship_migrate_backend::registry::BackendVendor] = &[
    &zeroship_migrate_mysql::VENDOR,
    &zeroship_migrate_postgres::VENDOR,
    &zeroship_migrate_sqlite::VENDOR,
];

mod authoring_surface;
mod column_shapes;
mod declared_column_roles;
mod dialect_matrix;
mod fold_live;
mod fold_offline;
mod gen_types;
mod ir_contract;
mod mysql_engine;
mod namespaces;
mod pg_drift;
mod pg_engine;
mod pg_project_lock;
mod policy_charter;
mod refusals;
mod rename;
mod rollback;
mod sqlite_engine;
