//! Renames and their carriers: every dependent body a rename has to follow, on both legs.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

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
