//! Offline refusals: an op that names something an earlier op in the same migration took away.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod alter_sequence_needs_an_action;
mod backfill_references_a_dropped_column;
mod column_accessors_are_exhaustive;
mod dml_aggregate_refusal;
mod dml_qualified_ref_refusal;
mod dml_references_a_dropped_column;
mod dropped_column_named_beyond_the_dml_ops;
mod exclusion_constraint_column_refs;
mod expr_references_a_dropped_column;
mod fk_candidate_key_after_alter_primary_key;
mod grant_targets_a_vacated_table;
mod instead_of_trigger_needs_a_view;
mod new_rules_do_not_over_refuse;
mod op_references_a_dropped_column;
mod op_references_a_dropped_named_object;
mod partition_parent_must_still_exist;
mod resolved_validator_parity;
mod role_and_schema_use_after_drop;
mod second_relation_reference_after_drop;
mod sqlite_dangling_foreign_key;
mod type_use_after_drop_beyond_create_table;
