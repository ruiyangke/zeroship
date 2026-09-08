//! Database fixtures use the migration engine that creates creator tables.

use serde_json::Value;
use zeroship_data_query_builder::{compile::SqlDialect, SchemaName};
use zeroship_migrate::schema::query::{FkEmission, IndexSpec, QueryError};

#[allow(dead_code)]
pub fn fixture_table_sql(
    schema: &SchemaName,
    collection: &str,
    fields: &Value,
    fks: &FkEmission<'_>,
) -> Result<String, QueryError> {
    fixture_table_sql_for(schema, collection, fields, fks, SqlDialect::Postgres)
}

pub fn fixture_table_sql_for(
    schema: &SchemaName,
    collection: &str,
    fields: &Value,
    fks: &FkEmission<'_>,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    let policy =
        zeroship_migrate_server::policy::ManagedPolicyConfig::default_confined([7u8; 32], 1)
            .expect("load the creator charter")
            .current_ceiling_for_app(&uuid::Uuid::nil(), None)
            .expect("compose the creator charter")
            .policy;
    let dialect = match dialect {
        SqlDialect::Postgres => &zeroship_migrate_postgres::DIALECT,
        SqlDialect::Sqlite => &zeroship_migrate_sqlite::DIALECT,
        SqlDialect::Mysql => panic!("the runtime has no MySQL fixture backend"),
    };
    zeroship_migrate::schema::query::build_create_table_with_fks_for_dialect(
        zeroship_migrate::shipping_vendors(),
        schema.as_str(),
        collection,
        fields,
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
        fields,
        &zeroship_migrate_postgres::DIALECT,
    )
}

#[allow(dead_code)]
pub fn fixture_schema_sql(schema: &SchemaName) -> String {
    format!("CREATE SCHEMA IF NOT EXISTS {}", schema.quoted())
}
