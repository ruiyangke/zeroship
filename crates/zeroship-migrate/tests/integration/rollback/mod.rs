//! Rollback and inverse lowering: what a down migration restores, and what it refuses to claim it can.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod drop_extension_rollback_pg;
mod drop_function_rollback_pg;
mod drop_policy_rollback_pg;
mod drop_schema_rollback_pg;
mod drop_sequence_position_pg;
mod drop_sequence_rollback_pg;
mod drop_trigger_rollback_pg;
mod drop_view_rollback_pg;
mod drop_view_rollback_sqlite;
mod inverse_carries_no_unguarded_sql;
mod ir_reverse;
mod journal_reverse_compat_sqlite;
mod partial_plan_failure_is_coherent;
mod replace_function_rollback_pg;
mod replace_view_rollback_sqlite;
mod rollback_restores_prior_schema;
mod rollback_restores_prior_schema_pg;
mod sqlite_rollback;
mod squash_supersession_pg;
mod vendor_guarded_create_down;
