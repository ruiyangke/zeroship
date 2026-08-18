//! The fold oracle adjudicated by a live server: fold a snapshot, then ask the database if it agrees.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod env_db_ts_matches_the_server_pg;
mod fold_drop_column_check_cascade_pg;
mod fold_drop_column_exclusion_cascade_pg;
mod fold_drop_column_index_cascade_pg;
mod fold_rename_column_check_body_pg;
mod fold_rename_column_constraint_definition_pg;
mod fold_rename_column_generated_expr_pg;
mod fold_rename_column_index_body_pg;
mod fold_rename_column_index_cascade_pg;
mod fold_retype_physical_type_mysql;
mod fold_role_extension_pg;
mod fold_roundtrip_mysql;
mod fold_roundtrip_pg;
mod fold_roundtrip_sqlite;
mod schema_model_equivalence_mysql;
mod schema_model_equivalence_pg;
mod sqlite_rebuild_field_defs_live;
