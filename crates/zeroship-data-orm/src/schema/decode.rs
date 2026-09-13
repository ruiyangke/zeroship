use super::*;

fn text(value: &Value, key: &str) -> Result<Option<String>, DbError> {
    value
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("schema property '{key}' must be a string")))
        })
        .transpose()
}

fn flag(value: &Value, key: &str, default: bool) -> Result<bool, DbError> {
    value
        .get(key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| invalid(format!("schema property '{key}' must be boolean")))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn unsigned(value: &Value, key: &str) -> Result<Option<u64>, DbError> {
    value
        .get(key)
        .map(|value| {
            value.as_u64().ok_or_else(|| {
                invalid(format!(
                    "schema property '{key}' must be an unsigned integer"
                ))
            })
        })
        .transpose()
}

fn number(value: &Value, key: &str) -> Result<Option<Number>, DbError> {
    value
        .get(key)
        .map(|value| match value {
            Value::Number(value) => Ok(value.clone()),
            _ => Err(invalid(format!("schema property '{key}' must be numeric"))),
        })
        .transpose()
}

fn logical_type(value: &str) -> Result<LogicalType, DbError> {
    Ok(match value {
        "string" | "text" | "id" | "ref" | "actor" => LogicalType::Text,
        "integer" | "int" => LogicalType::Integer,
        "bigInt" | "bigint" => LogicalType::BigInt,
        "number" | "float" | "double" => LogicalType::Number,
        "boolean" | "bool" => LogicalType::Boolean,
        "bytes" => LogicalType::Bytes,
        "date" | "timestamp" | "timestamptz" => LogicalType::Timestamp,
        "calendarDate" => LogicalType::CalendarDate,
        "time" => LogicalType::Time,
        "json" => LogicalType::Json,
        "object" => LogicalType::Object,
        "array" => LogicalType::Array,
        "union" => LogicalType::Union,
        "vector" => LogicalType::Vector,
        "geoPoint" => LogicalType::GeoPoint,
        "enum" => LogicalType::Enum,
        "literal" => LogicalType::Literal,
        _ => return Err(invalid("unsupported logical column type")),
    })
}

fn fields(value: &Value, depth: usize) -> Result<FieldMap, DbError> {
    if depth > crate::sql::codecs::MAX_JSON_DEPTH {
        return Err(invalid("schema nesting exceeds the supported depth"));
    }
    let object = value
        .as_object()
        .ok_or_else(|| invalid("collection fields must be an object"))?;
    let mut result = FieldMap::new();
    for (name, value) in object {
        if depth == 0 && crate::sql::mapping::is_schema_metadata_key(name) {
            continue;
        }
        let mut column = column(value, depth)?;
        column.normalize(name);
        result.insert(name.clone(), column);
    }
    Ok(result)
}

fn column(value: &Value, depth: usize) -> Result<ColumnSchema, DbError> {
    if !value.is_object() {
        return Err(invalid("field descriptor must be an object"));
    }
    let kind =
        text(value, "type")?.ok_or_else(|| invalid("field descriptor requires a logical type"))?;
    let mut field = ColumnSchema::new(logical_type(&kind)?);
    field.required = flag(value, "required", false)?;
    field.primary_key = flag(value, "primaryKey", false)?;
    field.unique = flag(value, "unique", false)?;
    field.readable = flag(value, "readable", true)?;
    field.projectable = flag(value, "projectable", true)?;
    field.filterable = flag(value, "filterable", true)?;
    field.sortable = flag(value, "sortable", true)?;
    field.aggregateable = flag(value, "aggregateable", true)?;
    field.writable = flag(value, "writable", true)?;
    field.soft_delete = flag(value, "softDelete", false)?;
    field.concurrency = flag(value, "concurrency", false)?;
    field.encrypted = flag(value, "encrypted", false)?;
    field.case_sensitive = flag(value, "caseSensitive", true)?;
    field.deferrable = flag(value, "deferrable", false)?;
    field.id_prefix = text(value, "idPrefix")?;
    field.on_delete = text(value, "onDelete")?;
    field.on_update = text(value, "onUpdate")?;
    field.format = text(value, "format")?;
    field.pattern = text(value, "pattern")?;
    field.max_length = unsigned(value, "maxLength")?;
    field.char_len = unsigned(value, "charLen")?;
    field.min = number(value, "min")?;
    field.max = number(value, "max")?;
    field.precision = unsigned(value, "precision")?;
    field.scale = unsigned(value, "scale")?;
    field.vector_dims = unsigned(value, "vectorDims")?
        .map(|dimensions| {
            usize::try_from(dimensions)
                .map_err(|_| invalid("vector dimensions exceed the supported range"))
        })
        .transpose()?;
    field.vector_metric = match text(value, "vectorMetric")?.as_deref() {
        None | Some("cosine") => VectorMetric::Cosine,
        Some("l2") => VectorMetric::L2,
        Some("innerProduct") => VectorMetric::InnerProduct,
        Some(_) => return Err(invalid("unsupported vector distance metric")),
    };
    field.default = value.get("default").cloned();
    if field.logical_type == LogicalType::Bytes {
        if let Some(Value::String(encoded)) = &field.default {
            use base64::Engine as _;
            field.default = Some(Value::Bytes(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| invalid("binary default must be valid base64"))?,
            ));
        }
    }
    field.generated = value.get("generated").cloned();
    field.identity = value.get("identity").cloned();
    field.literal_value = value.get("literalValue").cloned();
    if let Some(values) = value.get("enum") {
        field.enum_values = values
            .as_array()
            .cloned()
            .ok_or_else(|| invalid("enum values must be an array"))?;
    }
    if let Some(storage) = value.get("storage") {
        if !storage.is_object() {
            return Err(invalid("field storage must be an object"));
        }
        field.storage = StorageMapping {
            value_column: text(storage, "valueColumn")?,
            raw_column: text(storage, "rawColumn")?,
            raw_filterable: flag(storage, "rawFilterable", false)?,
            raw_sortable: flag(storage, "rawSortable", false)?,
            raw_projectable: flag(storage, "rawProjectable", false)?,
        };
    }
    if let Some(mask) = value.get("mask") {
        if !mask.is_object() {
            return Err(invalid("field mask must be an object"));
        }
        field.mask = Some(MaskSchema {
            kind: text(mask, "kind")?.unwrap_or_else(|| "full".into()),
            classification: text(mask, "classification")?.unwrap_or_else(|| "pii".into()),
        });
    }
    if let Some(assignment) = value.get("assign") {
        let by = text(assignment, "by")?
            .ok_or_else(|| {
                DbError::validation("invalid_assignment", "assignment requires a generator")
            })?
            .parse()
            .map_err(
                |error: zeroship_migrate_policy::AssignmentGeneratorParseError| {
                    DbError::validation("invalid_assignment", error.to_string())
                },
            )?;
        let on = match text(assignment, "on")?.as_deref() {
            Some("insert") => AssignmentEvent::Insert,
            Some("write") => AssignmentEvent::Write,
            Some("delete") => AssignmentEvent::Delete,
            _ => {
                return Err(DbError::validation(
                    "invalid_assignment",
                    "unknown assignment event",
                ))
            }
        };
        field.assignment = Some(Assignment { by, on });
    }
    let target = text(value, "refTarget")?;
    let target_column = text(value, "refColumn")?;
    let name = text(value, "relation")?;
    match (target, target_column, name) {
        (None, None, None) => {}
        (Some(collection), Some(column), name) if !collection.is_empty() && !column.is_empty() => {
            field.reference = Some(RelationSchema {
                collection,
                column,
                name,
            });
        }
        _ => {
            return Err(DbError::validation(
                "invalid_relation",
                "reference requires an explicit target collection and column",
            ))
        }
    }
    field.items = text(value, "items")?
        .as_deref()
        .map(logical_type)
        .transpose()?;
    if let Some(shape) = value.get("shape") {
        field.shape = fields(shape, depth + 1)?;
    }
    if let Some(variants) = value.get("variants") {
        field.variants = variants
            .as_array()
            .ok_or_else(|| invalid("union variants must be an array"))?
            .iter()
            .map(|variant| fields(variant, depth + 1))
            .collect::<Result<_, _>>()?;
    }
    field.discriminator = text(value, "discriminator")?;
    field.normalize_defaults();
    Ok(field)
}

impl ColumnSchema {
    pub fn from_descriptor(value: &Value) -> Result<Self, DbError> {
        column(value, 0)
    }
}

impl CollectionSchema {
    pub fn from_fields(value: &Value) -> Result<Self, DbError> {
        Ok(Self {
            fields: fields(value, 0)?,
            duplicate_field: None,
        })
    }
}

impl Schema {
    pub fn from_collections(
        collections: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<Self, DbError> {
        Ok(Self::new(
            collections
                .into_iter()
                .map(|(name, fields)| {
                    CollectionSchema::from_fields(&fields).map(|fields| (name, fields))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ))
    }

    pub fn from_runtime_descriptor(descriptor: &Value) -> Result<Self, DbError> {
        if descriptor.get("version").and_then(Value::as_u64) != Some(2) {
            return Err(invalid("unsupported runtime descriptor version"));
        }
        let collections = descriptor
            .get("collections")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("runtime descriptor requires collections"))?;
        let mut result = Vec::new();
        for (name, collection) in collections {
            let fields = collection
                .get("fields")
                .ok_or_else(|| invalid("collection descriptor requires fields"))?;
            result.push((name.clone(), CollectionSchema::from_fields(fields)?));
        }
        Ok(Self::new(result))
    }
}
