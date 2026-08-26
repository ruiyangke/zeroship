//! The IR wire contract: envelope schema, checksums, render parity and the recorded goldens.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod embedding_guide_is_compiled;
mod golden_trace_sqlite;
mod ir_author_render_parity;
mod ir_checksum;
mod ir_envelope_schema;
mod ir_wire_contract;
mod op_fixture_goldens;
mod preview_fold_table_presence;
mod sql_preview;
