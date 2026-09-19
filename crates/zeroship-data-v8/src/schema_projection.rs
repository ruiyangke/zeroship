//! Project the decoded ORM schema onto the wire `FieldDef` JSON the host
//! JavaScript adapter consumes.
//!
//! The adapter's `installSchema` receives an already-DECODED projection: Rust
//! owns descriptor validation and normalization, and the JavaScript side builds
//! SDK `Collection` objects without re-validating, re-decoding or re-normalizing
//! fields. This module is the single place that knows the JS `FieldDef` wire
//! shape; the ORM's `ColumnSchema`/`CollectionSchema`/`FieldMap` stay free of it
//! (they derive no `Serialize`) and their decode behavior is untouched.
//!
//! The projection is not a re-serialization of the raw descriptor. It is built
//! from `zeroship_data_orm::schema::Schema` (the normalized form
//! `Schema::from_runtime_descriptor` already produces), so `field.normalize`
//! facets - `storage.valueColumn`, the masked raw column, `writable=false` on
//! assigned fields - are the values the adapter sees. The one facet the decoded
//! form changes shape on is a `bytes` default: decode turns the descriptor's
//! base64 string into `Value::Bytes`, and this projection emits it as a JSON
//! number array the adapter rehydrates into a fresh `Uint8Array` per insert.
//!
//! Named indexes are NOT part of the decoded `Schema` (it drops them), so they
//! are threaded from the raw descriptor.

use serde_json::{json, Map, Value as Json};
use zeroship_data_orm::schema::{ColumnSchema, LogicalType, MaskSchema, Schema, StorageMapping};
use zeroship_data_orm::value::Value;

/// Project `schema` onto `{ collections: { name: { fields, indexes } } }`.
///
/// `raw_descriptor` is the original v2 descriptor; each collection's `indexes`
/// array (default `[]`) is copied verbatim because the decoded schema does not
/// carry them.
pub fn project_schema(
    schema: &Schema,
    raw_descriptor: &serde_json::Value,
) -> Result<Json, String> {
    let raw_collections = raw_descriptor.get("collections").and_then(Json::as_object);
    let mut collections = Map::new();
    for (name, collection) in schema.collections() {
        let mut fields = Map::new();
        for (field_name, column) in collection.fields() {
            fields.insert(field_name.clone(), project_field(column)?);
        }
        let indexes = raw_collections
            .and_then(|collections| collections.get(name))
            .and_then(|collection| collection.get("indexes"))
            .cloned()
            .unwrap_or_else(|| Json::Array(Vec::new()));
        collections.insert(
            name.to_owned(),
            json!({ "fields": Json::Object(fields), "indexes": indexes }),
        );
    }
    Ok(json!({ "collections": Json::Object(collections) }))
}

/// The SDK `TypeName` a decoded logical type projects to.
///
/// `LogicalType::as_str` is the token for every variant except `Enum`: the
/// generator spells a materialized enum/domain column as its base scalar and the
/// SDK has no `"enum"` validator, so an enum projects as `"string"`.
fn type_token(logical: LogicalType) -> &'static str {
    match logical {
        LogicalType::Enum => "string",
        other => other.as_str(),
    }
}

/// The SDK `PrimitiveTypeName` an array's `items` projects to.
///
/// Distinct from [`type_token`]: the SDK array-element validator only knows the
/// primitive vocabulary, so a non-primitive element projects as `"json"` while
/// a timestamp element keeps the `"timestamp"` spelling the top-level token
/// uses.
fn item_token(logical: LogicalType) -> &'static str {
    match logical {
        LogicalType::Text | LogicalType::Enum => "string",
        LogicalType::Integer | LogicalType::BigInt | LogicalType::Number => "number",
        LogicalType::Boolean => "boolean",
        LogicalType::Timestamp => "timestamp",
        LogicalType::CalendarDate => "calendarDate",
        LogicalType::Json
        | LogicalType::Object
        | LogicalType::Array
        | LogicalType::Union
        | LogicalType::Time
        | LogicalType::Bytes
        | LogicalType::Vector
        | LogicalType::GeoPoint
        | LogicalType::Literal => "json",
    }
}

/// A `bytes` default round-trips through decode as `Value::Bytes`; every other
/// default keeps its JSON spelling.
fn project_default(default: &Value) -> Result<Json, String> {
    match default {
        Value::Bytes(bytes) => Ok(Json::Array(
            bytes
                .iter()
                .map(|byte| Json::from(u64::from(*byte)))
                .collect(),
        )),
        other => serde_json::to_value(other).map_err(|error| error.to_string()),
    }
}

fn project_field(column: &ColumnSchema) -> Result<Json, String> {
    let mut out = Map::new();
    out.insert("type".into(), Json::String(type_token(column.logical_type).into()));
    out.insert("required".into(), Json::Bool(column.required));
    out.insert("readable".into(), Json::Bool(column.readable));
    out.insert("filterable".into(), Json::Bool(column.filterable));
    out.insert("sortable".into(), Json::Bool(column.sortable));
    out.insert("projectable".into(), Json::Bool(column.projectable));
    out.insert("writable".into(), Json::Bool(column.writable));

    if column.primary_key {
        out.insert("primaryKey".into(), Json::Bool(true));
    }
    if column.unique {
        out.insert("unique".into(), Json::Bool(true));
    }
    if column.soft_delete {
        out.insert("softDelete".into(), Json::Bool(true));
    }
    if column.concurrency {
        out.insert("concurrency".into(), Json::Bool(true));
    }
    if column.encrypted {
        out.insert("encrypted".into(), Json::Bool(true));
    }
    if column.deferrable {
        out.insert("deferrable".into(), Json::Bool(true));
    }
    if let Some(default) = &column.default {
        out.insert("default".into(), project_default(default)?);
    }
    if let Some(assignment) = &column.assignment {
        out.insert(
            "assign".into(),
            serde_json::to_value(assignment).map_err(|error| error.to_string())?,
        );
    }
    if let Some(min) = &column.min {
        out.insert(
            "min".into(),
            serde_json::to_value(min).map_err(|error| error.to_string())?,
        );
    }
    if let Some(max) = &column.max {
        out.insert(
            "max".into(),
            serde_json::to_value(max).map_err(|error| error.to_string())?,
        );
    }
    if !column.enum_values.is_empty() {
        out.insert(
            "enum".into(),
            serde_json::to_value(&column.enum_values).map_err(|error| error.to_string())?,
        );
    }
    if let Some(precision) = column.precision {
        out.insert("precision".into(), Json::from(precision));
    }
    if let Some(scale) = column.scale {
        out.insert("scale".into(), Json::from(scale));
    }
    if let Some(reference) = &column.reference {
        out.insert("refTarget".into(), Json::String(reference.collection.clone()));
        out.insert("refColumn".into(), Json::String(reference.column.clone()));
        if let Some(name) = &reference.name {
            out.insert("relation".into(), Json::String(name.clone()));
        }
    }
    if let Some(action) = &column.on_delete {
        out.insert("onDelete".into(), Json::String(action.clone()));
    }
    if let Some(action) = &column.on_update {
        out.insert("onUpdate".into(), Json::String(action.clone()));
    }
    if let Some(prefix) = &column.id_prefix {
        out.insert("idPrefix".into(), Json::String(prefix.clone()));
    }
    if let Some(dims) = column.vector_dims {
        out.insert("vectorDims".into(), Json::from(dims as u64));
    }
    if column.logical_type == LogicalType::Vector {
        out.insert(
            "vectorMetric".into(),
            Json::String(
                match column.vector_metric {
                    zeroship_data_orm::schema::VectorMetric::Cosine => "cosine",
                    zeroship_data_orm::schema::VectorMetric::L2 => "l2",
                    zeroship_data_orm::schema::VectorMetric::InnerProduct => "innerProduct",
                }
                .into(),
            ),
        );
    }
    if let Some(mask) = &column.mask {
        out.insert("mask".into(), project_mask(mask));
    }
    if let Some(items) = column.items {
        out.insert("items".into(), Json::String(item_token(items).into()));
    }
    if !column.shape.is_empty() {
        out.insert("shape".into(), project_fields(&column.shape)?);
    }
    if !column.variants.is_empty() {
        let variants = column
            .variants
            .iter()
            .map(|variant| project_fields(variant))
            .collect::<Result<Vec<_>, _>>()?;
        out.insert("variants".into(), Json::Array(variants));
    }
    if let Some(discriminator) = &column.discriminator {
        out.insert("discriminator".into(), Json::String(discriminator.clone()));
    }
    if let Some(literal) = &column.literal_value {
        out.insert(
            "literalValue".into(),
            serde_json::to_value(literal).map_err(|error| error.to_string())?,
        );
    }
    out.insert("storage".into(), project_storage(&column.storage));
    Ok(Json::Object(out))
}

fn project_fields(fields: &zeroship_data_orm::schema::FieldMap) -> Result<Json, String> {
    let mut out = Map::new();
    for (name, column) in fields {
        out.insert(name.clone(), project_field(column)?);
    }
    Ok(Json::Object(out))
}

fn project_mask(mask: &MaskSchema) -> Json {
    json!({ "kind": mask.kind, "classification": mask.classification })
}

fn project_storage(storage: &StorageMapping) -> Json {
    let mut out = Map::new();
    if let Some(value_column) = &storage.value_column {
        out.insert("valueColumn".into(), Json::String(value_column.clone()));
    }
    if let Some(raw_column) = &storage.raw_column {
        out.insert("rawColumn".into(), Json::String(raw_column.clone()));
    }
    if storage.raw_filterable {
        out.insert("rawFilterable".into(), Json::Bool(true));
    }
    if storage.raw_sortable {
        out.insert("rawSortable".into(), Json::Bool(true));
    }
    if storage.raw_projectable {
        out.insert("rawProjectable".into(), Json::Bool(true));
    }
    Json::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A round trip through decode then projection must reproduce the field
    /// facets the adapter relies on: type, required, primaryKey, refTarget,
    /// storage.valueColumn, and default. This is the regression control for the
    /// hand-off - if the projection drops a facet the SDK may no longer validate
    /// or dispatch the field.
    #[test]
    fn projection_reproduces_decoded_field_facets() {
        let descriptor = json!({
            "version": 2,
            "collections": {
                "posts": {
                    "fields": {
                        "id": {
                            "type": "string",
                            "required": true,
                            "primaryKey": true,
                            "idPrefix": "post"
                        },
                        "author_id": {
                            "type": "ref",
                            "required": true,
                            "refTarget": "users",
                            "refColumn": "id",
                            "relation": "author"
                        },
                        "title": { "type": "string", "default": "untitled" },
                        "weight": { "type": "number", "default": 1 },
                        "instants": { "type": "array", "items": "timestamp" }
                    },
                    "options": { "softDelete": false, "versioning": false },
                    "indexes": [
                        { "name": "posts_author_idx", "fields": ["author_id"] }
                    ]
                },
                "users": {
                    "fields": {
                        "id": { "type": "string", "required": true, "primaryKey": true }
                    },
                    "options": { "softDelete": false, "versioning": false },
                    "indexes": []
                }
            }
        });

        let schema = Schema::from_runtime_descriptor(&Value::from(descriptor.clone()))
            .expect("descriptor decodes");
        let projection = project_schema(&schema, &descriptor).expect("schema projects");

        let posts = &projection["collections"]["posts"];
        assert_eq!(posts["fields"]["id"]["type"], json!("string"));
        assert_eq!(posts["fields"]["id"]["required"], json!(true));
        assert_eq!(posts["fields"]["id"]["primaryKey"], json!(true));
        assert_eq!(posts["fields"]["id"]["idPrefix"], json!("post"));
        assert_eq!(posts["fields"]["id"]["storage"]["valueColumn"], json!("id"));

        // The decoded relation survives as the ref facets the SDK resolves joins by.
        assert_eq!(posts["fields"]["author_id"]["refTarget"], json!("users"));
        assert_eq!(posts["fields"]["author_id"]["refColumn"], json!("id"));
        assert_eq!(posts["fields"]["author_id"]["relation"], json!("author"));

        // Defaults are carried, not dropped.
        assert_eq!(posts["fields"]["title"]["default"], json!("untitled"));
        assert_eq!(posts["fields"]["weight"]["default"], json!(1));

        // An array's `items` uses the primitive vocabulary the SDK array-element
        // validator knows, so a timestamp element stays `timestamp`.
        assert_eq!(posts["fields"]["instants"]["type"], json!("array"));
        assert_eq!(posts["fields"]["instants"]["items"], json!("timestamp"));

        // `indexes` are absent from the decoded schema and come from the raw
        // descriptor, so their preservation is asserted explicitly.
        assert_eq!(
            posts["indexes"],
            json!([{ "name": "posts_author_idx", "fields": ["author_id"] }])
        );
        assert_eq!(projection["collections"]["users"]["indexes"], json!([]));

        // The projection itself is schema-only: no leftover version/options.
        assert!(projection.get("version").is_none());
        assert!(posts.get("options").is_none());
    }

    /// A binary default decodes from base64 to `Value::Bytes`; the projection
    /// must emit it as a number array (the adapter rehydrates a fresh
    /// `Uint8Array` per insert) rather than dropping it or re-base64-encoding it.
    #[test]
    fn binary_default_projects_as_number_array() {
        let descriptor = json!({
            "version": 2,
            "collections": {
                "blobs": {
                    "fields": {
                        "payload": { "type": "bytes", "required": true, "default": "AP8=" }
                    },
                    "indexes": []
                }
            }
        });
        let schema = Schema::from_runtime_descriptor(&Value::from(descriptor.clone()))
            .expect("descriptor decodes");
        let projection = project_schema(&schema, &descriptor).expect("schema projects");
        assert_eq!(
            projection["collections"]["blobs"]["fields"]["payload"]["default"],
            json!([0, 255])
        );
    }
}
