//! The offline fold: what replaying ops into a snapshot produces, without a server in the loop.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod fold_drop_column_exclusion_expression;
mod fold_rename_column_generated_expr_runtime;
mod fold_replace_view_materialized_kind;
mod fold_replays_column_facet_ops;
mod gen_types_mid_expand_rename;
mod plan_rollbackable;
mod rename_column_generated_expr_snapshot;
mod schema_model_god_object_bound;
mod structural_equality_field_sensitivity;
