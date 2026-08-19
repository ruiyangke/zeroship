//! Column shape: declared type, the facets a retype carries, identity, collation and value formats.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod catalog_format_proof;
mod domain_base_type_reaches_no_second_check;
mod encrypted_domain_catalog_sentinel;
mod enum_membership_reaches_no_second_check;
mod enums_domains;
mod generated_identity_columns;
mod injected_column_collation;
mod mysql_field_def_carrier_collation;
mod pg_column_retype_dependency_oracle;
mod pg_setcolumntype_half_migration;
mod scalar_precision_boundary_pg;
mod set_column_type_facets;
mod set_column_type_facets_pg;
mod set_column_type_generation_contracts;
mod set_column_type_generation_contracts_pg;
mod synchronize_identity_pg;
mod synchronize_identity_sqlite;
mod type_id_value_format;
mod typed_references;
mod ulid_value_format;
mod uuid_generation;
