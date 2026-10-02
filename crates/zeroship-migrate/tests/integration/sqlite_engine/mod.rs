//! The live SQLite engine: apply, journal, backfill, rebuild and the executor-side guards.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod alter_primary_key_sqlite;
mod apply_plan_sqlite;
mod backfill_sqlite;
mod declarative_sqlite;
mod dialect_scope_refuses_a_foreign_target;
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
