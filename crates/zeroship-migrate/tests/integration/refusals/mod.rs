//! Offline refusals: an op that names something an earlier op in the same migration took away.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


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
mod mysql_trigger_body_cannot_return_a_result_set;
mod new_rules_do_not_over_refuse;
mod op_references_a_dropped_column;
mod op_references_a_dropped_named_object;
mod partition_parent_must_still_exist;
mod resolved_validator_parity;
mod role_and_schema_use_after_drop;
mod second_relation_reference_after_drop;
mod sqlite_dangling_foreign_key;
mod type_use_after_drop_beyond_create_table;
mod virtual_generated_column_names_the_refusing_backend;
