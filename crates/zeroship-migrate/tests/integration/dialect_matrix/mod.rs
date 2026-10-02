//! The dialect table and its corpus: what each dialect declares, and what a dialect leg hides or shows.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod a_vendor_answers_one_capability_row;
mod alter_column_dialect_support;
mod alter_column_grammar_comes_from_the_backend;
mod an_authored_attribute_reaches_the_ddl;
mod checksum_corpus_stability;
mod created_tables_dialect_legs;
mod dialect_conformance_live;
mod dialect_table;
mod dialect_table_faithfulness;
mod dialectal_containers_are_expanded;
mod dialectal_ops;
mod every_backend_owns_the_attributes_it_declares;
mod every_backend_states_its_advisory_posture;
mod existence_probe_decides_per_dialect_leg;
mod gen_types_dialectal_runtime_metadata;
mod gen_types_dialectal_table_shape;
mod gen_types_drop_column_dialect_legs;
mod op_refused_observation;
mod op_support_matrix;
mod partition_recording_dialect_legs;
mod row_order_observation;
mod sqlite_declaration_flip_over_refusal_control;
mod sqlite_trigger_render_bytes;
mod touched_tables_dialect_legs;
mod unsupported_reason_is_operator_facing;
mod vendor_ops_dispatch_per_vendor;
mod vendor_registry_owns_shipping_descriptors;
