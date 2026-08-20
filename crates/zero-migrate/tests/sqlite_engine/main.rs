//! The live SQLite engine: apply, journal, backfill, rebuild and the executor-side guards.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod alter_primary_key_sqlite;
mod apply_plan_sqlite;
mod backfill_sqlite;
mod declarative_sqlite;
mod engine_sqlite;
mod existence_guard_sqlite;
mod hr_sqlite;
mod ir_apply_sqlite;
mod sqlite_apply;
mod sqlite_drift;
mod sqlite_goodies;
mod sqlite_multi_app;
mod sqlite_rebuild_apply;
mod unmet_precondition_blocks_the_ddl;
mod virtual_table_drop_refusal;
