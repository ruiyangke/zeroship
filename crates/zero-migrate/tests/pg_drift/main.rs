//! Structural drift against live PostgreSQL: what an out-of-band change makes the drift report say.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod collation_introspection;
mod drift_column_physical_type;
mod drift_function_body_pg;
mod drift_id_facets_pg;
mod drift_noop_index_predicate_pg;
mod drift_plain_column_default_pg;
mod f721_unguarded_index_shape;
mod fold_cross_schema_drift_pg;
mod fts_index_name_truncation_pg;
mod index_exact_name_shape_pg;
mod index_name_scheme_alias_pg;
mod rls_drift;
mod truncated_identifier_pg;
mod vendor_object_drift;
mod vendor_object_drift_pg;
