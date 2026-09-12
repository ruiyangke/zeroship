//! Runtime collection validation shared by ORM operation preparation.

use crate::{
    sql::{Ident, IdentRole},
    value::Value,
};
use std::collections::{BTreeSet, HashSet};

pub use crate::sql::lifecycle::WriteAssignments;

pub fn empty_read_schema() -> Value {
    Value::Object(crate::value::Map::new())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    InvalidFilter(String),
    InvalidCollection(String),
    InvalidIdent(String),
    ReservedIdPrefix(String),
    ImmutableAssignedField(String),
}

impl From<crate::sql::compiler::CompileError> for QueryError {
    fn from(error: crate::sql::compiler::CompileError) -> Self {
        Self::InvalidFilter(error.to_string())
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFilter(message) => write!(f, "invalid filter: {message}"),
            Self::InvalidCollection(message) => write!(f, "invalid collection: {message}"),
            Self::InvalidIdent(message) => write!(f, "invalid identifier: {message}"),
            Self::ReservedIdPrefix(message) => write!(f, "reserved ID prefix: {message}"),
            Self::ImmutableAssignedField(message) => {
                write!(f, "immutable assigned field: {message}")
            }
        }
    }
}

impl std::error::Error for QueryError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    Postgres,
    Sqlite,
}

const SQLITE_NOW_EXPR: &str = "(strftime('%Y-%m-%dT%H:%M:%fZ','now'))";

impl SqlDialect {
    pub fn binary_bind_placeholder(self, slot: usize) -> String {
        format!("${slot}")
    }

    pub fn encode_binary_param(self, value: Value) -> Result<Value, QueryError> {
        match value {
            Value::Bytes(_) | Value::Null => Ok(value),
            _ => Err(QueryError::InvalidFilter(
                "binary field requires native bytes".into(),
            )),
        }
    }

    pub fn current_timestamp_expr(self) -> &'static str {
        match self {
            Self::Postgres => "NOW()",
            Self::Sqlite => SQLITE_NOW_EXPR,
        }
    }
}

pub const MAX_QUERY_LIMIT: i64 = 500;
pub const MAX_QUERY_OFFSET: i64 = 10_000;
pub const MAX_INSERT_MANY_BATCH: usize = 1_000;

const MAX_FILTER_NESTING_DEPTH: usize = 16;
const MAX_FILTER_CLAUSE_COUNT: usize = 128;

pub fn effective_query_limit(explicit: Option<i64>) -> i64 {
    explicit.unwrap_or(MAX_QUERY_LIMIT)
}

pub fn validate_collection(name: &str) -> Result<(), QueryError> {
    Ident::parse_as(name, IdentRole::Collection)
        .map(|_| ())
        .map_err(|error| QueryError::InvalidCollection(error.to_string()))
}

pub fn is_schema_metadata_key(key: &str) -> bool {
    matches!(key, "_meta" | "_indexes")
}

pub fn validate_field_name(name: &str) -> Result<(), QueryError> {
    Ident::parse_as(name, IdentRole::Column)
        .map(|_| ())
        .map_err(|error| QueryError::InvalidIdent(error.to_string()))
}

pub fn validate_field_name_for_declaration(name: &str) -> Result<(), QueryError> {
    validate_field_name(name)
}

pub const RESERVED_ID_PREFIXES: &[&str] = &["usr"];

pub fn validate_id_prefix(prefix: &str) -> Result<(), QueryError> {
    let valid = prefix
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_lowercase())
        && prefix.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        });
    if !valid {
        return Err(QueryError::InvalidIdent(format!(
            "id prefix must match ^[a-z][a-z0-9_]*$: {prefix}"
        )));
    }
    if RESERVED_ID_PREFIXES.contains(&prefix) {
        return Err(QueryError::ReservedIdPrefix(format!(
            "'{prefix}' is reserved for platform identities"
        )));
    }
    Ok(())
}

pub(crate) fn validate_value_operation(field: &str, schema: &Value) -> Result<(), QueryError> {
    if schema
        .get(field)
        .is_some_and(crate::sql::descriptors::is_encrypted)
    {
        return Err(QueryError::InvalidFilter(format!(
            "encrypted field '{field}' cannot be filtered, sorted, grouped, or used as a conflict target"
        )));
    }
    Ok(())
}

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub const RAW_COLUMN_PREFIX: &str = "__zs_raw__";
pub const MAX_MASKED_FIELD_NAME_BYTES: usize =
    crate::sql::MAX_IDENT_BYTES - RAW_COLUMN_PREFIX.len();

#[must_use]
pub fn raw_column_name(field: &str) -> String {
    format!("{RAW_COLUMN_PREFIX}{field}")
}

pub fn raw_column_for_field(field: &str, definition: &Value) -> Option<String> {
    crate::sql::descriptors::effective_mask(definition)?;
    Some(raw_column_name(field))
}

pub fn declared_raw_column(field: &str, definition: &Value) -> Result<Option<String>, QueryError> {
    let Some(derived) = raw_column_for_field(field, definition) else {
        return Ok(None);
    };
    let declared = definition
        .get("storage")
        .and_then(|storage| storage.get("rawColumn"))
        .and_then(Value::as_str)
        .unwrap_or(&derived);
    Ident::parse_as(declared, IdentRole::StoredColumn)
        .map_err(|error| QueryError::InvalidIdent(error.to_string()))?;
    if Ident::parse_as(declared, IdentRole::Column).is_ok() {
        return Err(QueryError::InvalidIdent(format!(
            "raw column '{declared}' for masked field '{field}' is reachable by creator queries"
        )));
    }
    Ok(Some(declared.to_owned()))
}

fn field_is_readable(definition: &Value) -> bool {
    definition.get("readable").and_then(Value::as_bool) != Some(false)
        && definition.get("projectable").and_then(Value::as_bool) != Some(false)
}

fn field_needs_identity(definition: &Value) -> bool {
    crate::sql::descriptors::is_encrypted(definition)
        || crate::sql::descriptors::effective_mask(definition).is_some()
}

pub(crate) fn value_column_for_field(field: &str, schema: &Value) -> String {
    schema
        .get(field)
        .and_then(|definition| definition.get("storage"))
        .and_then(|storage| storage.get("valueColumn"))
        .and_then(Value::as_str)
        .unwrap_or(field)
        .to_owned()
}

pub(crate) fn implicit_read_fields(schema: &Value) -> Result<Vec<&str>, QueryError> {
    let fields = schema.as_object().ok_or_else(|| {
        QueryError::InvalidFilter("read schema must be a field-map object".into())
    })?;
    let mut readable = fields
        .iter()
        .filter(|(name, definition)| !is_schema_metadata_key(name) && field_is_readable(definition))
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    if fields
        .get("id")
        .is_some_and(|definition| !field_is_readable(definition))
        && fields
            .values()
            .any(|definition| field_is_readable(definition) && field_needs_identity(definition))
    {
        readable.push("id");
    }
    if readable.is_empty() {
        return Err(QueryError::InvalidFilter(
            "read requires a declared projection".into(),
        ));
    }
    Ok(readable)
}

pub const SYNTHETIC_RESULT_COLUMNS: &[&str] = &["_distance", "_distance_m"];

#[must_use]
pub fn read_surface_columns(schema: &Value) -> BTreeSet<String> {
    let mut columns = crate::sql::descriptors::readable_fields(schema);
    columns.extend(
        SYNTHETIC_RESULT_COLUMNS
            .iter()
            .map(|column| (*column).to_owned()),
    );
    columns
}

pub fn column_is_masked(name: &str, schema: &Value) -> bool {
    schema
        .get(name)
        .and_then(crate::sql::descriptors::effective_mask)
        .is_some()
}

pub fn parse_conflict_fields(value: &Value) -> Result<Vec<&str>, QueryError> {
    let fields = value
        .as_array()
        .ok_or_else(|| QueryError::InvalidFilter("conflict fields must be an array".into()))?;
    if fields.is_empty() {
        return Err(QueryError::InvalidFilter(
            "conflict fields cannot be empty".into(),
        ));
    }
    let mut seen = HashSet::new();
    fields
        .iter()
        .map(|value| {
            let field = value.as_str().ok_or_else(|| {
                QueryError::InvalidFilter("every conflict field must be a string".into())
            })?;
            validate_field_name(field)?;
            if !seen.insert(field) {
                return Err(QueryError::InvalidFilter(format!(
                    "duplicate conflict field '{field}'"
                )));
            }
            Ok(field)
        })
        .collect()
}

pub(crate) fn validate_filter_budget(filter: &Value) -> Result<(), QueryError> {
    let mut clauses = 0;
    count_filter_budget(filter, 1, &mut clauses)
}

fn count_filter_budget(value: &Value, depth: usize, clauses: &mut usize) -> Result<(), QueryError> {
    if depth > MAX_FILTER_NESTING_DEPTH {
        return Err(QueryError::InvalidFilter(
            "filter nesting exceeds its budget".into(),
        ));
    }
    let Some(map) = value.as_object() else {
        return Ok(());
    };
    for (key, child) in map {
        if key.starts_with('$') {
            *clauses += 1;
            check_clause_budget(*clauses)?;
            match key.as_str() {
                "$and" | "$or" => {
                    let children = child.as_array().ok_or_else(|| {
                        QueryError::InvalidFilter(format!("{key} must be an array"))
                    })?;
                    for child in children {
                        count_filter_budget(child, depth + 1, clauses)?;
                    }
                }
                "$not" => count_filter_budget(child, depth + 1, clauses)?,
                _ => {}
            }
            continue;
        }

        if let Some(operators) = child
            .as_object()
            .filter(|operators| operators.keys().any(|operator| operator.starts_with('$')))
        {
            for (operator, operand) in operators {
                *clauses += 1;
                check_clause_budget(*clauses)?;
                if matches!(operator.as_str(), "$in" | "$nin") {
                    let values = operand.as_array().ok_or_else(|| {
                        QueryError::InvalidFilter(format!("{operator} must be an array"))
                    })?;
                    if values.len() > crate::sql::MAX_MEMBERSHIP_LIST_LEN {
                        return Err(QueryError::InvalidFilter(format!(
                            "{operator} exceeds its membership budget"
                        )));
                    }
                }
            }
        } else {
            *clauses += 1;
            check_clause_budget(*clauses)?;
        }
    }
    Ok(())
}

fn check_clause_budget(clauses: usize) -> Result<(), QueryError> {
    if clauses > MAX_FILTER_CLAUSE_COUNT {
        return Err(QueryError::InvalidFilter(
            "filter exceeds its clause budget".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    #[test]
    fn identifier_validation_uses_the_typed_identifier_boundary() {
        assert!(validate_collection("records").is_ok());
        assert!(validate_collection("pg_catalog").is_err());
        assert!(validate_field_name("created_at").is_ok());
        assert!(validate_field_name("__zs_raw__secret").is_err());
    }

    #[test]
    fn read_surfaces_follow_descriptor_metadata() {
        let schema = value!({
            "id":{"type":"string","readable":false},
            "secret":{"type":"string","encrypted":true},
            "hidden":{"type":"string","readable":false},
            "_meta":{}
        });
        assert_eq!(implicit_read_fields(&schema).unwrap(), ["secret", "id"]);
        assert_eq!(
            read_surface_columns(&schema),
            BTreeSet::from(["_distance".into(), "_distance_m".into(), "secret".into()])
        );
    }

    #[test]
    fn raw_storage_must_remain_outside_creator_identifiers() {
        let definition = value!({
            "mask":{"kind":"full"},
            "storage":{"rawColumn":"plaintext"}
        });
        assert!(declared_raw_column("secret", &definition).is_err());
        assert_eq!(
            declared_raw_column("secret", &value!({"mask":{"kind":"full"}})).unwrap(),
            Some(raw_column_name("secret"))
        );
    }

    #[test]
    fn filter_budget_rejects_excessive_membership() {
        let filter = value!({
            "id":{"$in": vec![Value::from("record"); crate::sql::MAX_MEMBERSHIP_LIST_LEN + 1]}
        });
        assert!(validate_filter_budget(&filter).is_err());
    }
}
