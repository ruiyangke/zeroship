//! MySQL rendering: the shapes MySQL's grammar forces and the ones it refuses.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod mysql_alter_column_render;
mod mysql_conformance;
mod mysql_enum_collation;
mod mysql_expression_default_render;
mod mysql_query_renderer_collation;
mod mysql_recorded_engine_paths;
mod mysql_setcolumntype_restate;
mod mysql_storage_shapes;
mod mysql_text_column_key_gate;
