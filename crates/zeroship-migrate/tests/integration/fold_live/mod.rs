//! The fold oracle adjudicated by a live server: fold a snapshot, then ask the database if it agrees.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod env_db_ts_matches_the_server_pg;
mod field_def_column_facets_pg;
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
mod mysql_bounded_string_producer_live;
mod mysql_primary_key_name_is_the_catalogs;
mod pg_bounded_string_producer_live;
mod schema_model_equivalence_mysql;
mod schema_model_equivalence_pg;
mod sqlite_decimal_rebuild_live;
mod sqlite_field_def_type_tokens_live;
mod sqlite_rebuild_field_defs_live;
mod state_at_matches_the_server_pg;
