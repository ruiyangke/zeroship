//! Value descriptors shared by runtime catalog readers and storage backends.

/// ORM collections declare a non-null `id` as their sole primary key.
pub fn validate_collection_identity(schema: &crate::value::Value) -> Result<(), &'static str> {
    use crate::value::Value;
    let fields = schema
        .as_object()
        .ok_or("collection fields must be an object")?;
    let id = fields
        .get("id")
        .ok_or("collection requires an 'id' primary key")?;
    if id.get("primaryKey").and_then(Value::as_bool) != Some(true) {
        return Err("collection 'id' must be declared as its primary key");
    }
    if id.get("required").and_then(Value::as_bool) != Some(true) {
        return Err("collection 'id' must be required and non-null");
    }
    if !matches!(
        id.get("type").and_then(Value::as_str),
        Some("string" | "text" | "id" | "integer" | "int" | "bigint" | "bigInt")
    ) {
        return Err("collection 'id' must use text or integer storage");
    }
    if is_encrypted(id) || effective_mask(id).is_some() {
        return Err("collection 'id' cannot be encrypted or masked");
    }
    if id
        .get("assign")
        .is_some_and(|assignment| assignment.get("on").and_then(Value::as_str) != Some("insert"))
    {
        return Err("collection 'id' can only be assigned on insertion");
    }
    if fields.iter().any(|(name, def)| {
        name != "id" && def.get("primaryKey").and_then(Value::as_bool) == Some(true)
    }) {
        return Err("collection 'id' must be its sole primary key");
    }
    Ok(())
}

/// Projectable fields explicitly declared by the generated schema.
pub fn readable_fields(schema: &crate::value::Value) -> std::collections::BTreeSet<String> {
    schema
        .as_object()
        .into_iter()
        .flat_map(|fields| fields.iter())
        .filter(|(name, definition)| {
            !crate::sql::mapping::is_schema_metadata_key(name)
                && definition.is_object()
                && definition
                    .get("readable")
                    .and_then(crate::value::Value::as_bool)
                    != Some(false)
                && definition
                    .get("projectable")
                    .and_then(crate::value::Value::as_bool)
                    != Some(false)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Whether the field uses encrypted binary storage.
pub fn is_encrypted(field: &crate::value::Value) -> bool {
    field
        .get("encrypted")
        .and_then(crate::value::Value::as_bool)
        == Some(true)
}

/// Effective masking metadata from an installed field descriptor.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveMask<'a> {
    pub kind: &'a str,
    pub classification: &'a str,
}

/// An absent mask or explicit `kind: "none"` leaves the field unmasked.
pub fn effective_mask(field: &crate::value::Value) -> Option<EffectiveMask<'_>> {
    let metadata = field.get("mask")?.as_object()?;
    let kind = metadata
        .get("kind")
        .and_then(crate::value::Value::as_str)
        .unwrap_or("full");
    (kind != "none").then(|| EffectiveMask {
        kind,
        classification: metadata
            .get("classification")
            .and_then(crate::value::Value::as_str)
            .unwrap_or("pii"),
    })
}

#[derive(Clone, Copy)]
pub(crate) enum PredicateOperator {
    Equality,
    Ordering,
    Pattern,
}

pub(crate) fn supports_predicate_operator(
    field: &crate::value::Value,
    operator: PredicateOperator,
) -> bool {
    let kind = field.get("type").and_then(crate::value::Value::as_str);
    match operator {
        PredicateOperator::Equality => matches!(
            kind,
            Some(
                "string"
                    | "text"
                    | "id"
                    | "ref"
                    | "enum"
                    | "boolean"
                    | "bool"
                    | "integer"
                    | "int"
                    | "bigint"
                    | "bigInt"
                    | "number"
                    | "float"
                    | "double"
                    | "decimal"
                    | "bytes"
                    | "date"
                    | "timestamp"
                    | "timestamptz"
                    | "calendarDate"
                    | "time"
                    | "json"
                    | "object"
                    | "array"
                    | "union"
            )
        ),
        PredicateOperator::Ordering => effective_mask(field).is_none() && supports_sorting(field),
        PredicateOperator::Pattern => {
            matches!(kind, Some("string" | "text" | "id" | "ref" | "enum"))
        }
    }
}

pub(crate) fn supports_sorting(field: &crate::value::Value) -> bool {
    effective_mask(field).is_some()
        || matches!(
            field.get("type").and_then(crate::value::Value::as_str),
            Some(
                "string"
                    | "text"
                    | "id"
                    | "ref"
                    | "enum"
                    | "integer"
                    | "int"
                    | "bigint"
                    | "bigInt"
                    | "number"
                    | "float"
                    | "double"
                    | "date"
                    | "timestamp"
                    | "timestamptz"
                    | "calendarDate"
                    | "time"
            )
        )
}

pub(crate) fn supports_grouping(field: &crate::value::Value) -> bool {
    effective_mask(field).is_some()
        || matches!(
            field.get("type").and_then(crate::value::Value::as_str),
            Some(
                "string"
                    | "text"
                    | "id"
                    | "ref"
                    | "enum"
                    | "boolean"
                    | "bool"
                    | "integer"
                    | "int"
                    | "bigint"
                    | "bigInt"
                    | "number"
                    | "float"
                    | "double"
                    | "bytes"
                    | "date"
                    | "timestamp"
                    | "timestamptz"
                    | "calendarDate"
                    | "time"
            )
        )
}

/// Distance metric selected by a vector field descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorMetric {
    /// Cosine distance.
    Cosine,
    /// Euclidean distance.
    L2,
    /// Negative inner product.
    InnerProduct,
}

/// A geographic point in WGS84 coordinates.
#[derive(Debug, Clone, Copy)]
pub struct GeoPoint {
    pub lat: f64,
    pub lng: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_sorting_exposes_only_portable_ordered_values() {
        for kind in [
            "string",
            "id",
            "ref",
            "integer",
            "bigInt",
            "number",
            "date",
            "calendarDate",
            "time",
        ] {
            assert!(supports_sorting(&crate::value!({"type": kind})), "{kind}");
        }
        for kind in ["boolean", "decimal", "bytes", "json", "vector", "geoPoint"] {
            assert!(!supports_sorting(&crate::value!({"type": kind})), "{kind}");
        }
        assert!(supports_sorting(
            &crate::value!({"type":"decimal", "mask":{"kind":"full"}})
        ));
    }

    #[test]
    fn descriptor_grouping_exposes_only_portable_equality_values() {
        for kind in [
            "string",
            "id",
            "ref",
            "boolean",
            "integer",
            "bigInt",
            "number",
            "bytes",
            "date",
            "calendarDate",
            "time",
        ] {
            assert!(supports_grouping(&crate::value!({"type": kind})), "{kind}");
        }
        for kind in [
            "decimal", "json", "object", "array", "union", "vector", "geoPoint",
        ] {
            assert!(!supports_grouping(&crate::value!({"type": kind})), "{kind}");
        }
        assert!(supports_grouping(
            &crate::value!({"type":"json", "mask":{"kind":"full"}})
        ));
    }
}
