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

mod a_test_recorder_never_ships;
mod alter_column_dialect_support;
mod backend_modules_name_one_dialect;
mod backend_snapshot_privates_stay_core_only;
mod checksum_corpus_stability;
mod constraint_definition_is_comparison_text;
mod core_does_not_spell_a_vendors_bytes;
mod core_names_no_vendor_at_all;
mod core_names_no_vendor_backend_module;
mod core_names_no_vendor_crate;
mod created_tables_dialect_legs;
mod dialect_conformance_live;
mod dialect_table;
mod dialect_table_faithfulness;
mod dialectal_containers_are_expanded;
mod dialectal_ops;
mod dml_emitters_do_not_relookup_a_backend;
mod every_backend_states_its_advisory_posture;
mod existence_probe_decides_per_dialect_leg;
mod gen_types_dialectal_runtime_metadata;
mod gen_types_dialectal_table_shape;
mod gen_types_drop_column_dialect_legs;
mod mysql_type_text_lives_with_its_parser;
mod neutral_apply_layer_names_no_vendor;
mod op_refused_observation;
mod op_support_matrix;
mod partition_recording_dialect_legs;
mod plan_vocabulary_names_strategies_not_vendors;
mod registry_resolution_stays_core_only;
mod row_order_observation;
mod schema_emitters_do_not_relookup_a_backend;
mod shadow_dry_run_has_no_implementor;
mod sqlite_declaration_flip_over_refusal_control;
mod sqlite_trigger_quoting_reaches_postgres;
mod sqlite_trigger_render_bytes;
mod touched_tables_dialect_legs;
mod unsupported_reason_is_operator_facing;
mod value_format_comparison_is_not_an_emission_route;
mod vendor_ops_dispatch_per_vendor;
mod vendor_registry_owns_shipping_descriptors;

/// Test-only composition used to compare the generated review artifact with the
/// policies production resolves through its private registry. Keeping this here
/// prevents the vendor matrix and a registry test hook from entering core.
static SHIPPING_VENDORS: &[&zero_migrate_backend::registry::BackendVendor] = &[
    &zero_migrate_mysql::VENDOR,
    &zero_migrate_postgres::VENDOR,
    &zero_migrate_sqlite::VENDOR,
];
