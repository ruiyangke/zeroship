//! The IR wire contract: envelope schema, checksums, render parity and the recorded goldens.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod golden_trace_sqlite;
mod ir_author_render_parity;
mod ir_checksum;
mod ir_envelope_schema;
mod ir_wire_contract;
mod op_fixture_goldens;
mod preview_fold_table_presence;
mod sql_preview;
