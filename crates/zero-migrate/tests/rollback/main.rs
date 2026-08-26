//! Rollback and inverse lowering: what a down migration restores, and what it refuses to claim it can.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod drop_extension_rollback_pg;
mod drop_function_rollback_pg;
mod drop_policy_rollback_pg;
mod drop_schema_rollback_pg;
mod drop_sequence_position_pg;
mod drop_sequence_rollback_pg;
mod drop_trigger_rollback_pg;
mod drop_view_rollback_pg;
mod drop_view_rollback_sqlite;
mod extension_claim_is_exclusive;
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
