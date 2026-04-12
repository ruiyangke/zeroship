//! Filter JSON → parameterized SQL translation.
//!
//! Translates MongoDB-style filter objects into PostgreSQL WHERE clauses
//! with parameterized queries to prevent SQL injection.
//!
//! Supported operators:
//! - `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte` — comparison
//! - `$in`, `$nin` — set membership
//! - `$and`, `$or` — logical combinators
//! - `$exists` — null / not-null check
//! - `$like` — LIKE pattern matching
//!
//! All user values are bound as parameters (`$1`, `$2`, ...).
//! Column and table names are quoted with double-quotes to prevent injection.

use serde_json::Value;

/// Errors from query building.
#[derive(Debug)]
pub enum QueryError {
    /// Unsupported or malformed filter.
    InvalidFilter(String),
    /// Collection name is invalid.
    InvalidCollection(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFilter(msg) => write!(f, "invalid filter: {msg}"),
            Self::InvalidCollection(msg) => write!(f, "invalid collection: {msg}"),
        }
    }
}

/// A built SQL query with text parameters.
///
/// Parameters are always serialized as text strings — the PostgreSQL driver
/// handles type inference from context (column types).
#[derive(Debug)]
pub struct BuiltQuery {
    pub sql: String,
    pub params: Vec<String>,
}

/// Validate a collection name: alphanumeric + underscores only.
fn validate_collection(name: &str) -> Result<(), QueryError> {
    if name.is_empty() {
        return Err(QueryError::InvalidCollection(
            "collection name cannot be empty".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(QueryError::InvalidCollection(format!(
            "invalid collection name: {name}"
        )));
    }
    Ok(())
}

/// Quote an identifier (table or column name) with double-quotes.
/// Escapes any embedded double-quotes by doubling them.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Build a SELECT query: `SELECT * FROM "app_id"."collection" WHERE ... LIMIT ... OFFSET ...`
pub fn build_find(
    app_id: &str,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_collection(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("SELECT * FROM {schema}.{table}");
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }

    if let Some(order) = order_by {
        let order_clause = build_order_by(order)?;
        if !order_clause.is_empty() {
            sql.push_str(" ORDER BY ");
            sql.push_str(&order_clause);
        }
    }

    if let Some(lim) = limit {
        sql.push_str(&format!(" LIMIT {lim}"));
    }
    if let Some(off) = offset {
        sql.push_str(&format!(" OFFSET {off}"));
    }

    Ok(BuiltQuery { sql, params })
}

/// Build a SELECT COUNT(*) query.
pub fn build_count(
    app_id: &str,
    collection: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_collection(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("SELECT COUNT(*) AS count FROM {schema}.{table}");
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }

    Ok(BuiltQuery { sql, params })
}

/// Build an INSERT query: `INSERT INTO "app_id"."collection" (...) VALUES (...) RETURNING *`
pub fn build_insert(
    app_id: &str,
    collection: &str,
    doc: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_collection(app_id)?;

    let obj = doc
        .as_object()
        .ok_or_else(|| QueryError::InvalidFilter("insert document must be an object".to_string()))?;

    if obj.is_empty() {
        return Err(QueryError::InvalidFilter(
            "insert document cannot be empty".to_string(),
        ));
    }

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut columns = Vec::new();
    let mut placeholders = Vec::new();
    let mut params: Vec<String> = Vec::new();

    for (key, value) in obj {
        columns.push(quote_ident(key));
        params.push(value_to_param(value));
        placeholders.push(format!("${}", params.len()));
    }

    let sql = format!(
        "INSERT INTO {schema}.{table} ({}) VALUES ({}) RETURNING *",
        columns.join(", "),
        placeholders.join(", ")
    );

    Ok(BuiltQuery { sql, params })
}

/// Build an UPDATE query: `UPDATE "app_id"."collection" SET ... WHERE ... RETURNING *`
pub fn build_update_one(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_collection(app_id)?;

    let update_obj = update
        .as_object()
        .ok_or_else(|| QueryError::InvalidFilter("update must be an object".to_string()))?;

    // Support both { field: value } and { $set: { field: value } } syntax
    let fields = if let Some(set_val) = update_obj.get("$set") {
        set_val
            .as_object()
            .ok_or_else(|| QueryError::InvalidFilter("$set must be an object".to_string()))?
    } else {
        update_obj
    };

    if fields.is_empty() {
        return Err(QueryError::InvalidFilter(
            "update fields cannot be empty".to_string(),
        ));
    }

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let mut set_clauses = Vec::new();

    for (key, value) in fields {
        params.push(value_to_param(value));
        set_clauses.push(format!("{} = ${}", quote_ident(key), params.len()));
    }

    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!(
        "UPDATE {schema}.{table} SET {}",
        set_clauses.join(", ")
    );

    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }

    // LIMIT 1 for updateOne — use a subquery with ctid for Postgres
    sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE ctid = (SELECT ctid FROM {schema}.{table}{} LIMIT 1) RETURNING *",
        set_clauses.join(", "),
        if where_clause.is_empty() {
            String::new()
        } else {
            format!(" WHERE {where_clause}")
        }
    );

    Ok(BuiltQuery { sql, params })
}

/// Build a DELETE query: `DELETE FROM "app_id"."collection" WHERE ... RETURNING *`
pub fn build_delete_one(
    app_id: &str,
    collection: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_collection(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    // LIMIT 1 via subquery with ctid
    let sql = format!(
        "DELETE FROM {schema}.{table} WHERE ctid = (SELECT ctid FROM {schema}.{table}{} LIMIT 1) RETURNING *",
        if where_clause.is_empty() {
            String::new()
        } else {
            format!(" WHERE {where_clause}")
        }
    );

    Ok(BuiltQuery { sql, params })
}

// ---------------------------------------------------------------------------
// WHERE clause builder
// ---------------------------------------------------------------------------

/// Build a WHERE clause from a filter JSON value.
/// Returns empty string if the filter is null/empty.
fn build_where(filter: &Value, params: &mut Vec<String>) -> Result<String, QueryError> {
    match filter {
        Value::Null => Ok(String::new()),
        Value::Object(map) if map.is_empty() => Ok(String::new()),
        Value::Object(map) => {
            let mut conditions = Vec::new();
            for (key, value) in map {
                if key.starts_with('$') {
                    // Top-level operator
                    match key.as_str() {
                        "$and" => {
                            let arr = value.as_array().ok_or_else(|| {
                                QueryError::InvalidFilter("$and must be an array".to_string())
                            })?;
                            let sub: Result<Vec<String>, _> = arr
                                .iter()
                                .map(|v| build_where(v, params))
                                .collect();
                            let sub = sub?;
                            let non_empty: Vec<&str> =
                                sub.iter().filter(|s| !s.is_empty()).map(String::as_str).collect();
                            if !non_empty.is_empty() {
                                conditions.push(format!("({})", non_empty.join(" AND ")));
                            }
                        }
                        "$or" => {
                            let arr = value.as_array().ok_or_else(|| {
                                QueryError::InvalidFilter("$or must be an array".to_string())
                            })?;
                            let sub: Result<Vec<String>, _> = arr
                                .iter()
                                .map(|v| build_where(v, params))
                                .collect();
                            let sub = sub?;
                            let non_empty: Vec<&str> =
                                sub.iter().filter(|s| !s.is_empty()).map(String::as_str).collect();
                            if !non_empty.is_empty() {
                                conditions.push(format!("({})", non_empty.join(" OR ")));
                            }
                        }
                        other => {
                            return Err(QueryError::InvalidFilter(format!(
                                "unsupported top-level operator: {other}"
                            )));
                        }
                    }
                } else {
                    // Field-level condition
                    let cond = build_field_condition(key, value, params)?;
                    conditions.push(cond);
                }
            }
            Ok(conditions.join(" AND "))
        }
        _ => Err(QueryError::InvalidFilter(
            "filter must be an object or null".to_string(),
        )),
    }
}

/// Build a condition for a single field.
fn build_field_condition(
    field: &str,
    value: &Value,
    params: &mut Vec<String>,
) -> Result<String, QueryError> {
    let col = quote_ident(field);

    match value {
        // { field: { $op: val } }
        Value::Object(ops) if ops.keys().any(|k| k.starts_with('$')) => {
            let mut parts = Vec::new();
            for (op, val) in ops {
                let cond = match op.as_str() {
                    "$eq" => {
                        if val.is_null() {
                            format!("{col} IS NULL")
                        } else {
                            params.push(value_to_param(val));
                            format!("{col} = ${}", params.len())
                        }
                    }
                    "$ne" => {
                        if val.is_null() {
                            format!("{col} IS NOT NULL")
                        } else {
                            params.push(value_to_param(val));
                            format!("{col} != ${}", params.len())
                        }
                    }
                    "$gt" => {
                        params.push(value_to_param(val));
                        format!("{col} > ${}", params.len())
                    }
                    "$gte" => {
                        params.push(value_to_param(val));
                        format!("{col} >= ${}", params.len())
                    }
                    "$lt" => {
                        params.push(value_to_param(val));
                        format!("{col} < ${}", params.len())
                    }
                    "$lte" => {
                        params.push(value_to_param(val));
                        format!("{col} <= ${}", params.len())
                    }
                    "$in" => {
                        let arr = val.as_array().ok_or_else(|| {
                            QueryError::InvalidFilter("$in must be an array".to_string())
                        })?;
                        let placeholders: Vec<String> = arr
                            .iter()
                            .map(|v| {
                                params.push(value_to_param(v));
                                format!("${}", params.len())
                            })
                            .collect();
                        format!("{col} IN ({})", placeholders.join(", "))
                    }
                    "$nin" => {
                        let arr = val.as_array().ok_or_else(|| {
                            QueryError::InvalidFilter("$nin must be an array".to_string())
                        })?;
                        let placeholders: Vec<String> = arr
                            .iter()
                            .map(|v| {
                                params.push(value_to_param(v));
                                format!("${}", params.len())
                            })
                            .collect();
                        format!("{col} NOT IN ({})", placeholders.join(", "))
                    }
                    "$exists" => {
                        let exists = val.as_bool().ok_or_else(|| {
                            QueryError::InvalidFilter("$exists must be a boolean".to_string())
                        })?;
                        if exists {
                            format!("{col} IS NOT NULL")
                        } else {
                            format!("{col} IS NULL")
                        }
                    }
                    "$like" => {
                        let pattern = val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter("$like must be a string".to_string())
                        })?;
                        params.push(pattern.to_string());
                        format!("{col} LIKE ${}", params.len())
                    }
                    other => {
                        return Err(QueryError::InvalidFilter(format!(
                            "unsupported operator: {other}"
                        )));
                    }
                };
                parts.push(cond);
            }
            Ok(parts.join(" AND "))
        }
        // { field: value } — implicit $eq
        _ => {
            if value.is_null() {
                Ok(format!("{col} IS NULL"))
            } else {
                params.push(value_to_param(value));
                Ok(format!("{col} = ${}", params.len()))
            }
        }
    }
}

/// Build an ORDER BY clause from a JSON value.
///
/// Accepts: `{ "field": 1 }` or `{ "field": -1 }` (1 = ASC, -1 = DESC)
/// or `[["field", 1], ["field2", -1]]` for ordered multi-column sort.
fn build_order_by(order: &Value) -> Result<String, QueryError> {
    match order {
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(key, val)| {
                    let dir = match val.as_i64() {
                        Some(n) if n < 0 => "DESC",
                        _ => "ASC",
                    };
                    format!("{} {dir}", quote_ident(key))
                })
                .collect();
            Ok(parts.join(", "))
        }
        Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                let pair = item.as_array().ok_or_else(|| {
                    QueryError::InvalidFilter("orderBy array entries must be [field, dir]".to_string())
                })?;
                if pair.len() != 2 {
                    return Err(QueryError::InvalidFilter(
                        "orderBy array entries must be [field, dir]".to_string(),
                    ));
                }
                let field = pair[0].as_str().ok_or_else(|| {
                    QueryError::InvalidFilter("orderBy field must be a string".to_string())
                })?;
                let dir = match pair[1].as_i64() {
                    Some(n) if n < 0 => "DESC",
                    _ => "ASC",
                };
                parts.push(format!("{} {dir}", quote_ident(field)));
            }
            Ok(parts.join(", "))
        }
        _ => Err(QueryError::InvalidFilter(
            "orderBy must be an object or array".to_string(),
        )),
    }
}

/// Convert a JSON value to a text parameter string for Postgres.
fn value_to_param(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(), // should not be used as param (use IS NULL)
        // For arrays/objects, serialize as JSON text (stored as JSONB in PG)
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_simple_eq_filter() {
        let filter = json!({"name": "alice"});
        let q = build_find("app1", "users", &filter, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users" WHERE "name" = $1"#);
        assert_eq!(q.params, vec!["alice"]);
    }

    #[test]
    fn test_comparison_operators() {
        let filter = json!({"age": {"$gte": 18, "$lt": 65}});
        let q = build_find("app1", "users", &filter, None, None, None).unwrap();
        assert!(q.sql.contains(r#""age" >= $1"#));
        assert!(q.sql.contains(r#""age" < $2"#));
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_in_operator() {
        let filter = json!({"status": {"$in": ["active", "pending"]}});
        let q = build_find("app1", "users", &filter, None, None, None).unwrap();
        assert!(q.sql.contains(r#""status" IN ($1, $2)"#));
        assert_eq!(q.params, vec!["active", "pending"]);
    }

    #[test]
    fn test_or_combinator() {
        let filter = json!({"$or": [{"name": "alice"}, {"name": "bob"}]});
        let q = build_find("app1", "users", &filter, None, None, None).unwrap();
        assert!(q.sql.contains("OR"));
        assert_eq!(q.params, vec!["alice", "bob"]);
    }

    #[test]
    fn test_empty_filter() {
        let filter = json!({});
        let q = build_find("app1", "users", &filter, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users""#);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_null_filter() {
        let filter = Value::Null;
        let q = build_find("app1", "users", &filter, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users""#);
    }

    #[test]
    fn test_limit_offset() {
        let filter = json!({});
        let q = build_find("app1", "users", &filter, Some(10), Some(20), None).unwrap();
        assert!(q.sql.contains("LIMIT 10"));
        assert!(q.sql.contains("OFFSET 20"));
    }

    #[test]
    fn test_insert() {
        let doc = json!({"name": "alice", "age": 30});
        let q = build_insert("app1", "users", &doc).unwrap();
        assert!(q.sql.contains("INSERT INTO"));
        assert!(q.sql.contains("RETURNING *"));
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_invalid_collection() {
        let filter = json!({});
        let result = build_find("app1", "users; DROP TABLE", &filter, None, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_exists_operator() {
        let filter = json!({"email": {"$exists": true}});
        let q = build_find("app1", "users", &filter, None, None, None).unwrap();
        assert!(q.sql.contains(r#""email" IS NOT NULL"#));
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_count() {
        let filter = json!({"active": true});
        let q = build_count("app1", "users", &filter).unwrap();
        assert!(q.sql.contains("SELECT COUNT(*)"));
        assert_eq!(q.params, vec!["true"]);
    }
}
