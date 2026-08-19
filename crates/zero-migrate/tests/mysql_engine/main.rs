//! MySQL rendering: the shapes MySQL's grammar forces and the ones it refuses.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod mysql_alter_column_render;
mod mysql_enum_collation;
mod mysql_expression_default_render;
mod mysql_query_renderer_collation;
mod mysql_storage_shapes;
mod mysql_text_column_key_gate;
