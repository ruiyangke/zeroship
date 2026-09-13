//! SQL capabilities derived from canonical model metadata.

pub use crate::schema::VectorMetric;
use crate::schema::{ColumnSchema, FieldMap, LogicalType};

/// Resolve named forward edges without inferring names from columns or tables.
pub(crate) fn relation_fields(fields: &FieldMap) -> std::collections::BTreeMap<&str, &str> {
    fields
        .iter()
        .filter_map(|(field, definition)| {
            let name = definition.reference.as_ref()?.name.as_deref()?;
            Some((name, field.as_str()))
        })
        .collect()
}

/// Projectable fields explicitly declared by the model.
pub fn readable_fields(schema: &FieldMap) -> std::collections::BTreeSet<String> {
    schema
        .iter()
        .filter(|(_, field)| field.readable && field.projectable)
        .map(|(name, _)| name.clone())
        .collect()
}

/// Whether the field uses encrypted binary storage.
pub fn is_encrypted(field: &ColumnSchema) -> bool {
    field.encrypted
}

#[derive(Debug, Clone, Copy)]
pub struct EffectiveMask<'a> {
    pub kind: &'a str,
    pub classification: &'a str,
}

/// An absent mask or explicit `none` leaves the field unmasked.
pub fn effective_mask(field: &ColumnSchema) -> Option<EffectiveMask<'_>> {
    let mask = field.mask.as_ref()?;
    (mask.kind != "none").then_some(EffectiveMask {
        kind: &mask.kind,
        classification: &mask.classification,
    })
}

#[derive(Clone, Copy)]
pub(crate) enum PredicateOperator {
    Equality,
    Ordering,
    Pattern,
}

pub(crate) fn is_exact_decimal(field: &ColumnSchema) -> bool {
    field.logical_type == LogicalType::Number && field.precision.is_some()
}

pub(crate) fn supports_predicate_operator(
    field: &ColumnSchema,
    operator: PredicateOperator,
) -> bool {
    use LogicalType::*;
    match operator {
        PredicateOperator::Equality => matches!(
            field.logical_type,
            Text | Enum
                | Boolean
                | Integer
                | BigInt
                | Number
                | Bytes
                | Timestamp
                | CalendarDate
                | Time
                | Json
                | Object
                | Array
                | Union
        ),
        PredicateOperator::Ordering => effective_mask(field).is_none() && supports_sorting(field),
        PredicateOperator::Pattern => matches!(field.logical_type, Text | Enum),
    }
}

fn ordered_scalar(kind: LogicalType) -> bool {
    use LogicalType::*;
    matches!(
        kind,
        Text | Enum | Integer | BigInt | Number | Timestamp | CalendarDate | Time
    )
}

pub(crate) fn supports_sorting(field: &ColumnSchema) -> bool {
    effective_mask(field).is_some()
        || (!is_exact_decimal(field) && ordered_scalar(field.logical_type))
}

pub(crate) fn supports_grouping(field: &ColumnSchema) -> bool {
    effective_mask(field).is_some()
        || (!is_exact_decimal(field)
            && (ordered_scalar(field.logical_type)
                || matches!(
                    field.logical_type,
                    LogicalType::Boolean | LogicalType::Bytes
                )))
}

pub(crate) fn supports_aggregate(
    field: &ColumnSchema,
    function: crate::sql::AggregateFunc,
) -> bool {
    use crate::sql::AggregateFunc;
    match function {
        AggregateFunc::Count => true,
        AggregateFunc::Sum | AggregateFunc::Avg => {
            !is_exact_decimal(field)
                && matches!(
                    field.logical_type,
                    LogicalType::Integer | LogicalType::BigInt | LogicalType::Number
                )
        }
        AggregateFunc::Min | AggregateFunc::Max => {
            !is_exact_decimal(field) && ordered_scalar(field.logical_type)
        }
    }
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
    use crate::schema::MaskSchema;

    #[test]
    fn schema_sorting_exposes_only_portable_ordered_values() {
        use LogicalType::*;
        for kind in [Text, Integer, BigInt, Number, Timestamp, CalendarDate, Time] {
            assert!(supports_sorting(&ColumnSchema::new(kind)), "{kind:?}");
        }
        for kind in [Boolean, Bytes, Json, Vector, GeoPoint] {
            assert!(!supports_sorting(&ColumnSchema::new(kind)), "{kind:?}");
        }
        let mut decimal = ColumnSchema::new(Number);
        decimal.precision = Some(18);
        decimal.scale = Some(2);
        assert!(!supports_sorting(&decimal));
        decimal.mask = Some(MaskSchema {
            kind: "full".into(),
            classification: "pii".into(),
        });
        assert!(supports_sorting(&decimal));
    }

    #[test]
    fn schema_grouping_exposes_only_portable_equality_values() {
        use LogicalType::*;
        for kind in [
            Text,
            Boolean,
            Integer,
            BigInt,
            Number,
            Bytes,
            Timestamp,
            CalendarDate,
            Time,
        ] {
            assert!(supports_grouping(&ColumnSchema::new(kind)), "{kind:?}");
        }
        for kind in [Json, Object, Array, Union, Vector, GeoPoint] {
            assert!(!supports_grouping(&ColumnSchema::new(kind)), "{kind:?}");
        }
        let mut decimal = ColumnSchema::new(Number);
        decimal.precision = Some(18);
        decimal.scale = Some(2);
        assert!(!supports_grouping(&decimal));
        decimal.mask = Some(MaskSchema {
            kind: "full".into(),
            classification: "pii".into(),
        });
        assert!(supports_grouping(&decimal));
    }
}
