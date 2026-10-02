//! Renames and their carriers: every dependent body a rename has to follow, on both legs.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod declarative_rename_mysql;
mod ir_rename_sqlite_basic;
mod op_after_rename_targets_old_name;
mod rename_carrier_sweep_pg;
mod rename_carrier_sweep_sqlite;
mod rename_column_fk_definition_sqlite;
mod rename_column_indexed_sqlite;
mod rename_column_inline_check_sqlite;
mod rename_into_the_type_namespace_pg;
mod rename_table_to_itself_is_refused;
mod sqlite_repeat_rename_dialect_legs;
