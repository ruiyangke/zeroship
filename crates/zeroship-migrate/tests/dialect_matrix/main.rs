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

mod a_vendor_answers_one_capability_row;
mod alter_column_dialect_support;
mod alter_column_grammar_comes_from_the_backend;
mod an_authored_attribute_reaches_the_ddl;
mod backend_modules_name_one_dialect;
mod checksum_corpus_stability;
mod core_names_no_vendor_at_all;
mod core_names_no_vendor_backend_module;
mod core_names_no_vendor_crate;
mod core_spells_no_vendor_grammar;
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

/// Test-only composition used to compare the generated review artifact with the
/// policies production resolves through its private registry. Keeping this here
/// prevents the vendor matrix and a registry test hook from entering core.
static SHIPPING_VENDORS: &[&zeroship_migrate_backend::registry::BackendVendor] = &[
    &zeroship_migrate_mysql::VENDOR,
    &zeroship_migrate_postgres::VENDOR,
    &zeroship_migrate_sqlite::VENDOR,
];
