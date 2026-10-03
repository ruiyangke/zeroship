//! The public-API and PostgreSQL integration suites for `zeroship-data-orm`.
//!
//! `autotests = false`, so a new suite is compiled only once it is declared
//! here. The compile-failure fixtures under `tests/ui/`, `tests/pass/` and
//! `tests/sql/ui/` are data for `trybuild`, not modules.

mod derive_contract;
mod error_contract;
mod native_schema;
mod native_schema_allocation;
mod postgres_binding_fence;
mod postgres_database_encryption;
mod postgres_tenant_fence;
mod sql;
