use super::*;

fn identity(name: &str, fields: &FieldMap) -> Result<(), DbError> {
    let invalid = |message: &str| {
        DbError::validation("invalid_collection_identity", format!("{name}: {message}"))
    };
    let id = fields
        .get("id")
        .ok_or_else(|| invalid("collection requires an 'id' primary key"))?;
    if !id.primary_key {
        return Err(invalid(
            "collection 'id' must be declared as its primary key",
        ));
    }
    if !id.required {
        return Err(invalid("collection 'id' must be required and non-null"));
    }
    if id.logical_type != LogicalType::Text && !id.logical_type.is_integer() {
        return Err(invalid("collection 'id' must use text or integer storage"));
    }
    if id.encrypted || id.is_masked() {
        return Err(invalid("collection 'id' cannot be encrypted or masked"));
    }
    if id
        .assignment
        .as_ref()
        .is_some_and(|assignment| assignment.on != AssignmentEvent::Insert)
    {
        return Err(invalid("collection 'id' can only be assigned on insertion"));
    }
    if fields
        .iter()
        .any(|(name, column)| name != "id" && column.primary_key)
    {
        return Err(invalid("collection 'id' must be its sole primary key"));
    }
    Ok(())
}

pub(super) fn collection(name: &str, schema: &CollectionSchema) -> Result<(), DbError> {
    if let Some(field) = &schema.duplicate_field {
        return Err(invalid(format!("duplicate field '{name}.{field}'")));
    }
    identity(name, schema)?;
    let mut relation_names = HashSet::new();
    let mut physical_columns = HashSet::new();
    let mut soft_delete = false;
    let mut concurrency = false;
    for (field, column) in schema.fields() {
        crate::sql::mapping::validate_field_name(field)?;
        validate_column(field, column, 0)?;
        let value_column = column.storage.value_column.as_deref().unwrap_or(field);
        let raw_column = crate::sql::mapping::declared_raw_column(field, column)?;
        for physical in std::iter::once(value_column).chain(raw_column.as_deref()) {
            if !physical_columns.insert(physical.to_owned()) {
                return Err(invalid(format!(
                    "collection '{name}' has conflicting physical column mappings"
                )));
            }
        }
        if column.soft_delete && std::mem::replace(&mut soft_delete, true)
            || column.concurrency && std::mem::replace(&mut concurrency, true)
        {
            return Err(DbError::validation(
                "invalid_column_role",
                "operation requires an unambiguous column role",
            ));
        }
        if let Some(reference) = &column.reference {
            crate::sql::mapping::validate_collection(&reference.collection)?;
            crate::sql::mapping::validate_field_name(&reference.column)?;
            if let Some(relation) = &reference.name {
                if crate::sql::Ident::parse_as(relation, crate::sql::IdentRole::Alias).is_err()
                    || relation.starts_with('_')
                    || matches!(relation.as_str(), "__proto__" | "constructor" | "prototype")
                {
                    return Err(DbError::validation(
                        "invalid_relation",
                        "invalid relation name",
                    ));
                }
                if schema.contains_key(relation) || !relation_names.insert(relation) {
                    return Err(DbError::validation(
                        "invalid_relation",
                        "relation name collides with a column or another relation",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_column(name: &str, column: &ColumnSchema, depth: usize) -> Result<(), DbError> {
    if depth > crate::sql::codecs::MAX_JSON_DEPTH {
        return Err(invalid("schema nesting exceeds the supported depth"));
    }
    if column.storage.array == ArrayStorage::Native
        && (depth > 0
            || column.logical_type != LogicalType::Array
            || column.items != Some(LogicalType::Text)
            || column.encrypted
            || column.is_masked())
    {
        return Err(invalid(format!(
            "native array storage for '{name}' requires an unprotected top-level text array"
        )));
    }
    if let Some(assignment) = &column.assignment {
        let valid = match assignment.by {
            AssignmentGenerator::Now => column.logical_type == LogicalType::Timestamp,
            AssignmentGenerator::Actor => column.logical_type == LogicalType::Text,
            AssignmentGenerator::TypedId => {
                assignment.on == AssignmentEvent::Insert && column.logical_type == LogicalType::Text
            }
            AssignmentGenerator::Identity => {
                assignment.on == AssignmentEvent::Insert && column.logical_type.is_integer()
            }
            AssignmentGenerator::Increment(_) => {
                assignment.on == AssignmentEvent::Write && column.logical_type.is_integer()
            }
        };
        if !valid {
            return Err(DbError::validation(
                "invalid_assignment",
                format!("generator does not match the type or event of '{name}'"),
            ));
        }
    }
    if column.soft_delete
        && !matches!(
            column.assignment,
            Some(Assignment {
                by: AssignmentGenerator::Now,
                on: AssignmentEvent::Delete,
            })
        )
    {
        return Err(DbError::validation(
            "invalid_column_role",
            format!("'{name}' requires a matching generator for softDelete"),
        ));
    }
    if column.concurrency
        && !matches!(
            column.assignment,
            Some(Assignment {
                by: AssignmentGenerator::Increment(_),
                on: AssignmentEvent::Write,
            })
        )
    {
        return Err(DbError::validation(
            "invalid_column_role",
            format!("'{name}' requires a matching generator for concurrency"),
        ));
    }
    if column.encrypted {
        if !matches!(
            column.logical_type,
            LogicalType::Text | LogicalType::Number | LogicalType::Bytes
        ) {
            return Err(DbError::validation(
                "encrypted_type_unsupported",
                "encrypted field type must be string, number, or bytes",
            ));
        }
        if column.unique {
            return Err(DbError::validation(
                "encrypted_unique_unsupported",
                "encrypted fields cannot be unique",
            ));
        }
    }
    if let Some(precision) = column.precision {
        if column.logical_type != LogicalType::Number {
            return Err(invalid("fixed precision requires a numeric column"));
        }
        crate::sql::statement::DecimalStorage::new(precision, column.scale.unwrap_or(0))
            .map_err(|error| DbError::validation("invalid_numeric_metadata", error.to_string()))?;
    } else if column.scale.is_some() {
        return Err(invalid("numeric scale requires fixed precision"));
    }
    if column.logical_type == LogicalType::Vector
        && column.vector_dims.is_none_or(|dimensions| dimensions == 0)
    {
        return Err(invalid("vector columns require positive dimensions"));
    }
    if column.logical_type == LogicalType::Array
        && !matches!(
            column.items,
            Some(
                LogicalType::Text
                    | LogicalType::Number
                    | LogicalType::Boolean
                    | LogicalType::Timestamp
                    | LogicalType::CalendarDate
                    | LogicalType::Json
            )
        )
    {
        return Err(invalid("array columns require a supported item type"));
    }
    if let Some(value_column) = &column.storage.value_column {
        crate::sql::Ident::parse_as(value_column, crate::sql::IdentRole::StoredColumn)
            .map_err(|_| invalid("invalid physical value column"))?;
    }
    if column.is_masked() {
        crate::sql::mapping::declared_raw_column(name, column)?;
        if column.storage.raw_filterable
            || column.storage.raw_sortable
            || column.storage.raw_projectable
        {
            return Err(invalid(
                "protected raw storage cannot be exposed as a field",
            ));
        }
    }
    for (field, definition) in &column.shape {
        validate_column(field, definition, depth + 1)?;
    }
    for variant in &column.variants {
        for (field, definition) in variant {
            validate_column(field, definition, depth + 1)?;
        }
    }
    if column.logical_type == LogicalType::Union {
        let discriminator = column
            .discriminator
            .as_ref()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| invalid("union columns require a discriminator"))?;
        if column.variants.is_empty() {
            return Err(invalid(
                "union variants require literal discriminator values",
            ));
        }
        let mut tags = Vec::new();
        for variant in &column.variants {
            let tag = variant
                .get(discriminator)
                .filter(|column| column.logical_type == LogicalType::Literal)
                .and_then(|column| column.literal_value.as_ref())
                .ok_or_else(|| invalid("union variants require literal discriminator values"))?;
            if tags.contains(&tag) {
                return Err(invalid("union discriminator values must be unique"));
            }
            tags.push(tag);
        }
    }
    if let Some(default) = &column.default {
        validate_default(name, column, default)?;
    }
    Ok(())
}

fn validate_default(name: &str, column: &ColumnSchema, default: &Value) -> Result<(), DbError> {
    let invalid_default = || invalid(format!("invalid default for field '{name}'"));
    if default.is_null() {
        return if column.required {
            Err(invalid_default())
        } else {
            Ok(())
        };
    }
    let mut prepared = default.clone();
    crate::sql::codecs::prepare_value(name, column, &mut prepared)
        .map_err(|_| invalid_default())?;
    let valid = match column.logical_type {
        LogicalType::Text | LogicalType::Time => matches!(prepared, Value::String(_)),
        LogicalType::Integer | LogicalType::BigInt => {
            matches!(prepared, Value::Number(number) if number.as_i64().is_some())
        }
        LogicalType::Number => match prepared {
            Value::Number(number) => number.as_i64().is_some() || number.is_f64(),
            Value::Decimal(decimal) => crate::sql::decimal::valid(&decimal),
            _ => false,
        },
        LogicalType::Bytes => matches!(prepared, Value::Bytes(_)),
        LogicalType::Boolean
        | LogicalType::Timestamp
        | LogicalType::CalendarDate
        | LogicalType::Json
        | LogicalType::Object
        | LogicalType::Array
        | LogicalType::Union
        | LogicalType::Vector
        | LogicalType::GeoPoint
        | LogicalType::Enum
        | LogicalType::Literal => true,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid_default())
    }
}

pub(super) fn relations(schema: &Schema) -> Result<(), DbError> {
    for (_, collection) in schema.collections() {
        for column in collection.values() {
            let Some(reference) = &column.reference else {
                continue;
            };
            let target = schema
                .collections()
                .find(|(name, _)| *name == reference.collection)
                .map(|(_, fields)| fields)
                .ok_or_else(|| {
                    DbError::validation(
                        "invalid_relation",
                        "reference target collection is not declared",
                    )
                })?;
            let target_column = target.get(&reference.column).ok_or_else(|| {
                DbError::validation(
                    "invalid_relation",
                    "reference target column is not declared",
                )
            })?;
            if reference.name.is_none() {
                continue;
            }
            let compatible = column.logical_type == LogicalType::Text
                && target_column.logical_type == LogicalType::Text
                || column.logical_type.is_integer() && target_column.logical_type.is_integer();
            if !compatible {
                return Err(DbError::validation(
                    "invalid_relation",
                    "reference keys must use compatible text or integer types",
                ));
            }
            if !target_column.primary_key && !target_column.unique {
                return Err(DbError::validation(
                    "invalid_relation",
                    "a forward reference must target a unique column",
                ));
            }
        }
    }
    Ok(())
}
