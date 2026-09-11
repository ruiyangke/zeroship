use crate::{metrics, protection, system_shape_charter, tx_lanes};

/// Install a descriptor for an isolated test binding.
pub(crate) fn cache_schema(
    app_id: &str,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
) {
    cache_schema_for_deploy(
        &zeroship_data_orm::binding::DbBinding::cold_start(app_id),
        collection,
        schema,
    );
}

/// Test helper: [`cache_schema`] for an explicit binding, so a
/// fixture can install two deploys of one app and assert they do not see each
/// other's descriptor entries.
pub(crate) fn cache_schema_for_deploy(
    binding: &zeroship_data_orm::binding::DbBinding,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
) {
    zeroship_data_orm::schema_cache::with_mut(|c| c.insert_one(binding, collection, schema));
}

pub(crate) fn reset_engine() {
    tx_lanes::reset_for_tests();
    protection::mask_policy::reset_for_tests();
    protection::protection_floor::reset_for_tests();
    metrics::reset_for_tests();
    system_shape_charter::reset_for_tests();
    zeroship_data_orm::schema_cache::reset_for_tests();
}
