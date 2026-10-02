//! Structural drift against live PostgreSQL: what an out-of-band change makes the drift report say.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod collation_introspection;
mod drift_check_body_pg;
mod drift_column_physical_type;
mod drift_function_body_pg;
mod drift_id_facets_pg;
mod drift_noop_index_predicate_pg;
mod drift_plain_column_default_pg;
mod drift_unattributed_snapshot;
mod drift_view_body_pg;
mod f721_unguarded_index_shape;
mod fold_cross_schema_drift_pg;
mod index_exact_name_shape_pg;
mod index_name_scheme_alias_pg;
mod rls_drift;
mod truncated_identifier_pg;
mod vendor_object_drift;
mod vendor_object_drift_pg;
