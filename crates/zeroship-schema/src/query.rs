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

// Every DERIVED identifier in this module goes through this one function. See
// `crate::ident` for why capping (rather than refusing) is right for derived
// names, and why a second copy of the cap is the failure mode to avoid.
use crate::ident::cap_ident_name;

/// Errors from query building.
#[derive(Debug)]
pub enum QueryError {
    /// Unsupported or malformed filter.
    InvalidFilter(String),
    /// Collection name is invalid.
    InvalidCollection(String),
    /// Malformed identifier in a structured input (e.g. named index name or
    /// field reference). Carries a path-keyed message so the SDK can surface
    /// it back to the user without losing the offending input.
    InvalidIdent(String),
    /// Creator declared a field whose name collides with one
    /// of the seven platform-managed system fields (`id`, `created_at`,
    /// `updated_at`, `created_by`, `updated_by`, `version`, `deleted_at`).
    /// Distinct from [`QueryError::InvalidIdent`] so the SDK can surface a typed code
    /// (`reserved_system_field_name`) that's distinguishable from the
    /// generic `invalid_identifier` thrown by the `_*` / `__zs_*` prefix
    /// reservations. Filter-time use of these names is unrestricted
    /// (`db.users.find({ id: ... })` is the canonical query shape); the
    /// fence only fires on declaration paths (`field_to_column`).
    ReservedSystemFieldName(String),
    /// Creator UPDATE patch attempted to overwrite one of
    /// the three write-once system fields (`id`, `created_at`,
    /// `created_by`). These are auto-populated at INSERT and
    /// immutable thereafter. The carried string names the offending
    /// field for the SDK error envelope. Distinct from
    /// `ReservedSystemFieldName` (which fires only at declaration
    /// time): this fires at UPDATE-patch validation, NOT on filter
    /// reads (`update({ id: ... }, ...)` is fine — the filter
    /// references id; only the PATCH side is fenced).
    ImmutableSystemField(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFilter(msg) => write!(f, "invalid filter: {msg}"),
            Self::InvalidCollection(msg) => write!(f, "invalid collection: {msg}"),
            Self::InvalidIdent(msg) => write!(f, "invalid identifier: {msg}"),
            Self::ReservedSystemFieldName(msg) => {
                write!(f, "reserved system field name: {msg}")
            }
            Self::ImmutableSystemField(msg) => {
                write!(f, "immutable system field: {msg}")
            }
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

/// SQL dialect tag for the small set of build sites
/// whose BINARY-BIND placeholder shape diverges between PG and
/// SQLite.
///
/// A *binary-bind* column is one whose parameter travels through the
/// text-mode param list as base64 but must reach the database as RAW
/// BYTES. Two write passes produce them, and the mechanism does not
/// care which: `crud::encryption_pass` (base64 ciphertext for a
/// `t.encrypted(...)` column) and `crud::bytes_pass` (the base64 wire
/// string the SDK exchanges for a plain `t.bytes()` column). Both mark
/// the column with a `__zsbin__<col>` sibling key; this enum decides
/// how the marked placeholder is spelled.
///
/// PG uses `decode($N, 'base64')::bytea` so the BYTEA column receives
/// raw bytes from a base64-encoded text param. SQLite's text-mode bind
/// layer cannot represent BLOBs through a `&str` param: we emit a
/// plain `$N` placeholder and tag the param value with the
/// [`SQLITE_BINARY_BIND_PREFIX`] sentinel; the SQLite session actor
/// recognises the sentinel and binds raw `Vec<u8>` (BLOB) instead of
/// text. Unmarked parameters travel as plain `String` on both arms.
///
/// PG-side behaviour always emits the
/// `decode($N, 'base64')::bytea` SQL fragment, and the
/// sentinel-prefix is never produced on the PG arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    /// Postgres dialect: binary-bind columns wrap the placeholder
    /// with `decode($N, 'base64')::bytea` so the BYTEA column receives
    /// raw bytes from the base64 text param. This is the dialect every
    /// Postgres build site emits.
    Postgres,
    /// SQLite dialect: binary-bind columns emit `$N` and the param
    /// value is tagged with the [`SQLITE_BINARY_BIND_PREFIX`] sentinel so
    /// the session actor binds raw bytes (BLOB) instead of text.
    Sqlite,
    /// MySQL dialect: binary-bind columns wrap the placeholder with
    /// `FROM_BASE64(?)` so the LONGBLOB column receives raw bytes from the
    /// base64 text param. This dialect is render-only; no live MySQL runtime/backend
    /// constructs it for execution.
    Mysql,
}

/// Dialect-specific schema/DDL spelling.
///
/// This trait deliberately has no default methods: adding a third dialect must
/// provide every spelling explicitly. The single exhaustive dispatch match lives
/// in [`renderer`], so a new [`SqlDialect`] variant breaks there at compile time
/// and forces the missing renderer to be wired before the crate can build.
pub trait SchemaRenderer {
    fn dialect(&self) -> SqlDialect;
    fn binary_bind_placeholder(&self, n: usize) -> String;
    fn wrap_binary_bind_param(&self, b64_value: String) -> String;
    fn system_field_columns(&self) -> Vec<String>;
    fn system_field_indexes(
        &self,
        app_id: &str,
        collection: &str,
        sqlite_scope: SqliteEmitScope,
    ) -> Vec<String>;
    fn foreign_key_target(&self, app_id: &str, target: &str) -> String;
    fn column_type(&self, def: &serde_json::Value) -> String;
    fn json_object_default(&self) -> String;
    fn json_array_default(&self) -> String;
    fn current_timestamp_expr(&self) -> &'static str;
    fn column_comment_statements(
        &self,
        app_id: &str,
        collection: &str,
        schema: &serde_json::Value,
    ) -> Vec<String>;
    fn canonical_type(&self, raw: &str) -> String;
}

struct PostgresSchemaRenderer;
struct SqliteSchemaRenderer;
struct MysqlSchemaRenderer;

static POSTGRES_SCHEMA_RENDERER: PostgresSchemaRenderer = PostgresSchemaRenderer;
static SQLITE_SCHEMA_RENDERER: SqliteSchemaRenderer = SqliteSchemaRenderer;
static MYSQL_SCHEMA_RENDERER: MysqlSchemaRenderer = MysqlSchemaRenderer;

/// Return the schema renderer for a dialect.
///
/// This is the only schema-crate `SqlDialect` dispatch match for renderer
/// selection. Adding a third dialect intentionally breaks this match until that
/// dialect's renderer is implemented and wired.
pub fn renderer(dialect: SqlDialect) -> &'static dyn SchemaRenderer {
    match dialect {
        SqlDialect::Postgres => &POSTGRES_SCHEMA_RENDERER,
        SqlDialect::Sqlite => &SQLITE_SCHEMA_RENDERER,
        SqlDialect::Mysql => &MYSQL_SCHEMA_RENDERER,
    }
}

impl SqlDialect {
    /// Build the placeholder SQL fragment for a binary-bind column's
    /// parameter at position `n` (1-indexed). PG wraps the placeholder
    /// in a `decode(...)::bytea` cast; SQLite emits a bare `$N`.
    pub fn binary_bind_placeholder(self, n: usize) -> String {
        renderer(self).binary_bind_placeholder(n)
    }

    /// Wrap a binary-bind column's base64 param value with the
    /// dialect-appropriate side-channel. PG returns the value
    /// unchanged (it is decoded by the SQL fragment from
    /// [`SqlDialect::binary_bind_placeholder`]); SQLite
    /// prepends [`SQLITE_BINARY_BIND_PREFIX`] so the session actor can
    /// route the param through a binary bind.
    pub fn wrap_binary_bind_param(self, b64_value: String) -> String {
        renderer(self).wrap_binary_bind_param(b64_value)
    }
}

impl SchemaRenderer for PostgresSchemaRenderer {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }

    fn binary_bind_placeholder(&self, n: usize) -> String {
        format!("decode(${n}, 'base64')::bytea")
    }

    fn wrap_binary_bind_param(&self, b64_value: String) -> String {
        b64_value
    }

    fn system_field_columns(&self) -> Vec<String> {
        let (ts_type, ts_default) = ("TIMESTAMPTZ", "NOW()");
        vec![
            "id TEXT PRIMARY KEY".to_string(),
            format!("created_at {ts_type} NOT NULL DEFAULT {ts_default}"),
            format!("updated_at {ts_type} NOT NULL DEFAULT {ts_default}"),
            "created_by TEXT NULL".to_string(),
            "updated_by TEXT NULL".to_string(),
            "version INTEGER NOT NULL DEFAULT 1".to_string(),
            format!("deleted_at {ts_type} NULL"),
        ]
    }

    fn system_field_indexes(
        &self,
        app_id: &str,
        collection: &str,
        _sqlite_scope: SqliteEmitScope,
    ) -> Vec<String> {
        const SYSTEM_INDEXED_COLS: &[&str] = &["deleted_at", "updated_at", "created_by"];
        SYSTEM_INDEXED_COLS
            .iter()
            .map(|col| {
                let idx_name = index_name(collection, &[col], /* unique = */ false);
                format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {}.{} ({})",
                    quote_ident(&idx_name),
                    quote_ident(app_id),
                    quote_ident(collection),
                    quote_ident(col),
                )
            })
            .collect()
    }

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String {
        format!("{}.{}", quote_ident(app_id), quote_ident(target))
    }

    fn column_type(&self, def: &serde_json::Value) -> String {
        if def.get("encrypted").is_some() {
            return "BYTEA".to_string();
        }

        let zs_type = def.get("type").and_then(|t| t.as_str());

        if zs_type == Some("vector") {
            let dims = def
                .get("vectorDims")
                .and_then(serde_json::Value::as_i64)
                .filter(|d| *d > 0 && *d <= 16000)
                .unwrap_or(0);
            if dims > 0 {
                return format!("vector({dims})");
            }
            return "vector".to_string();
        }

        if zs_type == Some("geoPoint") {
            return "geography(POINT, 4326)".to_string();
        }

        if zs_type == Some("char") {
            if let Some(len) = char_len(def) {
                return format!("character({len})");
            }
        }

        def_to_pg_type(def).to_string()
    }

    fn json_object_default(&self) -> String {
        "DEFAULT '{}'::jsonb".to_string()
    }

    fn json_array_default(&self) -> String {
        "DEFAULT '[]'::jsonb".to_string()
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "NOW()"
    }

    fn column_comment_statements(
        &self,
        app_id: &str,
        collection: &str,
        schema: &serde_json::Value,
    ) -> Vec<String> {
        let mut statements = build_mask_sentinel_comments(app_id, collection, schema);
        statements.extend(build_encryption_sentinel_comments(app_id, collection, schema));
        statements
    }

    fn canonical_type(&self, raw: &str) -> String {
        raw.to_string()
    }
}

impl SchemaRenderer for SqliteSchemaRenderer {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }

    fn binary_bind_placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    fn wrap_binary_bind_param(&self, b64_value: String) -> String {
        format!("{SQLITE_BINARY_BIND_PREFIX}{b64_value}")
    }

    fn system_field_columns(&self) -> Vec<String> {
        let (ts_type, ts_default) = ("TEXT", "CURRENT_TIMESTAMP");
        vec![
            "id TEXT PRIMARY KEY".to_string(),
            format!("created_at {ts_type} NOT NULL DEFAULT {ts_default}"),
            format!("updated_at {ts_type} NOT NULL DEFAULT {ts_default}"),
            "created_by TEXT NULL".to_string(),
            "updated_by TEXT NULL".to_string(),
            "version INTEGER NOT NULL DEFAULT 1".to_string(),
            format!("deleted_at {ts_type} NULL"),
        ]
    }

    fn system_field_indexes(
        &self,
        app_id: &str,
        collection: &str,
        sqlite_scope: SqliteEmitScope,
    ) -> Vec<String> {
        const SYSTEM_INDEXED_COLS: &[&str] = &["deleted_at", "updated_at", "created_by"];
        SYSTEM_INDEXED_COLS
            .iter()
            .map(|col| {
                let idx_name = index_name(collection, &[col], /* unique = */ false);
                if sqlite_scope == SqliteEmitScope::MainUnqualified {
                    format!(
                        "CREATE INDEX IF NOT EXISTS {} ON {} ({})",
                        quote_ident(&idx_name),
                        quote_ident(collection),
                        quote_ident(col),
                    )
                } else {
                    format!(
                        "CREATE INDEX IF NOT EXISTS {}.{} ON {} ({})",
                        quote_ident(app_id),
                        quote_ident(&idx_name),
                        quote_ident(collection),
                        quote_ident(col),
                    )
                }
            })
            .collect()
    }

    fn foreign_key_target(&self, _app_id: &str, target: &str) -> String {
        quote_ident(target)
    }

    fn column_type(&self, def: &serde_json::Value) -> String {
        if def.get("encrypted").is_some() {
            return "BLOB".to_string();
        }

        let zs_type = def.get("type").and_then(|t| t.as_str());

        if zs_type == Some("vector") {
            return "BLOB".to_string();
        }

        if zs_type == Some("geoPoint") {
            return "BLOB".to_string();
        }

        match zs_type {
            Some("string") => "TEXT".to_string(),
            Some("char") => "TEXT".to_string(),
            Some("number") => "REAL".to_string(),
            Some("real") => "REAL".to_string(),
            Some("boolean") => "INTEGER".to_string(),
            Some("date") => "TEXT".to_string(),
            Some("calendarDate") => "TEXT".to_string(),
            Some("json") | Some("object") | Some("array") | Some("union") => {
                "TEXT".to_string()
            }
            Some("textArray") => "TEXT".to_string(),
            Some("ref") => "TEXT".to_string(),
            Some("literal") => match def.get("literalValue") {
                Some(serde_json::Value::Number(_)) => "NUMERIC".to_string(),
                Some(serde_json::Value::Bool(_)) => "INTEGER".to_string(),
                _ => "TEXT".to_string(),
            },
            Some("bigint") | Some("int8") | Some("integer") | Some("int") | Some("int4") => {
                "INTEGER".to_string()
            }
            Some("smallInt") => "INTEGER".to_string(),
            Some("inet") => "TEXT".to_string(),
            _ => "TEXT".to_string(),
        }
    }

    fn json_object_default(&self) -> String {
        "DEFAULT '{}'".to_string()
    }

    fn json_array_default(&self) -> String {
        "DEFAULT '[]'".to_string()
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "CURRENT_TIMESTAMP"
    }

    fn column_comment_statements(
        &self,
        _app_id: &str,
        _collection: &str,
        _schema: &serde_json::Value,
    ) -> Vec<String> {
        Vec::new()
    }

    fn canonical_type(&self, raw: &str) -> String {
        sqlite_canonical_type(raw).to_string()
    }
}

impl SchemaRenderer for MysqlSchemaRenderer {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Mysql
    }

    fn binary_bind_placeholder(&self, _n: usize) -> String {
        "FROM_BASE64(?)".to_string()
    }

    fn wrap_binary_bind_param(&self, b64_value: String) -> String {
        b64_value
    }

    fn system_field_columns(&self) -> Vec<String> {
        let (ts_type, ts_default) = ("DATETIME(6)", "CURRENT_TIMESTAMP(6)");
        vec![
            "`id` VARCHAR(191) PRIMARY KEY".to_string(),
            format!("`created_at` {ts_type} NOT NULL DEFAULT {ts_default}"),
            format!("`updated_at` {ts_type} NOT NULL DEFAULT {ts_default}"),
            "`created_by` VARCHAR(191) NULL".to_string(),
            "`updated_by` VARCHAR(191) NULL".to_string(),
            "`version` INT NOT NULL DEFAULT 1".to_string(),
            format!("`deleted_at` {ts_type} NULL"),
        ]
    }

    fn system_field_indexes(
        &self,
        app_id: &str,
        collection: &str,
        _sqlite_scope: SqliteEmitScope,
    ) -> Vec<String> {
        const SYSTEM_INDEXED_COLS: &[&str] = &["deleted_at", "updated_at", "created_by"];
        SYSTEM_INDEXED_COLS
            .iter()
            .map(|col| {
                let idx_name = index_name(collection, &[col], /* unique = */ false);
                format!(
                    "CREATE INDEX {} ON {}.{} ({})",
                    mysql_quote_ident(&idx_name),
                    mysql_quote_ident(app_id),
                    mysql_quote_ident(collection),
                    mysql_quote_ident(col),
                )
            })
            .collect()
    }

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String {
        format!("{}.{}", mysql_quote_ident(app_id), mysql_quote_ident(target))
    }

    fn column_type(&self, def: &serde_json::Value) -> String {
        if def.get("encrypted").is_some() {
            return "LONGBLOB".to_string();
        }

        if let Some(values) = mysql_native_enum_values(def) {
            return format!("ENUM({})", values.join(", "));
        }

        let zs_type = def.get("type").and_then(|t| t.as_str());

        if zs_type == Some("vector") {
            return "BLOB".to_string();
        }

        if zs_type == Some("geoPoint") {
            return "POINT SRID 4326".to_string();
        }

        match zs_type {
            Some("string") => {
                let max = def
                    .get("maxLength")
                    .or_else(|| def.get("max"))
                    .and_then(serde_json::Value::as_u64)
                    .filter(|n| *n > 0 && *n <= 65_535);
                match max {
                    Some(n) if n <= 16_383 => format!("VARCHAR({n})"),
                    Some(_) => "LONGTEXT".to_string(),
                    None => "VARCHAR(191)".to_string(),
                }
            }
            Some("char") => match char_len(def) {
                Some(len) => format!("CHAR({len})"),
                None => "CHAR(1)".to_string(),
            },
            Some("number") => "DOUBLE".to_string(),
            Some("real") => "FLOAT".to_string(),
            Some("boolean") => "TINYINT(1)".to_string(),
            Some("date") => "DATETIME(6)".to_string(),
            Some("calendarDate") => "DATE".to_string(),
            Some("json") | Some("object") | Some("array") | Some("union") => "JSON".to_string(),
            Some("textArray") => "JSON".to_string(),
            Some("ref") => "VARCHAR(191)".to_string(),
            Some("bytes") => "LONGBLOB".to_string(),
            Some("literal") => match def.get("literalValue") {
                Some(serde_json::Value::Number(_)) => "DECIMAL(65, 30)".to_string(),
                Some(serde_json::Value::Bool(_)) => "TINYINT(1)".to_string(),
                _ => "VARCHAR(191)".to_string(),
            },
            Some("bigInt") | Some("bigint") | Some("int8") => "BIGINT".to_string(),
            Some("integer") | Some("int") | Some("int4") => "INT".to_string(),
            Some("smallInt") => "SMALLINT".to_string(),
            Some("inet") => "VARCHAR(43)".to_string(),
            _ => "VARCHAR(191)".to_string(),
        }
    }

    fn json_object_default(&self) -> String {
        "DEFAULT (JSON_OBJECT())".to_string()
    }

    fn json_array_default(&self) -> String {
        "DEFAULT (JSON_ARRAY())".to_string()
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "CURRENT_TIMESTAMP(6)"
    }

    fn column_comment_statements(
        &self,
        _app_id: &str,
        _collection: &str,
        _schema: &serde_json::Value,
    ) -> Vec<String> {
        Vec::new()
    }

    fn canonical_type(&self, raw: &str) -> String {
        mysql_canonical_type(raw)
    }
}

#[cfg(test)]
mod schema_renderer_tests {
    use super::*;

    #[test]
    fn dispatch_returns_expected_schema_renderer() {
        assert_eq!(renderer(SqlDialect::Postgres).dialect(), SqlDialect::Postgres);
        assert_eq!(renderer(SqlDialect::Sqlite).dialect(), SqlDialect::Sqlite);
        assert_eq!(renderer(SqlDialect::Mysql).dialect(), SqlDialect::Mysql);
    }
}

/// Sentinel prefix the SQLite session uses to recognise a binary-bind
/// param that must be base64-decoded and bound as BLOB. The
/// prefix is deliberately long + improbable: a base64 payload contains
/// only `[A-Za-z0-9+/=]`, never `_` or `:`, so this prefix can never
/// collide with a real base64 value a write pass produces.
///
/// The SQLite session strips the prefix and base64-decodes the
/// remainder; PG never sees a value with this prefix because
/// [`SqlDialect::wrap_binary_bind_param`] is a no-op on the PG arm.
pub const SQLITE_BINARY_BIND_PREFIX: &str = "__zsbin_blob__:";

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
pub(crate) const PLATFORM_RESERVED_COLLECTION_PREFIXES: &[&str] = &["__zero_migrate", "__zeroship"];

/// Validate a collection name: alphanumeric + underscores only.
///
/// Additional security constraints (beyond character allowlist):
/// - Must not be empty.
/// - Must not exceed 63 bytes (Postgres `NAMEDATALEN` limit).
/// - Must not contain a null byte.
/// - Must not start with `pg_` (case-insensitive) — reserved for Postgres
///   system catalogs.
/// - Must not start with a platform-owned prefix (case-insensitive):
///   `__zero_migrate` or `__zeroship`.
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
    // .to_ascii_lowercase() per CRUD dispatch.
    let bytes = name.as_bytes();
    if bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"pg_") {
        return Err(QueryError::InvalidCollection(format!(
            "collection name '{name}' uses reserved prefix 'pg_' (Postgres system catalog)"
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

/// The seven platform-managed system fields.
///
/// Every creator table receives these at CREATE TABLE time;
/// creators cannot declare their own field with any of these
/// names. Filter-time use is unrestricted — `db.users.find({ id: "..." })`
/// is the canonical query shape.
///
/// This list is intentionally separate from [`RESERVED_NAMES`] because
/// the two categories enforce at different call sites:
///
/// - [`RESERVED_NAMES`] fires at BOTH schema-declaration time AND
///   filter time (e.g. `_masked` suffix, `_*` prefix). Synthetic /
///   sibling columns must never appear in user input at all.
/// - `SYSTEM_FIELD_NAMES` fires ONLY at schema-declaration time. The
///   names themselves (`id`, `created_at`, …) are the canonical
///   query keys creators use every day.
///
/// The reservation produces [`QueryError::ReservedSystemFieldName`]
/// (distinct from [`QueryError::InvalidIdent`]) so the SDK can branch
/// on a stable code (`reserved_system_field_name`).
pub const SYSTEM_FIELD_NAMES: &[&str] = &[
    "id",
    "created_at",
    "updated_at",
    "created_by",
    "updated_by",
    "version",
    "deleted_at",
];

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
    ReservedName::Prefix("sqlite_"),
    // Masked-column sibling suffix. The platform
    // emits `<col>_masked` siblings (Path B); creators must not
    // declare a column ending in `_masked` themselves. Refused at
    // both schema-registration time (in `field_to_column`) and
    // filter-time (so `db.users.find({ ssn_masked: ... })` is
    // refused with the same code path).
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

/// Validate a field (column) name used in DDL.
///
/// Postgres silently truncates identifiers longer than 63 bytes (NAMEDATALEN),
/// which would alias two distinct fields to the same column. Injection is
/// already blocked by `quote_ident`. The ASCII allowlist matches
/// [`validate_collection`]'s policy: a multi-byte identifier like `"café"`
/// is 4 chars / 5 bytes, and two distinct unicode-spelled fields could
/// collide on the same Postgres-truncated column if either side approached
/// the 63-byte ceiling. Enforcing ASCII-alphanumeric + underscore prevents
/// that whole class.
///
/// Also refuses any field name matching the
/// [`RESERVED_NAMES`] table (platform suffixes / prefixes / exact
/// names). The `_masked` suffix is reserved for Path B sibling
/// columns; the six default-classification names (`public`, `pii`,
/// `spi`, `phi`, `pci`, `internal`) are reserved at the column-name
/// level.
///
/// Note this function does NOT fence the seven
/// system-field names (`id`, `created_at`, `updated_at`, `created_by`,
/// `updated_by`, `version`, `deleted_at`). Those names are reserved
/// only at SCHEMA-DECLARATION time, not at filter time —
/// `db.users.find({ id: "..." })` is the canonical query shape and
/// must keep working. Declaration paths must call
/// [`validate_field_name_for_declaration`] instead of this function.
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
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
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
                ReservedName::Prefix(p) => format!(
                    "prefix '{p}' is reserved for platform-internal names"
                ),
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
    Ok(())
}

/// Declaration-time wrapper around [`validate_field_name`]
/// that additionally fences the seven platform-managed system field
/// names ([`SYSTEM_FIELD_NAMES`]).
///
/// Call this from every code path that translates a creator-declared
/// schema field into DDL (currently `field_to_column`). Filter-time
/// validators (`build_field_condition_with_dialect`, `build_vector_search`,
/// `build_spatial_near`) must continue to call the underlying
/// [`validate_field_name`] so creators can keep writing
/// `db.users.find({ id: "..." })`.
///
/// On reservation hit returns [`QueryError::ReservedSystemFieldName`]
/// — distinct from `InvalidIdent` so the SDK can branch on a stable
/// `reserved_system_field_name` code. The message names the offending
/// field; the hint enumerates all 7 system fields so the creator
/// knows the full reserved set without consulting docs.
pub fn validate_field_name_for_declaration(name: &str) -> Result<(), QueryError> {
    validate_field_name(name)?;
    if SYSTEM_FIELD_NAMES.contains(&name) {
        return Err(QueryError::ReservedSystemFieldName(format!(
            "Field name '{name}' is reserved for platform system fields. \
             System fields ({}) are managed by the platform and cannot be \
             overridden.",
            SYSTEM_FIELD_NAMES.join(", ")
        )));
    }
    Ok(())
}

/// Typed-id prefixes reserved for the platform. A creator-declared
/// `id: t.id("usr")` would mint ids that collide with platform user
/// ids (`crates/core/src/typed_id.rs`), so the prefix is rejected.
/// Only `usr` is reserved for now (matches the SDK-side fence in
/// `sdks/db/src/types.ts`).
pub const RESERVED_ID_PREFIXES: &[&str] = &["usr"];

/// Validate a creator-declared typed-id prefix (`t.id("blog")`).
///
/// Defense-in-depth mirror of the SDK-side check in
/// `sdks/db/src/types.ts`: the SDK throws at `pnpm dev` build time, but
/// a hand-built wire payload (a raw `default = { fetch }` deploy calling
/// `zeroship.db.registerModel` directly) skips the SDK entirely, so the
/// runtime re-validates at register-model.
///
/// Rules:
/// - must match `^[a-z][a-z0-9_]*$` → [`QueryError::InvalidIdent`]
/// - must not be a [`RESERVED_ID_PREFIXES`] entry → [`QueryError::ReservedSystemFieldName`]
///   (reuses the typed `reserved_system_field_name` SDK code; the prefix
///   collision is morally a system-field reservation).
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
        return Err(QueryError::ReservedSystemFieldName(format!(
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

/// **L24** — the read-identifier allowlist.
///
/// There is no permissive arm. Until this took a mandatory schema it raised
/// `InvalidIdent` only `if schema_hint.is_some()`, so a caller that had not yet
/// resolved a schema had EVERY field name accepted into `select` and `orderBy`
/// - including a mask sibling or any other internal physical column. The schema
/// is now a value the caller must already hold, so "not yet resolved" cannot be
/// expressed here at all.
fn validate_read_identifier(name: &str, schema_hint: &Value) -> Result<(), QueryError> {
    validate_field_name(name)?;
    if SYSTEM_FIELD_NAMES.contains(&name) || schema_declares_readable_field(schema_hint, name) {
        return Ok(());
    }
    Err(QueryError::InvalidIdent(format!(
        "field '{name}' is not a readable schema field; readable fields are declared schema fields plus the public system fields"
    )))
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
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a MySQL identifier with backticks.
/// Escapes any embedded backticks by doubling them.
pub fn mysql_quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn quote_ident_for_dialect(name: &str, dialect: SqlDialect) -> String {
    match dialect {
        SqlDialect::Postgres | SqlDialect::Sqlite => quote_ident(name),
        SqlDialect::Mysql => mysql_quote_ident(name),
    }
}

fn mysql_native_enum_values(def: &serde_json::Value) -> Option<Vec<String>> {
    let values = def.get("enum")?.as_array()?;
    let mut rendered = Vec::with_capacity(values.len());
    for value in values {
        let s = value.as_str()?;
        rendered.push(format!("'{}'", s.replace('\'', "''")));
    }
    if rendered.is_empty() {
        None
    } else {
        Some(rendered)
    }
}

// ---------------------------------------------------------------------------
// DDL builders for registerModel
// ---------------------------------------------------------------------------

/// Build CREATE SCHEMA IF NOT EXISTS for an app.
pub fn build_create_schema(app_id: &str) -> String {
    format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(app_id))
}

// `build_create_table` (the non-`_with_fks` wrapper that hardcoded
// `FkEmission::Inline`) was removed during the v2-only consolidation.
// Production paths (`zeroship_plugin_db::register_model::exec_register_model_with_pool`) always pass the
// orchestrator's live table set to `build_create_table_with_fks` so
// FKs to not-yet-created targets get deferred to a separate
// `ALTER TABLE … ADD CONSTRAINT`. Tests that need the legacy "always
// inline" behaviour call `build_create_table_with_fks(..., &Inline)`
// directly.

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

/// Controls **how the SQLite arm namespaces** the table / index targets it
/// emits. This is a SQLite-only concern — it has NO effect on the Postgres
/// arm, which always qualifies into the project schema (`"<schema>"."<table>"`).
///
/// Two different consumers ATTACH the app file under two different aliases, so
/// the SAME emitter must spell SQLite DDL two ways:
///
/// - [`SqliteEmitScope::AttachAlias`] — the **plugin-db runtime** ATTACHes each
///   app file under its `<app_id>` alias (`ATTACH … AS "<app_id>"`), so its DDL
///   is `"<app_id>"."<table>"` (table) and `"<app_id>"."<index>" ON "<table>"`
///   (index). This is the historical behaviour and the default for the
///   stable [`build_create_table_with_fks_for_dialect`] entry point.
/// - [`SqliteEmitScope::MainUnqualified`] — the **zeroship-migrate engine**'s
///   `SqliteBackend` ATTACHes the one app file as `main` (`main` IS the app
///   file), and its hardened authorizer DENIES any other alias. An unqualified
///   `CREATE TABLE users(...)` therefore lands in (and persists to) the app
///   file. A `"<app_id>"`-qualified statement would target a nonexistent alias
///   and be denied. So the engine MUST emit UNqualified DDL — that is this
///   variant: no schema qualifier on the table name OR the index name.
///
/// The Postgres arm ignores this enum entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteEmitScope {
    /// SQLite DDL is `"<app_id>"`-qualified (the plugin-db ATTACH-alias model).
    /// The default for the stable dialected entry point.
    AttachAlias,
    /// SQLite DDL is UNqualified — `main` IS the app file (the zeroship-migrate
    /// `SqliteBackend` model). The schema qualifier is dropped on the table
    /// name and the index name.
    MainUnqualified,
}

// pub (not pub(crate)): the external consumer is
// `crates/plugin-db/tests/integration.rs`, which reaches it through the glob
// re-export in `crates/plugin-db/src/lib.rs`. The crate prefix is load-bearing -
// written crate-relative this reads as belonging to zeroship-schema, which has
// no test by that name.
//
// PG-flavoured shim around
// [`build_create_table_with_fks_for_dialect`]. Every call site
// (orchestrator `zeroship_plugin_db::register_model::plan`, integration tests, internal
// query helpers) stays on this signature; the dialect-aware emitter
// lives behind the other symbol and routes the SQLite arm independently.
pub fn build_create_table_with_fks(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    fk_emit: &FkEmission<'_>,
) -> Result<String, QueryError> {
    build_create_table_with_fks_for_dialect(app_id, collection, schema, fk_emit, SqlDialect::Postgres)
}

/// Dialect-aware CREATE TABLE emitter.
///
/// Prepends the seven platform-managed system fields
/// ([`SYSTEM_FIELD_NAMES`]) before any user-declared columns and
/// appends three implicit B-tree indexes (`deleted_at`, `updated_at`,
/// `created_by`) as semicolon-separated `CREATE INDEX IF NOT EXISTS`
/// statements in the same multi-statement payload.
///
/// The dialect controls:
///
/// - timestamp column type: PG `TIMESTAMPTZ` / SQLite `TEXT`.
/// - default clause for `created_at` / `updated_at`: PG `NOW()` /
///   SQLite `CURRENT_TIMESTAMP`.
/// - index `ON` syntax: PG `ON <schema>.<table>` /
///   SQLite `<schema>.<index_name> ON <table>`.
/// - whether `COMMENT ON COLUMN` mask sentinels (PG only) are
///   appended; the SQLite arm drops them (the inline
///   `/* __zsmask:... */` comment on the sibling column is the
///   SQLite-side wire).
///
/// The `id TEXT PRIMARY KEY` is identical on both backends; the FK
/// column type cascades to `TEXT` so ref columns match the PK type —
/// see [`def_to_pg_type`].
pub fn build_create_table_with_fks_for_dialect(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    fk_emit: &FkEmission<'_>,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    // The stable entry point keeps the historical plugin-db namespacing: SQLite
    // DDL is `"<app_id>"`-qualified (the ATTACH-alias model). The zeroship-migrate
    // engine calls the `_scoped` form with `MainUnqualified` instead.
    build_create_table_with_fks_for_dialect_scoped(
        app_id,
        collection,
        schema,
        fk_emit,
        dialect,
        SqliteEmitScope::AttachAlias,
    )
}

/// Scope-aware variant of [`build_create_table_with_fks_for_dialect`]. Identical
/// in every respect except that the SQLite arm's table/index namespacing is
/// chosen by `sqlite_scope` (see [`SqliteEmitScope`]). The Postgres arm is
/// **byte-identical** regardless of `sqlite_scope` — the scope only flips the
/// SQLite qualifier.
///
/// The migrate engine's Confined SQLite path passes
/// [`SqliteEmitScope::MainUnqualified`] so the emitted DDL is UNqualified and
/// lands in `main` (= the app file) under the hardened authorizer (which denies
/// any non-`main` alias). The plugin-db runtime passes
/// [`SqliteEmitScope::AttachAlias`] (via the stable entry point) because it
/// ATTACHes the file under the `<app_id>` alias.
///
/// # Errors
/// Same as [`build_create_table_with_fks_for_dialect`].
pub fn build_create_table_with_fks_for_dialect_scoped(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    fk_emit: &FkEmission<'_>,
    dialect: SqlDialect,
    sqlite_scope: SqliteEmitScope,
) -> Result<String, QueryError> {
    // The canonical multi-statement payload is `;\n`-joined here; the STRUCTURAL
    // per-statement list (the migrate engine's guard-per-statement seam consumes
    // it directly, never re-splitting on a textual `;\n`) is exposed unchanged by
    // [`build_create_table_with_fks_for_dialect_scoped_statements`]. `join(";\n")`
    // over that list reproduces this string byte-for-byte.
    Ok(build_create_table_with_fks_for_dialect_scoped_statements(
        app_id,
        collection,
        schema,
        fk_emit,
        dialect,
        sqlite_scope,
    )?
    .join(";\n"))
}

/// **Structural** peer of [`build_create_table_with_fks_for_dialect_scoped`]:
/// returns the CREATE-TABLE payload as its individual statement list (the CREATE,
/// the implicit system-field `CREATE INDEX`es, and — on PG — the `COMMENT ON
/// COLUMN` mask/encryption sentinels) instead of the `;\n`-joined string.
///
/// `join(";\n")` over the returned vector is byte-identical to the joined form, so
/// the two entry points never diverge. The migrate engine's guard-per-statement
/// lower (the `zeroship_migrate` engine) consumes this list so a string-literal column
/// DEFAULT whose value itself contains `;\n` (e.g. `DEFAULT 'a;\nb'`) is NEVER
/// split mid-statement — the split is structural, not a textual `;\n` heuristic.
pub fn build_create_table_with_fks_for_dialect_scoped_statements(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    fk_emit: &FkEmission<'_>,
    dialect: SqlDialect,
    sqlite_scope: SqliteEmitScope,
) -> Result<Vec<String>, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    // SQLite `MainUnqualified` drops the schema qualifier entirely (`main` is the
    // app file); every other case keeps the `"<schema>"."<table>"` form. PG always
    // qualifies. `sqlite_table_unqualified` is true ONLY on the SQLite engine arm.
    let sqlite_table_unqualified =
        matches!(dialect, SqlDialect::Sqlite) && sqlite_scope == SqliteEmitScope::MainUnqualified;
    let table = if sqlite_table_unqualified {
        quote_ident(collection)
    } else {
        format!(
            "{}.{}",
            quote_ident_for_dialect(app_id, dialect),
            quote_ident_for_dialect(collection, dialect)
        )
    };

    let mut columns = build_system_field_columns(dialect);

    let mut deferred_fks: Vec<String> = Vec::new();
    let mut union_checks: Vec<String> = Vec::new();

    if let Some(obj) = schema.as_object() {
        for (field, def) in obj {
            // Skip top-level metadata keys (e.g.
            // `_meta`, `_indexes`). The `_` prefix is reserved for
            // synthetic-result columns at the field-name level
            // (`validate_field_name`), so these keys would otherwise
            // trip the validator; they are CRDT-like top-level
            // schema metadata rather than column declarations.
            if is_schema_metadata_key(field) {
                continue;
            }
            // `id: t.id("prefix")` is a PREFIX DECLARATION for
            // the system `id` PK column already emitted by
            // `build_system_field_columns`, NOT a second column. Skip it
            // so we neither duplicate the `id` column nor trip the
            // reserved-name fence in `validate_field_name_for_declaration`.
            // We still validate the declared `idPrefix` here (defense in
            // depth — mirrors the SDK fence so a hand-built wire payload
            // can't smuggle a reserved/malformed prefix past register-
            // model). A field named `id` with any OTHER type falls
            // through to `field_to_column_for_dialect`, which rejects it.
            if field == "id" && def.get("type").and_then(|t| t.as_str()) == Some("id") {
                if let Some(prefix) = def.get("idPrefix").and_then(|p| p.as_str()) {
                    validate_id_prefix(prefix)?;
                }
                continue;
            }
            // The declared type and the WHOLE constraint set
            // (`NOT NULL`, `DEFAULT`, range / literal / enum `CHECK`) travel
            // with the REAL value, which after the storage flip lives in
            // `__zs_raw__<col>` for a masked field and in the field's own
            // column otherwise. `field_to_column_for_dialect` picks the
            // physical name; it validates the LOGICAL one.
            let col_def = field_to_column_for_dialect(field, def, dialect)?;
            columns.push(col_def);

            // Masked-column emission. When the field carries a
            // `.mask({...})` declaration (or the auto-default mask attached
            // to `t.encrypted(...)` columns) AND the mask kind is NOT
            // `"none"`, the column with the field's OWN name holds the
            // pre-computed masked representation (e.g. `"***-**-6789"`) that
            // `crud::mask_pass` derives at INSERT/UPDATE time, and the real
            // value has already been emitted above under `__zs_raw__<col>`.
            //
            // The masked column is `TEXT` for every mask kind (full / last4 /
            // first4 / email / name / dateYear / dateDecade) — the union of
            // mask outputs is string-shaped — and carries NONE of the
            // declared constraints. That is not an oversight: a
            // `t.number().mask(...)` field's own column would refuse
            // `'***'` under `DOUBLE PRECISION`, and a
            // `t.string().enum([...]).mask(...)` field would refuse it under
            // `CHECK ("ssn" IN (...))`, so every write would fail. The
            // constraints belong to the value, and the value moved.
            //
            // Explicit `.mask({ kind: "none" })` opt-out → no second column.
            // The field's own column is the only storage site and holds the
            // real value, exactly as an unmasked column does.
            if raw_column_for_field(field, def).is_some() {
                // Attach a `/* __zsmask:kind=…, classification=… */` inline
                // comment to the MASKED column's DDL so the SQLite
                // introspector can recover the mask metadata from
                // `sqlite_master.sql`. PG ignores SQL comments at parse time,
                // so the introspector on the PG arm reads `pg_description`
                // populated by the `COMMENT ON COLUMN` statement emitted
                // alongside the table create (see `mask_sentinel_for_field`).
                let sentinel = mask_sentinel_for_field(def);
                let inline_comment = match &sentinel {
                    Some(s) => format!(" /* {s} */"),
                    None => String::new(),
                };
                columns.push(format!(
                    "{} TEXT{inline_comment}",
                    quote_ident_for_dialect(field, dialect)
                ));
            }

            // Append FOREIGN KEY clause when this is a ref. Inline
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
                        if let Ok(fk_clause) =
                            build_fk_clause(app_id, field, def, target, dialect)
                        {
                            deferred_fks.push(fk_clause);
                        }
                    }
                }
            }

            // Per-variant CHECK constraints for a flat-expanded
            // discriminated union. The SDK tags the discriminator
            // column with `discriminator: "__discriminator__"` and
            // attaches the full `variants` map; we emit one CHECK per
            // variant of the shape:
            //   CHECK (kind <> 'login' OR (userId IS NOT NULL AND ip IS NOT NULL))
            // so a row of a given discriminator value cannot store NULL
            // where the variant requires a value. The discriminator
            // column itself already gets `CHECK (kind IN (...))` via
            // the regular `enum` constraint emitted by
            // `def_to_constraints`, so we don't repeat the IN-list here.
            if def.get("discriminator").and_then(|v| v.as_str()) == Some("__discriminator__") {
                if let Some(variants) = def.get("variants").and_then(|v| v.as_array()) {
                    let constraint_clauses =
                        emit_union_variant_checks(collection, field, def, variants, dialect);
                    union_checks.extend(constraint_clauses);
                }
            }
        }
    }

    // `created_at` / `updated_at` are emitted as part of
    // the seven system-field prefix at the top of `columns`; the
    // legacy trailing emission is gone. See `build_system_field_columns`
    // for the canonical declaration order.

    // Defensive last-line-of-defence assertion. The
    // declaration-time validator in `field_to_column` (via
    // `validate_field_name_for_declaration`) already rejects creator
    // schemas that declare any of the seven system-field names; the
    // loop above propagates that error and returns before this
    // assertion runs. The assertion guards a future regression where
    // a creator-declared system field somehow makes it through the
    // schema-iteration loop without raising — under debug builds the
    // panic surfaces immediately; release builds tolerate the
    // duplicated declaration and let the engine raise a
    // `column "id" specified more than once` error.
    //
    // Scans the assembled `columns` vector (not the raw schema), so
    // the assertion measures the actual DDL output rather than
    // re-checking the input — catching any future emitter that adds
    // a column out-of-band (e.g. a sibling-column path that
    // accidentally lands on a system-field name).
    debug_assert!(
        {
            let mut seen = std::collections::HashSet::new();
            let mut ok = true;
            for col in &columns {
                // The column DDL starts with the quoted (or bareword)
                // identifier — first whitespace-delimited token. We
                // strip the leading `"` if present.
                let first = col.split_whitespace().next().unwrap_or("");
                let name = first.trim_matches('"').trim_matches('`');
                if SYSTEM_FIELD_NAMES.contains(&name) {
                    if !seen.insert(name.to_string()) {
                        ok = false;
                        break;
                    }
                }
            }
            ok
        },
        "build_create_table_with_fks_for_dialect: duplicate system-field \
         declaration in column list — the declaration-time validator \
         (validate_field_name_for_declaration) should have rejected a \
         creator-declared system field before reaching the DDL emitter. \
         Columns: {columns:?}",
    );

    // Append all FK clauses *after* the regular columns so the SQL reads
    // top-to-bottom in a natural order (columns, then constraints).
    columns.extend(deferred_fks);
    columns.extend(union_checks);

    // Append `COMMENT ON COLUMN` statements for every
    // sibling column carrying a mask sentinel. Multi-statement SQL is
    // accepted by `pool.query_text_params` (the underlying libpq
    // simple-query protocol) and by SQLite's `sqlite3_exec`. On the
    // SQLite arm `COMMENT ON COLUMN` is a syntax error — the
    // dialect-routing skips the `COMMENT ON COLUMN` append when
    // `dialect == Sqlite`; the inline `/* __zsmask:... */` comment
    // baked into the CREATE TABLE body is the SQLite-side wire (see
    // `mask_sentinel_for_field`).
    let create_table = format!(
        "CREATE TABLE IF NOT EXISTS {} (\n  {}\n)",
        table,
        columns.join(",\n  ")
    );

    // Append the three implicit B-tree indexes
    // (`deleted_at`, `updated_at`, `created_by`) as semicolon-
    // separated `CREATE INDEX IF NOT EXISTS` statements. Bound 1:1
    // to the table lifecycle — emitted here so a drop-table cascade
    // takes them with it (instead of tracking them as separate
    // `ChangeKind::AddIndex` diff ops).
    //
    // The index for `id` is not emitted (the PRIMARY KEY constraint
    // already builds an implicit unique index). The index for
    // `version` is not emitted (per §5 of the proposal —
    // `version` bumps on every UPDATE and an index would thrash).
    let system_index_stmts =
        build_system_field_indexes(app_id, collection, dialect, sqlite_scope);

    let mut statements: Vec<String> = vec![create_table];
    statements.extend(system_index_stmts);

    statements.extend(renderer(dialect).column_comment_statements(app_id, collection, schema));

    Ok(statements)
}

/// Render the `COMMENT ON COLUMN … 'zsenc:<mode>:<keyId>:<wraps>'`
/// statements for every `t.encrypted(...)` column in `schema` (PG only). The
/// comment BODY is built by the shared codec
/// ([`crate::mask_codec::build_encryption_sentinel`]) so it is byte-identical to
/// what the migration engine emits and what the runtime parser
/// ([`crate::mask_codec::parse_encryption_sentinel`], via `read_live_schema`)
/// expects. Returns the empty vector when no column is encrypted.
#[must_use]
pub fn build_encryption_sentinel_comments(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = schema.as_object() else {
        return out;
    };
    for (field, def) in obj {
        if is_schema_metadata_key(field) {
            continue;
        }
        // Reuse the single-source-of-truth body builder — no re-spelling.
        let Some(body) = encryption_sentinel_body_for_field(def) else {
            continue;
        };
        let escaped = body.replace('\'', "''");
        out.push(format!(
            "COMMENT ON COLUMN {}.{}.{} IS '{}'",
            quote_ident(app_id),
            quote_ident(collection),
            quote_ident(field),
            escaped,
        ));
    }
    out
}

/// Emit the seven platform-managed system-field column
/// declarations in canonical order ([`SYSTEM_FIELD_NAMES`]).
///
/// Order MUST match `SYSTEM_FIELD_NAMES`. The dialect controls
/// timestamp affinity (`TIMESTAMPTZ` on PG, `TEXT` on SQLite) and
/// the default expression (`NOW()` on PG, `CURRENT_TIMESTAMP` on
/// SQLite). `id`, `created_by`, `updated_by`, `version`, and the
/// `INTEGER` affinity for `version` are dialect-identical.
///
/// The `id` PK uses inline `PRIMARY KEY` (not a `CONSTRAINT ...`
/// table-level form) — matches the convention already used for
/// the legacy `id SERIAL PRIMARY KEY` line this replaces. The
/// existing FK-attachment logic (`build_fk_clause`) references
/// the `id` column by name, so the switch from `SERIAL` to `TEXT`
/// is transparent to the FK emitter (FK column TYPE narrowing
/// cascades separately).
fn build_system_field_columns(dialect: SqlDialect) -> Vec<String> {
    renderer(dialect).system_field_columns()
}

/// Emit the three implicit B-tree indexes the platform
/// auto-creates for every new table: `deleted_at` (soft-delete
/// filtering), `updated_at` (cursor-paged read paths), and
/// `created_by` (per-actor lookups + audit).
///
/// The PK on `id` covers `id` lookups via the implicit unique index;
/// `version` is not indexed (every UPDATE bumps it; the index would
/// thrash). See §5 of `docs/archive/platform-system-fields.md` for
/// the rationale.
///
/// Dialect controls the `ON` clause syntax:
///
/// - PG: `CREATE INDEX IF NOT EXISTS "<index>" ON "<schema>"."<table>" (<col>)`.
/// - SQLite: `CREATE INDEX IF NOT EXISTS "<schema>"."<index>" ON "<table>" (<col>)`
///   — SQLite places the schema on the index name, not the table.
///
/// The index name uses the existing [`index_name`] helper so the
/// `<table>_<col>_idx` shape stays consistent with the rest of the
/// auto-named per-field indexes and the NAMEDATALEN-safe 60-byte
/// truncation kicks in for long table names.
fn build_system_field_indexes(
    app_id: &str,
    collection: &str,
    dialect: SqlDialect,
    sqlite_scope: SqliteEmitScope,
) -> Vec<String> {
    renderer(dialect).system_field_indexes(app_id, collection, sqlite_scope)
}

/// Build an `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` statement.
///
/// Used by the diff engine when both tables already exist and the FK has
/// to be attached separately. The constraint name is content-addressed
/// from `<collection>_<field>_fkey` and truncated to 63 bytes via the
/// same hash strategy as index names.
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
    let fk_clause = build_fk_clause(app_id, field, def, target, SqlDialect::Postgres)?;
    Ok(format!("ALTER TABLE {} ADD {}", table, fk_clause))
}

/// Build `ALTER TABLE … DROP CONSTRAINT` for an existing FK (the diff
/// engine's `DropForeignKey` op).
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
    cap_ident_name(&format!("{field}_fkey"))
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
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    validate_collection(target)?;
    let constraint_name = fk_constraint_name(field, "");

    let on_delete =
        normalize_fk_action_for_dialect(def.get("onDelete").and_then(|v| v.as_str()), dialect);
    let on_update =
        normalize_fk_action_for_dialect(def.get("onUpdate").and_then(|v| v.as_str()), dialect);
    let deferrable = def
        .get("deferrable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let target_qualified = renderer(dialect).foreign_key_target(app_id, target);
    let deferrable_clause = if deferrable && !matches!(dialect, SqlDialect::Mysql) {
        " DEFERRABLE INITIALLY DEFERRED"
    } else {
        ""
    };

    let mut clause = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} (id)",
        quote_ident_for_dialect(&constraint_name, dialect),
        quote_ident_for_dialect(field, dialect),
        target_qualified,
    );
    if on_delete != "NO ACTION" {
        clause.push_str(" ON DELETE ");
        clause.push_str(on_delete);
    }
    if on_update != "NO ACTION" {
        clause.push_str(" ON UPDATE ");
        clause.push_str(on_update);
    }
    clause.push_str(deferrable_clause);
    Ok(clause)
}

/// Normalise an FK action to the SQL keyword form Postgres accepts.
fn normalize_fk_action_inner(s: Option<&str>) -> &'static str {
    match s.unwrap_or("no action").to_ascii_lowercase().as_str() {
        "cascade" => "CASCADE",
        "set null" | "set_null" | "setnull" => "SET NULL",
        "set default" | "set_default" | "setdefault" => "SET DEFAULT",
        "no action" | "no_action" | "noaction" => "NO ACTION",
        "restrict" => "RESTRICT",
        _ => "RESTRICT",
    }
}

/// Normalise an FK action; used cross-module by the diff engine.
pub fn normalize_fk_action(s: Option<&str>) -> &'static str {
    normalize_fk_action_inner(s)
}

/// Normalise an FK action for a dialect's canonical comparison/render form.
///
/// MySQL/InnoDB has no deferred constraint checks, so `RESTRICT` and
/// `NO ACTION` are the same immediate-reject default. Keep them distinct on
/// Postgres/SQLite, where the distinction is meaningful to their catalog/render
/// forms.
pub fn normalize_fk_action_for_dialect(
    s: Option<&str>,
    dialect: SqlDialect,
) -> &'static str {
    let action = normalize_fk_action_inner(s);
    if matches!(dialect, SqlDialect::Mysql) && matches!(action, "RESTRICT" | "NO ACTION") {
        "NO ACTION"
    } else {
        action
    }
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
    // The declared type and constraints go on the column that holds the REAL
    // value - `__zs_raw__<field>` when masked, the field's own name otherwise.
    let raw = raw_column_for_field(field, def);
    let physical = raw.clone().unwrap_or_else(|| field.to_string());
    let constraints = def_to_constraints(&physical, def);

    let mut sql = format!(
        "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {} {}",
        table,
        quote_ident(&physical),
        pg_type,
        constraints
    )
    .trim()
    .to_string();

    // When the field carries a `.mask({...})` declaration, also emit the
    // MASKED column - the one with the field's own name - as `TEXT NULL`, plus
    // the `COMMENT ON COLUMN` sentinel attachment, in the same multi-statement
    // payload. It is NULL here (versus the CREATE TABLE shape) because
    // existing rows would refuse the ALTER if it were NOT NULL; the backfill
    // populates it after every row has its mask computed.
    //
    // Note: this branch is taken ONLY when the diff classifier emits an
    // `AddColumn` for a fresh top-level field declared with `.mask({...})`.
    // The separate `MaskBackfill`-paired op the classifier emits for the
    // backfill passes a synthetic def carrying no mask block, so
    // `raw_column_for_field` returns `None` there and we do not double-emit.
    if raw.is_some() {
        sql.push_str(&format!(
            ";\nALTER TABLE {} ADD COLUMN IF NOT EXISTS {} TEXT NULL",
            table,
            quote_ident(field),
        ));
        if let Some(comment) =
            build_mask_sentinel_comment_for_field(app_id, collection, field, def)
        {
            sql.push_str(&format!(";\n{comment}"));
        }
    }

    Ok(sql)
}

// ---------------------------------------------------------------------------
// Index builders for registerModel. Materialises `t.string().index()` /
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
/// must not be retried — see the INVALID-index recovery path).
///
/// `kind` carries the index *shape* — B-tree (the default for
/// every call site), vector (pgvector / Rust flat-scan), or spatial
/// (PostGIS GIST on PG, haversine post-filter on SQLite). The default is
/// [`IndexKind::BTree`]
/// so existing call sites that build B-tree indexes (`build_create_indexes`,
/// `build_named_indexes`) need no churn — they construct with explicit
/// fields including `kind: IndexKind::BTree` to stay readable, but
/// `..Default::default()` would also work given the `#[derive(Default)]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexSpec {
    /// Deterministic index identifier (unquoted).
    pub name: String,
    /// Columns the index covers (unquoted, in declared order).
    pub columns: Vec<String>,
    /// Whether this is a UNIQUE index.
    pub unique: bool,
    /// `CREATE …` DDL ready for execution.
    pub sql: String,
    /// Index shape — selects the backend builder branch, wiring
    /// `Vector` / `Spatial` dispatch through the
    /// `zeroship_plugin_db::register_model::apply` Pass 2.
    pub kind: IndexKind,
}

/// Index shape - the closed sum over the three kinds of indexes
/// `registerModel` can materialise.
///
/// The default is [`IndexKind::BTree`] so every call site keeps
/// the same observable behaviour; `Vector` / `Spatial` dispatch is wired
/// through the `zeroship_plugin_db::register_model::apply` Pass 2.
///
/// **Why an enum, not a string**: same rationale as
/// [`crate::descriptors::VectorMetric`] - the rustc exhaustiveness check
/// trips every match arm if a future PR adds a fourth kind, rather
/// than a default branch silently routing the new kind to the B-tree
/// builder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IndexKind {
    /// Plain B-tree index over the listed columns. PG: `CREATE INDEX
    /// … (col1, col2, …)`. SQLite: same shape via the sqlite dialect.
    /// The default for every column with the `index` / `unique`
    /// modifier in the SDK schema DSL.
    #[default]
    BTree,
    /// Vector (ANN) index. `dims` is the declared vector dimensionality;
    /// `metric` selects the distance function. PG: `USING ivfflat`
    /// with the metric-appropriate opclass. SQLite: no actual index
    /// (flat scan); the kind value still flows through so the column
    /// DDL emits a `length("col") = 4 * dims` CHECK constraint.
    Vector {
        /// Declared vector dimensionality (e.g. 768 for `text-embedding-3-small`).
        dims: i32,
        /// Distance metric — see [`crate::descriptors::VectorMetric`].
        metric: crate::descriptors::VectorMetric,
    },
    /// Spatial index over a `geography(POINT, 4326)` (PG) or BLOB-
    /// packed `(lat, lng)` (SQLite) column. PG: `USING GIST`;
    /// SQLite: no actual index (haversine post-filter).
    Spatial,
}

/// Build the set of `CREATE INDEX CONCURRENTLY` statements for a schema.
///
/// Walks the field definitions and emits:
///   * a non-unique index per field with `index: true`,
///   * a unique index per field with `unique: true`.
///
/// Composite indexes (the proposal's `schema(...).index(name, fields)`
/// builder) are wired separately via [`build_named_indexes`] — callers
/// merge that `Vec` with this function's output at `bootstrap.rs`.
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
        // Skip top-level metadata keys (`_meta`,
        // `_indexes`) so the `_` reserved-prefix check in
        // `validate_field_name` doesn't trip on schema
        // bookkeeping.
        if is_schema_metadata_key(field) {
            continue;
        }
        // GeoPoint fields always emit an
        // `IndexKind::Spatial` spec regardless of the `index`/`unique`
        // markers. The impl builds the `USING GIST` DDL itself; the
        // `sql` field stays empty (same shape as the Vector branch).
        if def.get("type").and_then(|t| t.as_str()) == Some("geoPoint") {
            let name = index_name(collection, &[field.as_str()], /* unique = */ false);
            out.push(IndexSpec {
                name,
                columns: vec![field.clone()],
                unique: false,
                sql: String::new(),
                kind: IndexKind::Spatial,
            });
            continue;
        }

        // Vector fields always emit an `IndexKind::Vector`
        // spec regardless of the `index`/`unique` markers; the SDK's
        // `t.vector()` builder doesn't expose those modifiers (they
        // would be meaningless on an ivfflat-indexed column). The
        // builder dispatches to `VectorIndex::ensure_vector_index` in
        // `zeroship_plugin_db::register_model::apply` Pass 2 — the `sql` field stays empty
        // because the impl builds the DDL itself (it needs the
        // metric-specific opclass that isn't carried in the spec).
        if def.get("type").and_then(|t| t.as_str()) == Some("vector") {
            let dims = def
                .get("vectorDims")
                .and_then(serde_json::Value::as_i64)
                .filter(|d| *d > 0 && *d <= 16000)
                .map(|d| d as i32)
                .unwrap_or(0);
            if dims == 0 {
                // Malformed — skip the index. The column DDL emitter
                // will reject the table later (PG returns
                // `type "vector" does not exist` if the extension is
                // missing or `dims out of range` if dims is 0).
                continue;
            }
            let metric_str = def
                .get("vectorMetric")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("cosine");
            let metric = match metric_str {
                "l2" => crate::descriptors::VectorMetric::L2,
                "innerProduct" | "ip" => crate::descriptors::VectorMetric::InnerProduct,
                _ => crate::descriptors::VectorMetric::Cosine,
            };
            let name = index_name(collection, &[field.as_str()], /* unique = */ false);
            out.push(IndexSpec {
                name,
                columns: vec![field.clone()],
                unique: false,
                // The impl builds the DDL with the metric-appropriate
                // opclass; leave empty so an accidental BTree dispatch
                // would be a recognisable no-op rather than a stray
                // statement.
                sql: String::new(),
                kind: IndexKind::Vector { dims, metric },
            });
            continue;
        }

        // Deterministic-encrypted columns get an
        // automatic B-tree index. The SDK refuses range / regex / LIKE
        // on deterministic columns (only equality + `$in`), so a
        // B-tree on the ciphertext is sufficient and matches the
        // user's expectation that `find({ssnDet: "X"})` is fast.
        // Randomised columns do NOT get this index — the ciphertext is
        // different per write so equality lookups can't work anyway.
        let det_encrypted = def
            .get("encrypted")
            .and_then(|enc| enc.get("mode"))
            .and_then(|v| v.as_str())
            == Some("deterministic");
        if det_encrypted {
            let name = index_name(collection, &[field.as_str()], /* unique = */ false);
            let sql = format!(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} ({})",
                quote_ident(&name),
                table_qualified,
                quote_ident(field),
            );
            out.push(IndexSpec {
                name,
                columns: vec![field.clone()],
                unique: false,
                sql,
                kind: IndexKind::BTree,
            });
            // Fall through — a deterministic-encrypted column may also
            // carry `.unique()` (we still want a uniqueness constraint
            // on the ciphertext, valid because deterministic mode
            // preserves equality). The `wants_unique` branch below
            // emits the unique index alongside; PG dedupes
            // (two identical-shape indexes are cheap to ignore in
            // theory, but our deterministic-name contract collapses
            // them to a single entry if both were B-tree). We rely on
            // the caller-side scope check (Q-P5-H) to refuse
            // randomised+unique earlier; deterministic+unique is OK.
        }

        let wants_index = def.get("index").and_then(|v| v.as_bool()) == Some(true);
        let wants_unique = def.get("unique").and_then(|v| v.as_bool()) == Some(true);

        if !wants_index && !wants_unique {
            continue;
        }

        // `.index()` and `.unique()` are declarations about the REAL value, so
        // they land on the column that holds it: `__zs_raw__<field>` when the
        // field is masked, the field's own column otherwise.
        //
        // Putting a `.unique()` on the masked column instead would enforce
        // uniqueness over MASKS, where many rows legitimately share
        // `***-**-1234` - a data-integrity failure that presents as a
        // duplicate-key error on perfectly valid data, and for
        // `kind: "full"` (every mask is `***`) caps the table at one row.
        let raw = raw_column_for_field(field, def);
        let value_col = raw.clone().unwrap_or_else(|| field.to_string());

        // Unique implies an index — if both flags are set, prefer the unique
        // form (a unique index also serves as a lookup index, so emitting
        // both would be redundant and waste storage).
        if wants_unique {
            let name = index_name(collection, &[field.as_str()], /* unique = */ true);
            let col_list = quote_ident(&value_col);
            let sql = format!(
                "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} ({})",
                quote_ident(&name),
                table_qualified,
                col_list,
            );
            out.push(IndexSpec {
                name,
                columns: vec![value_col.clone()],
                unique: true,
                sql,
                kind: IndexKind::BTree,
            });
        } else if wants_index {
            let name = index_name(collection, &[field.as_str()], /* unique = */ false);
            let col_list = quote_ident(&value_col);
            let sql = format!(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} ({})",
                quote_ident(&name),
                table_qualified,
                col_list,
            );
            out.push(IndexSpec {
                name,
                columns: vec![value_col.clone()],
                unique: false,
                sql,
                kind: IndexKind::BTree,
            });
        }

        // Auto-emit a B-tree index on the MASKED column when the field has
        // `.index()` or `.uniqueIndex()` declared AND carries a mask
        // declaration with `kind != "none"`. Every creator-visible read and
        // filter now touches the masked column, so this is the index those
        // queries use; the one emitted above serves the raw column, which only
        // the unmask path reads. Naming: `<coll>__<col>_mask_idx`
        // (double-underscore separator, matching `named_index_name`'s
        // collision-avoidance convention). Never UNIQUE - see above.
        if wants_index || wants_unique {
            if raw.is_some() {
                let idx_name = cap_ident_name(&format!("{collection}__{field}_mask_idx"));
                let sql = format!(
                    "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} ({})",
                    quote_ident(&idx_name),
                    table_qualified,
                    quote_ident(field),
                );
                out.push(IndexSpec {
                    name: idx_name,
                    columns: vec![field.clone()],
                    unique: false,
                    sql,
                    kind: IndexKind::BTree,
                });
            }
        }
    }

    Ok(out)
}

/// Build the named multi-column index DDL declared via
/// `schema(...).index(name, fields)` on the SDK side.
///
/// The wire format is `[{name, fields, unique?}]`. Each entry becomes a
/// `CREATE [UNIQUE] INDEX CONCURRENTLY IF NOT EXISTS "<collection>__<name>"
/// ON "<schema>"."<collection>" (col1, col2, …)`. Collision with the
/// per-field auto-named indexes from `build_create_indexes` is avoided
/// by the `<collection>__` prefix (auto-named indexes use the
/// `<collection>_<col>_{idx,key}` shape — no double underscore).
///
/// Validation is intentionally light: the SDK already verified that
/// every field exists on the schema and that names are unique within
/// the schema. Here we re-check the wire-format shape so a hand-rolled
/// caller can't slip a malformed entry past the orchestrator.
pub fn build_named_indexes(
    app_id: &str,
    collection: &str,
    indexes: &serde_json::Value,
) -> Result<Vec<IndexSpec>, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let mut out = Vec::new();
    let Some(arr) = indexes.as_array() else {
        return Ok(out);
    };
    if arr.is_empty() {
        return Ok(out);
    }

    let table_qualified = format!("{}.{}", quote_ident(app_id), quote_ident(collection));

    for (i, entry) in arr.iter().enumerate() {
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| QueryError::InvalidIdent(format!("indexes[{i}].name is required")))?;
        if name.is_empty() {
            return Err(QueryError::InvalidIdent(format!(
                "indexes[{i}].name must be non-empty"
            )));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(QueryError::InvalidIdent(format!(
                "indexes[{i}].name {name:?} must match [A-Za-z0-9_]+"
            )));
        }
        let fields_v = entry
            .get("fields")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                QueryError::InvalidIdent(format!("indexes[{i}].fields must be a non-empty array"))
            })?;
        if fields_v.is_empty() {
            return Err(QueryError::InvalidIdent(format!(
                "indexes[{i}].fields must be non-empty"
            )));
        }
        let unique = entry
            .get("unique")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let mut columns: Vec<String> = Vec::with_capacity(fields_v.len());
        let mut quoted: Vec<String> = Vec::with_capacity(fields_v.len());
        for (j, fv) in fields_v.iter().enumerate() {
            let col = fv.as_str().ok_or_else(|| {
                QueryError::InvalidIdent(format!(
                    "indexes[{i}].fields[{j}] must be a string"
                ))
            })?;
            if col.is_empty() {
                return Err(QueryError::InvalidIdent(format!(
                    "indexes[{i}].fields[{j}] must be non-empty"
                )));
            }
            columns.push(col.to_string());
            quoted.push(quote_ident(col));
        }

        let pg_name = named_index_name(collection, name);
        let kind = if unique { "UNIQUE INDEX" } else { "INDEX" };
        let sql = format!(
            "CREATE {kind} CONCURRENTLY IF NOT EXISTS {} ON {} ({})",
            quote_ident(&pg_name),
            table_qualified,
            quoted.join(", "),
        );
        out.push(IndexSpec { name: pg_name, columns, unique, sql, kind: IndexKind::BTree });
    }

    Ok(out)
}

/// Construct the Postgres identifier for a named multi-column index.
///
/// Uses a double-underscore separator (`<collection>__<name>`) to avoid
/// collision with the single-underscore auto-named per-field indexes
/// produced by `index_name`. NAMEDATALEN-safe via [`cap_ident_name`], the
/// crate's single identifier cap.
pub fn named_index_name(collection: &str, name: &str) -> String {
    cap_ident_name(&format!("{collection}__{name}"))
}

/// Build a deterministic Postgres index name from a table name and columns.
///
/// Strategy:
///   1. Construct `<table>_<col1>_<col2>…_<suffix>` where suffix is
///      `key` for unique indexes and `idx` otherwise.
///   2. Cap the result to Postgres' `NAMEDATALEN` budget via
///      [`cap_ident_name`] — the crate's single identifier cap, which keeps
///      a readable prefix and appends a hash of the FULL natural name so two
///      long names cannot collapse onto one identifier.
///
/// Naming is content-addressed (same input → same name), so re-running
/// `registerModel` with `IF NOT EXISTS` is idempotent.
pub fn index_name(table: &str, columns: &[&str], unique: bool) -> String {
    let suffix = if unique { "key" } else { "idx" };
    let joined_cols = columns.join("_");
    cap_ident_name(&format!("{table}_{joined_cols}_{suffix}"))
}

/// The prefix every RAW-value column carries.
///
/// Chosen so that [`validate_field_name`] already refuses it: `RESERVED_NAMES`
/// reserves both `Prefix("_")` and `Prefix("__zs_")`, and that validator is
/// called on every inbound identifier surface the platform has (filter keys,
/// conflict-probe keys, write-document keys including `$set` nesting, and every
/// read identifier via `validate_read_identifier`). A raw column named with
/// this prefix is therefore **unnameable by creator code on surfaces nobody has
/// written yet**, which is a stronger property than adding a fence to each of
/// the surfaces that exist today.
pub const RAW_COLUMN_PREFIX: &str = "__zs_raw__";

/// The longest field name that can carry a mask.
///
/// `63 - RAW_COLUMN_PREFIX.len()`. Postgres truncates identifiers at 63 bytes
/// (NAMEDATALEN) and [`validate_field_name`] admits a 63-byte field, so a longer
/// masked field would produce a raw column the server silently truncates - and
/// two such fields could truncate to the same column. The declaration is
/// REFUSED instead, at DDL-emission time, in `field_to_column_for_dialect`.
///
/// (The pre-flip `<field>_masked` sibling had exactly this bug and did not cap:
/// a 60-character masked field produced a 67-character sibling.)
pub const MAX_MASKED_FIELD_NAME_BYTES: usize = 63 - RAW_COLUMN_PREFIX.len();

/// The physical column that holds `field`'s REAL value.
///
/// Total, and deliberately a plain concatenation rather than a hashing cap.
/// This name must stay byte-identical to
/// `zeroship_migrate_backend::schema::raw_column_name` - that one names the
/// column the migration engine CREATES, this one names the column the data
/// plane READS and WRITES - and a hashing cap implemented in two crates could
/// not be checked to agree by any compiler. Refusing an overlong masked field
/// name at declaration time removes the need for one entirely; see
/// [`MAX_MASKED_FIELD_NAME_BYTES`].
#[must_use]
pub fn raw_column_name(field: &str) -> String {
    format!("{RAW_COLUMN_PREFIX}{field}")
}

/// Return the RAW-value column name for `field` IFF the field's schema entry
/// carries a `.mask({...})` declaration with `kind != "none"`. Returns `None`
/// for non-masked columns and for columns that explicitly opt out via
/// `.mask({ kind: "none" })`.
///
/// # The storage flip
///
/// The field's OWN column (`ssn`) holds the **masked** string; this sibling
/// (`__zs_raw__ssn`) holds the real value and is unqueryable - not in a filter,
/// not in a projection, not in a sort, and not a field of the generated type.
///
/// It used to be the other way round: `ssn` held plaintext and `ssn_masked`
/// held the mask. The projection substituted `"ssn_masked" AS "ssn"`, but the
/// WHERE builder could not - it takes no schema hint - so
/// `find({ ssn: { $gt: "500-00-0000" } })` compared against **plaintext**. The
/// caller never saw a value and did not need to: the set of matching rows is
/// the answer, and repeated probes binary-search it with no authorization check
/// on the path and no audit row written.
///
/// After the flip the ignorant path is the safe path. A builder that knows
/// nothing about masking selects and filters the column with the natural name,
/// which is the mask, and leaks nothing. Plaintext has exactly one reader - the
/// explicit unmask API, where the authorization check and the audit row already
/// live.
///
/// Called by `build_create_table_with_fks` and `build_add_column` (DDL
/// emission), `build_create_indexes` (constraints follow the real value), and
/// the runtime's write relocation / read strip / unmask fetch.
pub fn raw_column_for_field(field: &str, def: &serde_json::Value) -> Option<String> {
    let mask_meta = def.get("mask").and_then(|v| v.as_object())?;
    let kind = mask_meta.get("kind").and_then(|v| v.as_str()).unwrap_or("full");
    if kind == "none" {
        return None;
    }
    Some(raw_column_name(field))
}

/// Render the canonical mask-sentinel comment payload
/// for a field's `.mask({...})` declaration, IFF the declaration is
/// present AND `kind != "none"`. Returns `None` when there's no
/// sibling to attach a sentinel to.
///
/// Reused by both backend introspectors (PG `COMMENT ON COLUMN` write
/// + SQLite inline-comment parse on read) — keeps the wire shape
/// consistent. The parser side lives in
/// [`crate::mask_codec::parse_mask_sentinel`].
pub fn mask_sentinel_for_field(def: &serde_json::Value) -> Option<String> {
    let mask_meta = def.get("mask").and_then(|v| v.as_object())?;
    let kind_str = mask_meta.get("kind").and_then(|v| v.as_str()).unwrap_or("full");
    if kind_str == "none" {
        return None;
    }
    let kind = crate::diff::MaskKind::from_sql(kind_str)?;
    let class_str = mask_meta
        .get("classification")
        .and_then(|v| v.as_str())
        .unwrap_or("pii");
    let classification = crate::diff::Classification::from_sql(class_str)?;
    Some(crate::mask_codec::build_mask_sentinel(
        kind,
        classification,
    ))
}

/// Render the `COMMENT ON COLUMN` statements that
/// attach the mask sentinel to every sibling column. Returns one
/// statement per masked field in `schema` (in declared order); the
/// caller joins them onto the CREATE TABLE / ALTER TABLE SQL via
/// `;` so they apply atomically.
///
/// Only the PG arm executes these statements — SQLite doesn't support
/// `COMMENT ON COLUMN`. The SQLite arm relies on the inline
/// `/* __zsmask:... */` comment emitted by `build_create_table_with_fks`,
/// preserved verbatim in `sqlite_master.sql`.
///
/// Returns the empty vector when the schema declares no masked
/// columns — the caller then emits no extra DDL.
#[must_use]
pub fn build_mask_sentinel_comments(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = schema.as_object() else {
        return out;
    };
    for (field, def) in obj {
        if is_schema_metadata_key(field) {
            continue;
        }
        if raw_column_for_field(field, def).is_none() {
            continue;
        }
        let Some(sentinel) = mask_sentinel_for_field(def) else {
            continue;
        };
        // Escape single quotes in the sentinel body for the SQL string
        // literal. The kind+classification alphabet contains none, but
        // be defensive against a future kind that does.
        let escaped = sentinel.replace('\'', "''");
        out.push(format!(
            "COMMENT ON COLUMN {}.{}.{} IS '{}'",
            quote_ident(app_id),
            quote_ident(collection),
            quote_ident(field),
            escaped,
        ));
    }
    out
}

/// Render the `COMMENT ON COLUMN` statement for one
/// masked field, IFF the field has a `.mask({...})` declaration
/// (`kind != "none"`). Used by the diff classifier's `MaskBackfill`
/// op to attach the sentinel at the same time as the masked column's
/// `ALTER TABLE ADD COLUMN` op.
///
/// The sentinel rides the MASKED column - the one with the field's own name -
/// because that is the column whose contents the sentinel describes.
///
/// Returns `None` for fields without a mask or with `kind: "none"` —
/// no masked column, no sentinel.
#[must_use]
pub fn build_mask_sentinel_comment_for_field(
    app_id: &str,
    collection: &str,
    field: &str,
    def: &serde_json::Value,
) -> Option<String> {
    raw_column_for_field(field, def)?;
    let sentinel = mask_sentinel_for_field(def)?;
    let escaped = sentinel.replace('\'', "''");
    Some(format!(
        "COMMENT ON COLUMN {}.{}.{} IS '{}'",
        quote_ident(app_id),
        quote_ident(collection),
        quote_ident(field),
        escaped,
    ))
}

/// Render the inline `/* zsenc:{mode}:{keyId}:{wraps} */`
/// encryption sentinel for a field's `t.encrypted({...})` declaration, IFF the
/// field carries an `encrypted` sub-object. Returns `None` for a plain column.
///
/// This is the SINGLE source of truth for the `zsenc` wire shape — both
/// [`field_to_column_for_dialect`] (the column-DDL emitter that bakes it after
/// the `BYTEA`/`BLOB` type) and the migration engine's declarative differ (which
/// appends it to its own snapshot-rendered column) call it, so the sentinel the
/// engine `generate`s is byte-identical to the one `registerModel` writes. The
/// parser side lives in `read_live_schema` (PG `pg_attribute` comment regex) /
/// the SQLite `sqlite_master.sql` regex.
///
/// The returned string INCLUDES the surrounding `/* … */` comment delimiters so
/// it can be embedded verbatim into DDL (PG ignores it at parse time; SQLite
/// preserves it in `sqlite_master.sql`).
#[must_use]
pub fn encryption_sentinel_for_field(def: &serde_json::Value) -> Option<String> {
    encryption_sentinel_body_for_field(def).map(|body| format!("/* {body} */"))
}

/// The bare `zsenc:<mode>:<keyId>:<wraps>` sentinel BODY for a field's
/// `t.encrypted({...})` declaration (no `/* */` wrapper, no comment statement),
/// or `None` for a plain column. The SINGLE source of truth for the `zsenc` wire
/// grammar: [`encryption_sentinel_for_field`] wraps it in `/* */` for the inline
/// DDL form, and [`build_encryption_sentinel_comments`] wraps it in a
/// `COMMENT ON COLUMN … '…'` statement for the PG-recoverable form. The runtime
/// parser is [`crate::mask_codec::parse_encryption_sentinel`].
#[must_use]
pub fn encryption_sentinel_body_for_field(def: &serde_json::Value) -> Option<String> {
    let enc = def.get("encrypted").and_then(|v| v.as_object())?;
    let mode = enc.get("mode").and_then(|v| v.as_str()).unwrap_or("randomised");
    // Normalise legacy `"randomized"` (US spelling) to the canonical
    // `randomised` so the introspector parser (which accepts both but the
    // emit side normalises to one) round-trips cleanly.
    let mode_norm = if mode == "randomized" { "randomised" } else { mode };
    let key_id = enc.get("keyId").and_then(|v| v.as_str()).unwrap_or("default");
    let wraps = enc.get("wraps").and_then(|v| v.as_str()).unwrap_or("string");
    Some(format!("zsenc:{mode_norm}:{key_id}:{wraps}"))
}

/// Convert a field definition to a full column definition for CREATE TABLE.
///
/// Validates the field name via [`validate_field_name_for_declaration`]
/// before emitting DDL. The declaration-time variant fences the 7
/// platform system field names ([`SYSTEM_FIELD_NAMES`]); filter-time
/// call sites stay on the underlying [`validate_field_name`] so creators
/// can keep filtering by `id` / `created_at` / etc.
fn field_to_column_for_dialect(
    field: &str,
    def: &serde_json::Value,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    validate_field_name_for_declaration(field)?;
    // The declared type and constraints are emitted under the column that
    // holds the REAL value: `__zs_raw__<field>` when the field is masked,
    // the field's own name otherwise. The LOGICAL name is what gets
    // validated - the physical one is the platform's, and
    // `validate_field_name_for_declaration` would refuse it (it starts
    // `__zs_`, which is exactly why that name was chosen).
    let physical = match raw_column_for_field(field, def) {
        Some(raw) => {
            if field.len() > MAX_MASKED_FIELD_NAME_BYTES {
                return Err(QueryError::InvalidIdent(format!(
                    "masked field name exceeds {MAX_MASKED_FIELD_NAME_BYTES} bytes: {field} \
                     (a mask needs a second column named '{RAW_COLUMN_PREFIX}<field>', and \
                     Postgres truncates identifiers at 63 bytes)"
                )));
            }
            raw
        }
        None => field.to_string(),
    };
    Ok(field_to_column_named_for_dialect(&physical, def, dialect))
}

/// [`field_to_column_for_dialect`] with the physical column name supplied.
///
/// Split out so the DDL emitter can put a masked field's declared type and
/// constraint set on the raw column while validating the logical field name.
/// Performs NO name validation: every caller has already validated the logical
/// name this physical one was derived from.
fn field_to_column_named_for_dialect(
    physical: &str,
    def: &serde_json::Value,
    dialect: SqlDialect,
) -> String {
    // `t.encrypted(...)`-declared columns always store the
    // ciphertext wire blob (`[version_flag | nonce | ct+tag]`) as BYTEA
    // regardless of `wraps`. The encryption pass swaps the plaintext
    // out before the INSERT/UPDATE, and the SQL builder casts the
    // base64 parameter back to BYTEA via `decode($N, 'base64')::bytea`.
    //
    // Emit a `/* zsenc:{mode}:{keyId}:{wraps} */` sentinel
    // comment alongside the column type so the SQLite-arm introspector
    // can regex-recover the encryption metadata from `sqlite_master.sql`.
    // PG ignores SQL comments at parse time (the type is still BYTEA);
    // SQLite stores the original CREATE TABLE text verbatim. SQLite's
    // type affinity treats "BYTEA" as NUMERIC (no INT/CHAR/TEXT/BLOB/
    // FLOA/REAL/DOUB substring match), which still accepts BLOB values
    // — same column shape both engines see byte-identical inserts.
    // Sentinel-on-DDL is the same regex-on-DDL pattern used for
    // vector dims; sidecar `__zs_schema_meta` is the upgrade path
    // (deferred). See
    // `docs/archive/p5-encryption-backup-implementation-plan.md` §5.
    let enc_comment_owned;
    let enc_comment: &str = if let Some(body) = encryption_sentinel_for_field(def) {
        enc_comment_owned = format!(" {body}");
        &enc_comment_owned
    } else {
        ""
    };
    let sql_type = def_to_column_type_for_dialect(def, dialect);
    let constraints = def_to_constraints_for_dialect(physical, def, dialect);
    // The sentinel comment (when present) sits between the type and the
    // constraints so the parsed shape is `"<col>" BYTEA /* zsenc:... */
    // <constraints>`. PG ignores the comment; SQLite preserves it in
    // `sqlite_master.sql` for the introspector regex.
    format!(
        "{} {}{} {}",
        quote_ident_for_dialect(physical, dialect),
        sql_type,
        enc_comment,
        constraints
    )
    .trim()
    .to_string()
}

/// Map a single SDK field definition (`{ type, encrypted?, vectorDims?, … }`)
/// to the column SQL TYPE for `dialect`, covering the FULL type surface —
/// `vector(N)`, `geography(POINT,4326)` (geoPoint), `BYTEA`/`BLOB`
/// (encrypted), `literal`'s primitive, and the plain B-tree types. This is
/// the single source of truth the migration engine's declarative differ
/// adopts: the engine builds a `def` from its
/// `FieldDescriptor` and calls this, so it reaches full capability
/// (vector/encrypted/geo) by reuse rather than re-implementing — and never
/// rejects those types again. The returned spelling is DDL (`vector(N)`,
/// `DOUBLE PRECISION`, `TIMESTAMPTZ`, …); callers that need the
/// `information_schema.data_type` spelling translate it themselves.
pub fn def_to_column_type_for_dialect(def: &serde_json::Value, dialect: SqlDialect) -> String {
    renderer(dialect).column_type(def)
}

fn char_len(def: &serde_json::Value) -> Option<u64> {
    def.get("charLen")
        .and_then(serde_json::Value::as_u64)
        .filter(|len| *len > 0)
}

fn parse_character_type_len(data_type: &str) -> Option<u64> {
    let lower = data_type.trim().to_ascii_lowercase();
    let inner = lower
        .strip_prefix("character(")
        .or_else(|| lower.strip_prefix("char("))
        .or_else(|| lower.strip_prefix("bpchar("))?
        .strip_suffix(')')?;
    inner.parse::<u64>().ok().filter(|len| *len > 0)
}

/// Emit per-variant CHECK constraints for a flat-expanded
/// discriminated union (proposal §C2). The discriminator field carries
/// the per-variant shape map; for each variant we emit a clause like
/// ```sql
/// CONSTRAINT events_kind_login_chk CHECK (
///   kind <> 'login' OR (userId IS NOT NULL AND ip IS NOT NULL)
/// )
/// ```
/// so a `kind='login'` row cannot store NULL where the variant requires
/// a value. The discriminator column itself already gets
/// `CHECK (kind IN ('login', 'error', ...))` from the regular `enum`
/// constraint emitter (`def_to_constraints`).
///
/// The constraint name is content-addressed (`<table>_<disc>_<value>_chk`)
/// and hash-truncated like our index names so it stays within Postgres'
/// `NAMEDATALEN` (63-byte) limit.
fn emit_union_variant_checks(
    collection: &str,
    disc_field: &str,
    disc_def: &serde_json::Value,
    variants: &[serde_json::Value],
    dialect: SqlDialect,
) -> Vec<String> {
    let disc_col = quote_ident_for_dialect(disc_field, dialect);
    let disc_primitive = disc_def.get("type").and_then(|t| t.as_str()).unwrap_or("string");

    let mut out = Vec::new();
    for variant in variants {
        let Some(variant_obj) = variant.as_object() else {
            continue;
        };
        let Some(disc_field_def) = variant_obj.get(disc_field) else {
            continue;
        };
        let Some(lit) = disc_field_def.get("literalValue") else {
            continue;
        };

        // Required (non-discriminator) fields in this variant — only
        // these need the NOT NULL clause inside the CHECK.
        let mut required_cols: Vec<String> = Vec::new();
        for (field, fd) in variant_obj {
            if field == disc_field {
                continue;
            }
            let is_required = fd.get("required").and_then(serde_json::Value::as_bool) == Some(true);
            if is_required {
                required_cols.push(quote_ident_for_dialect(field, dialect));
            }
        }

        // The literal value rendering must match how the column is
        // stored — string literals are single-quoted, numbers and
        // booleans are bare.
        let lit_sql = match disc_primitive {
            "number" => lit.as_f64().map(|n| n.to_string()).unwrap_or_default(),
            "boolean" => lit.as_bool().map(|b| b.to_string()).unwrap_or_default(),
            _ => {
                // string discriminator
                let s = lit.as_str().unwrap_or("");
                format!("'{}'", s.replace('\'', "''"))
            }
        };

        // Skip variants with empty literal rendering — would produce
        // bogus SQL like `kind <> ` (defensive — never hit when SDK
        // emits well-formed JSON).
        if lit_sql.is_empty() {
            continue;
        }

        // Generate a deterministic identifier. Stringy values get
        // included verbatim (lower-cased); for non-string discriminators
        // we use the literal stringified form.
        let value_tag = match disc_primitive {
            "number" => lit.as_f64().map(|n| format!("{n}")).unwrap_or_default(),
            "boolean" => lit.as_bool().map(|b| b.to_string()).unwrap_or_default(),
            _ => lit.as_str().unwrap_or("").to_string(),
        };
        let sanitized_tag = sanitize_for_identifier(&value_tag);
        let constraint_name =
            union_check_constraint_name(collection, disc_field, &sanitized_tag);

        let clause = if required_cols.is_empty() {
            // No per-variant required fields means no integrity beyond
            // the discriminator IN-list; skip emitting an empty CHECK.
            continue;
        } else {
            let null_clause = required_cols
                .iter()
                .map(|c| format!("{c} IS NOT NULL"))
                .collect::<Vec<_>>()
                .join(" AND ");
            format!(
                "CONSTRAINT {} CHECK ({} <> {} OR ({}))",
                quote_ident_for_dialect(&constraint_name, dialect),
                disc_col,
                lit_sql,
                null_clause
            )
        };
        out.push(clause);
    }
    out
}

/// Sanitise a discriminator value (e.g. `login-x.y`) into a string safe
/// to splice into a Postgres identifier — keep ASCII alphanumerics and
/// underscores, replace everything else with `_`.
fn sanitize_for_identifier(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('x');
    }
    out
}

/// Build the constraint name for a union-variant CHECK. NAMEDATALEN-safe
/// (≤ 63 bytes) via [`cap_ident_name`], the crate's single identifier cap.
fn union_check_constraint_name(collection: &str, disc: &str, value_tag: &str) -> String {
    cap_ident_name(&format!("{collection}_{disc}_{value_tag}_chk"))
}

/// Map schema type to PostgreSQL type.
///
/// `ref` columns emit `TEXT` so they match the `id TEXT PRIMARY KEY`
/// system-field DDL (typed_id wire format); an `INTEGER` FK would fail
/// with `column type mismatch` at FK-constraint creation time on
/// Postgres. SQLite tolerates type mismatch (declared types are
/// advisory) but the typed_id values inserted into a ref column are
/// TEXT-shaped strings, so the storage class is TEXT either way.
fn def_to_pg_type(def: &serde_json::Value) -> &'static str {
    match def.get("type").and_then(|t| t.as_str()) {
        Some("string") => "TEXT",
        Some("char") => "TEXT",
        // `t.vector(dims)` maps to pgvector's `vector(N)`.
        // Returning the bare `"vector"` token would lose the dims, so
        // this arm is unused; column DDL composes the dims back in via
        // [`def_to_pg_type_with_dims`]. Kept here to keep the
        // enumeration exhaustive at the type-vocabulary level — a
        // future caller that ignores dims (e.g. a generic introspection
        // path) gets the un-parameterised type.
        Some("vector") => "vector",
        // `t.number()` maps to DOUBLE PRECISION (FLOAT8). JS `number`
        // is an IEEE-754 double, so this is the exact 1:1 mapping.
        // NUMERIC would be more precise but compio-postgres' text-out
        // path doesn't decode it back to a JS value cleanly;
        // `t.bigInteger()` exists for callers who need exact 64-bit
        // ints.
        Some("number") => "DOUBLE PRECISION",
        Some("real") => "REAL",
        // `int`/`integer` are first-class integer tokens (the SQLite arm of
        // `def_to_column_type_for_dialect` already maps them to `INTEGER`; the dev
        // `registerModel` JSON declares `{ type: "int" }`). Before this arm the PG
        // map degraded them to the `_ => TEXT` fallback, so the engine's
        // dialect-agnostic `desired_snapshot` (which spells types via the PG map)
        // recorded `integer` while this emitter would have written TEXT — a
        // permanent drift. Mapping to `INTEGER` here makes the snapshot and the
        // emitter agree on BOTH dialects. PG stays byte-identical for every
        // existing column: the SDK's `t.*` surface never emits a bare `int` on PG
        // (`t.number()` → DOUBLE PRECISION, `t.bigInteger()` → BIGINT), so no
        // previously-emitted PG column changes type. The PG type *names*
        // (`bigint`/`int4`/`int8`) are deliberately NOT accepted — they are not DSL
        // tokens and stay on the TEXT fallback so they remain typo-rejected.
        Some("int") | Some("integer") => "INTEGER",
        Some("smallInt") => "SMALLINT",
        Some("bigInt") => "BIGINT",
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
        Some("textArray") => "text[]",
        // Cascades to TEXT so FK column type matches the
        // `id TEXT PRIMARY KEY` system-field DDL. See doc-comment on
        // [`def_to_pg_type`] for the rationale.
        Some("ref") => "TEXT",
        Some("inet") => "INET",
        // C2 — a top-level `t.union(...)` is flattened to discrete
        // columns by the SDK before it reaches the DDL emitter, so this
        // path should never fire for the discriminator column itself
        // (it has the discriminator's primitive type, not "union").
        // A *nested* `t.union(...)` (inside `t.object`) falls through
        // to JSONB storage; per-variant integrity is application-side.
        Some("union") => "JSONB",
        // C2 — a top-level `t.literal()` field outside a union would
        // store as TEXT/NUMERIC/BOOLEAN based on its literal type, but
        // by the time the DDL emitter sees it the SDK normaliser keeps
        // the `literal` tag. We pick the primitive type from the
        // literal value so a `t.literal("login")` column becomes TEXT
        // with a CHECK constraint elsewhere.
        Some("literal") => match def.get("literalValue") {
            Some(serde_json::Value::Number(_)) => "NUMERIC",
            Some(serde_json::Value::Bool(_)) => "BOOLEAN",
            _ => "TEXT",
        },
        _ => "TEXT",
    }
}

/// Canonicalise a column type to the SQLite affinity token used when comparing
/// PG-spelled desired snapshots against live SQLite declared types.
#[must_use]
pub fn sqlite_canonical_type(data_type: &str) -> &'static str {
    let lower = data_type.trim().to_ascii_lowercase();
    // Parameterised extension types keep their DDL spelling in the snapshot
    // (`vector(384)`, `geography(POINT, 4326)`); both emit BLOB on SQLite.
    if lower.starts_with("vector(")
        || lower == "vector"
        || lower.starts_with("geography(")
        || lower.starts_with("geometry(")
    {
        return "blob";
    }
    match lower.as_str() {
        // TEXT affinity: PG `text`/`jsonb`/`timestamp with time zone`/`date`
        // (date→TIMESTAMPTZ, calendarDate→DATE on PG; both → SQLite TEXT), and the
        // live SQLite `text` token itself.
        "text" | "text[]" | "jsonb" | "json" | "timestamp with time zone" | "timestamptz"
        | "date" | "inet" | "character" | "char" | "bpchar" => {
            "text"
        }
        // REAL affinity: PG `double precision` (`t.number()`), and live `real`.
        "double precision" | "float8" | "real" => "real",
        // INTEGER affinity: PG `boolean`/`integer` (and `bigint`), and live `integer`.
        "boolean" | "integer" | "bigint" | "smallint" | "int8" | "int4" | "int2" | "int" => {
            "integer"
        }
        // NUMERIC affinity: PG `numeric` (a numeric `t.literal()`), and live `numeric`.
        "numeric" | "decimal" => "numeric",
        // BLOB affinity: PG `bytea` (encrypted / `t.bytes()`), and live `blob`.
        "bytea" | "blob" => "blob",
        // Unknown / future spelling: fall back to TEXT (SQLite's catch-all affinity,
        // matching the emitter's `_ => TEXT` arm). An unrecognised pair still
        // compares equal-to-equal by its own lowercased form first (see the caller),
        // so this fallback only collapses genuinely unmapped tokens.
        _ => "text",
    }
}

/// Canonicalise MySQL `information_schema.COLUMNS.COLUMN_TYPE` / rendered DDL
/// type strings for drift/probe comparison.
#[must_use]
pub fn mysql_canonical_type(data_type: &str) -> String {
    let lower = data_type.trim().to_ascii_lowercase();
    let no_width = strip_mysql_int_display_width(&lower);
    if no_width.starts_with("enum(") {
        return no_width;
    }
    if no_width == "varchar(43)" || no_width == "inet" {
        return "inet".to_string();
    }
    if let Some(len) = parse_character_type_len(&no_width) {
        return format!("character({len})");
    }
    if no_width.starts_with("varchar(") || no_width.ends_with("text") || no_width == "char" {
        return "text".to_string();
    }
    if no_width.starts_with("varbinary(") || no_width.ends_with("blob") || no_width == "bytea" {
        return "blob".to_string();
    }
    if no_width.starts_with("datetime")
        || no_width.starts_with("timestamp")
        || matches!(no_width.as_str(), "timestamp with time zone" | "timestamptz")
    {
        return "datetime".to_string();
    }
    if no_width.starts_with("decimal") || no_width == "numeric" {
        return "decimal".to_string();
    }
    if no_width.starts_with("double") || matches!(no_width.as_str(), "double precision" | "float8")
    {
        return "double".to_string();
    }
    if matches!(no_width.as_str(), "float" | "real" | "float4") {
        return "real".to_string();
    }
    if no_width.starts_with("tinyint(1)") || no_width == "boolean" {
        return "boolean".to_string();
    }
    match no_width.as_str() {
        "smallint" | "int2" => "smallint".to_string(),
        "int" | "integer" | "int4" => "int".to_string(),
        "bigint" | "int8" => "bigint".to_string(),
        "json" | "jsonb" | "text[]" => "json".to_string(),
        "date" => "date".to_string(),
        "point" | "point srid 4326" | "geography(point, 4326)" | "geography(POINT, 4326)" => {
            "point".to_string()
        }
        other => other.to_string(),
    }
}

fn strip_mysql_int_display_width(input: &str) -> String {
    for ty in ["tinyint", "smallint", "mediumint", "int", "integer", "bigint"] {
        if let Some(rest) = input.strip_prefix(ty) {
            if let Some(after_open) = rest.strip_prefix('(') {
                if let Some((digits, after_close)) = after_open.split_once(')') {
                    if digits.chars().all(|c| c.is_ascii_digit()) {
                        return format!("{ty}{after_close}");
                    }
                }
            }
        }
    }
    input.to_string()
}

/// Generate column constraints from field definition.
fn def_to_constraints(field: &str, def: &serde_json::Value) -> String {
    def_to_constraints_for_dialect(field, def, SqlDialect::Postgres)
}

fn def_to_constraints_for_dialect(
    field: &str,
    def: &serde_json::Value,
    dialect: SqlDialect,
) -> String {
    let mut parts = Vec::new();

    if def.get("required").and_then(|v| v.as_bool()) == Some(true) {
        parts.push("NOT NULL".to_string());
    }

    // NOTE: `unique` is intentionally NOT emitted as a column-level constraint
    // here. The proposal (zeroship-db.md A1) mandates that every uniqueness
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
            Some("json") | Some("object") => parts.push(renderer(dialect).json_object_default()),
            Some("array") => parts.push(renderer(dialect).json_array_default()),
            _ => {}
        }
    } else {
        // Default defaults for json/object/array
        match def.get("type").and_then(|t| t.as_str()) {
            Some("json") | Some("object") => parts.push(renderer(dialect).json_object_default()),
            Some("array") => parts.push(renderer(dialect).json_array_default()),
            _ => {}
        }
    }

    // Check constraints for min/max
    let col = quote_ident_for_dialect(field, dialect);
    if let (Some("number"), Some(min)) = (def.get("type").and_then(|t| t.as_str()), def.get("min").and_then(|v| v.as_f64())) {
        if let Some(max) = def.get("max").and_then(|v| v.as_f64()) {
            parts.push(format!("CHECK ({col} >= {min} AND {col} <= {max})"));
        } else {
            parts.push(format!("CHECK ({col} >= {min})"));
        }
    } else if let (Some("number"), Some(max)) = (def.get("type").and_then(|t| t.as_str()), def.get("max").and_then(|v| v.as_f64())) {
        parts.push(format!("CHECK ({col} <= {max})"));
    }

    // C2 — standalone literal field. The value's primitive type is
    // already mapped by `def_to_pg_type`; here we attach a CHECK so the
    // column can hold only the literal value. Note this only fires for
    // a `t.literal()` used as a top-level *non-union* column — inside a
    // flat-expanded union the discriminator carries an `enum` of all
    // variant literals (handled by the regular enum constraint below).
    if def.get("type").and_then(|t| t.as_str()) == Some("literal") {
        if let Some(lit) = def.get("literalValue") {
            let lit_sql = match lit {
                serde_json::Value::String(s) => Some(format!("'{}'", s.replace('\'', "''"))),
                serde_json::Value::Number(n) => Some(n.to_string()),
                serde_json::Value::Bool(b) => Some(b.to_string()),
                _ => None,
            };
            if let Some(rendered) = lit_sql {
                parts.push(format!("CHECK ({col} = {rendered})"));
            }
        }
    }

    // Enum constraint — supports both string and numeric values
    if matches!(dialect, SqlDialect::Mysql) && mysql_native_enum_values(def).is_some() {
        return parts.join(" ");
    }

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

/// The empty read schema: a collection that declares no creator-visible field.
///
/// **L24** — this is what "no fields" looks like now, and it is FAIL-CLOSED: a
/// read against it projects the seven platform system columns and nothing else,
/// and every non-system identifier in `select` / `orderBy` is refused. It is not
/// a stand-in for an unresolved schema; the only production caller is
/// [`build_write_target_probe`], which selects `id` alone.
pub fn empty_read_schema() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Build the bounded id probe used before a write fans out per matching row.
///
/// The caller supplies an explicit bound. `updateMany` asks for one row above
/// [`MAX_QUERY_LIMIT`] so it can distinguish an exactly-full target set from
/// an overflowing one; `updateOne` asks for one. This is deliberately separate
/// from creator-facing `find`, whose public limit remains `MAX_QUERY_LIMIT`.
pub fn build_write_target_probe(
    app_id: &str,
    collection: &str,
    filter: &Value,
    limit: i64,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    let select = serde_json::json!(["id"]);
    // `id` is a platform system field, so the EMPTY read schema is the correct
    // and complete one for this probe: it declares no creator field, and this
    // query projects none.
    let probe_schema = empty_read_schema();
    let mut built = build_find_with_schema_and_unmask_and_soft_delete_with_dialect_and_limit_ceiling(
        app_id,
        collection,
        filter,
        Some(limit),
        None,
        None,
        Some(&select),
        &probe_schema,
        &[],
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
    app_id: &str,
    collection: &str,
    filter: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let obj = filter.as_object().ok_or_else(|| {
        QueryError::InvalidFilter("conflict probe filter must be an object".to_string())
    })?;
    if obj.is_empty() {
        return Err(QueryError::InvalidFilter(
            "conflict probe filter cannot be empty".to_string(),
        ));
    }

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let mut params = Vec::new();
    let mut conditions = Vec::new();

    for (field, value) in obj {
        if field.starts_with("__zsbin__") {
            continue;
        }
        validate_field_name(field)?;
        let col = quote_ident(field);
        if value.is_null() {
            conditions.push(format!("{col} IS NULL"));
            continue;
        }

        let raw = value_to_param(value);
        let binary_bind = obj.contains_key(&format!("__zsbin__{field}"));
        let param_value = if binary_bind {
            dialect.wrap_binary_bind_param(raw)
        } else {
            raw
        };
        params.push(param_value);
        let n = params.len();
        if binary_bind {
            conditions.push(format!("{col} = {}", dialect.binary_bind_placeholder(n)));
        } else {
            conditions.push(format!("{col} = ${n}"));
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
    Ok(BuiltQuery { sql, params })
}

/// Schema-aware SELECT builder.
///
/// Same shape as [`build_find`], plus an optional `schema` (the cached
/// `serde_json::Value` from `ThreadDbContext::schema_for`). When the
/// schema is `Some(_)` and declares masked columns (`def.mask = Some({...})`
/// with `kind != "none"`), the SELECT clause emits
/// `"<col>_masked" AS "<col>"` in place of the bare parent column, and
/// the ciphertext / plaintext column is NOT included. This flips the
/// read side from decrypting on read to serving the masked sibling.
///
/// Generated SQL example (PG):
/// ```sql
/// -- baseline (schema=None or no masked columns):
/// SELECT "id", "created_at", ... FROM users WHERE id = $1
///
/// -- schema declares ssn + email masked:
/// SELECT "id", "ssn_masked" AS "ssn", "email_masked" AS "email", "name"
///   FROM users WHERE id = $1
/// ```
///
/// Opt-out path: columns declared with `.mask({ kind: "none" })` keep
/// emitting the parent column directly, preserving decrypt-on-read
/// behaviour for callers that explicitly need plaintext.
///
/// When `select` carries an explicit projection array, each requested
/// column is rewritten the same way — `select: ["ssn"]` becomes
/// `SELECT "ssn_masked" AS "ssn"`. `id` and other non-masked columns
/// pass through unchanged.
#[allow(clippy::too_many_arguments)]
pub fn build_find_with_schema(
    app_id: &str,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
    schema_hint: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_find_with_schema_and_unmask(
        app_id,
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

/// Schema-aware SELECT builder with per-query unmask
/// hint support.
///
/// Same shape as [`build_find_with_schema`], plus an `unmask_columns`
/// slice listing columns the caller wants in plaintext rather than the
/// masked sibling form. For each column in the slice that is ALSO a
/// masked column on the schema, the SELECT clause emits the bare
/// parent (the ciphertext for encrypted columns, plaintext for mask-
/// only columns) rather than the `"<col>_masked" AS "<col>"` alias.
/// The downstream pipeline (`apply_encryption_on_read` →
/// `apply_mask_wrap_on_read` → `dispatch_unmask_for_query`) then
/// decrypts the parent and replaces the row slot with the plaintext.
///
/// `unmask_columns` items not present on the schema are silently
/// ignored at the build layer — the auth fence
/// (`crud::unmask::authorize_query_hint`) already refused that case
/// with a typed `unmask_column_not_masked` error. Columns named in
/// `unmask_columns` AND on the schema but NOT carrying a `.mask({...})`
/// declaration are also passed through verbatim.
///
/// Generated SQL example (PG, schema declares ssn + email masked,
/// `unmask_columns = ["ssn"]`):
/// ```sql
/// SELECT "id", "ssn", "email_masked" AS "email", "name"
///   FROM users WHERE id = $1
/// ```
///
/// Note `"ssn"` is the bare ciphertext column (BYTEA on PG; BLOB on
/// SQLite) — the encryption pass will decrypt it on the way out, and
/// the unmask-for-query pass will overwrite the row slot with the
/// plaintext for the SDK to consume.
///
/// Thin shim around
/// [`build_find_with_schema_and_unmask_and_soft_delete`] passing
/// `filter_soft_deleted = false` so direct callers (the legacy CRUD
/// entry points + tests) keep their existing contract. The CRUD dispatch
/// path threads the soft-delete flag through the dedicated entry.
#[allow(clippy::too_many_arguments)]
pub fn build_find_with_schema_and_unmask(
    app_id: &str,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
    schema_hint: &Value,
    unmask_columns: &[String],
) -> Result<BuiltQuery, QueryError> {
    build_find_with_schema_and_unmask_and_soft_delete(
        app_id,
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
    app_id: &str,
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
) -> Result<BuiltQuery, QueryError> {
    build_find_with_schema_and_unmask_and_soft_delete_with_dialect_and_limit_ceiling(
        app_id,
        collection,
        filter,
        limit,
        offset,
        order_by,
        select,
        schema_hint,
        unmask_columns,
        filter_soft_deleted,
        dialect,
        MAX_QUERY_LIMIT,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_find_with_schema_and_unmask_and_soft_delete_with_dialect_and_limit_ceiling(
    app_id: &str,
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
    limit_ceiling: i64,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where_with_dialect(filter, &mut params, dialect)?;
    if let Some(lim) = limit {
        validate_limit_bound("find.limit", lim, limit_ceiling)?;
    }
    if let Some(off) = offset {
        validate_limit_bound("find.offset", off, MAX_QUERY_OFFSET)?;
    }

    let select_expr =
        build_masked_aware_select_expr_with_unmask(select, schema_hint, unmask_columns)?;

    let mut sql = format!("SELECT {select_expr} FROM {schema}.{table}");
    let composed_where = compose_where_with_soft_delete(&where_clause, filter_soft_deleted);
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

    Ok(BuiltQuery { sql, params })
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
    app_id: &str,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
    schema_hint: &Value,
    unmask_columns: &[String],
    filter_soft_deleted: bool,
) -> Result<BuiltQuery, QueryError> {
    build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
        app_id,
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
/// pattern used by [`build_soft_delete_one_with_system_fields`] /
/// [`build_restore_one_with_system_fields`] inner SELECTs.
///
/// Three cases:
/// 1. `!filter_soft_deleted` → return `where_clause` verbatim (back-
///    compat with callers that don't filter soft-deletes).
/// 2. `filter_soft_deleted && where_clause.is_empty()` → return
///    `"deleted_at" IS NULL` (the auto-filter becomes the whole
///    WHERE body).
/// 3. `filter_soft_deleted && !where_clause.is_empty()` → return
///    `<where_clause> AND "deleted_at" IS NULL`.
fn compose_where_with_soft_delete(where_clause: &str, filter_soft_deleted: bool) -> String {
    if !filter_soft_deleted {
        return where_clause.to_string();
    }
    if where_clause.is_empty() {
        "\"deleted_at\" IS NULL".to_string()
    } else {
        format!("{where_clause} AND \"deleted_at\" IS NULL")
    }
}

/// Compose the SELECT column-list expression, accounting
/// for masked columns when `schema_hint` is `Some(_)`. Thin shim around
/// [`build_masked_aware_select_expr_with_unmask`] for legacy callers
/// that have no per-query unmask hint to thread through.
pub fn build_masked_aware_select_expr(
    select: Option<&Value>,
    schema_hint: &Value,
) -> Result<String, QueryError> {
    build_masked_aware_select_expr_with_unmask(select, schema_hint, &[])
}

/// Compose the implicit `SELECT` list for a qualified
/// table source (`t`, `src`, ...), accounting for masked columns in the
/// cached schema.
///
/// This is the specialized-search sibling of
/// [`build_masked_aware_select_expr`]. The search paths (`search`, `near`)
/// read from a table alias (`t`) and append one synthetic engine column
/// (`_distance`, `_distance_m`). When
/// the cached schema declares any masked column, emitting `t.*` drifts
/// back to the un-masked shape: the parent ciphertext/plaintext column
/// rides out of SQL and only gets corrected later in the read pipeline.
///
/// It always expands to an explicit qualified list:
/// - `t."id" AS "id"` first,
/// - `t."<col>_masked" AS "<col>"` for masked columns,
/// - `t."<col>" AS "<col>"` for non-masked columns.
///
/// **L24** — there is no `t.*` arm any more. It used to be taken whenever the
/// schema had not been resolved yet, which is precisely when the search paths
/// served the raw parent column and every internal physical column.
pub fn build_masked_aware_select_expr_for_table_alias(
    schema_hint: &Value,
    table_alias: &str,
) -> Result<String, QueryError> {
    let parts = implicit_read_projection_parts(schema_hint, Some(table_alias))?;
    Ok(parts.join(", "))
}

/// Compose the SELECT column-list expression, accounting
/// for masked columns AND a per-query unmask hint.
///
/// Three cases (same as [`build_masked_aware_select_expr`]) — the unmask hint just overrides the
/// per-column sibling-alias decision for any listed column:
/// 1. `select` is an explicit, non-empty projection array → for each
///    listed column, emit the bare parent if the column is unmask-
///    listed, the sibling alias if the schema marks it masked, else
///    the bare parent.
/// 2. `select` is absent / empty AND `schema_hint` is `Some(_)` →
///    expand to an explicit list: every public system field plus every
///    declared schema field, with masked columns aliased through the
///    sibling EXCEPT where the unmask hint promotes them back to the
///    parent.
/// 3. `select` is absent / empty AND `schema_hint` is `None` → fall
///    through to `*`.
fn build_masked_aware_select_expr_with_unmask(
    select: Option<&Value>,
    schema_hint: &Value,
    _unmask_columns: &[String],
) -> Result<String, QueryError> {
    // Case 1: explicit projection.
    if let Some(Value::Array(arr)) = select {
        if !arr.is_empty() {
            let mut cols: Vec<String> = Vec::with_capacity(arr.len());
            for value in arr {
                let name = value.as_str().ok_or_else(|| {
                    QueryError::InvalidFilter(
                        "select entries must be strings".to_string(),
                    )
                })?;
                validate_read_identifier(name, schema_hint)?;
                cols.push(project_read_field(name, schema_hint, None));
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

/// Is `field` on the creator-facing read surface, per the descriptor?
///
/// The v2 descriptor stamps `readable` on every field it emits
/// (`crates/zeroship-migrate-core/src/render/gen_types.rs`, `stamp_physical_storage`),
/// and until the write projection landed nothing in Rust read it: the flag was
/// emitted, shipped, cached and ignored, so "the version tag means less than it
/// looks" was literally true of this key.
///
/// **An absent flag means readable.** `readable` narrows a declared field OUT of
/// the surface; it is not what puts it in. A field map that predates the stamp -
/// every hand-written test schema, and the `t.<field>()` maps `registerModel`
/// receives from a dev-tier build that did not go through the fold - carries no
/// flag at all, and refusing those would take the projection to zero columns
/// rather than to a narrower set.
fn field_is_readable(def: &Value) -> bool {
    def.get("readable").and_then(Value::as_bool).unwrap_or(true)
}

/// The physical column that carries `field`'s creator-visible value.
///
/// Read from the descriptor's `storage.valueColumn` rather than formatted from
/// the field name. The two agree on every artifact the fold emits today
/// (`gen_artifacts_byte_identical.rs` asserts `valueColumn == name`), so this
/// buys nothing observable now and everything later: a physical rename that
/// keeps the logical name is then a producer change, where a `format!` here
/// would name a column the table does not have.
///
/// It is deliberately NOT where the raw column is excluded. `storage.rawColumn`
/// is a sibling key of `valueColumn`, so a projection that reads `valueColumn`
/// cannot reach the raw column by any input - there is no branch to get wrong.
fn value_column_for_field(field: &str, schema_hint: &Value) -> String {
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
/// the value under its own authorization check and audit row (`crud::unmask`),
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

/// **L24** — the explicit read allowlist. TOTAL: it has no "no schema" arm.
///
/// Every caller previously treated `None` here as "emit `*`", which is the
/// projection failing open: mask siblings, raw columns and every other
/// platform-emitted physical column ride out of SQL. The schema is now a value
/// the caller holds, and a non-object one is an error rather than a fallback,
/// so there is no input to this function that produces an unrestricted
/// projection.
///
/// The seven system fields are projected unconditionally, ahead of any
/// `readable` test. They are not the creator's to withdraw: `id` is the row
/// identity every later stage routes by (the decrypt pass reads it for the AAD,
/// the mask pass stamps it into `_meta.row_pk`), and a descriptor that said
/// `readable: false` on it would produce rows no consumer in the pipeline can
/// use rather than a narrower read surface.
fn implicit_read_projection_parts(
    schema_hint: &Value,
    table_alias: Option<&str>,
) -> Result<Vec<String>, QueryError> {
    let schema_obj = schema_hint.as_object().ok_or_else(|| {
        QueryError::InvalidFilter(
            "read schema must be a field-map object; a read cannot be projected without one"
                .to_string(),
        )
    })?;
    let mut parts = Vec::with_capacity(SYSTEM_FIELD_NAMES.len() + schema_obj.len());
    for field in SYSTEM_FIELD_NAMES {
        parts.push(project_read_field(field, schema_hint, table_alias));
    }
    for (field, def) in schema_obj {
        if is_schema_metadata_key(field)
            || SYSTEM_FIELD_NAMES.contains(&field.as_str())
            || !field_is_readable(def)
        {
            continue;
        }
        parts.push(project_read_field(field, schema_hint, table_alias));
    }
    Ok(parts)
}

/// The `RETURNING` column list every write builder emits.
///
/// # Why the write path needs one at all
///
/// `RETURNING *` and `SELECT *` are refused outright for a role that holds
/// COLUMN-level grants: `*` expands to columns the role cannot read, so
/// PostgreSQL rejects the whole statement rather than the columns
/// (`ERROR: permission denied for table t`, measured on 17.11 for INSERT,
/// UPDATE and SELECT alike). A per-app role narrowed to the columns its app
/// declares therefore cannot execute a single write verb while the star is
/// there. Naming the columns is what makes column grants expressible.
///
/// # What it does NOT replace
///
/// `crud::read_pipeline::restrict_rows_to_surface` still runs, and must. This
/// narrows the rows one SQL statement returns; that stage narrows every row that
/// reaches a creator, including the ones no statement here produced - the WAL
/// consumer decodes pgoutput with no schema in reach and no projection to apply.
/// The two overlap on the write path on purpose: the projection is the boundary
/// the database enforces, the predicate is the boundary the runtime enforces,
/// and the second is the only one that covers logical decoding.
///
/// Returns the bare comma-joined list, without the `RETURNING` keyword, because
/// `build_find_or_create` appends a computed column after it
/// (`(xmax = 0) AS __created`).
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

/// Every column name a decoded row may carry across the JS boundary.
///
/// The seven system fields, every declared field's LOGICAL name, and
/// [`SYNTHETIC_RESULT_COLUMNS`]. A physical column that is not one of those - a
/// raw column, an auxiliary shadow-table key - is not on this surface and is
/// removed before the row is serialised.
///
/// # A row predicate AS WELL AS a `RETURNING` column list
///
/// This paragraph used to read "rather than", and argued that narrowing the
/// twelve `RETURNING *` sites "would not close the hole it appears to close".
/// That argument was sound about EXPOSURE and is why this predicate still runs:
/// the same rows feed the change broker, and on a deployed Postgres app the
/// authoritative event source is the WAL consumer, which zips every physical
/// column out of pgoutput with no schema in sight. A predicate over key names
/// applies to both; a projection applies only to statements this file emits.
///
/// It was answering the wrong question. The projection is not a second attempt
/// at the same containment - it is what makes the statements EXECUTABLE for a
/// role holding column-level grants, because `*` expands to columns the role
/// cannot read and PostgreSQL then refuses the whole statement. See
/// [`build_returning_expr`]. Both now exist, and neither is redundant: the
/// projection is the boundary the database enforces, this predicate is the
/// boundary the runtime enforces, and only the second covers logical decoding.
///
/// The union of system fields and declared fields is not a choice between the
/// two: `id` is minted SDK-side and read back out of the `RETURNING` row,
/// `version` and `updated_at` are auto-bumped in SQL, `deleted_at` is what
/// soft-delete writes, and none of the seven is necessarily a descriptor key.
///
/// Note this set is deliberately WIDER than what [`build_returning_expr`]
/// projects: it admits every declared key, including one the descriptor marks
/// `readable: false`. A predicate that drops keys is the wrong place to enforce
/// a read surface the projection has already refused to produce - if an
/// unreadable column ever appears in a row here it arrived from logical
/// decoding, where narrowing it silently would hide the fact.
#[must_use]
pub fn read_surface_columns(schema_hint: &Value) -> std::collections::BTreeSet<String> {
    let mut out: std::collections::BTreeSet<String> = SYSTEM_FIELD_NAMES
        .iter()
        .chain(SYNTHETIC_RESULT_COLUMNS.iter())
        .map(|name| (*name).to_string())
        .collect();
    if let Some(obj) = schema_hint.as_object() {
        for field in obj.keys() {
            if is_schema_metadata_key(field) {
                continue;
            }
            out.insert(field.clone());
        }
    }
    out
}

/// Does the column named `name` declare a non-`none`
/// `.mask({...})` entry on `schema_hint`? Returns `false` when the
/// schema is missing, the column is absent from it, or the mask is the
/// explicit opt-out (`kind: "none"`).
pub fn column_is_masked(name: &str, schema_hint: &Value) -> bool {
    let Some(schema_obj) = schema_hint.as_object() else {
        return false;
    };
    let Some(def) = schema_obj.get(name) else {
        return false;
    };
    let Some(mask) = def.get("mask").and_then(|v| v.as_object()) else {
        return false;
    };
    let kind = mask.get("kind").and_then(|v| v.as_str()).unwrap_or("full");
    kind != "none"
}

// There is deliberately NO `read_column_for` here any more, and no
// `aggregate_read_ident`.
//
// They existed to substitute `"<col>_masked" AS "<col>"` on every read surface
// while `<col>` held plaintext, and the substitution is what the storage flip
// deleted. Every read surface - the implicit projection, an explicit `select`,
// `$group.by`, the aggregate accumulators, `$having`, `distinct`, `orderBy`,
// the vector and spatial builders - now names the field's own column, which
// holds the mask, so there is nothing to derive and nothing to keep in
// agreement.
//
// This is the point of the flip rather than a side effect of it. The old shape
// needed EVERY builder to ask "is this masked?"; the ones that asked were
// correct and the one that could not - `build_where`, which takes no schema at
// all - compared against plaintext, so `find({ ssn: { $gt: v } })` plus
// `orderBy` plus `limit` binary-searched a value the caller could not read,
// with no authorization check on the path and no audit row written. A function
// that maps a logical field to some other physical column is exactly the
// asymmetry that produced it, so it is gone rather than corrected.

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
    app_id: &str,
    collection: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_count_with_soft_delete(app_id, collection, filter, false)
}

/// COUNT(*) with the soft-delete auto-filter. The CRUD
/// dispatch path threads `should_filter_soft_deleted` through here so
/// `db.posts.count()` on a post-migration table excludes soft-deleted
/// rows by default.
pub fn build_count_with_soft_delete(
    app_id: &str,
    collection: &str,
    filter: &Value,
    filter_soft_deleted: bool,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("SELECT COUNT(*) AS count FROM {schema}.{table}");
    let composed_where = compose_where_with_soft_delete(&where_clause, filter_soft_deleted);
    if !composed_where.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&composed_where);
    }

    Ok(BuiltQuery { sql, params })
}

/// Build an INSERT query:
/// `INSERT INTO "app_id"."collection" (...) VALUES (...) RETURNING "id", ...`
///
/// PG-flavour wrapper around [`build_insert_with_dialect`]. Every
/// existing CRUD call site stays on this signature — the orchestrator's
/// `dispatch_insert` path still goes through Postgres today.
pub fn build_insert(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    doc: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_insert_with_dialect(app_id, collection, schema_hint, doc, SqlDialect::Postgres)
}

/// Dialect-aware INSERT builder.
///
/// PG emits `decode($N, 'base64')::bytea` for encrypted columns
/// (preserved byte-for-byte); SQLite emits `$N` and tags the
/// param value with [`SQLITE_BINARY_BIND_PREFIX`] so the session actor
/// can bind the raw bytes as BLOB. Non-encrypted columns are
/// dialect-agnostic on both arms.
pub fn build_insert_with_dialect(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    doc: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

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

    // A write pass marks each binary-bind column with a sibling
    // `__zsbin__<col>` key (`Value::Bool(true)`); the value at `<col>`
    // is base64 text that the column wants as raw bytes. Walk the doc
    // once to collect those marker keys so we can:
    //   1. Skip emitting marker keys as columns.
    //   2. Wrap binary-bind placeholders with the dialect's bind
    //      shape ([`SqlDialect::binary_bind_placeholder`]).
    let binary_bind_cols = collect_binary_bind_cols(obj);

    let mut columns = Vec::new();
    let mut placeholders = Vec::new();
    let mut params: Vec<String> = Vec::new();

    for (key, value) in obj {
        if key.starts_with("__zsbin__") {
            continue;
        }
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
            let raw = value_to_param(value);
            let param_value = if is_binary_bind {
                dialect.wrap_binary_bind_param(raw)
            } else {
                raw
            };
            params.push(param_value);
            let n = params.len();
            if is_binary_bind {
                placeholders.push(dialect.binary_bind_placeholder(n));
            } else {
                placeholders.push(format!("${n}"));
            }
        }
    }

    let sql = format!(
        "INSERT INTO {schema}.{table} ({}) VALUES ({}) RETURNING {returning}",
        columns.join(", "),
        placeholders.join(", ")
    );

    Ok(BuiltQuery { sql, params })
}

/// Collect the set of column names a write pass has marked as
/// binary-bind: the param at that column is base64 TEXT and the column
/// wants RAW BYTES. The marker is a sibling key `__zsbin__<col> = true`,
/// inserted by `crate::crud::encryption_pass::encrypt_row_on_write` for
/// ciphertext and by `crate::crud::bytes_pass::encode_bytes_on_write`
/// for a plain `t.bytes()` column. Callers walk the doc once with this
/// set to know which placeholders need the
/// `decode($N, 'base64')::bytea` cast.
pub fn collect_binary_bind_cols(
    obj: &serde_json::Map<String, Value>,
) -> std::collections::HashSet<&str> {
    let mut out = std::collections::HashSet::new();
    for (k, v) in obj {
        if let Some(name) = k.strip_prefix("__zsbin__") {
            if v.as_bool() == Some(true) {
                out.insert(name);
            }
        }
    }
    out
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
    build_set_clauses_with_dialect(update, params, SqlDialect::Postgres)
}

/// Knobs the SET-clause builder needs to compose the
/// platform's auto-bump system-field SET clauses correctly.
///
/// Three independent bumps, each suppressed when the creator's patch
/// already provided an explicit value for that column (per
/// `zeroship_plugin_db::crud::system_fields_pass::apply_system_fields_on_update`
/// which inspects the patch and surfaces these flags via
/// `UpdateAutoBumpHints`):
///
/// 1. `version` → `"version" = "version" + 1` — every UPDATE bumps,
///    unless `skip_version` is true (creator supplied an explicit
///    value).
/// 2. `updated_at` → `"updated_at" = NOW()` (PG) / `CURRENT_TIMESTAMP`
///    (SQLite) — same skip rule.
/// 3. `updated_by` → `"updated_by" = $N` bound to `actor_id` — emitted
///    only when an actor is in scope AND `skip_updated_by` is false.
///
/// `Default::default()` produces the "no auto-bump" shape, used by the
/// existing dispatch-free callers (e.g. raw SQL tests, the
/// `build_set_clauses_with_dialect` wrapper) so behaviour
/// outside the dispatch path is unchanged.
#[derive(Debug, Clone, Default)]
pub struct SystemFieldAutoBump<'a> {
    /// `true` when the call came from the CRUD dispatch path rather than a
    /// legacy direct builder caller. Controls whether version auto-bumps run
    /// for anonymous writes.
    pub dispatch_write: bool,
    /// Bind value for the `updated_by` placeholder. When `None`, the
    /// `updated_by` SET clause is suppressed (no actor in scope —
    /// matches the INSERT path's "leave NULL when anonymous"
    /// behaviour). When `Some`, the column is bound to the
    /// typed_id string.
    pub actor_id: Option<&'a str>,
    /// `true` when the creator's patch carried an explicit `version`.
    /// Suppresses the `"version" = "version" + 1` auto-bump so the
    /// explicit value wins.
    pub skip_version: bool,
    /// Same as `skip_version` for `updated_at`. Suppresses the
    /// dialect-appropriate `NOW()` / `CURRENT_TIMESTAMP` auto-bump.
    pub skip_updated_at: bool,
    /// Same as `skip_version` for `updated_by`. Suppresses the
    /// actor-bound `$N` SET clause.
    pub skip_updated_by: bool,
}

/// Dialect-aware SET-clause builder for `build_update_one` /
/// `build_update_many`. PG keeps the `decode($N, 'base64')::bytea` cast;
/// SQLite emits a plain `$N` and
/// tags the encrypted-column param value with [`SQLITE_BINARY_BIND_PREFIX`].
///
/// Emits the dialect-appropriate `updated_at` auto-bump
/// (`NOW()` on PG, `CURRENT_TIMESTAMP` on SQLite). To compose the full
/// `version` / `updated_at` / `updated_by` auto-bump set used by the
/// CRUD dispatch path, callers should use
/// [`build_set_clauses_with_system_fields`] instead — this wrapper
/// only auto-bumps `updated_at`, matching behaviour for
/// existing direct callers.
pub fn build_set_clauses_with_dialect(
    update: &Value,
    params: &mut Vec<String>,
    dialect: SqlDialect,
) -> Result<Vec<String>, QueryError> {
    // The default auto-bump is empty (no version bump, no updated_by) —
    // preserves the contract for callers that don't need auto-bump.
    build_set_clauses_with_system_fields(
        update,
        params,
        dialect,
        &SystemFieldAutoBump::default(),
    )
}

/// SET-clause builder + system-field auto-bump pass.
///
/// Mirrors [`build_set_clauses_with_dialect`] for the creator-supplied
/// portion of the SET clause (encryption-aware, operator-aware,
/// `$set`-flattening), then appends — strictly AFTER all creator
/// clauses, grep-friendly ordering — the three platform auto-bumps the
/// system-field contract requires:
///
/// ```text
///   "version"    = "version" + 1            (unless skip_version)
///   "updated_at" = NOW() / CURRENT_TIMESTAMP (unless skip_updated_at)
///   "updated_by" = $N                       (unless skip_updated_by OR
///                                            actor_id is None)
/// ```
///
/// The auto-bump SET clauses bypass the encryption / mask passes by
/// construction — they are appended AFTER the encryption-aware loop
/// runs, with their own SQL fragments and bind params (using the
/// running `$N` counter so encryption-pass `$N` claims don't collide
/// with the auto-bump's `$N`). System fields are platform-managed
/// plaintext; routing them through encryption / masking would corrupt
/// the on-disk values.
pub fn build_set_clauses_with_system_fields(
    update: &Value,
    params: &mut Vec<String>,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<Vec<String>, QueryError> {
    let update_obj = update
        .as_object()
        .ok_or_else(|| QueryError::InvalidFilter("update must be an object".to_string()))?;

    // Collect binary-bind markers from the update
    // doc (top-level AND nested `$set`). The write pass deposits
    // both the base64 value and a `__zsbin__<col>` marker; we use the
    // marker set to wrap the placeholder via the dialect's bind shape
    // (`SqlDialect::binary_bind_placeholder`).
    let mut binary_bind_cols = collect_binary_bind_cols(update_obj);
    if let Some(set_obj) = update_obj.get("$set").and_then(|v| v.as_object()) {
        binary_bind_cols.extend(collect_binary_bind_cols(set_obj));
    }

    // Collect all fields: flatten $set inline, keep other keys as-is.
    // Skip every `__zsbin__*` marker key: they are a side-channel from
    // the write passes, not user-declared columns.
    let mut fields: Vec<(&String, &Value)> = Vec::new();
    for (key, value) in update_obj.iter() {
        if key.starts_with("__zsbin__") {
            continue;
        }
        if key == "$set" {
            // Flatten $set fields into the top level
            let obj = value
                .as_object()
                .ok_or_else(|| QueryError::InvalidFilter("$set must be an object".to_string()))?;
            for (k, v) in obj.iter() {
                if k.starts_with("__zsbin__") {
                    continue;
                }
                fields.push((k, v));
            }
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
                        let is_binary_bind = binary_bind_cols.contains(key.as_str());
                        let raw = value_to_param(op_val);
                        let param_value = if is_binary_bind {
                            dialect.wrap_binary_bind_param(raw)
                        } else {
                            raw
                        };
                        params.push(param_value);
                        let n = params.len();
                        if is_binary_bind {
                            format!("{col} = {}", dialect.binary_bind_placeholder(n))
                        } else {
                            format!("{col} = ${n}")
                        }
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
        let is_binary_bind = binary_bind_cols.contains(key.as_str());
        let raw = value_to_param(value);
        let param_value = if is_binary_bind {
            dialect.wrap_binary_bind_param(raw)
        } else {
            raw
        };
        params.push(param_value);
        let n = params.len();
        if is_binary_bind {
            set_clauses.push(format!("{col} = {}", dialect.binary_bind_placeholder(n)));
        } else {
            set_clauses.push(format!("{col} = ${n}"));
        }
    }

    // System-field auto-bump SET clauses. Appended AFTER
    // every creator-supplied clause (encryption-pass / mask-pass output
    // included) so the diff against the creator's patch is grep-able
    // AND so the auto-bumps bypass encryption / masking by
    // construction. Each bump skipped when the creator's patch
    // explicitly supplied that column (the value flows through the
    // standard SET loop above; the explicit value wins per Q-SF-B).
    //
    // For direct callers, the
    // "auto-bump updated_at when not explicit" path stays
    // unchanged: when the caller passed `SystemFieldAutoBump::default()`
    // (the wrapper from `build_set_clauses_with_dialect`), the only
    // bump emitted is `updated_at` and it inspects the existing
    // `set_clauses` for an explicit override. The `autobump.skip_*`
    // flags are only ever set by the CRUD dispatch path
    // (`apply_system_fields_on_update` populates the hints).
    let already_has_updated_at = set_clauses.iter().any(|c| c.contains("\"updated_at\""));
    let already_has_version = set_clauses.iter().any(|c| c.contains("\"version\""));
    let already_has_updated_by = set_clauses.iter().any(|c| c.contains("\"updated_by\""));

    // `version` auto-bump fires only on the CRUD dispatch path
    // (signalled by an `actor_id` being threaded through OR by an
    // explicit `skip_version = false` from the caller's hints). To
    // keep the direct-caller contract intact, we use a
    // discriminator: direct callers always pass `actor_id = None`
    // AND `skip_version = false` (the `Default::default()` shape) —
    // we only emit the version bump when `actor_id.is_some()` OR the
    // caller asked for it explicitly via a `skip_updated_by = true`
    // setting (which is impossible from the default and only set by
    // the dispatch-path helper). The actor presence is the discriminator
    // because direct callers never thread one through.
    let on_pr4_dispatch_path = autobump.dispatch_write;
    if on_pr4_dispatch_path && !autobump.skip_version && !already_has_version {
        set_clauses.push("\"version\" = \"version\" + 1".to_string());
    }

    // `updated_at` auto-bump — dialect-aware (PG `NOW()` /
    // SQLite `CURRENT_TIMESTAMP`). This fires on BOTH paths (CRUD
    // dispatch AND direct callers) — every UPDATE emits
    // `updated_at = NOW()` regardless of caller.
    if !autobump.skip_updated_at && !already_has_updated_at {
        let ts_expr = renderer(dialect).current_timestamp_expr();
        set_clauses.push(format!("\"updated_at\" = {ts_expr}"));
    }

    // `updated_by` auto-bump — actor-bound. Fires only on the CRUD
    // dispatch path when an actor is in scope (anonymous writes leave
    // `updated_by` untouched, mirroring the INSERT "NULL when no
    // session actor" rule).
    if let Some(actor) = autobump.actor_id {
        if !autobump.skip_updated_by && !already_has_updated_by {
            params.push(actor.to_string());
            let n = params.len();
            set_clauses.push(format!("\"updated_by\" = ${n}"));
        }
    }

    Ok(set_clauses)
}

/// Build an UPDATE query:
/// `UPDATE "app_id"."collection" SET ... WHERE ctid = (...) RETURNING "id", ...`
///
/// PG-flavour wrapper — every existing call site goes through Postgres.
pub fn build_update_one(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_update_one_with_dialect(
        app_id,
        collection,
        schema_hint,
        filter,
        update,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware `updateOne` builder. Encrypted-column
/// binds follow the dialect's
/// [`SqlDialect::binary_bind_placeholder`]. The `ctid` subquery
/// shape is PG-specific (`SqlDialect::Sqlite` callers should rebuild
/// the LIMIT 1 narrowing differently — out of scope here; the
/// builder body remains PG-shaped here).
pub fn build_update_one_with_dialect(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    build_update_one_with_system_fields(
        app_id,
        collection,
        schema_hint,
        filter,
        update,
        dialect,
        &SystemFieldAutoBump::default(),
    )
}

/// Dialect-aware `updateOne` builder + system-field
/// auto-bump. The CRUD dispatch path uses this so every UPDATE
/// transparently bumps `version` + `updated_at` + `updated_by` (per
/// the `autobump` knobs). Direct callers that need SQL without the
/// auto-bumps keep using [`build_update_one_with_dialect`].
pub fn build_update_one_with_system_fields(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses =
        build_set_clauses_with_system_fields(update, &mut params, dialect, autobump)?;

    let where_clause = build_where(filter, &mut params)?;

    let inner_where = if where_clause.is_empty() {
        String::new()
    } else {
        format!(" WHERE {where_clause}")
    };
    let target_col = match dialect {
        SqlDialect::Postgres => "ctid",
        SqlDialect::Sqlite => "rowid",
        SqlDialect::Mysql => "id",
    };
    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{inner_where} LIMIT 1) RETURNING {returning}",
        set_clauses.join(", "),
    );

    Ok(BuiltQuery { sql, params })
}

/// Build an INSERT query for multiple documents:
/// `INSERT INTO "app_id"."collection" ("col1", "col2") VALUES ($1, $2), ($3, $4) RETURNING "id", ...`
///
/// Column names are unioned across documents; a missing or explicit null value
/// is emitted as a SQL `NULL` literal and consumes no bind parameter.
///
/// PG-flavour wrapper around [`build_insert_many_with_dialect`].
pub fn build_insert_many(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    docs: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_insert_many_with_dialect(app_id, collection, schema_hint, docs, SqlDialect::Postgres)
}

/// Dialect-aware `insertMany` builder.
pub fn build_insert_many_with_dialect(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    docs: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
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

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    // Binary-bind union across all docs. We treat
    // a column as binary-bind iff ANY doc carries the `__zsbin__<col>`
    // marker (the write passes mark every doc consistently: the
    // decision is schema-driven, so every row in the batch gets the
    // marker after the pass runs).
    let mut binary_bind_cols: std::collections::HashSet<String> = std::collections::HashSet::new();
    for doc in arr {
        if let Some(obj) = doc.as_object() {
            for name in collect_binary_bind_cols(obj) {
                binary_bind_cols.insert(name.to_string());
            }
        }
    }

    // Union all columns across all documents (not just the first).
    // Skip every `__zsbin__*` marker key: they are a side-channel from
    // the write passes, not user-declared columns.
    let mut column_set = std::collections::BTreeSet::<&String>::new();
    let mut bind_count = 0usize;
    for doc in arr {
        let obj = doc.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("insertMany: each document must be an object".to_string())
        })?;
        for (key, value) in obj {
            if key.starts_with("__zsbin__") {
                continue;
            }
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
        SqlDialect::Mysql => ("MySQL", POSTGRES_MAX_BIND_PARAMETERS),
    };
    if bind_count > bind_limit {
        return Err(QueryError::InvalidFilter(format!(
            "insertMany batch has {bind_count} non-null field values; one {dialect_name} insertMany call supports at most {bind_limit}; split the documents into smaller batches"
        )));
    }

    let column_names: Vec<&String> = column_set.into_iter().collect();
    let columns: Vec<String> = column_names.iter().map(|k| quote_ident(k)).collect();

    let mut params: Vec<String> = Vec::with_capacity(bind_count);
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
                let raw = value_to_param(val);
                let param_value = if is_binary_bind {
                    dialect.wrap_binary_bind_param(raw)
                } else {
                    raw
                };
                params.push(param_value);
                let n = params.len();
                if is_binary_bind {
                    placeholders.push(dialect.binary_bind_placeholder(n));
                } else {
                    placeholders.push(format!("${n}"));
                }
            }
        }
        value_groups.push(format!("({})", placeholders.join(", ")));
    }

    let sql = format!(
        "INSERT INTO {schema}.{table} ({}) VALUES {} RETURNING {returning}",
        columns.join(", "),
        value_groups.join(", ")
    );

    Ok(BuiltQuery { sql, params })
}

/// Build an UPDATE query for multiple rows (no LIMIT 1):
/// `UPDATE "app_id"."collection" SET ... WHERE ... RETURNING "id", ...`
///
/// PG-flavour wrapper around [`build_update_many_with_dialect`].
pub fn build_update_many(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_update_many_with_dialect(
        app_id,
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
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    build_update_many_with_system_fields(
        app_id,
        collection,
        schema_hint,
        filter,
        update,
        dialect,
        &SystemFieldAutoBump::default(),
    )
}

/// Dialect-aware `updateMany` builder + system-field
/// auto-bump. Same auto-bump semantics as
/// [`build_update_one_with_system_fields`]; CRUD dispatch path uses
/// this to keep the bulk-update SQL emitting `version` + `updated_at`
/// + `updated_by` bumps even on multi-row updates.
pub fn build_update_many_with_system_fields(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses =
        build_set_clauses_with_system_fields(update, &mut params, dialect, autobump)?;

    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("UPDATE {schema}.{table} SET {}", set_clauses.join(", "));
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(" RETURNING ");
    sql.push_str(&returning);

    Ok(BuiltQuery { sql, params })
}

/// Build a DELETE query for multiple rows (no LIMIT 1):
/// `DELETE FROM "app_id"."collection" WHERE ... RETURNING "id", ...`
pub fn build_delete_many(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("DELETE FROM {schema}.{table}");
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(" RETURNING ");
    sql.push_str(&returning);

    Ok(BuiltQuery { sql, params })
}

/// Build a DELETE query:
/// `DELETE FROM "app_id"."collection" WHERE ... RETURNING "id", ...`
pub fn build_delete_one(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_delete_one_with_dialect(app_id, collection, schema_hint, filter, SqlDialect::Postgres)
}

/// Dialect-aware single-row DELETE builder.
pub fn build_delete_one_with_dialect(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let target_col = match dialect {
        SqlDialect::Postgres => "ctid",
        SqlDialect::Sqlite => "rowid",
        SqlDialect::Mysql => "id",
    };
    let sql = format!(
        "DELETE FROM {schema}.{table} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{} LIMIT 1) RETURNING {returning}",
        if where_clause.is_empty() {
            String::new()
        } else {
            format!(" WHERE {where_clause}")
        }
    );

    Ok(BuiltQuery { sql, params })
}

// ---------------------------------------------------------------------------
// Soft-delete / restore SQL builders.
//
// `delete()` on a post-migration table becomes an UPDATE that flips
// `deleted_at` from NULL to `NOW()` / `CURRENT_TIMESTAMP`. The
// builders mirror `build_update_*_with_system_fields` but stamp the
// `deleted_at` SET clause themselves (system-field, not creator-
// supplied) and add `AND deleted_at IS NULL` to the WHERE clause so
// re-deleting an already-deleted row is a no-op (affected-rows = 0).
//
// `restore()` is the symmetric UPDATE: `deleted_at = NULL` with
// `AND deleted_at IS NOT NULL` so restoring a live row is a no-op.
//
// Both bump `version` + `updated_at` + `updated_by` via the same
// `SystemFieldAutoBump` knob the UPDATE path uses; the SET clauses are
// composed inline (rather than routing through
// `build_set_clauses_with_system_fields`) because the creator's "patch"
// for soft-delete / restore is fixed by the platform — only the actor
// and the timestamp expression differ from the auto-bump set.
// ---------------------------------------------------------------------------

/// Dialect-appropriate `NOW()` / `CURRENT_TIMESTAMP`
/// expression for stamping a `deleted_at` column on the soft-delete
/// path. Mirrors the same lookup
/// [`build_set_clauses_with_system_fields`] does for `updated_at`.
fn now_expr(dialect: SqlDialect) -> &'static str {
    renderer(dialect).current_timestamp_expr()
}

/// Compose the SET clauses for a soft-delete: the
/// `deleted_at` stamp + the standard `version` / `updated_at` /
/// `updated_by` auto-bump triple (per the `autobump` knobs).
///
/// `actor_id` flows through into the `updated_by` placeholder when
/// non-null; the dialect-flag picks the timestamp expression for both
/// `deleted_at` and `updated_at`. `skip_*` knobs work identically to
/// [`build_set_clauses_with_system_fields`].
///
/// SET clause ordering (grep-friendly diff): `deleted_at` first
/// (the soft-delete-specific stamp), then the standard `version` /
/// `updated_at` / `updated_by` bumps in that order.
fn build_soft_delete_set_clauses(
    params: &mut Vec<String>,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Vec<String> {
    let now = now_expr(dialect);
    let mut clauses = vec![format!("\"deleted_at\" = {now}")];
    if !autobump.skip_version {
        clauses.push("\"version\" = \"version\" + 1".to_string());
    }
    if !autobump.skip_updated_at {
        clauses.push(format!("\"updated_at\" = {now}"));
    }
    if let Some(actor) = autobump.actor_id {
        if !autobump.skip_updated_by {
            params.push(actor.to_string());
            let n = params.len();
            clauses.push(format!("\"updated_by\" = ${n}"));
        }
    }
    clauses
}

/// Compose the SET clauses for `restore()`: clear
/// `deleted_at` + bump the standard triple. Symmetric to
/// [`build_soft_delete_set_clauses`]. The timestamp expression isn't
/// needed for `deleted_at` here (we write `NULL` directly, not a stamp).
fn build_restore_set_clauses(
    params: &mut Vec<String>,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Vec<String> {
    let now = now_expr(dialect);
    let mut clauses = vec!["\"deleted_at\" = NULL".to_string()];
    if !autobump.skip_version {
        clauses.push("\"version\" = \"version\" + 1".to_string());
    }
    if !autobump.skip_updated_at {
        clauses.push(format!("\"updated_at\" = {now}"));
    }
    if let Some(actor) = autobump.actor_id {
        if !autobump.skip_updated_by {
            params.push(actor.to_string());
            let n = params.len();
            clauses.push(format!("\"updated_by\" = ${n}"));
        }
    }
    clauses
}

/// Dialect-aware `soft_delete_one` builder. Used by the
/// CRUD dispatch path on post-migration tables when `delete()` /
/// `deleteOne()` reaches a row that hasn't already been soft-deleted.
///
/// Generated SQL example (PG):
/// ```sql
/// UPDATE "app1"."posts"
/// SET "deleted_at" = NOW(), "version" = "version" + 1, "updated_at" = NOW(), "updated_by" = $2
/// WHERE ctid = (
///   SELECT ctid FROM "app1"."posts" WHERE "id" = $1 AND "deleted_at" IS NULL LIMIT 1
/// )
/// RETURNING "id", "created_at", ...
/// ```
///
/// The `AND deleted_at IS NULL` in the inner SELECT keeps the call
/// idempotent: re-deleting an already-deleted row affects 0 rows. The
/// dispatch layer translates 0-affected to a `null` result (matches the
/// `deleteOne` contract).
pub fn build_soft_delete_one_with_system_fields(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses = build_soft_delete_set_clauses(&mut params, dialect, autobump);

    let where_clause = build_where(filter, &mut params)?;
    // The inner SELECT scopes the soft-delete to a single live row.
    // If the filter is empty the WHERE becomes just `deleted_at IS
    // NULL` (any single live row). The dispatch path doesn't call this
    // builder with an empty filter — `Collection::delete()` requires an
    // id or filter — but we mirror the same defensive behaviour as
    // `build_delete_one`.
    let inner_where = if where_clause.is_empty() {
        " WHERE \"deleted_at\" IS NULL".to_string()
    } else {
        format!(" WHERE {where_clause} AND \"deleted_at\" IS NULL")
    };

    let target_col = match dialect {
        SqlDialect::Postgres => "ctid",
        SqlDialect::Sqlite => "rowid",
        SqlDialect::Mysql => "id",
    };
    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{inner_where} LIMIT 1) RETURNING {returning}",
        set_clauses.join(", "),
    );

    Ok(BuiltQuery { sql, params })
}

/// Dialect-aware `soft_delete_many` builder. Same shape
/// as [`build_soft_delete_one_with_system_fields`] minus the `ctid`
/// LIMIT 1 narrowing — every live row matching `filter` flips
/// `deleted_at` to the dialect's `NOW()`-equivalent.
///
/// `AND deleted_at IS NULL` is preserved so re-deleting an already-
/// deleted row is still a no-op.
pub fn build_soft_delete_many_with_system_fields(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses = build_soft_delete_set_clauses(&mut params, dialect, autobump);

    let where_clause = build_where(filter, &mut params)?;
    let where_sql = if where_clause.is_empty() {
        " WHERE \"deleted_at\" IS NULL".to_string()
    } else {
        format!(" WHERE {where_clause} AND \"deleted_at\" IS NULL")
    };

    let sql = format!(
        "UPDATE {schema}.{table} SET {}{where_sql} RETURNING {returning}",
        set_clauses.join(", "),
    );

    Ok(BuiltQuery { sql, params })
}

/// Dialect-aware `restore_one` builder. Symmetric to
/// [`build_soft_delete_one_with_system_fields`]: clears `deleted_at`
/// and scopes to rows that are CURRENTLY soft-deleted
/// (`deleted_at IS NOT NULL`) so restoring a live row is a no-op
/// (affected-rows = 0 → typed `not_found_or_already_live` via the
/// dispatch layer).
pub fn build_restore_one_with_system_fields(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses = build_restore_set_clauses(&mut params, dialect, autobump);

    let where_clause = build_where(filter, &mut params)?;
    let inner_where = if where_clause.is_empty() {
        " WHERE \"deleted_at\" IS NOT NULL".to_string()
    } else {
        format!(" WHERE {where_clause} AND \"deleted_at\" IS NOT NULL")
    };

    let target_col = match dialect {
        SqlDialect::Postgres => "ctid",
        SqlDialect::Sqlite => "rowid",
        SqlDialect::Mysql => "id",
    };
    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{inner_where} LIMIT 1) RETURNING {returning}",
        set_clauses.join(", "),
    );

    Ok(BuiltQuery { sql, params })
}

/// Dialect-aware `restore_many` builder.
pub fn build_restore_many_with_system_fields(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    filter: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses = build_restore_set_clauses(&mut params, dialect, autobump);

    let where_clause = build_where(filter, &mut params)?;
    let where_sql = if where_clause.is_empty() {
        " WHERE \"deleted_at\" IS NOT NULL".to_string()
    } else {
        format!(" WHERE {where_clause} AND \"deleted_at\" IS NOT NULL")
    };

    let sql = format!(
        "UPDATE {schema}.{table} SET {}{where_sql} RETURNING {returning}",
        set_clauses.join(", "),
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
///
/// Thin shim around
/// [`build_aggregate_with_soft_delete`] passing
/// `filter_soft_deleted = false`. CRUD dispatch threads the auto-
/// filter through the dedicated entry; direct callers keep the same
/// SQL byte-identical.
pub fn build_aggregate(
    app_id: &str,
    collection: &str,
    pipeline: &Value,
    schema_hint: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_aggregate_with_soft_delete(app_id, collection, pipeline, false, schema_hint)
}

/// Aggregate builder with the soft-delete auto-filter.
///
/// When `filter_soft_deleted = true`, appends `AND deleted_at IS NULL`
/// to whatever WHERE clause the pipeline's `$match` stage produced
/// (or `WHERE deleted_at IS NULL` when no `$match` is present).
pub fn build_aggregate_with_soft_delete(
    app_id: &str,
    collection: &str,
    pipeline: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_aggregate_with_soft_delete_with_dialect(
        app_id,
        collection,
        pipeline,
        filter_soft_deleted,
        schema_hint,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware variant of [`build_aggregate_with_soft_delete`].
pub fn build_aggregate_with_soft_delete_with_dialect(
    app_id: &str,
    collection: &str,
    pipeline: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    build_aggregate_with_result_columns(
        app_id,
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
    app_id: &str,
    collection: &str,
    pipeline: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<(BuiltQuery, Option<Vec<String>>), QueryError> {
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
            where_clause = build_where(match_val, &mut params)?;
        } else if let Some(group_val) = obj.get("$group") {
            let group_obj = group_val.as_object().ok_or_else(|| {
                QueryError::InvalidFilter("aggregate: $group must be an object".to_string())
            })?;

            // Handle optional `by` field
            if let Some(by_val) = group_obj.get("by") {
                match by_val {
                    Value::String(s) => {
                        validate_read_identifier(s, schema_hint)?;
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
                        // SEC-4: read the masked sibling for masked columns.
                        format!("SUM({})", quote_ident(field))
                    }
                    "$avg" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$avg requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        format!("AVG({})", quote_ident(field))
                    }
                    "$min" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$min requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        format!("MIN({})", quote_ident(field))
                    }
                    "$max" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$max requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        format!("MAX({})", quote_ident(field))
                    }
                    "$first" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$first requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        // SEC-4: read the masked sibling for masked columns.
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
            having_clause = build_having(having_val, &mut params, &agg_exprs, schema_hint)?;
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
            // the masked sibling, not plaintext. Aggregate aliases
            // (`agg_exprs`) order by the alias name as-is.
            order_clause =
                build_aggregate_order_by(sort_val, dialect, &agg_exprs, schema_hint)?;
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

    let composed_where = compose_where_with_soft_delete(&where_clause, filter_soft_deleted);
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

    Ok((BuiltQuery { sql, params }, result_columns))
}

/// Build a SELECT DISTINCT query:
/// `SELECT DISTINCT "field" FROM "schema"."table" WHERE ... ORDER BY "field"`
///
/// Thin shim around
/// [`build_distinct_with_soft_delete`] passing
/// `filter_soft_deleted = false`.
pub fn build_distinct(
    app_id: &str,
    collection: &str,
    field: &str,
    filter: &Value,
    schema_hint: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_distinct_with_soft_delete(app_id, collection, field, filter, false, schema_hint)
}

/// DISTINCT builder with the soft-delete auto-filter.
pub fn build_distinct_with_soft_delete(
    app_id: &str,
    collection: &str,
    field: &str,
    filter: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_distinct_with_soft_delete_with_dialect(
        app_id,
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
    app_id: &str,
    collection: &str,
    field: &str,
    filter: &Value,
    filter_soft_deleted: bool,
    schema_hint: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    validate_read_identifier(field, schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let col = quote_ident(field);
    // DISTINCT over the field's own column. For a masked field that column
    // holds the mask, so `distinct("ssn")` enumerates MASKS - it can no longer
    // be one `read_column_for` disagreement away from enumerating the values
    // behind them.
    let select_expr = col.clone();

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("SELECT DISTINCT {select_expr} FROM {schema}.{table}");
    let composed_where = compose_where_with_soft_delete(&where_clause, filter_soft_deleted);
    if !composed_where.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&composed_where);
    }
    sql.push_str(" ORDER BY ");
    sql.push_str(&build_order_term(field, false, dialect));

    Ok(BuiltQuery { sql, params })
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
pub fn build_vector_search(
    app_id: &str,
    collection: &str,
    column: &str,
    query: &[f32],
    k: usize,
    metric: crate::descriptors::VectorMetric,
    filter: &Value,
    schema_hint: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    validate_read_identifier(column, schema_hint)?;
    validate_search_limit_bound("search.k", k)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let col = quote_ident(column);

    // pgvector operator per metric — see `crate::descriptors::VectorMetric`
    // doc-comment for the operator/opclass mapping.
    let op = match metric {
        crate::descriptors::VectorMetric::Cosine => "<=>",
        crate::descriptors::VectorMetric::L2 => "<->",
        crate::descriptors::VectorMetric::InnerProduct => "<#>",
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
    let mut params: Vec<String> = Vec::with_capacity(2 + 4);
    params.push(vec_lit);
    params.push(k.to_string());

    let where_clause = build_where(filter, &mut params)?;

    let select_expr = build_masked_aware_select_expr(None, schema_hint)?;
    let mut sql = format!(
        "SELECT {select_expr}, {col} {op} $1::vector AS _distance FROM {schema}.{table}"
    );
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(&format!(" ORDER BY {col} {op} $1::vector LIMIT $2"));

    Ok(BuiltQuery { sql, params })
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
/// The Rust trait surface ([`crate::descriptors::GeoPoint`]) keeps the
/// `{lat, lng}` shape; the swap happens here at the SQL boundary so the
/// JS/Rust contract stays in `(lat, lng)` order. `$3 = radius_m`,
/// `$4 = limit`. Filter parameters start at `$5`.
///
/// **Column type**: the indexed column must be
/// `geography(POINT, 4326)`. The PG DDL emitter ([`field_to_column_for_dialect`])
/// wires this when the schema field type is `geoPoint`.
pub fn build_spatial_near(
    app_id: &str,
    collection: &str,
    column: &str,
    point: crate::descriptors::GeoPoint,
    radius_m: f64,
    filter: &Value,
    limit: Option<usize>,
    schema_hint: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    validate_read_identifier(column, schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let col = quote_ident(column);

    let limit = limit.unwrap_or(100);
    validate_search_limit_bound("near.limit", limit)?;

    // Bind order: (lng, lat, radius_m, limit). Note the swap: ST_MakePoint
    // takes (x, y) = (lng, lat), the inverse of the SDK's {lat, lng}
    // input shape.
    let mut params: Vec<String> = Vec::with_capacity(4 + 4);
    params.push(point.lng.to_string());
    params.push(point.lat.to_string());
    params.push(radius_m.to_string());
    params.push(limit.to_string());

    let where_clause = build_where(filter, &mut params)?;

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
    schema_hint: &Value,
) -> Result<String, QueryError> {
    validate_clause_budget(filter, ClauseBudgetKind::Having)?;
    build_having_inner(filter, params, agg_exprs, schema_hint)
}

fn build_having_inner(
    filter: &Value,
    params: &mut Vec<String>,
    agg_exprs: &std::collections::HashMap<String, String>,
    schema_hint: &Value,
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
                                .map(|v| build_having_inner(v, params, agg_exprs, schema_hint))
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
                                .map(|v| build_having_inner(v, params, agg_exprs, schema_hint))
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
                    // Resolve alias → aggregate expression, or fall back to
                    // the quoted column. SEC-4: a masked base column in
                    // HAVING reads its masked sibling, never plaintext.
                    let col = if let Some(expr) = agg_exprs.get(key) {
                        expr.clone()
                    } else {
                        validate_read_identifier(key, schema_hint)?;
                        quote_ident(key)
                    };
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

/// Build a single HAVING condition. Like `build_field_condition_with_dialect` but takes
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
///
/// Visibility lifted from `fn` to `pub` so SQLite backend helpers can
/// compose a parametrised predicate fragment without rebuilding the filter
/// machinery. The body itself is unchanged - every existing call site keeps
/// its behaviour byte-for-byte.
pub fn build_where(filter: &Value, params: &mut Vec<String>) -> Result<String, QueryError> {
    build_where_with_dialect(filter, params, SqlDialect::Postgres)
}

pub fn build_where_with_dialect(
    filter: &Value,
    params: &mut Vec<String>,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    validate_clause_budget(filter, ClauseBudgetKind::Filter)?;
    build_where_with_dialect_inner(filter, params, dialect)
}

fn build_where_with_dialect_inner(
    filter: &Value,
    params: &mut Vec<String>,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
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
                                .map(|v| build_where_with_dialect_inner(v, params, dialect))
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
                                .map(|v| build_where_with_dialect_inner(v, params, dialect))
                                .collect();
                            let sub = sub?;
                            let non_empty: Vec<&str> =
                                sub.iter().filter(|s| !s.is_empty()).map(String::as_str).collect();
                            if !non_empty.is_empty() {
                                conditions.push(format!("({})", non_empty.join(" OR ")));
                            }
                        }
                        "$not" => {
                            let sub = build_where_with_dialect_inner(value, params, dialect)?;
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
                    let cond = build_field_condition_with_dialect(key, value, params, dialect)?;
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

fn build_field_condition_with_dialect(
    field: &str,
    value: &Value,
    params: &mut Vec<String>,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    validate_field_name(field)?;
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
                        if arr.len() > MAX_MEMBERSHIP_LIST_LEN {
                            return Err(QueryError::InvalidFilter(format!(
                                "$in exceeds the maximum of {MAX_MEMBERSHIP_LIST_LEN} values"
                            )));
                        }
                        // A null member means IS NULL, exactly as a bare
                        // `{field: null}` does in the equality arm above.
                        // It must NOT reach `value_to_param`, which maps
                        // `Value::Null` to the empty string - that silently
                        // matched empty-string rows and missed real NULLs.
                        // Binding a literal NULL is not the fix either:
                        // `x IN (NULL)` matches nothing.
                        let (nulls, values): (Vec<&Value>, Vec<&Value>) =
                            arr.iter().partition(|v| v.is_null());
                        let placeholders: Vec<String> = values
                            .iter()
                            .map(|v| {
                                params.push(value_to_param(v));
                                format!("${}", params.len())
                            })
                            .collect();
                        match (placeholders.is_empty(), nulls.is_empty()) {
                            // `$in: []` matches nothing. It must NOT emit
                            // `IN ()`, which is a syntax error in PostgreSQL
                            // and so failed the whole query rather than
                            // returning an empty result - and an empty list is
                            // the natural result of narrowing a filter to
                            // nothing, so it is reachable from ordinary code.
                            (true, true) => "FALSE".to_string(),
                            (true, false) => format!("{col} IS NULL"),
                            (false, true) => format!("{col} IN ({})", placeholders.join(", ")),
                            (false, false) => format!(
                                "({col} IN ({}) OR {col} IS NULL)",
                                placeholders.join(", ")
                            ),
                        }
                    }
                    "$nin" => {
                        let arr = val.as_array().ok_or_else(|| {
                            QueryError::InvalidFilter("$nin must be an array".to_string())
                        })?;
                        if arr.len() > MAX_MEMBERSHIP_LIST_LEN {
                            return Err(QueryError::InvalidFilter(format!(
                                "$nin exceeds the maximum of {MAX_MEMBERSHIP_LIST_LEN} values"
                            )));
                        }
                        // Mirror of `$in`: a null member excludes NULL rows,
                        // so it becomes IS NOT NULL rather than a bound
                        // empty string. See the `$in` arm for why binding a
                        // literal NULL is not the alternative.
                        let (nulls, values): (Vec<&Value>, Vec<&Value>) =
                            arr.iter().partition(|v| v.is_null());
                        let placeholders: Vec<String> = values
                            .iter()
                            .map(|v| {
                                params.push(value_to_param(v));
                                format!("${}", params.len())
                            })
                            .collect();
                        match (placeholders.is_empty(), nulls.is_empty()) {
                            // `$nin: []` excludes nothing, so it matches
                            // everything - the mirror of the `$in` case, and
                            // `NOT IN ()` is the same syntax error.
                            (true, true) => "TRUE".to_string(),
                            (true, false) => format!("{col} IS NOT NULL"),
                            (false, true) => format!("{col} NOT IN ({})", placeholders.join(", ")),
                            (false, false) => format!(
                                "({col} NOT IN ({}) AND {col} IS NOT NULL)",
                                placeholders.join(", ")
                            ),
                        }
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
                        match dialect {
                            SqlDialect::Postgres => format!("{col} ILIKE ${}", params.len()),
                            SqlDialect::Sqlite => {
                                format!("{col} LIKE ${} COLLATE NOCASE", params.len())
                            }
                            SqlDialect::Mysql => {
                                format!("{col} LIKE ${} COLLATE utf8mb4_0900_ai_ci", params.len())
                            }
                        }
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
        |field| validate_read_identifier(field, schema_hint),
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
/// — when masked — lowered to its `<col>_masked` sibling so the sort
/// never touches the plaintext column.
fn build_aggregate_order_by(
    order: &Value,
    dialect: SqlDialect,
    agg_exprs: &std::collections::HashMap<String, String>,
    schema_hint: &Value,
) -> Result<String, QueryError> {
    let term = |field: &str, descending: bool| -> Result<String, QueryError> {
        if agg_exprs.contains_key(field) {
            Ok(build_order_term_expr(&quote_ident(field), descending, dialect))
        } else {
            validate_read_identifier(field, schema_hint)?;
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
        SqlDialect::Mysql => {
            let dir = if descending { "DESC" } else { "ASC" };
            let null_bucket = if descending { "DESC" } else { "ASC" };
            format!("{col} IS NULL {null_bucket}, {col} {dir}")
        }
    }
}

/// Convert a JSON value to a text parameter string for Postgres.
fn value_to_param_inner(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(), // should not be used as param (use IS NULL)
        // For arrays/objects, serialize as JSON text (stored as JSONB in PG)
        other => other.to_string(),
    }
}

/// Convert a JSON value to a Postgres text param; used cross-module (B1 migrations).
pub fn value_to_param(value: &Value) -> String {
    value_to_param_inner(value)
}

/// Build an UPSERT (INSERT ... ON CONFLICT DO UPDATE) query:
/// ```sql
/// INSERT INTO "app_id"."collection" ("col1", "col2") VALUES ($1, $2)
/// ON CONFLICT ("conflict_col") DO UPDATE SET "col2" = EXCLUDED."col2"
/// RETURNING "id", "created_at", ...
/// ```
///
/// `doc` is the full document to insert (as a JSON object).
/// `conflict_fields` is an array of column names that form the conflict target.
/// Non-conflict columns are set to `EXCLUDED."col"` in the DO UPDATE SET clause.
pub fn build_upsert(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    doc: &Value,
    conflict_fields: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_upsert_with_dialect(
        app_id,
        collection,
        schema_hint,
        doc,
        conflict_fields,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware UPSERT builder.
pub fn build_upsert_with_dialect(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    doc: &Value,
    conflict_fields: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

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
    let binary_bind_cols = collect_binary_bind_cols(obj);

    let mut columns = Vec::new();
    let mut placeholders = Vec::new();
    let mut params: Vec<String> = Vec::new();
    let mut update_clauses = Vec::new();
    let mut doc_has_version = false;
    let mut doc_has_updated_at = false;

    for (key, value) in obj {
        if key.starts_with("__zsbin__") {
            continue;
        }
        columns.push(quote_ident(key));

        match key.as_str() {
            "version" => doc_has_version = true,
            "updated_at" => doc_has_updated_at = true,
            _ => {}
        }

        if value.is_null() {
            placeholders.push("NULL".to_string());
        } else {
            let is_binary_bind = binary_bind_cols.contains(key.as_str());
            let raw = value_to_param(value);
            let param_value = if is_binary_bind {
                dialect.wrap_binary_bind_param(raw)
            } else {
                raw
            };
            params.push(param_value);
            let n = params.len();
            if is_binary_bind {
                placeholders.push(dialect.binary_bind_placeholder(n));
            } else {
                placeholders.push(format!("${n}"));
            }
        }

        // Non-conflict columns get updated to the EXCLUDED value
        if !conflict_set.contains(key.as_str())
            && !matches!(key.as_str(), "id" | "created_at" | "created_by")
        {
            update_clauses.push(format!("{} = EXCLUDED.{}", quote_ident(key), quote_ident(key)));
        }
    }

    let conflict_cols: Vec<String> = conflict_arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(quote_ident)
        .collect();

    if !doc_has_version {
        // The READ of `version` must name its relation. Inside `ON CONFLICT DO
        // UPDATE SET`, both the target row and the proposed row (`excluded`)
        // are in scope, so an unqualified column reference in the SET
        // EXPRESSION is ambiguous - PostgreSQL refuses the whole statement with
        // `42702 column reference "version" is ambiguous`. The assignment
        // TARGET on the left is unambiguous by position and stays bare, which
        // is why this reads lopsided.
        //
        // Every PG upsert took this branch: the insert-side system-fields pass
        // deliberately leaves `version` to the DDL default, so `doc_has_version`
        // is false on the dispatch path. Nothing caught it because the upsert
        // tests that EXECUTE run on SQLite, where the same reference is legal,
        // and the PG upsert tests only compare strings.
        update_clauses.push(format!(
            r#""version" = COALESCE({schema}.{table}."version", 0) + 1"#
        ));
    }
    if !doc_has_updated_at {
        update_clauses.push(format!(r#""updated_at" = {}"#, now_expr(dialect)));
    }

    // If all columns are conflict columns, use DO UPDATE SET for the first non-id conflict col
    // to make it a true upsert (otherwise Postgres treats it as DO NOTHING).
    if update_clauses.is_empty() {
        // All columns are conflict columns — set the first one to itself
        if let Some(first) = conflict_arr.first().and_then(|v| v.as_str()) {
            update_clauses.push(format!("{} = EXCLUDED.{}", quote_ident(first), quote_ident(first)));
        }
    }

    let sql = format!(
        "INSERT INTO {schema}.{table} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {} RETURNING {returning}",
        columns.join(", "),
        placeholders.join(", "),
        conflict_cols.join(", "),
        update_clauses.join(", ")
    );

    Ok(BuiltQuery { sql, params })
}

/// Build a findOrCreate query. Same shape as [`build_upsert`] but the
/// ON CONFLICT branch is a no-op self-assignment on the conflict column
/// (the existing row is returned untouched) and the RETURNING list
/// appends `(xmax = 0) AS __created` so the caller can tell whether
/// the row was newly inserted (xmax = 0) or pre-existing (xmax != 0).
pub fn build_find_or_create(
    app_id: &str,
    collection: &str,
    schema_hint: &Value,
    doc: &Value,
    conflict_fields: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    let returning = build_returning_expr(schema_hint)?;

    let obj = doc.as_object().ok_or_else(|| {
        QueryError::InvalidFilter("findOrCreate document must be an object".to_string())
    })?;

    if obj.is_empty() {
        return Err(QueryError::InvalidFilter(
            "findOrCreate document cannot be empty".to_string(),
        ));
    }

    let conflict_arr = conflict_fields.as_array().ok_or_else(|| {
        QueryError::InvalidFilter("conflict_fields must be an array".to_string())
    })?;

    if conflict_arr.is_empty() {
        return Err(QueryError::InvalidFilter(
            "conflict_fields cannot be empty".to_string(),
        ));
    }

    let first_conflict = conflict_arr
        .iter()
        .filter_map(|v| v.as_str())
        .next()
        .ok_or_else(|| {
            QueryError::InvalidFilter(
                "conflict_fields must contain string values".to_string(),
            )
        })?;

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

    let conflict_cols: Vec<String> = conflict_arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(quote_ident)
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

    Ok(BuiltQuery { sql, params })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The read schema the filter/order/limit-shape tests below build against.
    ///
    /// `build_find` used to exist in the production API as a shim that passed
    /// `None` for the schema; it had no production caller (measured: only this
    /// module, `crates/zeroship-plugin-db/tests/integration.rs` and
    /// `crates/zeroship-plugin-db/benches/bench_query_build.rs`) and it was the
    /// only way to reach the `SELECT *` arm L24 is about. It is gone. These
    /// tests are about WHERE / ORDER BY / LIMIT shape, so they declare the
    /// columns they name and let the projection be the ordinary allowlist.
    fn tschema() -> Value {
        json!({
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

    /// The SET clause of an UPDATE, i.e. everything between `SET ` and the
    /// first ` WHERE `.
    ///
    /// The no-actor tests below assert that `updated_by` is ABSENT, and
    /// `updated_by` is a system field that every RETURNING projection names.
    /// Asserting over the whole statement passed only while that clause was
    /// `*`; it is the clause, not the statement, that the property is about.
    fn set_clause_of(sql: &str) -> &str {
        let body = sql.split_once(" SET ").expect("an UPDATE has a SET clause").1;
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
        app_id: &str,
        collection: &str,
        filter: &Value,
        limit: Option<i64>,
        offset: Option<i64>,
        order_by: Option<&Value>,
        select: Option<&Value>,
    ) -> Result<BuiltQuery, QueryError> {
        build_find_with_schema(
            app_id,
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
        let filter = json!({"name": "alice"});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users" WHERE "name" = $1"#, tselect()));
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
        let filter = json!({"status": {"$in": [null]}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(
            q.sql.contains(r#""status" IS NULL"#),
            "a null $in member must become IS NULL; got {}",
            q.sql
        );
        assert!(
            !q.params.iter().any(|p| p.is_empty()),
            "a null must never be bound as the empty string; params={:?}",
            q.params
        );
    }

    /// Mixed members keep both halves: the non-null values stay a bound
    /// `IN (...)` and the null becomes an `IS NULL` disjunct.
    #[test]
    fn in_with_mixed_null_and_values_keeps_both_arms() {
        let filter = json!({"status": {"$in": ["active", null]}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
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
        let filter = json!({"status": {"$nin": [null]}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(
            q.sql.contains(r#""status" IS NOT NULL"#),
            "a null $nin member must become IS NOT NULL; got {}",
            q.sql
        );
        assert!(
            !q.params.iter().any(|p| p.is_empty()),
            "a null must never be bound as the empty string; params={:?}",
            q.params
        );
    }

    #[test]
    fn nin_with_mixed_null_and_values_keeps_both_arms() {
        let filter = json!({"status": {"$nin": ["active", null]}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
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
        assert_eq!(q.params, vec!["active"]);
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
        let filter = json!({"status": {"$in": []}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
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
        let filter = json!({"status": {"$nin": []}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert!(
            !q.sql.contains("NOT IN ()"),
            "NOT IN () is a syntax error in PostgreSQL; got {}",
            q.sql
        );
        assert!(
            q.sql.contains("TRUE"),
            "an empty $nin must become a constant-true predicate; got {}",
            q.sql
        );
        assert!(q.params.is_empty());
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
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users""#, tselect()));
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_null_filter() {
        let filter = Value::Null;
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users""#, tselect()));
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
        let q = build_insert("app1", "users", &tschema(), &doc).unwrap();
        assert!(q.sql.contains("INSERT INTO"));
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
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
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users" WHERE "name" ILIKE $1"#, tselect()));
        assert_eq!(q.params, vec!["%alice%"]);
    }

    #[test]
    fn test_ilike_operator_sqlite_uses_like_nocase() {
        let filter = json!({"name": {"$ilike": "%alice%"}});
        let q = build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
            "app1",
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
        assert_eq!(q.params, vec!["%alice%"]);
    }

    #[test]
    fn test_not_operator() {
        let filter = json!({"$not": {"role": "admin"}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users" WHERE NOT ("role" = $1)"#, tselect()));
        assert_eq!(q.params, vec!["admin"]);
    }

    #[test]
    fn test_update_inc() {
        let filter = json!({"id": 1});
        let update = json!({"views": {"$inc": 1}});
        let q = build_update_one("app1", "posts", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""views" = "views" + $1::numeric"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "1");
    }

    #[test]
    fn test_update_dec() {
        let filter = json!({"id": 1});
        let update = json!({"stock": {"$dec": 1}});
        let q = build_update_one("app1", "items", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""stock" = "stock" - $1::numeric"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "1");
    }

    #[test]
    fn test_update_mul() {
        let filter = json!({"id": 1});
        let update = json!({"price": {"$mul": 1.1}});
        let q = build_update_one("app1", "items", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""price" = "price" * $1::numeric"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "1.1");
    }

    #[test]
    fn test_update_push() {
        let filter = json!({"id": 1});
        let update = json!({"tags": {"$push": "new"}});
        let q = build_update_one("app1", "posts", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "posts", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "posts", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "posts", &tschema(), &filter, &update).unwrap();
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
        let q = build_insert_many("app1", "users", &tschema(), &docs).unwrap();
        assert!(q.sql.starts_with(r#"INSERT INTO "app1"."users""#), "sql: {}", q.sql);
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
        let over: Vec<Value> = (0..=MAX_INSERT_MANY_BATCH).map(|i| json!({ "n": i })).collect();
        let err = build_insert_many("app1", "users", &tschema(), &Value::Array(over)).unwrap_err();
        match err {
            QueryError::InvalidFilter(m) => assert!(m.contains("exceeds the maximum"), "{m}"),
            other => panic!("expected InvalidFilter, got {other:?}"),
        }
        let at_cap: Vec<Value> = (0..MAX_INSERT_MANY_BATCH).map(|i| json!({ "n": i })).collect();
        assert!(build_insert_many("app1", "users", &tschema(), &Value::Array(at_cap)).is_ok());
    }

    fn full_non_null_insert_many_batch(column_count: usize) -> Value {
        let template: serde_json::Map<String, Value> = (0..column_count)
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
        assert!(base_width > 0, "the exact-boundary fixture must have non-empty rows");
        Value::Array(
            (0..MAX_INSERT_MANY_BATCH)
                .map(|row| {
                    let width = base_width + usize::from(row < wider_rows);
                    let mut doc: serde_json::Map<String, Value> = (0..width)
                        .map(|column| (format!("field_{column}"), Value::from(column)))
                        .collect();
                    doc.insert("explicit_null".to_string(), Value::Null);
                    doc.insert("__zsbin__field_0".to_string(), Value::Bool(true));
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
        assert!(largest_full_width > 0, "the exercised column set must be non-empty");

        let accepted = build_insert_many(
            "app1",
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
            "app1",
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
        let accepted = build_insert_many("app1", "users", &tschema(), &accepted_docs)
            .expect("exactly the PostgreSQL non-null field-value limit must build");
        assert_eq!(accepted.params.len(), protocol_limit);

        let rejected_docs = insert_many_batch_with_exact_non_null_cells(protocol_limit + 1);
        let rejected = build_insert_many("app1", "users", &tschema(), &rejected_docs);
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
        assert!(largest_full_width > 0, "the exercised column set must be non-empty");

        let accepted = build_insert_many_with_dialect(
            "app1",
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
            "app1",
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
        let docs = json!([]);
        let result = build_insert_many("app1", "users", &tschema(), &docs);
        assert!(result.is_err(), "expected error for empty array");
    }

    #[test]
    fn test_update_many() {
        let filter = json!({"active": true});
        let update = json!({"status": "verified"});
        let q = build_update_many("app1", "users", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.starts_with(r#"UPDATE "app1"."users" SET"#), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        // Must NOT contain ctid subquery (that's updateOne's approach)
        assert!(!q.sql.contains("ctid"), "sql should not contain ctid: {}", q.sql);
    }

    #[test]
    fn test_delete_many() {
        let filter = json!({"active": false});
        let q = build_delete_many("app1", "users", &tschema(), &filter).unwrap();
        assert!(q.sql.starts_with(r#"DELETE FROM "app1"."users""#), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert!(!q.sql.contains("ctid"), "sql should not contain ctid: {}", q.sql);
        assert_eq!(q.params, vec!["false"]);
    }

    #[test]
    fn test_delete_many_no_filter() {
        let filter = json!({});
        let q = build_delete_many("app1", "users", &tschema(), &filter).unwrap();
        assert!(!q.sql.contains("WHERE"), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
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
        assert!(q.sql.starts_with(&tselect()), "sql: {}", q.sql);
    }

    #[test]
    fn test_distinct() {
        let filter = json!({});
        let q = build_distinct("app1", "users", "country", &filter, &tschema()).unwrap();
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
        let q = build_distinct("app1", "users", "role", &filter, &tschema()).unwrap();
        assert!(
            q.sql.contains(r#"SELECT DISTINCT "role" FROM "app1"."users" WHERE"#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains(r#"ORDER BY "role""#), "sql: {}", q.sql);
        assert_eq!(q.params, vec!["true"]);
    }

    /// After the storage flip, `email`'s own column already holds the
    /// masked value, so DISTINCT reads it directly with no sibling alias.
    /// The raw column (holding the real value) must never be named.
    #[test]
    fn distinct_on_a_masked_field_reads_the_masked_column() {
        let schema = json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let q = build_distinct_with_soft_delete_with_dialect(
            "app1",
            "users",
            "email",
            &json!({}),
            false,
            &schema,
            SqlDialect::Postgres,
        )
        .expect("build distinct with schema");

        assert!(
            q.sql.starts_with(r#"SELECT DISTINCT "email" FROM "app1"."users""#),
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
        let pipeline = json!([
            {"$match": {"active": true}},
            {"$group": {"by": "country", "count": {"$count": true}}},
            {"$sort": {"count": -1}},
            {"$limit": 5}
        ]);
        let q = build_aggregate("app1", "users", &pipeline, &tschema()).unwrap();
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
        let q = build_aggregate("app1", "orders", &pipeline, &tschema()).unwrap();
        assert!(q.sql.contains(r#"GROUP BY "country", "city""#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#"SUM("revenue") AS "total""#), "sql: {}", q.sql);
    }

    #[test]
    fn test_aggregate_having() {
        let pipeline = json!([
            {"$group": {"by": "category", "cnt": {"$count": true}}},
            {"$having": {"cnt": {"$gte": 10}}}
        ]);
        let q = build_aggregate("app1", "products", &pipeline, &tschema()).unwrap();
        assert!(q.sql.contains("HAVING COUNT(*) >= $1"), "sql: {}", q.sql);
        assert!(q.sql.contains("GROUP BY"), "sql: {}", q.sql);
        assert_eq!(q.params, vec!["10"]);
    }

    #[test]
    fn test_aggregate_no_group() {
        let pipeline = json!([
            {"$match": {"active": true}}
        ]);
        let q = build_aggregate("app1", "users", &pipeline, &tschema()).unwrap();
        assert!(q.sql.starts_with(&tselect()), "sql: {}", q.sql);
        assert!(!q.sql.contains("GROUP BY"), "sql: {}", q.sql);
    }

    // -----------------------------------------------------------------------
    // SEC-4: the aggregation pipeline must NOT leak the raw (real) value of a
    // masked column.
    //
    // Originally: for a mask-only column (`.mask({...})` without
    // `.encrypted()`), plaintext lived in `<col>` and the masked string in
    // `<col>_masked`. The aggregate builder used a bare `quote_ident(field)`
    // against the base plaintext column, so `$group.by:"ssn"` / `$max:"ssn"`
    // returned PLAINTEXT.
    //
    // After the storage flip, `<col>` itself holds the masked value and the
    // real value lives in the unqueryable `__zs_raw__<col>` sibling
    // (`raw_column_name`). The same bare `quote_ident(field)` the builder
    // always used now reads the masked column by construction - these pin
    // that the raw sibling is never named anywhere in the built SQL.
    // -----------------------------------------------------------------------

    fn mask_only_ssn_schema() -> Value {
        json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "tenant": { "type": "string" }
        })
    }

    #[test]
    fn sec4_aggregate_group_by_masked_field_reads_the_masked_column() {
        let pipeline = json!([
            {"$group": {"by": "ssn", "n": {"$count": true}}}
        ]);
        let schema = mask_only_ssn_schema();
        let q = build_aggregate_with_soft_delete_with_dialect(
            "app1", "users", &pipeline, false, &schema, SqlDialect::Postgres,
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
        let pipeline = json!([
            {"$group": {"by": "tenant", "top": {"$max": "ssn"}}}
        ]);
        let schema = mask_only_ssn_schema();
        let q = build_aggregate_with_soft_delete_with_dialect(
            "app1", "users", &pipeline, false, &schema, SqlDialect::Postgres,
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
            let pipeline = json!([
                {"$group": {"by": "tenant", "v": {op: "ssn"}}}
            ]);
            let q = build_aggregate_with_soft_delete_with_dialect(
                "app1", "users", &pipeline, false, &schema, SqlDialect::Postgres,
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
        let filter = json!({"id": 1});
        let update = json!({"name": "bob"});
        let q = build_update_one("app1", "users", &tschema(), &filter, &update).unwrap();
        // Plain field: value → SET "name" = $1
        assert!(q.sql.contains(r#""name" = $1"#), "sql: {}", q.sql);
        // ctid subquery for LIMIT 1
        assert!(q.sql.contains("ctid"), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert_eq!(q.params[0], "bob");
    }

    #[test]
    fn test_update_one_set_operator() {
        let filter = json!({"id": 1});
        let update = json!({"$set": {"name": "carol"}});
        let q = build_update_one("app1", "users", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $1"#), "sql: {}", q.sql);
        assert!(q.sql.contains("ctid"), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert_eq!(q.params[0], "carol");
    }

    #[test]
    fn test_delete_one() {
        let filter = json!({});
        let q = build_delete_one("app1", "users", &tschema(), &filter).unwrap();
        // ctid subquery for LIMIT 1
        assert!(q.sql.contains("ctid"), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
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
        let q = build_delete_one("app1", "users", &tschema(), &filter).unwrap();
        assert!(q.sql.contains("ctid"), "sql: {}", q.sql);
        // Filter should appear in the subquery
        assert!(q.sql.contains(r#""role" = $1"#), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert_eq!(q.params, vec!["guest"]);
    }

    #[test]
    fn test_order_by_object() {
        let order = json!({"name": 1, "age": -1});
        let clause = build_order_by(&order).unwrap();
        assert!(clause.contains(r#""name" ASC NULLS LAST"#), "clause: {clause}");
        assert!(clause.contains(r#""age" DESC NULLS FIRST"#), "clause: {clause}");
    }

    #[test]
    fn test_order_by_array() {
        let order = json!([["name", 1], ["age", -1]]);
        let clause = build_order_by(&order).unwrap();
        // Array form preserves declaration order
        assert!(clause.contains(r#""name" ASC NULLS LAST"#), "clause: {clause}");
        assert!(clause.contains(r#""age" DESC NULLS FIRST"#), "clause: {clause}");
        // "name" should appear before "age"
        let name_pos = clause.find(r#""name""#).unwrap();
        let age_pos = clause.find(r#""age""#).unwrap();
        assert!(name_pos < age_pos, "name should come before age");
    }

    #[test]
    fn test_order_by_sqlite_emulates_postgres_null_ordering() {
        let order = json!({"name": 1, "age": -1});
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
        let filter = json!({});
        let order = json!({"created_at": -1});
        let q = build_find("app1", "posts", &filter, Some(10), None, Some(&order), None).unwrap();
        assert!(
            q.sql.contains(r#"ORDER BY "created_at" DESC NULLS FIRST"#),
            "sql: {}",
            q.sql
        );
        assert!(q.sql.contains("LIMIT 10"), "sql: {}", q.sql);
    }

    #[test]
    fn build_find_sqlite_orders_nullable_columns_like_postgres() {
        let filter = json!({});
        let order = json!({"optional": 1});
        let q = build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
            "app1",
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
            q.sql.contains(r#"ORDER BY "optional" IS NULL ASC, "optional" ASC"#),
            "sql: {}",
            q.sql
        );
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
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users" WHERE "bio" IS NULL"#, tselect()));
        assert!(q.params.is_empty());
    }

    #[test]
    fn test_ne_null() {
        // { field: { $ne: null } } → IS NOT NULL
        let filter = json!({"bio": {"$ne": null}});
        let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users" WHERE "bio" IS NOT NULL"#, tselect()));
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
        assert_eq!(q.sql, format!(r#"{} FROM "app1"."users" WHERE "name" LIKE $1"#, tselect()));
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
        let q = build_insert("app1", "users", &tschema(), &doc).unwrap();
        assert_eq!(q.params, vec!["true"]);
    }

    #[test]
    fn test_insert_with_null_field() {
        let doc = json!({"name": "alice", "bio": null});
        let q = build_insert("app1", "users", &tschema(), &doc).unwrap();
        // null is inlined as a SQL `NULL` literal — not bound as a
        // text-format parameter (the wire protocol can't represent
        // NULL as a parameter; empty string would fail enum / NOT
        // NULL CHECKs).
        assert!(q.params.contains(&"alice".to_string()));
        assert!(!q.params.contains(&String::new()), "null must not be bound as empty-string param");
        assert!(q.sql.contains("NULL"), "null should appear as a SQL literal in: {}", q.sql);
    }

    #[test]
    fn test_insert_with_number() {
        let doc = json!({"age": 30});
        let q = build_insert("app1", "users", &tschema(), &doc).unwrap();
        assert_eq!(q.params, vec!["30"]);
    }

    #[test]
    fn test_insert_with_nested_json() {
        let doc = json!({"settings": {"theme": "dark"}});
        let q = build_insert("app1", "users", &tschema(), &doc).unwrap();
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
        let q = build_aggregate("app1", "users", &pipeline, &tschema()).unwrap();
        assert!(q.sql.starts_with(&tselect()), "sql: {}", q.sql);
    }

    #[test]
    fn test_aggregate_match_only() {
        // Only $match without $group → select * (same as no_group test)
        let pipeline = json!([{"$match": {"status": "active"}}]);
        let q = build_aggregate("app1", "users", &pipeline, &tschema()).unwrap();
        assert!(q.sql.starts_with(&tselect()), "sql: {}", q.sql);
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
        let q = build_aggregate("app1", "orders", &pipeline, &tschema()).unwrap();
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
        let result = build_update_one("app1", "users", &tschema(), &filter, &update);
        assert!(result.is_err(), "unsupported update operator should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unsupported"), "msg: {msg}");
    }

    #[test]
    fn test_insert_empty_doc() {
        let doc = json!({});
        let result = build_insert("app1", "users", &tschema(), &doc);
        assert!(result.is_err(), "empty document should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("empty") || msg.contains("cannot"), "msg: {msg}");
    }

    #[test]
    fn test_insert_non_object() {
        let doc = json!("just a string");
        let result = build_insert("app1", "users", &tschema(), &doc);
        assert!(result.is_err(), "non-object document should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("object"), "msg: {msg}");
    }

    #[test]
    fn test_update_empty_fields() {
        let filter = json!({});
        let update = json!({});
        let result = build_update_one("app1", "users", &tschema(), &filter, &update);
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
        let q = build_aggregate("app1", "employees", &pipeline, &tschema()).unwrap();
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
        let q = build_aggregate("app1", "employees", &pipeline, &tschema()).unwrap();
        // With a preceding $sort, $first threads the ORDER BY into array_agg
        assert!(
            q.sql.contains(r#"(array_agg("name" ORDER BY "salary" DESC NULLS FIRST))[1]"#),
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
        let q = build_aggregate("app1", "employees", &pipeline, &tschema()).unwrap();
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
        let q = build_aggregate("app1", "employees", &pipeline, &tschema()).unwrap();
        // $first should have ORDER BY
        assert!(
            q.sql.contains(r#"array_agg("name" ORDER BY "salary" DESC NULLS FIRST)"#),
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
        let q = build_upsert("app1", "users", &tschema(), &doc, &conflict).unwrap();
        assert!(q.sql.contains("INSERT INTO"), "sql: {}", q.sql);
        assert!(q.sql.contains("ON CONFLICT"), "sql: {}", q.sql);
        assert!(q.sql.contains("DO UPDATE SET"), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""name""#), "sql: {}", q.sql);
        // age is not a conflict field, so it should appear in DO UPDATE SET
        assert!(q.sql.contains(r#""age" = EXCLUDED."age""#), "sql: {}", q.sql);
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_upsert_multiple_conflict_fields() {
        let doc = json!({"email": "a@b.com", "name": "alice", "age": 30});
        let conflict = json!(["email", "name"]);
        let q = build_upsert("app1", "users", &tschema(), &doc, &conflict).unwrap();
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
        let q = build_upsert("app1", "users", &tschema(), &doc, &conflict).unwrap();
        assert!(q.sql.contains("DO UPDATE SET"), "sql: {}", q.sql);
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
    }

    #[test]
    fn test_upsert_preserves_insert_only_system_fields_on_conflict() {
        let doc = json!({
            "email": "a@b.com",
            "id": "user_new",
            "created_at": "2026-05-25T00:00:00Z",
            "created_by": "usr_new",
            "updated_by": "usr_actor",
            "name": "alice"
        });
        let conflict = json!(["email"]);
        let q = build_upsert("app1", "users", &tschema(), &doc, &conflict).unwrap();
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
    fn test_upsert_autobumps_version_and_updated_at_when_omitted() {
        let doc = json!({"email": "a@b.com", "name": "alice"});
        let conflict = json!(["email"]);
        let q = build_upsert_with_dialect("app1", "users", &tschema(), &doc, &conflict, SqlDialect::Sqlite)
            .unwrap();
        assert!(
            q.sql
                .contains(r#""version" = COALESCE("app1"."users"."version", 0) + 1"#),
            "upsert must auto-bump version on conflict when omitted: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#""updated_at" = CURRENT_TIMESTAMP"#),
            "SQLite upsert must stamp CURRENT_TIMESTAMP when updated_at omitted: {}",
            q.sql
        );
    }

    #[test]
    fn test_upsert_respects_creator_supplied_version_and_updated_at() {
        let doc = json!({
            "email": "a@b.com",
            "name": "alice",
            "version": 99,
            "updated_at": "2026-05-25T00:00:00Z"
        });
        let conflict = json!(["email"]);
        let q =
            build_upsert_with_dialect("app1", "users", &tschema(), &doc, &conflict, SqlDialect::Postgres)
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
    fn test_upsert_skips_encryption_markers_and_wraps_ciphertext_bind() {
        let doc = json!({
            "email": "a@b.com",
            "ssn": "Y2lwaGVydGV4dA==",
            "__zsbin__ssn": true,
        });
        let conflict = json!(["email"]);
        let q = build_upsert_with_dialect("app1", "users", &tschema(), &doc, &conflict, SqlDialect::Sqlite)
            .unwrap();
        assert!(
            !q.sql.contains("__zsbin__"),
            "marker keys must never be emitted as real columns: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#""ssn""#),
            "real encrypted column must still be emitted: {}",
            q.sql
        );
        assert!(
            q.params
                .iter()
                .any(|p| p.starts_with(SQLITE_BINARY_BIND_PREFIX)),
            "SQLite upsert must tag encrypted params for blob binding: {:?}",
            q.params
        );
    }

    #[test]
    fn test_upsert_empty_doc_error() {
        let doc = json!({});
        let conflict = json!(["name"]);
        let result = build_upsert("app1", "users", &tschema(), &doc, &conflict);
        assert!(result.is_err());
    }

    #[test]
    fn test_upsert_empty_conflict_fields_error() {
        let doc = json!({"name": "alice"});
        let conflict = json!([]);
        let result = build_upsert("app1", "users", &tschema(), &doc, &conflict);
        assert!(result.is_err());
    }

    #[test]
    fn test_upsert_invalid_collection_error() {
        let doc = json!({"name": "alice"});
        let conflict = json!(["name"]);
        let result = build_upsert("app1", "users; DROP TABLE", &tschema(), &doc, &conflict);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_or_create_emits_xmax_returning() {
        let doc = json!({"email": "a@b.com", "name": "alice"});
        let conflict = json!(["email"]);
        let q = build_find_or_create("app1", "users", &tschema(), &doc, &conflict).unwrap();
        assert!(q.sql.contains("INSERT INTO"), "sql: {}", q.sql);
        assert!(q.sql.contains(r#"ON CONFLICT ("email")"#), "sql: {}", q.sql);
        // No-op self-assignment on the conflict column so RETURNING
        // fires for the existing row without mutating it.
        assert!(
            q.sql.contains(r#"DO UPDATE SET "email" = "app1"."users"."email""#),
            "sql: {}", q.sql,
        );
        // The created flag is appended to the RETURNING list.
        assert!(
            q.sql.contains("(xmax = 0) AS __created"),
            "sql: {}", q.sql,
        );
        assert!(q.sql.contains(&treturning()), "sql: {}", q.sql);
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_find_or_create_rejects_empty_conflict() {
        let doc = json!({"email": "a@b.com"});
        let conflict = json!([]);
        assert!(build_find_or_create("app1", "users", &tschema(), &doc, &conflict).is_err());
    }

    #[test]
    fn test_find_or_create_rejects_empty_doc() {
        let doc = json!({});
        let conflict = json!(["email"]);
        assert!(build_find_or_create("app1", "users", &tschema(), &doc, &conflict).is_err());
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
        let q = build_update_one("app1", "games", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "games", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""flags" = "flags" || $1::jsonb"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "true");
    }

    #[test]
    fn test_update_push_object_preserves_type() {
        let filter = json!({"id": 1});
        let update = json!({"entries": {"$push": {"k": "v", "n": 3}}});
        let q = build_update_one("app1", "log", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "games", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "games", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "users", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""updated_at" = NOW()"#), "sql: {}", q.sql);
    }

    #[test]
    fn test_update_updated_at_not_overridden_when_explicit() {
        // If the caller explicitly provides updated_at, we must NOT add
        // our own `NOW()` clause — would collide and the user's value wins.
        let filter = json!({"id": 1});
        let explicit_ts = "2026-01-01T00:00:00Z";
        let update = json!({"name": "bob", "updated_at": explicit_ts});
        let q = build_update_one("app1", "users", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "users", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "posts", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "accounts", &tschema(), &filter, &update).unwrap();
        assert_eq!(q.params[0], "1.5");
    }

    #[test]
    fn test_update_negative_inc() {
        // Negative $inc must still render with the `+` operator (caller
        // uses $dec for subtraction semantically). Postgres handles the
        // minus sign on the numeric literal fine.
        let filter = json!({"id": 1});
        let update = json!({"stock": {"$inc": -3}});
        let q = build_update_one("app1", "items", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""stock" = "stock" + $1::numeric"#), "sql: {}", q.sql);
        assert_eq!(q.params[0], "-3");
    }

    #[test]
    fn test_update_param_indexing_with_filter() {
        // SET params come first, WHERE params come after. The `$N`
        // placeholders must be contiguous across both halves.
        let filter = json!({"status": "active"});
        let update = json!({"name": "alice", "views": {"$inc": 1}});
        let q = build_update_one("app1", "posts", &tschema(), &filter, &update).unwrap();
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
        let err = build_update_one("app1", "users", &tschema(), &filter, &update).unwrap_err();
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
        let err = build_update_one("app1", "posts", &tschema(), &filter, &update).unwrap_err();
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
        let q = build_update_many("app1", "posts", &tschema(), &filter, &update).unwrap();
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
        let q = build_update_one("app1", "users", &tschema(), &filter, &update).unwrap();
        assert!(q.sql.contains(r#""name" = $"#), "sql: {}", q.sql);
        assert!(q.sql.contains(r#""age" = $"#), "sql: {}", q.sql);
    }

    #[test]
    fn test_update_set_non_object_rejected() {
        // `$set` value that isn't an object should error, not be
        // silently treated as a scalar $set on a column named "$set".
        let filter = json!({"id": 1});
        let update = json!({"$set": "not an object"});
        let err = build_update_one("app1", "users", &tschema(), &filter, &update).unwrap_err();
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
        let q = build_update_one("app1", "accounts", &tschema(), &filter, &update).unwrap();
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
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(
            q.sql.contains(r#""version" = "version" + 1"#),
            "PR 4 must append version auto-bump: {}",
            q.sql,
        );
    }

    #[test]
    fn update_appends_version_increment_for_anonymous_dispatch_write() {
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Sqlite,
            &autobump,
        )
        .unwrap();
        assert!(
            q.sql.contains(r#""version" = "version" + 1"#),
            "anonymous dispatched updates must still bump version: {}",
            q.sql,
        );
    }

    #[test]
    fn update_appends_updated_at_now_pg() {
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
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
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Sqlite,
            &autobump,
        )
        .unwrap();
        assert!(
            q.sql.contains(r#""updated_at" = CURRENT_TIMESTAMP"#),
            "SQLite dialect must emit CURRENT_TIMESTAMP: {}",
            q.sql,
        );
        assert!(!q.sql.contains("NOW()"), "SQLite must NOT emit NOW(): {}", q.sql);
    }

    #[test]
    fn update_appends_updated_by_from_session_actor() {
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_session"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
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
            q.params.contains(&"usr_session".to_string()),
            "params must include actor id: {:?}",
            q.params,
        );
    }

    #[test]
    fn update_leaves_updated_by_null_when_no_session_actor() {
        // No actor: the dispatch path emits no `updated_by` SET clause.
        // Note: with `actor_id = None` AND no `skip_*` flags, the
        // default fallback applies — only `updated_at` auto-bumps.
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let autobump = SystemFieldAutoBump::default();
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
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
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new", "version": 99 });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            skip_version: true,
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        // The version auto-bump must NOT appear.
        assert!(
            !q.sql.contains(r#""version" = "version" + 1"#),
            "skip_version must suppress the auto-bump: {}",
            q.sql,
        );
        // The creator's explicit value flows through as a bound param.
        assert!(
            q.sql.contains(r#""version" = $"#),
            "creator's explicit version must reach SQL: {}",
            q.sql,
        );
        assert!(q.params.contains(&"99".to_string()), "params: {:?}", q.params);
    }

    #[test]
    fn update_respects_creator_supplied_updated_at_pr4() {
        let filter = json!({ "id": "post_x" });
        let explicit = "2026-01-01T00:00:00Z";
        let update = json!({ "title": "new", "updated_at": explicit });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            skip_updated_at: true,
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(
            !q.sql.contains("NOW()"),
            "skip_updated_at must suppress NOW(): {}",
            q.sql,
        );
        assert!(q.params.contains(&explicit.to_string()), "params: {:?}", q.params);
    }

    #[test]
    fn update_auto_bump_columns_bypass_encryption_pass() {
        // Build a doc with an encrypted-column marker. The auto-bump
        // version/updated_at/updated_by SET clauses must NOT be wrapped
        // with the encrypted-column placeholder shape.
        let filter = json!({ "id": "post_x" });
        // `__zsbin__secret` marks the `secret` column encrypted.
        let update = json!({
            "secret": "ciphertext",
            "__zsbin__secret": true,
        });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        // Encrypted column gets the decode(...)::bytea wrap.
        assert!(
            q.sql.contains("decode("),
            "encrypted column must still get decode wrap: {}",
            q.sql,
        );
        // Auto-bumps are plain SET clauses — must NOT be inside a decode().
        // The version bump's SQL fragment is `"version" = "version" + 1`
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
        let filter = json!({ "id": "post_x" });
        let update = json!({
            "secret": "ciphertext",
            "__zsbin__secret": true,
        });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
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
            .position(|p| p == "ciphertext")
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
        let filter = json!({ "id": "post_x", "version": 5 });
        let update = json!({ "title": "new" });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        // WHERE clause references `version` as an equality predicate.
        assert!(
            q.sql.contains(r#""version" = $"#),
            "filter version must appear in WHERE: {}",
            q.sql,
        );
        // The version `5` must appear as a bound param.
        assert!(q.params.contains(&"5".to_string()), "params: {:?}", q.params);
    }

    #[test]
    fn update_default_path_emits_dialect_aware_updated_at_sqlite() {
        // Direct calls to the legacy wrapper on the SQLite arm: the
        // auto-bump is dialect-aware rather than hardcoding NOW().
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let q = build_update_one_with_dialect(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert!(
            q.sql.contains(r#""updated_at" = CURRENT_TIMESTAMP"#),
            "SQLite-arm direct callers get CURRENT_TIMESTAMP: {}",
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
        let filter = serde_json::json!({ "id": "post_x", "version": 5 });
        let update = serde_json::json!({ "title": "new", "version": 99 });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            // The caller's `apply_system_fields_on_update` would set
            // this from inspecting the patch — we set it manually here
            // to pin the contract.
            skip_version: true,
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        // SET carries the creator's explicit value (`"version" = $N`).
        assert!(
            q.sql.contains(r#""version" = $"#),
            "explicit version reaches SET: {}",
            q.sql,
        );
        assert!(
            !q.sql.contains(r#""version" = "version" + 1"#),
            "auto-bump must be suppressed: {}",
            q.sql,
        );
        // Bind values must include BOTH `99` (SET) and `5` (WHERE).
        assert!(q.params.contains(&"99".to_string()), "params: {:?}", q.params);
        assert!(q.params.contains(&"5".to_string()), "params: {:?}", q.params);
    }

    #[test]
    fn update_encrypted_column_still_routes_through_encryption_pass() {
        // The encrypted-column marker continues to wrap the placeholder
        // with `decode($N, 'base64')::bytea`. Auto-bump columns
        // (version/updated_at/updated_by) are NOT subject to the
        // marker — only the creator-supplied `ssn` column gets the
        // encryption wrap.
        let filter = serde_json::json!({ "id": "post_x" });
        let update = serde_json::json!({
            "ssn": "Y2lwaGVydGV4dF9ibG9i",
            "__zsbin__ssn": true,
        });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        // `ssn` gets the decode(...)::bytea wrap.
        assert!(
            q.sql.contains("decode("),
            "encrypted column wrapped: {}",
            q.sql,
        );
        // The encrypted column SQL fragment contains the cast.
        let ssn_idx = q.sql.find(r#""ssn""#).unwrap();
        let ssn_end = q.sql[ssn_idx..].find(',').map(|i| ssn_idx + i).unwrap_or(q.sql.len());
        let ssn_clause = &q.sql[ssn_idx..ssn_end];
        assert!(
            ssn_clause.contains("decode("),
            "ssn SET clause must include decode wrap: {ssn_clause}"
        );
    }

    /// The auto-bump SQL must NOT carry an `__zsbin__updated_by`
    /// marker — system fields are platform-managed plaintext and bypass
    /// encryption by construction.
    #[test]
    fn update_auto_bump_columns_bypass_mask_pass() {
        // Mask-pass markers (sibling `<col>_masked` columns) only fire
        // for columns the schema declares as `t.mask(...)`. System
        // fields are never declared with a mask; the mask pass's
        // schema-iteration loop naturally skips them. We confirm the
        // SQL doesn't accidentally emit a sibling for any auto-bump
        // column.
        let filter = serde_json::json!({ "id": "post_x" });
        let update = serde_json::json!({ "title": "new" });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
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
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
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
        // 63-byte threshold (inclusive) — the real NAMEDATALEN ceiling.
        //   "t_" (2) + col (57) + "_idx" (4) = 63  → unhashed
        //   "t_" (2) + col (58) + "_idx" (4) = 64  → hashed
        let col = "c".repeat(57);
        let name = index_name("t", &[col.as_str()], false);
        assert_eq!(name.len(), crate::ident::PG_MAX_IDENT_BYTES, "name: {}", name);
        assert!(name.ends_with("_idx"), "should keep readable suffix: {}", name);
    }

    #[test]
    fn test_index_name_just_over_threshold_is_hashed() {
        let col = "c".repeat(58);
        let name = index_name("t", &[col.as_str()], false);
        assert!(name.len() <= crate::ident::PG_MAX_IDENT_BYTES);
        assert!(
            !name.ends_with("_idx"),
            "over-threshold name should end with the hash, not _idx: {}",
            name
        );
    }

    /// A collection name at the 63-byte ceiling PASSES `validate_collection`,
    /// but nothing bounds the names DERIVED from it. `build_create_indexes`
    /// builds the auto mask index (on the field's own, masked column) as
    /// `<coll>__<col>_mask_idx`, which is guaranteed to overflow NAMEDATALEN
    /// for such a collection. Postgres does not error on that - it truncates
    /// to 63 bytes and emits a NOTICE - so two mask indexes on the same
    /// collection collapse to ONE identifier and the second
    /// `CREATE INDEX ... IF NOT EXISTS` is a SILENT no-op.
    ///
    /// The two natural names here diverge only at byte 65 (`__alpha` vs
    /// `__beta`), i.e. strictly AFTER the truncation point - a shorter
    /// collection would leave the truncated forms distinct and the test would
    /// pass for the wrong reason.
    ///
    /// What this does NOT catch: it asserts distinctness of the emitted
    /// identifiers only. It does not prove the emitted DDL is accepted by a
    /// live Postgres, and it does not cover the UNIQUE variants (see
    /// `derived_unique_index_names_stay_distinct_at_the_63_byte_ceiling`).
    #[test]
    fn derived_masked_index_names_stay_distinct_at_the_63_byte_ceiling() {
        let coll = "c".repeat(63);
        assert!(validate_collection(&coll).is_ok(), "63 bytes must pass validation");

        let schema = serde_json::json!({
            "alpha": { "type": "string", "index": true, "mask": { "kind": "partial" } },
            "beta":  { "type": "string", "index": true, "mask": { "kind": "partial" } },
        });
        let specs = build_create_indexes("app1", &coll, &schema).expect("indexes build");

        // The auto mask index lands on the field's OWN (masked) column and
        // is named `<coll>__<field>_mask_idx` - but at this ceiling that
        // natural name overflows and `cap_ident_name` replaces the
        // `_mask_idx` tail with a hash, so the index CANNOT be selected by
        // name suffix (that would defeat the very truncation this test
        // exercises). Select it by column identity instead: its sole column
        // is the bare field name, distinct from the raw-column index's sole
        // column `raw_column_name(field)`.
        let masked: Vec<&IndexSpec> = specs
            .iter()
            .filter(|s| s.columns == ["alpha".to_string()] || s.columns == ["beta".to_string()])
            .collect();
        assert_eq!(masked.len(), 2, "expected one mask index per masked field: {masked:?}");

        for spec in &masked {
            assert!(
                spec.name.len() <= 63,
                "derived name {} is {} bytes - Postgres will truncate it silently",
                spec.name,
                spec.name.len()
            );
            // The mask index must cover the field's own column, never the
            // raw (real-value) sibling - that would defeat the point of
            // indexing the mask for creator-visible reads.
            assert!(
                spec.columns.iter().all(|c| !c.starts_with(RAW_COLUMN_PREFIX)),
                "mask index must not cover the raw column: {spec:?}"
            );
        }

        // What Postgres actually stores: the first 63 bytes.
        let truncate = |n: &String| n.as_bytes()[..n.len().min(63)].to_vec();
        assert_ne!(
            truncate(&masked[0].name),
            truncate(&masked[1].name),
            "distinct mask indexes collapsed to one identifier after \
             NAMEDATALEN truncation: {} / {}",
            masked[0].name,
            masked[1].name
        );
    }

    /// The serious half of the same defect: a silently-skipped UNIQUE index
    /// means the uniqueness the schema declares does not exist in the database.
    ///
    /// What this does NOT catch: same limits as the masked-sibling test above —
    /// identifier distinctness only, no live-database assertion.
    #[test]
    fn derived_unique_index_names_stay_distinct_at_the_63_byte_ceiling() {
        let coll = "u".repeat(63);
        let schema = serde_json::json!({
            "alpha": { "type": "string", "unique": true },
            "beta":  { "type": "string", "unique": true },
        });
        let specs = build_create_indexes("app1", &coll, &schema).expect("indexes build");
        let uniq: Vec<&IndexSpec> = specs.iter().filter(|s| s.unique).collect();
        assert_eq!(uniq.len(), 2, "expected two unique indexes: {uniq:?}");
        for spec in &uniq {
            assert!(spec.name.len() <= 63, "derived name {} is {} bytes", spec.name, spec.name.len());
        }
        let truncate = |n: &String| n.as_bytes()[..n.len().min(63)].to_vec();
        assert_ne!(
            truncate(&uniq[0].name),
            truncate(&uniq[1].name),
            "two UNIQUE indexes collapsed to one identifier: {} / {}",
            uniq[0].name,
            uniq[1].name
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
        let create = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline).unwrap();
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

    #[test]
    fn p7_id_prefix_decl_emits_single_id_column() {
        // `id: t.id("blog")` is a prefix declaration for the
        // system `id` PK column, NOT a second column. The emitter must
        // skip it: exactly one `id` column (the system PK), no duplicate,
        // and no reserved-name rejection.
        let schema = json!({
            "id": {"type": "id", "idPrefix": "blog"},
            "title": {"type": "string", "required": true},
        });
        let create =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        // The system PK is emitted as `id TEXT PRIMARY KEY` (unquoted —
        // see `build_system_field_columns`). The prefix declaration must
        // NOT add a second column (which would appear as a quoted
        // `"id"` from the field loop's `quote_ident`).
        assert!(
            create.contains("id TEXT PRIMARY KEY"),
            "system id PK column present: {create}"
        );
        assert_eq!(
            create.matches("\"id\"").count(),
            0,
            "no duplicate quoted id column from the prefix declaration: {create}"
        );
        assert!(
            create.contains("\"title\""),
            "user field still emitted: {create}"
        );
    }

    #[test]
    fn p7_id_prefix_decl_with_reserved_usr_is_rejected() {
        // Defense in depth: a hand-built wire payload declaring
        // `id: t.id("usr")` must be rejected at DDL build (mirrors the
        // SDK fence). Reuses `ReservedSystemFieldName`.
        let schema = json!({ "id": {"type": "id", "idPrefix": "usr"} });
        let err = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline)
            .unwrap_err();
        assert!(
            matches!(err, QueryError::ReservedSystemFieldName(_)),
            "usr prefix must be rejected as reserved, got {err:?}"
        );
    }

    #[test]
    fn p7_id_prefix_decl_with_malformed_prefix_is_rejected() {
        let schema = json!({ "id": {"type": "id", "idPrefix": "1bad"} });
        let err = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline)
            .unwrap_err();
        assert!(
            matches!(err, QueryError::InvalidIdent(_)),
            "malformed prefix must be rejected, got {err:?}"
        );
    }

    #[test]
    fn p7_id_with_non_id_type_still_rejected() {
        // A field literally named `id` with a NON-"id" type is NOT a
        // prefix declaration — it must still trip the reserved-name fence.
        let schema = json!({ "id": {"type": "string"} });
        let err = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline)
            .unwrap_err();
        assert!(
            matches!(err, QueryError::ReservedSystemFieldName(_)),
            "id with non-id type must stay rejected, got {err:?}"
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
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        // TEXT column for the FK (cascades to match the
        // `id TEXT PRIMARY KEY` system-field DDL; was INTEGER previously).
        assert!(sql.contains("\"authorId\" TEXT"), "{sql}");
        // Inline FK clause with SQL/Postgres defaults omitted.
        assert!(sql.contains("FOREIGN KEY (\"authorId\")"), "{sql}");
        assert!(
            sql.contains("REFERENCES \"app1\".\"users\" (id)"),
            "{sql}"
        );
        assert!(!sql.contains("ON DELETE"), "{sql}");
        assert!(!sql.contains("ON UPDATE"), "{sql}");
        assert!(!sql.contains("DEFERRABLE"), "{sql}");
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
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        assert!(sql.contains("ON DELETE CASCADE"), "{sql}");
        assert!(sql.contains("ON UPDATE CASCADE"), "{sql}");
    }

    #[test]
    fn mysql_fk_restrict_and_no_action_render_as_implicit_default() {
        let schema = json!({
            "authorDefault": {
                "type": "ref",
                "refTarget": "users",
            },
            "authorRestrict": {
                "type": "ref",
                "refTarget": "users",
                "onDelete": "restrict",
                "onUpdate": "restrict",
            },
            "authorNoAction": {
                "type": "ref",
                "refTarget": "users",
                "onDelete": "noAction",
                "onUpdate": "noAction",
            },
            "authorCascade": {
                "type": "ref",
                "refTarget": "users",
                "onDelete": "setNull",
                "onUpdate": "cascade",
            },
        });
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Mysql,
        )
        .unwrap();

        assert!(!sql.contains("ON DELETE RESTRICT"), "{sql}");
        assert!(!sql.contains("ON UPDATE RESTRICT"), "{sql}");
        assert!(!sql.contains("ON DELETE NO ACTION"), "{sql}");
        assert!(!sql.contains("ON UPDATE NO ACTION"), "{sql}");
        assert!(sql.contains("ON DELETE SET NULL"), "{sql}");
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
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        assert!(!sql.contains("DEFERRABLE"), "{sql}");
    }

    #[test]
    fn b2_ref_explicit_restrict_and_deferrable_render() {
        let schema = json!({
            "authorId": {
                "type": "ref",
                "refTarget": "users",
                "onUpdate": "restrict",
                "deferrable": true,
            },
        });
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        assert!(sql.contains("REFERENCES \"app1\".\"users\" (id)"), "{sql}");
        assert!(sql.contains("ON UPDATE RESTRICT"), "{sql}");
        assert!(!sql.contains("ON DELETE"), "{sql}");
        assert!(sql.contains("DEFERRABLE INITIALLY DEFERRED"), "{sql}");
    }

    #[test]
    fn b2_sqlite_inline_fk_uses_unqualified_parent_table() {
        let schema = json!({
            "authorId": {
                "type": "ref",
                "refTarget": "users",
            },
        });
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert!(sql.contains("FOREIGN KEY (\"authorId\")"), "{sql}");
        assert!(sql.contains("REFERENCES \"users\" (id)"), "{sql}");
        assert!(!sql.contains("REFERENCES \"app1\".\"users\" (id)"), "{sql}");
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

    /// The budget is Postgres' real `NAMEDATALEN` ceiling, not the crate-local
    /// 60 this assertion used to encode — see
    /// [`crate::ident::PG_MAX_IDENT_BYTES`].
    #[test]
    fn b2_fk_constraint_name_truncated() {
        let long = "a".repeat(80);
        let name = fk_constraint_name(&long, "");
        assert!(
            name.len() <= crate::ident::PG_MAX_IDENT_BYTES,
            "got {} bytes: {name}",
            name.len()
        );
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
        // FK is deferred — column still present but no FOREIGN KEY clause.
        // TEXT (cascade from the ref-column type change; was INTEGER previously).
        assert!(sql.contains("\"authorId\" TEXT"), "{sql}");
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
    // Where an FK target's SCHEMA comes from.
    //
    // Two properties of this renderer:
    //
    //   1. `build_fk_clause` runs `validate_collection(target)`, whose
    //      charset is `[A-Za-z0-9_]`, so a dot-qualified target is not a
    //      legal collection name at all; and
    //   2. `PostgresSchemaRenderer::foreign_key_target` qualifies with
    //      `app_id` -- the CALLER's app -- and never reads a schema out
    //      of the author's target string.
    //
    // Property 2 is the load-bearing one: 1 alone would be defeated by
    // any future target syntax that encodes a qualifier without a dot.
    //
    // WHAT THESE DO NOT PROVE, AND IT MATTERS. They are NOT evidence
    // about a deployed app. This crate is consumed only by plugin-db
    // (checked 2026-08-20: nothing else names it in a Cargo.toml), and
    // plugin-db's callers of these builders all sit in
    // `#[cfg(any(test, feature = "test-helpers"))]` code -- production
    // `registerModel` issues no DDL. The migration engine, which does
    // apply schema at deploy, carries its OWN copy of this renderer in
    // `third_party/zero-migrate`. Read the deployed behaviour off the
    // foreign keys section of `docs/reference/db.md`, which names the
    // engine's checks; these tests pin only what this crate renders.
    // -----------------------------------------------------------------

    /// Case: a dot-qualified `<other_app>.<collection>` target.
    #[test]
    fn fk_target_naming_another_app_is_not_a_legal_collection_name() {
        let def = json!({"type": "ref", "refTarget": "other_app.users"});
        let err = build_add_foreign_key("app_demo", "posts", "authorId", &def)
            .expect_err("a dot-qualified FK target must not build");
        match err {
            QueryError::InvalidCollection(msg) => assert!(
                msg.contains("other_app.users"),
                "error must name the offending target: {msg}"
            ),
            other => panic!("expected InvalidCollection, got {other:?}"),
        }
    }

    /// Control for the case above, differing in ONE variable: the same
    /// call with the dot removed. If this went red too, the test above
    /// would be proving only that `build_add_foreign_key` rejects
    /// things, not that it rejects the qualifier.
    #[test]
    fn fk_target_without_a_qualifier_builds() {
        let def = json!({"type": "ref", "refTarget": "other_app_users"});
        let sql = build_add_foreign_key("app_demo", "posts", "authorId", &def)
            .expect("an unqualified target must build");
        assert!(
            sql.contains("REFERENCES \"app_demo\".\"other_app_users\" (id)"),
            "{sql}"
        );
    }

    /// The rendered schema tracks `app_id`, not anything the schema
    /// author wrote. Same target string, two callers, two schemas.
    #[test]
    fn fk_reference_schema_is_the_calling_app_not_the_target() {
        let def = json!({"type": "ref", "refTarget": "users"});
        let a = build_add_foreign_key("app_a", "posts", "authorId", &def).expect("app_a");
        let b = build_add_foreign_key("app_b", "posts", "authorId", &def).expect("app_b");
        assert!(a.contains("REFERENCES \"app_a\".\"users\" (id)"), "{a}");
        assert!(b.contains("REFERENCES \"app_b\".\"users\" (id)"), "{b}");
        assert!(!a.contains("app_b"), "app_a's FK must not name app_b: {a}");
    }

    // -----------------------------------------------------------------
    // FK column type cascade (TEXT, was INTEGER previously)
    // -----------------------------------------------------------------

    /// `def_to_pg_type` returns TEXT for a ref field so the FK column
    /// matches the `id TEXT PRIMARY KEY` shape.
    /// Pin via the single-arm helper so a future regression that
    /// switches the arm back to INTEGER trips here.
    #[test]
    fn fk_ref_field_emits_text_column_type_pg() {
        let def = json!({"type": "ref", "refTarget": "users"});
        let pg_type = super::def_to_pg_type(&def);
        assert_eq!(
            pg_type, "TEXT",
            "ref column type must cascade to TEXT (was INTEGER pre-PR 3 — see proposal §9 PR 3)"
        );
    }

    /// `build_add_column` for a ref field emits a TEXT column type so
    /// ALTER TABLE ADD COLUMN runs on a column that matches the
    /// referenced table's PK (TEXT typed_id).
    #[test]
    fn fk_ref_field_build_add_column_emits_text() {
        let def = json!({"type": "ref", "refTarget": "users"});
        let sql = build_add_column("app1", "posts", "authorId", &def).expect("build_add_column");
        assert!(
            sql.contains("ADD COLUMN IF NOT EXISTS \"authorId\" TEXT"),
            "expected ADD COLUMN ... TEXT, got: {sql}"
        );
    }

    /// SQLite dialect: the CREATE TABLE DDL also carries `"<col>" TEXT`
    /// for ref columns. SQLite's type affinity rules treat TEXT
    /// literally (BLOB/INTEGER/etc affinities are inferred from the
    /// declared type), so a typed_id round-trips as a string.
    #[test]
    fn fk_ref_field_emits_text_column_type_sqlite() {
        let schema = json!({
            "authorId": {"type": "ref", "refTarget": "users"},
        });
        let existing: std::collections::HashSet<String> = std::collections::HashSet::new();
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Deferred(&existing),
            SqlDialect::Sqlite,
        )
        .expect("build sqlite DDL");
        assert!(
            sql.contains("\"authorId\" TEXT"),
            "sqlite DDL must declare authorId TEXT, got: {sql}"
        );
    }

    /// Negative pin: NO ref column anywhere in the DDL should emit
    /// `INTEGER` for the column type. A regression that
    /// flipped the arm back would trip the `b2_create_table_with_ref_emits_inline_fk`
    /// test too, but this assertion stays independent so a future
    /// fixture-touch can't mask the regression.
    #[test]
    fn fk_ref_field_does_not_emit_integer_post_pr3() {
        let schema = json!({
            "authorId": {"type": "ref", "refTarget": "users"},
            "title": {"type": "string"},
        });
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline)
            .expect("build DDL");
        // The column itself must NOT carry INTEGER. (The CONSTRAINT
        // clause text contains nothing about INTEGER, so a substring
        // check on the whole sql is safe.)
        assert!(
            !sql.contains("\"authorId\" INTEGER"),
            "ref column must not emit INTEGER (PR 3 cascade): {sql}"
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
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline).unwrap();
        assert!(sql.contains("\"profile\" JSONB"), "{sql}");
        // Defaults to an empty JSON object (like t.json()).
        assert!(sql.contains("DEFAULT '{}'::jsonb"), "{sql}");
    }

    #[test]
    fn sqlite_create_table_uses_sqlite_types_for_object_bool_and_int() {
        let schema = json!({
            "flag": { "type": "boolean", "required": true },
            "meta": { "type": "object", "required": true },
            "rank": { "type": "int", "required": true },
        });
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "users",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build sqlite DDL");
        assert!(sql.contains("\"flag\" INTEGER NOT NULL"), "{sql}");
        assert!(sql.contains("\"meta\" TEXT NOT NULL DEFAULT '{}'"), "{sql}");
        assert!(sql.contains("\"rank\" INTEGER NOT NULL"), "{sql}");
        assert!(!sql.contains("JSONB"), "{sql}");
        assert!(!sql.contains("::jsonb"), "{sql}");
    }

    // -----------------------------------------------------------------
    // D3 — calendar dates → DATE column type
    // -----------------------------------------------------------------

    #[test]
    fn d3_calendar_date_emits_date_column() {
        let schema = json!({
            "birthday": { "type": "calendarDate" },
        });
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline).unwrap();
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
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline).unwrap();
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
    //
    // `version` is a reserved system-field name
    // (`SYSTEM_FIELD_NAMES`); the declaration-time validator refuses
    // a creator-declared `version` column. `build_create_table_with_fks`
    // injects the seven system fields directly (not via a creator-shape
    // entry). This test uses a placeholder field
    // name (`schema_revision`) to keep exercising the
    // `t.number().default(N)` DDL path that produces `DOUBLE PRECISION
    // ... DEFAULT 1`.
    // -----------------------------------------------------------------

    #[test]
    fn d4_version_column_default_one() {
        let schema = json!({
            "title": { "type": "string", "required": true },
            "schema_revision": { "type": "number", "default": 1 },
        });
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        assert!(sql.contains("\"schema_revision\" DOUBLE PRECISION"), "{sql}");
        assert!(sql.contains("DEFAULT 1"), "{sql}");
    }

    // -----------------------------------------------------------------
    // C2 — discriminated union document shapes
    //
    // The SDK normalises `t.union(t.object({...}), t.object({...}))` into
    // a flat schema where each variant's fields are top-level entries
    // and the discriminator column carries a `variants` JSON payload
    // plus `discriminator: "__discriminator__"`. The DDL emitter
    // converts that into:
    //   - TEXT/NUMERIC/BOOLEAN column for the discriminator with
    //     `CHECK (col IN (...))` (via the regular enum constraint)
    //   - nullable columns for every variant field
    //   - per-variant CHECK constraint enforcing that required fields
    //     for the active variant are NOT NULL
    // -----------------------------------------------------------------

    fn c2_events_union_schema() -> serde_json::Value {
        // Equivalent of:
        //   events: t.union(
        //     t.object({ kind: t.literal("login"), userId: t.number().required(), ip: t.string().required() }),
        //     t.object({ kind: t.literal("error"), message: t.string().required(), stack: t.string() }),
        //     t.object({ kind: t.literal("metric"), name: t.string().required(), value: t.number().required() }),
        //   )
        json!({
            "kind": {
                "type": "string",
                "required": true,
                "enum": ["login", "error", "metric"],
                "discriminator": "__discriminator__",
                "variants": [
                    {
                        "kind":   { "type": "literal", "literalValue": "login", "required": true },
                        "userId": { "type": "number", "required": true },
                        "ip":     { "type": "string", "required": true }
                    },
                    {
                        "kind":    { "type": "literal", "literalValue": "error", "required": true },
                        "message": { "type": "string", "required": true },
                        "stack":   { "type": "string" }
                    },
                    {
                        "kind":  { "type": "literal", "literalValue": "metric", "required": true },
                        "name":  { "type": "string", "required": true },
                        "value": { "type": "number", "required": true }
                    }
                ]
            },
            "userId":  { "type": "number" },
            "ip":      { "type": "string" },
            "message": { "type": "string" },
            "stack":   { "type": "string" },
            "name":    { "type": "string" },
            "value":   { "type": "number" }
        })
    }

    #[test]
    fn c2_union_creates_flat_columns() {
        // Verify that the DDL declares every union-wide field as a
        // top-level column, with nullability reflecting "not in every
        // variant" semantics (so the column is nullable at the table
        // level; per-variant CHECK constraints enforce integrity).
        let schema = c2_events_union_schema();
        let sql = build_create_table_with_fks("app1", "events", &schema, &FkEmission::Inline).unwrap();

        // Discriminator: TEXT, NOT NULL, with CHECK IN-list.
        assert!(sql.contains("\"kind\" TEXT"), "expected kind TEXT: {sql}");
        assert!(sql.contains("\"kind\" TEXT NOT NULL"), "expected kind NOT NULL: {sql}");
        assert!(
            sql.contains("CHECK (\"kind\" IN ('login', 'error', 'metric'))"),
            "missing discriminator IN constraint: {sql}"
        );

        // Non-discriminator columns exist and are NOT marked NOT NULL.
        for col in ["userId", "ip", "message", "stack", "name", "value"] {
            assert!(sql.contains(&format!("\"{col}\"")), "missing column {col}: {sql}");
            // No standalone `NOT NULL` immediately after the column type — these are nullable.
            let bad = format!("\"{col}\" TEXT NOT NULL");
            let bad2 = format!("\"{col}\" NUMERIC NOT NULL");
            assert!(!sql.contains(&bad) && !sql.contains(&bad2), "column {col} must be nullable: {sql}");
        }
    }

    #[test]
    fn c2_union_emits_per_variant_check_constraints() {
        // Per proposal §C2, each variant gets a CHECK constraint of the
        // form: `kind <> 'login' OR (userId IS NOT NULL AND ip IS NOT NULL)`.
        let schema = c2_events_union_schema();
        let sql = build_create_table_with_fks("app1", "events", &schema, &FkEmission::Inline).unwrap();

        // The login variant requires userId AND ip.
        assert!(
            sql.contains("\"kind\" <> 'login' OR (\"userId\" IS NOT NULL AND \"ip\" IS NOT NULL)")
                || sql.contains("\"kind\" <> 'login' OR (\"ip\" IS NOT NULL AND \"userId\" IS NOT NULL)"),
            "missing login variant CHECK: {sql}"
        );
        // The error variant requires message (stack is optional → not in the NOT NULL list).
        assert!(
            sql.contains("\"kind\" <> 'error' OR (\"message\" IS NOT NULL)"),
            "missing error variant CHECK: {sql}"
        );
        assert!(
            !sql.contains("\"stack\" IS NOT NULL"),
            "stack is optional and must not appear in CHECK: {sql}"
        );
        // The metric variant requires name AND value.
        assert!(
            sql.contains("\"kind\" <> 'metric' OR (\"name\" IS NOT NULL AND \"value\" IS NOT NULL)")
                || sql.contains("\"kind\" <> 'metric' OR (\"value\" IS NOT NULL AND \"name\" IS NOT NULL)"),
            "missing metric variant CHECK: {sql}"
        );
    }

    #[test]
    fn c2_union_constraint_names_are_unique_per_variant() {
        let schema = c2_events_union_schema();
        let sql = build_create_table_with_fks("app1", "events", &schema, &FkEmission::Inline).unwrap();
        // Each variant constraint name follows `<table>_<disc>_<value>_chk`.
        assert!(sql.contains("CONSTRAINT \"events_kind_login_chk\""), "{sql}");
        assert!(sql.contains("CONSTRAINT \"events_kind_error_chk\""), "{sql}");
        assert!(sql.contains("CONSTRAINT \"events_kind_metric_chk\""), "{sql}");
    }

    #[test]
    fn c2_union_with_only_optional_variants_skips_check() {
        // A variant with no required (non-discriminator) fields should
        // not emit a CHECK constraint — the discriminator IN-list is
        // sufficient.
        let schema = json!({
            "kind": {
                "type": "string",
                "required": true,
                "enum": ["a", "b"],
                "discriminator": "__discriminator__",
                "variants": [
                    {
                        "kind": { "type": "literal", "literalValue": "a", "required": true },
                        "x":    { "type": "string" }
                    },
                    {
                        "kind": { "type": "literal", "literalValue": "b", "required": true },
                        "y":    { "type": "string" }
                    }
                ]
            },
            "x": { "type": "string" },
            "y": { "type": "string" }
        });
        let sql = build_create_table_with_fks("app1", "evt", &schema, &FkEmission::Inline).unwrap();
        // No per-variant CHECK clauses, but discriminator IN-list still
        // applies.
        assert!(sql.contains("CHECK (\"kind\" IN ('a', 'b'))"), "{sql}");
        assert!(
            !sql.contains("\"kind\" <> 'a' OR ("),
            "unexpected CHECK on variant with no requireds: {sql}"
        );
    }

    #[test]
    fn c2_union_numeric_discriminator() {
        // Discriminator can be a number — verify the literal renders
        // without single quotes and the IN-list does the same.
        let schema = json!({
            "code": {
                "type": "number",
                "required": true,
                "enum": [1, 2],
                "discriminator": "__discriminator__",
                "variants": [
                    {
                        "code": { "type": "literal", "literalValue": 1, "required": true },
                        "a":    { "type": "string", "required": true }
                    },
                    {
                        "code": { "type": "literal", "literalValue": 2, "required": true },
                        "b":    { "type": "string", "required": true }
                    }
                ]
            },
            "a": { "type": "string" },
            "b": { "type": "string" }
        });
        let sql = build_create_table_with_fks("app1", "evt", &schema, &FkEmission::Inline).unwrap();
        assert!(sql.contains("\"code\" DOUBLE PRECISION"), "{sql}");
        // Number enum members are bare (no quotes).
        assert!(sql.contains("CHECK (\"code\" IN (1, 2))"), "{sql}");
        assert!(sql.contains("\"code\" <> 1 OR (\"a\" IS NOT NULL)"), "{sql}");
        assert!(sql.contains("\"code\" <> 2 OR (\"b\" IS NOT NULL)"), "{sql}");
    }

    #[test]
    fn mysql_string_enum_uses_native_enum_type() {
        let schema = json!({
            "status": {
                "type": "string",
                "required": true,
                "enum": ["active", "paused"]
            }
        });
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "apps",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Mysql,
        )
        .unwrap();
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS `app1`.`apps`"), "{sql}");
        assert!(
            sql.contains("`status` ENUM('active', 'paused') NOT NULL"),
            "{sql}"
        );
        assert!(!sql.contains("CHECK (`status` IN"), "{sql}");
    }

    #[test]
    fn c2_standalone_literal_field_emits_check_equality() {
        // A top-level (non-union) literal field — `kind: t.literal("login")`
        // alone — gets a `CHECK (kind = 'login')` constraint.
        let schema = json!({
            "kind": { "type": "literal", "literalValue": "login", "required": true },
        });
        let sql = build_create_table_with_fks("app1", "events", &schema, &FkEmission::Inline).unwrap();
        assert!(sql.contains("\"kind\" TEXT"), "{sql}");
        assert!(sql.contains("CHECK (\"kind\" = 'login')"), "{sql}");
    }

    #[test]
    fn c2_union_value_with_special_chars_sanitized_in_constraint_name() {
        // Discriminator values containing characters not legal in a
        // Postgres identifier (hyphens, dots, etc.) must be sanitised
        // for the constraint name; the literal itself is still SQL-
        // single-quoted with apostrophes escaped.
        let schema = json!({
            "kind": {
                "type": "string",
                "required": true,
                "enum": ["page.view", "click-out"],
                "discriminator": "__discriminator__",
                "variants": [
                    {
                        "kind": { "type": "literal", "literalValue": "page.view", "required": true },
                        "url":  { "type": "string", "required": true }
                    },
                    {
                        "kind": { "type": "literal", "literalValue": "click-out", "required": true },
                        "target": { "type": "string", "required": true }
                    }
                ]
            },
            "url":    { "type": "string" },
            "target": { "type": "string" }
        });
        let sql = build_create_table_with_fks("app1", "evt", &schema, &FkEmission::Inline).unwrap();
        // Sanitised identifiers (dots / hyphens → underscore).
        assert!(sql.contains("CONSTRAINT \"evt_kind_page_view_chk\""), "{sql}");
        assert!(sql.contains("CONSTRAINT \"evt_kind_click_out_chk\""), "{sql}");
        // Literal still rendered correctly inside the CHECK body.
        assert!(sql.contains("'page.view'"), "{sql}");
        assert!(sql.contains("'click-out'"), "{sql}");
    }

    // -----------------------------------------------------------------------
    // Security IMPORTANT #1 — validate_collection reserved-name checks
    // -----------------------------------------------------------------------

    /// Valid collection names must still pass — no regression.
    #[test]
    fn validate_collection_accepts_valid_names() {
        for name in &["users", "todos", "order_items", "a", "A1_b"] {
            assert!(validate_collection(name).is_ok(), "expected '{name}' to be valid");
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

    /// Names starting with `__zero_migrate` (any case) must be rejected.
    #[test]
    fn validate_collection_rejects_zero_migrate_prefix() {
        for name in &[
            "__zero_migrate_migrations",
            "__ZERO_MIGRATE_audit",
            "__zero_migrate",
        ] {
            let err = validate_collection(name).unwrap_err();
            match err {
                QueryError::InvalidCollection(msg) => assert!(
                    msg.contains("__zero_migrate") || msg.contains("reserved"),
                    "for '{name}': {msg}"
                ),
                other => panic!("expected InvalidCollection for '{name}', got {other:?}"),
            }
        }
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
            zeroship_data_plan::ident::PLATFORM_RESERVED_COLLECTION_PREFIXES,
            "data-plane and runtime-plan collection prefixes diverged"
        );
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
        assert!(validate_collection(&"a".repeat(63)).is_ok(), "63-byte name should pass");
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
            assert!(validate_field_name(name).is_ok(), "field name should be valid");
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
        assert!(matches!(err, QueryError::InvalidIdent(_)), "expected InvalidIdent");
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

    /// build_create_table_with_fks must propagate field-name validation errors.
    #[test]
    fn build_create_table_rejects_oversized_field_name() {
        let long_field = "f".repeat(64);
        let schema = serde_json::json!({ long_field: { "type": "string" } });
        let result = build_create_table_with_fks("app1", "events", &schema, &FkEmission::Inline);
        assert!(result.is_err(), "expected error for 64-byte field name");
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
        let filter = serde_json::json!({ "ssn_masked": "***-**-6789" });
        let mut params: Vec<String> = Vec::new();
        let err = build_where(&filter, &mut params).unwrap_err();
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

    /// CREATE TABLE on a schema containing only `_meta` produces a
    /// table with no user columns (only the auto-injected `id`,
    /// `created_at`, `updated_at`). Smoke test for the metadata-key
    /// filter at the schema-iteration site.
    #[test]
    fn build_create_table_skips_top_level_meta_key() {
        let schema = serde_json::json!({
            "_meta": { "strictness": "off" },
            "name": { "type": "string" },
        });
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline)
            .expect("schema with _meta + a real field should build");
        assert!(sql.contains("\"name\""), "expected name column: {sql}");
        assert!(!sql.contains("\"_meta\""), "_meta must NOT be emitted as a column: {sql}");
    }

    // -----------------------------------------------------------------
    // Platform system-field reservation (declaration-only)
    // -----------------------------------------------------------------

    /// Each of the 7 platform-managed system field names must be refused
    /// by `validate_field_name_for_declaration`. Mirrors the seven names
    /// in `SYSTEM_FIELD_NAMES`. Filter-time validators continue to
    /// accept these names (covered by
    /// `system_field_names_allowed_in_filter_path`).
    #[test]
    fn system_field_names_refused_at_declaration() {
        for name in &[
            "id",
            "created_at",
            "updated_at",
            "created_by",
            "updated_by",
            "version",
            "deleted_at",
        ] {
            let err = validate_field_name_for_declaration(name).unwrap_err();
            match err {
                QueryError::ReservedSystemFieldName(msg) => {
                    assert!(
                        msg.contains(name) && msg.contains("reserved"),
                        "expected reserved-system-field message naming {name:?}, got: {msg}"
                    );
                }
                other => panic!(
                    "expected ReservedSystemFieldName for {name:?}, got {other:?}"
                ),
            }
        }
    }

    /// Filter-time validation (`validate_field_name`) MUST continue to
    /// accept all 7 system field names. `db.users.find({ id: "..." })`
    /// is the canonical query shape — fencing `id` at filter time would
    /// break the entire SDK. The system-field reservation is declaration-only.
    #[test]
    fn system_field_names_allowed_in_filter_path() {
        for name in SYSTEM_FIELD_NAMES {
            assert!(
                validate_field_name(name).is_ok(),
                "system field {name:?} must be accepted by the filter-time validator"
            );
        }
    }

    /// Filter-time use of a system-field name flows end-to-end through
    /// `build_where`: a query like `db.users.find({ id: "usr_01" })`
    /// must build a WHERE clause, NOT raise an error. This pins the
    /// "declaration-only" boundary at the call-site level.
    #[test]
    fn build_where_accepts_system_field_names_in_filter() {
        for name in SYSTEM_FIELD_NAMES {
            let mut filter_obj = serde_json::Map::new();
            filter_obj.insert((*name).to_string(), serde_json::json!("any-value"));
            let filter = serde_json::Value::Object(filter_obj);
            let mut params: Vec<String> = Vec::new();
            let clause = build_where(&filter, &mut params)
                .unwrap_or_else(|e| panic!("filter on {name:?} must build, got {e:?}"));
            assert!(
                clause.contains(&format!("\"{name}\"")),
                "WHERE clause must reference {name:?}; got: {clause}"
            );
        }
    }

    /// Non-system-field names continue to be accepted by the
    /// declaration-time validator (regression fence for the
    /// `validate_field_name_for_declaration` wrapper).
    #[test]
    fn non_system_field_names_accepted_at_declaration() {
        for name in &["title", "content", "user_id", "createdAt", "first_name"] {
            assert!(
                validate_field_name_for_declaration(name).is_ok(),
                "non-system field {name:?} must be accepted at declaration"
            );
        }
    }

    /// `SYSTEM_FIELD_NAMES` is the canonical list — every new addition
    /// is a deliberate platform decision. Pinning the size to 7 surfaces
    /// any drift in code review.
    #[test]
    fn system_field_names_has_exactly_seven_entries() {
        assert_eq!(
            SYSTEM_FIELD_NAMES.len(),
            7,
            "SYSTEM_FIELD_NAMES must list exactly 7 entries (id, created_at, \
             updated_at, created_by, updated_by, version, deleted_at)"
        );
    }

    // NOTE: `system_field_reservation_error_carries_correct_code` — which
    // asserted the `From<QueryError> for DbError` lift carries
    // `code = "reserved_system_field_name"` — was relocated to plugin-db's
    // `error.rs` test module as part of the schema-authority extraction.
    // `DbError` lives in plugin-db (it is built on `zeroship_runtime::OpError`)
    // and cannot be named from this leaf crate. The validator
    // (`validate_field_name_for_declaration`) and the `QueryError`
    // variant it produces are tested here; the *mapping* to `DbError` is
    // tested where `DbError` lives.

    /// `field_to_column` (the DDL builder for one column) must propagate
    /// the system-field reservation. End-to-end check that the
    /// declaration-time fence is wired at the right call site —
    /// CREATE TABLE on a schema declaring `id` as a creator column
    /// fails before any SQL is generated.
    #[test]
    fn build_create_table_refuses_creator_declared_system_field() {
        for name in SYSTEM_FIELD_NAMES {
            let mut schema_obj = serde_json::Map::new();
            schema_obj.insert(
                (*name).to_string(),
                serde_json::json!({ "type": "string" }),
            );
            let schema = serde_json::Value::Object(schema_obj);
            let err = build_create_table_with_fks(
                "app1",
                "posts",
                &schema,
                &FkEmission::Inline,
            )
            .unwrap_err();
            match err {
                QueryError::ReservedSystemFieldName(msg) => {
                    assert!(
                        msg.contains(name),
                        "CREATE TABLE must refuse system-field {name:?}; got: {msg}"
                    );
                }
                other => panic!(
                    "expected ReservedSystemFieldName for {name:?}, got {other:?}"
                ),
            }
        }
    }

    // -----------------------------------------------------------------
    // CREATE TABLE prepends 7 system fields + 3 auto-indexes
    //
    // Tests the dialect-aware emitter
    // (`build_create_table_with_fks_for_dialect`) and the PG-flavoured
    // shim (`build_create_table_with_fks`). The system-field prefix and
    // auto-index emission are dialect-symmetric except for timestamp
    // type / default expression and the SQLite `<schema>.<index_name>`
    // form vs PG's `ON <schema>.<table>`.
    // -----------------------------------------------------------------

    /// All seven system fields must appear in CREATE TABLE on PG, in the
    /// canonical `SYSTEM_FIELD_NAMES` order, before any user-declared
    /// columns. Pin the substring presence — the textual shape of each
    /// column (type / NOT NULL / DEFAULT) is exercised by the dedicated
    /// shape tests below.
    #[test]
    fn create_table_prepends_seven_system_fields_pg() {
        let schema = serde_json::json!({
            "title": { "type": "string" },
        });
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("build ok");
        for name in SYSTEM_FIELD_NAMES {
            assert!(
                sql.contains(&format!(" {name} ")) || sql.contains(&format!(" {name},")),
                "missing system field {name:?} in PG DDL: {sql}"
            );
        }
        // Canonical declaration order: each name appears BEFORE the
        // next, and all of them appear before the user field `title`.
        let positions: Vec<usize> = SYSTEM_FIELD_NAMES
            .iter()
            .map(|n| sql.find(n).expect("each name appears"))
            .collect();
        for w in positions.windows(2) {
            assert!(w[0] < w[1], "system fields out of order: {sql}");
        }
        let title_pos = sql.find("\"title\"").expect("title column present");
        let last_system_pos = *positions.last().unwrap();
        assert!(
            last_system_pos < title_pos,
            "system fields must precede user fields: {sql}"
        );
    }

    /// SQLite mirrors PG for the system-field prefix; only timestamp
    /// affinity (`TEXT` vs `TIMESTAMPTZ`) and the default expression
    /// (`CURRENT_TIMESTAMP` vs `NOW()`) differ.
    #[test]
    fn create_table_prepends_seven_system_fields_sqlite() {
        let schema = serde_json::json!({
            "title": { "type": "string" },
        });
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build ok");
        for name in SYSTEM_FIELD_NAMES {
            assert!(
                sql.contains(name),
                "missing system field {name:?} in SQLite DDL: {sql}"
            );
        }
        let positions: Vec<usize> = SYSTEM_FIELD_NAMES
            .iter()
            .map(|n| sql.find(n).expect("each name appears"))
            .collect();
        for w in positions.windows(2) {
            assert!(w[0] < w[1], "system fields out of order: {sql}");
        }
    }

    /// `id TEXT PRIMARY KEY` — identical on both engines. Replaces the
    /// legacy `id SERIAL PRIMARY KEY`.
    #[test]
    fn create_table_emits_id_text_primary_key() {
        let schema = serde_json::json!({});
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let sql = build_create_table_with_fks_for_dialect(
                "app1",
                "posts",
                &schema,
                &FkEmission::Inline,
                dialect,
            )
            .expect("build ok");
            assert!(
                sql.contains("id TEXT PRIMARY KEY"),
                "missing `id TEXT PRIMARY KEY` for {dialect:?}: {sql}"
            );
            assert!(
                !sql.contains("id SERIAL"),
                "must not emit legacy `id SERIAL` for {dialect:?}: {sql}"
            );
        }
    }

    /// PG: `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`.
    #[test]
    fn create_table_emits_created_at_default_now_pg() {
        let schema = serde_json::json!({});
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("build ok");
        assert!(
            sql.contains("created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()"),
            "PG created_at must be TIMESTAMPTZ NOT NULL DEFAULT NOW(): {sql}"
        );
        assert!(
            sql.contains("updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()"),
            "PG updated_at must be TIMESTAMPTZ NOT NULL DEFAULT NOW(): {sql}"
        );
    }

    /// SQLite: `created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP`.
    #[test]
    fn create_table_emits_created_at_default_current_timestamp_sqlite() {
        let schema = serde_json::json!({});
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build ok");
        assert!(
            sql.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"),
            "SQLite created_at must be TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP: {sql}"
        );
        assert!(
            sql.contains("updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"),
            "SQLite updated_at must be TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP: {sql}"
        );
        // SQLite arm must NEVER emit PG-specific tokens.
        assert!(
            !sql.contains("TIMESTAMPTZ"),
            "SQLite DDL must not contain TIMESTAMPTZ: {sql}"
        );
        assert!(
            !sql.contains("NOW()"),
            "SQLite DDL must not contain NOW(): {sql}"
        );
    }

    /// `version INTEGER NOT NULL DEFAULT 1` — identical on both
    /// backends. Auto-bumped by CRUD updates.
    #[test]
    fn create_table_emits_version_default_one() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let sql = build_create_table_with_fks_for_dialect(
                "app1",
                "posts",
                &serde_json::json!({}),
                &FkEmission::Inline,
                dialect,
            )
            .expect("build ok");
            assert!(
                sql.contains("version INTEGER NOT NULL DEFAULT 1"),
                "missing version default for {dialect:?}: {sql}"
            );
        }
    }

    /// `deleted_at <ts_type> NULL` — soft-delete sentinel. The
    /// nullability is load-bearing for the find() auto-filter
    /// (`WHERE deleted_at IS NULL`).
    #[test]
    fn create_table_emits_deleted_at_nullable() {
        let schema = serde_json::json!({});
        let sql_pg = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("build ok");
        assert!(
            sql_pg.contains("deleted_at TIMESTAMPTZ NULL"),
            "PG deleted_at must be TIMESTAMPTZ NULL: {sql_pg}"
        );
        let sql_sq = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build ok");
        assert!(
            sql_sq.contains("deleted_at TEXT NULL"),
            "SQLite deleted_at must be TEXT NULL: {sql_sq}"
        );
    }

    /// The three implicit B-tree indexes ride along with CREATE TABLE
    /// as semicolon-separated statements. Each names its column in the
    /// auto-named `<table>_<col>_idx` shape (idempotent — re-running
    /// `IF NOT EXISTS`).
    #[test]
    fn create_table_emits_three_indexes_deleted_at_updated_at_created_by() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let sql = build_create_table_with_fks_for_dialect(
                "app1",
                "posts",
                &serde_json::json!({}),
                &FkEmission::Inline,
                dialect,
            )
            .expect("build ok");
            for col in &["deleted_at", "updated_at", "created_by"] {
                let idx = index_name("posts", &[col], false);
                assert!(
                    sql.contains(&idx),
                    "missing index {idx} for column {col} on {dialect:?}: {sql}"
                );
                assert!(
                    sql.contains("CREATE INDEX IF NOT EXISTS"),
                    "implicit indexes must use IF NOT EXISTS for idempotency on \
                     {dialect:?}: {sql}"
                );
                assert!(
                    sql.contains(&format!("({})", quote_ident(col))),
                    "index DDL must reference column ({col}) on {dialect:?}: {sql}"
                );
            }
        }
    }

    /// The `id` column is implicitly indexed by the PRIMARY KEY
    /// constraint — emitting an explicit B-tree on `id` would be
    /// redundant.
    #[test]
    fn create_table_does_not_emit_index_for_id() {
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &serde_json::json!({}),
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("build ok");
        let id_idx = index_name("posts", &["id"], false);
        assert!(
            !sql.contains(&id_idx),
            "must NOT emit explicit index for id (PK covers it): {sql}"
        );
    }

    /// `version` is bumped on every UPDATE (auto-bumped by the CRUD
    /// dispatch path); an index on it would thrash. Per §5 of the
    /// proposal it stays unindexed.
    #[test]
    fn create_table_does_not_emit_index_for_version() {
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &serde_json::json!({}),
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("build ok");
        let version_idx = index_name("posts", &["version"], false);
        assert!(
            !sql.contains(&version_idx),
            "must NOT emit index for version (would thrash on UPDATE): {sql}"
        );
    }

    /// User-declared fields land AFTER the seven system fields. Pin
    /// the order so an accidental refactor that inverts the prepend
    /// loop fails here.
    #[test]
    fn create_table_appends_user_fields_after_system_fields() {
        let schema = serde_json::json!({
            "title": { "type": "string", "required": true },
            "body":  { "type": "string" },
        });
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("build ok");
        let last_system = sql.find("deleted_at").expect("deleted_at present");
        let first_user = sql.find("\"title\"").expect("title present");
        assert!(
            last_system < first_user,
            "user fields must follow system fields: {sql}"
        );
    }

    /// CREATE TABLE on an empty schema still produces a valid table —
    /// every system field is present and the 3 auto-indexes ride along.
    /// Smoke test for the no-user-columns edge.
    #[test]
    fn create_table_with_zero_user_fields_emits_seven_columns_only() {
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &serde_json::json!({}),
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("build ok");
        // All 7 names present.
        for name in SYSTEM_FIELD_NAMES {
            assert!(
                sql.contains(name),
                "missing system field {name}: {sql}"
            );
        }
        // The CREATE TABLE statement only has the 7 system-field
        // column declarations (no user columns + no FKs + no checks).
        // Slice from the FIRST `(` to the LAST `)` of the CREATE TABLE
        // statement (auto-indexes live on subsequent statements,
        // separated by `;\n`). `NOW()` etc. add inner parens, so we
        // scope the slice with the CREATE TABLE statement boundary.
        let create_stmt_end = sql.find(";\n").unwrap_or(sql.len());
        let create_stmt = &sql[..create_stmt_end];
        let table_body_start = create_stmt.find('(').expect("open paren");
        let table_body_end = create_stmt.rfind(')').expect("close paren");
        let body = &create_stmt[table_body_start + 1..table_body_end];
        // Count commas at the top level of the body — `NOW()` and
        // similar default expressions have no commas inside, so a
        // flat scan is correct here. 7 column declarations means 6
        // commas separating them.
        let commas = body.matches(',').count();
        assert_eq!(
            commas, 6,
            "expected exactly 6 commas (7 columns) in the table body, got {commas}: {body}"
        );
    }

    /// FK emission on a user-declared `ref` field continues to work
    /// alongside the system-field prefix. Pins the structural invariant
    /// that B2 FK clauses ride after the column declarations.
    #[test]
    fn create_table_with_fk_user_field_still_creates_fk_constraint() {
        let schema = serde_json::json!({
            "authorId": { "type": "ref", "refTarget": "users" },
        });
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("build ok");
        assert!(
            sql.contains("FOREIGN KEY (\"authorId\")"),
            "FK clause must still emit: {sql}"
        );
        assert!(
            sql.contains("REFERENCES \"app1\".\"users\" (id)"),
            "FK target must still reference id: {sql}"
        );
        // FK target IS the new TEXT id; the FK clause itself unchanged.
        assert!(sql.contains("id TEXT PRIMARY KEY"), "{sql}");
    }

    /// SQLite places the schema name on the INDEX, not the TABLE:
    /// `CREATE INDEX "<schema>"."<idx>" ON "<table>" (...)`. This
    /// matches SQLite's ATTACH-alias addressing, where the schema
    /// qualifies the index name rather than the table reference.
    #[test]
    fn create_table_sqlite_uses_dotted_schema_for_index() {
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &serde_json::json!({}),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build ok");
        let idx = index_name("posts", &["deleted_at"], false);
        // SQLite: `CREATE INDEX IF NOT EXISTS "app1"."posts_deleted_at_idx" ON "posts" (...)`.
        let expected_prefix = format!(
            "CREATE INDEX IF NOT EXISTS \"app1\".\"{idx}\" ON \"posts\""
        );
        assert!(
            sql.contains(&expected_prefix),
            "SQLite index DDL must use schema-on-index form ({expected_prefix}): {sql}"
        );
    }

    /// PG places the schema name on the TABLE in `ON`:
    /// `CREATE INDEX "<idx>" ON "<schema>"."<table>" (...)`. SQLite
    /// requires the dotted form on the index; PG accepts neither.
    #[test]
    fn create_table_pg_uses_on_dot_schema_for_index() {
        let sql = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &serde_json::json!({}),
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("build ok");
        let idx = index_name("posts", &["deleted_at"], false);
        let expected_prefix = format!(
            "CREATE INDEX IF NOT EXISTS \"{idx}\" ON \"app1\".\"posts\""
        );
        assert!(
            sql.contains(&expected_prefix),
            "PG index DDL must use ON <schema>.<table> form ({expected_prefix}): {sql}"
        );
    }

    /// The system-field index names go through the existing
    /// [`index_name`] helper, so an overlong collection name gets the sha2 hash
    /// truncation. Regression fence for the NAMEDATALEN-safety contract.
    ///
    /// The budget and the hash encoding are now
    /// [`crate::ident::cap_ident_name`]'s (63 bytes, 10 hex chars), not the
    /// crate-local 60-byte / 8-char-base32 pair this test used to encode.
    #[test]
    fn index_name_truncates_with_sha2_suffix_at_the_namedatalen_ceiling() {
        // 63-byte collection name (the Postgres NAMEDATALEN ceiling).
        // The naive `<table>_deleted_at_idx` is far over 63 bytes,
        // triggering the hash-truncation path.
        let long = "a".repeat(63);
        let idx = index_name(&long, &["deleted_at"], false);
        assert!(
            idx.len() <= crate::ident::PG_MAX_IDENT_BYTES,
            "truncated index name must fit NAMEDATALEN ({} bytes): {idx}",
            idx.len()
        );
        // The 10-char hex suffix is the hash tail.
        let tail = &idx[idx.len() - 10..];
        for b in tail.bytes() {
            assert!(
                b.is_ascii_hexdigit() && !b.is_ascii_uppercase(),
                "hash suffix must be lowercase hex: {tail}"
            );
        }
    }

    /// The debug_assert at the end of `build_create_table_with_fks_for_dialect`
    /// is the last line of defence: under debug builds it panics if two
    /// declarations end up referencing the same system-field name in
    /// the column list. The declaration-time validator catches creator-declared
    /// system fields before this point — so this test exercises the
    /// assertion's *unreachable* path under a hand-rolled internal
    /// invariant violation by constructing the columns vector directly.
    ///
    /// We can't actually trigger the assertion through the public API
    /// (every entry path is gated by `validate_field_name_for_declaration`),
    /// so instead this test pins the validator pre-check: when a creator
    /// schema declares `id`, the validator raises BEFORE the
    /// assertion runs — confirming the assertion is a true safety net,
    /// not the primary gate.
    #[cfg(debug_assertions)]
    #[test]
    fn debug_assert_panics_when_user_schema_collides_with_system_field() {
        // The validator raises `ReservedSystemFieldName` before
        // the debug_assert runs — verify the rejection happens at the
        // validator layer (the canonical first line of defence).
        for name in SYSTEM_FIELD_NAMES {
            let mut obj = serde_json::Map::new();
            obj.insert((*name).to_string(), serde_json::json!({ "type": "string" }));
            let schema = serde_json::Value::Object(obj);
            let err = build_create_table_with_fks_for_dialect(
                "app1",
                "posts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Postgres,
            )
            .expect_err("validator must reject system-field declaration");
            assert!(
                matches!(err, QueryError::ReservedSystemFieldName(_)),
                "validator must raise ReservedSystemFieldName for {name:?}, got {err:?}"
            );
        }
    }

    /// PG and SQLite emit equivalent column COUNT and ORDER for the
    /// system-field prefix; only the types differ. Snapshot-style
    /// comparison: any drift in the count or the order of system
    /// fields between dialects fails here.
    #[test]
    fn pg_and_sqlite_emit_equivalent_create_table_for_system_fields() {
        let schema = serde_json::json!({
            "title": { "type": "string", "required": true },
        });
        let pg = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("pg ok");
        let sq = build_create_table_with_fks_for_dialect(
            "app1",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("sqlite ok");

        // Same system-field NAMES in the same ORDER on both arms.
        let pg_positions: Vec<usize> = SYSTEM_FIELD_NAMES
            .iter()
            .map(|n| pg.find(n).expect("pg has name"))
            .collect();
        let sq_positions: Vec<usize> = SYSTEM_FIELD_NAMES
            .iter()
            .map(|n| sq.find(n).expect("sqlite has name"))
            .collect();
        // Names appear in canonical order on both arms.
        for w in pg_positions.windows(2) {
            assert!(w[0] < w[1], "pg names out of order: {pg}");
        }
        for w in sq_positions.windows(2) {
            assert!(w[0] < w[1], "sqlite names out of order: {sq}");
        }

        // Both arms emit the 3 implicit indexes.
        for col in &["deleted_at", "updated_at", "created_by"] {
            let idx = index_name("posts", &[col], false);
            assert!(pg.contains(&idx), "pg missing index {idx}");
            assert!(sq.contains(&idx), "sqlite missing index {idx}");
        }
    }

    /// `validate_field_name_for_declaration` MUST still enforce all
    /// the underlying `validate_field_name` rules (ASCII allowlist,
    /// length cap, null bytes, the `_*` / `__zs_*` / `_masked` reserved
    /// shapes). Regression fence for the wrapper composition.
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
        assert_eq!(p, "decode($3, 'base64')::bytea");
    }

    #[test]
    fn dialect_sqlite_encrypted_placeholder_is_bare_param() {
        let p = SqlDialect::Sqlite.binary_bind_placeholder(7);
        assert_eq!(p, "$7");
    }

    #[test]
    fn dialect_mysql_encrypted_placeholder_is_from_base64_param() {
        let p = SqlDialect::Mysql.binary_bind_placeholder(7);
        assert_eq!(p, "FROM_BASE64(?)");
    }

    #[test]
    fn dialect_pg_wrap_binary_bind_param_is_identity() {
        let v = SqlDialect::Postgres.wrap_binary_bind_param("abc==".to_string());
        assert_eq!(v, "abc==");
    }

    #[test]
    fn dialect_sqlite_wrap_binary_bind_param_prepends_sentinel() {
        let v = SqlDialect::Sqlite.wrap_binary_bind_param("abc==".to_string());
        assert_eq!(v, format!("{SQLITE_BINARY_BIND_PREFIX}abc=="));
    }

    #[test]
    fn dialect_mysql_wrap_binary_bind_param_is_identity() {
        let v = SqlDialect::Mysql.wrap_binary_bind_param("abc==".to_string());
        assert_eq!(v, "abc==");
    }

    /// PG-flavour `build_insert` for an encrypted column must continue
    /// to emit the `decode($N, 'base64')::bytea` cast byte-for-byte and
    /// pass the base64 param through unchanged. This pins the
    /// regression check the task calls out as load-bearing.
    #[test]
    fn build_insert_pg_encrypted_column_unchanged_from_pr2() {
        let doc = serde_json::json!({
            "id": "row1",
            "ssn": "Y2lwaGVydGV4dF9ibG9i",
            "__zsbin__ssn": true,
        });
        let bq = build_insert("app1", "users", &tschema(), &doc).expect("build_insert ok");
        assert!(
            bq.sql.contains("decode($"),
            "PG path must wrap encrypted-column placeholder with decode(...)::bytea: {}",
            bq.sql,
        );
        assert!(
            bq.sql.contains("'base64')::bytea"),
            "PG path must keep the BYTEA cast: {}",
            bq.sql,
        );
        // The encrypted param must reach the bind layer as plain
        // base64 (no `__zsbin_blob__:` sentinel on the PG arm).
        assert!(
            bq.params
                .iter()
                .all(|p| !p.starts_with(SQLITE_BINARY_BIND_PREFIX)),
            "PG path must never emit the SQLite blob sentinel: {:?}",
            bq.params,
        );
        // The marker key itself must not leak as a column.
        assert!(
            !bq.sql.contains("__zsbin__"),
            "marker keys must not appear as columns: {}",
            bq.sql,
        );
    }

    /// SQLite-flavour `build_insert_with_dialect` must emit a bare `$N`
    /// placeholder for encrypted columns and tag the param value with
    /// `SQLITE_BINARY_BIND_PREFIX` so the session actor binds raw bytes
    /// as BLOB. The marker key must not surface in the SQL.
    #[test]
    fn build_insert_sqlite_encrypted_column_emits_bare_placeholder() {
        let doc = serde_json::json!({
            "id": "row1",
            "ssn": "Y2lwaGVydGV4dF9ibG9i",
            "__zsbin__ssn": true,
        });
        let bq = build_insert_with_dialect("app1", "users", &tschema(), &doc, SqlDialect::Sqlite)
            .expect("build_insert_with_dialect ok");
        assert!(
            !bq.sql.contains("decode("),
            "SQLite path must NOT emit the PG `decode(...)::bytea` cast: {}",
            bq.sql,
        );
        assert!(
            !bq.sql.contains("::bytea"),
            "SQLite path must NOT emit the PG `::bytea` cast: {}",
            bq.sql,
        );
        // At least one param must carry the sentinel prefix (the
        // encrypted ssn value).
        assert!(
            bq.params
                .iter()
                .any(|p| p.starts_with(SQLITE_BINARY_BIND_PREFIX)),
            "SQLite path must tag the encrypted param with sentinel: {:?}",
            bq.params,
        );
        // The marker key itself must not leak as a column.
        assert!(
            !bq.sql.contains("__zsbin__"),
            "marker keys must not appear as columns: {}",
            bq.sql,
        );
    }

    /// `build_update_one_with_dialect` on the SQLite arm must mirror
    /// the insert path: bare `$N` for encrypted columns + sentinel-
    /// prefixed param. PG behaviour is byte-for-byte identical to the insert path.
    #[test]
    fn build_update_one_sqlite_encrypted_column_sentinel_tag() {
        let filter = serde_json::json!({ "id": "row1" });
        let update = serde_json::json!({
            "ssn": "Y2lwaGVydGV4dF9ibG9i",
            "__zsbin__ssn": true,
        });
        let bq = build_update_one_with_dialect(
            "app1",
            "users",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Sqlite,
        )
        .expect("build_update_one_with_dialect ok");
        assert!(!bq.sql.contains("decode("), "no PG cast: {}", bq.sql);
        assert!(
            bq.params
                .iter()
                .any(|p| p.starts_with(SQLITE_BINARY_BIND_PREFIX)),
            "sentinel tag on the encrypted param: {:?}",
            bq.params,
        );
    }

    /// `build_insert_many_with_dialect` SQLite arm: every per-doc
    /// encrypted-column param carries the sentinel; the SQL stays
    /// bare-`$N`.
    #[test]
    fn build_insert_many_sqlite_encrypted_columns_all_tagged() {
        let docs = serde_json::json!([
            { "id": "row1", "ssn": "Y2lwaGVydGV4dF9ibG9i", "__zsbin__ssn": true },
            { "id": "row2", "ssn": "YW5vdGhlcl9jaXBoZXI=", "__zsbin__ssn": true },
        ]);
        let bq = build_insert_many_with_dialect("app1", "users", &tschema(), &docs, SqlDialect::Sqlite)
            .expect("build_insert_many_with_dialect ok");
        assert!(!bq.sql.contains("decode("), "no PG cast: {}", bq.sql);
        let tagged = bq
            .params
            .iter()
            .filter(|p| p.starts_with(SQLITE_BINARY_BIND_PREFIX))
            .count();
        assert_eq!(tagged, 2, "both encrypted ssn params must be tagged: {:?}", bq.params);
    }

    // -----------------------------------------------------------------
    // Raw-column DDL emission
    // -----------------------------------------------------------------

    /// `raw_column_for_field` returns the raw column for masked columns and
    /// `None` for non-masked / kind=none columns.
    #[test]
    fn raw_column_for_field_returns_the_raw_column_for_masked() {
        let def = serde_json::json!({
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
        for field in ["ssn", "email", "a", &"x".repeat(MAX_MASKED_FIELD_NAME_BYTES)] {
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
        assert!(RESERVED_NAMES.iter().any(|r| matches!(r, ReservedName::Prefix("__zs_"))));
    }

    /// A masked field name one byte too long is REFUSED, not silently
    /// truncated.
    ///
    /// `raw_column_name` is a plain concatenation so that it can be
    /// byte-identical to the migration engine's copy without two hashing
    /// implementations that no compiler can check agree. The price is that the
    /// overlong case has to be refused somewhere, and this is where.
    ///
    /// The pre-flip `<field>_masked` sibling did neither: it neither capped nor
    /// refused, so a 60-character masked field produced a 67-character sibling
    /// that Postgres truncated, and two such fields could collide on one
    /// column.
    #[test]
    fn a_masked_field_name_too_long_for_its_raw_column_is_refused() {
        let masked = serde_json::json!({
            "type": "string",
            "mask": { "kind": "full", "classification": "pii" }
        });
        let plain = serde_json::json!({ "type": "string" });

        let at_limit = "x".repeat(MAX_MASKED_FIELD_NAME_BYTES);
        assert!(
            field_to_column_for_dialect(&at_limit, &masked, SqlDialect::Postgres).is_ok(),
            "a masked field exactly at the limit must still be declarable",
        );

        let over = "x".repeat(MAX_MASKED_FIELD_NAME_BYTES + 1);
        let err = field_to_column_for_dialect(&over, &masked, SqlDialect::Postgres)
            .expect_err("one byte over must be refused, not truncated");
        assert!(
            format!("{err}").contains("masked field name exceeds"),
            "expected the length refusal, got {err}",
        );

        // The control: the SAME name is fine on an UNMASKED field, because it
        // needs no second column. Without this arm a validator that refused
        // every long name would pass the assertion above.
        assert!(
            field_to_column_for_dialect(&over, &plain, SqlDialect::Postgres).is_ok(),
            "an unmasked field is bounded by NAMEDATALEN alone",
        );
    }

    #[test]
    fn raw_column_for_field_returns_none_for_unmasked() {
        let def = serde_json::json!({ "type": "string" });
        assert_eq!(raw_column_for_field("name", &def), None);
    }

    #[test]
    fn raw_column_for_field_returns_none_for_kind_none() {
        let def = serde_json::json!({
            "type": "string",
            "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
            "mask": { "kind": "none", "classification": "spi" }
        });
        assert_eq!(raw_column_for_field("ssn", &def), None);
    }

    /// **DDL shape** — a masked column emits its own `TEXT` column for the mask
    /// plus `__zs_raw__<col>` carrying the declared type.
    #[test]
    fn build_create_table_emits_a_raw_column_for_a_masked_column() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline)
            .expect("build_create_table_with_fks ok");
        assert!(
            sql.contains("\"ssn\" TEXT"),
            "the field's own column holds the mask, as bare TEXT: {sql}"
        );
        assert!(
            sql.contains(&format!("\"{}\" TEXT", raw_column_name("ssn"))),
            "the raw column carries the declared type: {sql}"
        );
        assert!(
            !sql.contains("\"ssn_masked\""),
            "the `_masked` sibling is gone: {sql}"
        );
        assert!(
            !sql.contains(&raw_column_name("name")),
            "a non-masked column must NOT emit a raw column: {sql}"
        );
    }

    /// **The type-and-constraint swap.** The declared type and the whole
    /// constraint set travel to the raw column; the masked column is bare TEXT.
    ///
    /// Leaving them on the field's own column is not a cosmetic mistake, it is
    /// a total write failure: `'***'` is not a `DOUBLE PRECISION`, and it is
    /// not in an enum `CHECK` list.
    #[test]
    fn a_masked_columns_type_and_constraints_travel_to_the_raw_column() {
        let schema = serde_json::json!({
            "score": {
                "type": "number",
                "required": true,
                "mask": { "kind": "full", "classification": "pii" }
            },
            "tier": {
                "type": "string",
                "enum": ["gold", "silver"],
                "mask": { "kind": "full", "classification": "pii" }
            }
        });
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline)
            .expect("build_create_table_with_fks ok");
        let raw_score = raw_column_name("score");
        let raw_tier = raw_column_name("tier");

        assert!(
            sql.contains(&format!("\"{raw_score}\" DOUBLE PRECISION")),
            "the declared numeric type belongs to the value: {sql}"
        );
        assert!(
            sql.contains("\"score\" TEXT"),
            "the masked column must be TEXT so it can hold '***': {sql}"
        );
        assert!(
            !sql.contains("\"score\" DOUBLE PRECISION"),
            "a mask written into a DOUBLE PRECISION column is a hard error: {sql}"
        );
        assert!(
            sql.contains(&format!("CHECK (\"{raw_tier}\" IN")),
            "the enum CHECK must name the raw column: {sql}"
        );
        assert!(
            !sql.contains("CHECK (\"tier\" IN"),
            "an enum CHECK on the masked column refuses every write: {sql}"
        );
    }

    /// Masked column CREATE TABLE emits `COMMENT ON COLUMN` on the field's
    /// OWN column (the masked one) so PG introspection round-trips the mask
    /// metadata via `pg_description`. Before the storage flip this rode on a
    /// `<col>_masked` sibling; that sibling is gone and the field's own name
    /// IS the masked column now, so the comment - and never the raw column -
    /// is what must carry the sentinel.
    #[test]
    fn build_create_table_emits_comment_on_column_sentinel_for_masked_column() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline)
            .expect("build_create_table_with_fks ok");
        assert!(
            sql.contains("COMMENT ON COLUMN \"app1\".\"users\".\"ssn\""),
            "expected COMMENT ON COLUMN for the field's own (masked) column: {sql}"
        );
        assert!(
            sql.contains("'__zsmask:kind=last4,classification=spi'"),
            "expected sentinel literal: {sql}"
        );
        assert!(
            !sql.contains(&format!(
                "COMMENT ON COLUMN \"app1\".\"users\".\"{}\"",
                raw_column_name("ssn")
            )),
            "the raw column must never carry the mask-sentinel comment: {sql}"
        );
    }

    /// Inline `/* __zsmask:... */` comment rides on the field's OWN
    /// (masked) column for SQLite-arm introspection (PG ignores SQL
    /// comments; SQLite preserves them in `sqlite_master.sql`).
    #[test]
    fn build_create_table_emits_inline_mask_sentinel_comment() {
        let schema = serde_json::json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline)
            .expect("build_create_table_with_fks ok");
        assert!(
            sql.contains("\"email\" TEXT /* __zsmask:kind=email,classification=pii */"),
            "expected inline /* __zsmask:... */ comment on the field's own \
             (masked) column: {sql}"
        );
        assert!(
            !sql.contains(&format!("\"{}\" TEXT /* __zsmask:", raw_column_name("email"))),
            "the raw column must never carry the inline mask-sentinel comment: {sql}"
        );
    }

    /// `kind: "none"` opt-out emits no sibling and no
    /// `COMMENT ON COLUMN`.
    #[test]
    fn build_create_table_no_comment_when_mask_kind_none() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "none", "classification": "pii" }
            }
        });
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline)
            .expect("build_create_table_with_fks ok");
        assert!(
            !sql.contains("COMMENT ON COLUMN"),
            "kind=none must emit no COMMENT: {sql}"
        );
        assert!(
            !sql.contains("__zsmask:"),
            "kind=none must emit no sentinel: {sql}"
        );
    }

    /// `build_add_column` for a fresh field with a `.mask({...})`
    /// declaration emits the RAW column ADD (the real value, carrying the
    /// declared type), the MASKED column ADD (the field's own name, bare
    /// `TEXT NULL`), and the `COMMENT ON COLUMN` sentinel attached to the
    /// masked column - all in one multi-statement payload.
    #[test]
    fn build_add_column_emits_raw_and_masked_columns_and_sentinel_when_masked() {
        let def = serde_json::json!({
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        });
        let sql = build_add_column("app1", "users", "ssn", &def).expect("build_add_column ok");
        let raw = raw_column_name("ssn");
        assert!(
            sql.contains(&format!("ADD COLUMN IF NOT EXISTS \"{raw}\"")),
            "raw column: {sql}"
        );
        assert!(
            sql.contains("ADD COLUMN IF NOT EXISTS \"ssn\" TEXT NULL"),
            "masked column (the field's own name): {sql}"
        );
        assert!(
            sql.contains("COMMENT ON COLUMN \"app1\".\"users\".\"ssn\""),
            "comment attaches to the masked column: {sql}"
        );
        assert!(
            !sql.contains(&format!("COMMENT ON COLUMN \"app1\".\"users\".\"{raw}\"")),
            "the raw column must never carry the mask-sentinel comment: {sql}"
        );
        assert!(
            sql.contains("'__zsmask:kind=last4,classification=spi'"),
            "sentinel: {sql}"
        );
    }

    /// `build_add_column` for a non-masked field emits
    /// only the single parent ADD; no sibling DDL, no comment.
    #[test]
    fn build_add_column_no_sibling_when_unmasked() {
        let def = serde_json::json!({ "type": "string" });
        let sql = build_add_column("app1", "users", "name", &def).expect("build_add_column ok");
        assert!(!sql.contains("_masked"), "no sibling for unmasked: {sql}");
        assert!(!sql.contains("COMMENT ON COLUMN"), "no comment: {sql}");
    }

    /// **DDL shape** - `t.encrypted(...)` (default-mask path) gets a raw
    /// column because schema-normalisation auto-populates
    /// `mask: {kind: "full", ...}` on encrypted columns. The raw column (the
    /// real, encrypted value) carries the declared ciphertext type BYTEA;
    /// the field's own column (the mask) is bare, nullable TEXT.
    #[test]
    fn build_create_table_emits_raw_column_for_encrypted_with_default_mask() {
        // Mirror the SDK's auto-fill: `t.encrypted(...)` -> mask = full.
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
                "mask": { "kind": "full", "classification": "pii" }
            }
        });
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline)
            .expect("build_create_table_with_fks ok");
        let raw = raw_column_name("ssn");
        assert!(
            sql.contains(&format!("\"{raw}\" BYTEA")),
            "raw column carries the ciphertext type BYTEA: {sql}"
        );
        assert!(
            sql.contains("\"ssn\" TEXT"),
            "the field's own column (the mask) is TEXT: {sql}"
        );
        assert!(
            !sql.contains("\"ssn\" TEXT NOT NULL"),
            "masked column must be nullable / omittable: {sql}"
        );
        assert!(
            !sql.contains("\"ssn\" BYTEA"),
            "the field's own column must never carry the ciphertext type: {sql}"
        );
    }

    /// **DDL shape** — `kind: "none"` explicit opt-out → no sibling.
    /// The parent encrypted column behaves like the unmasked baseline.
    #[test]
    fn build_create_table_no_sibling_when_mask_kind_none() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
                "mask": { "kind": "none", "classification": "pii" }
            }
        });
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline)
            .expect("build_create_table_with_fks ok");
        assert!(
            !sql.contains("\"ssn_masked\""),
            "kind=none must NOT emit a sibling: {sql}"
        );
        assert!(sql.contains("\"ssn\" BYTEA"), "parent still present: {sql}");
    }

    /// **Index auto-emit** - `.index()` on a masked field produces a B-tree
    /// index on the RAW column (the real value `.index()` describes) PLUS
    /// an automatic B-tree index on the field's own (masked) column, since
    /// every creator-visible read/filter now touches that column. Neither
    /// is ever UNIQUE (the field only declared `.index()`).
    #[test]
    fn build_create_indexes_emits_btree_on_raw_and_mask_columns_when_indexed() {
        let schema = serde_json::json!({
            "email": {
                "type": "string",
                "index": true,
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();

        let raw = raw_column_name("email");
        let raw_idx: Vec<_> = out.iter().filter(|s| s.columns == vec![raw.clone()]).collect();
        assert_eq!(raw_idx.len(), 1, "expected one index on the raw column: {out:?}");
        assert!(!raw_idx[0].unique, "raw-column index must NEVER be UNIQUE: {raw_idx:?}");
        assert!(
            raw_idx[0].sql.contains("CREATE INDEX") && !raw_idx[0].sql.contains("UNIQUE"),
            "raw-column index uses CREATE INDEX, not CREATE UNIQUE INDEX: {}",
            raw_idx[0].sql,
        );

        let mask_idx: Vec<_> = out
            .iter()
            .filter(|s| s.columns == vec!["email".to_string()])
            .collect();
        assert_eq!(mask_idx.len(), 1, "expected one auto index on the masked column: {out:?}");
        assert!(
            !mask_idx[0].unique,
            "the auto mask-column index must NEVER be UNIQUE: {mask_idx:?}"
        );
        assert!(
            mask_idx[0].sql.contains("CREATE INDEX") && !mask_idx[0].sql.contains("UNIQUE"),
            "mask-column index DDL must not say UNIQUE: {}",
            mask_idx[0].sql,
        );
    }

    /// **Index auto-emit** - `.unique()` on a masked field puts the UNIQUE
    /// constraint on the RAW column (the real value), never on the field's
    /// own (masked) column: a `.unique()` on the mask would enforce
    /// uniqueness over MASKS, and for `kind: "full"` every mask is the same
    /// `***`, capping the table at one row. The automatic mask-column index
    /// alongside it is always a plain (non-unique) B-tree.
    #[test]
    fn build_create_indexes_unique_on_raw_column_plain_btree_on_mask_column() {
        let schema = serde_json::json!({
            "email": {
                "type": "string",
                "unique": true,
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        let raw = raw_column_name("email");
        let raw_idx = out
            .iter()
            .find(|s| s.columns == vec![raw.clone()])
            .expect("unique index on the raw column");
        assert!(raw_idx.unique, "uniqueness must land on the raw column: {raw_idx:?}");

        let mask_idx = out
            .iter()
            .find(|s| s.columns == vec!["email".to_string()])
            .expect("auto index on the masked column");
        assert!(
            !mask_idx.unique,
            "the mask column's auto index must never be UNIQUE, even when \
             the field declares .unique(): {mask_idx:?}"
        );
    }

    /// **Index auto-emit** — a masked field with NO `.index()` / `.unique()`
    /// gets no index at all, on either of its two columns.
    ///
    /// This test used to look for a column named literally `"ssn_masked"`,
    /// which cannot exist under any outcome after the storage flip - so it
    /// passed whatever `build_create_indexes` did, including emitting both
    /// indexes for an unindexed field. It now names both real columns.
    #[test]
    fn build_create_indexes_emits_nothing_for_an_unindexed_masked_field() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        let raw = raw_column_name("ssn");
        assert!(
            out.iter()
                .all(|s| !s.columns.iter().any(|c| c == "ssn" || *c == raw)),
            "an unindexed masked field must produce no index on either of its \
             columns ({raw:?} or \"ssn\"): {out:?}"
        );

        // The control: the SAME schema WITH `.index()` produces both, so this
        // is not a green from `build_create_indexes` returning nothing ever.
        let indexed = serde_json::json!({
            "ssn": {
                "type": "string",
                "index": true,
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let out = build_create_indexes("app1", "users", &indexed).unwrap();
        assert!(out.iter().any(|s| s.columns == vec![raw.clone()]));
        assert!(out.iter().any(|s| s.columns == vec!["ssn".to_string()]));
    }

    /// **Build insert** — when the row carries both parent + sibling
    /// (mask pass already ran), the INSERT statement includes both
    /// columns atomically.
    #[test]
    fn build_insert_includes_sibling_column_when_present() {
        let doc = serde_json::json!({
            "id": "usr_01",
            "ssn": "123-45-6789",
            "ssn_masked": "***-**-6789"
        });
        let bq = build_insert("app1", "users", &tschema(), &doc).expect("build_insert ok");
        assert!(bq.sql.contains("\"ssn\""), "parent column in SQL: {}", bq.sql);
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
        let schema = serde_json::json!({
            "ssn":   { "type": "string", "encrypted": { "mode": "randomised" },
                       "mask": { "kind": "last4", "classification": "spi" } },
            "email": { "type": "string" },
            "name":  { "type": "string" },
        });
        let filter = serde_json::json!({ "id": 7 });
        let bq = build_find_with_schema(
            "app1", "users", &filter, Some(1), None, None, None, &schema,
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
        let schema = serde_json::json!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
        });
        let filter = serde_json::json!({});
        let select = serde_json::json!(["id", "ssn"]);
        let bq = build_find_with_schema(
            "app1", "users", &filter, None, None, None, Some(&select), &schema,
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
        let schema = serde_json::json!({
            "ssn":   { "type": "string", "encrypted": { "mode": "randomised" },
                       "mask": { "kind": "last4", "classification": "spi" } },
            "email": { "type": "string",
                       "mask": { "kind": "full", "classification": "pii" } },
        });
        let filter = serde_json::json!({});

        let bq_no_hint = build_find_with_schema(
            "app1", "users", &filter, None, None, None, None, &schema,
        )
        .expect("build_find_with_schema ok");

        let unmask: Vec<String> = vec!["ssn".to_string()];
        let bq_with_hint = build_find_with_schema_and_unmask(
            "app1", "users", &filter, None, None, None, None, &schema, &unmask,
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
        let schema = serde_json::json!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
        });
        let filter = serde_json::json!({ "ssn_masked": "***-**-6789" });
        let err = build_find_with_schema(
            "app1", "users", &filter, None, None, None, None, &schema,
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
        let schema = serde_json::json!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
        });
        let filter = serde_json::json!({});
        let bq = build_find_with_schema(
            "app1", "users", &filter, None, None, None, None, &schema,
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
        assert!(bq.sql.contains("\"id\""));
        assert!(
            !bq.sql.contains(&raw_column_name("ssn")),
            "the raw column must never appear: {}",
            bq.sql,
        );
    }

    /// **L24, arm 1** — the read projection is TOTAL. There is no schema value
    /// that produces `SELECT *`.
    ///
    /// The defect this replaces: `implicit_read_projection_parts` returned
    /// `None` for an absent schema and
    /// `build_masked_aware_select_expr_with_unmask` fell through to a bare `*`,
    /// so the allowlist that keeps mask siblings, raw columns and every other
    /// platform-emitted column out of a result set simply stopped applying -
    /// exactly when the schema had not arrived yet. Measured before the fix:
    /// `build_find_with_schema(..., None)` returned
    /// `SELECT * FROM "app1"."users"`.
    ///
    /// The absent case is no longer expressible: the parameter is `&Value`, not
    /// `Option<&Value>`. The weakest schema a caller can now supply is the
    /// EMPTY one, and this pins that even THAT fails closed.
    #[test]
    fn the_weakest_possible_schema_still_projects_an_explicit_allowlist() {
        let bq = build_find_with_schema(
            "app1",
            "users",
            &serde_json::json!({}),
            None,
            None,
            None,
            None,
            &empty_read_schema(),
        )
        .unwrap();
        assert!(
            !bq.sql.contains('*'),
            "the empty schema must still project an explicit list; got {}",
            bq.sql,
        );
        for field in SYSTEM_FIELD_NAMES {
            assert!(
                bq.sql.contains(&quote_ident(field)),
                "the system field {field} must be projected; got {}",
                bq.sql,
            );
        }
    }

    /// **L24, arm 2** — the read-identifier allowlist has no permissive arm.
    ///
    /// `validate_read_identifier` used to raise `InvalidIdent` only `if
    /// schema_hint.is_some()`. Measured before the fix, a `select` of an
    /// undeclared field with no schema returned
    /// `SELECT "not_a_declared_field" FROM "app1"."users"`. The empty schema is
    /// the closest a caller can now come to "no schema", and it refuses.
    #[test]
    fn an_undeclared_identifier_is_refused_under_the_empty_schema() {
        let refused = build_find_with_schema(
            "app1",
            "users",
            &serde_json::json!({}),
            None,
            None,
            None,
            Some(&serde_json::json!(["not_a_declared_field"])),
            &empty_read_schema(),
        );
        assert!(
            matches!(refused, Err(QueryError::InvalidIdent(_))),
            "an undeclared identifier must be refused; got {:?}",
            refused.map(|bq| bq.sql),
        );
    }

    /// **L24, arm 3** — a schema value that is not a field map is an ERROR, not
    /// a fallback. This is the one remaining way a caller could smuggle
    /// "absent" through a `&Value`, and it is closed.
    #[test]
    fn a_non_object_schema_is_an_error_not_an_unrestricted_projection() {
        for bad in [Value::Null, json!([]), json!("users"), json!(7)] {
            let refused = build_find_with_schema(
                "app1",
                "users",
                &serde_json::json!({}),
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

    /// The same property on the SEARCH projection
    /// (`build_masked_aware_select_expr_for_table_alias`), which had its own
    /// `"t".*` fallback on the identical condition.
    #[test]
    fn the_table_alias_projection_has_no_star_arm() {
        let expanded = build_masked_aware_select_expr_for_table_alias(&empty_read_schema(), "t")
            .expect("the empty schema is a field map");
        assert!(
            !expanded.contains('*'),
            "the aliased projection must never expand to `t.*`; got {expanded}",
        );
        assert!(
            build_masked_aware_select_expr_for_table_alias(&Value::Null, "t").is_err(),
            "a non-object schema must be refused rather than expanded to `t.*`",
        );
    }

    /// The masked schema both L26 arms are built from. It carries the v2
    /// descriptor's `storage` block, because that is what a `.zship` deploy
    /// actually caches (`crates/zeroship-migrate-core/src/render/gen_types.rs:327-360`
    /// stamps it; `sdks/bootstrap/src/install-schema.ts:369-372` spreads it into
    /// the `registerModel` payload verbatim).
    fn l26_masked_schema() -> Value {
        serde_json::json!({
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
            "app1",
            "users",
            &serde_json::json!({}),
            Some(10),
            None,
            Some(&serde_json::json!({ "ssn": 1 })),
            None,
            &schema,
        )
        .unwrap();
        assert!(
            bq.sql.contains(&format!("ORDER BY {} ASC", quote_ident(read))),
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
            "app1",
            "users",
            &serde_json::json!({}),
            None,
            None,
            Some(&serde_json::json!([["ssn", -1]])),
            None,
            &schema,
        )
        .unwrap();
        assert!(
            bq.sql.contains(&format!("ORDER BY {} DESC", quote_ident(read))),
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
        let schema = serde_json::json!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
            "embedding": { "type": "vector" },
        });
        let q = build_vector_search(
            "app1",
            "users",
            "embedding",
            &[0.1, 0.2],
            5,
            crate::descriptors::VectorMetric::Cosine,
            &serde_json::json!({}),
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
        let schema = serde_json::json!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
            "location": { "type": "geoPoint" },
        });
        let q = build_spatial_near(
            "app1",
            "users",
            "location",
            crate::descriptors::GeoPoint { lat: 37.7, lng: -122.4 },
            1000.0,
            &serde_json::json!({}),
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
        let schema = serde_json::json!({
            "name": { "type": "string" },
        });
        let bq = build_find_with_schema(
            "app1",
            "users",
            &serde_json::json!({}),
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
            bq.sql.contains("\"created_at\"") && bq.sql.contains("\"name\""),
            "schema-backed find must project public system fields + declared fields: {}",
            bq.sql,
        );
    }

    #[test]
    fn read_side_identifiers_reject_internal_physical_columns() {
        let schema = serde_json::json!({
            "name": { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
        });

        let select_err = build_find_with_schema(
            "app1",
            "users",
            &serde_json::json!({}),
            None,
            None,
            None,
            Some(&serde_json::json!(["ssn_masked"])),
            &schema,
        )
        .expect_err("select on masked sibling must be refused");
        assert!(matches!(select_err, QueryError::InvalidIdent(_)));

        let sort_err = build_find_with_schema(
            "app1",
            "users",
            &serde_json::json!({}),
            None,
            None,
            Some(&serde_json::json!({ "ssn_masked": 1 })),
            None,
            &schema,
        )
        .expect_err("sort on masked sibling must be refused");
        assert!(matches!(sort_err, QueryError::InvalidIdent(_)));

        let distinct_err = build_distinct_with_soft_delete_with_dialect(
            "app1",
            "users",
            "ssn_masked",
            &serde_json::json!({}),
            false,
            &schema,
            SqlDialect::Postgres,
        )
        .expect_err("distinct on masked sibling must be refused");
        assert!(matches!(distinct_err, QueryError::InvalidIdent(_)));

        let aggregate_err = build_aggregate_with_soft_delete_with_dialect(
            "app1",
            "users",
            &serde_json::json!([
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
        let schema = serde_json::json!({
            "name": { "type": "string" },
        });
        let err = build_find_with_schema(
            "app1",
            "users",
            &serde_json::json!({}),
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
        let schema = serde_json::json!({
            "embedding": { "type": "vector" },
        });
        let err = build_vector_search(
            "app1",
            "users",
            "embedding",
            &[0.1, 0.2],
            MAX_SEARCH_LIMIT + 1,
            crate::descriptors::VectorMetric::Cosine,
            &serde_json::json!({}),
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
        let schema = serde_json::json!({
            "name": { "type": "string" },
        });
        let mut filter = serde_json::json!({ "name": "alice" });
        for _ in 0..MAX_FILTER_NESTING_DEPTH {
            filter = serde_json::json!({ "$and": [filter] });
        }
        let err = build_find_with_schema(
            "app1",
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
        let mut filter = serde_json::Map::new();
        for idx in 0..=MAX_FILTER_CLAUSE_COUNT {
            filter.insert(format!("f{idx}"), serde_json::json!(idx));
        }
        let mut params = Vec::new();
        let err = build_where(&Value::Object(filter), &mut params)
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
        assert_eq!(compose_where_with_soft_delete("", false), "");
        assert_eq!(
            compose_where_with_soft_delete("\"id\" = $1", false),
            "\"id\" = $1"
        );
    }

    #[test]
    fn compose_where_with_soft_delete_empty_to_lone_predicate() {
        assert_eq!(
            compose_where_with_soft_delete("", true),
            "\"deleted_at\" IS NULL"
        );
    }

    #[test]
    fn compose_where_with_soft_delete_appends_with_and() {
        assert_eq!(
            compose_where_with_soft_delete("\"id\" = $1", true),
            "\"id\" = $1 AND \"deleted_at\" IS NULL"
        );
    }

    #[test]
    fn build_soft_delete_one_emits_update_with_deleted_at_now() {
        let filter = serde_json::json!({ "id": "post_x" });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_soft_delete_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(q.sql.starts_with("UPDATE \"app1\".\"posts\" SET"), "sql: {}", q.sql);
        assert!(
            q.sql.contains("\"deleted_at\" = NOW()"),
            "expected deleted_at = NOW(); got: {}",
            q.sql
        );
        assert!(q.sql.contains("\"version\" = \"version\" + 1"));
        assert!(q.sql.contains("\"updated_at\" = NOW()"));
        assert!(q.sql.contains("\"updated_by\" ="));
        assert!(q.sql.contains("AND \"deleted_at\" IS NULL"));
        assert!(q.sql.contains("WHERE ctid = (SELECT ctid FROM"));
    }

    #[test]
    fn build_soft_delete_one_sqlite_uses_current_timestamp() {
        let filter = serde_json::json!({ "id": "post_x" });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr"),
            ..Default::default()
        };
        let q = build_soft_delete_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Sqlite,
            &autobump,
        )
        .unwrap();
        assert!(
            q.sql.contains("\"deleted_at\" = CURRENT_TIMESTAMP"),
            "SQLite must use CURRENT_TIMESTAMP: {}",
            q.sql
        );
        assert!(q.sql.contains("\"updated_at\" = CURRENT_TIMESTAMP"));
    }

    #[test]
    fn build_soft_delete_many_omits_ctid_narrowing() {
        let filter = serde_json::json!({ "author": "usr_x" });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_soft_delete_many_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(
            !q.sql.contains("WHERE ctid ="),
            "bulk soft-delete must not narrow via ctid: {}",
            q.sql
        );
        assert!(q.sql.contains("AND \"deleted_at\" IS NULL"));
        assert!(q.sql.ends_with(&treturning()), "sql: {}", q.sql);
    }

    #[test]
    fn build_soft_delete_one_no_actor_omits_updated_by_clause() {
        let filter = serde_json::json!({ "id": "post_x" });
        let autobump = SystemFieldAutoBump::default();
        let q = build_soft_delete_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(
            !set_clause_of(&q.sql).contains("\"updated_by\""),
            "no actor → no updated_by SET clause: {}",
            q.sql
        );
        assert!(q.sql.contains("\"deleted_at\" = NOW()"));
    }

    #[test]
    fn build_restore_one_clears_deleted_at_and_scopes_to_soft_deleted() {
        let filter = serde_json::json!({ "id": "post_x" });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_restore_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(q.sql.contains("\"deleted_at\" = NULL"));
        assert!(q.sql.contains("\"version\" = \"version\" + 1"));
        assert!(q.sql.contains("\"updated_at\" = NOW()"));
        assert!(q.sql.contains("\"updated_by\" ="));
        assert!(q.sql.contains("AND \"deleted_at\" IS NOT NULL"));
    }

    #[test]
    fn build_restore_many_omits_ctid_narrowing() {
        let filter = serde_json::json!({ "author": "usr_x" });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_restore_many_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(!q.sql.contains("WHERE ctid ="));
        assert!(q.sql.contains("AND \"deleted_at\" IS NOT NULL"));
        assert!(q.sql.ends_with(&treturning()), "sql: {}", q.sql);
    }

    #[test]
    fn build_find_with_soft_delete_flag_appends_filter() {
        let filter = serde_json::json!({ "title": "hi" });
        let q = build_find_with_schema_and_unmask_and_soft_delete(
            "app1", "posts", &filter, None, None, None, None, &tschema(), &[], true,
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
        let filter = serde_json::json!({ "title": "hi" });
        let q_legacy = build_find_with_schema_and_unmask(
            "app1", "posts", &filter, None, None, None, None, &tschema(), &[],
        )
        .unwrap();
        let q_new = build_find_with_schema_and_unmask_and_soft_delete(
            "app1", "posts", &filter, None, None, None, None, &tschema(), &[], false,
        )
        .unwrap();
        assert_eq!(q_legacy.sql, q_new.sql, "back-compat: identical SQL");
        assert_eq!(q_legacy.params, q_new.params);
    }

    #[test]
    fn build_find_empty_filter_with_soft_delete_flag_emits_lone_predicate() {
        let filter = serde_json::json!({});
        let q = build_find_with_schema_and_unmask_and_soft_delete(
            "app1", "posts", &filter, None, None, None, None, &tschema(), &[], true,
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
        let filter = serde_json::json!({});
        let q = build_count_with_soft_delete("app1", "posts", &filter, true).unwrap();
        assert!(q.sql.contains("WHERE \"deleted_at\" IS NULL"));
        let q2 = build_count_with_soft_delete("app1", "posts", &filter, false).unwrap();
        assert!(!q2.sql.contains("WHERE"));
    }

    #[test]
    fn build_aggregate_with_soft_delete_appends_filter() {
        let pipeline = serde_json::json!([
            { "$match": { "country": "US" } },
            { "$group": { "by": "city", "n": { "$count": 1 } } },
        ]);
        let q = build_aggregate_with_soft_delete("app1", "users", &pipeline, true, &tschema()).unwrap();
        assert!(
            q.sql.contains("WHERE ") && q.sql.contains("AND \"deleted_at\" IS NULL"),
            "aggregate WHERE must compose creator $match AND soft-delete: {}",
            q.sql
        );
    }

    #[test]
    fn build_distinct_with_soft_delete_appends_filter() {
        let filter = serde_json::json!({});
        let q = build_distinct_with_soft_delete("app1", "users", "country", &filter, true, &tschema()).unwrap();
        assert!(q.sql.contains("WHERE \"deleted_at\" IS NULL"));
    }

    #[test]
    fn legacy_build_count_is_byte_identical_to_soft_delete_off() {
        let filter = serde_json::json!({ "id": "x" });
        let q_legacy = build_count("app1", "posts", &filter).unwrap();
        let q_new = build_count_with_soft_delete("app1", "posts", &filter, false).unwrap();
        assert_eq!(q_legacy.sql, q_new.sql);
    }

    #[test]
    fn build_update_one_sqlite_uses_rowid_narrowing() {
        let filter = serde_json::json!({ "id": "post_1" });
        let update = serde_json::json!({ "title": "next" });
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            &update,
            SqlDialect::Sqlite,
            &SystemFieldAutoBump::default(),
        )
        .unwrap();
        assert!(q.sql.contains("WHERE rowid = (SELECT rowid FROM"));
        assert!(!q.sql.contains("WHERE ctid = (SELECT ctid FROM"));
    }

    #[test]
    fn build_delete_one_sqlite_uses_rowid_narrowing() {
        let filter = serde_json::json!({ "id": "post_1" });
        let q = build_delete_one_with_dialect("app1", "posts", &tschema(), &filter, SqlDialect::Sqlite)
            .unwrap();
        assert!(q.sql.contains("WHERE rowid = (SELECT rowid FROM"));
        assert!(!q.sql.contains("WHERE ctid = (SELECT ctid FROM"));
    }

    #[test]
    fn build_soft_delete_one_sqlite_uses_rowid_narrowing() {
        let filter = serde_json::json!({ "id": "post_1" });
        let q = build_soft_delete_one_with_system_fields(
            "app1",
            "posts",
            &tschema(),
            &filter,
            SqlDialect::Sqlite,
            &SystemFieldAutoBump::default(),
        )
        .unwrap();
        assert!(q.sql.contains("WHERE rowid = (SELECT rowid FROM"));
        assert!(!q.sql.contains("WHERE ctid = (SELECT ctid FROM"));
    }

    // -----------------------------------------------------------------------
    // PHASE 4 — `SqliteEmitScope` namespacing (descriptor→DDL for the migrate
    // engine). The `MainUnqualified` scope drops the `<app_id>` qualifier on
    // the SQLite arm so the DDL lands in `main` (= the app file). PG and the
    // `AttachAlias` SQLite default are unchanged (regression guard).
    // -----------------------------------------------------------------------

    /// A descriptor carrying a masked column, an encrypted column, and an FK —
    /// the goodies PHASE 4 must round-trip through emit→apply→drift.
    fn goodies_schema() -> serde_json::Value {
        json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "pii" }
            },
            "secret": {
                "type": "bytes",
                "encrypted": { "mode": "randomized", "keyId": "k1" }
            },
            "owner": {
                "type": "ref",
                "refTarget": "users"
            }
        })
    }

    /// `MainUnqualified` SQLite emits an UNqualified `CREATE TABLE "<coll>"`
    /// (no `"<app_id>".` prefix) and UNqualified system indexes — so the DDL
    /// lands in `main` under the migrate engine's hardened authorizer.
    #[test]
    fn sqlite_main_unqualified_drops_app_id_qualifier() {
        let app_id = "app_demo";
        let sql = build_create_table_with_fks_for_dialect_scoped(
            app_id,
            "posts",
            &json!({ "title": { "type": "string", "required": true } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
            SqliteEmitScope::MainUnqualified,
        )
        .expect("build unqualified sqlite ddl");

        // The table is UNqualified.
        assert!(
            sql.contains(r#"CREATE TABLE IF NOT EXISTS "posts" ("#),
            "table must be unqualified, got: {sql}"
        );
        // No `"<app_id>".` qualifier anywhere in the payload.
        assert!(
            !sql.contains(r#""app_demo"."#),
            "MainUnqualified must not emit any `\"app_demo\".` qualifier: {sql}"
        );
        // The system indexes are unqualified too (no schema on the index name).
        assert!(
            sql.contains(r#"CREATE INDEX IF NOT EXISTS "posts_deleted_at_idx" ON "posts""#),
            "system index must be unqualified, got: {sql}"
        );
    }

    /// The stable `AttachAlias` SQLite default is BYTE-UNCHANGED — it keeps the
    /// `"<app_id>"`-qualified table + index spelling plugin-db's runtime depends
    /// on (it ATTACHes the file under the `<app_id>` alias).
    #[test]
    fn sqlite_attach_alias_keeps_app_id_qualifier() {
        let app_id = "app_demo";
        let default_sql = build_create_table_with_fks_for_dialect(
            app_id,
            "posts",
            &json!({ "title": { "type": "string", "required": true } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build default sqlite ddl");
        let scoped_sql = build_create_table_with_fks_for_dialect_scoped(
            app_id,
            "posts",
            &json!({ "title": { "type": "string", "required": true } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
            SqliteEmitScope::AttachAlias,
        )
        .expect("build attach-alias sqlite ddl");

        // The stable entry point == the explicit `AttachAlias` scope.
        assert_eq!(
            default_sql, scoped_sql,
            "the stable dialected entry point must equal AttachAlias scope"
        );
        // It is `<app_id>`-qualified (the plugin-db ATTACH-alias contract).
        assert!(
            default_sql.contains(r#"CREATE TABLE IF NOT EXISTS "app_demo"."posts" ("#),
            "AttachAlias must keep the app_id-qualified table: {default_sql}"
        );
        assert!(
            default_sql.contains(r#"CREATE INDEX IF NOT EXISTS "app_demo"."posts_deleted_at_idx""#),
            "AttachAlias must keep the app_id-qualified index: {default_sql}"
        );
    }

    /// The PG arm is BYTE-IDENTICAL regardless of `sqlite_scope` (the scope only
    /// flips the SQLite qualifier). This is the PG-regression bar.
    #[test]
    fn pg_arm_byte_identical_across_sqlite_scopes() {
        let app_id = "app_demo";
        let schema = goodies_schema();
        let via_stable = build_create_table_with_fks_for_dialect(
            app_id,
            "accounts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Postgres,
        )
        .expect("pg via stable entry");
        for scope in [SqliteEmitScope::AttachAlias, SqliteEmitScope::MainUnqualified] {
            let via_scoped = build_create_table_with_fks_for_dialect_scoped(
                app_id,
                "accounts",
                &schema,
                &FkEmission::Inline,
                SqlDialect::Postgres,
                scope,
            )
            .expect("pg via scoped entry");
            assert_eq!(
                via_stable, via_scoped,
                "PG arm must be byte-identical regardless of sqlite_scope ({scope:?})"
            );
        }
        // And the PG arm is still `<schema>`-qualified.
        assert!(via_stable.contains(r#"CREATE TABLE IF NOT EXISTS "app_demo"."accounts" ("#));
    }

    /// `MainUnqualified` SQLite carries the goodies: the inline `__zsmask:`
    /// mask sentinel rides on the field's own (masked) `ssn` column - not a
    /// `_masked` sibling, which is gone after the storage flip - the inline
    /// `zsenc:` encryption sentinel rides on the (unmasked) `secret` BLOB
    /// column, and an unqualified FK clause is present - so all three
    /// survive into `sqlite_master.sql` for the drift snapshot to recover.
    #[test]
    fn sqlite_main_unqualified_carries_mask_enc_and_fk() {
        let sql = build_create_table_with_fks_for_dialect_scoped(
            "app_demo",
            "accounts",
            &goodies_schema(),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
            SqliteEmitScope::MainUnqualified,
        )
        .expect("build goodies sqlite ddl");

        // Mask sentinel rides inline on the field's own (masked) column.
        assert!(
            sql.contains(r#""ssn" TEXT /* __zsmask:"#),
            "mask sentinel must ride inline on the field's own (masked) column: {sql}"
        );
        // The raw column (the real ssn value) must never carry the mask
        // sentinel.
        assert!(
            !sql.contains(&format!("\"{}\" TEXT /* __zsmask:", raw_column_name("ssn"))),
            "the raw column must never carry the inline mask sentinel: {sql}"
        );
        // Encryption: BLOB physical column + inline `zsenc:` sentinel. (The
        // `secret` field carries no `mask` declaration, so it stays on its
        // own column - no raw sibling here.)
        assert!(
            sql.contains("BLOB") && sql.contains("/* zsenc:"),
            "encrypted column must be BLOB with an inline zsenc sentinel: {sql}"
        );
        // FK present and UNqualified (SQLite REFERENCES rejects a schema-qualified
        // parent name).
        assert!(
            sql.contains("FOREIGN KEY") && sql.contains(r#"REFERENCES "users" (id)"#),
            "FK must be present and reference an unqualified parent: {sql}"
        );
        // No SQLite-arm `COMMENT ON COLUMN` (PG-only); the inline sentinels are
        // the SQLite wire.
        assert!(
            !sql.contains("COMMENT ON COLUMN"),
            "SQLite arm must not emit COMMENT ON COLUMN: {sql}"
        );
        // Still fully unqualified.
        assert!(!sql.contains(r#""app_demo"."#), "must stay unqualified: {sql}");
    }

    // -----------------------------------------------------------------------
    // The write-side projection: `RETURNING` names columns, never `*`
    // -----------------------------------------------------------------------

    /// Every write builder in this file, built over one schema, with the verb
    /// each entry is named for.
    ///
    /// A `Vec` rather than twelve separate tests because the property is about
    /// the SET: the defect this guards is one builder being missed, and a test
    /// per builder cannot fail for a builder nobody wrote a test for. The arm
    /// count is asserted by the callers against
    /// [`WRITE_BUILDERS_EMITTING_RETURNING`].
    fn every_write_query(schema: &Value) -> Vec<(&'static str, BuiltQuery)> {
        let doc = serde_json::json!({ "ssn": "123-45-6789" });
        let docs = serde_json::json!([{ "ssn": "1" }, { "ssn": "2" }]);
        let filter = serde_json::json!({ "id": "usr_1" });
        let update = serde_json::json!({ "ssn": "9" });
        let conflict = serde_json::json!(["id"]);
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let d = SqlDialect::Postgres;
        vec![
            (
                "insert",
                build_insert_with_dialect("app1", "users", schema, &doc, d).unwrap(),
            ),
            (
                "insertMany",
                build_insert_many_with_dialect("app1", "users", schema, &docs, d).unwrap(),
            ),
            (
                "updateOne",
                build_update_one_with_system_fields(
                    "app1", "users", schema, &filter, &update, d, &autobump,
                )
                .unwrap(),
            ),
            (
                "updateMany",
                build_update_many_with_system_fields(
                    "app1", "users", schema, &filter, &update, d, &autobump,
                )
                .unwrap(),
            ),
            (
                "deleteOne",
                build_delete_one_with_dialect("app1", "users", schema, &filter, d).unwrap(),
            ),
            (
                "deleteMany",
                build_delete_many("app1", "users", schema, &filter).unwrap(),
            ),
            (
                "softDeleteOne",
                build_soft_delete_one_with_system_fields(
                    "app1", "users", schema, &filter, d, &autobump,
                )
                .unwrap(),
            ),
            (
                "softDeleteMany",
                build_soft_delete_many_with_system_fields(
                    "app1", "users", schema, &filter, d, &autobump,
                )
                .unwrap(),
            ),
            (
                "restoreOne",
                build_restore_one_with_system_fields(
                    "app1", "users", schema, &filter, d, &autobump,
                )
                .unwrap(),
            ),
            (
                "restoreMany",
                build_restore_many_with_system_fields(
                    "app1", "users", schema, &filter, d, &autobump,
                )
                .unwrap(),
            ),
            (
                "upsert",
                build_upsert_with_dialect("app1", "users", schema, &doc, &conflict, d).unwrap(),
            ),
            (
                "findOrCreate",
                build_find_or_create("app1", "users", schema, &doc, &conflict).unwrap(),
            ),
        ]
    }

    /// The arm floor for [`every_write_query`]. Twelve, counted from the
    /// `RETURNING`-emitting `format!`/`push_str` sites in this file - NOT from
    /// the twelve doc comments that also spell the word, and not from the
    /// `//`-comment at [`SYNTHETIC_RESULT_COLUMNS`].
    const WRITE_BUILDERS_EMITTING_RETURNING: usize = 12;

    /// A schema whose masked field makes the raw column REACHABLE by `*`.
    ///
    /// `ssn` is masked, so the physical table has `ssn` (the mask) AND
    /// `__zs_raw__ssn` (the real value). `RETURNING *` expands to both. This is
    /// the fixture that can tell a projection from a star; a schema with no
    /// masked field cannot, because there every physical column is also a
    /// logical one.
    fn write_projection_schema() -> Value {
        l26_masked_schema()
    }

    #[test]
    fn every_write_builder_projects_named_columns_and_never_a_star() {
        let schema = write_projection_schema();
        let queries = every_write_query(&schema);
        assert_eq!(
            queries.len(),
            WRITE_BUILDERS_EMITTING_RETURNING,
            "this test must rule on every RETURNING-emitting builder in the file",
        );
        for (verb, q) in &queries {
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
            for field in SYSTEM_FIELD_NAMES {
                assert!(
                    q.sql.contains(&quote_ident(field)),
                    "{verb} must name the system field {field}: {}",
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
        assert_eq!(queries.len(), WRITE_BUILDERS_EMITTING_RETURNING);
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
        let schema = serde_json::json!({
            "public_note": { "type": "string", "readable": true },
            "internal_note": { "type": "string", "readable": false },
        });
        let queries = every_write_query(&schema);
        assert_eq!(queries.len(), WRITE_BUILDERS_EMITTING_RETURNING);
        for (verb, q) in &queries {
            assert!(
                q.sql.contains(r#""public_note""#),
                "{verb} must project the readable field: {}",
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
        let doc = serde_json::json!({ "ssn": "1" });
        let conflict = serde_json::json!(["id"]);
        let upsert = build_upsert_with_dialect(
            "app1",
            "users",
            &schema,
            &doc,
            &conflict,
            SqlDialect::Postgres,
        )
        .unwrap();
        let foc = build_find_or_create("app1", "users", &schema, &doc, &conflict).unwrap();

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
        let schema = serde_json::json!({
            "amount": { "type": "number", "storage": { "valueColumn": "amount_v2" } },
        });
        let queries = every_write_query(&schema);
        assert_eq!(queries.len(), WRITE_BUILDERS_EMITTING_RETURNING);
        for (verb, q) in &queries {
            assert!(
                q.sql.contains(r#""amount_v2" AS "amount""#),
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
        let doc = serde_json::json!({ "ssn": "1" });
        let conflict = serde_json::json!(["id"]);
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let q =
                build_upsert_with_dialect("app1", "users", &schema, &doc, &conflict, dialect)
                    .unwrap();
            assert!(
                q.sql
                    .contains(r#""version" = COALESCE("app1"."users"."version", 0) + 1"#),
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
        let doc = serde_json::json!({ "ssn": "1" });
        let err =
            build_insert_with_dialect("app1", "users", &Value::Null, &doc, SqlDialect::Postgres)
                .expect_err("a schema-less write must be refused");
        assert!(
            matches!(err, QueryError::InvalidFilter(_)),
            "expected the projection's own refusal, got {err:?}",
        );
    }
}
