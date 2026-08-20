//! The dialect table and its corpus: what each dialect declares, and what a dialect leg hides or shows.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;
#[path = "../dialect_corpus/mod.rs"]
mod dialect_corpus;

mod alter_column_dialect_support;
mod backend_modules_name_one_dialect;
mod checksum_corpus_stability;
mod created_tables_dialect_legs;
mod dialect_conformance_live;
mod dialect_table_faithfulness;
mod dialectal_containers_are_expanded;
mod dialectal_ops;
mod gen_types_dialectal_runtime_metadata;
mod gen_types_dialectal_table_shape;
mod gen_types_drop_column_dialect_legs;
mod op_support_matrix;
mod partition_recording_dialect_legs;
mod sqlite_declaration_flip_over_refusal_control;
mod sqlite_trigger_quoting_reaches_postgres;
mod sqlite_trigger_render_bytes;
mod touched_tables_dialect_legs;
mod unsupported_reason_is_operator_facing;
