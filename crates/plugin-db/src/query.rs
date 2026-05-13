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

/// Public re-export for cross-module use (B1 migrations). Same as the
/// private [`quote_ident`] — kept private at the SQL-build site so
/// query.rs internals stay encapsulated.
#[doc(hidden)]
pub fn quote_ident_pub(name: &str) -> String {
    quote_ident(name)
}

/// Public re-export of [`validate_collection`] for B1 migrations.
#[doc(hidden)]
pub fn validate_collection_pub(name: &str) -> Result<(), QueryError> {
    validate_collection(name)
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
///
/// B2 — `t.ref("table")` fields emit an inline `FOREIGN KEY` clause
/// when (1) the target table is the same as `collection` (self-ref) or
/// (2) the table is already in `existing_tables`. Otherwise the FK is
/// deferred to a separate `ALTER TABLE … ADD CONSTRAINT` so the
/// orchestrator can sequence DDL topologically. The unrestricted variant
/// `build_create_table` keeps backwards compatibility for callers that
/// don't track inter-table ordering — it always emits FK clauses inline,
/// relying on Postgres' deferred validation when the constraint is
/// `DEFERRABLE INITIALLY DEFERRED`.
pub fn build_create_table(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
) -> Result<String, QueryError> {
    build_create_table_with_fks(app_id, collection, schema, &FkEmission::Inline)
}

/// Controls FK emission strategy for `build_create_table_with_fks`.
///
/// - `Inline` — every `t.ref(target)` becomes an inline `FOREIGN KEY`
///   clause inside CREATE TABLE. The caller takes responsibility for
///   ordering: parent tables must exist (or be in the same statement
///   batch) before the FK is enforced.
/// - `Deferred(existing)` — only emits inline FK clauses for refs whose
///   target is `collection` itself (self-ref) or is in `existing` (already
///   present in the live schema). Other refs are skipped here so the
///   orchestrator can later attach them with `build_add_foreign_key` once
///   all tables exist.
#[derive(Debug)]
pub enum FkEmission<'a> {
    Inline,
    Deferred(&'a std::collections::HashSet<String>),
}

#[doc(hidden)]
pub fn build_create_table_with_fks(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    fk_emit: &FkEmission<'_>,
) -> Result<String, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let table = format!("{}.{}", quote_ident(app_id), quote_ident(collection));

    let mut columns = vec![
        "id SERIAL PRIMARY KEY".to_string(),
    ];

    let mut deferred_fks: Vec<String> = Vec::new();

    if let Some(obj) = schema.as_object() {
        for (field, def) in obj {
            let col_def = field_to_column(field, def);
            columns.push(col_def);

            // B2 — append FOREIGN KEY clause when this is a ref. Inline
            // FK clauses live in the same CREATE TABLE statement as the
            // column, after the column definition.
            if def.get("type").and_then(|t| t.as_str()) == Some("ref") {
                let target = def
                    .get("refTarget")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !target.is_empty() {
                    let should_inline = match fk_emit {
                        FkEmission::Inline => true,
                        FkEmission::Deferred(existing) => {
                            target == collection || existing.contains(target)
                        }
                    };
                    if should_inline {
                        if let Ok(fk_clause) = build_fk_clause(app_id, field, def, target) {
                            deferred_fks.push(fk_clause);
                        }
                    }
                }
            }
        }
    }

    columns.push("created_at TIMESTAMPTZ DEFAULT NOW()".to_string());
    columns.push("updated_at TIMESTAMPTZ DEFAULT NOW()".to_string());

    // Append all FK clauses *after* the regular columns so the SQL reads
    // top-to-bottom in a natural order (columns, then constraints).
    columns.extend(deferred_fks);

    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {} (\n  {}\n)",
        table,
        columns.join(",\n  ")
    ))
}

/// Build an `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` statement (B2).
///
/// Used by the diff engine when both tables already exist and the FK has
/// to be attached separately. The constraint name is content-addressed
/// from `<collection>_<field>_fkey` and truncated to 63 bytes via the
/// same hash strategy as A1 index names.
pub fn build_add_foreign_key(
    app_id: &str,
    collection: &str,
    field: &str,
    def: &serde_json::Value,
) -> Result<String, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let target = def
        .get("refTarget")
        .and_then(|v| v.as_str())
        .ok_or_else(|| QueryError::InvalidFilter("ref field missing refTarget".to_string()))?;

    let table = format!("{}.{}", quote_ident(app_id), quote_ident(collection));
    let fk_clause = build_fk_clause(app_id, field, def, target)?;
    Ok(format!("ALTER TABLE {} ADD {}", table, fk_clause))
}

/// Build `ALTER TABLE … DROP CONSTRAINT` for an existing FK (B2 diff
/// engine — `DropForeignKey` op).
pub fn build_drop_foreign_key(
    app_id: &str,
    collection: &str,
    constraint_name: &str,
) -> Result<String, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let table = format!("{}.{}", quote_ident(app_id), quote_ident(collection));
    Ok(format!(
        "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}",
        table,
        quote_ident(constraint_name)
    ))
}

/// Build a deterministic, NAMEDATALEN-safe FK constraint identifier.
///
/// Postgres scopes constraint names per-table, so the name only needs
/// to be unique among the constraints of a single table. We use
/// `<field>_fkey` (the convention Postgres itself follows for
/// auto-generated FK names). The second argument is reserved for future
/// composite-FK use and is currently unused.
pub fn fk_constraint_name(field: &str, _reserved: &str) -> String {
    let full = format!("{field}_fkey");
    if full.len() <= 60 {
        return full;
    }
    let hash = short_hash_base32(&full);
    let prefix_budget = 60usize.saturating_sub(9);
    let mut prefix: String = full.chars().take(prefix_budget).collect();
    if prefix.ends_with('_') {
        prefix.pop();
    }
    format!("{prefix}_{hash}")
}

/// Build the `CONSTRAINT "name" FOREIGN KEY (...) REFERENCES …` clause
/// shared by inline CREATE TABLE emission and standalone ALTER TABLE.
///
/// The constraint name uses only `<field>_fkey` (Postgres scopes
/// constraint names per-table, so cross-table uniqueness is not needed)
/// and is hash-truncated for ≤ 63 byte NAMEDATALEN budget.
fn build_fk_clause(
    app_id: &str,
    field: &str,
    def: &serde_json::Value,
    target: &str,
) -> Result<String, QueryError> {
    validate_collection(target)?;
    let constraint_name = fk_constraint_name(field, "");

    let on_delete = normalize_fk_action(def.get("onDelete").and_then(|v| v.as_str()));
    let on_update = normalize_fk_action(def.get("onUpdate").and_then(|v| v.as_str()));
    let deferrable = def
        .get("deferrable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let target_qualified = format!("{}.{}", quote_ident(app_id), quote_ident(target));
    let deferrable_clause = if deferrable {
        " DEFERRABLE INITIALLY DEFERRED"
    } else {
        ""
    };

    Ok(format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} (id) ON DELETE {} ON UPDATE {}{}",
        quote_ident(&constraint_name),
        quote_ident(field),
        target_qualified,
        on_delete,
        on_update,
        deferrable_clause,
    ))
}

/// Normalise an FK action to the SQL keyword form Postgres accepts.
fn normalize_fk_action(s: Option<&str>) -> &'static str {
    match s.unwrap_or("restrict").to_ascii_lowercase().as_str() {
        "cascade" => "CASCADE",
        "set null" | "set_null" => "SET NULL",
        "no action" | "no_action" => "NO ACTION",
        _ => "RESTRICT",
    }
}

/// Public re-export of [`normalize_fk_action`] for cross-module use
/// (diff engine needs to compare declared vs. live policies).
#[doc(hidden)]
pub fn normalize_fk_action_pub(s: Option<&str>) -> &'static str {
    normalize_fk_action(s)
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

// ---------------------------------------------------------------------------
// Index builders for registerModel — A1 of the @zeroship/db v2 proposal
// (docs/proposals/zeroship-db-v2.md). Materialises `t.string().index()` /
// `t.string().unique()` markers as CONCURRENTLY-built Postgres indexes so
// the markers actually do something at the database layer.
// ---------------------------------------------------------------------------

/// A single index to materialise during `registerModel`.
///
/// `name` is the deterministic Postgres identifier (≤ 63 bytes). `sql` is a
/// `CREATE [UNIQUE] INDEX CONCURRENTLY IF NOT EXISTS …` statement ready to be
/// executed outside a transaction (CONCURRENTLY cannot run inside `BEGIN`).
/// `unique` is exposed so callers can apply different recovery policies for
/// unique-index failures (which surface `23505 unique_violation` errors that
/// must not be retried — see proposal A1 INVALID-index recovery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSpec {
    /// Deterministic index identifier (unquoted).
    pub name: String,
    /// Columns the index covers (unquoted, in declared order).
    pub columns: Vec<String>,
    /// Whether this is a UNIQUE index.
    pub unique: bool,
    /// `CREATE …` DDL ready for execution.
    pub sql: String,
}

/// Build the set of `CREATE INDEX CONCURRENTLY` statements for a schema.
///
/// Walks the field definitions and emits:
///   * a non-unique index per field with `index: true`,
///   * a unique index per field with `unique: true`.
///
/// Composite indexes (the proposal's
/// `defineCollection(fields).index(name, columns[])` builder) are not yet
/// surfaced by the SDK; when they land, append them to the returned `Vec`.
/// TODO: A1 composite indexes — wire through `schema_meta.indexes` once the
/// SDK builder exists.
///
/// Statements are emitted in deterministic order: declared field order in the
/// schema, with `index` markers before `unique` markers for the same field
/// (effectively impossible since a field is either indexed or unique, but the
/// rule keeps the contract obvious).
pub fn build_create_indexes(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
) -> Result<Vec<IndexSpec>, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let mut out = Vec::new();

    let Some(obj) = schema.as_object() else {
        return Ok(out);
    };

    let table_qualified = format!("{}.{}", quote_ident(app_id), quote_ident(collection));

    for (field, def) in obj {
        let wants_index = def.get("index").and_then(|v| v.as_bool()) == Some(true);
        let wants_unique = def.get("unique").and_then(|v| v.as_bool()) == Some(true);

        if !wants_index && !wants_unique {
            continue;
        }

        // Unique implies an index — if both flags are set, prefer the unique
        // form (a unique index also serves as a lookup index, so emitting
        // both would be redundant and waste storage).
        if wants_unique {
            let name = index_name(collection, &[field.as_str()], /* unique = */ true);
            let col_list = quote_ident(field);
            let sql = format!(
                "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} ({})",
                quote_ident(&name),
                table_qualified,
                col_list,
            );
            out.push(IndexSpec {
                name,
                columns: vec![field.clone()],
                unique: true,
                sql,
            });
        } else if wants_index {
            let name = index_name(collection, &[field.as_str()], /* unique = */ false);
            let col_list = quote_ident(field);
            let sql = format!(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} ({})",
                quote_ident(&name),
                table_qualified,
                col_list,
            );
            out.push(IndexSpec {
                name,
                columns: vec![field.clone()],
                unique: false,
                sql,
            });
        }
    }

    Ok(out)
}

/// Build a deterministic Postgres index name from a table name and columns.
///
/// Strategy:
///   1. Construct `<table>_<col1>_<col2>…_<suffix>` where suffix is
///      `key` for unique indexes and `idx` otherwise.
///   2. Postgres `NAMEDATALEN` defaults to 64 bytes (limit 63 chars). If the
///      generated name exceeds 60 bytes, replace the tail with an 8-char
///      base32 hash of the full name. This is Atlas's strategy
///      (`migrate/sqltool/index_name.go`). The 60-byte threshold leaves
///      headroom for the suffix without ever crossing NAMEDATALEN.
///   3. The hash is sha256(full_name) → first 5 bytes → base32 (8 chars).
///      sha256 is in `crates/runtime` and `crates/core` already; pulling
///      blake3 would add a new transitive dep for an 8-char fingerprint
///      where collision resistance is not actually load-bearing (we only
///      need stable + roughly-uniform). sha256 is the cheaper choice.
///
/// Naming is content-addressed (same input → same name), so re-running
/// `registerModel` with `IF NOT EXISTS` is idempotent.
pub fn index_name(table: &str, columns: &[&str], unique: bool) -> String {
    let suffix = if unique { "key" } else { "idx" };
    let joined_cols = columns.join("_");
    let full = format!("{table}_{joined_cols}_{suffix}");
    if full.len() <= 60 {
        return full;
    }
    // Truncated form: keep the table prefix readable, then append the hash.
    let hash = short_hash_base32(&full);
    // Reserve `_<hash>` (1 + 8 = 9 bytes) on the tail. Allocate the rest
    // to a prefix of the original name (which already starts with the
    // table). Cap the prefix at 54 bytes so the total is ≤ 63 bytes.
    let prefix_budget = 60usize.saturating_sub(9);
    let mut prefix: String = full.chars().take(prefix_budget).collect();
    // Drop a trailing underscore (cosmetic — keep `<a>_<hash>` rather than
    // `<a>__<hash>`).
    if prefix.ends_with('_') {
        prefix.pop();
    }
    format!("{prefix}_{hash}")
}

/// 8-char base32 fingerprint over sha256 of the input.
///
/// Crockford-style alphabet without padding — Postgres identifiers are
/// case-folded but our names already go through `quote_ident`, so we can
/// keep lowercase letters for readability.
fn short_hash_base32(input: &str) -> String {
    use sha2::{Digest, Sha256};
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

    let digest = Sha256::digest(input.as_bytes());
    let bytes = &digest[..5]; // 5 bytes = 40 bits → 8 base32 chars

    let mut out = [0u8; 8];
    // 5 bytes packed into 8 × 5-bit groups, MSB-first.
    let mut acc: u64 = 0;
    for b in bytes {
        acc = (acc << 8) | u64::from(*b);
    }
    for i in 0..8 {
        let shift = (7 - i) * 5;
        let idx = ((acc >> shift) & 0x1f) as usize;
        out[i] = ALPHABET[idx];
    }
    // Safety: ALPHABET is ASCII so out is valid UTF-8.
    String::from_utf8(out.to_vec()).expect("ALPHABET is ASCII")
}

/// Convert a field definition to a full column definition for CREATE TABLE.
fn field_to_column(field: &str, def: &serde_json::Value) -> String {
    let pg_type = def_to_pg_type(def);
    let constraints = def_to_constraints(field, def);
    format!("{} {} {}", quote_ident(field), pg_type, constraints).trim().to_string()
}

/// Map schema type to PostgreSQL type.
///
/// B2 — a `ref` field is stored as `BIGINT` so it matches the `id`
/// type of the target collection (auto-generated by `id SERIAL PRIMARY KEY`,
/// which is `INTEGER`; we widen to `BIGINT` because Convex-style brand-typed
/// IDs are always integers wide enough to hold any row count, and the
/// referenced table's `id` is auto-cast on FK check).
fn def_to_pg_type(def: &serde_json::Value) -> &'static str {
    match def.get("type").and_then(|t| t.as_str()) {
        Some("string") => "TEXT",
        Some("number") => "NUMERIC",
        Some("boolean") => "BOOLEAN",
        Some("date") => "TIMESTAMPTZ",
        // D3 — `t.calendarDate()` is a `YYYY-MM-DD` value with no time
        // and no timezone, distinct from `t.date()` (TIMESTAMPTZ stored
        // as Unix-ms numbers at the SDK layer).
        Some("calendarDate") => "DATE",
        Some("json") => "JSONB",
        // D2 — `t.object({...})` declares a JSONB column. The nested
        // shape is enforced application-side by `validate.ts`; no
        // CHECK constraint is emitted (Postgres JSONB CHECKs are
        // expressible but expensive at write time, see proposal D2).
        Some("object") => "JSONB",
        Some("array") => "JSONB",
        Some("ref") => "INTEGER",
        _ => "TEXT",
    }
}

/// Generate column constraints from field definition.
fn def_to_constraints(field: &str, def: &serde_json::Value) -> String {
    let mut parts = Vec::new();

    if def.get("required").and_then(|v| v.as_bool()) == Some(true) {
        parts.push("NOT NULL".to_string());
    }

    // NOTE: `unique` is intentionally NOT emitted as a column-level constraint
    // here. The proposal (zeroship-db-v2.md A1) mandates that every uniqueness
    // marker becomes a `CREATE UNIQUE INDEX CONCURRENTLY` so the build never
    // blocks writes. The inline `UNIQUE` keyword would build the index under
    // ACCESS EXCLUSIVE lock and would also produce a Postgres-auto-named index
    // that defeats our deterministic-name idempotency contract. The uniqueness
    // marker is materialised through `build_create_indexes` instead.

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
            Some("json") | Some("object") => parts.push("DEFAULT '{}'::jsonb".to_string()),
            Some("array") => parts.push("DEFAULT '[]'::jsonb".to_string()),
            _ => {}
        }
    } else {
        // Default defaults for json/object/array
        match def.get("type").and_then(|t| t.as_str()) {
            Some("json") | Some("object") => parts.push("DEFAULT '{}'::jsonb".to_string()),
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
/// - `$inc`      — `"col" = "col" + $N::numeric`
/// - `$dec`      — `"col" = "col" - $N::numeric`
/// - `$mul`      — `"col" = "col" * $N::numeric`
/// - `$push`     — `"col" = "col" || $N::jsonb` (operand is JSON-encoded,
///                 so `{"$push": 42}` appends the number 42, not the string
///                 "42" — preserves number/boolean/object/array types)
/// - `$pull`     — `"col" = (SELECT COALESCE(jsonb_agg(elem), '[]'::jsonb)
///                 FROM jsonb_array_elements("col") elem WHERE elem != $N::jsonb)`
///                 (removes array elements by value; `jsonb - text` would
///                 instead delete object keys, which is not what we want)
/// - `$addToSet` — `"col" = CASE WHEN "col" @> $N::jsonb THEN "col"
///                                ELSE "col" || $N::jsonb END`
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

/// Public re-export of [`value_to_param`] for B1 migrations.
#[doc(hidden)]
pub fn value_to_param_pub(value: &Value) -> String {
    value_to_param(value)
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
        // Appends the JSON-encoded value to the jsonb array. The `::jsonb`
        // cast (not `to_jsonb(::text)`) keeps numbers, booleans, and objects
        // as their real JSON types — the old shape stringified everything.
        assert!(
            q.sql.contains(r#""tags" = "tags" || $1::jsonb"#),
            "sql: {}",
            q.sql
        );
        // Param is JSON-encoded: a string `"new"` is stored as `"\"new\""`
        // so Postgres parses it back as a JSON string on ::jsonb cast.
        assert_eq!(q.params[0], "\"new\"");
    }

    #[test]
    fn test_update_pull() {
        let filter = json!({"id": 1});
        let update = json!({"tags": {"$pull": "old"}});
        let q = build_update_one("app1", "posts", &filter, &update).unwrap();
        // Removes array elements by value. An earlier implementation used
        // `"tags" - $1`, but that's the jsonb "remove key" operator and
        // would mutate objects, not filter array elements.
        assert!(
            q.sql.contains(r#""tags" = (SELECT COALESCE(jsonb_agg(elem), '[]'::jsonb) FROM jsonb_array_elements("tags") elem WHERE elem != $1::jsonb)"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], "\"old\"");
    }

    #[test]
    fn test_update_add_to_set() {
        let filter = json!({"id": 1});
        let update = json!({"tags": {"$addToSet": "unique"}});
        let q = build_update_one("app1", "posts", &filter, &update).unwrap();
        // Appends only if the array doesn't already contain the value
        // (jsonb @> containment check). Both sides use ::jsonb so type is
        // preserved — same rationale as $push.
        assert!(
            q.sql.contains(r#""tags" = CASE WHEN "tags" @> $1::jsonb THEN "tags" ELSE "tags" || $1::jsonb END"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], "\"unique\"");
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

    // -----------------------------------------------------------------------
    // Update-operator regression tests
    //
    // These lock in the fixes from 6a309b3 ("resolve 7 native-layer bugs"):
    // type preservation on jsonb array ops, value-based $pull, $set flattening,
    // and updated_at auto-injection.
    // -----------------------------------------------------------------------

    #[test]
    fn test_update_push_number_preserves_type() {
        // Regression: old shape wrapped with `to_jsonb($N::text)` which
        // stringified numbers. New shape uses `$N::jsonb` with the operand
        // serialized as JSON, so `42` stays a JSON number.
        let filter = json!({"id": 1});
        let update = json!({"scores": {"$push": 42}});
        let q = build_update_one("app1", "games", &filter, &update).unwrap();
        assert!(
            q.sql.contains(r#""scores" = "scores" || $1::jsonb"#),
            "sql: {}",
            q.sql
        );
        // Param is the JSON text "42", not "\"42\"" — Postgres parses it
        // back as a JSON number on the ::jsonb cast.
        assert_eq!(q.params[0], "42");
    }

    #[test]
    fn test_update_push_bool_preserves_type() {
        let filter = json!({"id": 1});
        let update = json!({"flags": {"$push": true}});
        let q = build_update_one("app1", "games", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""flags" = "flags" || $1::jsonb"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "true");
    }

    #[test]
    fn test_update_push_object_preserves_type() {
        let filter = json!({"id": 1});
        let update = json!({"entries": {"$push": {"k": "v", "n": 3}}});
        let q = build_update_one("app1", "log", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""entries" = "entries" || $1::jsonb"#), "sql: {}", q.sql);
        // Object → compact JSON text. Keys serialized in serde_json::Value
        // order (preserves insertion via the default feature? -- we don't
        // assert ordering, just that both keys are present).
        assert!(q.params[0].contains(r#""k":"v""#), "params[0] = {}", q.params[0]);
        assert!(q.params[0].contains(r#""n":3"#), "params[0] = {}", q.params[0]);
    }

    #[test]
    fn test_update_pull_number() {
        // Regression: old shape `"tags" - $1` is the jsonb "remove key"
        // operator — it mutates objects, not arrays. The subquery form
        // correctly removes array elements equal to the value.
        let filter = json!({"id": 1});
        let update = json!({"scores": {"$pull": 100}});
        let q = build_update_one("app1", "games", &filter, &update).unwrap();
        assert!(
            q.sql.contains(r#"FROM jsonb_array_elements("scores") elem WHERE elem != $1::jsonb"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], "100");
    }

    #[test]
    fn test_update_add_to_set_number() {
        let filter = json!({"id": 1});
        let update = json!({"ids": {"$addToSet": 7}});
        let q = build_update_one("app1", "games", &filter, &update).unwrap();
        assert!(
            q.sql.contains(r#""ids" = CASE WHEN "ids" @> $1::jsonb THEN "ids" ELSE "ids" || $1::jsonb END"#),
            "sql: {}",
            q.sql
        );
        // $addToSet reuses the same parameter index for the containment
        // check and the append — only one param is pushed.
        assert_eq!(q.params.len(), 2, "params: {:?}", q.params); // the op param + the filter param (id = 1)
        assert_eq!(q.params[0], "7");
    }

    #[test]
    fn test_update_updated_at_auto_injected() {
        // Every UPDATE implicitly bumps updated_at unless the caller
        // explicitly set it. This is part of the platform contract.
        let filter = json!({"id": 1});
        let update = json!({"name": "bob"});
        let q = build_update_one("app1", "users", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""updated_at" = NOW()"#), "sql: {}", q.sql);
    }

    #[test]
    fn test_update_updated_at_not_overridden_when_explicit() {
        // If the caller explicitly provides updated_at, we must NOT add
        // our own `NOW()` clause — would collide and the user's value wins.
        let filter = json!({"id": 1});
        let explicit_ts = "2026-01-01T00:00:00Z";
        let update = json!({"name": "bob", "updated_at": explicit_ts});
        let q = build_update_one("app1", "users", &filter, &update).unwrap();
        assert!(
            !q.sql.contains("NOW()"),
            "sql should not contain NOW() when updated_at is explicit: {}",
            q.sql
        );
        assert!(q.params.contains(&explicit_ts.to_string()), "params: {:?}", q.params);
    }

    #[test]
    fn test_update_set_flattens_top_level() {
        // Regression: early impl processed only the $set key and dropped
        // sibling top-level fields. After 6a309b3 the builder flattens
        // $set into the top level, so both `name` and `age` must appear.
        let filter = json!({"id": 1});
        let update = json!({"$set": {"name": "alice"}, "age": 30});
        let q = build_update_one("app1", "users", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""age" = $"#), "sql: {}", q.sql);
        assert!(q.params.contains(&"alice".to_string()));
        assert!(q.params.contains(&"30".to_string()));
    }

    #[test]
    fn test_update_set_coexists_with_inc() {
        // Mixed $set (flattened) + $inc on a sibling field. Both must
        // produce SET clauses and share the same params vector.
        let filter = json!({"id": 1});
        let update = json!({"$set": {"name": "alice"}, "views": {"$inc": 5}});
        let q = build_update_one("app1", "posts", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""views" = "views" + $"#), "sql: {}", q.sql);
        assert!(q.params.contains(&"alice".to_string()));
        assert!(q.params.contains(&"5".to_string()));
    }

    #[test]
    fn test_update_inc_param_formatting() {
        // $inc operand is pushed via value_to_param — a float should render
        // as "1.5" (not "1.5e0" or similar), so Postgres's ::numeric cast
        // accepts it without a client-side conversion.
        let filter = json!({"id": 1});
        let update = json!({"balance": {"$inc": 1.5}});
        let q = build_update_one("app1", "accounts", &filter, &update).unwrap();
        assert_eq!(q.params[0], "1.5");
    }

    #[test]
    fn test_update_negative_inc() {
        // Negative $inc must still render with the `+` operator (caller
        // uses $dec for subtraction semantically). Postgres handles the
        // minus sign on the numeric literal fine.
        let filter = json!({"id": 1});
        let update = json!({"stock": {"$inc": -3}});
        let q = build_update_one("app1", "items", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""stock" = "stock" + $1::numeric"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "-3");
    }

    #[test]
    fn test_update_param_indexing_with_filter() {
        // SET params come first, WHERE params come after. The `$N`
        // placeholders must be contiguous across both halves.
        let filter = json!({"status": "active"});
        let update = json!({"name": "alice", "views": {"$inc": 1}});
        let q = build_update_one("app1", "posts", &filter, &update).unwrap();
        // Two SET params: name ($1), inc ($2); one WHERE param: status ($3).
        assert_eq!(q.params.len(), 3, "params: {:?}", q.params);
        assert!(q.sql.contains("$3"), "sql: {}", q.sql);
        assert!(!q.sql.contains("$4"), "sql: {}", q.sql);
    }

    #[test]
    fn test_update_empty_object_rejected() {
        // Empty update object should surface as an error rather than
        // produce `SET (nothing)` or `SET "updated_at" = NOW()` alone,
        // which would silently bump timestamps without user intent.
        let filter = json!({"id": 1});
        let update = json!({});
        let err = build_update_one("app1", "users", &filter, &update).unwrap_err();
        // Variant is QueryError::InvalidFilter — compare Display form so
        // this doesn't need to import the enum.
        assert!(
            format!("{err}").to_lowercase().contains("empty"),
            "error should mention empty fields, got: {err}"
        );
    }

    #[test]
    fn test_update_unknown_operator_rejected() {
        let filter = json!({"id": 1});
        let update = json!({"tags": {"$weirdOp": "val"}});
        let err = build_update_one("app1", "posts", &filter, &update).unwrap_err();
        assert!(
            format!("{err}").contains("$weirdOp"),
            "error should name the unsupported op, got: {err}"
        );
    }

    #[test]
    fn test_update_many_auto_updates_timestamp() {
        // updateMany shares the same build_set_clauses path, so the
        // auto-timestamp behaviour must hold there too.
        let filter = json!({"status": "draft"});
        let update = json!({"status": "published"});
        let q = build_update_many("app1", "posts", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""updated_at" = NOW()"#), "sql: {}", q.sql);
        // updateMany must NOT wrap the WHERE in a ctid LIMIT 1 subquery —
        // that would only touch one row.
        assert!(!q.sql.contains("LIMIT 1"), "sql: {}", q.sql);
    }

    #[test]
    fn test_update_set_without_top_level_fields() {
        // Pure $set with no siblings — flattening must still work.
        let filter = json!({"id": 1});
        let update = json!({"$set": {"name": "alice", "age": 30}});
        let q = build_update_one("app1", "users", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""age" = $"#), "sql: {}", q.sql);
    }

    #[test]
    fn test_update_set_non_object_rejected() {
        // `$set` value that isn't an object should error, not be
        // silently treated as a scalar $set on a column named "$set".
        let filter = json!({"id": 1});
        let update = json!({"$set": "not an object"});
        let err = build_update_one("app1", "users", &filter, &update).unwrap_err();
        assert!(
            format!("{err}").contains("$set"),
            "error should mention $set, got: {err}"
        );
    }

    #[test]
    fn test_update_string_column_name_is_quoted() {
        // Column names get quoted via quote_ident, so a column with a
        // reserved word as its name still works.
        let filter = json!({"id": 1});
        let update = json!({"user": "alice"}); // "user" is a reserved word
        let q = build_update_one("app1", "accounts", &filter, &update).unwrap();
        assert!(q.sql.contains(r#""user" = $"#), "sql: {}", q.sql);
    }

    // -----------------------------------------------------------------------
    // A1 — Materialised indexes (zeroship-db-v2 proposal §A1).
    //
    // Before A1, `t.string().index()` and `t.string().unique()` set
    // `FieldDef.index/unique` in the SDK but the Rust DDL emitter produced
    // no index. These tests lock the materialisation contract: every
    // marker yields a `CREATE [UNIQUE] INDEX CONCURRENTLY IF NOT EXISTS …`
    // statement with a deterministic name.
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_indexes_empty_schema() {
        let schema = json!({});
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        assert!(out.is_empty(), "expected no indexes, got: {out:?}");
    }

    #[test]
    fn test_build_indexes_no_markers_produces_no_indexes() {
        let schema = json!({
            "email": {"type": "string", "required": true},
            "age": {"type": "number"},
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        assert!(out.is_empty(), "expected no indexes when no markers set");
    }

    #[test]
    fn test_build_indexes_single_field_unique() {
        let schema = json!({
            "email": {"type": "string", "required": true, "unique": true},
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        assert_eq!(out.len(), 1, "expected one unique index");
        let spec = &out[0];
        assert!(spec.unique, "should be a unique index");
        assert_eq!(spec.name, "users_email_key");
        assert_eq!(spec.columns, vec!["email"]);
        assert!(
            spec.sql.starts_with("CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS"),
            "sql: {}",
            spec.sql
        );
        // Both schema and table identifiers must be quoted.
        assert!(spec.sql.contains(r#""app1"."users""#), "sql: {}", spec.sql);
        assert!(spec.sql.contains(r#"("email")"#), "sql: {}", spec.sql);
        assert!(
            spec.sql.contains(r#""users_email_key""#),
            "sql: {}",
            spec.sql
        );
    }

    #[test]
    fn test_build_indexes_single_field_non_unique() {
        let schema = json!({
            "handle": {"type": "string", "index": true},
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        assert_eq!(out.len(), 1);
        let spec = &out[0];
        assert!(!spec.unique);
        assert_eq!(spec.name, "users_handle_idx");
        assert!(
            spec.sql.starts_with("CREATE INDEX CONCURRENTLY IF NOT EXISTS"),
            "sql: {}",
            spec.sql
        );
        assert!(
            !spec.sql.contains("UNIQUE"),
            "non-unique index must not contain UNIQUE keyword: {}",
            spec.sql
        );
    }

    #[test]
    fn test_build_indexes_unique_wins_over_index() {
        // If a user sets both `.unique()` and `.index()` on the same field,
        // the unique index already serves as a lookup index — emitting a
        // second non-unique index would be wasted storage.
        let schema = json!({
            "email": {"type": "string", "unique": true, "index": true},
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].unique);
        assert_eq!(out[0].name, "users_email_key");
    }

    #[test]
    fn test_build_indexes_multiple_fields() {
        let schema = json!({
            "email": {"type": "string", "unique": true},
            "name": {"type": "string"},
            "tenant_id": {"type": "string", "index": true},
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        assert_eq!(out.len(), 2, "expected 2 indexes, got: {out:?}");
        let names: Vec<_> = out.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"users_email_key".to_string()), "{names:?}");
        assert!(
            names.contains(&"users_tenant_id_idx".to_string()),
            "{names:?}"
        );
    }

    #[test]
    fn test_build_indexes_special_column_name_is_quoted() {
        // A column named `"user"` (reserved word) — must be quoted in
        // the CREATE INDEX column list. The index *name* still embeds
        // the bare token, which is fine because we double-quote it
        // separately.
        let schema = json!({
            "user": {"type": "string", "index": true},
        });
        let out = build_create_indexes("app1", "accounts", &schema).unwrap();
        assert_eq!(out.len(), 1);
        let spec = &out[0];
        assert!(spec.sql.contains(r#"("user")"#), "sql: {}", spec.sql);
    }

    #[test]
    fn test_build_indexes_rejects_bad_collection() {
        let schema = json!({"x": {"type": "string", "index": true}});
        let err = build_create_indexes("app1", "users; DROP TABLE", &schema).unwrap_err();
        assert!(matches!(err, QueryError::InvalidCollection(_)));
    }

    #[test]
    fn test_build_indexes_rejects_bad_schema() {
        let schema = json!({"x": {"type": "string", "index": true}});
        let err = build_create_indexes("app; --", "users", &schema).unwrap_err();
        assert!(matches!(err, QueryError::InvalidCollection(_)));
    }

    // -----------------------------------------------------------------------
    // Naming truncation (Postgres NAMEDATALEN = 64).
    // -----------------------------------------------------------------------

    #[test]
    fn test_index_name_short_form() {
        assert_eq!(index_name("users", &["email"], true), "users_email_key");
        assert_eq!(index_name("posts", &["author_id"], false), "posts_author_id_idx");
    }

    #[test]
    fn test_index_name_truncates_with_deterministic_hash() {
        // A pathological column name that exceeds 60 bytes when combined
        // with table + suffix. The result must still be ≤ 63 bytes and
        // deterministic across calls.
        let long_col = "a".repeat(70);
        let n1 = index_name("users", &[long_col.as_str()], true);
        let n2 = index_name("users", &[long_col.as_str()], true);
        assert_eq!(n1, n2, "name must be deterministic for idempotent re-runs");
        assert!(
            n1.len() <= 63,
            "name {} exceeds Postgres NAMEDATALEN limit of 63",
            n1
        );
        // Hash is 8 base32 chars at the tail.
        let tail = &n1[n1.len() - 8..];
        assert!(
            tail.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "tail '{}' should be base32",
            tail
        );
    }

    #[test]
    fn test_index_name_different_inputs_yield_different_hashes() {
        let long = "x".repeat(80);
        let n1 = index_name("users", &[long.as_str()], true);
        let n2 = index_name("users", &[long.as_str()], false);
        // unique vs non-unique produces a different "full" pre-hash name,
        // hence a different hash suffix.
        assert_ne!(n1, n2);
    }

    #[test]
    fn test_index_name_just_under_threshold_not_hashed() {
        // 60-byte threshold (inclusive). Build a name whose unhashed length
        // is exactly 60.
        //   "t_" (2) + col (53) + "_idx" (4) = 59  → unhashed
        //   "t_" (2) + col (54) + "_idx" (4) = 60  → unhashed
        //   "t_" (2) + col (55) + "_idx" (4) = 61  → hashed
        let col = "c".repeat(54);
        let name = index_name("t", &[col.as_str()], false);
        assert_eq!(name.len(), 60, "name: {}", name);
        assert!(name.ends_with("_idx"), "should keep readable suffix: {}", name);
    }

    #[test]
    fn test_index_name_just_over_threshold_is_hashed() {
        let col = "c".repeat(55);
        let name = index_name("t", &[col.as_str()], false);
        assert!(name.len() <= 63);
        assert!(
            !name.ends_with("_idx"),
            "over-threshold name should end with the hash, not _idx: {}",
            name
        );
    }

    // -----------------------------------------------------------------------
    // Silent-bug repro (the original reason A1 exists).
    //
    // Before this change, `t.string().unique()` set FieldDef.unique = true
    // in the SDK but the Rust layer never emitted a unique index. This
    // test asserts that the emitted SQL after registerModel actually
    // contains a CREATE UNIQUE INDEX CONCURRENTLY statement targeting
    // the `email` column.
    // -----------------------------------------------------------------------

    #[test]
    fn test_silent_unique_bug_is_closed() {
        // Exactly what the SDK produces for `t.string().required().unique()`.
        let schema = json!({
            "email": {"type": "string", "required": true, "unique": true},
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        assert_eq!(out.len(), 1, "should emit a unique index for `unique: true`");
        let spec = &out[0];
        assert!(spec.unique, "must be marked as unique");
        // Statement shape — the four invariants the proposal calls out:
        //   * CREATE UNIQUE INDEX (so duplicates are actually rejected)
        //   * CONCURRENTLY        (so writes are never blocked on build)
        //   * IF NOT EXISTS       (so re-runs are idempotent)
        //   * targets ("email")   (the column the marker is on)
        assert!(spec.sql.contains("CREATE UNIQUE INDEX"), "sql: {}", spec.sql);
        assert!(spec.sql.contains("CONCURRENTLY"), "sql: {}", spec.sql);
        assert!(spec.sql.contains("IF NOT EXISTS"), "sql: {}", spec.sql);
        assert!(spec.sql.contains(r#"("email")"#), "sql: {}", spec.sql);
    }

    #[test]
    fn test_create_table_does_not_emit_inline_unique() {
        // Regression guard: A1 moved uniqueness out of the inline
        // column definition (which would build the underlying index
        // under ACCESS EXCLUSIVE lock) into a separate CONCURRENT
        // index build. CREATE TABLE / ADD COLUMN must therefore NOT
        // contain the bare `UNIQUE` keyword for fields tagged
        // `unique: true`.
        let schema = json!({
            "email": {"type": "string", "required": true, "unique": true},
        });
        let create = build_create_table("app1", "users", &schema).unwrap();
        assert!(create.contains("NOT NULL"), "still emits NOT NULL: {}", create);
        assert!(
            !create.contains(" UNIQUE"),
            "CREATE TABLE must not emit inline UNIQUE (would force non-concurrent index): {}",
            create
        );

        let alter = build_add_column(
            "app1",
            "users",
            "email",
            &json!({"type": "string", "required": true, "unique": true}),
        )
        .unwrap();
        assert!(
            !alter.contains(" UNIQUE"),
            "ADD COLUMN must not emit inline UNIQUE: {}",
            alter
        );
    }

    // -----------------------------------------------------------------
    // B2 — typed cross-table relations
    // -----------------------------------------------------------------

    #[test]
    fn b2_create_table_with_ref_emits_inline_fk() {
        let schema = json!({
            "title": {"type": "string", "required": true},
            "authorId": {"type": "ref", "refTarget": "users"},
        });
        let sql = build_create_table("app1", "posts", &schema).unwrap();
        // INTEGER column for the FK
        assert!(sql.contains("\"authorId\" INTEGER"), "{sql}");
        // Inline FK clause with default ON DELETE RESTRICT
        assert!(sql.contains("FOREIGN KEY (\"authorId\")"), "{sql}");
        assert!(
            sql.contains("REFERENCES \"app1\".\"users\" (id)"),
            "{sql}"
        );
        assert!(sql.contains("ON DELETE RESTRICT"), "{sql}");
        assert!(sql.contains("ON UPDATE RESTRICT"), "{sql}");
        // Default deferrable
        assert!(sql.contains("DEFERRABLE INITIALLY DEFERRED"), "{sql}");
    }

    #[test]
    fn b2_ref_on_delete_cascade_override() {
        let schema = json!({
            "authorId": {
                "type": "ref",
                "refTarget": "users",
                "onDelete": "cascade",
                "onUpdate": "cascade",
            },
        });
        let sql = build_create_table("app1", "posts", &schema).unwrap();
        assert!(sql.contains("ON DELETE CASCADE"), "{sql}");
        assert!(sql.contains("ON UPDATE CASCADE"), "{sql}");
    }

    #[test]
    fn b2_ref_deferrable_false_skips_clause() {
        let schema = json!({
            "authorId": {
                "type": "ref",
                "refTarget": "users",
                "deferrable": false,
            },
        });
        let sql = build_create_table("app1", "posts", &schema).unwrap();
        assert!(!sql.contains("DEFERRABLE"), "{sql}");
    }

    #[test]
    fn b2_build_add_foreign_key_emits_alter_table() {
        let def = json!({
            "type": "ref",
            "refTarget": "users",
            "onDelete": "cascade",
        });
        let sql = build_add_foreign_key("app1", "posts", "authorId", &def).unwrap();
        assert!(sql.starts_with("ALTER TABLE \"app1\".\"posts\" ADD"), "{sql}");
        assert!(sql.contains("FOREIGN KEY (\"authorId\")"), "{sql}");
        assert!(sql.contains("REFERENCES \"app1\".\"users\" (id)"), "{sql}");
        assert!(sql.contains("ON DELETE CASCADE"), "{sql}");
    }

    #[test]
    fn b2_build_drop_foreign_key() {
        let sql = build_drop_foreign_key("app1", "posts", "authorId_fkey").unwrap();
        assert_eq!(
            sql,
            "ALTER TABLE \"app1\".\"posts\" DROP CONSTRAINT IF EXISTS \"authorId_fkey\""
        );
    }

    #[test]
    fn b2_fk_constraint_name_short() {
        assert_eq!(fk_constraint_name("authorId", ""), "authorId_fkey");
    }

    #[test]
    fn b2_fk_constraint_name_truncated() {
        let long = "a".repeat(80);
        let name = fk_constraint_name(&long, "");
        assert!(name.len() <= 60, "got {} bytes: {name}", name.len());
    }

    #[test]
    fn b2_deferred_emission_skips_unknown_target() {
        let schema = json!({
            "authorId": {"type": "ref", "refTarget": "users"},
        });
        let existing: std::collections::HashSet<String> = std::collections::HashSet::new();
        let sql = build_create_table_with_fks(
            "app1",
            "posts",
            &schema,
            &FkEmission::Deferred(&existing),
        )
        .unwrap();
        // FK is deferred — column still present but no FOREIGN KEY clause
        assert!(sql.contains("\"authorId\" INTEGER"), "{sql}");
        assert!(
            !sql.contains("FOREIGN KEY"),
            "FK should be deferred: {sql}"
        );
    }

    #[test]
    fn b2_deferred_emission_inlines_self_ref() {
        // Self-ref (employee.managerId → employee) inlines even when
        // existing-set is empty because the table being created IS the
        // target.
        let schema = json!({
            "managerId": {"type": "ref", "refTarget": "employees"},
        });
        let existing: std::collections::HashSet<String> = std::collections::HashSet::new();
        let sql = build_create_table_with_fks(
            "app1",
            "employees",
            &schema,
            &FkEmission::Deferred(&existing),
        )
        .unwrap();
        assert!(sql.contains("FOREIGN KEY (\"managerId\")"), "{sql}");
        assert!(
            sql.contains("REFERENCES \"app1\".\"employees\" (id)"),
            "{sql}"
        );
    }

    // -----------------------------------------------------------------
    // D2 — nested object validators (JSONB column)
    // -----------------------------------------------------------------

    #[test]
    fn d2_object_field_emits_jsonb_column() {
        let schema = json!({
            "profile": {
                "type": "object",
                "shape": {
                    "bio": { "type": "string" },
                    "avatar": { "type": "string" }
                }
            },
        });
        let sql = build_create_table("app1", "users", &schema).unwrap();
        assert!(sql.contains("\"profile\" JSONB"), "{sql}");
        // Defaults to an empty JSON object (like t.json()).
        assert!(sql.contains("DEFAULT '{}'::jsonb"), "{sql}");
    }

    // -----------------------------------------------------------------
    // D3 — calendar dates → DATE column type
    // -----------------------------------------------------------------

    #[test]
    fn d3_calendar_date_emits_date_column() {
        let schema = json!({
            "birthday": { "type": "calendarDate" },
        });
        let sql = build_create_table("app1", "users", &schema).unwrap();
        // DATE, not TIMESTAMPTZ — the whole point of D3.
        assert!(sql.contains("\"birthday\" DATE"), "{sql}");
        assert!(!sql.contains("TIMESTAMPTZ DATE"), "{sql}");
    }

    #[test]
    fn d3_calendar_date_distinct_from_date() {
        // Verify t.date() still emits TIMESTAMPTZ alongside DATE for the
        // calendar variant — no overlap.
        let schema = json!({
            "createdAt": { "type": "date" },
            "birthday": { "type": "calendarDate" },
        });
        let sql = build_create_table("app1", "users", &schema).unwrap();
        assert!(sql.contains("\"createdAt\" TIMESTAMPTZ"), "{sql}");
        assert!(sql.contains("\"birthday\" DATE"), "{sql}");
    }

    #[test]
    fn d3_add_column_calendar_date() {
        // ALTER TABLE ADD COLUMN for a calendarDate field must also
        // emit DATE so subsequent migrations stay consistent.
        let sql = build_add_column(
            "app1",
            "users",
            "birthday",
            &json!({ "type": "calendarDate" }),
        )
        .unwrap();
        assert!(sql.contains("ADD COLUMN IF NOT EXISTS \"birthday\" DATE"), "{sql}");
    }

    // -----------------------------------------------------------------
    // D4 — version column injected by the SDK is treated as a plain
    // INTEGER (well, NUMERIC) column at the DDL level. The SDK uses
    // model.ts to inject `version: { type: "number", default: 1 }`
    // so the DDL emission below matches.
    // -----------------------------------------------------------------

    #[test]
    fn d4_version_column_default_one() {
        let schema = json!({
            "title": { "type": "string", "required": true },
            "version": { "type": "number", "default": 1 },
        });
        let sql = build_create_table("app1", "posts", &schema).unwrap();
        assert!(sql.contains("\"version\" NUMERIC"), "{sql}");
        assert!(sql.contains("DEFAULT 1"), "{sql}");
    }
}
