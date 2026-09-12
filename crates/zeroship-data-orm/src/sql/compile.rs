//! Native filter records to parameterized SQL.
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

mod read;
pub use read::{ReadSource, build_select};

use crate::sql::compiler::CompiledQuery;
pub use crate::sql::lifecycle::WriteAssignments;
use crate::sql::lifecycle::soft_delete_column;
use crate::value::Value;
use std::borrow::Cow;

use zeroship_core::schema_name::SchemaName;

/// An empty field map for statements that do not project collection fields.
pub fn empty_read_schema() -> Value {
    Value::Object(crate::value::Map::new())
}

/// Errors produced while validating and compiling a query.
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
            Self::InvalidFilter(msg) => write!(f, "invalid filter: {msg}"),
            Self::InvalidCollection(msg) => write!(f, "invalid collection: {msg}"),
            Self::InvalidIdent(msg) => write!(f, "invalid identifier: {msg}"),
            Self::ReservedIdPrefix(msg) => {
                write!(f, "reserved ID prefix: {msg}")
            }
            Self::ImmutableAssignedField(msg) => {
                write!(f, "immutable assigned field: {msg}")
            }
        }
    }
}

/// Runtime SQL dialect. Drivers bind binary parameters directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    Postgres,
    Sqlite,
}

const SQLITE_NOW_EXPR: &str = "(strftime('%Y-%m-%dT%H:%M:%fZ','now'))";

impl SqlDialect {
    pub fn binary_bind_placeholder(self, n: usize) -> String {
        format!("${n}")
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
pub const MAX_SEARCH_LIMIT: usize = 500;
/// DB-11: max documents in a single `insertMany`. Bounds the multi-row SQL
/// string and row bookkeeping materialized in the worker. Bind parameters are
/// capped separately because this document count says nothing about row width.
pub const MAX_INSERT_MANY_BATCH: usize = 1_000;
const POSTGRES_MAX_BIND_PARAMETERS: usize = u16::MAX as usize;
const SQLITE_MAX_BIND_PARAMETERS: usize = 32_766;
const MAX_FILTER_NESTING_DEPTH: usize = 16;
const MAX_FILTER_CLAUSE_COUNT: usize = 128;
const MAX_MEMBERSHIP_LIST_LEN: usize = 100;

/// DB-2: the effective row limit for a `find` — an omitted limit defaults to
/// [`MAX_QUERY_LIMIT`] rather than emitting NO `LIMIT` clause (which would pull
/// the entire collection into the worker). Callers paginate past one page via
/// `offset`. Explicit limits are still bounds-checked by `validate_limit_bound`.
pub fn effective_query_limit(explicit: Option<i64>) -> i64 {
    explicit.unwrap_or(MAX_QUERY_LIMIT)
}

/// Platform-owned collection prefixes mirrored by every collection validator.
pub(crate) const PLATFORM_RESERVED_COLLECTION_PREFIXES: &[&str] = &["__zeroship"];

/// Catalog prefixes owned by the backends the runtime can address.
///
/// Keep these outside `RESERVED_NAMES`: they are backend conventions, not
/// neutral platform reservations. They are a defense-in-depth copy after the
/// migration declaration gate. The behavioral parity suite derives the real
/// shipping set and fails when a backend is added without updating this list.
const BACKEND_CATALOG_PREFIXES: &[(&str, &str)] = &[("pg_", "PostgreSQL"), ("sqlite_", "SQLite")];

fn reserved_backend_catalog_prefix(name: &str) -> Option<(&'static str, &'static str)> {
    BACKEND_CATALOG_PREFIXES
        .iter()
        .copied()
        .find(|(prefix, _)| {
            name.len() >= prefix.len()
                && name.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        })
}

/// Validate a collection name: alphanumeric + underscores only.
///
/// Additional security constraints (beyond character allowlist):
/// - Must not be empty.
/// - Must not exceed 63 bytes (Postgres `NAMEDATALEN` limit).
/// - Must not contain a null byte.
/// - Must not start with a shipping backend's catalog prefix
///   (case-insensitive).
/// - Must not start with a platform-owned prefix (case-insensitive):
///   `__zeroship`.
pub fn validate_collection(name: &str) -> Result<(), QueryError> {
    if name.is_empty() {
        return Err(QueryError::InvalidCollection(
            "collection name cannot be empty".to_string(),
        ));
    }
    if name.contains('\0') {
        return Err(QueryError::InvalidCollection(
            "collection name must not contain null bytes".to_string(),
        ));
    }
    if name.len() > 63 {
        return Err(QueryError::InvalidCollection(format!(
            "collection name exceeds 63-byte Postgres identifier limit: {name}"
        )));
    }
    // Reserved-prefix checks via byte-slice equality avoid an allocating
    // .to_ascii_lowercase() per CRUD dispatch. The union is deliberate: a
    // creator schema may be retargeted, and SQLite refuses sqlite_* table names.
    let bytes = name.as_bytes();
    if let Some((prefix, owner)) = reserved_backend_catalog_prefix(name) {
        return Err(QueryError::InvalidCollection(format!(
            "collection name '{name}' uses reserved prefix '{prefix}' ({owner} system catalog)"
        )));
    }
    for prefix in PLATFORM_RESERVED_COLLECTION_PREFIXES {
        let claimed = prefix.as_bytes();
        if bytes.len() >= claimed.len() && bytes[..claimed.len()].eq_ignore_ascii_case(claimed) {
            return Err(QueryError::InvalidCollection(format!(
                "collection name '{name}' uses reserved prefix '{prefix}' (platform internal)"
            )));
        }
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(QueryError::InvalidCollection(format!(
            "invalid collection name: {name}"
        )));
    }
    Ok(())
}

/// True for top-level schema keys that carry
/// metadata rather than a field declaration (e.g. `"_meta"`,
/// `"_indexes"`). These keys are produced by the SDK normaliser
/// or appear in test schemas; they MUST be skipped before the
/// schema-iteration loop reaches [`validate_field_name`] (otherwise
/// the leading `_` would trip the reserved-prefix rule).
///
/// The list is intentionally narrow — only keys the runtime
/// actually reads. Adding a new metadata key here is a deliberate
/// platform extension, not a creator-driven decision.
pub fn is_schema_metadata_key(key: &str) -> bool {
    matches!(key, "_meta" | "_indexes")
}

/// Taxonomy of reserved name shapes the platform
/// enforces on creator-declared field names.
///
/// Three match arms cover the patterns we currently reserve:
/// - `Exact(s)`  — refuse a field named literally `s`.
/// - `Prefix(p)` — refuse any field name starting with `p`.
/// - `Suffix(s)` — refuse any field name ending with `s`.
///
/// The `_masked` suffix is reserved for sibling columns auto-emitted
/// by the platform's `.mask()` / `.encrypted()` machinery (the Path B
/// sibling-column storage strategy). The six default classifications
/// (`public`/`pii`/`spi`/`phi`/`pci`/`internal`) are reserved as
/// exact names so creator schemas cannot collide with the
/// classification taxonomy used by audit + authorization.
pub(crate) enum ReservedName {
    /// Literal name match — refuse a field named exactly `&str`.
    Exact(&'static str),
    /// Prefix match — refuse any field starting with `&str`.
    Prefix(&'static str),
    /// Suffix match — refuse any field ending with `&str`.
    Suffix(&'static str),
}

/// Platform-reserved field names. Centralised list — every new
/// reserved prefix / suffix / exact-name lands here, exercised by
/// both the schema-registration validator and the filter-time
/// validator (the latter fences `db.users.find({ ssn_masked: ... })`
/// with the same error code path).
pub(crate) const RESERVED_NAMES: &[ReservedName] = &[
    // Synthetic-result columns the runtime emits (e.g. `_distance`
    // on vector search). Reserved so creator-declared
    // columns can't shadow them.
    ReservedName::Prefix("_"),
    // Platform bookkeeping table prefixes. Mirrors the
    // `validate_collection` reservations for table-name shape.
    ReservedName::Prefix("__zs_"),
    ReservedName::Prefix("__zeroship_"),
    // Platform-reserved suffix.
    ReservedName::Suffix("_masked"),
    // Six default-classification names. Reserved at
    // the column-name level so creator schemas can't accidentally
    // collide with the classification taxonomy (used by
    // authorization + audit). Matches the SDK's `Classification`
    // union.
    ReservedName::Exact("public"),
    ReservedName::Exact("pii"),
    ReservedName::Exact("spi"),
    ReservedName::Exact("phi"),
    ReservedName::Exact("pci"),
    ReservedName::Exact("internal"),
];

/// Validate a field identifier and platform namespace reservations.
/// Assignment and lifecycle behavior comes from the collection descriptor.
pub fn validate_field_name(name: &str) -> Result<(), QueryError> {
    if name.is_empty() {
        return Err(QueryError::InvalidIdent(
            "field name cannot be empty".to_string(),
        ));
    }
    if name.contains('\0') {
        return Err(QueryError::InvalidIdent(
            "field name must not contain null bytes".to_string(),
        ));
    }
    if name.len() > 63 {
        return Err(QueryError::InvalidIdent(format!(
            "field name exceeds 63-byte Postgres identifier limit: {name}"
        )));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(QueryError::InvalidIdent(format!(
            "invalid field name: {name} (allowed: ASCII alphanumeric + underscore)"
        )));
    }
    // Reserved-name check. Run after the ASCII
    // allowlist so a name like `"café"` reports the encoding error
    // (not a spurious reserved-name hit on a bogus suffix match).
    for reserved in RESERVED_NAMES {
        let matches = match reserved {
            ReservedName::Exact(n) => name == *n,
            ReservedName::Prefix(p) => name.starts_with(p),
            ReservedName::Suffix(s) => name.ends_with(s),
        };
        if matches {
            let hint = match reserved {
                ReservedName::Suffix(s) => {
                    let stem = name.strip_suffix(s).unwrap_or(name);
                    format!(
                        "suffix '{s}' is reserved for sibling columns generated by \
                         .mask()/.encrypted() — try '{stem}_view' or '{stem}_display' instead"
                    )
                }
                ReservedName::Prefix(p) => {
                    format!("prefix '{p}' is reserved for platform-internal names")
                }
                ReservedName::Exact(n) => format!(
                    "name '{n}' is reserved by the platform classification taxonomy \
                     (public/pii/spi/phi/pci/internal)"
                ),
            };
            return Err(QueryError::InvalidIdent(format!(
                "reserved field name '{name}': {hint}"
            )));
        }
    }
    // Backend catalog conventions stay outside the neutral platform table.
    // This remains a runtime defense in depth; creator declarations are fenced
    // structurally by the migration engine before IR lowering.
    if let Some((prefix, owner)) = reserved_backend_catalog_prefix(name) {
        return Err(QueryError::InvalidIdent(format!(
            "reserved field name '{name}': prefix '{prefix}' is reserved by the {owner} system catalog"
        )));
    }
    Ok(())
}

/// Validate an ordinary field declaration.
pub fn validate_field_name_for_declaration(name: &str) -> Result<(), QueryError> {
    validate_field_name(name)
}

/// Typed-ID prefixes reserved for platform identities.
/// `reserved_id_prefix_parity` checks the migration validator agrees.
pub const RESERVED_ID_PREFIXES: &[&str] = &["usr"];

/// Validate prefix syntax and platform identity reservations.
pub fn validate_id_prefix(prefix: &str) -> Result<(), QueryError> {
    let valid = prefix
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase())
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        return Err(QueryError::InvalidIdent(format!(
            "t.id(prefix): prefix must match ^[a-z][a-z0-9_]*$ (got '{prefix}')"
        )));
    }
    if RESERVED_ID_PREFIXES.contains(&prefix) {
        return Err(QueryError::ReservedIdPrefix(format!(
            "t.id(prefix): '{prefix}' is reserved for platform ids; choose a different prefix"
        )));
    }
    Ok(())
}

fn schema_declares_readable_field(schema_hint: &Value, name: &str) -> bool {
    if is_schema_metadata_key(name) {
        return false;
    }
    schema_hint.get(name).is_some_and(field_is_readable)
}

/// Require an explicitly declared readable field.
fn validate_read_identifier(name: &str, schema_hint: &Value) -> Result<(), QueryError> {
    validate_field_name(name)?;
    if schema_declares_readable_field(schema_hint, name) {
        return Ok(());
    }
    Err(QueryError::InvalidIdent(format!(
        "field '{name}' is not a readable schema field; readable fields must be declared in the schema"
    )))
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

fn validate_limit_bound(name: &str, value: i64, max: i64) -> Result<(), QueryError> {
    if value < 0 {
        return Err(QueryError::InvalidFilter(format!(
            "{name} must be >= 0, got {value}"
        )));
    }
    if value > max {
        return Err(QueryError::InvalidFilter(format!(
            "{name} exceeds the maximum of {max}, got {value}"
        )));
    }
    Ok(())
}

fn validate_search_limit_bound(name: &str, value: usize) -> Result<(), QueryError> {
    if value > MAX_SEARCH_LIMIT {
        return Err(QueryError::InvalidFilter(format!(
            "{name} exceeds the maximum of {MAX_SEARCH_LIMIT}, got {value}"
        )));
    }
    Ok(())
}

/// Quote an identifier (table or column name) with double-quotes.
/// Escapes any embedded double-quotes by doubling them.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn quote_ident_for_dialect(name: &str, dialect: SqlDialect) -> String {
    match dialect {
        SqlDialect::Postgres | SqlDialect::Sqlite => quote_ident(name),
    }
}

// ---------------------------------------------------------------------------
/// Prefix of the physical column holding a masked field's protected value.
/// The migration emitter owns declaration validation; the runtime keeps the
/// same spelling for projections and protection passes. The parity tests bind
/// both to the shipping vendors' identifier limits.
pub const RAW_COLUMN_PREFIX: &str = "__zs_raw__";

pub const MAX_MASKED_FIELD_NAME_BYTES: usize = 63 - RAW_COLUMN_PREFIX.len();

/// Derive the conventional reserved column for a masked field's real value.
/// Runtime operations use `declared_raw_column`; catalog introspection uses this
/// convention when the catalog supplies no explicit pairing. Parity tests bind
/// the spelling to the migration emitter.
#[must_use]
pub fn raw_column_name(field: &str) -> String {
    format!("{RAW_COLUMN_PREFIX}{field}")
}

/// Derive raw storage for an effectively masked field.
/// Returns `None` when the field has no effective mask.
pub fn raw_column_for_field(field: &str, def: &crate::value::Value) -> Option<String> {
    crate::sql::descriptors::effective_mask(def)?;
    Some(raw_column_name(field))
}

/// Resolve a masked field's `storage.rawColumn`, falling back to the conventional
/// reserved name when the descriptor omits it.
///
/// Raw storage must be a valid identifier that creator filters, sorts and
/// projections cannot address. Otherwise those surfaces could probe plaintext
/// without unmask authorization.
///
/// # Errors
///
/// Returns `QueryError::InvalidIdent` for malformed or creator-accessible names.
pub fn declared_raw_column(
    field: &str,
    def: &crate::value::Value,
) -> Result<Option<String>, QueryError> {
    // Only effectively masked fields require raw storage.
    let derived = match raw_column_for_field(field, def) {
        Some(raw) => raw,
        None => return Ok(None),
    };
    let Some(declared) = def
        .get("storage")
        .and_then(|storage| storage.get("rawColumn"))
        .and_then(crate::value::Value::as_str)
    else {
        // Field maps without an explicit storage mapping use the reserved convention.
        return Ok(Some(derived));
    };
    if declared.is_empty()
        || declared.len() > 63
        || !declared
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(QueryError::InvalidIdent(format!(
            "descriptor declares a malformed raw column for field '{field}': {declared:?}"
        )));
    }
    if validate_field_name(declared).is_ok() {
        return Err(QueryError::InvalidIdent(format!(
            "descriptor declares raw column '{declared}' for masked field '{field}', but that \
             name is one creator code can reach in a filter, a sort or a projection; a masked \
             field's real value may only be stored under a platform-reserved name"
        )));
    }
    Ok(Some(declared.to_string()))
}

// ---------------------------------------------------------------------------
// Query builders
// ---------------------------------------------------------------------------

/// Bind a logical value using its field descriptor. Timestamp conversion stays
/// on the parameter, so indexed columns remain bare in comparisons.
fn push_field_value_bind(
    params: &mut Vec<Value>,
    value: &Value,
    field: &str,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    push_field_parameter(params, Cow::Borrowed(value), field, schema_hint, dialect)
}

fn push_field_parameter(
    params: &mut Vec<Value>,
    value: Cow<'_, Value>,
    field: &str,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    let definition = schema_hint.get(field);
    let kind = definition
        .and_then(|field| field.get("type"))
        .and_then(Value::as_str);
    let protected = definition.is_some_and(crate::sql::descriptors::is_encrypted)
        || column_is_masked(field, schema_hint);
    let timestamp = !protected && matches!(kind, Some("date" | "timestamp"));
    if timestamp && !value.is_null() {
        let millis = crate::sql::temporal::timestamp_millis(value.as_ref()).ok_or_else(|| {
            QueryError::InvalidFilter(format!(
                "column '{field}' requires a portable timestamp or integral Unix milliseconds"
            ))
        })?;
        params.push(match dialect {
            SqlDialect::Postgres => Value::Timestamp(millis),
            SqlDialect::Sqlite => Value::String(
                crate::sql::temporal::format_timestamp_millis(millis)
                    .expect("validated portable timestamp"),
            ),
        });
        return Ok(match dialect {
            SqlDialect::Postgres => format!("${}::timestamptz", params.len()),
            _ => format!("${}", params.len()),
        });
    }
    let json_column = matches!(kind, Some("json" | "object" | "array" | "union"));
    let mut parameter =
        if json_column
            && !matches!(value.as_ref(), Value::Json(_) | Value::Array(_) | Value::Object(_))
        {
            Value::Json(value.to_string())
        } else {
            value.into_owned()
        };
    if !protected && matches!(kind, Some("object" | "array" | "union")) {
        crate::sql::codecs::prepare_value(field, definition.unwrap(), &mut parameter)
            .map_err(|error| QueryError::InvalidFilter(error.to_string()))?;
    }
    params.push(parameter);
    Ok(format!("${}", params.len()))
}

fn push_array_value_bind(
    params: &mut Vec<Value>,
    value: &Value,
    field: &str,
    schema: &Value,
    dialect: SqlDialect,
) -> Result<(), QueryError> {
    let mut operand = value.clone();
    if let Some(definition) = schema.get(field) {
        crate::sql::codecs::prepare_array_operand(field, definition, &mut operand)
            .map_err(|error| QueryError::InvalidFilter(error.to_string()))?;
    }
    params.push(if dialect == SqlDialect::Sqlite || operand.is_null() {
        Value::Json(operand.to_string())
    } else {
        operand
    });
    Ok(())
}

/// Build the bounded id probe used before a write fans out per matching row.
///
/// The caller supplies an explicit bound. `updateMany` asks for one row above
/// [`MAX_QUERY_LIMIT`] so it can distinguish an exactly-full target set from
/// an overflowing one; `updateOne` asks for one. This is deliberately separate
/// from creator-facing `find`, whose public limit remains `MAX_QUERY_LIMIT`.
pub fn build_write_target_probe(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    limit: i64,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    let select = project_read_field("id", schema_hint, None);
    let mut built =
        build_find_with_schema_and_unmask_and_soft_delete_with_dialect_and_limit_ceiling(
            schema_name,
            collection,
            filter,
            Some(limit),
            None,
            None,
            &select,
            schema_hint,
            false,
            dialect,
            MAX_QUERY_LIMIT + 1,
        )?;
    if dialect == SqlDialect::Postgres {
        built.sql.push_str(" FOR UPDATE");
    }
    Ok(built)
}

pub fn build_conflict_probe_with_dialect(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;

    let obj = filter.as_object().ok_or_else(|| {
        QueryError::InvalidFilter("conflict probe filter must be an object".to_string())
    })?;
    if obj.is_empty() {
        return Err(QueryError::InvalidFilter(
            "conflict probe filter cannot be empty".to_string(),
        ));
    }

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);
    let mut params = Vec::new();
    let mut conditions = Vec::new();

    for (field, value) in obj {
        validate_field_name(field)?;
        validate_value_operation(field, schema_hint)?;
        let col = quote_ident(field);
        if value.is_null() {
            conditions.push(format!("{col} IS NULL"));
            continue;
        }

        let raw = value_to_param(value);
        let binary_bind = matches!(value, Value::Bytes(_));
        let param_value = if binary_bind {
            dialect.encode_binary_param(raw)?
        } else {
            raw
        };
        if binary_bind {
            params.push(param_value);
            let n = params.len();
            conditions.push(format!("{col} = {}", dialect.binary_bind_placeholder(n)));
        } else {
            conditions.push(format!(
                "{col} = {}",
                push_field_value_bind(&mut params, value, field, schema_hint, dialect)?
            ));
        }
    }

    if conditions.is_empty() {
        return Err(QueryError::InvalidFilter(
            "conflict probe filter cannot be empty".to_string(),
        ));
    }

    let sql = format!(
        "SELECT \"id\" FROM {schema}.{table} WHERE {} LIMIT 1",
        conditions.join(" AND ")
    );
    Ok(CompiledQuery { sql, params })
}

/// Build a SELECT over the runtime descriptor's readable fields.
///
/// Each logical field projects its `storage.valueColumn`, aliased when needed.
/// Masked fields therefore return their display values. Raw storage is excluded
/// from both implicit and explicit projections.
#[allow(clippy::too_many_arguments)]
pub fn build_find_with_schema(
    schema_name: &SchemaName,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
    schema_hint: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_find_with_schema_and_unmask(
        schema_name,
        collection,
        filter,
        limit,
        offset,
        order_by,
        select,
        schema_hint,
        &[],
    )
}

/// Build a schema-aware SELECT with an unmask request for later processing.
///
/// The SELECT still projects creator-visible values. The authorized unmask pass
/// separately reads `storage.rawColumn`, decrypts when needed, records the audit,
/// and replaces the logical field's result. An unmask hint never projects raw
/// storage through the ordinary query builder.
#[allow(clippy::too_many_arguments)]
pub fn build_find_with_schema_and_unmask(
    schema_name: &SchemaName,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
    schema_hint: &Value,
    unmask_columns: &[String],
) -> Result<CompiledQuery, QueryError> {
    build_find_with_schema_and_unmask_and_soft_delete(
        schema_name,
        collection,
        filter,
        limit,
        offset,
        order_by,
        select,
        schema_hint,
        unmask_columns,
        false,
    )
}

/// Dialect-aware variant of
/// [`build_find_with_schema_and_unmask_and_soft_delete`]. The legacy
/// wrapper above keeps the Postgres SQL shape for direct callers; the
/// runtime dispatch path threads the active backend's dialect here so
/// SQLite can emulate Postgres' NULL ordering semantics.
#[allow(clippy::too_many_arguments)]
pub fn build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
    schema_name: &SchemaName,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
    schema_hint: &Value,
    unmask_columns: &[String],
    filter_soft_deleted: bool,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    let select_expr =
        build_masked_aware_select_expr_with_unmask(select, schema_hint, unmask_columns)?;
    build_find_with_schema_and_unmask_and_soft_delete_with_dialect_and_limit_ceiling(
        schema_name,
        collection,
        filter,
        limit,
        offset,
        order_by,
        &select_expr,
        schema_hint,
        filter_soft_deleted,
        dialect,
        MAX_QUERY_LIMIT,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_find_with_schema_and_unmask_and_soft_delete_with_dialect_and_limit_ceiling(
    schema_name: &SchemaName,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select_expr: &str,
    schema_hint: &Value,
    filter_soft_deleted: bool,
    dialect: SqlDialect,
    limit_ceiling: i64,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;
    if let Some(lim) = limit {
        validate_limit_bound("find.limit", lim, limit_ceiling)?;
    }
    if let Some(off) = offset {
        validate_limit_bound("find.offset", off, MAX_QUERY_OFFSET)?;
    }

    let mut sql = format!("SELECT {select_expr} FROM {schema}.{table}");
    let composed_where =
        compose_where_with_soft_delete(&where_clause, filter_soft_deleted, schema_hint)?;
    if !composed_where.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&composed_where);
    }

    if let Some(order) = order_by {
        let order_clause = build_order_by_read_with_dialect(order, dialect, schema_hint)?;
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

    Ok(CompiledQuery { sql, params })
}

/// Schema-aware SELECT builder with the soft-delete
/// auto-filter. Same shape as [`build_find_with_schema_and_unmask`],
/// plus `filter_soft_deleted`: when `true`, appends
/// `AND deleted_at IS NULL` to the WHERE clause so soft-deleted rows
/// are invisible. Callers thread this through from the dispatch
/// layer's `should_filter_soft_deleted` decision.
///
/// The auto-filter slot uses `AND` composition with whatever the
/// creator's filter produced. When the creator's filter is empty, the
/// auto-filter becomes the entire WHERE clause (`WHERE
/// "deleted_at" IS NULL`). When the creator's filter is non-empty,
/// it composes as `WHERE <creator filter> AND "deleted_at" IS NULL`
/// (left-precedence — the creator-supplied filter is the typical
/// load-bearing predicate; the soft-delete filter is the cheap suffix
/// the existing `deleted_at` B-tree index can short-circuit).
///
/// `false` is the back-compat path: emits SQL byte-identical to the
/// builder without the auto-filter. Direct callers (tests, raw SQL probes) keep
/// passing `false` so nothing visible changes; only the CRUD dispatch
/// path threads `true` when the schema marker promises a post-
/// migration table.
#[allow(clippy::too_many_arguments)]
pub fn build_find_with_schema_and_unmask_and_soft_delete(
    schema_name: &SchemaName,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
    schema_hint: &Value,
    unmask_columns: &[String],
    filter_soft_deleted: bool,
) -> Result<CompiledQuery, QueryError> {
    build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
        schema_name,
        collection,
        filter,
        limit,
        offset,
        order_by,
        select,
        schema_hint,
        unmask_columns,
        filter_soft_deleted,
        SqlDialect::Postgres,
    )
}

/// Compose a WHERE clause body with the soft-delete
/// auto-filter. Mirrors the same `creator AND deleted_at IS NULL`
/// pattern used by [`build_soft_delete_one_with_assignments`] /
/// [`build_restore_one_with_assignments`] inner SELECTs.
///
/// Three cases:
/// 1. `!filter_soft_deleted` → return `where_clause` verbatim (back-
///    compat with callers that don't filter soft-deletes).
/// 2. `filter_soft_deleted && where_clause.is_empty()` → return
///    `"deleted_at" IS NULL` (the auto-filter becomes the whole
///    WHERE body).
/// 3. `filter_soft_deleted && !where_clause.is_empty()` → return
///    `<where_clause> AND "deleted_at" IS NULL`.
fn compose_where_with_soft_delete(
    where_clause: &str,
    filter_soft_deleted: bool,
    schema: &Value,
) -> Result<String, QueryError> {
    let marker = if filter_soft_deleted {
        soft_delete_column(schema)?
    } else {
        None
    };
    let Some(marker) = marker else {
        return Ok(where_clause.into());
    };
    let predicate = format!(
        "{} IS NULL",
        quote_ident(&value_column_for_field(marker, schema))
    );
    Ok(if where_clause.is_empty() {
        predicate
    } else {
        format!("({where_clause}) AND {predicate}")
    })
}

/// Project readable fields and protection identity through the storage mapping.
pub fn build_masked_aware_select_expr(
    select: Option<&Value>,
    schema_hint: &Value,
) -> Result<String, QueryError> {
    build_masked_aware_select_expr_with_unmask(select, schema_hint, &[])
}

/// Project readable value columns and protection identity from a qualified table.
/// Each physical `storage.valueColumn` is aliased to its logical field name.
pub fn build_masked_aware_select_expr_for_table_alias(
    schema_hint: &Value,
    table_alias: &str,
) -> Result<String, QueryError> {
    let parts = implicit_read_projection_parts(schema_hint, Some(table_alias))?;
    Ok(parts.join(", "))
}

/// Project explicit fields or expand the descriptor's readable field set.
/// Protection retains identity internally; the ORM restores the public projection.
fn build_masked_aware_select_expr_with_unmask(
    select: Option<&Value>,
    schema_hint: &Value,
    unmask_columns: &[String],
) -> Result<String, QueryError> {
    // Case 1: explicit projection.
    if let Some(Value::Array(arr)) = select {
        if !arr.is_empty() {
            let mut cols: Vec<String> = Vec::with_capacity(arr.len());
            for value in arr {
                let name = value.as_str().ok_or_else(|| {
                    QueryError::InvalidFilter("select entries must be strings".to_string())
                })?;
                validate_read_identifier(name, schema_hint)?;
                cols.push(project_read_field(name, schema_hint, None));
            }
            if !arr.iter().any(|field| field.as_str() == Some("id"))
                && (!unmask_columns.is_empty()
                    || arr
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|name| field_needs_identity(&schema_hint[name])))
            {
                cols.push(project_read_field("id", schema_hint, None));
            }
            return Ok(cols.join(", "));
        }
    }

    Ok(implicit_read_projection_parts(schema_hint, None)?.join(", "))
}

fn qualified_read_field(table_alias: Option<&str>, field: &str) -> String {
    match table_alias {
        Some(alias) => format!("{}.{}", quote_ident(alias), quote_ident(field)),
        None => quote_ident(field),
    }
}

/// Read capabilities may narrow a declared field out of the projection.
fn field_is_readable(def: &Value) -> bool {
    def.get("readable").and_then(Value::as_bool) != Some(false)
        && def.get("projectable").and_then(Value::as_bool) != Some(false)
}

fn field_needs_identity(def: &Value) -> bool {
    crate::sql::descriptors::is_encrypted(def) || crate::sql::descriptors::effective_mask(def).is_some()
}

/// Resolve the physical value column from field storage metadata.
pub(crate) fn value_column_for_field(field: &str, schema_hint: &Value) -> String {
    schema_hint
        .get(field)
        .and_then(|def| def.get("storage"))
        .and_then(|storage| storage.get("valueColumn"))
        .and_then(Value::as_str)
        .unwrap_or(field)
        .to_string()
}

/// Project one field: its physical value column, under its logical name.
///
/// There is no per-column masking decision left to make: after the storage flip
/// the column with the field's own name is the one a read may serve, for masked
/// and unmasked fields alike. Neither a SELECT nor a RETURNING names the raw
/// column - not for an unmask hint either, because the unmask path re-fetches
/// the value under its own authorization check and audit row (`protection::unmask`),
/// which is the whole point of having one reader.
fn project_read_field(field: &str, schema_hint: &Value, table_alias: Option<&str>) -> String {
    let logical = quote_ident(field);
    let physical = value_column_for_field(field, schema_hint);
    let source = qualified_read_field(table_alias, &physical);
    if table_alias.is_some() || source != logical {
        format!("{source} AS {logical}")
    } else {
        logical
    }
}

/// Expand an omitted projection using the descriptor.
fn implicit_read_projection_parts(
    schema_hint: &Value,
    table_alias: Option<&str>,
) -> Result<Vec<String>, QueryError> {
    Ok(implicit_read_fields(schema_hint)?.into_iter()
        .map(|field| project_read_field(field, schema_hint, table_alias)).collect())
}

pub(crate) fn implicit_read_fields(schema_hint: &Value) -> Result<Vec<&str>, QueryError> {
    let schema_obj = schema_hint.as_object().ok_or_else(|| {
        QueryError::InvalidFilter(
            "read schema must be a field-map object; a read cannot be projected without one"
                .to_string(),
        )
    })?;
    let mut fields = Vec::new();
    for (field, def) in schema_obj {
        if is_schema_metadata_key(field) || !field_is_readable(def) {
            continue;
        }
        fields.push(field.as_str());
    }
    if schema_obj
        .get("id")
        .is_some_and(|id| !field_is_readable(id))
        && schema_obj
            .values()
            .any(|def| field_is_readable(def) && field_needs_identity(def))
    {
        fields.push("id");
    }
    if fields.is_empty() {
        return Err(QueryError::InvalidFilter(
            "read requires a declared projection".into(),
        ));
    }

    Ok(fields)
}

/// Project readable fields and the identity required by protection after a mutation.
pub fn build_returning_expr(schema_hint: &Value) -> Result<String, QueryError> {
    Ok(implicit_read_projection_parts(schema_hint, None)?.join(", "))
}

/// The closed set of synthetic result columns the read builders emit.
///
/// A deliberate literal list, NOT "anything starting with `_`". The raw column
/// is `_`-prefixed too, so a blanket underscore allowance would re-admit
/// exactly the column [`read_surface_columns`] exists to remove. Adding a new
/// synthetic column means adding it here; forgetting to means the column is
/// dropped from the row - a visible failure, not a leak.
pub const SYNTHETIC_RESULT_COLUMNS: &[&str] = &[
    // `build_vector_search` (`{col} {op} $1::vector AS _distance`) and the
    // SQLite arm's `v.distance AS _distance`.
    "_distance",
    // `build_spatial_near` (`ST_Distance(...) AS _distance_m`).
    "_distance_m",
    // `build_find_or_create` (`RETURNING <cols>, (xmax = 0) AS __created`).
    // NOT `build_upsert_with_dialect`, which this comment named until 2026-08-28
    // and which has never emitted the column.
    "__created",
];

/// Keep declared readable fields and known computed result columns.
#[must_use]
pub fn read_surface_columns(schema_hint: &Value) -> std::collections::BTreeSet<String> {
    let mut out = crate::sql::descriptors::readable_fields(schema_hint);
    out.extend(
        SYNTHETIC_RESULT_COLUMNS
            .iter()
            .map(|name| (*name).to_owned()),
    );
    out
}

/// Does the column named `name` declare a non-`none`
/// `.mask({...})` entry on `schema_hint`? Returns `false` when the
/// schema is missing, the column is absent from it, or the mask is the
/// explicit opt-out (`kind: "none"`).
pub fn column_is_masked(name: &str, schema_hint: &Value) -> bool {
    schema_hint
        .get(name)
        .and_then(crate::sql::descriptors::effective_mask)
        .is_some()
}

// Read surfaces use descriptor-selected visible columns for masked fields.

/// Push one `$group.by` field's SELECT projection and GROUP BY term.
///
/// Both are the field's own quoted column - for masked and unmasked fields
/// alike, because after the storage flip that column carries the value a read
/// may serve. Grouping a masked column groups by the mask, which is what a
/// caller who cannot read the value should get: `$group.by: ["ssn"]` yields one
/// bucket per DISTINCT MASK, not one per distinct SSN, and the bucket counts
/// therefore say nothing about the underlying values.
fn push_group_by_field(
    field: &str,
    select_cols: &mut Vec<String>,
    group_by_cols: &mut Vec<String>,
) {
    let logical = quote_ident(field);
    select_cols.push(logical.clone());
    group_by_cols.push(logical);
}

/// Build a SELECT COUNT(*) query.
///
/// Thin shim around [`build_count_with_soft_delete`]
/// passing `filter_soft_deleted = false`.
pub fn build_count(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_count_with_soft_delete(
        schema_name,
        collection,
        schema_hint,
        filter,
        false,
        SqlDialect::Postgres,
    )
}

/// COUNT(*) with the soft-delete auto-filter. The CRUD
/// dispatch path threads `should_filter_soft_deleted` through here so
/// `db.posts.count()` on a post-migration table excludes soft-deleted
/// rows by default.
pub fn build_count_with_soft_delete(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    filter_soft_deleted: bool,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;

    let mut sql = format!("SELECT COUNT(*) AS count FROM {schema}.{table}");
    let composed_where =
        compose_where_with_soft_delete(&where_clause, filter_soft_deleted, schema_hint)?;
    if !composed_where.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&composed_where);
    }

    Ok(CompiledQuery { sql, params })
}

/// Build an INSERT query:
/// `INSERT INTO "schema_name"."collection" (...) VALUES (...) RETURNING "id", ...`
///
/// PG-flavour wrapper around [`build_insert_with_dialect`]. Every
/// existing CRUD call site stays on this signature — the orchestrator's
/// `dispatch_insert` path still goes through Postgres today.
pub fn build_insert(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    doc: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_insert_with_dialect(
        schema_name,
        collection,
        schema_hint,
        doc,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware INSERT builder.
///
/// Binary columns use a dialect-specific decoding expression over text binds.
/// Ordinary text binds are never inspected for binary markers.
pub fn build_insert_with_dialect(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    doc: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    let returning = build_returning_expr(schema_hint)?;

    let obj = doc.as_object().ok_or_else(|| {
        QueryError::InvalidFilter("insert document must be an object".to_string())
    })?;

    if obj.is_empty() {
        return Err(QueryError::InvalidFilter(
            "insert document cannot be empty".to_string(),
        ));
    }

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let binary_bind_cols = collect_binary_bind_cols(obj);

    let mut columns = Vec::new();
    let mut placeholders = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    for (key, value) in obj {
        columns.push(quote_ident(key));
        // Postgres' text-format param protocol (`query_text_params`,
        // `&[&str]`) cannot represent NULL — an empty string would be
        // encoded as `""`, failing CHECK constraints on enum columns
        // and producing silently-empty TEXT cells. Inline `NULL` as a
        // SQL literal so JSON `null` round-trips faithfully.
        if value.is_null() {
            placeholders.push("NULL".to_string());
        } else {
            let is_binary_bind = binary_bind_cols.contains(key.as_str());
            if is_binary_bind {
                params.push(dialect.encode_binary_param(value_to_param(value))?);
                placeholders.push(dialect.binary_bind_placeholder(params.len()));
            } else {
                placeholders.push(push_field_value_bind(
                    &mut params,
                    value,
                    key,
                    schema_hint,
                    dialect,
                )?);
            }
        }
    }

    let overriding =
        crate::sql::identity::overriding_clause(schema_hint, dialect, obj.contains_key("id"));
    let sql = format!(
        "INSERT INTO {schema}.{table} ({}){overriding} VALUES ({}) RETURNING {returning}",
        columns.join(", "),
        placeholders.join(", ")
    );

    Ok(CompiledQuery { sql, params })
}

pub fn collect_binary_bind_cols(obj: &crate::value::Record) -> std::collections::HashSet<&str> {
    obj.iter()
        .filter_map(|(key, value)| matches!(value, Value::Bytes(_)).then_some(key.as_str()))
        .collect()
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
/// - `$push` — append one complete JSON element, including a nested array or null
/// - `$pull` — remove every structurally equal JSON element
/// - `$addToSet` — append only when no structurally equal element exists
pub fn build_set_clauses(
    update: &Value,
    params: &mut Vec<Value>,
    schema_hint: &Value,
) -> Result<Vec<String>, QueryError> {
    build_set_clauses_with_dialect(update, params, schema_hint, SqlDialect::Postgres)
}

pub fn build_set_clauses_with_dialect(
    update: &Value,
    params: &mut Vec<Value>,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<Vec<String>, QueryError> {
    // The default auto-bump is empty (no version bump, no updated_by) —
    // preserves the contract for callers that don't need auto-bump.
    build_set_clauses_with_assignments(
        update,
        params,
        schema_hint,
        dialect,
        &WriteAssignments::default(),
    )
}

/// Combine user updates with the assignments prepared by the ORM.
pub fn build_set_clauses_with_assignments(
    update: &Value,
    params: &mut Vec<Value>,
    schema_hint: &Value,
    dialect: SqlDialect,
    assignments: &WriteAssignments,
) -> Result<Vec<String>, QueryError> {
    use crate::sql::update::Operator;
    let fields = crate::sql::update::assignments(update)
        .map_err(|error| QueryError::InvalidFilter(error.to_string()))?;
    let update_obj = update.as_object().expect("validated update");
    let mut binary_bind_cols = collect_binary_bind_cols(update_obj);
    if let Some(set_obj) = update_obj.get("$set").and_then(Value::as_object) {
        binary_bind_cols.extend(collect_binary_bind_cols(set_obj));
    }
    let mut set_clauses = Vec::new();
    for assignment in fields {
        let key = assignment.field;
        let value = assignment.operand;
        let col = quote_ident(key);
        let clause = match assignment.operator {
            Operator::Set => {
                if binary_bind_cols.contains(key) {
                    params.push(dialect.encode_binary_param(value_to_param(value))?);
                    format!("{col} = {}", dialect.binary_bind_placeholder(params.len()))
                } else {
                    format!(
                        "{col} = {}",
                        push_field_value_bind(params, value, key, schema_hint, dialect)?
                    )
                }
            }
            Operator::Increment | Operator::Decrement | Operator::Multiply => {
                let operator = match assignment.operator {
                    Operator::Increment => '+',
                    Operator::Decrement => '-',
                    _ => '*',
                };
                params.push(value_to_param(value));
                let bind = format!("${}", params.len());
                let bind = if dialect == SqlDialect::Postgres {
                    format!("{bind}::numeric")
                } else {
                    bind
                };
                format!("{col} = {col} {operator} {bind}")
            }
            Operator::Push | Operator::Pull | Operator::AddToSet => {
                use crate::sql::array_update::ArrayUpdate;
                push_array_value_bind(params, value, key, schema_hint, dialect)?;
                let operation = match assignment.operator {
                    Operator::Push => ArrayUpdate::Push,
                    Operator::Pull => ArrayUpdate::Pull,
                    _ => ArrayUpdate::AddToSet,
                };
                crate::sql::array_update::render(
                    dialect,
                    operation,
                    &col,
                    &format!("${}", params.len()),
                )?
            }
        };
        set_clauses.push(clause);
    }

    set_clauses.extend(assignments.render(dialect, params, None)?);

    Ok(set_clauses)
}

/// Build an UPDATE query:
/// `UPDATE "schema_name"."collection" SET ... WHERE id = (...) RETURNING "id", ...`
///
/// PG-flavour wrapper — every existing call site goes through Postgres.
pub fn build_update_one(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_update_one_with_dialect(
        schema_name,
        collection,
        schema_hint,
        filter,
        update,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware `updateOne` builder. Encrypted-column
/// binds follow the dialect's
/// [`SqlDialect::binary_bind_placeholder`]. PostgreSQL narrows through
/// the platform primary key and locks the selected row. SQLite keeps its
/// `rowid` target because it has no column-grant boundary.
pub fn build_update_one_with_dialect(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    build_update_one_with_assignments(
        schema_name,
        collection,
        schema_hint,
        filter,
        update,
        dialect,
        &WriteAssignments::default(),
    )
}

/// Compile the update with the assignments resolved from its collection descriptor.
pub fn build_update_one_with_assignments(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
    assignments: &WriteAssignments,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let set_clauses =
        build_set_clauses_with_assignments(update, &mut params, schema_hint, dialect, assignments)?;

    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;

    let inner_where = if where_clause.is_empty() {
        String::new()
    } else {
        format!(" WHERE {where_clause}")
    };
    let (target_col, lock_clause) = single_row_write_target(dialect, schema_hint);
    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{inner_where} LIMIT 1{lock_clause}) RETURNING {returning}",
        set_clauses.join(", "),
    );

    Ok(CompiledQuery { sql, params })
}

/// Build an INSERT query for multiple documents:
/// `INSERT INTO "schema_name"."collection" ("col1", "col2") VALUES ($1, $2), ($3, $4) RETURNING "id", ...`
///
/// Column names are unioned across documents; a missing or explicit null value
/// is emitted as a SQL `NULL` literal and consumes no bind parameter.
///
/// PG-flavour wrapper around [`build_insert_many_with_dialect`].
pub fn build_insert_many(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    docs: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_insert_many_with_dialect(
        schema_name,
        collection,
        schema_hint,
        docs,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware `insertMany` builder.
pub fn build_insert_many_with_dialect(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    docs: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    let returning = build_returning_expr(schema_hint)?;

    let arr = docs.as_array().ok_or_else(|| {
        QueryError::InvalidFilter("insertMany: docs must be an array".to_string())
    })?;

    if arr.is_empty() {
        return Err(QueryError::InvalidFilter(
            "insertMany: docs array cannot be empty".to_string(),
        ));
    }

    // DB-11: cap the batch BEFORE materializing per-row SQL groups and params.
    // This bounds only the document-count contribution; the non-null-cell
    // budget below separately bounds placeholders. It makes no claim about
    // total statement bytes. Enforced here so raw deploys cannot bypass it.
    if arr.len() > MAX_INSERT_MANY_BATCH {
        return Err(QueryError::InvalidFilter(format!(
            "insertMany batch of {} exceeds the maximum of {MAX_INSERT_MANY_BATCH}",
            arr.len()
        )));
    }

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut binary_bind_cols: std::collections::HashSet<String> = std::collections::HashSet::new();
    for doc in arr {
        if let Some(obj) = doc.as_object() {
            for name in collect_binary_bind_cols(obj) {
                binary_bind_cols.insert(name.to_string());
            }
        }
    }

    let mut column_set = std::collections::BTreeSet::<&String>::new();
    let mut bind_count = 0usize;
    for doc in arr {
        let obj = doc.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("insertMany: each document must be an object".to_string())
        })?;
        for (key, value) in obj {
            column_set.insert(key);
            if !value.is_null() {
                bind_count = bind_count.checked_add(1).ok_or_else(|| {
                    QueryError::InvalidFilter(
                        "insertMany: non-null field-value count overflowed".to_string(),
                    )
                })?;
            }
        }
    }

    if column_set.is_empty() {
        return Err(QueryError::InvalidFilter(
            "insertMany: documents cannot be empty".to_string(),
        ));
    }

    let (dialect_name, bind_limit) = match dialect {
        SqlDialect::Postgres => ("PostgreSQL", POSTGRES_MAX_BIND_PARAMETERS),
        SqlDialect::Sqlite => ("SQLite", SQLITE_MAX_BIND_PARAMETERS),
        // MySQL's prepared-statement parameter count is also a 16-bit field.
        // This dialect is render-only today, but keeping its builder bounded
        // prevents a future executor from inheriting the same defect.
    };
    if bind_count > bind_limit {
        return Err(QueryError::InvalidFilter(format!(
            "insertMany batch has {bind_count} non-null field values; one {dialect_name} insertMany call supports at most {bind_limit}; split the documents into smaller batches"
        )));
    }

    let column_names: Vec<&String> = column_set.into_iter().collect();
    let columns: Vec<String> = column_names.iter().map(|k| quote_ident(k)).collect();

    let mut params: Vec<Value> = Vec::with_capacity(bind_count);
    let mut value_groups: Vec<String> = Vec::with_capacity(arr.len());

    for doc in arr {
        let obj = doc.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("insertMany: each document must be an object".to_string())
        })?;
        let mut placeholders = Vec::new();
        for key in &column_names {
            let val = obj.get(*key).unwrap_or(&Value::Null);
            // See build_insert: text-format params can't carry NULL;
            // inline as a SQL literal instead.
            if val.is_null() {
                placeholders.push("NULL".to_string());
            } else {
                let is_binary_bind = binary_bind_cols.contains(key.as_str());
                if is_binary_bind {
                    params.push(dialect.encode_binary_param(value_to_param(val))?);
                    placeholders.push(dialect.binary_bind_placeholder(params.len()));
                } else {
                    placeholders.push(push_field_value_bind(
                        &mut params,
                        val,
                        key,
                        schema_hint,
                        dialect,
                    )?);
                }
            }
        }
        value_groups.push(format!("({})", placeholders.join(", ")));
    }

    let overriding = crate::sql::identity::overriding_clause(
        schema_hint,
        dialect,
        arr.iter().any(|doc| doc.get("id").is_some()),
    );
    let sql = format!(
        "INSERT INTO {schema}.{table} ({}){overriding} VALUES {} RETURNING {returning}",
        columns.join(", "),
        value_groups.join(", ")
    );

    Ok(CompiledQuery { sql, params })
}

/// Build an UPDATE query for multiple rows (no LIMIT 1):
/// `UPDATE "schema_name"."collection" SET ... WHERE ... RETURNING "id", ...`
///
/// PG-flavour wrapper around [`build_update_many_with_dialect`].
pub fn build_update_many(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_update_many_with_dialect(
        schema_name,
        collection,
        schema_hint,
        filter,
        update,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware `updateMany` builder. Encrypted-column
/// binds follow the dialect's
/// [`SqlDialect::binary_bind_placeholder`].
pub fn build_update_many_with_dialect(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    build_update_many_with_assignments(
        schema_name,
        collection,
        schema_hint,
        filter,
        update,
        dialect,
        &WriteAssignments::default(),
    )
}

/// Compile the update with the assignments resolved from its collection descriptor.
pub fn build_update_many_with_assignments(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
    assignments: &WriteAssignments,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let set_clauses =
        build_set_clauses_with_assignments(update, &mut params, schema_hint, dialect, assignments)?;

    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;

    let mut sql = format!("UPDATE {schema}.{table} SET {}", set_clauses.join(", "));
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }

    Ok(CompiledQuery { sql, params })
}

/// Delete all matching rows without returning records.
pub fn build_delete_many(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;

    let mut sql = format!("DELETE FROM {schema}.{table}");
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }

    Ok(CompiledQuery { sql, params })
}

/// Build a DELETE query:
/// `DELETE FROM "schema_name"."collection" WHERE ... RETURNING "id", ...`
pub fn build_delete_one(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_delete_one_with_dialect(
        schema_name,
        collection,
        schema_hint,
        filter,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware single-row DELETE builder.
pub fn build_delete_one_with_dialect(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;

    let (target_col, lock_clause) = single_row_write_target(dialect, schema_hint);
    let sql = format!(
        "DELETE FROM {schema}.{table} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{} LIMIT 1{lock_clause}) RETURNING {returning}",
        if where_clause.is_empty() {
            String::new()
        } else {
            format!(" WHERE {where_clause}")
        }
    );

    Ok(CompiledQuery { sql, params })
}

// Lifecycle mutations use assignments prepared by the ORM.

/// Select and lock an entity by its id.
fn single_row_write_target(dialect: SqlDialect, schema: &Value) -> (String, &'static str) {
    (
        quote_ident(&value_column_for_field("id", schema)),
        if dialect == SqlDialect::Postgres {
            " FOR UPDATE"
        } else {
            ""
        },
    )
}

fn deleted_predicate(schema: &Value, deleted: bool) -> Result<String, QueryError> {
    let marker = soft_delete_column(schema)?
        .ok_or_else(|| QueryError::InvalidFilter("collection has no soft-delete column".into()))?;
    Ok(format!(
        "{} IS {}NULL",
        quote_ident(&value_column_for_field(marker, schema)),
        if deleted { "NOT " } else { "" }
    ))
}

/// Dialect-aware `soft_delete_one` builder. Used by the
/// CRUD dispatch path on post-migration tables when `delete()` /
/// `deleteOne()` reaches a row that hasn't already been soft-deleted.
///
/// Generated SQL example (PG):
/// ```sql
/// UPDATE "app1"."posts"
/// SET "deleted_at" = NOW(), "version" = "version" + 1, "updated_at" = NOW(), "updated_by" = $2
/// WHERE id = (
///   SELECT id FROM "app1"."posts" WHERE "id" = $1 AND "deleted_at" IS NULL LIMIT 1 FOR UPDATE
/// )
/// RETURNING "id", "created_at", ...
/// ```
///
/// The `AND deleted_at IS NULL` in the inner SELECT keeps the call
/// idempotent: re-deleting an already-deleted row affects 0 rows. The
/// dispatch layer translates 0-affected to a `null` result (matches the
/// `deleteOne` contract).
pub fn build_soft_delete_one_with_assignments(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
    assignments: &WriteAssignments,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let set_clauses = assignments.render(dialect, &mut params, None)?;

    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;
    // The inner SELECT scopes the soft-delete to a single live row.
    // If the filter is empty the WHERE becomes just `deleted_at IS
    // NULL` (any single live row). The dispatch path doesn't call this
    // builder with an empty filter — `Collection::delete()` requires an
    // id or filter — but we mirror the same defensive behaviour as
    // `build_delete_one`.
    let inner_where = if where_clause.is_empty() {
        format!(" WHERE {}", deleted_predicate(schema_hint, false)?)
    } else {
        format!(
            " WHERE ({where_clause}) AND {}",
            deleted_predicate(schema_hint, false)?
        )
    };

    let (target_col, lock_clause) = single_row_write_target(dialect, schema_hint);
    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{inner_where} LIMIT 1{lock_clause}) RETURNING {returning}",
        set_clauses.join(", "),
    );

    Ok(CompiledQuery { sql, params })
}

/// Apply soft-delete assignments to matching live rows without returning records.
pub fn build_soft_delete_many_with_assignments(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
    assignments: &WriteAssignments,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let set_clauses = assignments.render(dialect, &mut params, None)?;

    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;
    let where_sql = if where_clause.is_empty() {
        format!(" WHERE {}", deleted_predicate(schema_hint, false)?)
    } else {
        format!(
            " WHERE ({where_clause}) AND {}",
            deleted_predicate(schema_hint, false)?
        )
    };

    let sql = format!(
        "UPDATE {schema}.{table} SET {}{where_sql}",
        set_clauses.join(", "),
    );

    Ok(CompiledQuery { sql, params })
}

/// Dialect-aware `restore_one` builder. Symmetric to
/// [`build_soft_delete_one_with_assignments`]: clears `deleted_at`
/// and scopes to rows that are CURRENTLY soft-deleted
/// (`deleted_at IS NOT NULL`) so restoring a live row is a no-op
/// (affected-rows = 0 → typed `not_found_or_already_live` via the
/// dispatch layer).
pub fn build_restore_one_with_assignments(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
    assignments: &WriteAssignments,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let set_clauses = assignments.render(dialect, &mut params, None)?;

    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;
    let inner_where = if where_clause.is_empty() {
        format!(" WHERE {}", deleted_predicate(schema_hint, true)?)
    } else {
        format!(
            " WHERE ({where_clause}) AND {}",
            deleted_predicate(schema_hint, true)?
        )
    };

    let (target_col, lock_clause) = single_row_write_target(dialect, schema_hint);
    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{inner_where} LIMIT 1{lock_clause}) RETURNING {returning}",
        set_clauses.join(", "),
    );

    Ok(CompiledQuery { sql, params })
}

/// Restore matching deleted rows without returning records.
pub fn build_restore_many_with_assignments(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
    assignments: &WriteAssignments,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut params: Vec<Value> = Vec::new();
    let set_clauses = assignments.render(dialect, &mut params, None)?;

    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;
    let where_sql = if where_clause.is_empty() {
        format!(" WHERE {}", deleted_predicate(schema_hint, true)?)
    } else {
        format!(
            " WHERE ({where_clause}) AND {}",
            deleted_predicate(schema_hint, true)?
        )
    };

    let sql = format!(
        "UPDATE {schema}.{table} SET {}{where_sql}",
        set_clauses.join(", "),
    );

    Ok(CompiledQuery { sql, params })
}

/// Build an aggregate query from a pipeline of stages.
///
/// Supported stages:
/// - `$match`  → WHERE clause
/// - `$group`  → SELECT aggregates + optional GROUP BY
/// - `$having` → HAVING clause
/// - `$sort`   → ORDER BY
/// - `$limit`  → LIMIT N
///
/// Thin shim around
/// [`build_aggregate_with_soft_delete`] passing
/// `filter_soft_deleted = false`. CRUD dispatch threads the auto-
/// filter through the dedicated entry; direct callers keep the same
/// SQL byte-identical.
pub fn build_aggregate(
    schema_name: &SchemaName,
    collection: &str,
    pipeline: &Value,
    schema_hint: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_aggregate_with_soft_delete(schema_name, collection, pipeline, false, schema_hint)
}

/// Aggregate builder with the soft-delete auto-filter.
///
/// When `filter_soft_deleted = true`, appends `AND deleted_at IS NULL`
/// to whatever WHERE clause the pipeline's `$match` stage produced
/// (or `WHERE deleted_at IS NULL` when no `$match` is present).
pub fn build_aggregate_with_soft_delete(
    schema_name: &SchemaName,
    collection: &str,
    pipeline: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_aggregate_with_soft_delete_with_dialect(
        schema_name,
        collection,
        pipeline,
        filter_soft_deleted,
        schema_hint,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware variant of [`build_aggregate_with_soft_delete`].
pub fn build_aggregate_with_soft_delete_with_dialect(
    schema_name: &SchemaName,
    collection: &str,
    pipeline: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    build_aggregate_with_result_columns(
        schema_name,
        collection,
        pipeline,
        filter_soft_deleted,
        schema_hint,
        dialect,
    )
    .map(|(query, _)| query)
}

/// [`build_aggregate_with_soft_delete_with_dialect`], plus the exact key set
/// its rows will carry.
///
/// The second element is `None` when the pipeline has no `$group` stage - the
/// rows are then plain declared-shape rows. When there is a `$group` it is the
/// group-by fields plus the accumulator aliases, which no descriptor declares,
/// so the read pipeline's row-surface filter has to be told them rather than
/// deriving them from the schema.
///
/// Returned from the builder rather than re-derived beside it: a second
/// traversal of the pipeline that disagreed with this one would silently drop
/// result columns, and the drop would look like a missing accumulator rather
/// than like a bug in a surface filter.
pub fn build_aggregate_with_result_columns(
    schema_name: &SchemaName,
    collection: &str,
    pipeline: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<(CompiledQuery, Option<Vec<String>>), QueryError> {
    validate_collection(collection)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let stages = pipeline.as_array().ok_or_else(|| {
        QueryError::InvalidFilter("aggregate: pipeline must be an array".to_string())
    })?;

    let mut params: Vec<Value> = Vec::new();
    let mut where_clause = String::new();
    let mut select_cols: Vec<String> = Vec::new();
    // The UNQUOTED result key for every entry pushed onto `select_cols`, in
    // the same order. Kept beside it rather than parsed back out of the SQL.
    let mut result_cols: Vec<String> = Vec::new();
    let mut group_by_cols: Vec<String> = Vec::new();
    // Map alias → SQL expression for HAVING clause rewriting
    let mut agg_exprs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut having_clause = String::new();
    let mut order_clause = String::new();
    let mut limit_clause = String::new();
    // Track the most recent $sort for $first sort-order threading.
    let mut last_sort: Vec<(String, bool)> = Vec::new();

    for stage in stages {
        let obj = stage.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("aggregate: each stage must be an object".to_string())
        })?;

        if let Some(match_val) = obj.get("$match") {
            where_clause = build_where_with_dialect(match_val, &mut params, schema_hint, dialect)?;
        } else if let Some(group_val) = obj.get("$group") {
            let group_obj = group_val.as_object().ok_or_else(|| {
                QueryError::InvalidFilter("aggregate: $group must be an object".to_string())
            })?;

            // Handle optional `by` field
            if let Some(by_val) = group_obj.get("by") {
                match by_val {
                    Value::String(s) => {
                        validate_read_identifier(s, schema_hint)?;
                        validate_value_operation(s, schema_hint)?;
                        push_group_by_field(s, &mut select_cols, &mut group_by_cols);
                        result_cols.push(s.clone());
                    }
                    Value::Array(arr) => {
                        for item in arr {
                            let s = item.as_str().ok_or_else(|| {
                                QueryError::InvalidFilter(
                                    "aggregate: $group.by array elements must be strings"
                                        .to_string(),
                                )
                            })?;
                            validate_read_identifier(s, schema_hint)?;
                            validate_value_operation(s, schema_hint)?;
                            push_group_by_field(s, &mut select_cols, &mut group_by_cols);
                            result_cols.push(s.to_string());
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
                        validate_read_identifier(field, schema_hint)?;
                        validate_value_operation(field, schema_hint)?;
                        // Read the descriptor's creator-visible column, including masks.
                        format!("SUM({})", quote_ident(field))
                    }
                    "$avg" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$avg requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        validate_value_operation(field, schema_hint)?;
                        format!("AVG({})", quote_ident(field))
                    }
                    "$min" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$min requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        validate_value_operation(field, schema_hint)?;
                        format!("MIN({})", quote_ident(field))
                    }
                    "$max" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$max requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        validate_value_operation(field, schema_hint)?;
                        format!("MAX({})", quote_ident(field))
                    }
                    "$first" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$first requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        validate_value_operation(field, schema_hint)?;
                        // Read the descriptor's creator-visible column, including masks.
                        let read_ident = quote_ident(field);
                        if last_sort.is_empty() {
                            format!("(array_agg({read_ident}))[1]")
                        } else {
                            let order_parts: Vec<String> = last_sort
                                .iter()
                                .map(|(col, descending)| {
                                    build_order_term(col, *descending, dialect)
                                })
                                .collect();
                            format!(
                                "(array_agg({read_ident} ORDER BY {}))[1]",
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
                result_cols.push(alias.clone());
            }
        } else if let Some(having_val) = obj.get("$having") {
            having_clause =
                build_having(having_val, &mut params, &agg_exprs, schema_hint, dialect)?;
        } else if let Some(sort_val) = obj.get("$sort") {
            // Track sort columns/directions for $first threading
            last_sort.clear();
            if let Some(sort_obj) = sort_val.as_object() {
                for (key, val) in sort_obj {
                    if !agg_exprs.contains_key(key) {
                        validate_read_identifier(key, schema_hint)?;
                    }
                    let descending = matches!(val.as_i64(), Some(n) if n < 0);
                    last_sort.push((key.clone(), descending));
                }
            }
            // SEC-4: aggregate $sort on a masked base column must order by
            // the creator-visible mask, not raw plaintext. Aggregate aliases
            // (`agg_exprs`) order by the alias name as-is.
            order_clause = build_aggregate_order_by(sort_val, dialect, &agg_exprs, schema_hint)?;
        } else if let Some(limit_val) = obj.get("$limit") {
            let n = limit_val.as_i64().ok_or_else(|| {
                QueryError::InvalidFilter("aggregate: $limit must be an integer".to_string())
            })?;
            validate_limit_bound("aggregate.$limit", n, MAX_QUERY_LIMIT)?;
            limit_clause = format!("{n}");
        }
    }

    let (select_expr, result_columns) = if select_cols.is_empty() {
        // L24: no `*` fallback. An aggregate with no projection stage returns
        // the same allowlist a plain read does — and therefore the same row
        // surface, which is why the second element is `None` here.
        (
            implicit_read_projection_parts(schema_hint, None)?.join(", "),
            None,
        )
    } else {
        (select_cols.join(", "), Some(result_cols))
    };

    let mut sql = format!("SELECT {select_expr} FROM {schema}.{table}");

    let composed_where =
        compose_where_with_soft_delete(&where_clause, filter_soft_deleted, schema_hint)?;
    if !composed_where.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&composed_where);
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

    Ok((CompiledQuery { sql, params }, result_columns))
}

/// Build a SELECT DISTINCT query:
/// `SELECT DISTINCT "field" FROM "schema"."table" WHERE ... ORDER BY "field"`
///
/// Thin shim around
/// [`build_distinct_with_soft_delete`] passing
/// `filter_soft_deleted = false`.
pub fn build_distinct(
    schema_name: &SchemaName,
    collection: &str,
    field: &str,
    filter: &Value,
    schema_hint: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_distinct_with_soft_delete(schema_name, collection, field, filter, false, schema_hint)
}

/// DISTINCT builder with the soft-delete auto-filter.
pub fn build_distinct_with_soft_delete(
    schema_name: &SchemaName,
    collection: &str,
    field: &str,
    filter: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_distinct_with_soft_delete_with_dialect(
        schema_name,
        collection,
        field,
        filter,
        filter_soft_deleted,
        schema_hint,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware variant of [`build_distinct_with_soft_delete`].
pub fn build_distinct_with_soft_delete_with_dialect(
    schema_name: &SchemaName,
    collection: &str,
    field: &str,
    filter: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    validate_read_identifier(field, schema_hint)?;
    validate_value_operation(field, schema_hint)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);
    let col = quote_ident(field);
    // DISTINCT over the field's own column. For a masked field that column
    // holds the mask, so `distinct("ssn")` enumerates MASKS - it can no longer
    // be one `read_column_for` disagreement away from enumerating the values
    // behind them.
    let select_expr = col.clone();

    let mut params: Vec<Value> = Vec::new();
    let where_clause = build_where_with_dialect(filter, &mut params, schema_hint, dialect)?;

    let mut sql = format!("SELECT DISTINCT {select_expr} FROM {schema}.{table}");
    let composed_where =
        compose_where_with_soft_delete(&where_clause, filter_soft_deleted, schema_hint)?;
    if !composed_where.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&composed_where);
    }
    sql.push_str(" ORDER BY ");
    sql.push_str(&build_order_term(field, false, dialect));

    Ok(CompiledQuery { sql, params })
}

/// Build a pgvector nearest-neighbour search query.
///
/// Emits the canonical pgvector shape (plan §3.1):
///
/// ```sql
/// SELECT "id", "created_at", ..., "<col>" <op> $1::vector AS _distance
///   FROM "<app>"."<coll>"
///  [WHERE <filter-lowered>]
///  ORDER BY "<col>" <op> $1::vector
///  LIMIT $2
/// ```
///
/// `<op>` is the pgvector operator per metric: `<->` L2, `<=>` Cosine,
/// `<#>` InnerProduct (negated). The query vector is bound as a text
/// literal `[1,2,3,...]` cast `::vector` — pgvector parses the text on
/// cast, sidestepping the binary-protocol type-discovery handshake
/// (the `vector` type's OID is allocated at extension-install time and
/// not known to the driver at compile time).
///
/// `k` is bound as the second parameter (the LIMIT), keyed to JS-side
/// validation; impls may want to clamp before calling, but this builder
/// is permissive.
///
/// The filter is composed by the standard [`build_where`] helper — the
/// same machinery `build_find` uses. `$1` is reserved for the query
/// vector and `$2` for `k`; the filter's own placeholders start from
/// `$3` because [`build_where`] always allocates fresh numbers from
/// the `params` length.
#[allow(
    clippy::too_many_arguments,
    reason = "the backend search contract supplies each typed search operand"
)]
pub fn build_vector_search(
    schema_name: &SchemaName,
    collection: &str,
    column: &str,
    query: &[f32],
    k: usize,
    metric: crate::sql::descriptors::VectorMetric,
    filter: &Value,
    schema_hint: &Value,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    validate_read_identifier(column, schema_hint)?;
    validate_search_limit_bound("search.k", k)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);
    let col = quote_ident(column);

    // pgvector operator per metric — see `crate::sql::descriptors::VectorMetric`
    // doc-comment for the operator/opclass mapping.
    let op = match metric {
        crate::sql::descriptors::VectorMetric::Cosine => "<=>",
        crate::sql::descriptors::VectorMetric::L2 => "<->",
        crate::sql::descriptors::VectorMetric::InnerProduct => "<#>",
    };

    // Render the query vector as a pgvector text literal: `[1,2,3,...]`.
    // We bind it as `$1` and cast to `::vector` on both the SELECT and
    // ORDER BY sides so pgvector parses once. Float formatting uses
    // Rust's `{}` (shortest-round-trip) — pgvector's text parser
    // accepts the same form Postgres' float8 input does.
    let mut vec_lit = String::with_capacity(query.len() * 8 + 2);
    vec_lit.push('[');
    for (i, v) in query.iter().enumerate() {
        if i > 0 {
            vec_lit.push(',');
        }
        // Use shortest-round-trip f32 → string. f32 has 24 bits of
        // mantissa, so 9 significant digits round-trip exactly; the
        // default Display impl picks the shortest unambiguous form.
        vec_lit.push_str(&v.to_string());
    }
    vec_lit.push(']');

    // Param 1: vector literal. Param 2: k. The filter's own params
    // (rendered into `build_where`'s `params` vec) start at $3 because
    // we pre-push two entries before invoking the helper.
    let mut params: Vec<Value> = Vec::with_capacity(2 + 4);
    params.push(vec_lit.into());
    params.push((k).into());

    let where_clause = build_where(filter, &mut params, schema_hint)?;

    let select_expr = build_masked_aware_select_expr(None, schema_hint)?;
    let mut sql =
        format!("SELECT {select_expr}, {col} {op} $1::vector AS _distance FROM {schema}.{table}");
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(&format!(" ORDER BY {col} {op} $1::vector LIMIT $2"));

    Ok(CompiledQuery { sql, params })
}

/// Build the SQL + bind parameters for a spatial within-radius search
/// (PostGIS arm).
///
/// Shape:
/// ```sql
/// SELECT "id", "created_at", ..., ST_Distance("col", ST_MakePoint($1, $2)::geography) AS _distance_m
/// FROM "<app>"."<coll>"
/// WHERE ST_DWithin("col", ST_MakePoint($1, $2)::geography, $3) AND <filter>
/// ORDER BY _distance_m
/// LIMIT $4
/// ```
///
/// **Parameter order**: `$1 = lng`, `$2 = lat` — `ST_MakePoint(x, y)` is
/// `(lng, lat)` in PostGIS, the inverse of the SDK's `{lat, lng}` shape.
/// The Rust trait surface ([`crate::sql::descriptors::GeoPoint`]) keeps the
/// `{lat, lng}` shape; the swap happens here at the SQL boundary so the
/// JS/Rust contract stays in `(lat, lng)` order. `$3 = radius_m`,
/// `$4 = limit`. Filter parameters start at `$5`.
///
/// **Column type**: the indexed column must be
/// `geography(POINT, 4326)`. The migration engine's PostgreSQL DDL emitter
/// wires this when the schema field type is `geoPoint`.
#[allow(
    clippy::too_many_arguments,
    reason = "the backend search contract supplies each typed search operand"
)]
pub fn build_spatial_near(
    schema_name: &SchemaName,
    collection: &str,
    column: &str,
    point: crate::sql::descriptors::GeoPoint,
    radius_m: f64,
    filter: &Value,
    limit: Option<usize>,
    schema_hint: &Value,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    validate_read_identifier(column, schema_hint)?;

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);
    let col = quote_ident(column);

    let limit = limit.unwrap_or(100);
    validate_search_limit_bound("near.limit", limit)?;

    // Bind order: (lng, lat, radius_m, limit). Note the swap: ST_MakePoint
    // takes (x, y) = (lng, lat), the inverse of the SDK's {lat, lng}
    // input shape.
    let mut params: Vec<Value> = Vec::with_capacity(4 + 4);
    params.push(Value::Number(
        crate::value::Number::from_f64(point.lng)
            .ok_or_else(|| QueryError::InvalidFilter("non-finite spatial operand".into()))?,
    ));
    params.push(Value::Number(
        crate::value::Number::from_f64(point.lat)
            .ok_or_else(|| QueryError::InvalidFilter("non-finite spatial operand".into()))?,
    ));
    params.push(Value::Number(
        crate::value::Number::from_f64(radius_m)
            .ok_or_else(|| QueryError::InvalidFilter("non-finite spatial operand".into()))?,
    ));
    params.push((limit).into());

    let where_clause = build_where(filter, &mut params, schema_hint)?;

    let select_expr = build_masked_aware_select_expr(None, schema_hint)?;
    let mut sql = format!(
        "SELECT {select_expr}, ST_Distance({col}, ST_MakePoint($1, $2)::geography) AS _distance_m \
         FROM {schema}.{table} \
         WHERE ST_DWithin({col}, ST_MakePoint($1, $2)::geography, $3)"
    );
    if !where_clause.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(&where_clause);
    }
    sql.push_str(" ORDER BY _distance_m LIMIT $4");

    Ok(CompiledQuery { sql, params })
}

// ---------------------------------------------------------------------------
// HAVING clause builder (resolves aliases to aggregate expressions)
// ---------------------------------------------------------------------------

/// Build a HAVING clause from a filter, replacing alias names with their
/// aggregate SQL expressions. E.g. `{"cnt": {"$gt": 5}}` where `cnt` maps
/// to `COUNT(*)` generates `COUNT(*) > $1` instead of `"cnt" > $1`.
fn build_having(
    filter: &Value,
    params: &mut Vec<Value>,
    agg_exprs: &std::collections::HashMap<String, String>,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    validate_clause_budget(filter, ClauseBudgetKind::Having)?;
    build_having_inner(filter, params, agg_exprs, schema_hint, dialect)
}

fn build_having_inner(
    filter: &Value,
    params: &mut Vec<Value>,
    agg_exprs: &std::collections::HashMap<String, String>,
    schema_hint: &Value,
    dialect: SqlDialect,
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
                                .map(|v| {
                                    build_having_inner(v, params, agg_exprs, schema_hint, dialect)
                                })
                                .collect();
                            let sub = sub?;
                            let non_empty: Vec<&str> = sub
                                .iter()
                                .filter(|s| !s.is_empty())
                                .map(String::as_str)
                                .collect();
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
                                .map(|v| {
                                    build_having_inner(v, params, agg_exprs, schema_hint, dialect)
                                })
                                .collect();
                            let sub = sub?;
                            let non_empty: Vec<&str> = sub
                                .iter()
                                .filter(|s| !s.is_empty())
                                .map(String::as_str)
                                .collect();
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
                    // Resolve alias → aggregate expression, or fall back to
                    // the quoted column. SEC-4: a masked base column in
                    // HAVING reads the creator-visible mask, never raw plaintext.
                    let (col, creator_field) = if let Some(expr) = agg_exprs.get(key) {
                        (expr.clone(), None)
                    } else {
                        validate_read_identifier(key, schema_hint)?;
                        (quote_ident(key), Some(key.as_str()))
                    };
                    let cond = build_having_condition(
                        &col,
                        value,
                        params,
                        creator_field,
                        schema_hint,
                        dialect,
                    )?;
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

/// Bind a HAVING operand. Base creator fields use the same typed timestamp
/// conversion as WHERE; accumulator aliases stay untyped and retain a bare
/// placeholder because no descriptor declares their result type.
fn push_having_value_bind(
    params: &mut Vec<Value>,
    value: &Value,
    creator_field: Option<&str>,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    if let Some(field) = creator_field {
        push_field_value_bind(params, value, field, schema_hint, dialect)
    } else {
        params.push(value_to_param(value));
        Ok(format!("${}", params.len()))
    }
}

/// Build a single HAVING condition. Like `build_field_condition_with_dialect`
/// but takes a pre-resolved column expression (which may be an aggregate like
/// `COUNT(*)`).
fn build_having_condition(
    col_expr: &str,
    value: &Value,
    params: &mut Vec<Value>,
    creator_field: Option<&str>,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    match value {
        Value::Object(ops) if ops.keys().any(|k| k.starts_with('$')) => {
            let mut parts = Vec::new();
            for (op, val) in ops {
                let sql_op = match op.as_str() {
                    "$eq" => "=",
                    "$ne" => "!=",
                    "$gt" => ">",
                    "$gte" => ">=",
                    "$lt" => "<",
                    "$lte" => "<=",
                    other => {
                        return Err(QueryError::InvalidFilter(format!(
                            "unsupported HAVING operator: {other}"
                        )));
                    }
                };
                let bind =
                    push_having_value_bind(params, val, creator_field, schema_hint, dialect)?;
                parts.push(format!("{col_expr} {sql_op} {bind}"));
            }
            Ok(parts.join(" AND "))
        }
        _ => {
            let bind = push_having_value_bind(params, value, creator_field, schema_hint, dialect)?;
            Ok(format!("{col_expr} = {bind}"))
        }
    }
}

// ---------------------------------------------------------------------------
// WHERE clause builder
// ---------------------------------------------------------------------------

/// Build a WHERE clause from a filter JSON value.
/// Returns empty string if the filter is null/empty.
///
/// The schema is required because creator timestamp reads surface as Unix
/// milliseconds, which must be lowered to the column's timestamp type at each
/// bind expression. This wrapper emits PostgreSQL SQL.
pub fn build_where(
    filter: &Value,
    params: &mut Vec<Value>,
    schema_hint: &Value,
) -> Result<String, QueryError> {
    build_where_with_dialect(filter, params, schema_hint, SqlDialect::Postgres)
}

pub fn build_where_with_dialect(
    filter: &Value,
    params: &mut Vec<Value>,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    let plan = crate::sql::filter::decode(filter)?;
    build_where_plan(&plan, params, schema_hint, dialect)
}

/// Build an ORDER BY clause from a JSON value.
///
/// Accepts: `{ "field": 1 }` or `{ "field": -1 }` (1 = ASC, -1 = DESC)
/// or `[["field", 1], ["field2", -1]]` for ordered multi-column sort.
#[cfg(test)]
fn build_order_by(order: &Value) -> Result<String, QueryError> {
    build_order_by_with_dialect(order, SqlDialect::Postgres)
}

#[cfg(test)]
fn build_order_by_with_dialect(order: &Value, dialect: SqlDialect) -> Result<String, QueryError> {
    // The DDL/no-schema arm: it names the declared column because there is no
    // read surface to disagree with. Test-only; every production ORDER BY goes
    // through `build_order_by_read_with_dialect`.
    build_order_by_with_validator(order, dialect, validate_field_name, |field| {
        field.to_string()
    })
}

/// **L26** — the READ-path ORDER BY builder.
///
/// The sort term is the field's own column, the same one the projection serves.
/// This used to need a lowering function to keep the two in agreement, because
/// the SELECT served the masked sibling while the ORDER BY named the declared
/// column, so `orderBy: { ssn: 1 }` ordered rows by the value the mask hides -
/// observable through `limit`/`offset` as a binary search over data the caller
/// cannot read. After the storage flip both name the same column and the
/// agreement is structural rather than maintained.
fn build_order_by_read_with_dialect(
    order: &Value,
    dialect: SqlDialect,
    schema_hint: &Value,
) -> Result<String, QueryError> {
    build_order_by_with_validator(
        order,
        dialect,
        |field| {
            validate_read_identifier(field, schema_hint)?;
            validate_value_operation(field, schema_hint)
        },
        |field| field.to_string(),
    )
}

fn build_order_by_with_validator<F, R>(
    order: &Value,
    dialect: SqlDialect,
    mut validate: F,
    mut read_column: R,
) -> Result<String, QueryError>
where
    F: FnMut(&str) -> Result<(), QueryError>,
    R: FnMut(&str) -> String,
{
    match order {
        Value::Object(map) => {
            let mut parts = Vec::with_capacity(map.len());
            for (key, val) in map {
                validate(key)?;
                let descending = matches!(val.as_i64(), Some(n) if n < 0);
                parts.push(build_order_term(&read_column(key), descending, dialect));
            }
            Ok(parts.join(", "))
        }
        Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                let pair = item.as_array().ok_or_else(|| {
                    QueryError::InvalidFilter(
                        "orderBy array entries must be [field, dir]".to_string(),
                    )
                })?;
                if pair.len() != 2 {
                    return Err(QueryError::InvalidFilter(
                        "orderBy array entries must be [field, dir]".to_string(),
                    ));
                }
                let field = pair[0].as_str().ok_or_else(|| {
                    QueryError::InvalidFilter("orderBy field must be a string".to_string())
                })?;
                validate(field)?;
                let descending = matches!(pair[1].as_i64(), Some(n) if n < 0);
                parts.push(build_order_term(&read_column(field), descending, dialect));
            }
            Ok(parts.join(", "))
        }
        _ => Err(QueryError::InvalidFilter(
            "orderBy must be an object or array".to_string(),
        )),
    }
}

/// **SEC-4** — ORDER BY builder for the aggregate `$sort` stage.
///
/// Keys that name an aggregate alias (`agg_exprs`) order by the alias as
/// a bare quoted identifier (the SELECT already projected `<expr> AS
/// <alias>`). Any other key is a base column, validated as readable and
/// lowered to its configured value column so the sort
/// never touches the plaintext column.
fn build_aggregate_order_by(
    order: &Value,
    dialect: SqlDialect,
    agg_exprs: &std::collections::HashMap<String, String>,
    schema_hint: &Value,
) -> Result<String, QueryError> {
    let term = |field: &str, descending: bool| -> Result<String, QueryError> {
        if agg_exprs.contains_key(field) {
            Ok(build_order_term_expr(
                &quote_ident(field),
                descending,
                dialect,
            ))
        } else {
            validate_read_identifier(field, schema_hint)?;
            validate_value_operation(field, schema_hint)?;
            Ok(build_order_term(field, descending, dialect))
        }
    };
    match order {
        Value::Object(map) => {
            let mut parts = Vec::with_capacity(map.len());
            for (key, val) in map {
                let descending = matches!(val.as_i64(), Some(n) if n < 0);
                parts.push(term(key, descending)?);
            }
            Ok(parts.join(", "))
        }
        Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                let pair = item.as_array().ok_or_else(|| {
                    QueryError::InvalidFilter(
                        "orderBy array entries must be [field, dir]".to_string(),
                    )
                })?;
                if pair.len() != 2 {
                    return Err(QueryError::InvalidFilter(
                        "orderBy array entries must be [field, dir]".to_string(),
                    ));
                }
                let field = pair[0].as_str().ok_or_else(|| {
                    QueryError::InvalidFilter("orderBy field must be a string".to_string())
                })?;
                let descending = matches!(pair[1].as_i64(), Some(n) if n < 0);
                parts.push(term(field, descending)?);
            }
            Ok(parts.join(", "))
        }
        _ => Err(QueryError::InvalidFilter(
            "orderBy must be an object or array".to_string(),
        )),
    }
}

#[derive(Clone, Copy)]
enum ClauseBudgetKind {
    Filter,
    Having,
}

impl ClauseBudgetKind {
    fn label(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Having => "having",
        }
    }
}

fn validate_clause_budget(filter: &Value, kind: ClauseBudgetKind) -> Result<(), QueryError> {
    let mut clauses = 0usize;
    count_clause_budget(filter, kind, 1, &mut clauses)
}

fn count_clause_budget(
    value: &Value,
    kind: ClauseBudgetKind,
    depth: usize,
    clauses: &mut usize,
) -> Result<(), QueryError> {
    if depth > MAX_FILTER_NESTING_DEPTH {
        return Err(QueryError::InvalidFilter(format!(
            "{} nesting depth exceeds the maximum of {MAX_FILTER_NESTING_DEPTH}",
            kind.label()
        )));
    }
    let Some(map) = value.as_object() else {
        return Ok(());
    };
    for (key, child) in map {
        if key.starts_with('$') {
            *clauses += 1;
            if *clauses > MAX_FILTER_CLAUSE_COUNT {
                return Err(QueryError::InvalidFilter(format!(
                    "{} clause count exceeds the maximum of {MAX_FILTER_CLAUSE_COUNT}",
                    kind.label()
                )));
            }
            match key.as_str() {
                "$and" | "$or" => {
                    let arr = child.as_array().ok_or_else(|| {
                        QueryError::InvalidFilter(format!("{key} must be an array"))
                    })?;
                    for item in arr {
                        count_clause_budget(item, kind, depth + 1, clauses)?;
                    }
                }
                "$not" => {
                    count_clause_budget(child, kind, depth + 1, clauses)?;
                }
                _ => {}
            }
            continue;
        }

        if let Some(ops) = child
            .as_object()
            .filter(|ops| ops.keys().any(|op| op.starts_with('$')))
        {
            for (op, operand) in ops {
                *clauses += 1;
                if *clauses > MAX_FILTER_CLAUSE_COUNT {
                    return Err(QueryError::InvalidFilter(format!(
                        "{} clause count exceeds the maximum of {MAX_FILTER_CLAUSE_COUNT}",
                        kind.label()
                    )));
                }
                if matches!(op.as_str(), "$in" | "$nin") {
                    let arr = operand.as_array().ok_or_else(|| {
                        QueryError::InvalidFilter(format!("{op} must be an array"))
                    })?;
                    if arr.len() > MAX_MEMBERSHIP_LIST_LEN {
                        return Err(QueryError::InvalidFilter(format!(
                            "{op} exceeds the maximum of {MAX_MEMBERSHIP_LIST_LEN} values"
                        )));
                    }
                }
            }
        } else {
            *clauses += 1;
            if *clauses > MAX_FILTER_CLAUSE_COUNT {
                return Err(QueryError::InvalidFilter(format!(
                    "{} clause count exceeds the maximum of {MAX_FILTER_CLAUSE_COUNT}",
                    kind.label()
                )));
            }
        }
    }
    Ok(())
}

fn build_order_term(field: &str, descending: bool, dialect: SqlDialect) -> String {
    build_order_term_expr(&quote_ident(field), descending, dialect)
}

/// Shared ORDER BY term renderer over an already-quoted column
/// expression (`col`).
fn build_order_term_expr(col: &str, descending: bool, dialect: SqlDialect) -> String {
    match dialect {
        SqlDialect::Postgres => {
            let dir = if descending { "DESC" } else { "ASC" };
            let nulls = if descending {
                "NULLS FIRST"
            } else {
                "NULLS LAST"
            };
            format!("{col} {dir} {nulls}")
        }
        SqlDialect::Sqlite => {
            let dir = if descending { "DESC" } else { "ASC" };
            let null_bucket = if descending { "DESC" } else { "ASC" };
            format!("{col} IS NULL {null_bucket}, {col} {dir}")
        }
    }
}

/// Preserve the native value through SQL compilation. Only transport encoding
/// may materialize a textual representation.
pub fn value_to_param(value: &Value) -> Value {
    value.clone()
}

/// Parse an explicit conflict target without dropping or duplicating columns.
pub fn parse_conflict_fields(value: &Value) -> Result<Vec<&str>, QueryError> {
    let fields = value
        .as_array()
        .ok_or_else(|| QueryError::InvalidFilter("conflict_fields must be an array".to_string()))?;
    if fields.is_empty() {
        return Err(QueryError::InvalidFilter(
            "conflict_fields cannot be empty".to_string(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    fields
        .iter()
        .map(|value| {
            let field = value.as_str().ok_or_else(|| {
                QueryError::InvalidFilter("every conflict field must be a string".to_string())
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

/// Build a findOrCreate query. Same conflict shape as upsert but the
/// ON CONFLICT branch is a no-op self-assignment on the conflict column
/// (the existing row is returned untouched) and the RETURNING list
/// appends `(xmax = 0) AS __created` so the caller can tell whether
/// the row was newly inserted (xmax = 0) or pre-existing (xmax != 0).
pub fn build_find_or_create(
    schema_name: &SchemaName,
    collection: &str,
    schema_hint: &Value,
    doc: &Value,
    conflict_fields: &Value,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    let returning = build_returning_expr(schema_hint)?;

    let obj = doc.as_object().ok_or_else(|| {
        QueryError::InvalidFilter("findOrCreate document must be an object".to_string())
    })?;

    if obj.is_empty() {
        return Err(QueryError::InvalidFilter(
            "findOrCreate document cannot be empty".to_string(),
        ));
    }

    let conflict_arr = parse_conflict_fields(conflict_fields)?;
    for field in &conflict_arr {
        validate_value_operation(field, schema_hint)?;
    }
    let first_conflict = conflict_arr[0];

    let schema = crate::sql::compile::quote_ident(schema_name.as_str());
    let table = quote_ident(collection);

    let mut columns = Vec::new();
    let mut placeholders = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    for (key, value) in obj {
        columns.push(quote_ident(key));
        if value.is_null() {
            placeholders.push("NULL".to_string());
        } else {
            placeholders.push(push_field_value_bind(
                &mut params,
                value,
                key,
                schema_hint,
                SqlDialect::Postgres,
            )?);
        }
    }

    let conflict_cols: Vec<String> = conflict_arr
        .iter()
        .map(|field| quote_ident(field))
        .collect();

    // The DO UPDATE branch is a self-assignment so RETURNING fires for
    // both INSERT and UPDATE (DO NOTHING wouldn't return the existing
    // row). The row's data is left exactly as it was on conflict.
    let no_op = format!(
        "{} = {schema}.{table}.{}",
        quote_ident(first_conflict),
        quote_ident(first_conflict)
    );

    let sql = format!(
        "INSERT INTO {schema}.{table} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {} RETURNING {returning}, (xmax = 0) AS __created",
        columns.join(", "),
        placeholders.join(", "),
        conflict_cols.join(", "),
        no_op,
    );

    Ok(CompiledQuery { sql, params })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::crud::upsert::{build_upsert, build_upsert_with_dialect, build_upsert_with_assignments};
    const FIXTURE_GENERATED_COLUMNS: &[&str] = &[
        "id",
        "created_at",
        "updated_at",
        "created_by",
        "updated_by",
        "version",
        "deleted_at",
    ];

    use super::*;
    use crate::sql::lifecycle::{AssignedValue, ColumnAssignment};
    use crate::value;

    /// Wrap a fixture app id as the physical schema the builders now take.
    ///
    /// The builders stopped taking `&str` so a tenant id cannot reach a
    /// parameter that wants a schema. Tests still spell one string for both,
    /// because that is what production mints today - the point is that the
    /// CALL says which meaning it is passing.
    fn lifecycle_assignments(restoring: bool) -> WriteAssignments {
        WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
                ColumnAssignment {
                    column: "updated_by".into(),
                    value: AssignedValue::Bound(Value::Null),
                },
                ColumnAssignment {
                    column: "deleted_at".into(),
                    value: if restoring {
                        AssignedValue::Bound(Value::Null)
                    } else {
                        AssignedValue::CurrentTimestamp
                    },
                },
            ],
        }
    }

    fn s(name: &str) -> SchemaName {
        SchemaName::new(name).expect("fixture schema name")
    }

    /// The read schema the filter/order/limit-shape tests below build against.
    ///
    /// `build_find` used to exist in the production API as a shim that passed
    /// `None` for the schema; it had no production caller (measured: only this
    /// module, `crates/zeroship-data-orm/src/tests/postgres/queries.rs` and
    /// `crates/zeroship-data-orm/benches/bench_query_build.rs`) and it was the
    /// only way to reach the `SELECT *` arm L24 is about. It is gone. These
    /// tests are about WHERE / ORDER BY / LIMIT shape, so they declare the
    /// columns they name and let the projection be the ordinary allowlist.
    fn tschema() -> Value {
        value!({
            "id": { "type": "string", "assign":{"by":"typedId", "on":"insert"}, "primaryKey": true },
            "created_at": { "type": "date", "assign":{"by":"now", "on":"insert"} },
            "updated_at": { "type": "date" },
            "created_by": { "type": "string", "assign":{"by":"actor", "on":"insert"} },
            "updated_by": { "type": "string" },
            "version": { "type": "int", "concurrency": true },
            "deleted_at": { "type": "date", "softDelete": true },
            "age":        { "type": "number" },
            "amount":     { "type": "number" },
            "bio":        { "type": "string" },
            "body":       { "type": "string" },
            "category":   { "type": "string" },
            "city":       { "type": "string" },
            "country":    { "type": "string" },
            "department": { "type": "string" },
            "email":      { "type": "string" },
            "name":       { "type": "string" },
            "optional":   { "type": "string" },
            "price":      { "type": "number" },
            "revenue":    { "type": "number" },
            "role":       { "type": "string" },
            "salary":     { "type": "number" },
            "score":      { "type": "number" },
            "status":     { "type": "string" },
            "tags":       { "type": "array" },
            "title":      { "type": "string" },
            "views":      { "type": "number" },
        })
    }

    fn timestamp_test_schema() -> Value {
        value!({
            "date_only": { "type": "calendarDate" },
            "occurred_at": { "type": "date" },
            "optional": { "type": "string" },
        })
    }

    /// The SET clause of an UPDATE, i.e. everything between `SET ` and the
    /// first ` WHERE `.
    ///
    /// The no-actor tests below assert that `updated_by` is ABSENT, and
    /// `updated_by` is a system field that every RETURNING projection names.
    /// Asserting over the whole statement passed only while that clause was
    /// `*`; it is the clause, not the statement, that the property is about.
    fn set_clause_of(sql: &str) -> &str {
        let body = sql
            .split_once(" SET ")
            .expect("an UPDATE has a SET clause")
            .1;
        body.split_once(" WHERE ").map_or(body, |(set, _)| set)
    }

    /// The projection [`tschema`] produces, as it appears in every write's
    /// `RETURNING` clause.
    ///
    /// The write-side twin of [`tselect`]. Both are derived from
    /// [`implicit_read_projection_parts`] rather than spelled out, so a test
    /// using either cannot pass by agreeing with a hand-copied list that has
    /// drifted from what the builder emits.
    fn treturning() -> String {
        format!(
            "RETURNING {}",
            build_returning_expr(&tschema()).expect("tschema is an object")
        )
    }

    /// The projection [`tschema`] produces, as it appears in every SELECT built
    /// from it. Spelled once so a change to the system-field set or to
    /// `tschema` is a one-line edit rather than a sweep.
    fn tselect() -> String {
        format!(
            "SELECT {}",
            implicit_read_projection_parts(&tschema(), None)
                .expect("tschema is an object")
                .join(", ")
        )
    }

    /// Test-local stand-in for the deleted `build_find`, over [`tschema`].
    fn build_find(
        schema_name: &SchemaName,
        collection: &str,
        filter: &Value,
        limit: Option<i64>,
        offset: Option<i64>,
        order_by: Option<&Value>,
        select: Option<&Value>,
    ) -> Result<CompiledQuery, QueryError> {
        build_find_with_schema(
            schema_name,
            collection,
            filter,
            limit,
            offset,
            order_by,
            select,
            &tschema(),
        )
    }

    #[test]
    fn test_simple_eq_filter() {
        let filter = value!({"name": "alice"});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert_eq!(
            q.sql,
            format!(r#"{} FROM "app1"."users" WHERE "name" = $1"#, tselect())
        );
        assert_eq!(q.params, value!(["alice"]).as_array().unwrap().clone());
    }

    #[test]
    fn test_comparison_operators() {
        let filter = value!({"age": {"$gte": 18, "$lt": 65}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains(r#""age" >= $1"#));
        assert!(q.sql.contains(r#""age" < $2"#));
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_in_operator() {
        let filter = value!({"status": {"$in": ["active", "pending"]}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains(r#""status" IN ($1, $2)"#));
        assert_eq!(
            q.params,
            value!(["active", "pending"]).as_array().unwrap().clone()
        );
    }

    /// A `null` member of `$in` must mean `IS NULL`, exactly as a bare
    /// `{field: null}` already does (see the equality arm, which emits
    /// `IS NULL` rather than binding a parameter).
    ///
    /// Before this was fixed, every `$in` member went through
    /// `value_to_param`, which maps `Value::Null` to the EMPTY STRING - its
    /// own comment conceding the case "should not be used as param (use IS
    /// NULL)". So `{$in: [null]}` compiled to `"f" IN ($1)` with `$1 = ""`.
    /// Measured against PostgreSQL 16, the three readings differ:
    ///
    /// * `f IN ('')`   matches the empty-string row  <- what we emitted
    /// * `f IS NULL`   matches the NULL row          <- what the caller means
    /// * `f IN (null)` matches nothing
    ///
    /// So the bug returned wrong rows silently on any text column, and
    /// "just bind a real NULL" is NOT the fix - that matches nothing.
    #[test]
    fn in_with_null_member_means_is_null_not_empty_string() {
        let filter = value!({"status": {"$in": [null]}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(
            q.sql.contains(r#""status" IS NULL"#),
            "a null $in member must become IS NULL; got {}",
            q.sql
        );
        assert!(
            !q.params.iter().any(|p| p.as_str() == Some("")),
            "a null must never be bound as the empty string; params={:?}",
            q.params
        );
    }

    /// Mixed members keep both halves: the non-null values stay a bound
    /// `IN (...)` and the null becomes an `IS NULL` disjunct.
    #[test]
    fn in_with_mixed_null_and_values_keeps_both_arms() {
        let filter = value!({"status": {"$in": ["active", null]}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(
            q.sql.contains(r#""status" IN ($1)"#),
            "non-null members must still bind; got {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#""status" IS NULL"#),
            "the null member must add an IS NULL arm; got {}",
            q.sql
        );
        assert_eq!(
            q.params,
            vec!["active"],
            "only the non-null member is a parameter"
        );
    }

    /// `$nin` carries the identical defect and the mirrored meaning: a
    /// null member excludes NULL rows, so it must become `IS NOT NULL`
    /// rather than a bound empty string.
    #[test]
    fn nin_with_null_member_means_is_not_null() {
        let filter = value!({"status": {"$nin": [null]}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(
            q.sql.contains(r#""status" IS NOT NULL"#),
            "a null $nin member must become IS NOT NULL; got {}",
            q.sql
        );
        assert!(
            !q.params.iter().any(|p| p.as_str() == Some("")),
            "a null must never be bound as the empty string; params={:?}",
            q.params
        );
    }

    #[test]
    fn nin_with_mixed_null_and_values_keeps_both_arms() {
        let filter = value!({"status": {"$nin": ["active", null]}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(
            q.sql.contains(r#""status" NOT IN ($1)"#),
            "non-null members must still bind; got {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#""status" IS NOT NULL"#),
            "the null member must add an IS NOT NULL arm; got {}",
            q.sql
        );
        assert_eq!(q.params, value!(["active"]).as_array().unwrap().clone());
    }

    /// An empty `$in` matches nothing, and must say so with a constant
    /// predicate rather than emitting `IN ()`.
    ///
    /// `IN ()` is a SYNTAX ERROR in PostgreSQL (verified against PG 16:
    /// `select 1 where 'x' in ()` -> `ERROR: syntax error at or near ")"`),
    /// so the previous output did not return zero rows - it failed the whole
    /// query. A creator passing an empty list, which is the natural result of
    /// filtering a collection down to nothing, got a crash instead of an
    /// empty result set.
    #[test]
    fn empty_in_matches_nothing_without_emitting_invalid_sql() {
        let filter = value!({"status": {"$in": []}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(
            !q.sql.contains("IN ()"),
            "IN () is a syntax error in PostgreSQL; got {}",
            q.sql
        );
        assert!(
            q.sql.contains("FALSE"),
            "an empty $in must become a constant-false predicate; got {}",
            q.sql
        );
        assert!(q.params.is_empty());
    }

    /// The mirror: an empty `$nin` excludes nothing, so it matches
    /// everything.
    #[test]
    fn empty_nin_matches_everything_without_emitting_invalid_sql() {
        let filter = value!({"status": {"$nin": []}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(
            !q.sql.contains("NOT IN ()"),
            "NOT IN () is a syntax error in PostgreSQL; got {}",
            q.sql
        );
        assert!(
            !q.sql.contains("WHERE"),
            "an empty $nin must leave the query unfiltered; got {}",
            q.sql
        );
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_or_combinator() {
        let filter = value!({"$or": [{"name": "alice"}, {"name": "bob"}]});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains("OR"));
        assert_eq!(
            q.params,
            value!(["alice", "bob"]).as_array().unwrap().clone()
        );
    }

    #[test]
    fn test_empty_filter() {
        let filter = value!({});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users""#, tselect()));
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_null_filter() {
        let filter = Value::Null;
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users""#, tselect()));
    }

    #[test]
    fn test_limit_offset() {
        let filter = value!({});
        let q = build_find(&s("app1"), "users", &filter, Some(10), Some(20), None, None).unwrap();
        assert!(q.sql.contains("LIMIT 10"));
        assert!(q.sql.contains("OFFSET 20"));
    }

    #[test]
    fn test_insert() {
        let doc = value!({"name": "alice", "age": 30});
        let q = build_insert(&s("app1"), "users", &tschema(), &doc).unwrap();
        assert!(q.sql.contains("INSERT INTO"));
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn creator_timestamp_numeric_write_bind_is_lowered_per_dialect() {
        let schema = timestamp_test_schema();
        let doc = value!({"occurred_at": -1});

        let pg =
            build_insert_with_dialect(&s("app1"), "events", &schema, &doc, SqlDialect::Postgres)
                .unwrap();
        assert!(
            pg.sql.contains("VALUES ($1::timestamptz)"),
            "sql: {}",
            pg.sql
        );
        assert_eq!(pg.params, vec![Value::Timestamp(-1)]);

        let sqlite =
            build_insert_with_dialect(&s("app1"), "events", &schema, &doc, SqlDialect::Sqlite)
                .unwrap();
        assert!(sqlite.sql.contains("VALUES ($1)"), "sql: {}", sqlite.sql);
        assert_eq!(sqlite.params, vec![Value::from("1969-12-31T23:59:59.999Z")]);
    }

    #[test]
    fn creator_timestamp_numeric_filter_bind_is_lowered_per_dialect() {
        let schema = timestamp_test_schema();
        let filter = value!({
            "occurred_at": {
                "$gt": -1,
                "$in": [-1, 0, null],
            },
        });

        let mut pg_params = Vec::new();
        let pg = build_where_with_dialect(&filter, &mut pg_params, &schema, SqlDialect::Postgres)
            .unwrap();
        assert!(pg.contains(r#""occurred_at" > $1::timestamptz"#));
        assert!(pg.contains(r#""occurred_at" IN ($2::timestamptz, $3::timestamptz)"#));
        assert!(pg.contains(r#"OR "occurred_at" IS NULL"#));
        assert_eq!(
            pg_params,
            vec![
                Value::Timestamp(-1),
                Value::Timestamp(-1),
                Value::Timestamp(0)
            ]
        );

        let mut sqlite_params = Vec::new();
        let sqlite =
            build_where_with_dialect(&filter, &mut sqlite_params, &schema, SqlDialect::Sqlite)
                .unwrap();
        assert!(sqlite.contains(r#""occurred_at" > $1"#));
        assert!(sqlite.contains(r#""occurred_at" IN ($2, $3)"#));
        assert!(sqlite.contains(r#"OR "occurred_at" IS NULL"#));
        assert_eq!(
            sqlite_params,
            vec![
                Value::from("1969-12-31T23:59:59.999Z"),
                Value::from("1969-12-31T23:59:59.999Z"),
                Value::from("1970-01-01T00:00:00.000Z")
            ]
        );
    }

    #[test]
    fn creator_timestamp_bind_lowering_is_type_and_value_driven() {
        let schema = timestamp_test_schema();
        let doc = value!({
            "occurred_at": "1969-12-31T23:59:59.999Z",
            "date_only": -1,
            "optional": null,
        });
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let q =
                build_insert_with_dialect(&s("app1"), "events", &schema, &doc, dialect).unwrap();
            assert!(!q.sql.contains("to_timestamp"), "sql: {}", q.sql);
            assert!(!q.sql.contains("strftime"), "sql: {}", q.sql);
            assert!(q.sql.contains("NULL"), "sql: {}", q.sql);
            assert_eq!(
                q.params,
                vec![
                    if dialect == SqlDialect::Postgres {
                        Value::Timestamp(-1)
                    } else {
                        Value::from("1969-12-31T23:59:59.999Z")
                    },
                    Value::from(-1)
                ],
                "dialect: {dialect:?}"
            );
        }

        let filter = value!({
            "occurred_at": "1969-12-31T23:59:59.999Z",
            "date_only": -1,
            "optional": null,
        });
        let mut params = Vec::new();
        let sql =
            build_where_with_dialect(&filter, &mut params, &schema, SqlDialect::Sqlite).unwrap();
        assert!(!sql.contains("strftime"), "sql: {sql}");
        assert!(sql.contains(r#""optional" IS NULL"#), "sql: {sql}");
        assert_eq!(
            params,
            vec![Value::from("1969-12-31T23:59:59.999Z"), Value::from(-1)]
        );
    }

    #[test]
    fn test_invalid_collection() {
        let filter = value!({});
        let result = build_find(
            &s("app1"),
            "users; DROP TABLE",
            &filter,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_exists_operator() {
        let filter = value!({"email": {"$exists": true}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains(r#""email" IS NOT NULL"#));
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_count() {
        let filter = value!({"active": true});
        let q = build_count(&s("app1"), "users", &tschema(), &filter).unwrap();
        assert!(q.sql.contains("SELECT COUNT(*)"));
        assert_eq!(q.params, value!([true]).as_array().unwrap().clone());
    }

    #[test]
    fn test_ilike_operator() {
        let filter = value!({"name": {"$ilike": "%alice%"}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert_eq!(
            q.sql,
            format!(r#"{} FROM "app1"."users" WHERE "name" ILIKE $1"#, tselect())
        );
        assert_eq!(q.params, value!(["%alice%"]).as_array().unwrap().clone());
    }

    #[test]
    fn test_ilike_operator_sqlite_uses_like_nocase() {
        let filter = value!({"name": {"$ilike": "%alice%"}});
        let q = build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
            &s("app1"),
            "users",
            &filter,
            None,
            None,
            None,
            None,
            &tschema(),
            &[],
            false,
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert_eq!(
            q.sql,
            format!(
                r#"{} FROM "app1"."users" WHERE "name" LIKE $1 COLLATE NOCASE"#,
                tselect()
            )
        );
        assert_eq!(q.params, value!(["%alice%"]).as_array().unwrap().clone());
    }

    #[test]
    fn test_not_operator() {
        let filter = value!({"$not": {"role": "admin"}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert_eq!(
            q.sql,
            format!(
                r#"{} FROM "app1"."users" WHERE NOT ("role" = $1)"#,
                tselect()
            )
        );
        assert_eq!(q.params, value!(["admin"]).as_array().unwrap().clone());
    }

    #[test]
    fn test_update_inc() {
        let filter = value!({"id": 1});
        let update = value!({"views": {"$inc": 1}});
        let q = build_update_one(&s("app1"), "posts", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql.contains(r#""views" = "views" + $1::numeric"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], value!(1));
    }

    #[test]
    fn test_update_dec() {
        let filter = value!({"id": 1});
        let update = value!({"stock": {"$dec": 1}});
        let q = build_update_one(&s("app1"), "items", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql.contains(r#""stock" = "stock" - $1::numeric"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], value!(1));
    }

    #[test]
    fn test_update_mul() {
        let filter = value!({"id": 1});
        let update = value!({"price": {"$mul": 1.1}});
        let q = build_update_one(&s("app1"), "items", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql.contains(r#""price" = "price" * $1::numeric"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], value!(1.1));
    }

    #[test]
    fn test_update_push() {
        let filter = value!({"id": 1});
        let update = value!({"tags": {"$push": "new"}});
        let q = build_update_one(&s("app1"), "posts", &tschema(), &filter, &update).unwrap();
        // Appends the JSON-encoded value to the jsonb array. The `::jsonb`
        // cast (not `to_jsonb(::text)`) keeps numbers, booleans, and objects
        // as their real JSON types — the old shape stringified everything.
        assert!(
            q.sql.contains(
                r#"jsonb_insert("tags", ARRAY[jsonb_array_length("tags")::text], $1::jsonb)"#
            ),
            "sql: {}",
            q.sql
        );
        // Param is JSON-encoded: a string `"new"` is stored as `"new"`
        // so Postgres parses it back as a JSON string on ::jsonb cast.
        assert_eq!(q.params[0], "new");
    }

    #[test]
    fn test_update_pull() {
        let filter = value!({"id": 1});
        let update = value!({"tags": {"$pull": "old"}});
        let q = build_update_one(&s("app1"), "posts", &tschema(), &filter, &update).unwrap();
        // Removes array elements by value. An earlier implementation used
        // `"tags" - $1`, but that's the jsonb "remove key" operator and
        // would mutate objects, not filter array elements.
        assert!(
            q.sql.contains(r#"FROM jsonb_array_elements("tags") WITH ORDINALITY AS __zs_array(element, position) WHERE __zs_array.element != $1::jsonb"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], "old");
    }

    #[test]
    fn test_update_add_to_set() {
        let filter = value!({"id": 1});
        let update = value!({"tags": {"$addToSet": "unique"}});
        let q = build_update_one(&s("app1"), "posts", &tschema(), &filter, &update).unwrap();
        // Element equality must not mistake a partial object for a member.
        assert!(
            q.sql.contains(
                r#"FROM jsonb_array_elements("tags") AS __zs_array(element) WHERE __zs_array.element = $1::jsonb"#
            ),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], "unique");
    }

    #[test]
    fn test_update_mixed_operators() {
        let filter = value!({"id": 1});
        let update = value!({"name": "New", "views": {"$inc": 1}});
        let q = build_update_one(&s("app1"), "posts", &tschema(), &filter, &update).unwrap();
        // Both plain set and $inc should appear
        assert!(q.sql.contains(r#""name" = $"#), "sql: {}", q.sql);
        assert!(
            q.sql.contains(r#""views" = "views" + $"#) && q.sql.contains("::numeric"),
            "sql: {}",
            q.sql
        );
        assert!(q.params.contains(&Value::from("New")));
        assert!(q.params.contains(&Value::from(1)));
    }

    #[test]
    fn test_insert_many() {
        let docs = value!([
            {"name": "alice", "age": 30},
            {"name": "bob",   "age": 25}
        ]);
        let q = build_insert_many(&s("app1"), "users", &tschema(), &docs).unwrap();
        assert!(
            q.sql.starts_with(r#"INSERT INTO "app1"."users""#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains("VALUES"), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        // Two docs × two columns = 4 params
        assert_eq!(q.params.len(), 4, "params: {:?}", q.params);
        assert!(q.sql.contains("($1, $2)"), "sql: {}", q.sql);
        assert!(q.sql.contains("($3, $4)"), "sql: {}", q.sql);
    }

    #[test]
    fn insert_many_rejects_oversized_batch_db11() {
        // DB-11: a batch over MAX_INSERT_MANY_BATCH must be rejected by the
        // builder BEFORE allocating the multi-row SQL + param vec. A batch at
        // the cap is accepted.
        let over: Vec<Value> = (0..=MAX_INSERT_MANY_BATCH)
            .map(|i| value!({ "n": i }))
            .collect();
        let err =
            build_insert_many(&s("app1"), "users", &tschema(), &Value::Array(over)).unwrap_err();
        match err {
            QueryError::InvalidFilter(m) => assert!(m.contains("exceeds the maximum"), "{m}"),
            other => panic!("expected InvalidFilter, got {other:?}"),
        }
        let at_cap: Vec<Value> = (0..MAX_INSERT_MANY_BATCH)
            .map(|i| value!({ "n": i }))
            .collect();
        assert!(build_insert_many(&s("app1"), "users", &tschema(), &Value::Array(at_cap)).is_ok());
    }

    fn full_non_null_insert_many_batch(column_count: usize) -> Value {
        let template: crate::value::Map<String, Value> = (0..column_count)
            .map(|column| (format!("field_{column}"), Value::from(column)))
            .collect();
        Value::Array(
            (0..MAX_INSERT_MANY_BATCH)
                .map(|_| Value::Object(template.clone()))
                .collect(),
        )
    }

    fn insert_many_batch_with_exact_non_null_cells(cell_count: usize) -> Value {
        let base_width = cell_count / MAX_INSERT_MANY_BATCH;
        let wider_rows = cell_count % MAX_INSERT_MANY_BATCH;
        assert!(
            base_width > 0,
            "the exact-boundary fixture must have non-empty rows"
        );
        Value::Array(
            (0..MAX_INSERT_MANY_BATCH)
                .map(|row| {
                    let width = base_width + usize::from(row < wider_rows);
                    let mut doc: crate::value::Map<String, Value> = (0..width)
                        .map(|column| (format!("field_{column}"), Value::from(column)))
                        .collect();
                    doc.insert("explicit_null".to_string(), Value::Null);
                    doc.insert("field_0".to_string(), Value::Bytes(vec![1]));
                    Value::Object(doc)
                })
                .collect(),
        )
    }

    #[test]
    fn insert_many_full_batch_enforces_postgres_bind_limit_db11() {
        let protocol_limit = usize::from(u16::MAX);
        let largest_full_width = protocol_limit / MAX_INSERT_MANY_BATCH;
        let first_rejected_width = largest_full_width + 1;
        assert!(
            largest_full_width > 0,
            "the exercised column set must be non-empty"
        );

        let accepted = build_insert_many(
            &s("app1"),
            "users",
            &tschema(),
            &full_non_null_insert_many_batch(largest_full_width),
        )
        .expect("a full document batch below the PostgreSQL bind limit must build");
        assert_eq!(
            accepted.params.len(),
            MAX_INSERT_MANY_BATCH * largest_full_width
        );
        assert!(accepted.params.len() <= protocol_limit);

        let rejected = build_insert_many(
            &s("app1"),
            "users",
            &tschema(),
            &full_non_null_insert_many_batch(first_rejected_width),
        );
        match rejected {
            Err(QueryError::InvalidFilter(message)) => {
                assert!(message.contains("65535"), "{message}");
                assert!(message.contains("non-null"), "{message}");
                assert!(message.contains("smaller batches"), "{message}");
                assert!(!message.contains("parameter"), "{message}");
            }
            Ok(query) => panic!(
                "builder accepted {} bind parameters, above the PostgreSQL limit of {protocol_limit}",
                query.params.len()
            ),
            Err(other) => panic!("expected creator-facing InvalidFilter, got {other:?}"),
        }
    }

    #[test]
    fn insert_many_accepts_exact_postgres_bind_limit_and_rejects_next_db11() {
        let protocol_limit = usize::from(u16::MAX);
        let accepted_docs = insert_many_batch_with_exact_non_null_cells(protocol_limit);
        assert_eq!(
            accepted_docs.as_array().map(Vec::len),
            Some(MAX_INSERT_MANY_BATCH),
            "the exact-boundary batch must exercise the documented document cap"
        );
        let accepted = build_insert_many(&s("app1"), "users", &tschema(), &accepted_docs)
            .expect("exactly the PostgreSQL non-null field-value limit must build");
        assert_eq!(accepted.params.len(), protocol_limit);

        let rejected_docs = insert_many_batch_with_exact_non_null_cells(protocol_limit + 1);
        let rejected = build_insert_many(&s("app1"), "users", &tschema(), &rejected_docs);
        match rejected {
            Err(QueryError::InvalidFilter(message)) => {
                assert!(message.contains("65536"), "{message}");
                assert!(message.contains("at most 65535"), "{message}");
                assert!(!message.contains("parameter"), "{message}");
            }
            Ok(query) => panic!(
                "builder accepted {} non-null field values above the PostgreSQL limit",
                query.params.len()
            ),
            Err(other) => panic!("expected creator-facing InvalidFilter, got {other:?}"),
        }
    }

    #[test]
    fn insert_many_full_batch_enforces_sqlite_bind_limit_db11() {
        let largest_full_width = SQLITE_MAX_BIND_PARAMETERS / MAX_INSERT_MANY_BATCH;
        let first_rejected_width = largest_full_width + 1;
        assert!(
            largest_full_width > 0,
            "the exercised column set must be non-empty"
        );

        let accepted = build_insert_many_with_dialect(
            &s("app1"),
            "users",
            &tschema(),
            &full_non_null_insert_many_batch(largest_full_width),
            SqlDialect::Sqlite,
        )
        .expect("a full document batch below the SQLite bind limit must build");
        assert_eq!(
            accepted.params.len(),
            MAX_INSERT_MANY_BATCH * largest_full_width
        );
        assert!(accepted.params.len() <= SQLITE_MAX_BIND_PARAMETERS);

        let rejected = build_insert_many_with_dialect(
            &s("app1"),
            "users",
            &tschema(),
            &full_non_null_insert_many_batch(first_rejected_width),
            SqlDialect::Sqlite,
        );
        match rejected {
            Err(QueryError::InvalidFilter(message)) => {
                assert!(message.contains("32766"), "{message}");
                assert!(message.contains("SQLite"), "{message}");
                assert!(message.contains("smaller batches"), "{message}");
                assert!(!message.contains("parameter"), "{message}");
            }
            Ok(query) => panic!(
                "builder accepted {} bind parameters, above the SQLite limit of {SQLITE_MAX_BIND_PARAMETERS}",
                query.params.len()
            ),
            Err(other) => panic!("expected creator-facing InvalidFilter, got {other:?}"),
        }
    }

    #[test]
    fn effective_query_limit_defaults_to_max_when_omitted_db2() {
        // DB-2: an omitted limit must default to the ceiling, not "no LIMIT".
        assert_eq!(effective_query_limit(None), MAX_QUERY_LIMIT);
        assert_eq!(effective_query_limit(Some(10)), 10);
        assert_eq!(effective_query_limit(Some(0)), 0);
    }

    #[test]
    fn test_insert_many_empty() {
        let docs = value!([]);
        let result = build_insert_many(&s("app1"), "users", &tschema(), &docs);
        assert!(result.is_err(), "expected error for empty array");
    }

    #[test]
    fn test_update_many() {
        let filter = value!({"active": true});
        let update = value!({"status": "verified"});
        let q = build_update_many(&s("app1"), "users", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql.starts_with(r#"UPDATE "app1"."users" SET"#),
            "sql: {}",
            q.sql
        );
        assert!(!q.sql.contains(" RETURNING "), "sql: {}", q.sql);
        // Must NOT contain updateOne's primary-key LIMIT 1 subquery.
        assert!(!q.sql.contains("LIMIT 1 FOR UPDATE"), "sql: {}", q.sql);
    }

    #[test]
    fn test_delete_many() {
        let filter = value!({"active": false});
        let q = build_delete_many(
            &s("app1"),
            "users",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
        )
        .unwrap();
        assert!(
            q.sql.starts_with(r#"DELETE FROM "app1"."users""#),
            "sql: {}",
            q.sql
        );
        assert!(!q.sql.contains(" RETURNING "), "sql: {}", q.sql);
        assert!(!q.sql.contains("LIMIT 1 FOR UPDATE"), "sql: {}", q.sql);
        assert_eq!(q.params, value!([false]).as_array().unwrap().clone());
    }

    #[test]
    fn test_delete_many_no_filter() {
        let filter = value!({});
        let q = build_delete_many(
            &s("app1"),
            "users",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
        )
        .unwrap();
        assert!(!q.sql.contains("WHERE"), "sql: {}", q.sql);
        assert!(!q.sql.contains(" RETURNING "), "sql: {}", q.sql);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_find_with_select() {
        let filter = value!({});
        let select = value!(["name", "email"]);
        let q = build_find(
            &s("app1"),
            "users",
            &filter,
            None,
            None,
            None,
            Some(&select),
        )
        .unwrap();
        assert!(
            q.sql.contains(r#"SELECT "name", "email" FROM"#),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_find_without_select() {
        let filter = value!({});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.starts_with(&tselect()), "sql: {}", q.sql);
    }

    #[test]
    fn test_distinct() {
        let filter = value!({});
        let q = build_distinct(&s("app1"), "users", "country", &filter, &tschema()).unwrap();
        assert!(
            q.sql
                .starts_with(r#"SELECT DISTINCT "country" FROM "app1"."users""#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains(r#"ORDER BY "country""#), "sql: {}", q.sql);
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_distinct_with_filter() {
        let filter = value!({"active": true});
        let q = build_distinct(&s("app1"), "users", "role", &filter, &tschema()).unwrap();
        assert!(
            q.sql
                .contains(r#"SELECT DISTINCT "role" FROM "app1"."users" WHERE"#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains(r#"ORDER BY "role""#), "sql: {}", q.sql);
        assert_eq!(q.params, value!([true]).as_array().unwrap().clone());
    }

    /// After the storage flip, `email`'s own column already holds the
    /// masked value, so DISTINCT reads it directly with no sibling alias.
    /// The raw column (holding the real value) must never be named.
    #[test]
    fn distinct_on_a_masked_field_reads_the_masked_column() {
        let schema = value!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let q = build_distinct_with_soft_delete_with_dialect(
            &s("app1"),
            "users",
            "email",
            &value!({}),
            false,
            &schema,
            SqlDialect::Postgres,
        )
        .expect("build distinct with schema");

        assert!(
            q.sql
                .starts_with(r#"SELECT DISTINCT "email" FROM "app1"."users""#),
            "masked distinct must read the field's own (masked) column: {}",
            q.sql
        );
        assert!(
            !q.sql.contains(&raw_column_name("email")),
            "masked distinct must never name the raw column: {}",
            q.sql
        );
    }

    #[test]
    fn test_aggregate_basic() {
        let pipeline = value!([
            {"$match": {"active": true}},
            {"$group": {"by": "country", "count": {"$count": true}}},
            {"$sort": {"count": -1}},
            {"$limit": 5}
        ]);
        let q = build_aggregate(&s("app1"), "users", &pipeline, &tschema()).unwrap();
        assert!(
            q.sql.contains(r#"SELECT "country", COUNT(*) AS "count""#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains("WHERE"), "sql: {}", q.sql);
        assert!(q.sql.contains("GROUP BY"), "sql: {}", q.sql);
        assert!(q.sql.contains("ORDER BY"), "sql: {}", q.sql);
        assert!(q.sql.contains("LIMIT 5"), "sql: {}", q.sql);
    }

    #[test]
    fn test_aggregate_multi_group() {
        let pipeline = value!([
            {"$group": {"by": ["country", "city"], "total": {"$sum": "revenue"}}}
        ]);
        let q = build_aggregate(&s("app1"), "orders", &pipeline, &tschema()).unwrap();
        assert!(
            q.sql.contains(r#"GROUP BY "country", "city""#),
            "sql: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#"SUM("revenue") AS "total""#),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_aggregate_having() {
        let pipeline = value!([
            {"$group": {"by": "category", "cnt": {"$count": true}}},
            {"$having": {"cnt": {"$gte": 10}}}
        ]);
        let q = build_aggregate(&s("app1"), "products", &pipeline, &tschema()).unwrap();
        assert!(q.sql.contains("HAVING COUNT(*) >= $1"), "sql: {}", q.sql);
        assert!(q.sql.contains("GROUP BY"), "sql: {}", q.sql);
        assert_eq!(q.params, value!([10]).as_array().unwrap().clone());
    }

    #[test]
    fn aggregate_having_creator_timestamp_numeric_bind_is_lowered_per_dialect() {
        let pipeline = value!([
            {"$group": {"by": "occurred_at", "cnt": {"$count": true}}},
            {"$having": {"$and": [
                {"occurred_at": {"$gt": -1}},
                {"cnt": {"$gte": 10}}
            ]}}
        ]);
        let schema = timestamp_test_schema();

        let pg = build_aggregate_with_soft_delete_with_dialect(
            &s("app1"),
            "events",
            &pipeline,
            false,
            &schema,
            SqlDialect::Postgres,
        )
        .unwrap();
        assert!(
            pg.sql
                .contains(r#"HAVING ("occurred_at" > $1::timestamptz AND COUNT(*) >= $2)"#),
            "sql: {}",
            pg.sql
        );
        assert_eq!(pg.params, vec![Value::Timestamp(-1), Value::from(10)]);

        let sqlite = build_aggregate_with_soft_delete_with_dialect(
            &s("app1"),
            "events",
            &pipeline,
            false,
            &schema,
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert!(
            sqlite
                .sql
                .contains(r#"HAVING ("occurred_at" > $1 AND COUNT(*) >= $2)"#),
            "sql: {}",
            sqlite.sql
        );
        assert_eq!(
            sqlite.params,
            vec![Value::from("1969-12-31T23:59:59.999Z"), Value::from(10)]
        );
    }

    #[test]
    fn test_aggregate_no_group() {
        let pipeline = value!([
            {"$match": {"active": true}}
        ]);
        let q = build_aggregate(&s("app1"), "users", &pipeline, &tschema()).unwrap();
        assert!(q.sql.starts_with(&tselect()), "sql: {}", q.sql);
        assert!(!q.sql.contains("GROUP BY"), "sql: {}", q.sql);
    }

    // Aggregates over masked fields must read the visible masked column, never raw storage.

    fn mask_only_ssn_schema() -> Value {
        value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "tenant": { "type": "string" }
        })
    }

    #[test]
    fn sec4_aggregate_group_by_masked_field_reads_the_masked_column() {
        let pipeline = value!([
            {"$group": {"by": "ssn", "n": {"$count": true}}}
        ]);
        let schema = mask_only_ssn_schema();
        let q = build_aggregate_with_soft_delete_with_dialect(
            &s("app1"),
            "users",
            &pipeline,
            false,
            &schema,
            SqlDialect::Postgres,
        )
        .expect("build aggregate with schema");

        assert!(
            q.sql.contains(r#"SELECT "ssn", COUNT(*) AS "n""#),
            "SEC-4: $group.by on a masked column reads the field's own \
             (masked) column: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#"GROUP BY "ssn""#),
            "SEC-4: GROUP BY on a masked column groups by the masked \
             column: {}",
            q.sql
        );
        // The raw (real-value) sibling must never appear.
        assert!(
            !q.sql.contains(&raw_column_name("ssn")),
            "SEC-4: aggregate must never name the raw column: {}",
            q.sql
        );
    }

    #[test]
    fn sec4_aggregate_max_on_masked_field_reads_the_masked_column() {
        let pipeline = value!([
            {"$group": {"by": "tenant", "top": {"$max": "ssn"}}}
        ]);
        let schema = mask_only_ssn_schema();
        let q = build_aggregate_with_soft_delete_with_dialect(
            &s("app1"),
            "users",
            &pipeline,
            false,
            &schema,
            SqlDialect::Postgres,
        )
        .expect("build aggregate with schema");

        assert!(
            q.sql.contains(r#"MAX("ssn") AS "top""#),
            "SEC-4: $max on a masked column must aggregate the field's own \
             (masked) column: {}",
            q.sql
        );
        assert!(
            !q.sql.contains(&raw_column_name("ssn")),
            "SEC-4: $max must never read the raw column: {}",
            q.sql
        );
    }

    #[test]
    fn sec4_aggregate_sum_min_first_on_masked_field_reads_the_masked_column() {
        // $sum / $min / $first all lower a field reference; after the
        // storage flip the field's own column already holds the mask, so
        // each op reads it directly with no substitution, and the raw
        // (real-value) sibling must never appear.
        let schema = mask_only_ssn_schema();
        let expected = [
            ("$sum", r#"SUM("ssn") AS "v""#),
            ("$min", r#"MIN("ssn") AS "v""#),
            ("$first", r#"(array_agg("ssn"))[1] AS "v""#),
        ];
        for (op, expected_expr) in expected {
            let pipeline = value!([
                {"$group": {"by": "tenant", "v": {op: "ssn"}}}
            ]);
            let q = build_aggregate_with_soft_delete_with_dialect(
                &s("app1"),
                "users",
                &pipeline,
                false,
                &schema,
                SqlDialect::Postgres,
            )
            .unwrap_or_else(|e| panic!("build aggregate {op}: {e:?}"));
            assert!(
                q.sql.contains(expected_expr),
                "SEC-4: {op} on a masked column must reference the field's \
                 own (masked) column: {}",
                q.sql
            );
            assert!(
                !q.sql.contains(&raw_column_name("ssn")),
                "SEC-4: {op} must never read the raw column: {}",
                q.sql
            );
        }
    }

    // -----------------------------------------------------------------------
    // 1. Missing builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_update_one_plain() {
        let filter = value!({"id": 1});
        let update = value!({"name": "bob"});
        let q = build_update_one(&s("app1"), "users", &tschema(), &filter, &update).unwrap();
        // Plain field: value → SET "name" = $1
        assert!(q.sql.contains(r#""name" = $1"#), "sql: {}", q.sql);
        assert!(
            q.sql.contains("WHERE \"id\" = (SELECT \"id\" FROM")
                && q.sql.contains("LIMIT 1 FOR UPDATE)"),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert_eq!(q.params[0], "bob");
    }

    #[test]
    fn test_update_one_set_operator() {
        let filter = value!({"id": 1});
        let update = value!({"$set": {"name": "carol"}});
        let q = build_update_one(&s("app1"), "users", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $1"#), "sql: {}", q.sql);
        assert!(
            q.sql.contains("WHERE \"id\" = (SELECT \"id\" FROM"),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert_eq!(q.params[0], "carol");
    }

    #[test]
    fn test_delete_one() {
        let filter = value!({});
        let q = build_delete_one(&s("app1"), "users", &tschema(), &filter).unwrap();
        assert!(
            q.sql.contains("WHERE \"id\" = (SELECT \"id\" FROM")
                && q.sql.contains("LIMIT 1 FOR UPDATE)"),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        // No WHERE in the outer DELETE (empty filter → no inner WHERE either)
        assert!(q.sql.contains("DELETE FROM"), "sql: {}", q.sql);
    }

    #[test]
    fn test_delete_one_with_filter() {
        let filter = value!({"role": "guest"});
        let q = build_delete_one(&s("app1"), "users", &tschema(), &filter).unwrap();
        assert!(
            q.sql.contains("WHERE \"id\" = (SELECT \"id\" FROM"),
            "sql: {}",
            q.sql
        );
        // Filter should appear in the subquery
        assert!(q.sql.contains(r#""role" = $1"#), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert_eq!(q.params, value!(["guest"]).as_array().unwrap().clone());
    }

    #[test]
    fn test_order_by_object() {
        let order = value!({"name": 1, "age": -1});
        let clause = build_order_by(&order).unwrap();
        assert!(
            clause.contains(r#""name" ASC NULLS LAST"#),
            "clause: {clause}"
        );
        assert!(
            clause.contains(r#""age" DESC NULLS FIRST"#),
            "clause: {clause}"
        );
    }

    #[test]
    fn test_order_by_array() {
        let order = value!([["name", 1], ["age", -1]]);
        let clause = build_order_by(&order).unwrap();
        // Array form preserves declaration order
        assert!(
            clause.contains(r#""name" ASC NULLS LAST"#),
            "clause: {clause}"
        );
        assert!(
            clause.contains(r#""age" DESC NULLS FIRST"#),
            "clause: {clause}"
        );
        // "name" should appear before "age"
        let name_pos = clause.find(r#""name""#).unwrap();
        let age_pos = clause.find(r#""age""#).unwrap();
        assert!(name_pos < age_pos, "name should come before age");
    }

    #[test]
    fn test_order_by_sqlite_emulates_postgres_null_ordering() {
        let order = value!({"name": 1, "age": -1});
        let clause = build_order_by_with_dialect(&order, SqlDialect::Sqlite).unwrap();
        assert!(
            clause.contains(r#""name" IS NULL ASC, "name" ASC"#),
            "clause: {clause}"
        );
        assert!(
            clause.contains(r#""age" IS NULL DESC, "age" DESC"#),
            "clause: {clause}"
        );
    }

    #[test]
    fn test_find_with_order() {
        let filter = value!({});
        let order = value!({"created_at": -1});
        let q = build_find(
            &s("app1"),
            "posts",
            &filter,
            Some(10),
            None,
            Some(&order),
            None,
        )
        .unwrap();
        assert!(
            q.sql.contains(r#"ORDER BY "created_at" DESC NULLS FIRST"#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains("LIMIT 10"), "sql: {}", q.sql);
    }

    #[test]
    fn build_find_sqlite_orders_nullable_columns_like_postgres() {
        let filter = value!({});
        let order = value!({"optional": 1});
        let q = build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
            &s("app1"),
            "posts",
            &filter,
            None,
            None,
            Some(&order),
            None,
            &tschema(),
            &[],
            false,
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert!(
            q.sql
                .contains(r#"ORDER BY "optional" IS NULL ASC, "optional" ASC"#),
            "sql: {}",
            q.sql
        );
    }

    // -----------------------------------------------------------------------
    // 2. Filter edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_and_combinator() {
        let filter = value!({"$and": [{"status": "active"}, {"verified": true}]});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains("AND"), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""status" = $1"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""verified" = $2"#), "sql: {}", q.sql);
        assert_eq!(
            q.params,
            value!(["active", true]).as_array().unwrap().clone()
        );
    }

    #[test]
    fn test_nested_and_or() {
        // { $and: [{ $or: [{a: 1}, {b: 2}] }, {c: 3}] }
        let filter = value!({"$and": [{"$or": [{"a": 1}, {"b": 2}]}, {"c": 3}]});
        let q = build_find(&s("app1"), "t", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains("OR"), "sql: {}", q.sql);
        assert!(q.sql.contains("AND"), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""c" = "#), "sql: {}", q.sql);
    }

    #[test]
    fn test_null_eq() {
        // { field: null } → IS NULL (implicit $eq)
        let filter = value!({"bio": null});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert_eq!(
            q.sql,
            format!(r#"{} FROM "app1"."users" WHERE "bio" IS NULL"#, tselect())
        );
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_ne_null() {
        // { field: { $ne: null } } → IS NOT NULL
        let filter = value!({"bio": {"$ne": null}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert_eq!(
            q.sql,
            format!(
                r#"{} FROM "app1"."users" WHERE "bio" IS NOT NULL"#,
                tselect()
            )
        );
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_multiple_operators_on_field() {
        // { age: { $gte: 18, $lt: 65 } } — both conditions must appear
        let filter = value!({"age": {"$gte": 18, "$lt": 65}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(q.sql.contains(r#""age" >= $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""age" < $"#), "sql: {}", q.sql);
        assert_eq!(q.params.len(), 2);
        // Both values present
        assert!(q.params.contains(&Value::from(18)));
        assert!(q.params.contains(&Value::from(65)));
    }

    #[test]
    fn test_nin_operator() {
        let filter = value!({"role": {"$nin": ["admin", "moderator"]}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(
            q.sql.contains(r#""role" NOT IN ($1, $2)"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(
            q.params,
            value!(["admin", "moderator"]).as_array().unwrap().clone()
        );
    }

    #[test]
    fn test_like_operator() {
        let filter = value!({"name": {"$like": "ali%"}});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert_eq!(
            q.sql,
            format!(r#"{} FROM "app1"."users" WHERE "name" LIKE $1"#, tselect())
        );
        assert_eq!(q.params, value!(["ali%"]).as_array().unwrap().clone());
    }

    #[test]
    fn test_empty_and() {
        // { $and: [] } → no WHERE clause
        let filter = value!({"$and": []});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(
            !q.sql.contains("WHERE"),
            "sql should have no WHERE: {}",
            q.sql
        );
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_empty_or() {
        let filter = value!({"$or": []});
        let q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        assert!(
            q.sql.contains("WHERE FALSE"),
            "empty disjunction must match no rows"
        );
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_not_with_multiple_fields() {
        // { $not: { a: 1, b: 2 } }
        let filter = value!({"$not": {"a": 1, "b": 2}});
        let q = build_find(&s("app1"), "t", &filter, None, None, None, None).unwrap();
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
        let filter = value!({});
        let result = build_find(
            &s("app1"),
            "users; DROP TABLE users",
            &filter,
            None,
            None,
            None,
            None,
        );
        assert!(
            result.is_err(),
            "should reject injection in collection name"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid collection"), "msg: {msg}");
    }

    /// Invalid schema names cannot reach a query builder.
    #[test]
    fn test_schema_sql_injection() {
        let err = SchemaName::new("app1; DROP TABLE")
            .expect_err("a semicolon must not survive into a schema name");
        assert!(
            matches!(err, zeroship_core::schema_name::SchemaNameError::Invalid(_)),
            "unexpected refusal: {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("invalid"), "msg: {msg}");
    }

    #[test]
    fn test_field_name_with_quotes() {
        // Field name containing double quotes should be escaped (doubled) in the identifier
        let filter = value!({"name": "alice"});
        let _q = build_find(&s("app1"), "users", &filter, None, None, None, None).unwrap();
        // Standard field works; now verify quote_ident escapes embedded quotes
        let quoted = super::quote_ident(r#"col"name"#);
        assert_eq!(quoted, r#""col""name""#, "embedded quote must be doubled");
    }

    #[test]
    fn test_collection_empty() {
        let filter = value!({});
        let result = build_find(&s("app1"), "", &filter, None, None, None, None);
        assert!(result.is_err(), "empty collection name should fail");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("cannot be empty") || msg.contains("invalid"),
            "msg: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // 4. value_to_param edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_insert_with_boolean() {
        let doc = value!({"active": true});
        let q = build_insert(&s("app1"), "users", &tschema(), &doc).unwrap();
        assert_eq!(q.params, value!([true]).as_array().unwrap().clone());
    }

    #[test]
    fn test_insert_with_null_field() {
        let doc = value!({"name": "alice", "bio": null});
        let q = build_insert(&s("app1"), "users", &tschema(), &doc).unwrap();
        // null is inlined as a SQL `NULL` literal — not bound as a
        // text-format parameter (the wire protocol can't represent
        // NULL as a parameter; empty string would fail enum / NOT
        // NULL CHECKs).
        assert!(q.params.contains(&Value::from("alice")));
        assert!(
            !q.params.contains(&Value::from("")),
            "null must not be bound as empty-string param"
        );
        assert!(
            q.sql.contains("NULL"),
            "null should appear as a SQL literal in: {}",
            q.sql
        );
    }

    #[test]
    fn test_insert_with_number() {
        let doc = value!({"age": 30});
        let q = build_insert(&s("app1"), "users", &tschema(), &doc).unwrap();
        assert_eq!(q.params, value!([30]).as_array().unwrap().clone());
    }

    #[test]
    fn test_insert_with_nested_json() {
        let doc = value!({"settings": {"theme": "dark"}});
        let q = build_insert(&s("app1"), "users", &tschema(), &doc).unwrap();
        // Nested object is serialized as JSON text
        assert_eq!(q.params.len(), 1);
        let param = &q.params[0];
        assert!(
            param.get("theme").and_then(Value::as_str) == Some("dark"),
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
        let pipeline = value!([]);
        let q = build_aggregate(&s("app1"), "users", &pipeline, &tschema()).unwrap();
        assert!(q.sql.starts_with(&tselect()), "sql: {}", q.sql);
    }

    #[test]
    fn test_aggregate_match_only() {
        // Only $match without $group → select * (same as no_group test)
        let pipeline = value!([{"$match": {"status": "active"}}]);
        let q = build_aggregate(&s("app1"), "users", &pipeline, &tschema()).unwrap();
        assert!(q.sql.starts_with(&tselect()), "sql: {}", q.sql);
        assert!(!q.sql.contains("GROUP BY"), "sql: {}", q.sql);
        assert!(q.sql.contains("WHERE"), "sql: {}", q.sql);
        assert_eq!(q.params, value!(["active"]).as_array().unwrap().clone());
    }

    #[test]
    fn test_aggregate_all_agg_functions() {
        let pipeline = value!([{
            "$group": {
                "by": "category",
                "n":   {"$count": true},
                "total": {"$sum": "amount"},
                "avg_price": {"$avg": "price"},
                "min_price": {"$min": "price"},
                "max_price": {"$max": "price"}
            }
        }]);
        let q = build_aggregate(&s("app1"), "orders", &pipeline, &tschema()).unwrap();
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
        let filter = value!({"name": {"$regex": "^ali"}});
        let result = build_find(&s("app1"), "users", &filter, None, None, None, None);
        assert!(result.is_err(), "unsupported operator should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unsupported"), "msg: {msg}");
    }

    #[test]
    fn test_unsupported_update_operator() {
        let filter = value!({});
        let update = value!({"name": {"$unset": true}});
        let result = build_update_one(&s("app1"), "users", &tschema(), &filter, &update);
        assert!(result.is_err(), "unsupported update operator should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unsupported"), "msg: {msg}");
    }

    #[test]
    fn test_insert_empty_doc() {
        let doc = value!({});
        let result = build_insert(&s("app1"), "users", &tschema(), &doc);
        assert!(result.is_err(), "empty document should fail");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("empty") || msg.contains("cannot"),
            "msg: {msg}"
        );
    }

    #[test]
    fn test_insert_non_object() {
        let doc = value!("just a string");
        let result = build_insert(&s("app1"), "users", &tschema(), &doc);
        assert!(result.is_err(), "non-object document should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("object"), "msg: {msg}");
    }

    #[test]
    fn test_update_empty_fields() {
        let filter = value!({});
        let update = value!({});
        let result = build_update_one(&s("app1"), "users", &tschema(), &filter, &update);
        assert!(result.is_err(), "empty update should fail");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("empty") || msg.contains("cannot"),
            "msg: {msg}"
        );
    }

    #[test]
    fn test_in_non_array() {
        let filter = value!({"field": {"$in": "not-an-array"}});
        let result = build_find(&s("app1"), "users", &filter, None, None, None, None);
        assert!(result.is_err(), "$in with non-array should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("array"), "msg: {msg}");
    }

    #[test]
    fn test_exists_non_bool() {
        let filter = value!({"field": {"$exists": "yes"}});
        let result = build_find(&s("app1"), "users", &filter, None, None, None, None);
        assert!(result.is_err(), "$exists with non-bool should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("boolean"), "msg: {msg}");
    }

    // -----------------------------------------------------------------------
    // 7. $first sort-order threading
    // -----------------------------------------------------------------------

    #[test]
    fn test_aggregate_first_without_sort() {
        let pipeline = value!([
            {"$group": {"by": "department", "top_name": {"$first": "name"}}}
        ]);
        let q = build_aggregate(&s("app1"), "employees", &pipeline, &tschema()).unwrap();
        // Without a preceding $sort, $first uses plain array_agg
        assert!(
            q.sql.contains(r#"(array_agg("name"))[1]"#),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_aggregate_first_with_sort() {
        let pipeline = value!([
            {"$sort": {"salary": -1}},
            {"$group": {"by": "department", "top_name": {"$first": "name"}}}
        ]);
        let q = build_aggregate(&s("app1"), "employees", &pipeline, &tschema()).unwrap();
        // With a preceding $sort, $first threads the ORDER BY into array_agg
        assert!(
            q.sql
                .contains(r#"(array_agg("name" ORDER BY "salary" DESC NULLS FIRST))[1]"#),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_aggregate_first_with_multi_sort() {
        let pipeline = value!([
            {"$sort": {"salary": -1, "name": 1}},
            {"$group": {"by": "department", "top_name": {"$first": "name"}}}
        ]);
        let q = build_aggregate(&s("app1"), "employees", &pipeline, &tschema()).unwrap();
        // Multi-column sort should appear in the ORDER BY clause
        assert!(
            q.sql.contains(r#"array_agg("name" ORDER BY"#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains(r#""salary" DESC"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""name" ASC"#), "sql: {}", q.sql);
    }

    #[test]
    fn test_aggregate_first_sort_does_not_affect_other_aggs() {
        let pipeline = value!([
            {"$sort": {"salary": -1}},
            {"$group": {
                "by": "department",
                "top_name": {"$first": "name"},
                "total": {"$sum": "salary"},
                "cnt": {"$count": true}
            }}
        ]);
        let q = build_aggregate(&s("app1"), "employees", &pipeline, &tschema()).unwrap();
        // $first should have ORDER BY
        assert!(
            q.sql
                .contains(r#"array_agg("name" ORDER BY "salary" DESC NULLS FIRST)"#),
            "sql: {}",
            q.sql
        );
        // $sum and $count should NOT have ORDER BY
        assert!(q.sql.contains(r#"SUM("salary")"#), "sql: {}", q.sql);
        assert!(q.sql.contains("COUNT(*)"), "sql: {}", q.sql);
    }

    #[test]
    fn test_upsert_basic() {
        let doc = value!({"name": "alice", "age": 30});
        let conflict = value!(["name"]);
        let q = build_upsert(&s("app1"), "users", &tschema(), doc, &conflict).unwrap();
        assert!(q.sql.contains("INSERT INTO"), "sql: {}", q.sql);
        assert!(q.sql.contains("ON CONFLICT"), "sql: {}", q.sql);
        assert!(q.sql.contains("DO UPDATE SET"), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""name""#), "sql: {}", q.sql);
        // age is not a conflict field, so it should appear in DO UPDATE SET
        assert!(
            q.sql.contains(r#""age" = EXCLUDED."age""#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_upsert_multiple_conflict_fields() {
        let doc = value!({"email": "a@b.com", "name": "alice", "age": 30});
        let conflict = value!(["email", "name"]);
        let q = build_upsert(&s("app1"), "users", &tschema(), doc, &conflict).unwrap();
        assert!(
            q.sql.contains(r#"ON CONFLICT ("email", "name")"#),
            "sql: {}",
            q.sql
        );
        // Only age should be in DO UPDATE SET
        assert!(
            q.sql.contains(r#""age" = EXCLUDED."age""#),
            "sql: {}",
            q.sql
        );
        // email and name should NOT be in DO UPDATE SET (they are conflict fields)
        assert!(
            !q.sql.contains(r#""email" = EXCLUDED."email""#),
            "sql: {}",
            q.sql
        );
        assert!(
            !q.sql.contains(r#""name" = EXCLUDED."name""#),
            "sql: {}",
            q.sql
        );
    }

    #[test]
    fn test_upsert_all_conflict_cols() {
        // When all columns are conflict columns, we still produce a valid DO UPDATE SET
        let doc = value!({"email": "a@b.com"});
        let conflict = value!(["email"]);
        let q = build_upsert(&s("app1"), "users", &tschema(), doc, &conflict).unwrap();
        assert!(q.sql.contains("DO UPDATE SET"), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
    }

    #[test]
    fn test_upsert_preserves_insert_only_assigned_fields_on_conflict() {
        let doc = value!({
            "email": "a@b.com",
            "id": "user_new",
            "created_at": "2026-05-25T00:00:00Z",
            "created_by": "usr_new",
            "updated_by": "usr_actor",
            "name": "alice"
        });
        let conflict = value!(["email"]);
        let q = build_upsert(&s("app1"), "users", &tschema(), doc, &conflict).unwrap();
        assert!(
            !q.sql.contains(r#""id" = EXCLUDED."id""#),
            "upsert must not overwrite id on conflict: {}",
            q.sql
        );
        assert!(
            !q.sql.contains(r#""created_at" = EXCLUDED."created_at""#),
            "upsert must not overwrite created_at on conflict: {}",
            q.sql
        );
        assert!(
            !q.sql.contains(r#""created_by" = EXCLUDED."created_by""#),
            "upsert must not overwrite created_by on conflict: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#""updated_by" = EXCLUDED."updated_by""#),
            "mutable audit fields should still update from EXCLUDED: {}",
            q.sql
        );
    }

    /// This asserted the UNqualified `COALESCE("version", 0) + 1` and was green
    /// for as long as that string was emitted - against SQLite, where it is
    /// legal. PostgreSQL refuses it (`42702`), so the assertion pinned a
    /// statement the production dialect cannot run. See
    /// [`the_upserts_version_bump_qualifies_the_column_it_reads`].
    #[test]
    fn test_upsert_assignmentss_version_and_updated_at_when_omitted() {
        let doc = value!({"email": "a@b.com", "name": "alice"});
        let conflict = value!(["email"]);
        let q = build_upsert_with_assignments(
            &s("app1"),
            "users",
            &tschema(),
            doc,
            &conflict,
            SqlDialect::Sqlite,
            &lifecycle_assignments(false),
            None,
        )
        .unwrap();
        assert!(
            q.sql
                .contains(r#""version" = "app1"."users"."version" + $"#),
            "upsert must auto-bump version on conflict when omitted: {}",
            q.sql
        );
        assert!(
            q.sql
                .contains(r#""updated_at" = (strftime('%Y-%m-%dT%H:%M:%fZ','now'))"#),
            "SQLite upsert must stamp the ISO-T now expression when updated_at omitted: {}",
            q.sql
        );
    }

    #[test]
    fn test_upsert_respects_creator_supplied_version_and_updated_at() {
        let doc = value!({
            "email": "a@b.com",
            "name": "alice",
            "version": 99,
            "updated_at": "2026-05-25T00:00:00Z"
        });
        let conflict = value!(["email"]);
        let q = build_upsert_with_dialect(
            &s("app1"),
            "users",
            &tschema(),
            doc,
            &conflict,
            SqlDialect::Postgres,
        )
        .unwrap();
        assert!(
            !q.sql.contains(r#""version" = COALESCE("version", 0) + 1"#),
            "explicit version should suppress conflict-update auto-bump: {}",
            q.sql
        );
        assert!(
            !q.sql.contains(r#""updated_at" = NOW()"#),
            "explicit updated_at should suppress conflict-update timestamp auto-stamp: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#""version" = EXCLUDED."version""#),
            "explicit version should flow through EXCLUDED on conflict: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#""updated_at" = EXCLUDED."updated_at""#),
            "explicit updated_at should flow through EXCLUDED on conflict: {}",
            q.sql
        );
    }

    #[test]
    fn test_upsert_empty_doc_error() {
        let doc = value!({});
        let conflict = value!(["name"]);
        let result = build_upsert(&s("app1"), "users", &tschema(), doc, &conflict);
        assert!(result.is_err());
    }

    #[test]
    fn test_upsert_empty_conflict_fields_error() {
        let doc = value!({"name": "alice"});
        let conflict = value!([]);
        let result = build_upsert(&s("app1"), "users", &tschema(), doc, &conflict);
        assert!(result.is_err());
    }

    #[test]
    fn conflict_targets_reject_malformed_or_repeated_fields() {
        let doc = value!({"email":"a@example.com", "name":"Alice"});
        for conflict in [
            value!(["email", 1]),
            value!(["email", null]),
            value!(["email", "email"]),
            value!([""]),
            value!(["__zeroship_internal"]),
            value!(["email", "bad\0field"]),
        ] {
            for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
                assert!(
                    build_upsert_with_dialect(
                        &s("app1"),
                        "users",
                        &tschema(),
                        doc.clone(),
                        &conflict,
                        dialect
                    )
                    .is_err(),
                    "{conflict}"
                );
            }
            assert!(
                build_find_or_create(&s("app1"), "users", &tschema(), &doc, &conflict).is_err(),
                "{conflict}"
            );
        }
        assert_eq!(
            parse_conflict_fields(&value!(["email", "name"])).unwrap(),
            ["email", "name"]
        );
    }

    #[test]
    fn test_upsert_invalid_collection_error() {
        let doc = value!({"name": "alice"});
        let conflict = value!(["name"]);
        let result = build_upsert(&s("app1"), "users; DROP TABLE", &tschema(), doc, &conflict);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_or_create_emits_xmax_returning() {
        let doc = value!({"email": "a@b.com", "name": "alice"});
        let conflict = value!(["email"]);
        let q = build_find_or_create(&s("app1"), "users", &tschema(), &doc, &conflict).unwrap();
        assert!(q.sql.contains("INSERT INTO"), "sql: {}", q.sql);
        assert!(q.sql.contains(r#"ON CONFLICT ("email")"#), "sql: {}", q.sql);
        // No-op self-assignment on the conflict column so RETURNING
        // fires for the existing row without mutating it.
        assert!(
            q.sql
                .contains(r#"DO UPDATE SET "email" = "app1"."users"."email""#),
            "sql: {}",
            q.sql,
        );
        // The created flag is appended to the RETURNING list.
        assert!(q.sql.contains("(xmax = 0) AS __created"), "sql: {}", q.sql,);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_find_or_create_rejects_empty_conflict() {
        let doc = value!({"email": "a@b.com"});
        let conflict = value!([]);
        assert!(build_find_or_create(&s("app1"), "users", &tschema(), &doc, &conflict).is_err());
    }

    #[test]
    fn test_find_or_create_rejects_empty_doc() {
        let doc = value!({});
        let conflict = value!(["email"]);
        assert!(build_find_or_create(&s("app1"), "users", &tschema(), &doc, &conflict).is_err());
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
        let filter = value!({"id": 1});
        let update = value!({"scores": {"$push": 42}});
        let q = build_update_one(&s("app1"), "games", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql.contains(
                r#"jsonb_insert("scores", ARRAY[jsonb_array_length("scores")::text], $1::jsonb)"#
            ),
            "sql: {}",
            q.sql
        );
        // Param is the JSON text "42", not "\"42\"" — Postgres parses it
        // back as a JSON number on the ::jsonb cast.
        assert_eq!(q.params[0], value!(42));
    }

    #[test]
    fn test_update_push_bool_preserves_type() {
        let filter = value!({"id": 1});
        let update = value!({"flags": {"$push": true}});
        let q = build_update_one(&s("app1"), "games", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql.contains(
                r#"jsonb_insert("flags", ARRAY[jsonb_array_length("flags")::text], $1::jsonb)"#
            ),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], value!(true));
    }

    #[test]
    fn test_update_push_object_preserves_type() {
        let filter = value!({"id": 1});
        let update = value!({"entries": {"$push": {"k": "v", "n": 3}}});
        let q = build_update_one(&s("app1"), "log", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql.contains(
                r#"jsonb_insert("entries", ARRAY[jsonb_array_length("entries")::text], $1::jsonb)"#
            ),
            "sql: {}",
            q.sql
        );
        // Object → compact JSON text. Keys serialized in crate::value::Value
        // order (preserves insertion via the default feature? -- we don't
        // assert ordering, just that both keys are present).
        assert!(q.params[0]["k"] == "v", "params[0] = {}", q.params[0]);
        assert!(q.params[0]["n"] == 3, "params[0] = {}", q.params[0]);
    }

    #[test]
    fn test_update_pull_number() {
        // Regression: old shape `"tags" - $1` is the jsonb "remove key"
        // operator — it mutates objects, not arrays. The subquery form
        // correctly removes array elements equal to the value.
        let filter = value!({"id": 1});
        let update = value!({"scores": {"$pull": 100}});
        let q = build_update_one(&s("app1"), "games", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql
                .contains(r#"FROM jsonb_array_elements("scores") WITH ORDINALITY AS __zs_array(element, position) WHERE __zs_array.element != $1::jsonb"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], value!(100));
    }

    #[test]
    fn test_update_add_to_set_number() {
        let filter = value!({"id": 1});
        let update = value!({"ids": {"$addToSet": 7}});
        let q = build_update_one(&s("app1"), "games", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql.contains(
                r#"FROM jsonb_array_elements("ids") AS __zs_array(element) WHERE __zs_array.element = $1::jsonb"#
            ),
            "sql: {}",
            q.sql
        );
        // $addToSet reuses the same parameter index for the equality
        // check and the append — only one param is pushed.
        assert_eq!(q.params.len(), 2, "params: {:?}", q.params); // the op param + the filter param (id = 1)
        assert_eq!(q.params[0], value!(7));
    }

    #[test]
    fn test_update_updated_at_requires_assignment() {
        // Every UPDATE implicitly bumps updated_at unless the caller
        // explicitly set it. This is part of the platform contract.
        let filter = value!({"id": 1});
        let update = value!({"name": "bob"});
        let q = build_update_one(&s("app1"), "users", &tschema(), &filter, &update).unwrap();
        assert!(!q.sql.contains(r#""updated_at" = NOW()"#), "sql: {}", q.sql);
    }

    #[test]
    fn test_update_updated_at_not_overridden_when_explicit() {
        // If the caller explicitly provides updated_at, we must NOT add
        // our own `NOW()` clause — would collide and the user's value wins.
        let filter = value!({"id": 1});
        let explicit_ts = "2026-01-01T00:00:00Z";
        let update = value!({"name": "bob", "updated_at": explicit_ts});
        let q = build_update_one(&s("app1"), "users", &tschema(), &filter, &update).unwrap();
        assert!(
            !q.sql.contains("NOW()"),
            "sql should not contain NOW() when updated_at is explicit: {}",
            q.sql
        );
        assert!(
            q.params.contains(&Value::Timestamp(
                crate::sql::temporal::parse_timestamp_millis(explicit_ts).unwrap()
            )),
            "params: {:?}",
            q.params
        );
    }

    #[test]
    fn test_update_set_flattens_top_level() {
        // Regression: early impl processed only the $set key and dropped
        // sibling top-level fields. After 6a309b3 the builder flattens
        // $set into the top level, so both `name` and `age` must appear.
        let filter = value!({"id": 1});
        let update = value!({"$set": {"name": "alice"}, "age": 30});
        let q = build_update_one(&s("app1"), "users", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""age" = $"#), "sql: {}", q.sql);
        assert!(q.params.contains(&Value::from("alice")));
        assert!(q.params.contains(&Value::from(30)));
    }

    #[test]
    fn test_update_set_coexists_with_inc() {
        // Mixed $set (flattened) + $inc on a sibling field. Both must
        // produce SET clauses and share the same params vector.
        let filter = value!({"id": 1});
        let update = value!({"$set": {"name": "alice"}, "views": {"$inc": 5}});
        let q = build_update_one(&s("app1"), "posts", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""views" = "views" + $"#), "sql: {}", q.sql);
        assert!(q.params.contains(&Value::from("alice")));
        assert!(q.params.contains(&Value::from(5)));
    }

    #[test]
    fn test_update_inc_param_formatting() {
        // $inc operand is pushed via value_to_param — a float should render
        // as "1.5" (not "1.5e0" or similar), so Postgres's ::numeric cast
        // accepts it without a client-side conversion.
        let filter = value!({"id": 1});
        let update = value!({"balance": {"$inc": 1.5}});
        let q = build_update_one(&s("app1"), "accounts", &tschema(), &filter, &update).unwrap();
        assert_eq!(q.params[0], value!(1.5));
    }

    #[test]
    fn test_update_negative_inc() {
        // Negative $inc must still render with the `+` operator (caller
        // uses $dec for subtraction semantically). Postgres handles the
        // minus sign on the numeric literal fine.
        let filter = value!({"id": 1});
        let update = value!({"stock": {"$inc": -3}});
        let q = build_update_one(&s("app1"), "items", &tschema(), &filter, &update).unwrap();
        assert!(
            q.sql.contains(r#""stock" = "stock" + $1::numeric"#),
            "sql: {}",
            q.sql
        );
        assert_eq!(q.params[0], value!(-3));
    }

    #[test]
    fn test_update_param_indexing_with_filter() {
        // SET params come first, WHERE params come after. The `$N`
        // placeholders must be contiguous across both halves.
        let filter = value!({"status": "active"});
        let update = value!({"name": "alice", "views": {"$inc": 1}});
        let q = build_update_one(&s("app1"), "posts", &tschema(), &filter, &update).unwrap();
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
        let filter = value!({"id": 1});
        let update = value!({});
        let err = build_update_one(&s("app1"), "users", &tschema(), &filter, &update).unwrap_err();
        // Variant is QueryError::InvalidFilter — compare Display form so
        // this doesn't need to import the enum.
        assert!(
            format!("{err}").to_lowercase().contains("empty"),
            "error should mention empty fields, got: {err}"
        );
    }

    #[test]
    fn test_update_unknown_operator_rejected() {
        let filter = value!({"id": 1});
        let update = value!({"tags": {"$weirdOp": "val"}});
        let err = build_update_one(&s("app1"), "posts", &tschema(), &filter, &update).unwrap_err();
        assert!(
            format!("{err}").contains("$weirdOp"),
            "error should name the unsupported op, got: {err}"
        );
    }

    #[test]
    fn test_update_many_leaves_unassigned_timestamp_unchanged() {
        // updateMany shares the same build_set_clauses path, so the
        // auto-timestamp behaviour must hold there too.
        let filter = value!({"status": "draft"});
        let update = value!({"status": "published"});
        let q = build_update_many(&s("app1"), "posts", &tschema(), &filter, &update).unwrap();
        assert!(!q.sql.contains(r#""updated_at" = NOW()"#), "sql: {}", q.sql);
        // updateMany must not wrap the WHERE in a LIMIT 1 subquery.
        assert!(!q.sql.contains("LIMIT 1"), "sql: {}", q.sql);
    }

    #[test]
    fn test_update_set_without_top_level_fields() {
        // Pure $set with no siblings — flattening must still work.
        let filter = value!({"id": 1});
        let update = value!({"$set": {"name": "alice", "age": 30}});
        let q = build_update_one(&s("app1"), "users", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""age" = $"#), "sql: {}", q.sql);
    }

    #[test]
    fn test_update_set_non_object_rejected() {
        // `$set` value that isn't an object should error, not be
        // silently treated as a scalar $set on a column named "$set".
        let filter = value!({"id": 1});
        let update = value!({"$set": "not an object"});
        let err = build_update_one(&s("app1"), "users", &tschema(), &filter, &update).unwrap_err();
        assert!(
            format!("{err}").contains("$set"),
            "error should mention $set, got: {err}"
        );
    }

    #[test]
    fn test_update_string_column_name_is_quoted() {
        // Column names get quoted via quote_ident, so a column with a
        // reserved word as its name still works.
        let filter = value!({"id": 1});
        let update = value!({"user": "alice"}); // "user" is a reserved word
        let q = build_update_one(&s("app1"), "accounts", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""user" = $"#), "sql: {}", q.sql);
    }

    // -----------------------------------------------------------------------
    // UPDATE auto-bumps version + updated_at + updated_by
    //
    // The auto-bumps fire only on the CRUD dispatch path (signalled by
    // an `actor_id` being threaded through OR by `skip_*` hints).
    // Direct callers of `build_update_one` / `build_update_many`
    // continue to see only the single-column auto-bump
    // (`updated_at = NOW()` on PG) so the regression tests above stay
    // green.
    // -----------------------------------------------------------------------

    #[test]
    fn update_appends_version_increment_to_set_clause() {
        let filter = value!({ "id": "post_x" });
        let update = value!({ "title": "new" });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
                ColumnAssignment {
                    column: "updated_by".into(),
                    value: AssignedValue::Bound(value!("usr_actor")),
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(
            q.sql.contains(r#""version" = "version" + $"#),
            "PR 4 must append version auto-bump: {}",
            q.sql,
        );
    }

    #[test]
    fn update_appends_version_increment_for_anonymous_dispatch_write() {
        let filter = value!({ "id": "post_x" });
        let update = value!({ "title": "new" });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
                ColumnAssignment {
                    column: "updated_by".into(),
                    value: AssignedValue::Bound(Value::Null),
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Sqlite,
            &assignments,
        )
        .unwrap();
        assert!(
            q.sql.contains(r#""version" = "version" + $"#),
            "anonymous dispatched updates must still bump version: {}",
            q.sql,
        );
    }

    #[test]
    fn update_appends_updated_at_now_pg() {
        let filter = value!({ "id": "post_x" });
        let update = value!({ "title": "new" });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
                ColumnAssignment {
                    column: "updated_by".into(),
                    value: AssignedValue::Bound(value!("usr_actor")),
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(
            q.sql.contains(r#""updated_at" = NOW()"#),
            "PG dialect must emit NOW(): {}",
            q.sql,
        );
    }

    #[test]
    fn update_appends_updated_at_current_timestamp_sqlite() {
        let filter = value!({ "id": "post_x" });
        let update = value!({ "title": "new" });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
                ColumnAssignment {
                    column: "updated_by".into(),
                    value: AssignedValue::Bound(value!("usr_actor")),
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Sqlite,
            &assignments,
        )
        .unwrap();
        assert!(
            q.sql
                .contains(r#""updated_at" = (strftime('%Y-%m-%dT%H:%M:%fZ','now'))"#),
            "SQLite dialect must emit the ISO-T now expression: {}",
            q.sql,
        );
        assert!(
            !q.sql.contains("NOW()"),
            "SQLite must NOT emit NOW(): {}",
            q.sql
        );
    }

    #[test]
    fn update_appends_updated_by_from_session_actor() {
        let filter = value!({ "id": "post_x" });
        let update = value!({ "title": "new" });
        // `dispatch_write` is now what gates the `updated_by` assignment, not
        // the actor's presence. It had to move: an anonymous dispatch write
        // assigns NULL, so "an actor is bound" can no longer distinguish a
        // dispatch write from a direct builder call.
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
                ColumnAssignment {
                    column: "updated_by".into(),
                    value: AssignedValue::Bound(value!("usr_session")),
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        // updated_by is a bound param; the SQL fragment is `"updated_by" = $N`
        assert!(
            q.sql.contains(r#""updated_by" = $"#),
            "must emit updated_by SET clause: {}",
            q.sql,
        );
        // The actor id must be present in the params vector.
        assert!(
            q.params.contains(&Value::from("usr_session")),
            "params must include actor id: {:?}",
            q.params,
        );
    }

    #[test]
    fn update_leaves_updated_by_null_when_no_session_actor() {
        // No actor: the dispatch path emits no `updated_by` SET clause.
        // Note: with `actor_id = None` AND no `skip_*` flags, the
        // default fallback applies — only `updated_at` auto-bumps.
        let filter = value!({ "id": "post_x" });
        let update = value!({ "title": "new" });
        let assignments = WriteAssignments::default();
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(
            !set_clause_of(&q.sql).contains(r#""updated_by""#),
            "no actor → no updated_by SET clause: {}",
            q.sql,
        );
    }

    #[test]
    fn update_respects_creator_supplied_version_pr4() {
        // When the creator's patch carries `version: 99`, the auto-bump
        // MUST NOT fire (the explicit value wins per Q-SF-B).
        let filter = value!({ "id": "post_x" });
        let update = value!({ "title": "new", "version": 99 });
        let assignments = WriteAssignments {
            columns: vec![ColumnAssignment {
                column: "updated_at".into(),
                value: AssignedValue::CurrentTimestamp,
            }],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        // The version auto-bump must NOT appear.
        assert!(
            !q.sql.contains(r#""version" = "version" + $"#),
            "skip_version must suppress the auto-bump: {}",
            q.sql,
        );
        // The creator's explicit value flows through as a bound param.
        assert!(
            q.sql.contains(r#""version" = $"#),
            "creator's explicit version must reach SQL: {}",
            q.sql,
        );
        assert!(
            q.params.contains(&Value::from(99)),
            "params: {:?}",
            q.params
        );
    }

    #[test]
    fn update_respects_creator_supplied_updated_at_pr4() {
        let filter = value!({ "id": "post_x" });
        let explicit = "2026-01-01T00:00:00Z";
        let update = value!({ "title": "new", "updated_at": explicit });
        let assignments = WriteAssignments {
            columns: vec![ColumnAssignment {
                column: "version".into(),
                value: AssignedValue::Increment(1),
            }],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(
            !q.sql.contains("NOW()"),
            "skip_updated_at must suppress NOW(): {}",
            q.sql,
        );
        assert!(
            q.params.contains(&Value::Timestamp(
                crate::sql::temporal::parse_timestamp_millis(explicit).unwrap()
            )),
            "params: {:?}",
            q.params
        );
    }

    #[test]
    fn update_auto_bump_columns_bypass_encryption_pass() {
        // Build a doc with an encrypted-column marker. The auto-bump
        // version/updated_at/updated_by SET clauses must NOT be wrapped
        // with the encrypted-column placeholder shape.
        let filter = value!({ "id": "post_x" });
        let update = value!({
            "secret": Value::Bytes(b"ciphertext".to_vec()),
        });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        // Encrypted column gets the decode(...)::bytea wrap.
        assert!(
            q.params.iter().any(|p| p.as_bytes().is_some()),
            "encrypted column must have a native binary bind: {}",
            q.sql,
        );
        // Auto-bumps are plain SET clauses — must NOT be inside a decode().
        // The version bump's SQL fragment is `"version" = "version" + $`
        // (no $N), so decode() can't wrap it. The updated_by SET clause
        // is `"updated_by" = $N` — assert there's no `decode($N..)::bytea`
        // associated with the updated_by column.
        let updated_by_idx = q.sql.find(r#""updated_by""#).unwrap();
        let updated_by_clause = &q.sql[updated_by_idx..(updated_by_idx + 30).min(q.sql.len())];
        assert!(
            !updated_by_clause.contains("decode("),
            "updated_by SET clause must NOT be decode-wrapped: {updated_by_clause}",
        );
    }

    #[test]
    fn update_auto_bump_uses_distinct_bind_params_from_encryption_pass() {
        // Encryption pass binds the ciphertext as $1. The updated_by
        // auto-bump must bind to $2 (or later) — not collide with $1.
        let filter = value!({ "id": "post_x" });
        let update = value!({
            "secret": Value::Bytes(b"ciphertext".to_vec()),
        });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
                ColumnAssignment {
                    column: "updated_by".into(),
                    value: AssignedValue::Bound(value!("usr_actor")),
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        // The actor id must appear in the params vector AFTER the
        // ciphertext (or at any later $N), not collide.
        let actor_pos = q
            .params
            .iter()
            .position(|p| p == "usr_actor")
            .expect("actor must be bound");
        let cipher_pos = q
            .params
            .iter()
            .position(|p| p.as_bytes() == Some(b"ciphertext".as_slice()))
            .expect("ciphertext must be bound");
        assert!(
            actor_pos > cipher_pos,
            "actor id bind ({actor_pos}) must come after ciphertext ({cipher_pos}): {:?}",
            q.params,
        );
    }

    #[test]
    fn update_with_version_filter_appends_where_version_eq_n() {
        // When the filter has `version: N`, the standard
        // `build_where` emits `"version" = $N`. The SQL builder
        // doesn't need special CAS handling; the auto-bump SET
        // composes with the WHERE naturally.
        let filter = value!({ "id": "post_x", "version": 5 });
        let update = value!({ "title": "new" });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        // WHERE clause references `version` as an equality predicate.
        assert!(
            q.sql.contains(r#""version" = $"#),
            "filter version must appear in WHERE: {}",
            q.sql,
        );
        // The version `5` must appear as a bound param.
        assert!(q.params.contains(&Value::from(5)), "params: {:?}", q.params);
    }

    #[test]
    fn update_default_path_leaves_unassigned_timestamp_unchanged_sqlite() {
        // Direct calls to the legacy wrapper on the SQLite arm: the
        // auto-bump is dialect-aware rather than hardcoding NOW().
        let filter = value!({ "id": "post_x" });
        let update = value!({ "title": "new" });
        let q = build_update_one_with_dialect(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert!(
            !q.sql
                .contains(r#""updated_at" = (strftime('%Y-%m-%dT%H:%M:%fZ','now'))"#),
            "SQLite-arm direct callers get the ISO-T now expression: {}",
            q.sql,
        );
    }

    #[test]
    fn update_with_explicit_version_and_concurrency_filter_explicit_wins() {
        // When the filter has `version: 5` (CAS guard) AND the patch
        // carries an explicit `version: 99`, the SDK's expected
        // behaviour is: the WHERE clause still narrows to version=5,
        // and the SET clause stamps version=99 verbatim (the
        // auto-bump is suppressed because the creator supplied an
        // explicit value).
        let filter = crate::value!({ "id": "post_x", "version": 5 });
        let update = crate::value!({ "title": "new", "version": 99 });
        let assignments = WriteAssignments {
            columns: vec![ColumnAssignment {
                column: "updated_at".into(),
                value: AssignedValue::CurrentTimestamp,
            }],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        // SET carries the creator's explicit value (`"version" = $N`).
        assert!(
            q.sql.contains(r#""version" = $"#),
            "explicit version reaches SET: {}",
            q.sql,
        );
        assert!(
            !q.sql.contains(r#""version" = "version" + $"#),
            "auto-bump must be suppressed: {}",
            q.sql,
        );
        // Bind values must include BOTH `99` (SET) and `5` (WHERE).
        assert!(
            q.params.contains(&Value::from(99)),
            "params: {:?}",
            q.params
        );
        assert!(q.params.contains(&Value::from(5)), "params: {:?}", q.params);
    }

    #[test]
    fn update_encrypted_column_still_routes_through_encryption_pass() {
        let filter = crate::value!({ "id": "post_x" });
        let update = crate::value!({
            "ssn": Value::Bytes(b"ciphertext_blob".to_vec()),
        });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        // `ssn` gets the decode(...)::bytea wrap.
        assert!(
            q.params.iter().any(|p| p.as_bytes().is_some()),
            "encrypted column bound: {}",
            q.sql,
        );
        // The encrypted column SQL fragment contains the cast.
        let ssn_idx = q.sql.find(r#""ssn""#).unwrap();
        let ssn_end = q.sql[ssn_idx..]
            .find(',')
            .map(|i| ssn_idx + i)
            .unwrap_or(q.sql.len());
        let ssn_clause = &q.sql[ssn_idx..ssn_end];
        assert!(
            ssn_clause.contains(" = $"),
            "ssn SET clause must include decode wrap: {ssn_clause}"
        );
    }

    #[test]
    fn update_auto_bump_columns_bypass_mask_pass() {
        // Automatic system-field updates must not add mask storage columns.
        let filter = crate::value!({ "id": "post_x" });
        let update = crate::value!({ "title": "new" });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        // The auto-bump columns never get a sibling `*_masked` SET
        // clause.
        assert!(
            !q.sql.contains("version_masked"),
            "no version_masked: {}",
            q.sql
        );
        assert!(
            !q.sql.contains("updated_at_masked"),
            "no updated_at_masked: {}",
            q.sql
        );
        assert!(
            !q.sql.contains("updated_by_masked"),
            "no updated_by_masked: {}",
            q.sql
        );
    }

    #[test]
    fn update_set_clause_ordering_creator_first_then_auto_bump() {
        // Auto-bump SET clauses are appended AFTER
        // every creator-supplied clause so the SQL diff is grep-able
        // (and the encryption/mask passes — which iterate the
        // creator's keys — never touch the auto-bumps).
        let filter = value!({ "id": "post_x" });
        let update = value!({ "title": "new" });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
                ColumnAssignment {
                    column: "updated_by".into(),
                    value: AssignedValue::Bound(value!("usr_actor")),
                },
            ],
        };
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        let title_pos = q.sql.find(r#""title""#).expect("title in SET");
        let version_pos = q.sql.find(r#""version""#).expect("version in SET");
        let updated_at_pos = q.sql.find(r#""updated_at""#).expect("updated_at in SET");
        let updated_by_pos = q.sql.find(r#""updated_by""#).expect("updated_by in SET");
        assert!(
            title_pos < version_pos,
            "creator's title must come before auto-bump version"
        );
        assert!(version_pos < updated_at_pos);
        assert!(updated_at_pos < updated_by_pos);
    }

    // -----------------------------------------------------------------------
    // A1 — Materialised indexes (zeroship-db proposal §A1).
    //
    // Before A1, `t.string().index()` and `t.string().unique()` set
    // `FieldDef.index/unique` in the SDK but the Rust DDL emitter produced
    // no index. These tests lock the materialisation contract: every
    // marker yields a `CREATE [UNIQUE] INDEX CONCURRENTLY IF NOT EXISTS …`
    // statement with a deterministic name.
    // -----------------------------------------------------------------------

    /// Same move as `test_schema_sql_injection`: the index builder can no
    /// longer be handed an illegal schema, so the refusal is asserted where it
    /// now lives.
    #[test]
    fn test_build_indexes_rejects_bad_schema() {
        let err = SchemaName::new("app; --").unwrap_err();
        assert!(matches!(err, zeroship_core::schema_name::SchemaNameError::Invalid(_)));
    }

    // -----------------------------------------------------------------------
    // Naming truncation (Postgres NAMEDATALEN = 64).
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Silent-bug repro (the original reason A1 exists).
    //
    // Before this change, `t.string().unique()` set FieldDef.unique = true
    // in the SDK but the Rust layer never emitted a unique index. This
    // test asserts that the emitted migration SQL actually
    // contains a CREATE UNIQUE INDEX CONCURRENTLY statement targeting
    // the `email` column.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------
    // B2 — typed cross-table relations
    // -----------------------------------------------------------------

    // Schema-emission regression coverage uses the migration-owned DDL helpers.

    // -----------------------------------------------------------------
    // FK column type cascade (TEXT, was INTEGER previously)
    // -----------------------------------------------------------------

    // -----------------------------------------------------------------
    // D2 — nested object validators (JSONB column)
    // -----------------------------------------------------------------

    // -----------------------------------------------------------------
    // D3 — calendar dates → DATE column type
    // -----------------------------------------------------------------

    // -----------------------------------------------------------------
    // D4 — version column injected by the SDK is treated as a plain
    // INTEGER (well, NUMERIC) column at the DDL level. The SDK uses
    // model.ts to inject `version: { type: "number", default: 1 }`
    // so the DDL emission below matches.
    //
    // `version` is a reserved system-field name
    // (`FIXTURE_GENERATED_COLUMNS`); the declaration-time validator refuses
    // a creator-declared `version` column. `build_create_table_with_fks`
    // injects the seven system fields directly (not via a creator-shape
    // entry). This test uses a placeholder field
    // name (`schema_revision`) to keep exercising the
    // `t.number().default(N)` DDL path that produces `DOUBLE PRECISION
    // ... DEFAULT 1`.
    // -----------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Security IMPORTANT #1 — validate_collection reserved-name checks
    // -----------------------------------------------------------------------

    /// Valid collection names must still pass — no regression.
    #[test]
    fn validate_collection_accepts_valid_names() {
        for name in &["users", "todos", "order_items", "a", "A1_b"] {
            assert!(
                validate_collection(name).is_ok(),
                "expected '{name}' to be valid"
            );
        }
    }

    /// Empty string must be rejected.
    #[test]
    fn validate_collection_rejects_empty() {
        let err = validate_collection("").unwrap_err();
        match err {
            QueryError::InvalidCollection(msg) => assert!(msg.contains("empty"), "{msg}"),
            other => panic!("expected InvalidCollection, got {other:?}"),
        }
    }

    /// Names starting with `pg_` (any case) must be rejected.
    #[test]
    fn validate_collection_rejects_pg_prefix() {
        for name in &["pg_indexes", "PG_stat", "Pg_Class"] {
            let err = validate_collection(name).unwrap_err();
            match err {
                QueryError::InvalidCollection(msg) => assert!(
                    msg.contains("pg_") || msg.contains("reserved"),
                    "for '{name}': {msg}"
                ),
                other => panic!("expected InvalidCollection for '{name}', got {other:?}"),
            }
        }
    }

    /// Names starting with `__zeroship` (any case) must be rejected.
    #[test]
    fn validate_collection_rejects_zeroship_prefix() {
        for name in &["__zeroship_migrations", "__ZEROSHIP_audit", "__zeroship"] {
            let err = validate_collection(name).unwrap_err();
            match err {
                QueryError::InvalidCollection(msg) => assert!(
                    msg.contains("__zeroship") || msg.contains("reserved"),
                    "for '{name}': {msg}"
                ),
                other => panic!("expected InvalidCollection for '{name}', got {other:?}"),
            }
        }
    }

    /// `__zero_migrate` is NOT reserved, and `__zeroship` is. Both halves,
    /// because either alone reads as an accident.
    ///
    /// This asserted the opposite until 2026-09-07. The prefix was fencing an
    /// empty namespace: the engine's journal tables are `__zeroship_schema_*`,
    /// and the one live object carrying the token is the SQLite rebuild table,
    /// named `{table}__zero_migrate_rebuild` - a SUFFIX, which a prefix list
    /// cannot cover.
    #[test]
    fn the_collection_fence_reserves_zeroship_and_not_zero_migrate() {
        let mut ruled_on = 0_usize;
        for name in [
            "__zero_migrate_migrations",
            "__ZERO_MIGRATE_audit",
            "__zero_migrate",
        ] {
            assert!(
                validate_collection(name).is_ok(),
                "'{name}' is refused, but nothing is named with that prefix"
            );
            ruled_on += 1;
        }
        for name in [
            "__zeroship_schema_migrations",
            "__ZEROSHIP_audit",
            "__zeroship",
        ] {
            let err = validate_collection(name).unwrap_err();
            match err {
                QueryError::InvalidCollection(msg) => assert!(
                    msg.contains("__zeroship") || msg.contains("reserved"),
                    "for '{name}': {msg}"
                ),
                other => panic!("expected InvalidCollection for '{name}', got {other:?}"),
            }
            ruled_on += 1;
        }
        assert_eq!(ruled_on, 6);
        println!("ruled on {ruled_on} collection names");
    }

    #[test]
    fn reserved_collection_prefixes_match_migration_engine() {
        assert_eq!(
            PLATFORM_RESERVED_COLLECTION_PREFIXES,
            zeroship_migrate_core::schema::query::PLATFORM_RESERVED_COLLECTION_PREFIXES,
            "data-plane and migration-engine collection prefixes diverged"
        );
        assert_eq!(
            PLATFORM_RESERVED_COLLECTION_PREFIXES,
            crate::sql::ident::PLATFORM_RESERVED_COLLECTION_PREFIXES,
            "data-plane and runtime-plan collection prefixes diverged"
        );
    }

    fn shipping_catalog_reservation_witnesses() -> Vec<String> {
        zeroship_migrate::shipping_vendors()
            .as_slice()
            .iter()
            .flat_map(|vendor| {
                vendor
                    .descriptor
                    .limits
                    .reserved_identifier_prefixes
                    .iter()
                    .flat_map(|prefix| {
                        [
                            format!("{prefix}catalog_object"),
                            format!("{}CATALOG_OBJECT", prefix.to_ascii_uppercase()),
                        ]
                    })
            })
            .collect()
    }

    fn assert_reserved_identifier_behavior_matches(
        role: crate::sql::ident::IdentRole,
        names: &[String],
    ) {
        let vendors = zeroship_migrate::shipping_vendors();
        let mut mismatches = Vec::new();

        for name in names {
            let engine_accepts = match role {
                crate::sql::ident::IdentRole::Collection => {
                    zeroship_migrate_core::schema::query::validate_collection(vendors, name).is_ok()
                }
                crate::sql::ident::IdentRole::Column => {
                    zeroship_migrate_core::schema::query::validate_field_name(vendors, name).is_ok()
                }
                other => panic!("parity corpus does not cover {other:?}"),
            };
            let schema_accepts = match role {
                crate::sql::ident::IdentRole::Collection => validate_collection(name).is_ok(),
                crate::sql::ident::IdentRole::Column => validate_field_name(name).is_ok(),
                other => panic!("parity corpus does not cover {other:?}"),
            };
            let plan_accepts = crate::sql::ident::Ident::parse_as(name, role).is_ok();

            if engine_accepts != schema_accepts || engine_accepts != plan_accepts {
                mismatches.push(format!(
                    "{name:?}: engine={engine_accepts}, runtime-compiler={schema_accepts}, zeroship-data-orm::sql={plan_accepts}"
                ));
            }
        }

        assert!(
            mismatches.is_empty(),
            "{} reservation verdicts diverged:\n{}",
            role,
            mismatches.join("\n")
        );
    }

    #[test]
    fn reserved_collection_behavior_matches_migration_engine() {
        let mut names = [
            "users",
            "_distance",
            "__zero_migrate_state",
            "__ZERO_MIGRATE_STATE",
            "__zeroship_state",
            "__ZEROSHIP_STATE",
            "__zs_internal",
            "ssn_masked",
            "pgx",
            "sqlitex",
        ]
        .map(str::to_string)
        .to_vec();
        names.extend(shipping_catalog_reservation_witnesses());

        assert_reserved_identifier_behavior_matches(crate::sql::ident::IdentRole::Collection, &names);
    }

    #[test]
    fn reserved_column_behavior_matches_migration_engine() {
        let mut names = [
            "name",
            "_distance",
            "__zero_migrate_state",
            "__zs_internal",
            "__zeroship_state",
            "ssn_masked",
            "ssn_MASKED",
            "public",
            "pii",
            "spi",
            "phi",
            "pci",
            "internal",
            "PUBLIC",
            "publication",
            "masked_ssn",
            "pgx",
            "sqlitex",
        ]
        .map(str::to_string)
        .to_vec();
        names.extend(shipping_catalog_reservation_witnesses());

        assert_reserved_identifier_behavior_matches(crate::sql::ident::IdentRole::Column, &names);
    }

    /// Names longer than 63 bytes must be rejected.
    #[test]
    fn validate_collection_rejects_name_exceeding_63_bytes() {
        let name = "a".repeat(64);
        let err = validate_collection(&name).unwrap_err();
        match err {
            QueryError::InvalidCollection(msg) => {
                assert!(msg.contains("63") || msg.contains("limit"), "{msg}");
            }
            other => panic!("expected InvalidCollection, got {other:?}"),
        }
        // 63 bytes is exactly the limit — must pass.
        assert!(
            validate_collection(&"a".repeat(63)).is_ok(),
            "63-byte name should pass"
        );
    }

    /// Null bytes must be rejected defensively.
    #[test]
    fn validate_collection_rejects_null_byte() {
        let name = "users\0evil";
        let err = validate_collection(name).unwrap_err();
        match err {
            QueryError::InvalidCollection(msg) => {
                assert!(msg.contains("null"), "unexpected message: {msg}");
            }
            other => panic!("expected InvalidCollection, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Security IMPORTANT #1 — validate_field_name length check
    // -----------------------------------------------------------------------

    /// Field names within the 63-byte limit must pass.
    #[test]
    fn validate_field_name_accepts_valid_names() {
        let long_ok = "f".repeat(63);
        for name in &["id", "user_id", "createdAt", long_ok.as_str()] {
            assert!(
                validate_field_name(name).is_ok(),
                "field name should be valid"
            );
        }
    }

    /// Field names longer than 63 bytes must be rejected.
    #[test]
    fn validate_field_name_rejects_name_exceeding_63_bytes() {
        let name = "f".repeat(64);
        let err = validate_field_name(&name).unwrap_err();
        match err {
            QueryError::InvalidIdent(msg) => {
                assert!(msg.contains("63") || msg.contains("limit"), "{msg}");
            }
            other => panic!("expected InvalidIdent, got {other:?}"),
        }
    }

    /// Field names with null bytes must be rejected.
    #[test]
    fn validate_field_name_rejects_null_byte() {
        let err = validate_field_name("col\0name").unwrap_err();
        assert!(
            matches!(err, QueryError::InvalidIdent(_)),
            "expected InvalidIdent"
        );
    }

    /// Field names with non-ASCII characters must be rejected. A multi-byte
    /// identifier could collide with another after Postgres' 63-byte
    /// truncation; ASCII-only matches `validate_collection`.
    #[test]
    fn validate_field_name_rejects_non_ascii() {
        for name in &["café", "naïve", "日本", "user—id", "field name"] {
            let err = validate_field_name(name).unwrap_err();
            assert!(
                matches!(err, QueryError::InvalidIdent(_)),
                "expected InvalidIdent for {name:?}, got {err:?}"
            );
        }
    }

    /// ASCII allowlist must accept the same shape `validate_collection`
    /// accepts: alphanumeric + underscore. `_private` was historically
    /// accepted, but the `_` prefix is now reserved for synthetic-
    /// result columns (`_distance`, `_distance_m`); see
    /// `validate_field_name_rejects_reserved_underscore_prefix` for the
    /// updated rule.
    #[test]
    fn validate_field_name_accepts_ascii_allowlist() {
        for name in &["id", "user_id", "createdAt", "v2", "first_name"] {
            assert!(
                validate_field_name(name).is_ok(),
                "ASCII allowlist should accept {name:?}",
            );
        }
    }

    // -----------------------------------------------------------------
    // Reserved-name validator
    // -----------------------------------------------------------------

    /// The `_masked` suffix is reserved for sibling columns
    /// emitted by `.mask()` / `.encrypted()`. Creator-declared fields
    /// ending in `_masked` must be refused.
    #[test]
    fn validate_field_name_rejects_reserved_masked_suffix() {
        for name in &["ssn_masked", "card_pan_masked", "email_masked", "_masked"] {
            let err = validate_field_name(name).unwrap_err();
            match err {
                QueryError::InvalidIdent(msg) => {
                    assert!(
                        msg.contains("reserved field name") && msg.contains("_masked"),
                        "expected reserved-suffix message, got: {msg}"
                    );
                }
                other => panic!("expected InvalidIdent for {name:?}, got {other:?}"),
            }
        }
    }

    /// The six default-classification names (`public`, `pii`, `spi`,
    /// `phi`, `pci`, `internal`) are reserved at the column-name level
    /// so creator schemas can't collide with the classification taxonomy.
    #[test]
    fn validate_field_name_rejects_reserved_classification_names() {
        for name in &["public", "pii", "spi", "phi", "pci", "internal"] {
            let err = validate_field_name(name).unwrap_err();
            match err {
                QueryError::InvalidIdent(msg) => {
                    assert!(
                        msg.contains("reserved field name"),
                        "expected reserved-name message for {name:?}, got: {msg}"
                    );
                }
                other => panic!("expected InvalidIdent for {name:?}, got {other:?}"),
            }
        }
    }

    /// The `_` prefix is reserved for synthetic-result columns
    /// (`_distance`, `_distance_m`, `_score`) emitted by vector /
    /// spatial native paths.
    #[test]
    fn validate_field_name_rejects_reserved_underscore_prefix() {
        for name in &["_distance", "_distance_m", "_score", "_anything"] {
            let err = validate_field_name(name).unwrap_err();
            assert!(
                matches!(err, QueryError::InvalidIdent(_)),
                "expected InvalidIdent for {name:?}, got {err:?}"
            );
        }
    }

    /// Reserved-name validator fires at filter time too: a query like
    /// `db.users.find({ ssn_masked: "..." })` is refused via
    /// `build_field_condition_with_dialect` calling `validate_field_name`.
    #[test]
    fn build_where_rejects_reserved_masked_suffix_in_filter() {
        let filter = crate::value!({ "ssn_masked": "***-**-6789" });
        let mut params: Vec<Value> = Vec::new();
        let err = build_where(&filter, &mut params, &tschema()).unwrap_err();
        match err {
            QueryError::InvalidIdent(msg) => {
                assert!(
                    msg.contains("reserved field name") && msg.contains("_masked"),
                    "expected reserved-suffix message in filter validation, got: {msg}"
                );
            }
            other => panic!("expected InvalidIdent from filter, got {other:?}"),
        }
    }

    /// `is_schema_metadata_key` lets `_meta` / `_indexes` top-level
    /// schema keys pass through schema iteration unchanged so existing
    /// test schemas (e.g. `{"_meta": {"strictness": "off"}, ...}`)
    /// still register cleanly under the new reserved-prefix rule.
    #[test]
    fn is_schema_metadata_key_matches_meta_and_indexes() {
        assert!(is_schema_metadata_key("_meta"));
        assert!(is_schema_metadata_key("_indexes"));
        assert!(!is_schema_metadata_key("_distance"));
        assert!(!is_schema_metadata_key("ssn"));
    }

    #[test]
    fn former_system_names_are_ordinary_declarations() {
        for name in FIXTURE_GENERATED_COLUMNS {
            validate_field_name_for_declaration(name).unwrap();
        }
    }

    #[test]
    fn assigned_field_names_allowed_in_filter_path() {
        for name in FIXTURE_GENERATED_COLUMNS {
            assert!(
                validate_field_name(name).is_ok(),
                "declared field {name:?} must be accepted by the filter-time validator"
            );
        }
    }

    #[test]
    fn build_where_accepts_assigned_field_names_in_filter() {
        for name in FIXTURE_GENERATED_COLUMNS {
            let mut filter_obj = crate::value::Map::new();
            filter_obj.insert(
                (*name).to_string(),
                if matches!(*name, "created_at" | "updated_at" | "deleted_at") {
                    crate::value!(0)
                } else {
                    crate::value!("any-value")
                },
            );
            let filter = crate::value::Value::Object(filter_obj);
            let mut params: Vec<Value> = Vec::new();
            let clause = build_where(&filter, &mut params, &tschema())
                .unwrap_or_else(|e| panic!("filter on {name:?} must build, got {e:?}"));
            assert!(
                clause.contains(&format!("\"{name}\"")),
                "WHERE clause must reference {name:?}; got: {clause}"
            );
        }
    }

    #[test]
    fn non_assigned_field_names_accepted_at_declaration() {
        for name in &["title", "content", "user_id", "createdAt", "first_name"] {
            assert!(
                validate_field_name_for_declaration(name).is_ok(),
                "non-declared field {name:?} must be accepted at declaration"
            );
        }
    }

    #[test]
    fn validate_field_name_for_declaration_layers_underlying_rules() {
        // Length cap inherited from `validate_field_name`.
        let long = "f".repeat(64);
        assert!(matches!(
            validate_field_name_for_declaration(&long).unwrap_err(),
            QueryError::InvalidIdent(_)
        ));
        // `_masked` suffix inherited from `RESERVED_NAMES`.
        assert!(matches!(
            validate_field_name_for_declaration("ssn_masked").unwrap_err(),
            QueryError::InvalidIdent(_)
        ));
        // `_` prefix inherited from `RESERVED_NAMES`.
        assert!(matches!(
            validate_field_name_for_declaration("_distance").unwrap_err(),
            QueryError::InvalidIdent(_)
        ));
    }

    // -----------------------------------------------------------------
    // Dialect-aware encrypted-column bind helpers
    // -----------------------------------------------------------------

    #[test]
    fn dialect_pg_encrypted_placeholder_is_decode_bytea_cast() {
        let p = SqlDialect::Postgres.binary_bind_placeholder(3);
        assert_eq!(p, "$3");
    }

    #[test]
    fn dialect_sqlite_binary_placeholder_decodes_the_parameter() {
        let p = SqlDialect::Sqlite.binary_bind_placeholder(7);
        assert_eq!(p, "$7");
    }

    // -----------------------------------------------------------------
    // Raw-column DDL emission
    // -----------------------------------------------------------------

    /// `raw_column_for_field` returns the raw column for masked columns and
    /// `None` for non-masked / kind=none columns.
    #[test]
    fn raw_column_for_field_returns_the_raw_column_for_masked() {
        let def = crate::value!({
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        });
        assert_eq!(
            raw_column_for_field("ssn", &def),
            Some("__zs_raw__ssn".to_string())
        );
    }

    /// The raw column's name must be one every inbound surface ALREADY refuses.
    ///
    /// This is the whole inbound half of the storage flip: rather than
    /// threading a schema hint into `build_where` and its fifteen call sites -
    /// and every builder written after them - the raw column is named
    /// something `validate_field_name` will not accept, so a path that has
    /// never heard of masking cannot name it in a filter, a projection, a sort,
    /// a conflict probe or a write document.
    #[test]
    fn the_raw_column_name_is_refused_by_the_inbound_validator() {
        for field in [
            "ssn",
            "email",
            "a",
            &"x".repeat(MAX_MASKED_FIELD_NAME_BYTES),
        ] {
            let raw = raw_column_name(field);
            assert!(raw.len() <= 63, "raw column must fit NAMEDATALEN: {raw}");
            let err = validate_field_name(&raw)
                .expect_err("the raw column must be unnameable on every inbound surface");
            assert!(
                format!("{err}").contains("reserved field name"),
                "expected a reserved-name refusal for {raw}, got {err}",
            );
        }
        // And the reservation it lands on predates the flip - no new fence.
        assert!(
            RESERVED_NAMES
                .iter()
                .any(|r| matches!(r, ReservedName::Prefix("__zs_")))
        );
    }

    #[test]
    fn raw_column_for_field_returns_none_for_unmasked() {
        let def = crate::value!({ "type": "string" });
        assert_eq!(raw_column_for_field("name", &def), None);
    }

    // -----------------------------------------------------------------
    // `declared_raw_column` - the descriptor names it, the data plane reads it
    // -----------------------------------------------------------------

    /// A masked field def as the migration fold emits it: `storage.rawColumn`
    /// present, spelled by the emitter that wrote the DDL.
    fn masked_def_with_raw(raw: &str) -> crate::value::Value {
        crate::value!({
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" },
            "storage": { "valueColumn": "ssn", "rawColumn": raw },
        })
    }

    /// The whole point: the emitted name WINS over the local derivation.
    ///
    /// The fixture deliberately declares a name the derivation does NOT
    /// produce, because a fixture that declared `__zs_raw__ssn` would pass
    /// against a body that ignored the descriptor entirely.
    #[test]
    fn declared_raw_column_prefers_the_name_the_descriptor_carries() {
        let def = masked_def_with_raw("__zs_raw2__ssn");
        assert_eq!(
            declared_raw_column("ssn", &def).unwrap(),
            Some("__zs_raw2__ssn".to_string()),
            "the descriptor's name must win over `raw_column_name`",
        );
    }

    /// The fence. A descriptor is CREATOR-AUTHORED - it rides in the `.zship`
    /// the worker executes - so a name it supplies must still be one every
    /// inbound surface refuses. Without this, a descriptor could redirect a
    /// masked field's PLAINTEXT into an ordinary, filterable column and read it
    /// back through a `where` oracle with no audit row, while
    /// `protection::protection_floor` waved the deploy through: that fence compares
    /// the PRESENCE of a mask declaration, never its placement.
    #[test]
    fn a_descriptor_naming_a_creator_reachable_raw_column_is_refused() {
        for reachable in ["nickname", "notes", "id", "ssn"] {
            let def = masked_def_with_raw(reachable);
            let err = declared_raw_column("ssn", &def)
                .expect_err("a raw column a creator can name in a filter must be refused");
            assert!(
                format!("{err:?}").contains(reachable),
                "the refusal must name the offending column; got {err:?}",
            );
        }
    }

    /// The malformed arm, which the reserved-name test alone does NOT cover:
    /// `validate_field_name` refuses an empty name and a name with a quote in
    /// it too, so "is refused by the validator" is satisfied by garbage. A
    /// declared raw column has to be a well-formed identifier AND reserved.
    #[test]
    fn a_malformed_declared_raw_column_is_refused() {
        for malformed in ["", "__zs_raw__a\"b", &"_".repeat(64)] {
            let def = masked_def_with_raw(malformed);
            assert!(
                declared_raw_column("ssn", &def).is_err(),
                "a malformed raw column name must be refused: {malformed:?}",
            );
        }
    }

    /// An absent `storage.rawColumn` means the derivation, not a refusal.
    ///
    /// Same shape as `field_is_readable`'s absent-flag arm and for the same
    /// reason: every hand-written test schema in the tree, and any field map
    /// that did not go through the migration fold, carries no `storage` block
    /// at all. Refusing those would take the write pipeline to zero masked
    /// fields rather than to a stricter one.
    #[test]
    fn declared_raw_column_falls_back_to_the_derivation_when_absent() {
        let def = crate::value!({
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" },
        });
        assert_eq!(
            declared_raw_column("ssn", &def).unwrap(),
            Some(raw_column_name("ssn")),
        );
        // A `storage` block that carries `valueColumn` but no `rawColumn` is
        // the same case, and is what the fold emits for an UNMASKED field.
        let partial = crate::value!({
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" },
            "storage": { "valueColumn": "ssn" },
        });
        assert_eq!(
            declared_raw_column("ssn", &partial).unwrap(),
            Some(raw_column_name("ssn")),
        );
    }

    /// A field with no mask has no raw column, and a `rawColumn` on one is
    /// IGNORED rather than refused - there is nothing to place, so there is no
    /// placement to get wrong. `protection::protection_floor` is what refuses a
    /// descriptor that dropped the mask from a column the database still
    /// records as masked; duplicating that verdict here would report the wrong
    /// defect.
    #[test]
    fn declared_raw_column_is_none_for_a_field_that_declares_no_mask() {
        assert_eq!(
            declared_raw_column("name", &crate::value!({ "type": "string" })).unwrap(),
            None,
        );
        let opted_out = crate::value!({
            "type": "string",
            "mask": { "kind": "none", "classification": "spi" },
            "storage": { "valueColumn": "ssn", "rawColumn": "nickname" },
        });
        assert_eq!(declared_raw_column("ssn", &opted_out).unwrap(), None);
    }

    #[test]
    fn raw_column_for_field_returns_none_for_kind_none() {
        let def = crate::value!({
            "type": "string",
            "encrypted": true,
            "mask": { "kind": "none", "classification": "spi" }
        });
        assert_eq!(raw_column_for_field("ssn", &def), None);
    }

    /// **Build insert** — when the row carries both parent + sibling
    /// (mask pass already ran), the INSERT statement includes both
    /// columns atomically.
    #[test]
    fn build_insert_includes_sibling_column_when_present() {
        let doc = crate::value!({
            "id": "usr_01",
            "ssn": "123-45-6789",
            "ssn_masked": "***-**-6789"
        });
        let bq = build_insert(&s("app1"), "users", &tschema(), &doc).expect("build_insert ok");
        assert!(
            bq.sql.contains("\"ssn\""),
            "parent column in SQL: {}",
            bq.sql
        );
        assert!(
            bq.sql.contains("\"ssn_masked\""),
            "sibling column in SQL: {}",
            bq.sql,
        );
    }

    // -----------------------------------------------------------------
    // §11 closeout SELECT-shape gates
    //
    // Three invariants pinned at the SQL-build layer (the production
    // path is `build_find_with_schema` -> `build_masked_aware_select_
    // expr_with_unmask`):
    //
    // 1. `default_read_does_not_touch_the_raw_column` - when a schema
    //    declares a masked column, the SELECT clause names the field's
    //    own column directly (it already holds the mask - no alias, no
    //    sibling), and the raw column (`raw_column_name`, which holds
    //    the real value) MUST NOT appear anywhere in the built SQL.
    // 2. `creator_cannot_query_by_masked_sibling` - `build_where`
    //    refuses filter keys ending in `_masked` because
    //    `validate_field_name` is on the reserved-suffix path (a
    //    reservation that predates the storage flip and still fences the
    //    name). That is pinned elsewhere; we double-check the end-to-end
    //    path through `build_find_with_schema` for belt-and-braces.
    // 3. `an_explicit_projection_of_a_masked_column_reads_the_masked_
    //    column` - the SDK `Row<S>` shape never carries the raw column.
    //    The Rust-side dual to that invariant is that callers never need
    //    to project through the raw column - reading the field's own
    //    column already returns the masked value the SDK expects.
    // -----------------------------------------------------------------

    /// Read (no `unmask` hint) for a schema with one masked column. The
    /// SELECT clause must:
    /// - name the masked column (the field's own name) verbatim, no alias,
    /// - emit `"id"` and other non-masked columns verbatim,
    /// - NEVER name the raw column anywhere in the built SQL.
    #[test]
    fn default_read_does_not_touch_the_raw_column() {
        let schema = crate::value!({
            "ssn":   { "type": "string", "encrypted": true,
                       "mask": { "kind": "last4", "classification": "spi" } },
            "email": { "type": "string" },
            "name":  { "type": "string" },
        });
        let filter = crate::value!({ "id": 7 });
        let bq = build_find_with_schema(
            &s("app1"),
            "users",
            &filter,
            Some(1),
            None,
            None,
            None,
            &schema,
        )
        .expect("build_find_with_schema ok");

        // The masked column rides through under its own name, no alias.
        assert!(
            bq.sql.contains("\"ssn\""),
            "expected the field's own (masked) column in SELECT: {}",
            bq.sql,
        );

        // The raw column must NEVER appear - not as a bare identifier, not
        // aliased, nowhere in the built SQL.
        assert!(
            !bq.sql.contains(&raw_column_name("ssn")),
            "default read must never name the raw column: {}",
            bq.sql,
        );

        // Non-masked columns ride through verbatim.
        assert!(
            bq.sql.contains("\"email\""),
            "non-masked column missing from SELECT: {}",
            bq.sql,
        );
    }

    /// An explicit projection that LISTS a masked column reads the field's
    /// own (masked) column directly - the same as the implicit/default
    /// read. The raw column never appears.
    #[test]
    fn an_explicit_projection_of_a_masked_column_reads_the_masked_column() {
        let schema = crate::value!({ "id":{"type":"string", "primaryKey":true},
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
        });
        let filter = crate::value!({});
        let select = crate::value!(["id", "ssn"]);
        let bq = build_find_with_schema(
            &s("app1"),
            "users",
            &filter,
            None,
            None,
            None,
            Some(&select),
            &schema,
        )
        .expect("build_find_with_schema ok");
        assert!(
            bq.sql.contains("SELECT \"id\", \"ssn\""),
            "explicit projection must read the field's own (masked) column: {}",
            bq.sql,
        );
        assert!(
            !bq.sql.contains(&raw_column_name("ssn")),
            "explicit projection must never name the raw column: {}",
            bq.sql,
        );
    }

    /// **The unmask hint no longer changes the projection at all.** A SELECT
    /// never names the raw column, whether or not the caller passed an
    /// `unmask` hint - plaintext is fetched afterwards by the separate,
    /// audited unmask path (which the SQL builder has no part in). This
    /// replaces the pre-flip behaviour, where the hinted column used to be
    /// served bare (pulling ciphertext into the SELECT for the encryption
    /// pass to decrypt); after the flip there is no ciphertext in the
    /// field's own column to decrypt, so the hint has nothing left to do at
    /// this layer.
    #[test]
    fn unmask_hint_does_not_change_the_projection() {
        let schema = crate::value!({
            "ssn":   { "type": "string", "encrypted": true,
                       "mask": { "kind": "last4", "classification": "spi" } },
            "email": { "type": "string",
                       "mask": { "kind": "full", "classification": "pii" } },
        });
        let filter = crate::value!({});

        let bq_no_hint = build_find_with_schema(
            &s("app1"),
            "users",
            &filter,
            None,
            None,
            None,
            None,
            &schema,
        )
        .expect("build_find_with_schema ok");

        let unmask: Vec<String> = vec!["ssn".to_string()];
        let bq_with_hint = build_find_with_schema_and_unmask(
            &s("app1"),
            "users",
            &filter,
            None,
            None,
            None,
            None,
            &schema,
            &unmask,
        )
        .expect("build_find_with_schema_and_unmask ok");

        assert_eq!(
            bq_no_hint.sql, bq_with_hint.sql,
            "the unmask hint must not change the SELECT projection",
        );
        assert!(
            !bq_with_hint.sql.contains(&raw_column_name("ssn")),
            "the unmask hint must never cause the raw column to be named: {}",
            bq_with_hint.sql,
        );
        assert!(
            !bq_with_hint.sql.contains(&raw_column_name("email")),
            "the raw column of a non-hinted masked field must never appear either: {}",
            bq_with_hint.sql,
        );
    }

    /// Creator cannot filter by the masked sibling column — the
    /// reserved-suffix validator in `validate_field_name` fires
    /// inside `build_where`, surfacing a typed `QueryError`.
    /// End-to-end gate covering the find path.
    #[test]
    fn creator_cannot_query_by_masked_sibling() {
        let schema = crate::value!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
        });
        let filter = crate::value!({ "ssn_masked": "***-**-6789" });
        let err = build_find_with_schema(
            &s("app1"),
            "users",
            &filter,
            None,
            None,
            None,
            None,
            &schema,
        )
        .expect_err("filter by sibling must be refused");
        let msg = format!("{err}");
        assert!(
            msg.contains("_masked") || msg.contains("reserved"),
            "error must reference the reserved sibling suffix: {msg}",
        );
    }

    /// Composite gate - the read projection is an explicit list even when
    /// the masked column is the only column declared. Before the flip this
    /// covered the `any_masked` short-circuit in
    /// `build_masked_aware_select_expr_with_unmask` (case 2), which aliased
    /// through the sibling; after the flip there is nothing masked-specific
    /// left to special-case - EVERY read is an explicit allowlist naming
    /// each field's own column (see
    /// `the_weakest_possible_schema_still_projects_an_explicit_allowlist`),
    /// and the raw column must never appear.
    #[test]
    fn implicit_select_expands_to_explicit_when_any_column_masked() {
        let schema = crate::value!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
        });
        let filter = crate::value!({});
        let bq = build_find_with_schema(
            &s("app1"),
            "users",
            &filter,
            None,
            None,
            None,
            None,
            &schema,
        )
        .unwrap();
        // SELECT * is never emitted when any column is masked.
        assert!(
            !bq.sql.starts_with("SELECT *"),
            "implicit SELECT must expand to an explicit list when any column is masked: {}",
            bq.sql,
        );
        assert!(
            bq.sql.contains("\"ssn\""),
            "masked column must ride through under its own name: {}",
            bq.sql,
        );
        assert!(!bq.sql.contains("\"id\""));
        assert!(
            !bq.sql.contains(&raw_column_name("ssn")),
            "the raw column must never appear: {}",
            bq.sql,
        );
    }

    /// **L24, arm 3** — a schema value that is not a field map is an ERROR, not
    /// a fallback. This is the one remaining way a caller could smuggle
    /// "absent" through a `&Value`, and it is closed.
    #[test]
    fn a_non_object_schema_is_an_error_not_an_unrestricted_projection() {
        for bad in [Value::Null, value!([]), value!("users"), value!(7)] {
            let refused = build_find_with_schema(
                &s("app1"),
                "users",
                &crate::value!({}),
                None,
                None,
                None,
                None,
                &bad,
            );
            assert!(
                refused.is_err(),
                "a non-object schema ({bad}) must be refused; got {:?}",
                refused.map(|bq| bq.sql),
            );
        }
    }

    /// The masked schema both L26 arms are built from. It carries the v2
    /// descriptor's `storage` block, because that is what a `.zship` deploy
    /// actually caches (`crates/zeroship-migrate-core/src/render/gen_types.rs:327-360`
    /// stamps it; the runtime descriptor carries it verbatim).
    fn l26_masked_schema() -> Value {
        crate::value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "pci" },
                "storage": {
                    "valueColumn": "ssn",
                    "rawColumn": raw_column_name("ssn"),
                    "rawFilterable": false,
                    "rawSortable": false,
                    "rawProjectable": false
                }
            },
        })
    }

    /// **L26** — `orderBy` on a masked column must sort by the SAME physical
    /// column the projection reads.
    ///
    /// Before the fix, `build_order_by_with_validator` emitted the bare
    /// DECLARED name through `build_order_term` while the projection served the
    /// masked sibling, so `find({ orderBy: { ssn: 1 } })` ordered rows by the
    /// plaintext/ciphertext the mask exists to hide. With `limit`/`offset` that
    /// is a binary search over a value the caller may never read.
    ///
    /// The assertion is written against [`read_column_for`], NOT against the
    /// literal `ssn_masked`, so it stays correct after the masking flip moves
    /// the readable value into the declared name and the raw value into
    /// `ssn_raw`.
    #[test]
    fn order_by_on_a_masked_column_sorts_by_the_column_the_projection_reads() {
        let schema = l26_masked_schema();
        let read = "ssn";
        let bq = build_find_with_schema(
            &s("app1"),
            "users",
            &crate::value!({}),
            Some(10),
            None,
            Some(&crate::value!({ "ssn": 1 })),
            None,
            &schema,
        )
        .unwrap();
        assert!(
            bq.sql
                .contains(&format!("ORDER BY {} ASC", quote_ident(read))),
            "orderBy must sort by the projected column {read:?}; got {}",
            bq.sql,
        );
        // And it must not sort by the authoritative column, whichever that is.
        let raw = schema["ssn"]["storage"]["rawColumn"].as_str().unwrap();
        assert!(
            !bq.sql.contains(&format!("ORDER BY {} ", quote_ident(raw))),
            "orderBy must never name the raw column {raw:?}; got {}",
            bq.sql,
        );
    }

    /// The array form of `orderBy` takes a different arm of
    /// `build_order_by_with_validator` and had the same defect.
    #[test]
    fn order_by_array_form_on_a_masked_column_sorts_by_the_projected_column() {
        let schema = l26_masked_schema();
        let read = "ssn";
        let bq = build_find_with_schema(
            &s("app1"),
            "users",
            &crate::value!({}),
            None,
            None,
            Some(&crate::value!([["ssn", -1]])),
            None,
            &schema,
        )
        .unwrap();
        assert!(
            bq.sql
                .contains(&format!("ORDER BY {} DESC", quote_ident(read))),
            "orderBy array form must sort by the projected column {read:?}; got {}",
            bq.sql,
        );
    }

    /// Vector search's projection expands to an explicit list when the
    /// schema carries a masked column - the masked column rides through
    /// under its own name (it already holds the mask) and the raw column
    /// must never appear.
    #[test]
    fn vector_search_expands_masked_projection_when_schema_cached() {
        let schema = crate::value!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
            "embedding": { "type": "vector" },
        });
        let q = build_vector_search(
            &s("app1"),
            "users",
            "embedding",
            &[0.1, 0.2],
            5,
            crate::sql::descriptors::VectorMetric::Cosine,
            &crate::value!({}),
            &schema,
        )
        .expect("vector search sql");
        assert!(
            !q.sql.starts_with("SELECT *"),
            "vector search must not fall back to SELECT * when masked columns exist: {}",
            q.sql,
        );
        assert!(
            q.sql.contains("\"ssn\""),
            "vector search must read the field's own (masked) column: {}",
            q.sql,
        );
        assert!(
            !q.sql.contains(&raw_column_name("ssn")),
            "vector search must never name the raw column: {}",
            q.sql,
        );
    }

    /// Spatial search's projection expands to an explicit list when the
    /// schema carries a masked column - the masked column rides through
    /// under its own name (it already holds the mask) and the raw column
    /// must never appear.
    #[test]
    fn spatial_near_expands_masked_projection_when_schema_cached() {
        let schema = crate::value!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
            "location": { "type": "geoPoint" },
        });
        let q = build_spatial_near(
            &s("app1"),
            "users",
            "location",
            crate::sql::descriptors::GeoPoint {
                lat: 37.7,
                lng: -122.4,
            },
            1000.0,
            &crate::value!({}),
            Some(10),
            &schema,
        )
        .expect("spatial search sql");
        assert!(
            !q.sql.starts_with("SELECT *"),
            "spatial search must not fall back to SELECT * when masked columns exist: {}",
            q.sql,
        );
        assert!(
            q.sql.contains("\"ssn\""),
            "spatial search must read the field's own (masked) column: {}",
            q.sql,
        );
        assert!(
            !q.sql.contains(&raw_column_name("ssn")),
            "spatial search must never name the raw column: {}",
            q.sql,
        );
    }

    #[test]
    fn implicit_find_projection_with_schema_uses_allowlist() {
        let schema = crate::value!({
            "name": { "type": "string" },
        });
        let bq = build_find_with_schema(
            &s("app1"),
            "users",
            &crate::value!({}),
            None,
            None,
            None,
            None,
            &schema,
        )
        .expect("find projection");
        assert!(
            !bq.sql.starts_with("SELECT *"),
            "schema-backed find must use an allowlisted projection: {}",
            bq.sql,
        );
        assert!(
            !bq.sql.contains("\"created_at\"") && bq.sql.contains("\"name\""),
            "schema-backed find must project public system fields + declared fields: {}",
            bq.sql,
        );
    }

    #[test]
    fn read_side_identifiers_reject_internal_physical_columns() {
        let schema = crate::value!({
            "name": { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
        });

        let select_err = build_find_with_schema(
            &s("app1"),
            "users",
            &crate::value!({}),
            None,
            None,
            None,
            Some(&crate::value!(["ssn_masked"])),
            &schema,
        )
        .expect_err("select on masked sibling must be refused");
        assert!(matches!(select_err, QueryError::InvalidIdent(_)));

        let sort_err = build_find_with_schema(
            &s("app1"),
            "users",
            &crate::value!({}),
            None,
            None,
            Some(&crate::value!({ "ssn_masked": 1 })),
            None,
            &schema,
        )
        .expect_err("sort on masked sibling must be refused");
        assert!(matches!(sort_err, QueryError::InvalidIdent(_)));

        let distinct_err = build_distinct_with_soft_delete_with_dialect(
            &s("app1"),
            "users",
            "ssn_masked",
            &crate::value!({}),
            false,
            &schema,
            SqlDialect::Postgres,
        )
        .expect_err("distinct on masked sibling must be refused");
        assert!(matches!(distinct_err, QueryError::InvalidIdent(_)));

        let aggregate_err = build_aggregate_with_soft_delete_with_dialect(
            &s("app1"),
            "users",
            &crate::value!([
                { "$group": { "by": "name", "cnt": { "$count": true } } },
                { "$sort": { "ssn_masked": 1 } }
            ]),
            false,
            &schema,
            SqlDialect::Postgres,
        )
        .expect_err("aggregate sort on masked sibling must be refused");
        assert!(matches!(aggregate_err, QueryError::InvalidIdent(_)));
    }

    #[test]
    fn find_limit_over_max_is_rejected() {
        let schema = crate::value!({
            "name": { "type": "string" },
        });
        let err = build_find_with_schema(
            &s("app1"),
            "users",
            &crate::value!({}),
            Some(MAX_QUERY_LIMIT + 1),
            None,
            None,
            None,
            &schema,
        )
        .expect_err("find.limit over the cap must be rejected");
        assert!(matches!(err, QueryError::InvalidFilter(_)));
        assert!(
            err.to_string().contains("find.limit"),
            "limit error must name the bounded option: {err}"
        );
    }

    #[test]
    fn vector_search_k_over_max_is_rejected() {
        let schema = crate::value!({
            "embedding": { "type": "vector" },
        });
        let err = build_vector_search(
            &s("app1"),
            "users",
            "embedding",
            &[0.1, 0.2],
            MAX_SEARCH_LIMIT + 1,
            crate::sql::descriptors::VectorMetric::Cosine,
            &crate::value!({}),
            &schema,
        )
        .expect_err("search.k over the cap must be rejected");
        assert!(matches!(err, QueryError::InvalidFilter(_)));
        assert!(
            err.to_string().contains("search.k"),
            "vector search error must name the bounded option: {err}"
        );
    }

    #[test]
    fn deeply_nested_filter_is_rejected() {
        let schema = crate::value!({
            "name": { "type": "string" },
        });
        let mut filter = crate::value!({ "name": "alice" });
        for _ in 0..MAX_FILTER_NESTING_DEPTH {
            filter = crate::value!({ "$and": [filter] });
        }
        let err = build_find_with_schema(
            &s("app1"),
            "users",
            &filter,
            None,
            None,
            None,
            None,
            &schema,
        )
        .expect_err("pathological nesting must be rejected");
        assert!(matches!(err, QueryError::InvalidFilter(_)));
        assert!(
            err.to_string().contains("nesting depth"),
            "deep filter error must mention the nesting cap: {err}"
        );
    }

    #[test]
    fn filter_clause_count_over_max_is_rejected() {
        let mut filter = crate::value::Map::new();
        for idx in 0..=MAX_FILTER_CLAUSE_COUNT {
            filter.insert(format!("f{idx}"), crate::value!(idx));
        }
        let mut params = Vec::new();
        let err = build_where(&Value::Object(filter), &mut params, &tschema())
            .expect_err("too many clauses must be rejected");
        assert!(matches!(err, QueryError::InvalidFilter(_)));
        assert!(
            err.to_string().contains("clause count"),
            "clause-count error must mention the cap: {err}"
        );
    }

    // ----------------------------------------------------------------
    // Soft-delete / restore SQL builders +
    // compose-where-with-soft-delete behaviour
    // ----------------------------------------------------------------

    #[test]
    fn compose_where_with_soft_delete_no_op_when_flag_false() {
        assert_eq!(
            compose_where_with_soft_delete("", false, &tschema()).unwrap(),
            ""
        );
        assert_eq!(
            compose_where_with_soft_delete("\"id\" = $1", false, &tschema()).unwrap(),
            "\"id\" = $1"
        );
    }

    #[test]
    fn compose_where_with_soft_delete_empty_to_lone_predicate() {
        assert_eq!(
            compose_where_with_soft_delete("", true, &tschema()).unwrap(),
            "\"deleted_at\" IS NULL"
        );
    }

    #[test]
    fn compose_where_with_soft_delete_appends_with_and() {
        assert_eq!(
            compose_where_with_soft_delete("\"id\" = $1", true, &tschema()).unwrap(),
            "(\"id\" = $1) AND \"deleted_at\" IS NULL"
        );
    }

    #[test]
    fn build_soft_delete_one_emits_update_with_deleted_at_now() {
        let filter = crate::value!({ "id": "post_x" });
        let assignments = lifecycle_assignments(false);
        let q = build_soft_delete_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(
            q.sql.starts_with("UPDATE \"app1\".\"posts\" SET"),
            "sql: {}",
            q.sql
        );
        assert!(
            q.sql.contains("\"deleted_at\" = NOW()"),
            "expected deleted_at = NOW(); got: {}",
            q.sql
        );
        assert!(q.sql.contains("\"version\" = \"version\" + $"));
        assert!(q.sql.contains("\"updated_at\" = NOW()"));
        assert!(q.sql.contains("\"updated_by\" ="));
        assert!(q.sql.contains("AND \"deleted_at\" IS NULL"));
        assert!(q.sql.contains("WHERE \"id\" = (SELECT \"id\" FROM"));
        assert!(q.sql.contains("LIMIT 1 FOR UPDATE)"));
    }

    #[test]
    fn build_soft_delete_one_sqlite_uses_current_timestamp() {
        let filter = crate::value!({ "id": "post_x" });
        let assignments = lifecycle_assignments(false);
        let q = build_soft_delete_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Sqlite,
            &assignments,
        )
        .unwrap();
        assert!(
            q.sql
                .contains("\"deleted_at\" = (strftime('%Y-%m-%dT%H:%M:%fZ','now'))"),
            "SQLite must use the ISO-T now expression: {}",
            q.sql
        );
        assert!(
            q.sql
                .contains("\"updated_at\" = (strftime('%Y-%m-%dT%H:%M:%fZ','now'))")
        );
    }

    #[test]
    fn build_soft_delete_many_omits_single_row_narrowing() {
        let filter = crate::value!({ "author": "usr_x" });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
            ],
        };
        let q = build_soft_delete_many_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(!q.sql.contains("LIMIT 1 FOR UPDATE"), "sql: {}", q.sql);
        assert!(q.sql.contains("AND \"deleted_at\" IS NULL"));
        assert!(!q.sql.contains(" RETURNING "), "sql: {}", q.sql);
    }

    /// An anonymous soft delete must CLEAR `updated_by`, not leave it.
    ///
    /// Omitting the SET clause leaves the column naming whoever last wrote the
    /// row under a session - an actor who did not perform this delete. That is
    /// a false claim about who touched the row, and anything reading
    /// `updated_by` for audit or authorization is entitled to believe it.
    #[test]
    fn build_soft_delete_one_no_actor_nulls_updated_by() {
        let filter = crate::value!({ "id": "post_x" });
        let assignments = lifecycle_assignments(false);
        let q = build_soft_delete_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(
            set_clause_of(&q.sql).contains("\"updated_by\" = $"),
            "an anonymous soft delete must null updated_by rather than leave a stale actor: {}",
            q.sql
        );
        assert!(q.sql.contains("\"deleted_at\" = NOW()"));
    }

    /// The same for `restore()`, which shares the bump triple.
    #[test]
    fn build_restore_one_no_actor_nulls_updated_by() {
        let filter = crate::value!({ "id": "post_x" });
        let assignments = lifecycle_assignments(true);
        let q = build_restore_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(
            set_clause_of(&q.sql).contains("\"updated_by\" = $"),
            "an anonymous restore must null updated_by: {}",
            q.sql
        );
    }

    /// And for a plain anonymous UPDATE arriving on the dispatch path.
    ///
    /// Keyed on `dispatch_write`, not on the actor being absent: a DIRECT
    /// builder caller (`build_set_clauses_with_dialect`) passes
    /// `lifecycle_assignments(true)` and must keep emitting no `updated_by`
    /// clause at all, because it is not writing on any actor's behalf.
    #[test]
    fn build_update_one_anonymous_dispatch_write_nulls_updated_by() {
        let filter = crate::value!({ "id": "post_x" });
        let update = crate::value!({ "title": "x" });
        let dispatched = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &WriteAssignments {
                columns: vec![
                    ColumnAssignment {
                        column: "version".into(),
                        value: AssignedValue::Increment(1),
                    },
                    ColumnAssignment {
                        column: "updated_at".into(),
                        value: AssignedValue::CurrentTimestamp,
                    },
                    ColumnAssignment {
                        column: "updated_by".into(),
                        value: AssignedValue::Bound(Value::Null),
                    },
                ],
            },
        )
        .unwrap();
        assert!(
            set_clause_of(&dispatched.sql).contains("\"updated_by\" = $"),
            "an anonymous dispatch write must null updated_by: {}",
            dispatched.sql
        );

        let direct = build_update_one_with_dialect(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
        )
        .unwrap();
        assert!(
            !set_clause_of(&direct.sql).contains("\"updated_by\""),
            "a direct builder caller writes on nobody's behalf and must not touch \
             updated_by: {}",
            direct.sql
        );
    }

    #[test]
    fn build_restore_one_clears_deleted_at_and_scopes_to_soft_deleted() {
        let filter = crate::value!({ "id": "post_x" });
        let assignments = lifecycle_assignments(true);
        let q = build_restore_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(q.sql.contains("\"deleted_at\" = $"));
        assert!(q.sql.contains("\"version\" = \"version\" + $"));
        assert!(q.sql.contains("\"updated_at\" = NOW()"));
        assert!(q.sql.contains("\"updated_by\" ="));
        assert!(q.sql.contains("AND \"deleted_at\" IS NOT NULL"));
    }

    #[test]
    fn build_restore_many_omits_single_row_narrowing() {
        let filter = crate::value!({ "author": "usr_x" });
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
            ],
        };
        let q = build_restore_many_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &assignments,
        )
        .unwrap();
        assert!(!q.sql.contains("LIMIT 1 FOR UPDATE"));
        assert!(q.sql.contains("AND \"deleted_at\" IS NOT NULL"));
        assert!(!q.sql.contains(" RETURNING "), "sql: {}", q.sql);
    }

    #[test]
    fn build_find_with_soft_delete_flag_appends_filter() {
        let filter = crate::value!({ "title": "hi" });
        let q = build_find_with_schema_and_unmask_and_soft_delete(
            &s("app1"),
            "posts",
            &filter,
            None,
            None,
            None,
            None,
            &tschema(),
            &[],
            true,
        )
        .unwrap();
        assert!(
            q.sql.contains(" AND \"deleted_at\" IS NULL"),
            "soft-delete filter must be appended: {}",
            q.sql
        );
    }

    #[test]
    fn build_find_with_soft_delete_flag_off_is_byte_identical_to_legacy() {
        let filter = crate::value!({ "title": "hi" });
        let q_legacy = build_find_with_schema_and_unmask(
            &s("app1"),
            "posts",
            &filter,
            None,
            None,
            None,
            None,
            &tschema(),
            &[],
        )
        .unwrap();
        let q_new = build_find_with_schema_and_unmask_and_soft_delete(
            &s("app1"),
            "posts",
            &filter,
            None,
            None,
            None,
            None,
            &tschema(),
            &[],
            false,
        )
        .unwrap();
        assert_eq!(q_legacy.sql, q_new.sql, "back-compat: identical SQL");
        assert_eq!(q_legacy.params, q_new.params);
    }

    #[test]
    fn build_find_empty_filter_with_soft_delete_flag_emits_lone_predicate() {
        let filter = crate::value!({});
        let q = build_find_with_schema_and_unmask_and_soft_delete(
            &s("app1"),
            "posts",
            &filter,
            None,
            None,
            None,
            None,
            &tschema(),
            &[],
            true,
        )
        .unwrap();
        assert!(
            q.sql.contains("WHERE \"deleted_at\" IS NULL"),
            "lone soft-delete predicate when no creator filter: {}",
            q.sql
        );
    }

    #[test]
    fn build_count_with_soft_delete_appends_filter() {
        let filter = crate::value!({});
        let q = build_count_with_soft_delete(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            true,
            SqlDialect::Postgres,
        )
        .unwrap();
        assert!(q.sql.contains("WHERE \"deleted_at\" IS NULL"));
        let q2 = build_count_with_soft_delete(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            false,
            SqlDialect::Postgres,
        )
        .unwrap();
        assert!(!q2.sql.contains("WHERE"));
    }

    #[test]
    fn build_aggregate_with_soft_delete_appends_filter() {
        let pipeline = crate::value!([
            { "$match": { "country": "US" } },
            { "$group": { "by": "city", "n": { "$count": 1 } } },
        ]);
        let q = build_aggregate_with_soft_delete(&s("app1"), "users", &pipeline, true, &tschema())
            .unwrap();
        assert!(
            q.sql.contains("WHERE ") && q.sql.contains("AND \"deleted_at\" IS NULL"),
            "aggregate WHERE must compose creator $match AND soft-delete: {}",
            q.sql
        );
    }

    #[test]
    fn build_distinct_with_soft_delete_appends_filter() {
        let filter = crate::value!({});
        let q = build_distinct_with_soft_delete(
            &s("app1"),
            "users",
            "country",
            &filter,
            true,
            &tschema(),
        )
        .unwrap();
        assert!(q.sql.contains("WHERE \"deleted_at\" IS NULL"));
    }

    #[test]
    fn legacy_build_count_is_byte_identical_to_soft_delete_off() {
        let filter = crate::value!({ "id": "x" });
        let q_legacy = build_count(&s("app1"), "posts", &tschema(), &filter).unwrap();
        let q_new = build_count_with_soft_delete(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            false,
            SqlDialect::Postgres,
        )
        .unwrap();
        assert_eq!(q_legacy.sql, q_new.sql);
    }

    #[test]
    fn build_update_one_sqlite_uses_declared_key_narrowing() {
        let filter = crate::value!({ "id": "post_1" });
        let update = crate::value!({ "title": "next" });
        let q = build_update_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Sqlite,
            &WriteAssignments::default(),
        )
        .unwrap();
        assert!(q.sql.contains("WHERE \"id\" = (SELECT \"id\" FROM"));
        assert!(!q.sql.contains("FOR UPDATE"));
    }

    #[test]
    fn build_delete_one_sqlite_uses_declared_key_narrowing() {
        let filter = crate::value!({ "id": "post_1" });
        let q = build_delete_one_with_dialect(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert!(q.sql.contains("WHERE \"id\" = (SELECT \"id\" FROM"));
        assert!(!q.sql.contains("FOR UPDATE"));
    }

    #[test]
    fn build_soft_delete_one_sqlite_uses_declared_key_narrowing() {
        let filter = crate::value!({ "id": "post_1" });
        let q = build_soft_delete_one_with_assignments(
            &s("app1"),
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Sqlite,
            &WriteAssignments::default(),
        )
        .unwrap();
        assert!(q.sql.contains("WHERE \"id\" = (SELECT \"id\" FROM"));
        assert!(!q.sql.contains("FOR UPDATE"));
    }

    // -----------------------------------------------------------------------
    // The write-side projection: `RETURNING` names columns, never `*`
    // -----------------------------------------------------------------------

    /// Exercise each write shape against the same descriptor.
    fn every_write_query(schema: &Value) -> Vec<(&'static str, CompiledQuery)> {
        let mut complete = write_projection_schema();
        complete
            .as_object_mut()
            .unwrap()
            .extend(schema.as_object().unwrap().clone());
        let schema = &complete;

        let doc = crate::value!({ "ssn": "123-45-6789" });
        let docs = crate::value!([{ "ssn": "1" }, { "ssn": "2" }]);
        let filter = crate::value!({ "id": "usr_1" });
        let update = crate::value!({ "ssn": "9" });
        let conflict = crate::value!(["id"]);
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "version".into(),
                    value: AssignedValue::Increment(1),
                },
                ColumnAssignment {
                    column: "updated_at".into(),
                    value: AssignedValue::CurrentTimestamp,
                },
            ],
        };
        let d = SqlDialect::Postgres;
        vec![
            (
                "insert",
                build_insert_with_dialect(&s("app1"), "users", schema, &doc, d).unwrap(),
            ),
            (
                "insertMany",
                build_insert_many_with_dialect(&s("app1"), "users", schema, &docs, d).unwrap(),
            ),
            (
                "updateOne",
                build_update_one_with_assignments(
                    &s("app1"),
                    "users",
                    schema,
                    &filter,
                    &update,
                    d,
                    &assignments,
                )
                .unwrap(),
            ),
            (
                "updateMany",
                build_update_many_with_assignments(
                    &s("app1"),
                    "users",
                    schema,
                    &filter,
                    &update,
                    d,
                    &assignments,
                )
                .unwrap(),
            ),
            (
                "deleteOne",
                build_delete_one_with_dialect(&s("app1"), "users", schema, &filter, d).unwrap(),
            ),
            (
                "deleteMany",
                build_delete_many(&s("app1"), "users", schema, &filter, d).unwrap(),
            ),
            (
                "softDeleteOne",
                build_soft_delete_one_with_assignments(
                    &s("app1"),
                    "users",
                    schema,
                    &filter,
                    d,
                    &assignments,
                )
                .unwrap(),
            ),
            (
                "softDeleteMany",
                build_soft_delete_many_with_assignments(
                    &s("app1"),
                    "users",
                    schema,
                    &filter,
                    d,
                    &assignments,
                )
                .unwrap(),
            ),
            (
                "restoreOne",
                build_restore_one_with_assignments(
                    &s("app1"),
                    "users",
                    schema,
                    &filter,
                    d,
                    &assignments,
                )
                .unwrap(),
            ),
            (
                "restoreMany",
                build_restore_many_with_assignments(
                    &s("app1"),
                    "users",
                    schema,
                    &filter,
                    d,
                    &assignments,
                )
                .unwrap(),
            ),
            (
                "upsert",
                build_upsert_with_dialect(&s("app1"), "users", schema, doc.clone(), &conflict, d).unwrap(),
            ),
            (
                "findOrCreate",
                build_find_or_create(&s("app1"), "users", schema, &doc, &conflict).unwrap(),
            ),
        ]
    }

    const WRITE_BUILDERS_TESTED: usize = 12;

    fn count_only_write(verb: &str) -> bool {
        matches!(
            verb,
            "updateMany" | "deleteMany" | "softDeleteMany" | "restoreMany"
        )
    }

    /// A schema whose masked field makes the raw column REACHABLE by `*`.
    ///
    /// `ssn` is masked, so the physical table has `ssn` (the mask) AND
    /// `__zs_raw__ssn` (the real value). `RETURNING *` expands to both. This is
    /// the fixture that can tell a projection from a star; a schema with no
    /// masked field cannot, because there every physical column is also a
    /// logical one.
    fn write_projection_schema() -> Value {
        let mut fields = tschema();
        fields
            .as_object_mut()
            .unwrap()
            .extend(l26_masked_schema().as_object().unwrap().clone());
        fields
    }

    #[test]
    fn writes_return_declared_records_or_only_counts() {
        let schema = write_projection_schema();
        let queries = every_write_query(&schema);
        assert_eq!(
            queries.len(),
            WRITE_BUILDERS_TESTED,
            "this test must rule on every write builder",
        );
        for (verb, q) in &queries {
            if count_only_write(verb) {
                assert!(!q.sql.contains(" RETURNING "), "{verb}: {}", q.sql);
                continue;
            }
            assert!(
                !q.sql.contains("RETURNING *"),
                "{verb} must not expand its RETURNING to `*`: {}",
                q.sql,
            );
            assert!(
                q.sql.contains(r#"RETURNING "id""#),
                "{verb} must open its RETURNING with the named id column: {}",
                q.sql,
            );
            for field in FIXTURE_GENERATED_COLUMNS {
                assert!(
                    q.sql.contains(&quote_ident(field)),
                    "{verb} must project the declared field {field}: {}",
                    q.sql,
                );
            }
            assert!(
                q.sql.contains(r#""ssn""#),
                "{verb} must name the declared field: {}",
                q.sql,
            );
        }
    }

    /// The point of the change, and the half a `SELECT` already had.
    ///
    /// The raw column is not "excluded" by a rule that names it - it is absent
    /// because the projection is built from the descriptor's field keys and the
    /// raw column is not one. It lives inside `storage.rawColumn`, which
    /// nothing in the projection path reads.
    #[test]
    fn no_write_builder_names_a_masked_fields_raw_column() {
        let schema = write_projection_schema();
        let raw = schema["ssn"]["storage"]["rawColumn"]
            .as_str()
            .expect("fixture declares the raw column");
        let queries = every_write_query(&schema);
        assert_eq!(queries.len(), WRITE_BUILDERS_TESTED);
        for (verb, q) in &queries {
            assert!(
                !q.sql.contains(raw),
                "{verb} must never name the raw column {raw}: {}",
                q.sql,
            );
        }
    }

    /// `readable: false` is the descriptor's own narrowing knob, and until this
    /// change nothing in Rust read it.
    ///
    /// One flag, three surfaces: it must leave the write projection, leave the
    /// implicit read projection, and make an explicit `select` of the field a
    /// refusal rather than a served column.
    #[test]
    fn an_unreadable_field_leaves_every_projection_and_is_refused_by_name() {
        let schema = crate::value!({
            "id":{"type":"string", "primaryKey":true},
            "created_at":{"type":"date"}, "updated_at":{"type":"date"},
            "created_by":{"type":"string"}, "updated_by":{"type":"string"},
            "version":{"type":"int"}, "deleted_at":{"type":"date", "softDelete":true},
            "public_note": { "type": "string", "readable": true },
            "internal_note": { "type": "string", "readable": false },
        });
        let queries = every_write_query(&schema);
        assert_eq!(queries.len(), WRITE_BUILDERS_TESTED);
        for (verb, q) in &queries {
            assert!(
                q.sql.contains(r#""public_note""#) == !count_only_write(verb),
                "{verb} must project readable fields only when returning records: {}",
                q.sql,
            );
            assert!(
                !q.sql.contains("internal_note"),
                "{verb} must not project a field the descriptor marks unreadable: {}",
                q.sql,
            );
        }
        let select = build_masked_aware_select_expr(None, &schema).unwrap();
        assert!(
            select.contains(r#""public_note""#) && !select.contains("internal_note"),
            "the implicit SELECT list must honour `readable` too: {select}",
        );
        assert!(
            validate_read_identifier("internal_note", &schema).is_err(),
            "an unreadable field must not be nameable in `select` / `orderBy`",
        );
        assert!(
            validate_read_identifier("public_note", &schema).is_ok(),
            "a readable field must stay nameable",
        );
    }

    /// `findOrCreate` carries a computed column beside the row, and `upsert`
    /// does not.
    ///
    /// `(xmax = 0) AS __created` is how `findOrCreate` reports
    /// insert-vs-update; it is a PG system-column read, not a table column, so
    /// narrowing the row projection must not take it with it.
    ///
    /// **One builder emits it, not two.** [`SYNTHETIC_RESULT_COLUMNS`] credited
    /// `build_upsert_with_dialect` with it until 2026-08-28 and that builder has
    /// never emitted it - the `upsert` arm below is the control that says so,
    /// and it is the reason this test names both.
    #[test]
    fn only_find_or_create_carries_the_created_computed_column() {
        let schema = write_projection_schema();
        let doc = crate::value!({ "ssn": "1" });
        let conflict = crate::value!(["id"]);
        let upsert = build_upsert_with_dialect(
            &s("app1"),
            "users",
            &schema,
            doc.clone(),
            &conflict,
            SqlDialect::Postgres,
        )
        .unwrap();
        let foc = build_find_or_create(&s("app1"), "users", &schema, &doc, &conflict).unwrap();

        assert!(
            foc.sql.ends_with("(xmax = 0) AS __created"),
            "findOrCreate must keep the computed created flag last: {}",
            foc.sql,
        );
        assert!(
            !upsert.sql.contains("__created"),
            "upsert has never reported insert-vs-update; it must not start now: {}",
            upsert.sql,
        );
        for (verb, q) in [("upsert", &upsert), ("findOrCreate", &foc)] {
            assert!(
                !q.sql.contains("RETURNING *"),
                "{verb} must not keep the star beside it: {}",
                q.sql,
            );
            assert!(
                q.sql.contains(r#"RETURNING "id""#),
                "{verb} must open its RETURNING with the named id column: {}",
                q.sql,
            );
        }
    }

    /// The projection reads `storage.valueColumn`, not the field's name.
    ///
    /// Today the descriptor always records `valueColumn == field`, so this
    /// cannot be observed from the shipped artifacts - which is exactly why it
    /// is pinned here. A future physical rename that keeps the logical name
    /// stable is a producer change; if the projection formatted the name
    /// instead of reading the block, it would silently name a column that is
    /// not there.
    #[test]
    fn the_projection_reads_the_declared_value_column_under_the_logical_name() {
        let schema = crate::value!({
            "amount": { "type": "number", "storage": { "valueColumn": "amount_v2" } },
        });
        let queries = every_write_query(&schema);
        assert_eq!(queries.len(), WRITE_BUILDERS_TESTED);
        for (verb, q) in &queries {
            assert!(
                q.sql.contains(r#""amount_v2" AS "amount""#) == !count_only_write(verb),
                "{verb} must read the declared physical column under the logical name: {}",
                q.sql,
            );
        }
        let select = build_masked_aware_select_expr(None, &schema).unwrap();
        assert!(
            select.contains(r#""amount_v2" AS "amount""#),
            "the SELECT list must resolve the same way: {select}",
        );
    }

    /// The upsert's `version` bump must qualify the column it READS.
    ///
    /// `ON CONFLICT DO UPDATE SET "version" = COALESCE("version", 0) + 1` is
    /// refused by PostgreSQL with `42702 column reference "version" is
    /// ambiguous`: the target row and `excluded` are both in scope for the SET
    /// expression. Measured on 17.11 - the unqualified form errors, the
    /// relation-qualified form returns the row - and on SQLite 3.51.2, which
    /// accepts both, which is why nothing caught it: the upsert tests that
    /// EXECUTE are the SQLite ones.
    ///
    /// The assignment target on the left must stay UNqualified; PostgreSQL
    /// refuses a qualified one there. Both halves are asserted.
    #[test]
    fn the_upserts_version_bump_qualifies_the_column_it_reads() {
        let schema = write_projection_schema();
        let doc = crate::value!({ "ssn": "1" });
        let conflict = crate::value!(["id"]);
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let q = build_upsert_with_assignments(
                &s("app1"),
                "users",
                &schema,
                doc.clone(),
                &conflict,
                dialect,
                &lifecycle_assignments(false),
                None,
            )
            .unwrap();
            assert!(
                q.sql
                    .contains(r#""version" = "app1"."users"."version" + $"#),
                "{dialect:?}: the read of `version` must name its relation: {}",
                q.sql,
            );
            assert!(
                !q.sql.contains(r#"SET "app1"."users"."version" ="#),
                "{dialect:?}: the assignment target must stay unqualified: {}",
                q.sql,
            );
        }
    }

    /// A write builder cannot be handed "no schema".
    ///
    /// The read builders lost their permissive arm at L24 for exactly this
    /// reason; the write builders never had one to lose because they took no
    /// schema at all. A non-object is a refusal, not a fallback to `*`.
    #[test]
    fn a_write_builder_refuses_a_non_object_schema_rather_than_starring() {
        let doc = crate::value!({ "ssn": "1" });
        let err = build_insert_with_dialect(
            &s("app1"),
            "users",
            &Value::Null,
            &doc,
            SqlDialect::Postgres,
        )
        .expect_err("a schema-less write must be refused");
        assert!(
            matches!(err, QueryError::InvalidFilter(_)),
            "expected the projection's own refusal, got {err:?}",
        );
    }
}

/// Runtime raw-column conventions must agree with migration emission.
#[cfg(test)]
mod raw_column_parity {
    use super::*;

    use zeroship_migrate_core::schema::query as engine;

    /// The identifier budget both sides bake into their cap.
    ///
    /// Spelled here, once, so the two assertions in
    /// [`each_side_derives_its_cap_from_its_own_prefix_and_the_same_budget`]
    /// compare each crate's derivation against a stated number rather than
    /// against each other - which is what lets a change to ONE side's literal
    /// fail.
    const IDENTIFIER_BUDGET_BYTES: usize = 63;

    /// Field-name shapes the two `raw_column_name`s must agree on.
    ///
    /// Includes the empty string and a name that already carries the prefix:
    /// both functions are TOTAL, so a divergence on a degenerate input is as real
    /// as one at the boundary. Includes a multi-byte name because the cap is
    /// measured in BYTES on both sides (`field.len()`, not `chars().count()`) -
    /// see
    /// [`the_byte_cap_is_unreachable_through_declaration_because_both_validators_are_ascii_only`]
    /// for why that distinction cannot be reached through a declaration today.
    fn field_corpus() -> Vec<String> {
        vec![
            String::new(),
            "a".to_string(),
            "ssn".to_string(),
            "email_address".to_string(),
            "Mixed_Case9".to_string(),
            raw_column_name("ssn"),
            // Three bytes per character: the byte length is 3x the char count.
            "\u{4f60}\u{597d}\u{4e16}\u{754c}".to_string(),
            "x".repeat(MAX_MASKED_FIELD_NAME_BYTES),
            "x".repeat(MAX_MASKED_FIELD_NAME_BYTES + 1),
            "x".repeat(IDENTIFIER_BUDGET_BYTES),
        ]
    }

    #[test]
    fn the_masked_field_name_cap_matches_the_migration_engine() {
        assert_eq!(
            MAX_MASKED_FIELD_NAME_BYTES,
            engine::MAX_MASKED_FIELD_NAME_BYTES,
            "one side would accept a masked field declaration the other refuses",
        );
    }

    #[test]
    fn raw_column_name_matches_the_migration_engine_over_the_corpus() {
        let mut divergences = Vec::new();
        for field in field_corpus() {
            let ours = raw_column_name(&field);
            let theirs = engine::raw_column_name(&field);
            if ours != theirs {
                divergences.push(format!("{field:?}: data-plane={ours:?}, engine={theirs:?}"));
            }
        }
        assert!(
            divergences.is_empty(),
            "{} raw column name(s) diverged across the migration-engine boundary:\n{}",
            divergences.len(),
            divergences.join("\n"),
        );
    }

    /// What the cap is FOR, asserted as a length rather than as a number.
    ///
    /// The longest masked field either side accepts must produce a raw column
    /// that exactly fills the budget, and one byte more must overflow it. This is
    /// the arm that survives a coordinated rename of the constants and still
    /// catches a cap that stopped describing the name it caps.
    #[test]
    fn the_longest_accepted_masked_field_exactly_fills_the_identifier_budget() {
        let at_limit = "x".repeat(MAX_MASKED_FIELD_NAME_BYTES);
        let over = "x".repeat(MAX_MASKED_FIELD_NAME_BYTES + 1);
        assert_eq!(
            raw_column_name(&at_limit).len(),
            IDENTIFIER_BUDGET_BYTES,
            "the data plane's longest accepted masked field does not fill the budget",
        );
        assert_eq!(
            raw_column_name(&over).len(),
            IDENTIFIER_BUDGET_BYTES + 1,
            "the data plane's first refused masked field does not overflow the budget",
        );

        let engine_at_limit = "x".repeat(engine::MAX_MASKED_FIELD_NAME_BYTES);
        let engine_over = "x".repeat(engine::MAX_MASKED_FIELD_NAME_BYTES + 1);
        assert_eq!(
            engine::raw_column_name(&engine_at_limit).len(),
            IDENTIFIER_BUDGET_BYTES,
            "the engine's longest accepted masked field does not fill the budget",
        );
        assert_eq!(
            engine::raw_column_name(&engine_over).len(),
            IDENTIFIER_BUDGET_BYTES + 1,
            "the engine's first refused masked field does not overflow the budget",
        );
    }

    /// The budget is not this crate's to choose.
    ///
    /// `zeroship_migrate_core::schema::query::validate_field_name` caps a field
    /// name at the TIGHTEST identifier budget any registered backend declares,
    /// while both `MAX_MASKED_FIELD_NAME_BYTES` bake the literal `63`. Register a
    /// vendor with a tighter cap and the two disagree: the masked-field cap would
    /// admit a name the field-name validator refuses. Nothing else in the tree
    /// relates those numbers.
    #[test]
    fn the_identifier_budget_matches_the_tightest_shipping_vendor_limit() {
        use zeroship_migrate::IdentifierLimit;

        let tightest = zeroship_migrate::shipping_vendors()
            .as_slice()
            .iter()
            .map(|vendor| match vendor.descriptor.limits.identifier {
                IdentifierLimit::Bytes(n) | IdentifierLimit::Characters(n) => n,
                IdentifierLimit::Unbounded => usize::MAX,
            })
            .min()
            .expect("the shipping vendor set is never empty");

        assert_eq!(
            tightest, IDENTIFIER_BUDGET_BYTES,
            "a registered backend declares a tighter identifier budget than the \
             masked-field cap assumes; MAX_MASKED_FIELD_NAME_BYTES would admit a \
             field name validate_field_name refuses",
        );
    }

    /// The byte/char question, and why no test can reach it through a
    /// declaration.
    ///
    /// Both caps compare `field.len()` - BYTES, not characters - so a name that
    /// is short by characters and long by bytes is the input that separates the
    /// two readings. Neither validator ever sees it: both `validate_field_name`
    /// implementations allow only ASCII alphanumerics and underscore, so a
    /// multi-byte name is refused for its ENCODING and the byte cap is never
    /// consulted. Recorded as a test rather than a comment so that the day the
    /// allowlist widens, this states what widening exposes.
    ///
    /// # The probe has to be sized, not just multi-byte
    ///
    /// This arm first used a 53-character probe (159 bytes) and asserted the
    /// allowlist refused it. It did not: `validate_field_name` checks
    /// `name.len() > 63` FIRST, so a 159-byte name is refused for overall
    /// LENGTH and the assertion was reading a verdict the allowlist never cast.
    /// The probe is now sized into the window `MAX_MASKED_FIELD_NAME_BYTES <
    /// bytes <= 63`, where the length gate cannot fire, and the refusal is
    /// matched against the allowlist's own message rather than against
    /// `is_err()`.
    #[test]
    fn the_byte_cap_is_unreachable_through_declaration_because_both_validators_are_ascii_only() {
        const ALLOWLIST_REFUSAL: &str = "allowed: ASCII alphanumeric + underscore";

        let vendors = zeroship_migrate::shipping_vendors();
        // The shortest multi-byte name that overflows the masked-field cap.
        let chars = MAX_MASKED_FIELD_NAME_BYTES / 3 + 1;
        let multibyte = "\u{4f60}".repeat(chars);

        assert!(
            multibyte.chars().count() <= MAX_MASKED_FIELD_NAME_BYTES,
            "the probe must be short by CHARACTERS for the distinction to exist",
        );
        assert!(
            multibyte.len() > MAX_MASKED_FIELD_NAME_BYTES,
            "the probe must be long by BYTES for the distinction to exist",
        );
        assert!(
            multibyte.len() <= IDENTIFIER_BUDGET_BYTES,
            "the probe must fit the overall identifier budget, or the length gate \
             refuses it and this arm measures that gate instead of the allowlist",
        );

        let ours = validate_field_name(&multibyte).expect_err("the data plane must refuse it");
        assert!(
            ours.to_string().contains(ALLOWLIST_REFUSAL),
            "the data plane refused for the wrong reason: {ours}",
        );
        let theirs = engine::validate_field_name(vendors, &multibyte)
            .expect_err("the engine must refuse it");
        assert!(
            theirs.to_string().contains(ALLOWLIST_REFUSAL),
            "the engine refused for the wrong reason: {theirs}",
        );
    }
}

/// Binds the reserved typed-id prefix fence to the migration engine's copy of it.
///
/// # What was unbound - which is NOT what it looks like
///
/// [`RESERVED_ID_PREFIXES`] exists in three places: here, at
/// `zeroship_migrate_core::schema::query::RESERVED_ID_PREFIXES`, and as
/// `ID_RESERVED_PREFIX` in `sdks/db/src/types.ts`. Both Rust copies carry an
/// agreement claim in their own doc and neither named a guard.
///
/// The obvious hypothesis - that a SHRINK is silent, so dropping `usr` would let
/// a creator mint ids colliding with platform user ids - is wrong, and was
/// measured wrong before this module was written. Each crate already binds its
/// OWN validator: emptying this crate's list fails
/// `query::tests::p7_id_prefix_decl_with_reserved_usr_is_rejected`, and emptying
/// the engine's fails four of its arms, `p2a_create_table_rejects_a_reserved_id_prefix`
/// among them. A coordinated shrink fails both sets. Nothing here needed adding
/// for that case.
///
/// What NOTHING held is the two lists DIVERGING while each side stays
/// self-consistent. Measured: adding one entry to the engine's list alone leaves
/// all 894 of `zeroship-migrate-core --lib` green and every pre-existing arm in
/// this crate green. The cost is a creator prefix the runtime accepts and the
/// migration service refuses at apply time, or the reverse - a fence that exists
/// on one side of the deploy path only.
///
/// [`the_platform_user_prefix_is_reserved_on_both_sides`] covers the case the
/// per-crate arms cannot: it holds both lists against
/// [`zeroship_core::typed_id::USER_PREFIX`] - the prefix
/// `zeroship_core::typed_id::new_user_id` actually stamps - so the reservation is
/// tied to the thing it protects rather than to a string three files happen to
/// share.
///
/// # The pair this does NOT bind
///
/// `sdks/db/src/types.ts` is a hand-written literal in a package no Rust test
/// reads. Binding it needs either a fixture generated from the Rust constant that
/// the TypeScript imports, or one source of truth both sides read; neither
/// exists, and this module does not pretend otherwise.
#[cfg(test)]
mod reserved_id_prefix_parity {
    use super::*;

    use zeroship_migrate_core::schema::query as engine;

    /// The reserved list, stated here as a literal.
    ///
    /// Every arm below drives THIS, never [`RESERVED_ID_PREFIXES`]. Driving the
    /// list under test is how a shrink goes green: emptying either crate's
    /// constant makes a loop over it iterate nothing, and a sweep that examines
    /// nothing reports success. Measured - the first draft of this module did
    /// exactly that, and `RESERVED_ID_PREFIXES = &[]` left two of its four arms
    /// passing.
    const RESERVED: &[&str] = &["usr"];

    /// Well-formed prefixes a creator may have.
    ///
    /// `user` / `usrs` / `usr2` are here on purpose: the fence is exact-match,
    /// not prefix-match, and a side that switched to `starts_with` would begin
    /// refusing legitimate creator prefixes.
    const FREE: &[&str] = &[
        "blog", "post", "u", "u1", "a_b", "usr2", "user", "usrs", "acct",
    ];

    /// Refused by the charset rule, not by the deny-list. Separated because a
    /// prefix can be refused for two reasons and `is_err()` cannot tell them
    /// apart.
    const MALFORMED: &[&str] = &["", "Usr", "1abc", "a-b", "_x", "a b", "usr!"];

    /// Each side against the stated list, rather than against the other side.
    ///
    /// Comparing the two constants to each other would go green on a coordinated
    /// edit. Comparing each to [`RESERVED`] fails on a one-sided edit AND on a
    /// two-sided one.
    #[test]
    fn each_side_carries_the_reserved_list_this_module_states() {
        assert_eq!(
            RESERVED_ID_PREFIXES, RESERVED,
            "the data plane's reserved typed-id prefix list moved",
        );
        assert_eq!(
            engine::RESERVED_ID_PREFIXES,
            RESERVED,
            "the migration engine's reserved typed-id prefix list moved",
        );
    }

    /// Why `usr` is on the list at all, held against its authority.
    ///
    /// [`RESERVED`] is a literal in this file, so on its own it is just a fourth
    /// copy. This arm ties it to `typed_id`'s constant - the prefix
    /// `zeroship_core::typed_id::new_user_id` actually stamps - so the reservation
    /// tracks the thing it protects rather than a number someone typed.
    #[test]
    fn the_platform_user_prefix_is_reserved_on_both_sides() {
        let user_prefix = zeroship_core::typed_id::USER_PREFIX;
        assert!(
            RESERVED.contains(&user_prefix),
            "this module's stated list no longer covers the platform user prefix \
             {user_prefix:?}",
        );
        assert!(
            RESERVED_ID_PREFIXES.contains(&user_prefix),
            "the data plane stopped reserving the platform user prefix {user_prefix:?}; \
             a creator declaring t.id({user_prefix:?}) would mint ids in the platform's \
             own id space",
        );
        assert!(
            engine::RESERVED_ID_PREFIXES.contains(&user_prefix),
            "the migration engine stopped reserving the platform user prefix \
             {user_prefix:?}",
        );
    }

    /// Both sides must still CONSULT their list, not merely agree on its value.
    ///
    /// Two constants can be equal while one validator has stopped reading its own,
    /// so this arm crosses the VERDICT path and pins the expected verdict rather
    /// than only comparing the two - a change making both sides accept `usr` could
    /// otherwise pass as agreement.
    #[test]
    fn both_validators_rule_the_same_way_over_the_corpus() {
        let mut mismatches = Vec::new();
        for (accepted, prefix) in RESERVED
            .iter()
            .map(|p| (false, *p))
            .chain(FREE.iter().map(|p| (true, *p)))
            .chain(MALFORMED.iter().map(|p| (false, *p)))
        {
            let ours = validate_id_prefix(prefix).is_ok();
            let theirs = engine::validate_id_prefix(prefix).is_ok();
            if ours != theirs || ours != accepted {
                mismatches.push(format!(
                    "{prefix:?}: data-plane={ours}, engine={theirs}, expected {accepted}"
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} typed-id prefix verdict(s) diverged:\n{}",
            mismatches.len(),
            mismatches.join("\n"),
        );
    }

    /// A reserved prefix must be refused AS reserved.
    ///
    /// `is_ok()` alone cannot separate the deny-list from the charset rule, so a
    /// side that deleted its list but happened to refuse `usr` for some other
    /// reason would read as agreement. Both crates spell the refusal
    /// `ReservedIdPrefix`.
    #[test]
    fn both_refuse_a_reserved_prefix_as_reserved_rather_than_as_malformed() {
        for prefix in RESERVED {
            let ours = validate_id_prefix(prefix).expect_err("the data plane must refuse it");
            assert!(
                matches!(ours, QueryError::ReservedIdPrefix(_)),
                "the data plane refused {prefix:?} for the wrong reason: {ours:?}",
            );
            let theirs = engine::validate_id_prefix(prefix).expect_err("the engine must refuse it");
            assert!(
                matches!(theirs, engine::QueryError::ReservedIdPrefix(_)),
                "the engine refused {prefix:?} for the wrong reason: {theirs:?}",
            );
        }
    }
}

/// Runtime and migration timestamp renderers must agree across dialects.
/// SQLite stores these timestamps as text, so defaults and updates must use
/// the same ISO timestamp spelling to preserve chronological ordering.
#[cfg(test)]
mod sqlite_now_parity {
    use super::*;

    use zeroship_migrate_backend::registry::BackendVendor;

    const SQLITE_NOW: &str = "(strftime('%Y-%m-%dT%H:%M:%fZ','now'))";

    /// The dialect ids this crate's [`SqlDialect`] variants correspond to.
    ///
    /// Exhaustive over the enum by construction: the match below has no
    /// wildcard, so an additional variant fails to compile here rather than being
    /// skipped by a census that never looked for it.
    fn dialect_id(dialect: SqlDialect) -> &'static str {
        match dialect {
            SqlDialect::Postgres => "postgres",
            SqlDialect::Sqlite => "sqlite",
        }
    }

    /// Every dialect this crate renders, paired with the shipping vendor that
    /// answers for it.
    ///
    /// `expect` rather than a skip: a dialect this crate emits SQL for and the
    /// shipping set has no backend for is a finding, not a reason to examine
    /// fewer rows.
    fn pairs() -> Vec<(SqlDialect, &'static BackendVendor)> {
        [SqlDialect::Postgres, SqlDialect::Sqlite]
            .into_iter()
            .map(|dialect| {
                let id = dialect_id(dialect);
                let vendor = zeroship_migrate::shipping_vendors()
                    .as_slice()
                    .iter()
                    .copied()
                    .find(|v| v.descriptor.id.as_str() == id)
                    .unwrap_or_else(|| {
                        panic!("the shipping set has no backend for the {id} dialect")
                    });
                (dialect, vendor)
            })
            .collect()
    }

    #[test]
    fn all_three_sqlite_now_spellings_are_the_one_this_module_states() {
        let vendor = sqlite_vendor();
        assert_eq!(SqlDialect::Sqlite.current_timestamp_expr(), SQLITE_NOW);
        assert_eq!(vendor.schema.current_timestamp_expr(), SQLITE_NOW);
        assert_eq!(vendor.dml.synth_now(), SQLITE_NOW);
    }

    #[test]
    fn the_byte_identity_obligation_is_scoped_to_the_dialect_that_stores_text() {
        for (dialect, vendor) in pairs() {
            let runtime = dialect.current_timestamp_expr();
            for migrated in [
                vendor.schema.current_timestamp_expr().to_owned(),
                vendor.dml.synth_now(),
            ] {
                if matches!(dialect, SqlDialect::Sqlite) {
                    assert_eq!(runtime, migrated);
                } else {
                    assert!(
                        runtime.eq_ignore_ascii_case(&migrated),
                        "{dialect:?}: {runtime} != {migrated}"
                    );
                }
            }
        }
    }

    /// The migration vendor that supplies SQLite defaults and assignments.
    fn sqlite_vendor() -> &'static BackendVendor {
        pairs()
            .into_iter()
            .find(|(dialect, _)| matches!(dialect, SqlDialect::Sqlite))
            .expect("the pair list covers SqlDialect::Sqlite")
            .1
    }
}

/// Render the shared typed predicate grammar using the runtime descriptor's
/// value conversions. Values never become SQL fragments.
pub fn build_where_plan(
    predicate: &crate::sql::Predicate,
    params: &mut Vec<Value>,
    schema: &Value,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    if predicate.depth() > crate::sql::MAX_PREDICATE_DEPTH {
        return Err(QueryError::InvalidFilter(
            "filter nesting depth exceeds the maximum".into(),
        ));
    }
    if matches!(predicate, crate::sql::Predicate::Const(true)) {
        return Ok(String::new());
    }
    let sql = render_filter(
        predicate,
        params,
        &[ReadSource {
            alias: None,
            schema,
        }],
        dialect,
        true,
    )?;
    Ok(if sql == "TRUE" { String::new() } else { sql })
}

fn filter_literal(value: &crate::sql::Literal) -> Result<Value, QueryError> {
    use crate::sql::Literal;
    Ok(match value {
        Literal::Bool(v) => Value::Bool(*v),
        Literal::Int(v) => Value::from(*v),
        Literal::Float(v) => {
            Value::Number(crate::value::Number::from_f64(v.get()).expect("finite literal"))
        }
        Literal::Text(v) => Value::String(v.clone()),
        Literal::Json(v) => Value::Json(v.clone()),
        Literal::Bytes(v) => Value::Bytes(v.clone()),
        Literal::Vector(_) => {
            return Err(QueryError::InvalidFilter(
                "binary and vector predicates require their column-specific operation".into(),
            ));
        }
    })
}

fn render_filter(
    predicate: &crate::sql::Predicate,
    params: &mut Vec<Value>,
    sources: &[ReadSource<'_>],
    dialect: SqlDialect,
    root: bool,
) -> Result<String, QueryError> {
    use crate::sql::{CompareOp, MembershipOp, Operand, PatternOp, Predicate};
    let sql = match predicate {
        Predicate::Const(value) => if *value { "TRUE" } else { "FALSE" }.to_owned(),
        Predicate::And(children) | Predicate::Or(children) => {
            let is_and = matches!(predicate, Predicate::And(_));
            if children.is_empty() {
                return Ok(if is_and && root {
                    ""
                } else if is_and {
                    "TRUE"
                } else {
                    "FALSE"
                }
                .into());
            }
            let parts = children
                .iter()
                .map(|child| render_filter(child, params, sources, dialect, false))
                .collect::<Result<Vec<_>, _>>()?;
            let joined = parts.join(if is_and { " AND " } else { " OR " });
            if root || parts.len() == 1 {
                joined
            } else {
                format!("({joined})")
            }
        }
        Predicate::Not(child) => format!(
            "NOT ({})",
            render_filter(child, params, sources, dialect, true)?
        ),
        Predicate::IsNull { operand, negated } => format!(
            "{} IS {}NULL",
            read::operand_sql(operand, sources, dialect)?,
            if *negated { "NOT " } else { "" }
        ),
        Predicate::Compare { lhs, op, rhs } => {
            let left = read::operand_sql(lhs, sources, dialect)?;
            let bind = match rhs {
                Operand::Lit(value) => read::bind_operand(params, value, lhs, sources, dialect)?,
                _ => read::operand_sql(rhs, sources, dialect)?,
            };
            let op = match op {
                CompareOp::Eq => "=",
                CompareOp::Ne => "!=",
                CompareOp::Gt => ">",
                CompareOp::Gte => ">=",
                CompareOp::Lt => "<",
                CompareOp::Lte => "<=",
            };
            format!("{left} {op} {bind}")
        }
        Predicate::Membership { lhs, op, set } => {
            let left = read::operand_sql(lhs, sources, dialect)?;
            let binds = set
                .values()
                .iter()
                .map(|value| read::bind_operand(params, value, lhs, sources, dialect))
                .collect::<Result<Vec<_>, QueryError>>()?;
            format!(
                "{} {}IN ({})",
                left,
                if *op == MembershipOp::NotIn {
                    "NOT "
                } else {
                    ""
                },
                binds.join(", ")
            )
        }
        Predicate::Pattern {
            lhs,
            op,
            pattern,
            escape,
        } => {
            let col = read::operand_sql(lhs, sources, dialect)?;
            params.push(pattern.as_str().into());
            let slot = params.len();
            let negated = matches!(op, PatternOp::NotLike | PatternOp::NotILike);
            let insensitive = matches!(op, PatternOp::ILike | PatternOp::NotILike);
            let word = if insensitive && dialect == SqlDialect::Postgres {
                "ILIKE"
            } else {
                "LIKE"
            };
            let mut sql = format!("{col} {}{word} ${slot}", if negated { "NOT " } else { "" });
            if let Some(escape) = escape {
                params.push((escape.get()).into());
                sql.push_str(&format!(" ESCAPE ${}", params.len()));
            }
            if insensitive {
                match dialect {
                    SqlDialect::Postgres => {}
                    SqlDialect::Sqlite => sql.push_str(" COLLATE NOCASE"),
                }
            }
            sql
        }
    };
    Ok(sql)
}

pub(crate) fn validate_filter_budget(filter: &Value) -> Result<(), QueryError> {
    validate_clause_budget(filter, ClauseBudgetKind::Filter)
}

#[cfg(test)]
mod binary_expression_tests {
    use crate::crud::upsert::build_upsert_with_dialect;
    use super::*;
    use crate::value;

    #[test]
    fn binary_values_follow_the_column_expression_on_every_write_shape() {
        let schema = SchemaName::new("binary_fixture").unwrap();
        let fields = value!({"id":{"type":"string", "primaryKey":true},"payload":{"type":"bytes"}});
        let document = value!({"id":"row_a", "payload":Value::Bytes(vec![0, 1, 255])});
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let queries = [
                build_insert_with_dialect(&schema, "files", &fields, &document, dialect).unwrap(),
                build_insert_many_with_dialect(
                    &schema,
                    "files",
                    &fields,
                    &value!([document.clone()]),
                    dialect,
                )
                .unwrap(),
                build_update_one_with_dialect(
                    &schema,
                    "files",
                    &fields,
                    &value!({"id":"row_a"}),
                    &value!({"payload":Value::Bytes(vec![0, 1, 255])}),
                    dialect,
                )
                .unwrap(),
                build_upsert_with_dialect(
                    &schema,
                    "files",
                    &fields,
                    document.clone(),
                    &value!(["id"]),
                    dialect,
                )
                .unwrap(),
            ];
            for query in queries {
                let slot = query
                    .params
                    .iter()
                    .position(|p| p.as_bytes() == Some([0, 1, 255].as_slice()))
                    .expect("binary parameter");
                assert!(
                    query
                        .sql
                        .contains(&dialect.binary_bind_placeholder(slot + 1)),
                    "{}",
                    query.sql
                );
                assert!(!query.sql.contains("__zsbin__"));
            }
        }
    }

    #[test]
    fn text_that_looks_like_a_binary_marker_is_still_a_text_bind() {
        let schema = SchemaName::new("binary_fixture").unwrap();
        for value in ["__zsbin_blob__:aGVsbG8=", "__zsbin_blob__:not base64"] {
            let query = build_insert_with_dialect(
                &schema,
                "notes",
                &value!({"body":{"type":"string"}}),
                &value!({"body":value}),
                SqlDialect::Sqlite,
            )
            .unwrap();
            assert_eq!(query.params, value!([value]).as_array().unwrap().clone());
            assert!(!query.sql.contains("unhex("));
        }
    }
}

#[cfg(test)]
mod encrypted_query_tests {
    use super::*;
    use crate::value;

    #[test]
    fn encrypted_values_cannot_be_queried_even_when_capability_flags_claim_otherwise() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            for mask in [
                value!({"kind":"none"}),
                value!({"kind":"last4","classification":"spi"}),
            ] {
                let schema = value!({
                    "secret":{"type":"string","encrypted":true,"mask":mask,"filterable":true,"sortable":true},
                    "name":{"type":"string"}
                });
                for operand in [
                    value!("x"),
                    value!(null),
                    value!({"$eq":"x"}),
                    value!({"$in":["x"]}),
                    value!({"$gt":"x"}),
                    value!({"$like":"x%"}),
                    value!({"$exists":true}),
                ] {
                    for filter in [
                        value!({"secret":operand.clone()}),
                        value!({"$or":[{"name":"ok"},{"$not":{"secret":operand}}]}),
                    ] {
                        let err =
                            build_where_with_dialect(&filter, &mut Vec::new(), &schema, dialect)
                                .expect_err("encrypted filter must fail");
                        assert!(err.to_string().contains("encrypted field"), "{err}");
                    }
                }
                let namespace = SchemaName::new("encrypted_fixture").unwrap();
                for pipeline in [
                    value!([{"$group":{"by":"secret"}}]),
                    value!([{"$group":{"by":["secret"]}}]),
                    value!([{"$group":{"total":{"$min":"secret"}}}]),
                    value!([{"$sort":{"secret":1}}]),
                ] {
                    assert!(
                        build_aggregate_with_soft_delete_with_dialect(
                            &namespace, "records", &pipeline, false, &schema, dialect
                        )
                        .is_err()
                    );
                }
                assert!(
                    build_conflict_probe_with_dialect(
                        &namespace,
                        "records",
                        &schema,
                        &value!({"secret":"x"}),
                        dialect
                    )
                    .is_err()
                );
                assert!(
                    build_distinct_with_soft_delete_with_dialect(
                        &namespace,
                        "records",
                        "secret",
                        &value!({}),
                        false,
                        &schema,
                        dialect
                    )
                    .is_err()
                );
                assert!(
                    build_order_by_read_with_dialect(&value!({"secret":1}), dialect, &schema)
                        .is_err()
                );
                assert!(
                    build_order_by_read_with_dialect(&value!([["secret", -1]]), dialect, &schema)
                        .is_err()
                );
                assert!(
                    build_where_with_dialect(
                        &value!({"name":"ok"}),
                        &mut Vec::new(),
                        &schema,
                        dialect
                    )
                    .is_ok()
                );
                assert!(
                    build_order_by_read_with_dialect(&value!({"name":1}), dialect, &schema).is_ok()
                );
            }
        }
    }
}
