//! Column shape: declared type, the facets a retype carries, identity, collation and value formats.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


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
mod structured_types;
mod synchronize_identity_mysql;
mod synchronize_identity_pg;
mod synchronize_identity_sqlite;
mod typed_references;
mod uuid_generation;
