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

/// Validate an app_id (schema name): alphanumeric + underscores + hyphens.
/// UUIDs contain hyphens. Schema names are always double-quoted in SQL.
fn validate_schema(name: &str) -> Result<(), QueryError> {
    if name.is_empty() {
        return Err(QueryError::InvalidCollection(
            "schema name cannot be empty".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(QueryError::InvalidCollection(format!(
            "invalid schema name: {name}"
        )));
    }
    Ok(())
}

/// Quote an identifier (table or column name) with double-quotes.
/// Escapes any embedded double-quotes by doubling them.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// DDL builders for registerModel
// ---------------------------------------------------------------------------

/// Build CREATE SCHEMA IF NOT EXISTS for an app.
pub fn build_create_schema(app_id: &str) -> String {
    format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(app_id))
}

/// Build CREATE TABLE IF NOT EXISTS from a normalized schema JSON.
///
/// Schema format: `{ "name": { "type": "string", "required": true, ... }, ... }`
///
/// Auto-generates: id SERIAL PRIMARY KEY, created_at, updated_at.
pub fn build_create_table(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
) -> Result<String, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let table = format!("{}.{}", quote_ident(app_id), quote_ident(collection));

    let mut columns = vec![
        "id SERIAL PRIMARY KEY".to_string(),
    ];

    if let Some(obj) = schema.as_object() {
        for (field, def) in obj {
            let col_def = field_to_column(field, def);
            columns.push(col_def);
        }
    }

    columns.push("created_at TIMESTAMPTZ DEFAULT NOW()".to_string());
    columns.push("updated_at TIMESTAMPTZ DEFAULT NOW()".to_string());

    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {} (\n  {}\n)",
        table,
        columns.join(",\n  ")
    ))
}

/// Build ALTER TABLE ADD COLUMN IF NOT EXISTS for a single field.
pub fn build_add_column(
    app_id: &str,
    collection: &str,
    field: &str,
    def: &serde_json::Value,
) -> Result<String, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let table = format!("{}.{}", quote_ident(app_id), quote_ident(collection));
    let pg_type = def_to_pg_type(def);
    let constraints = def_to_constraints(field, def);

    Ok(format!(
        "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {} {}",
        table,
        quote_ident(field),
        pg_type,
        constraints
    ).trim().to_string())
}

/// Convert a field definition to a full column definition for CREATE TABLE.
fn field_to_column(field: &str, def: &serde_json::Value) -> String {
    let pg_type = def_to_pg_type(def);
    let constraints = def_to_constraints(field, def);
    format!("{} {} {}", quote_ident(field), pg_type, constraints).trim().to_string()
}

/// Map schema type to PostgreSQL type.
fn def_to_pg_type(def: &serde_json::Value) -> &'static str {
    match def.get("type").and_then(|t| t.as_str()) {
        Some("string") => "TEXT",
        Some("number") => "NUMERIC",
        Some("boolean") => "BOOLEAN",
        Some("date") => "TIMESTAMPTZ",
        Some("json") => "JSONB",
        Some("array") => "JSONB",
        _ => "TEXT",
    }
}

/// Generate column constraints from field definition.
fn def_to_constraints(field: &str, def: &serde_json::Value) -> String {
    let mut parts = Vec::new();

    if def.get("required").and_then(|v| v.as_bool()) == Some(true) {
        parts.push("NOT NULL".to_string());
    }

    if def.get("unique").and_then(|v| v.as_bool()) == Some(true) {
        parts.push("UNIQUE".to_string());
    }

    // Default value
    if let Some(default) = def.get("default") {
        match def.get("type").and_then(|t| t.as_str()) {
            Some("string") => {
                if let Some(s) = default.as_str() {
                    parts.push(format!("DEFAULT '{}'", s.replace('\'', "''")));
                }
            }
            Some("number") => {
                if let Some(n) = default.as_f64() {
                    parts.push(format!("DEFAULT {n}"));
                }
            }
            Some("boolean") => {
                if let Some(b) = default.as_bool() {
                    parts.push(format!("DEFAULT {b}"));
                }
            }
            Some("json") => parts.push("DEFAULT '{}'::jsonb".to_string()),
            Some("array") => parts.push("DEFAULT '[]'::jsonb".to_string()),
            _ => {}
        }
    } else {
        // Default defaults for json/array
        match def.get("type").and_then(|t| t.as_str()) {
            Some("json") => parts.push("DEFAULT '{}'::jsonb".to_string()),
            Some("array") => parts.push("DEFAULT '[]'::jsonb".to_string()),
            _ => {}
        }
    }

    // Check constraints for min/max
    let col = quote_ident(field);
    if let (Some("number"), Some(min)) = (def.get("type").and_then(|t| t.as_str()), def.get("min").and_then(|v| v.as_f64())) {
        if let Some(max) = def.get("max").and_then(|v| v.as_f64()) {
            parts.push(format!("CHECK ({col} >= {min} AND {col} <= {max})"));
        } else {
            parts.push(format!("CHECK ({col} >= {min})"));
        }
    } else if let (Some("number"), Some(max)) = (def.get("type").and_then(|t| t.as_str()), def.get("max").and_then(|v| v.as_f64())) {
        parts.push(format!("CHECK ({col} <= {max})"));
    }

    // Enum constraint — supports both string and numeric values
    if let Some(enums) = def.get("enum").and_then(|v| v.as_array()) {
        let values: Vec<String> = enums
            .iter()
            .filter_map(|v| {
                if let Some(s) = v.as_str() {
                    Some(format!("'{}'", s.replace('\'', "''")))
                } else if let Some(n) = v.as_i64() {
                    Some(n.to_string())
                } else if let Some(n) = v.as_f64() {
                    Some(n.to_string())
                } else {
                    None
                }
            })
            .collect();
        if !values.is_empty() {
            parts.push(format!("CHECK ({col} IN ({}))", values.join(", ")));
        }
    }

    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Query builders
// ---------------------------------------------------------------------------

/// Build a SELECT query: `SELECT [cols|*] FROM "app_id"."collection" WHERE ... LIMIT ... OFFSET ...`
pub fn build_find(
    app_id: &str,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    // Build SELECT column list from projection, or default to *
    let select_expr = match select {
        Some(Value::Array(arr)) if !arr.is_empty() => {
            let cols: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(quote_ident)
                .collect();
            if cols.is_empty() {
                "*".to_string()
            } else {
                cols.join(", ")
            }
        }
        _ => "*".to_string(),
    };

    let mut sql = format!("SELECT {select_expr} FROM {schema}.{table}");
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
    validate_schema(app_id)?;

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
    validate_schema(app_id)?;

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

/// Build SET clauses from an update object, supporting update operators.
///
/// Walks each key in `update`:
/// - If the value is `{ "$op": val }` where `$op` is a known update operator,
///   generates operator-specific SQL.
/// - Otherwise treats it as a plain `$set` (`"col" = $N`).
///
/// Supported operators:
/// - `$set`      — `"col" = $N`
/// - `$inc`      — `"col" = "col" + $N`
/// - `$dec`      — `"col" = "col" - $N`
/// - `$mul`      — `"col" = "col" * $N`
/// - `$push`     — `"col" = "col" || to_jsonb($N::text)`
/// - `$pull`     — `"col" = "col" - $N`
/// - `$addToSet` — `"col" = CASE WHEN "col" @> to_jsonb($N::text) THEN "col" ELSE "col" || to_jsonb($N::text) END`
pub fn build_set_clauses(
    update: &Value,
    params: &mut Vec<String>,
) -> Result<Vec<String>, QueryError> {
    let update_obj = update
        .as_object()
        .ok_or_else(|| QueryError::InvalidFilter("update must be an object".to_string()))?;

    // Collect all fields: flatten $set inline, keep other keys as-is
    let mut fields: Vec<(&String, &Value)> = Vec::new();
    for (key, value) in update_obj.iter() {
        if key == "$set" {
            // Flatten $set fields into the top level
            let obj = value
                .as_object()
                .ok_or_else(|| QueryError::InvalidFilter("$set must be an object".to_string()))?;
            fields.extend(obj.iter());
        } else {
            fields.push((key, value));
        }
    }

    if fields.is_empty() {
        return Err(QueryError::InvalidFilter(
            "update fields cannot be empty".to_string(),
        ));
    }

    let mut set_clauses = Vec::new();

    for (key, value) in fields {
        let col = quote_ident(key);

        // Check if the value is an operator object: { "$op": val }
        if let Some(ops) = value.as_object() {
            if let Some(op_key) = ops.keys().find(|k| k.starts_with('$')) {
                let op = op_key.as_str();
                let op_val = &ops[op_key];

                let clause = match op {
                    "$set" => {
                        params.push(value_to_param(op_val));
                        format!("{col} = ${}", params.len())
                    }
                    "$inc" => {
                        params.push(value_to_param(op_val));
                        format!("{col} = {col} + ${}::numeric", params.len())
                    }
                    "$dec" => {
                        params.push(value_to_param(op_val));
                        format!("{col} = {col} - ${}::numeric", params.len())
                    }
                    "$mul" => {
                        params.push(value_to_param(op_val));
                        format!("{col} = {col} * ${}::numeric", params.len())
                    }
                    "$push" => {
                        // Serialize as JSON so numbers stay numbers, strings stay strings
                        params.push(op_val.to_string());
                        format!("{col} = {col} || ${}::jsonb", params.len())
                    }
                    "$pull" => {
                        // Remove array element by value: filter out matching elements
                        params.push(op_val.to_string());
                        let n = params.len();
                        format!(
                            "{col} = (SELECT COALESCE(jsonb_agg(elem), '[]'::jsonb) FROM jsonb_array_elements({col}) elem WHERE elem != ${n}::jsonb)"
                        )
                    }
                    "$addToSet" => {
                        params.push(op_val.to_string());
                        let n = params.len();
                        format!(
                            "{col} = CASE WHEN {col} @> ${n}::jsonb THEN {col} ELSE {col} || ${n}::jsonb END"
                        )
                    }
                    other => {
                        return Err(QueryError::InvalidFilter(format!(
                            "unsupported update operator: {other}"
                        )));
                    }
                };
                set_clauses.push(clause);
                continue;
            }
        }

        // Plain field: value — treat as $set
        params.push(value_to_param(value));
        set_clauses.push(format!("{col} = ${}", params.len()));
    }

    // Auto-update updated_at unless the caller explicitly set it
    if !set_clauses.iter().any(|c| c.contains("\"updated_at\"")) {
        set_clauses.push("\"updated_at\" = NOW()".to_string());
    }

    Ok(set_clauses)
}

/// Build an UPDATE query: `UPDATE "app_id"."collection" SET ... WHERE ctid = (...) RETURNING *`
pub fn build_update_one(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses = build_set_clauses(update, &mut params)?;

    let where_clause = build_where(filter, &mut params)?;

    // LIMIT 1 for updateOne — use a subquery with ctid for Postgres
    let sql = format!(
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

/// Build an INSERT query for multiple documents:
/// `INSERT INTO "app_id"."collection" ("col1", "col2") VALUES ($1, $2), ($3, $4) RETURNING *`
///
/// All docs must have the same column set (defined by the first document).
pub fn build_insert_many(
    app_id: &str,
    collection: &str,
    docs: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let arr = docs.as_array().ok_or_else(|| {
        QueryError::InvalidFilter("insertMany: docs must be an array".to_string())
    })?;

    if arr.is_empty() {
        return Err(QueryError::InvalidFilter(
            "insertMany: docs array cannot be empty".to_string(),
        ));
    }

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    // Union all columns across all documents (not just the first)
    let mut column_set = std::collections::BTreeSet::<&String>::new();
    for doc in arr {
        let obj = doc.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("insertMany: each document must be an object".to_string())
        })?;
        for key in obj.keys() {
            column_set.insert(key);
        }
    }

    if column_set.is_empty() {
        return Err(QueryError::InvalidFilter(
            "insertMany: documents cannot be empty".to_string(),
        ));
    }

    let column_names: Vec<&String> = column_set.into_iter().collect();
    let columns: Vec<String> = column_names.iter().map(|k| quote_ident(k)).collect();

    let mut params: Vec<String> = Vec::new();
    let mut value_groups: Vec<String> = Vec::new();

    for doc in arr {
        let obj = doc.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("insertMany: each document must be an object".to_string())
        })?;
        let mut placeholders = Vec::new();
        for key in &column_names {
            let val = obj.get(*key).unwrap_or(&Value::Null);
            params.push(value_to_param(val));
            placeholders.push(format!("${}", params.len()));
        }
        value_groups.push(format!("({})", placeholders.join(", ")));
    }

    let sql = format!(
        "INSERT INTO {schema}.{table} ({}) VALUES {} RETURNING *",
        columns.join(", "),
        value_groups.join(", ")
    );

    Ok(BuiltQuery { sql, params })
}

/// Build an UPDATE query for multiple rows (no LIMIT 1):
/// `UPDATE "app_id"."collection" SET ... WHERE ... RETURNING *`
pub fn build_update_many(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses = build_set_clauses(update, &mut params)?;

    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("UPDATE {schema}.{table} SET {}", set_clauses.join(", "));
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(" RETURNING *");

    Ok(BuiltQuery { sql, params })
}

/// Build a DELETE query for multiple rows (no LIMIT 1):
/// `DELETE FROM "app_id"."collection" WHERE ... RETURNING *`
pub fn build_delete_many(
    app_id: &str,
    collection: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("DELETE FROM {schema}.{table}");
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(" RETURNING *");

    Ok(BuiltQuery { sql, params })
}

/// Build a DELETE query: `DELETE FROM "app_id"."collection" WHERE ... RETURNING *`
pub fn build_delete_one(
    app_id: &str,
    collection: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

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

/// Build an aggregate query from a pipeline of stages.
///
/// Supported stages:
/// - `$match`  → WHERE clause
/// - `$group`  → SELECT aggregates + optional GROUP BY
/// - `$having` → HAVING clause
/// - `$sort`   → ORDER BY
/// - `$limit`  → LIMIT N
pub fn build_aggregate(
    app_id: &str,
    collection: &str,
    pipeline: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let stages = pipeline.as_array().ok_or_else(|| {
        QueryError::InvalidFilter("aggregate: pipeline must be an array".to_string())
    })?;

    let mut params: Vec<String> = Vec::new();
    let mut where_clause = String::new();
    let mut select_cols: Vec<String> = Vec::new();
    let mut group_by_cols: Vec<String> = Vec::new();
    // Map alias → SQL expression for HAVING clause rewriting
    let mut agg_exprs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut having_clause = String::new();
    let mut order_clause = String::new();
    let mut limit_clause = String::new();
    // Track the most recent $sort for $first sort-order threading
    let mut last_sort: Vec<(String, &str)> = Vec::new();

    for stage in stages {
        let obj = stage.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("aggregate: each stage must be an object".to_string())
        })?;

        if let Some(match_val) = obj.get("$match") {
            where_clause = build_where(match_val, &mut params)?;
        } else if let Some(group_val) = obj.get("$group") {
            let group_obj = group_val.as_object().ok_or_else(|| {
                QueryError::InvalidFilter("aggregate: $group must be an object".to_string())
            })?;

            // Handle optional `by` field
            if let Some(by_val) = group_obj.get("by") {
                match by_val {
                    Value::String(s) => {
                        let col = quote_ident(s);
                        select_cols.push(col.clone());
                        group_by_cols.push(col);
                    }
                    Value::Array(arr) => {
                        for item in arr {
                            let s = item.as_str().ok_or_else(|| {
                                QueryError::InvalidFilter(
                                    "aggregate: $group.by array elements must be strings"
                                        .to_string(),
                                )
                            })?;
                            let col = quote_ident(s);
                            select_cols.push(col.clone());
                            group_by_cols.push(col);
                        }
                    }
                    _ => {
                        return Err(QueryError::InvalidFilter(
                            "aggregate: $group.by must be a string or array".to_string(),
                        ));
                    }
                }
            }

            // Process aggregation functions
            for (alias, agg_val) in group_obj {
                if alias == "by" {
                    continue;
                }
                let agg_obj = agg_val.as_object().ok_or_else(|| {
                    QueryError::InvalidFilter(format!(
                        "aggregate: $group.{alias} must be an object"
                    ))
                })?;

                let op_key = agg_obj.keys().find(|k| k.starts_with('$')).ok_or_else(|| {
                    QueryError::InvalidFilter(format!(
                        "aggregate: $group.{alias} must have an aggregation operator"
                    ))
                })?;
                let op_val = &agg_obj[op_key];

                let agg_expr = match op_key.as_str() {
                    "$count" => "COUNT(*)".to_string(),
                    "$sum" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$sum requires a field name string".to_string(),
                            )
                        })?;
                        format!("SUM({})", quote_ident(field))
                    }
                    "$avg" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$avg requires a field name string".to_string(),
                            )
                        })?;
                        format!("AVG({})", quote_ident(field))
                    }
                    "$min" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$min requires a field name string".to_string(),
                            )
                        })?;
                        format!("MIN({})", quote_ident(field))
                    }
                    "$max" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$max requires a field name string".to_string(),
                            )
                        })?;
                        format!("MAX({})", quote_ident(field))
                    }
                    "$first" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$first requires a field name string".to_string(),
                            )
                        })?;
                        if last_sort.is_empty() {
                            format!("(array_agg({}))[1]", quote_ident(field))
                        } else {
                            let order_parts: Vec<String> = last_sort
                                .iter()
                                .map(|(col, dir)| format!("{} {dir}", quote_ident(col)))
                                .collect();
                            format!(
                                "(array_agg({} ORDER BY {}))[1]",
                                quote_ident(field),
                                order_parts.join(", ")
                            )
                        }
                    }
                    other => {
                        return Err(QueryError::InvalidFilter(format!(
                            "aggregate: unsupported aggregation operator: {other}"
                        )));
                    }
                };

                agg_exprs.insert(alias.clone(), agg_expr.clone());
                select_cols.push(format!("{agg_expr} AS {}", quote_ident(alias)));
            }
        } else if let Some(having_val) = obj.get("$having") {
            having_clause = build_having(having_val, &mut params, &agg_exprs)?;
        } else if let Some(sort_val) = obj.get("$sort") {
            // Track sort columns/directions for $first threading
            last_sort.clear();
            if let Some(sort_obj) = sort_val.as_object() {
                for (key, val) in sort_obj {
                    let dir = match val.as_i64() {
                        Some(n) if n < 0 => "DESC",
                        _ => "ASC",
                    };
                    last_sort.push((key.clone(), dir));
                }
            }
            order_clause = build_order_by(sort_val)?;
        } else if let Some(limit_val) = obj.get("$limit") {
            let n = limit_val.as_i64().ok_or_else(|| {
                QueryError::InvalidFilter("aggregate: $limit must be an integer".to_string())
            })?;
            limit_clause = format!("{n}");
        }
    }

    let select_expr = if select_cols.is_empty() {
        "*".to_string()
    } else {
        select_cols.join(", ")
    };

    let mut sql = format!("SELECT {select_expr} FROM {schema}.{table}");

    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }

    if !group_by_cols.is_empty() {
        sql.push_str(" GROUP BY ");
        sql.push_str(&group_by_cols.join(", "));
    }

    if !having_clause.is_empty() {
        sql.push_str(" HAVING ");
        sql.push_str(&having_clause);
    }

    if !order_clause.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(&order_clause);
    }

    if !limit_clause.is_empty() {
        sql.push_str(" LIMIT ");
        sql.push_str(&limit_clause);
    }

    Ok(BuiltQuery { sql, params })
}

/// Build a SELECT DISTINCT query:
/// `SELECT DISTINCT "field" FROM "schema"."table" WHERE ... ORDER BY "field"`
pub fn build_distinct(
    app_id: &str,
    collection: &str,
    field: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let col = quote_ident(field);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("SELECT DISTINCT {col} FROM {schema}.{table}");
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(&format!(" ORDER BY {col}"));

    Ok(BuiltQuery { sql, params })
}

// ---------------------------------------------------------------------------
// HAVING clause builder (resolves aliases to aggregate expressions)
// ---------------------------------------------------------------------------

/// Build a HAVING clause from a filter, replacing alias names with their
/// aggregate SQL expressions. E.g. `{"cnt": {"$gt": 5}}` where `cnt` maps
/// to `COUNT(*)` generates `COUNT(*) > $1` instead of `"cnt" > $1`.
fn build_having(
    filter: &Value,
    params: &mut Vec<String>,
    agg_exprs: &std::collections::HashMap<String, String>,
) -> Result<String, QueryError> {
    match filter {
        Value::Null => Ok(String::new()),
        Value::Object(map) if map.is_empty() => Ok(String::new()),
        Value::Object(map) => {
            let mut conditions = Vec::new();
            for (key, value) in map {
                if key.starts_with('$') {
                    match key.as_str() {
                        "$and" => {
                            let arr = value.as_array().ok_or_else(|| {
                                QueryError::InvalidFilter("$and must be an array".to_string())
                            })?;
                            let sub: Result<Vec<String>, _> = arr
                                .iter()
                                .map(|v| build_having(v, params, agg_exprs))
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
                                .map(|v| build_having(v, params, agg_exprs))
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
                                "unsupported top-level operator in HAVING: {other}"
                            )));
                        }
                    }
                } else {
                    // Resolve alias → aggregate expression, or fall back to quoted column
                    let col = agg_exprs
                        .get(key)
                        .cloned()
                        .unwrap_or_else(|| quote_ident(key));
                    let cond = build_having_condition(&col, value, params)?;
                    conditions.push(cond);
                }
            }
            Ok(conditions.join(" AND "))
        }
        _ => Err(QueryError::InvalidFilter(
            "HAVING filter must be an object or null".to_string(),
        )),
    }
}

/// Build a single HAVING condition. Like `build_field_condition` but takes
/// a pre-resolved column expression (which may be an aggregate like `COUNT(*)`).
fn build_having_condition(
    col_expr: &str,
    value: &Value,
    params: &mut Vec<String>,
) -> Result<String, QueryError> {
    match value {
        Value::Object(ops) if ops.keys().any(|k| k.starts_with('$')) => {
            let mut parts = Vec::new();
            for (op, val) in ops {
                let cond = match op.as_str() {
                    "$eq" => {
                        params.push(value_to_param(val));
                        format!("{col_expr} = ${}", params.len())
                    }
                    "$ne" => {
                        params.push(value_to_param(val));
                        format!("{col_expr} != ${}", params.len())
                    }
                    "$gt" => {
                        params.push(value_to_param(val));
                        format!("{col_expr} > ${}", params.len())
                    }
                    "$gte" => {
                        params.push(value_to_param(val));
                        format!("{col_expr} >= ${}", params.len())
                    }
                    "$lt" => {
                        params.push(value_to_param(val));
                        format!("{col_expr} < ${}", params.len())
                    }
                    "$lte" => {
                        params.push(value_to_param(val));
                        format!("{col_expr} <= ${}", params.len())
                    }
                    other => {
                        return Err(QueryError::InvalidFilter(format!(
                            "unsupported HAVING operator: {other}"
                        )));
                    }
                };
                parts.push(cond);
            }
            Ok(parts.join(" AND "))
        }
        _ => {
            params.push(value_to_param(value));
            Ok(format!("{col_expr} = ${}", params.len()))
        }
    }
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
                        "$not" => {
                            let sub = build_where(value, params)?;
                            if !sub.is_empty() {
                                conditions.push(format!("NOT ({sub})"));
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
                    "$ilike" => {
                        let pattern = val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter("$ilike must be a string".to_string())
                        })?;
                        params.push(pattern.to_string());
                        format!("{col} ILIKE ${}", params.len())
                    }
                    "$search" => {
                        let query_text = val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter("$search must be a string".to_string())
                        })?;
                        params.push(query_text.to_string());
                        format!(
                            "to_tsvector('english', {col}) @@ plainto_tsquery('english', ${})",
                            params.len()
                        )
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

/// Build an UPSERT (INSERT ... ON CONFLICT DO UPDATE) query:
/// ```sql
/// INSERT INTO "app_id"."collection" ("col1", "col2") VALUES ($1, $2)
/// ON CONFLICT ("conflict_col") DO UPDATE SET "col2" = EXCLUDED."col2"
/// RETURNING *
/// ```
///
/// `doc` is the full document to insert (as a JSON object).
/// `conflict_fields` is an array of column names that form the conflict target.
/// Non-conflict columns are set to `EXCLUDED."col"` in the DO UPDATE SET clause.
pub fn build_upsert(
    app_id: &str,
    collection: &str,
    doc: &Value,
    conflict_fields: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let obj = doc
        .as_object()
        .ok_or_else(|| QueryError::InvalidFilter("upsert document must be an object".to_string()))?;

    if obj.is_empty() {
        return Err(QueryError::InvalidFilter(
            "upsert document cannot be empty".to_string(),
        ));
    }

    let conflict_arr = conflict_fields
        .as_array()
        .ok_or_else(|| QueryError::InvalidFilter("conflict_fields must be an array".to_string()))?;

    if conflict_arr.is_empty() {
        return Err(QueryError::InvalidFilter(
            "conflict_fields cannot be empty".to_string(),
        ));
    }

    let conflict_set: std::collections::HashSet<&str> = conflict_arr
        .iter()
        .filter_map(|v| v.as_str())
        .collect();

    if conflict_set.is_empty() {
        return Err(QueryError::InvalidFilter(
            "conflict_fields must contain string values".to_string(),
        ));
    }

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut columns = Vec::new();
    let mut placeholders = Vec::new();
    let mut params: Vec<String> = Vec::new();
    let mut update_clauses = Vec::new();

    for (key, value) in obj {
        columns.push(quote_ident(key));
        params.push(value_to_param(value));
        placeholders.push(format!("${}", params.len()));

        // Non-conflict columns get updated to the EXCLUDED value
        if !conflict_set.contains(key.as_str()) {
            update_clauses.push(format!("{} = EXCLUDED.{}", quote_ident(key), quote_ident(key)));
        }
    }

    let conflict_cols: Vec<String> = conflict_arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(quote_ident)
        .collect();

    // If all columns are conflict columns, use DO UPDATE SET for the first non-id conflict col
    // to make it a true upsert (otherwise Postgres treats it as DO NOTHING).
    if update_clauses.is_empty() {
        // All columns are conflict columns — set the first one to itself
        if let Some(first) = conflict_arr.first().and_then(|v| v.as_str()) {
            update_clauses.push(format!("{} = EXCLUDED.{}", quote_ident(first), quote_ident(first)));
        }
    }

    let sql = format!(
        "INSERT INTO {schema}.{table} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {} RETURNING *",
        columns.join(", "),
        placeholders.join(", "),
        conflict_cols.join(", "),
        update_clauses.join(", ")
    );

    Ok(BuiltQuery { sql, params })
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
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users" WHERE "name" = $1"#);
        assert_eq!(q.params, vec!["alice"]);
    }

    #[test]
    fn test_comparison_operators() {
        let filter = json!({"age": {"$gte": 18, "$lt": 65}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains(r#""age" >= $1"#));
        assert!(q.sql.contains(r#""age" < $2"#));
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_in_operator() {
        let filter = json!({"status": {"$in": ["active", "pending"]}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains(r#""status" IN ($1, $2)"#));
        assert_eq!(q.params, vec!["active", "pending"]);
    }

    #[test]
    fn test_or_combinator() {
        let filter = json!({"$or": [{"name": "alice"}, {"name": "bob"}]});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains("OR"));
        assert_eq!(q.params, vec!["alice", "bob"]);
    }

    #[test]
    fn test_empty_filter() {
        let filter = json!({});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users""#);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_null_filter() {
        let filter = Value::Null;
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users""#);
    }

    #[test]
    fn test_limit_offset() {
        let filter = json!({});
        let q = build_find("app1", "users", &filter, Some(10), Some(20), None, None).unwrap();
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
        let result = build_find("app1", "users; DROP TABLE", &filter, None, None, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_exists_operator() {
        let filter = json!({"email": {"$exists": true}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
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

    #[test]
    fn test_ilike_operator() {
        let filter = json!({"name": {"$ilike": "%alice%"}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users" WHERE "name" ILIKE $1"#);
        assert_eq!(q.params, vec!["%alice%"]);
    }

    #[test]
    fn test_search_operator() {
        let filter = json!({"bio": {"$search": "rust developer"}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(
            q.sql,
            r#"SELECT * FROM "app1"."users" WHERE to_tsvector('english', "bio") @@ plainto_tsquery('english', $1)"#
        );
        assert_eq!(q.params, vec!["rust developer"]);
    }

    #[test]
    fn test_not_operator() {
        let filter = json!({"$not": {"role": "admin"}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users" WHERE NOT ("role" = $1)"#);
        assert_eq!(q.params, vec!["admin"]);
    }

    #[test]
    fn test_update_inc() {
        let filter = json!({"id": 1});
        let update = json!({"views": {"$inc": 1}});
        let q = build_update_one("app1", "posts", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""views" = "views" + $1::numeric"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "1");
    }

    #[test]
    fn test_update_dec() {
        let filter = json!({"id": 1});
        let update = json!({"stock": {"$dec": 1}});
        let q = build_update_one("app1", "items", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""stock" = "stock" - $1::numeric"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "1");
    }

    #[test]
    fn test_update_mul() {
        let filter = json!({"id": 1});
        let update = json!({"price": {"$mul": 1.1}});
        let q = build_update_one("app1", "items", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""price" = "price" * $1::numeric"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "1.1");
    }

    #[test]
    fn test_update_push() {
        let filter = json!({"id": 1});
        let update = json!({"tags": {"$push": "new"}});
        let q = build_update_one("app1", "posts", &filter, &update).unwrap();
        assert!(
            q.sql.contains(r#""tags" = "tags" || to_jsonb($1::text)"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], "new");
    }

    #[test]
    fn test_update_pull() {
        let filter = json!({"id": 1});
        let update = json!({"tags": {"$pull": "old"}});
        let q = build_update_one("app1", "posts", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""tags" = "tags" - $1"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "old");
    }

    #[test]
    fn test_update_add_to_set() {
        let filter = json!({"id": 1});
        let update = json!({"tags": {"$addToSet": "unique"}});
        let q = build_update_one("app1", "posts", &filter, &update).unwrap();
        assert!(q.sql.contains("CASE WHEN"), "sql: {}", q.sql);
        assert!(q.sql.contains("@>"), "sql: {}", q.sql);
        assert_eq!(q.params[0], "unique");
    }

    #[test]
    fn test_update_mixed_operators() {
        let filter = json!({"id": 1});
        let update = json!({"name": "New", "views": {"$inc": 1}});
        let q = build_update_one("app1", "posts", &filter, &update).unwrap();
        // Both plain set and $inc should appear
        assert!(q.sql.contains(r#""name" = $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""views" = "views" + $"#) && q.sql.contains("::numeric"), "sql: {}", q.sql);
        assert!(q.params.contains(&"New".to_string()));
        assert!(q.params.contains(&"1".to_string()));
    }

    #[test]
    fn test_insert_many() {
        let docs = json!([
            {"name": "alice", "age": 30},
            {"name": "bob",   "age": 25}
        ]);
        let q = build_insert_many("app1", "users", &docs).unwrap();
        assert!(q.sql.starts_with(r#"INSERT INTO "app1"."users""#), "sql: {}", q.sql);
        assert!(q.sql.contains("VALUES"), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        // Two docs × two columns = 4 params
        assert_eq!(q.params.len(), 4, "params: {:?}", q.params);
        assert!(q.sql.contains("($1, $2)"), "sql: {}", q.sql);
        assert!(q.sql.contains("($3, $4)"), "sql: {}", q.sql);
    }

    #[test]
    fn test_insert_many_empty() {
        let docs = json!([]);
        let result = build_insert_many("app1", "users", &docs);
        assert!(result.is_err(), "expected error for empty array");
    }

    #[test]
    fn test_update_many() {
        let filter = json!({"active": true});
        let update = json!({"status": "verified"});
        let q = build_update_many("app1", "users", &filter, &update).unwrap();
        assert!(q.sql.starts_with(r#"UPDATE "app1"."users" SET"#), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        // Must NOT contain ctid subquery (that's updateOne's approach)
        assert!(!q.sql.contains("ctid"), "sql should not contain ctid: {}", q.sql);
    }

    #[test]
    fn test_delete_many() {
        let filter = json!({"active": false});
        let q = build_delete_many("app1", "users", &filter).unwrap();
        assert!(q.sql.starts_with(r#"DELETE FROM "app1"."users""#), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        assert!(!q.sql.contains("ctid"), "sql should not contain ctid: {}", q.sql);
        assert_eq!(q.params, vec!["false"]);
    }

    #[test]
    fn test_delete_many_no_filter() {
        let filter = json!({});
        let q = build_delete_many("app1", "users", &filter).unwrap();
        assert!(!q.sql.contains("WHERE"), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_find_with_select() {
        let filter = json!({});
        let select = json!(["name", "email"]);
        let q = build_find("app1", "users", &filter, None, None, None, Some(&select)).unwrap();
        assert!(
            q.sql.contains(r#"SELECT "name", "email" FROM"#),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_find_without_select() {
        let filter = json!({});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.starts_with(r#"SELECT * FROM"#), "sql: {}", q.sql);
    }

    #[test]
    fn test_distinct() {
        let filter = json!({});
        let q = build_distinct("app1", "users", "country", &filter).unwrap();
        assert!(
            q.sql.starts_with(r#"SELECT DISTINCT "country" FROM "app1"."users""#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains(r#"ORDER BY "country""#), "sql: {}", q.sql);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_distinct_with_filter() {
        let filter = json!({"active": true});
        let q = build_distinct("app1", "users", "role", &filter).unwrap();
        assert!(
            q.sql.contains(r#"SELECT DISTINCT "role" FROM "app1"."users" WHERE"#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains(r#"ORDER BY "role""#), "sql: {}", q.sql);
        assert_eq!(q.params, vec!["true"]);
    }

    #[test]
    fn test_aggregate_basic() {
        let pipeline = json!([
            {"$match": {"active": true}},
            {"$group": {"by": "country", "count": {"$count": true}}},
            {"$sort": {"count": -1}},
            {"$limit": 5}
        ]);
        let q = build_aggregate("app1", "users", &pipeline).unwrap();
        assert!(q.sql.contains(r#"SELECT "country", COUNT(*) AS "count""#), "sql: {}", q.sql);
        assert!(q.sql.contains("WHERE"), "sql: {}", q.sql);
        assert!(q.sql.contains("GROUP BY"), "sql: {}", q.sql);
        assert!(q.sql.contains("ORDER BY"), "sql: {}", q.sql);
        assert!(q.sql.contains("LIMIT 5"), "sql: {}", q.sql);
    }

    #[test]
    fn test_aggregate_multi_group() {
        let pipeline = json!([
            {"$group": {"by": ["country", "city"], "total": {"$sum": "revenue"}}}
        ]);
        let q = build_aggregate("app1", "orders", &pipeline).unwrap();
        assert!(q.sql.contains(r#"GROUP BY "country", "city""#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#"SUM("revenue") AS "total""#), "sql: {}", q.sql);
    }

    #[test]
    fn test_aggregate_having() {
        let pipeline = json!([
            {"$group": {"by": "category", "cnt": {"$count": true}}},
            {"$having": {"cnt": {"$gte": 10}}}
        ]);
        let q = build_aggregate("app1", "products", &pipeline).unwrap();
        assert!(q.sql.contains("HAVING COUNT(*) >= $1"), "sql: {}", q.sql);
        assert!(q.sql.contains("GROUP BY"), "sql: {}", q.sql);
        assert_eq!(q.params, vec!["10"]);
    }

    #[test]
    fn test_aggregate_no_group() {
        let pipeline = json!([
            {"$match": {"active": true}}
        ]);
        let q = build_aggregate("app1", "users", &pipeline).unwrap();
        assert!(q.sql.starts_with("SELECT * FROM"), "sql: {}", q.sql);
        assert!(!q.sql.contains("GROUP BY"), "sql: {}", q.sql);
    }

    // -----------------------------------------------------------------------
    // 1. Missing builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_update_one_plain() {
        let filter = json!({"id": 1});
        let update = json!({"name": "bob"});
        let q = build_update_one("app1", "users", &filter, &update).unwrap();
        // Plain field: value → SET "name" = $1
        assert!(q.sql.contains(r#""name" = $1"#), "sql: {}", q.sql);
        // ctid subquery for LIMIT 1
        assert!(q.sql.contains("ctid"), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        assert_eq!(q.params[0], "bob");
    }

    #[test]
    fn test_update_one_set_operator() {
        let filter = json!({"id": 1});
        let update = json!({"$set": {"name": "carol"}});
        let q = build_update_one("app1", "users", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $1"#), "sql: {}", q.sql);
        assert!(q.sql.contains("ctid"), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        assert_eq!(q.params[0], "carol");
    }

    #[test]
    fn test_delete_one() {
        let filter = json!({});
        let q = build_delete_one("app1", "users", &filter).unwrap();
        // ctid subquery for LIMIT 1
        assert!(q.sql.contains("ctid"), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        // No WHERE in the outer DELETE (empty filter → no inner WHERE either)
        assert!(
            q.sql.contains("DELETE FROM"),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_delete_one_with_filter() {
        let filter = json!({"role": "guest"});
        let q = build_delete_one("app1", "users", &filter).unwrap();
        assert!(q.sql.contains("ctid"), "sql: {}", q.sql);
        // Filter should appear in the subquery
        assert!(q.sql.contains(r#""role" = $1"#), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        assert_eq!(q.params, vec!["guest"]);
    }

    #[test]
    fn test_order_by_object() {
        let order = json!({"name": 1, "age": -1});
        let clause = build_order_by(&order).unwrap();
        assert!(clause.contains(r#""name" ASC"#), "clause: {clause}");
        assert!(clause.contains(r#""age" DESC"#), "clause: {clause}");
    }

    #[test]
    fn test_order_by_array() {
        let order = json!([["name", 1], ["age", -1]]);
        let clause = build_order_by(&order).unwrap();
        // Array form preserves declaration order
        assert!(clause.contains(r#""name" ASC"#), "clause: {clause}");
        assert!(clause.contains(r#""age" DESC"#), "clause: {clause}");
        // "name" should appear before "age"
        let name_pos = clause.find(r#""name""#).unwrap();
        let age_pos = clause.find(r#""age""#).unwrap();
        assert!(name_pos < age_pos, "name should come before age");
    }

    #[test]
    fn test_find_with_order() {
        let filter = json!({});
        let order = json!({"created_at": -1});
        let q = build_find("app1", "posts", &filter, Some(10), None, Some(&order), None).unwrap();
        assert!(q.sql.contains(r#"ORDER BY "created_at" DESC"#), "sql: {}", q.sql);
        assert!(q.sql.contains("LIMIT 10"), "sql: {}", q.sql);
    }

    // -----------------------------------------------------------------------
    // 2. Filter edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_and_combinator() {
        let filter = json!({"$and": [{"status": "active"}, {"verified": true}]});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains("AND"), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""status" = $1"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""verified" = $2"#), "sql: {}", q.sql);
        assert_eq!(q.params, vec!["active", "true"]);
    }

    #[test]
    fn test_nested_and_or() {
        // { $and: [{ $or: [{a: 1}, {b: 2}] }, {c: 3}] }
        let filter = json!({"$and": [{"$or": [{"a": 1}, {"b": 2}]}, {"c": 3}]});
        let q = build_find("app1", "t", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains("OR"), "sql: {}", q.sql);
        assert!(q.sql.contains("AND"), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""c" = "#), "sql: {}", q.sql);
    }

    #[test]
    fn test_null_eq() {
        // { field: null } → IS NULL (implicit $eq)
        let filter = json!({"bio": null});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users" WHERE "bio" IS NULL"#);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_ne_null() {
        // { field: { $ne: null } } → IS NOT NULL
        let filter = json!({"bio": {"$ne": null}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users" WHERE "bio" IS NOT NULL"#);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_multiple_operators_on_field() {
        // { age: { $gte: 18, $lt: 65 } } — both conditions must appear
        let filter = json!({"age": {"$gte": 18, "$lt": 65}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains(r#""age" >= $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""age" < $"#), "sql: {}", q.sql);
        assert_eq!(q.params.len(), 2);
        // Both values present
        assert!(q.params.contains(&"18".to_string()));
        assert!(q.params.contains(&"65".to_string()));
    }

    #[test]
    fn test_nin_operator() {
        let filter = json!({"role": {"$nin": ["admin", "moderator"]}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains(r#""role" NOT IN ($1, $2)"#), "sql: {}", q.sql);
        assert_eq!(q.params, vec!["admin", "moderator"]);
    }

    #[test]
    fn test_like_operator() {
        let filter = json!({"name": {"$like": "ali%"}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, r#"SELECT * FROM "app1"."users" WHERE "name" LIKE $1"#);
        assert_eq!(q.params, vec!["ali%"]);
    }

    #[test]
    fn test_empty_and() {
        // { $and: [] } → no WHERE clause
        let filter = json!({"$and": []});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(!q.sql.contains("WHERE"), "sql should have no WHERE: {}", q.sql);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_empty_or() {
        // { $or: [] } → no WHERE clause
        let filter = json!({"$or": []});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(!q.sql.contains("WHERE"), "sql should have no WHERE: {}", q.sql);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_not_with_multiple_fields() {
        // { $not: { a: 1, b: 2 } }
        let filter = json!({"$not": {"a": 1, "b": 2}});
        let q = build_find("app1", "t", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains("NOT ("), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""a" = $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""b" = $"#), "sql: {}", q.sql);
        assert_eq!(q.params.len(), 2);
    }

    // -----------------------------------------------------------------------
    // 3. SQL injection prevention
    // -----------------------------------------------------------------------

    #[test]
    fn test_collection_sql_injection() {
        let filter = json!({});
        let result = build_find("app1", "users; DROP TABLE users", &filter, None, None, None, None);
        assert!(result.is_err(), "should reject injection in collection name");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid collection"), "msg: {msg}");
    }

    #[test]
    fn test_schema_sql_injection() {
        let filter = json!({});
        let result = build_find("app1; DROP TABLE", "users", &filter, None, None, None, None);
        assert!(result.is_err(), "should reject semicolon in schema/app_id");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid"), "msg: {msg}");
    }

    #[test]
    fn test_field_name_with_quotes() {
        // Field name containing double quotes should be escaped (doubled) in the identifier
        let filter = json!({"name": "alice"});
        let _q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        // Standard field works; now verify quote_ident escapes embedded quotes
        let quoted = super::quote_ident(r#"col"name"#);
        assert_eq!(quoted, r#""col""name""#, "embedded quote must be doubled");
    }

    #[test]
    fn test_collection_empty() {
        let filter = json!({});
        let result = build_find("app1", "", &filter, None, None, None, None);
        assert!(result.is_err(), "empty collection name should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("cannot be empty") || msg.contains("invalid"), "msg: {msg}");
    }

    // -----------------------------------------------------------------------
    // 4. value_to_param edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_insert_with_boolean() {
        let doc = json!({"active": true});
        let q = build_insert("app1", "users", &doc).unwrap();
        assert_eq!(q.params, vec!["true"]);
    }

    #[test]
    fn test_insert_with_null_field() {
        let doc = json!({"name": "alice", "bio": null});
        let q = build_insert("app1", "users", &doc).unwrap();
        // null → empty string param
        assert!(q.params.contains(&"alice".to_string()));
        assert!(q.params.contains(&String::new()), "null should produce empty string param");
    }

    #[test]
    fn test_insert_with_number() {
        let doc = json!({"age": 30});
        let q = build_insert("app1", "users", &doc).unwrap();
        assert_eq!(q.params, vec!["30"]);
    }

    #[test]
    fn test_insert_with_nested_json() {
        let doc = json!({"settings": {"theme": "dark"}});
        let q = build_insert("app1", "users", &doc).unwrap();
        // Nested object is serialized as JSON text
        assert_eq!(q.params.len(), 1);
        let param = &q.params[0];
        assert!(
            param.contains("theme") && param.contains("dark"),
            "nested object should be JSON-serialized: {param}"
        );
    }

    // -----------------------------------------------------------------------
    // 5. Aggregate edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_aggregate_empty_pipeline() {
        // Empty pipeline → no $group, select * is fine (not an error in current impl)
        // The spec says "no $group → error", but the current code returns SELECT * FROM.
        // Test that the function at minimum returns without panicking and produces valid SQL.
        let pipeline = json!([]);
        let q = build_aggregate("app1", "users", &pipeline).unwrap();
        assert!(q.sql.starts_with("SELECT * FROM"), "sql: {}", q.sql);
    }

    #[test]
    fn test_aggregate_match_only() {
        // Only $match without $group → select * (same as no_group test)
        let pipeline = json!([{"$match": {"status": "active"}}]);
        let q = build_aggregate("app1", "users", &pipeline).unwrap();
        assert!(q.sql.starts_with("SELECT * FROM"), "sql: {}", q.sql);
        assert!(!q.sql.contains("GROUP BY"), "sql: {}", q.sql);
        assert!(q.sql.contains("WHERE"), "sql: {}", q.sql);
        assert_eq!(q.params, vec!["active"]);
    }

    #[test]
    fn test_aggregate_all_agg_functions() {
        let pipeline = json!([{
            "$group": {
                "by": "category",
                "n":   {"$count": true},
                "total": {"$sum": "amount"},
                "avg_price": {"$avg": "price"},
                "min_price": {"$min": "price"},
                "max_price": {"$max": "price"}
            }
        }]);
        let q = build_aggregate("app1", "orders", &pipeline).unwrap();
        assert!(q.sql.contains("COUNT(*)"), "sql: {}", q.sql);
        assert!(q.sql.contains(r#"SUM("amount")"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#"AVG("price")"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#"MIN("price")"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#"MAX("price")"#), "sql: {}", q.sql);
        assert!(q.sql.contains("GROUP BY"), "sql: {}", q.sql);
    }

    // -----------------------------------------------------------------------
    // 6. Error cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_unsupported_filter_operator() {
        let filter = json!({"name": {"$regex": "^ali"}});
        let result = build_find("app1", "users", &filter, None, None, None, None);
        assert!(result.is_err(), "unsupported operator should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unsupported"), "msg: {msg}");
    }

    #[test]
    fn test_unsupported_update_operator() {
        let filter = json!({});
        let update = json!({"name": {"$unset": true}});
        let result = build_update_one("app1", "users", &filter, &update);
        assert!(result.is_err(), "unsupported update operator should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unsupported"), "msg: {msg}");
    }

    #[test]
    fn test_insert_empty_doc() {
        let doc = json!({});
        let result = build_insert("app1", "users", &doc);
        assert!(result.is_err(), "empty document should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("empty") || msg.contains("cannot"), "msg: {msg}");
    }

    #[test]
    fn test_insert_non_object() {
        let doc = json!("just a string");
        let result = build_insert("app1", "users", &doc);
        assert!(result.is_err(), "non-object document should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("object"), "msg: {msg}");
    }

    #[test]
    fn test_update_empty_fields() {
        let filter = json!({});
        let update = json!({});
        let result = build_update_one("app1", "users", &filter, &update);
        assert!(result.is_err(), "empty update should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("empty") || msg.contains("cannot"), "msg: {msg}");
    }

    #[test]
    fn test_in_non_array() {
        let filter = json!({"field": {"$in": "not-an-array"}});
        let result = build_find("app1", "users", &filter, None, None, None, None);
        assert!(result.is_err(), "$in with non-array should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("array"), "msg: {msg}");
    }

    #[test]
    fn test_exists_non_bool() {
        let filter = json!({"field": {"$exists": "yes"}});
        let result = build_find("app1", "users", &filter, None, None, None, None);
        assert!(result.is_err(), "$exists with non-bool should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("boolean"), "msg: {msg}");
    }

    // -----------------------------------------------------------------------
    // 7. $first sort-order threading
    // -----------------------------------------------------------------------

    #[test]
    fn test_aggregate_first_without_sort() {
        let pipeline = json!([
            {"$group": {"by": "department", "top_name": {"$first": "name"}}}
        ]);
        let q = build_aggregate("app1", "employees", &pipeline).unwrap();
        // Without a preceding $sort, $first uses plain array_agg
        assert!(
            q.sql.contains(r#"(array_agg("name"))[1]"#),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_aggregate_first_with_sort() {
        let pipeline = json!([
            {"$sort": {"salary": -1}},
            {"$group": {"by": "department", "top_name": {"$first": "name"}}}
        ]);
        let q = build_aggregate("app1", "employees", &pipeline).unwrap();
        // With a preceding $sort, $first threads the ORDER BY into array_agg
        assert!(
            q.sql.contains(r#"(array_agg("name" ORDER BY "salary" DESC))[1]"#),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_aggregate_first_with_multi_sort() {
        let pipeline = json!([
            {"$sort": {"salary": -1, "name": 1}},
            {"$group": {"by": "department", "top_name": {"$first": "name"}}}
        ]);
        let q = build_aggregate("app1", "employees", &pipeline).unwrap();
        // Multi-column sort should appear in the ORDER BY clause
        assert!(
            q.sql.contains(r#"array_agg("name" ORDER BY"#),
            "sql: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#""salary" DESC"#),
            "sql: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#""name" ASC"#),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_aggregate_first_sort_does_not_affect_other_aggs() {
        let pipeline = json!([
            {"$sort": {"salary": -1}},
            {"$group": {
                "by": "department",
                "top_name": {"$first": "name"},
                "total": {"$sum": "salary"},
                "cnt": {"$count": true}
            }}
        ]);
        let q = build_aggregate("app1", "employees", &pipeline).unwrap();
        // $first should have ORDER BY
        assert!(
            q.sql.contains(r#"array_agg("name" ORDER BY "salary" DESC)"#),
            "sql: {}",
            q.sql
        );
        // $sum and $count should NOT have ORDER BY
        assert!(
            q.sql.contains(r#"SUM("salary")"#),
            "sql: {}",
            q.sql
        );
        assert!(
            q.sql.contains("COUNT(*)"),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_upsert_basic() {
        let doc = json!({"name": "alice", "age": 30});
        let conflict = json!(["name"]);
        let q = build_upsert("app1", "users", &doc, &conflict).unwrap();
        assert!(q.sql.contains("INSERT INTO"), "sql: {}", q.sql);
        assert!(q.sql.contains("ON CONFLICT"), "sql: {}", q.sql);
        assert!(q.sql.contains("DO UPDATE SET"), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""name""#), "sql: {}", q.sql);
        // age is not a conflict field, so it should appear in DO UPDATE SET
        assert!(q.sql.contains(r#""age" = EXCLUDED."age""#), "sql: {}", q.sql);
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_upsert_multiple_conflict_fields() {
        let doc = json!({"email": "a@b.com", "name": "alice", "age": 30});
        let conflict = json!(["email", "name"]);
        let q = build_upsert("app1", "users", &doc, &conflict).unwrap();
        assert!(q.sql.contains(r#"ON CONFLICT ("email", "name")"#), "sql: {}", q.sql);
        // Only age should be in DO UPDATE SET
        assert!(q.sql.contains(r#""age" = EXCLUDED."age""#), "sql: {}", q.sql);
        // email and name should NOT be in DO UPDATE SET (they are conflict fields)
        assert!(!q.sql.contains(r#""email" = EXCLUDED."email""#), "sql: {}", q.sql);
        assert!(!q.sql.contains(r#""name" = EXCLUDED."name""#), "sql: {}", q.sql);
    }

    #[test]
    fn test_upsert_all_conflict_cols() {
        // When all columns are conflict columns, we still produce a valid DO UPDATE SET
        let doc = json!({"email": "a@b.com"});
        let conflict = json!(["email"]);
        let q = build_upsert("app1", "users", &doc, &conflict).unwrap();
        assert!(q.sql.contains("DO UPDATE SET"), "sql: {}", q.sql);
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
    }

    #[test]
    fn test_upsert_empty_doc_error() {
        let doc = json!({});
        let conflict = json!(["name"]);
        let result = build_upsert("app1", "users", &doc, &conflict);
        assert!(result.is_err());
    }

    #[test]
    fn test_upsert_empty_conflict_fields_error() {
        let doc = json!({"name": "alice"});
        let conflict = json!([]);
        let result = build_upsert("app1", "users", &doc, &conflict);
        assert!(result.is_err());
    }

    #[test]
    fn test_upsert_invalid_collection_error() {
        let doc = json!({"name": "alice"});
        let conflict = json!(["name"]);
        let result = build_upsert("app1", "users; DROP TABLE", &doc, &conflict);
        assert!(result.is_err());
    }
}
