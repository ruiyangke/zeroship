//! The offline fold: what replaying ops into a snapshot produces, without a server in the loop.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod fold_drop_column_exclusion_expression;
mod fold_rename_column_generated_expr_runtime;
mod fold_replace_view_materialized_kind;
mod fold_replays_column_facet_ops;
mod gen_types_mid_expand_rename;
mod plan_rollbackable;
mod rename_column_generated_expr_snapshot;
mod schema_model_god_object_bound;
mod structural_equality_field_sensitivity;
