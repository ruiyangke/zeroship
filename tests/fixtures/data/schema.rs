//! Database fixtures use the migration engine that creates creator tables.

use zeroship_data_orm::sql::SchemaName;
use zeroship_data_orm::value::Value;
use zeroship_migrate::schema::query::{FkEmission, IndexSpec, QueryError};

#[allow(dead_code)]
pub fn fixture_table_sql(
    schema: &SchemaName,
    collection: &str,
    fields: &Value,
    fks: &FkEmission<'_>,
) -> Result<String, QueryError> {
    fixture_table_sql_for(
        schema,
        collection,
        fields,
        fks,
        &zeroship_migrate_postgres::DIALECT,
    )
}

#[allow(dead_code)]
pub fn fixture_table_sql_sqlite(
    schema: &SchemaName,
    collection: &str,
    fields: &Value,
    fks: &FkEmission<'_>,
) -> Result<String, QueryError> {
    fixture_table_sql_for(
        schema,
        collection,
        fields,
        fks,
        &zeroship_migrate_sqlite::DIALECT,
    )
}

fn fixture_table_sql_for(
    schema: &SchemaName,
    collection: &str,
    fields: &Value,
    fks: &FkEmission<'_>,
    dialect: &zeroship_migrate::DialectId,
) -> Result<String, QueryError> {
    let policy =
        zeroship_migrate_server::policy::ManagedPolicyConfig::default_confined([7u8; 32], 1)
            .expect("load the creator charter")
            .current_ceiling_for_schema(&zeroship_core::AppId::mint(), schema.as_str(), None)
            .expect("compose the creator charter")
            .policy;
    // The confined policy supplies assigned fields when compiling authored DDL.
    let authored = Value::Object(
        fields
            .as_object()
            .expect("fixture field map")
            .iter()
            .filter(|(_, definition)| definition.get("assign").is_none())
            .map(|(name, definition)| (name.clone(), definition.clone()))
            .collect(),
    );
    zeroship_migrate::schema::query::build_create_table_with_fks_for_dialect(
        zeroship_migrate::shipping_vendors(),
        schema.as_str(),
        collection,
        &serde_json::to_value(&authored).expect("encode migration descriptor"),
        fks,
        dialect,
        &policy,
    )
}

#[allow(dead_code)]
pub fn fixture_indexes(
    schema: &SchemaName,
    collection: &str,
    fields: &Value,
) -> Result<Vec<IndexSpec>, QueryError> {
    zeroship_migrate::schema::query::build_create_indexes(
        zeroship_migrate::shipping_vendors(),
        schema.as_str(),
        collection,
        &serde_json::to_value(fields).expect("encode migration descriptor"),
        &zeroship_migrate_postgres::DIALECT,
    )
}

#[allow(dead_code)]
pub fn fixture_schema_sql(schema: &SchemaName) -> String {
    format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        zeroship_data_orm::sql::mapping::quote_ident(schema.as_str())
    )
}

/// Add the fields emitted by the fixture's confined migration policy.
pub fn generated_fields(fields: Value) -> Value {
    let descriptor: Value = serde_json::from_str(include_str!(
        "../../../crates/zeroship-data-orm/tests/fixtures/schema.runtime.json"
    ))
    .expect("generated fixture descriptor");
    let mut generated = descriptor["collections"]["posts"]["fields"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, definition)| definition.get("assign").is_some())
        .map(|(name, definition)| (name.clone(), definition.clone()))
        .collect::<zeroship_data_orm::value::Map<_, _>>();
    for (name, definition) in fields.as_object().expect("fixture field map") {
        if let Some(existing) = generated.get_mut(name) {
            existing
                .as_object_mut()
                .unwrap()
                .extend(definition.as_object().unwrap().clone());
        } else {
            generated.insert(name.clone(), definition.clone());
        }
    }
    Value::Object(generated)
}
