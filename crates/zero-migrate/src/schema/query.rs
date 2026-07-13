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
    /// Distinct from [`InvalidIdent`] so the SDK can surface a typed code
    /// (`reserved_system_field_name`) that's distinguishable from the
    /// generic `invalid_identifier` thrown by the `_*` / `__zero_migrate_*` prefix
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

// The SQL deploy-target enum is a pure-data wire descriptor and lives in the
// leaf `zero-migrate-ir` contract (so `zero-migrate-guard`, which is below the
// engine, can name it without depending on the engine). Re-exported here so the
// dialect-specific spelling machinery below — and every `crate::schema::query::
// SqlDialect` caller — keeps a stable path.
pub use zero_migrate_ir::dialect::SqlDialect;

/// Dialect-specific schema/DDL spelling.
///
/// This trait deliberately has no default methods: adding a third dialect must
/// provide every spelling explicitly. The single exhaustive dispatch match lives
/// in [`renderer`], so a new [`SqlDialect`] variant breaks there at compile time
/// and forces the missing renderer to be wired before the crate can build.
pub trait SchemaRenderer {
    fn dialect(&self) -> SqlDialect;
    fn encrypted_column_bind_placeholder(&self, n: usize) -> String;
    fn wrap_encrypted_param(&self, b64_value: String) -> String;
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

impl SchemaRenderer for PostgresSchemaRenderer {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }

    fn encrypted_column_bind_placeholder(&self, n: usize) -> String {
        format!("decode(${n}, 'base64')::bytea")
    }

    fn wrap_encrypted_param(&self, b64_value: String) -> String {
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
        statements.extend(build_encryption_sentinel_comments(
            app_id, collection, schema,
        ));
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

    fn encrypted_column_bind_placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    fn wrap_encrypted_param(&self, b64_value: String) -> String {
        format!("{SQLITE_ENC_BLOB_PREFIX}{b64_value}")
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
            Some("json") | Some("object") | Some("array") | Some("union") => "TEXT".to_string(),
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

    fn encrypted_column_bind_placeholder(&self, _n: usize) -> String {
        "FROM_BASE64(?)".to_string()
    }

    fn wrap_encrypted_param(&self, b64_value: String) -> String {
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
        format!(
            "{}.{}",
            mysql_quote_ident(app_id),
            mysql_quote_ident(target)
        )
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
        assert_eq!(
            renderer(SqlDialect::Postgres).dialect(),
            SqlDialect::Postgres
        );
        assert_eq!(renderer(SqlDialect::Sqlite).dialect(), SqlDialect::Sqlite);
        assert_eq!(renderer(SqlDialect::Mysql).dialect(), SqlDialect::Mysql);
    }
}

/// Sentinel prefix the SQLite session uses to recognise an encrypted-
/// column param that must be base64-decoded and bound as BLOB. The
/// prefix is deliberately long + improbable: a base64 payload contains
/// only `[A-Za-z0-9+/=]`, never `_` or `:`, so this prefix can never
/// collide with a real base64 value the encryption pass writes.
///
/// The SQLite session strips the prefix and base64-decodes the
/// remainder; PG never sees a value with this prefix because the PG
/// renderer's `wrap_encrypted_param` is a no-op on the PG arm.
pub const SQLITE_ENC_BLOB_PREFIX: &str = "__zero_migrate_enc_blob__:";

/// Validate a collection name: alphanumeric + underscores only.
///
/// Additional security constraints (beyond character allowlist):
/// - Must not be empty.
/// - Must not exceed 63 bytes (Postgres `NAMEDATALEN` limit).
/// - Must not contain a null byte.
/// - Must not start with `pg_` (case-insensitive) — reserved for Postgres
///   system catalogs.
/// - Must not start with `__zero_migrate` (case-insensitive) — reserved for the
///   platform's own internal tables (e.g. `__zero_migrate_migrations`).
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
    // .to_ascii_lowercase() per CRUD dispatch (performance r4 N4-I4).
    let bytes = name.as_bytes();
    if bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"pg_") {
        return Err(QueryError::InvalidCollection(format!(
            "collection name '{name}' uses reserved prefix 'pg_' (Postgres system catalog)"
        )));
    }
    if bytes.len() >= 14 && bytes[..14].eq_ignore_ascii_case(b"__zero_migrate") {
        return Err(QueryError::InvalidCollection(format!(
            "collection name '{name}' uses reserved prefix '__zero_migrate' (platform internal)"
        )));
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
/// by the platform's `.mask()` / `.encrypted()` machinery (Path B).
/// The six default classifications
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
    // Synthetic-result columns the runtime emits (e.g. `_rank`,
    // `_score` on FTS / vector search). Reserved so creator-declared
    // columns can't shadow them.
    ReservedName::Prefix("_"),
    // Platform bookkeeping table prefixes. Mirrors the
    // `validate_collection` reservations for table-name shape.
    ReservedName::Prefix("__zero_migrate_"),
    ReservedName::Prefix("__zero_migrate_"),
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
/// the db SDK types: the SDK throws at `pnpm dev` build time, but
/// a hand-built wire payload (a raw `default = { fetch }` deploy calling
/// the `db.registerModel` op directly) skips the SDK entirely, so the
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
// Production paths (`exec_register_model_with_pool`) always pass the
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
/// - [`SqliteEmitScope::MainUnqualified`] — the **zero-migrate engine**'s
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
    /// SQLite DDL is `"<app_id>"`-qualified (the data-plane ATTACH-alias model).
    /// The default for the stable dialected entry point.
    AttachAlias,
    /// SQLite DDL is UNqualified — `main` IS the app file (the zero-migrate
    /// `SqliteBackend` model). The schema qualifier is dropped on the table
    /// name and the index name.
    MainUnqualified,
}

// pub (not pub): external consumer tests/integration.rs calls this via glob import.
//
// PG-flavoured shim around
// [`build_create_table_with_fks_for_dialect`]. Every existing call site
// (orchestrator `register_model::plan`, integration tests, internal
// query helpers) stays on this signature; the dialect-aware emitter
// lives behind the new symbol and routes the SQLite arm independently.
pub fn build_create_table_with_fks(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    fk_emit: &FkEmission<'_>,
) -> Result<String, QueryError> {
    build_create_table_with_fks_for_dialect(
        app_id,
        collection,
        schema,
        fk_emit,
        SqlDialect::Postgres,
    )
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
///   `/* zero-migrate:mask:... */` comment on the sibling column is the
///   SQLite-side wire).
///
/// The `id TEXT PRIMARY KEY` is identical on both backends. The FK
/// column type cascades to `TEXT` so ref columns match the new
/// PK type — see [`def_to_pg_type`].
pub fn build_create_table_with_fks_for_dialect(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    fk_emit: &FkEmission<'_>,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    // The stable entry point keeps the historical data-plane namespacing: SQLite
    // DDL is `"<app_id>"`-qualified (the ATTACH-alias model). The zero-migrate
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
/// lower ([`zero_migrate`]) consumes this list so a string-literal column
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
            let col_def = field_to_column_for_dialect(field, def, dialect)?;
            columns.push(col_def);

            // Path B sibling-column emission. When the
            // field carries a `.mask({...})` declaration (or the
            // auto-default mask attached to `t.encrypted(...)` columns)
            // AND the mask kind is NOT `"none"`, emit a sibling
            // `<col>_masked TEXT` column alongside the parent. The sibling is
            // engine-managed: raw inserts may omit it and the runtime mask-write
            // pass fills it when the parent value is written.
            // The sibling stores the pre-computed masked representation
            // (e.g. `"***-**-6789"`) computed at INSERT/UPDATE time by
            // `crud::mask_pass::apply_mask_on_write`. Reads default to
            // the sibling; writes dual-bind both columns atomically.
            //
            // The sibling type is `TEXT` for every mask kind
            // (full / last4 / first4 / email / name / dateYear /
            // dateDecade) — the union of mask outputs is string-shaped.
            // Future BYTEA-shaped masks would extend this with a per-
            // kind type lookup.
            //
            // Explicit `.mask({ kind: "none" })` opt-out → no sibling
            // emission. The decrypt-on-read path continues to serve
            // such columns; the parent column is the only storage site.
            if let Some(sibling_col) = mask_sibling_column_for_field(field, def) {
                // `_masked` suffix is platform-reserved
                // (`validate_field_name`'s `ReservedName::Suffix`
                // forbids creator-declared columns ending in
                // `_masked`); no collision possible.
                //
                // Attach a `/* zero-migrate:mask:kind=…,
                // classification=… */` inline comment to the sibling
                // DDL so the SQLite introspector can recover the mask
                // metadata from `sqlite_master.sql`. PG ignores SQL
                // comments at parse time, so the introspector on the
                // PG arm reads `pg_description` populated by the
                // `COMMENT ON COLUMN` statement emitted alongside the
                // table create (see `mask_sentinel_for_field`).
                let sentinel = mask_sentinel_for_field(def);
                let inline_comment = match &sentinel {
                    Some(s) => format!(" /* {s} */"),
                    None => String::new(),
                };
                columns.push(format!(
                    "{} TEXT{inline_comment}",
                    quote_ident_for_dialect(&sibling_col, dialect)
                ));
            }

            // Append FOREIGN KEY clause when this is a ref. Inline
            // FK clauses live in the same CREATE TABLE statement as the
            // column, after the column definition.
            if def.get("type").and_then(|t| t.as_str()) == Some("ref") {
                let target = def.get("refTarget").and_then(|v| v.as_str()).unwrap_or("");
                if !target.is_empty() {
                    let should_inline = match fk_emit {
                        FkEmission::Inline => true,
                        FkEmission::Deferred(existing) => {
                            target == collection || existing.contains(target)
                        }
                    };
                    if should_inline {
                        if let Ok(fk_clause) = build_fk_clause(app_id, field, def, target, dialect)
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
    // `dialect == Sqlite`; the inline `/* zero-migrate:mask:... */` comment
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
    // `version` is not emitted (`version` bumps on every UPDATE and
    // an index would thrash).
    let system_index_stmts = build_system_field_indexes(app_id, collection, dialect, sqlite_scope);

    let mut statements: Vec<String> = vec![create_table];
    statements.extend(system_index_stmts);

    statements.extend(renderer(dialect).column_comment_statements(app_id, collection, schema));

    Ok(statements)
}

/// Render the `COMMENT ON COLUMN … 'zero-migrate:enc:<mode>:<keyId>:<wraps>'`
/// statements for every `t.encrypted(...)` column in `schema` (PG only). The
/// comment BODY is built by the shared codec
/// ([`crate::schema::mask_codec::build_encryption_sentinel`]) so it is byte-identical to
/// what the migration engine emits and what the runtime parser
/// ([`crate::schema::mask_codec::parse_encryption_sentinel`], via `read_live_schema`)
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
/// table-level form) — matches the convention used for
/// the legacy `id SERIAL PRIMARY KEY` line this replaces. The
/// existing FK-attachment logic (`build_fk_clause`) references
/// the `id` column by name, so the switch from `SERIAL` to `TEXT`
/// is transparent to the FK emitter (the FK column TYPE narrowing
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
/// thrash). See `docs/proposals/platform-system-fields.md` for
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
    let fk_clause = build_fk_clause(app_id, field, def, target, SqlDialect::Postgres)?;
    Ok(format!("ALTER TABLE {table} ADD {fk_clause}"))
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
pub fn normalize_fk_action_for_dialect(s: Option<&str>, dialect: SqlDialect) -> &'static str {
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
    let constraints = def_to_constraints(field, def);

    let mut sql = format!(
        "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {} {}",
        table,
        quote_ident(field),
        pg_type,
        constraints
    )
    .trim()
    .to_string();

    // When the field carries a `.mask({...})`
    // declaration, also emit the sibling `<col>_masked TEXT NULL` ADD
    // COLUMN op and the `COMMENT ON COLUMN` sentinel attachment in the
    // same multi-statement payload. Only the sibling is NULL here
    // (versus NOT NULL on CREATE TABLE) — existing rows would refuse
    // the ALTER if the sibling were NOT NULL; the mask backfill flips it
    // to NOT NULL after every row has its sibling populated.
    //
    // Note: this branch is taken ONLY when the diff classifier emits
    // an `AddColumn` for a fresh top-level field declared with
    // `.mask({...})` — for that case the sibling tags along in the
    // same payload. The separate `MaskBackfill`-paired
    // `AddColumn(<col>_masked)` op the diff classifier emits for the
    // backfill sets `mask_sibling_for` in `details` and the field IS the
    // sibling itself; `mask_sibling_column_for_field(sibling, def)`
    // returns `None` there because the synthetic def carries no
    // mask block. So we don't double-emit.
    if let Some(sibling) = mask_sibling_column_for_field(field, def) {
        sql.push_str(&format!(
            ";\nALTER TABLE {} ADD COLUMN IF NOT EXISTS {} TEXT NULL",
            table,
            quote_ident(&sibling),
        ));
        if let Some(comment) = build_mask_sentinel_comment_for_field(app_id, collection, field, def)
        {
            sql.push_str(&format!(";\n{comment}"));
        }
    }

    Ok(sql)
}

// ---------------------------------------------------------------------------
// Index builders for registerModel — see the db proposal
// (docs/proposals/db.md). Materialises `t.string().index()` /
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
/// must not be retried — see the proposal's INVALID-index recovery).
///
/// `kind` carries the index *shape* — B-tree (the default for
/// every existing call site), vector (pgvector / Rust flat-scan), full-text
/// (tsvector+GIN on PG, FTS5 on SQLite), or spatial (PostGIS GIST on PG,
/// haversine post-filter on SQLite). The default is [`IndexKind::BTree`]
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
    /// Index shape — selects the backend builder branch. `Vector` /
    /// `Fts` / `Spatial` dispatch through the `register_model::apply` Pass 2.
    pub kind: IndexKind,
}

/// Index shape — the closed sum over the four kinds of indexes
/// `registerModel` can materialise.
///
/// See `docs/proposals/p4-search-implementation-plan.md`.
/// The default is [`IndexKind::BTree`] so every existing call site keeps
/// the same observable behaviour; `Vector` / `Fts` /
/// `Spatial` dispatch through the `register_model::apply` Pass 2.
///
/// **Why an enum, not a string**: same rationale as
/// [`crate::schema::descriptors::VectorMetric`] — the rustc exhaustiveness check
/// trips every match arm if a future change adds a fifth kind, rather
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
        /// Distance metric — see [`crate::schema::descriptors::VectorMetric`].
        metric: crate::schema::descriptors::VectorMetric,
    },
    /// Full-text index. `language` is the tsvector configuration
    /// (`english`, `simple`, …) on PG; SQLite FTS5 ignores it (its
    /// default tokenizer is language-agnostic Unicode).
    Fts {
        /// Tokeniser language. Honoured on PG; ignored on SQLite.
        language: String,
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

    // Accumulate FTS-marked columns into a single composite
    // index per collection. The SDK's
    // `.fts()` per-field modifier sets `def.fts = true; def.ftsLanguage =
    // <lang>` on each text column; we collect those into one
    // `IndexSpec { kind: Fts { language } }` after the per-field loop.
    //
    // **Language**: every `.fts()`-marked column must agree on the
    // language token (a single `__fts` tsvector column can only carry
    // one config). We pick the first non-empty language we see and
    // ignore mismatches at this layer; the SDK is expected to validate
    // language consistency at schema-definition time. If no language is
    // declared the fallback is `english`.
    let mut fts_cols: Vec<String> = Vec::new();
    let mut fts_language: Option<String> = None;

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

        // Collect FTS-marked text columns. A column is
        // FTS-marked when `def.fts === true`; the language defaults to
        // `english` (matches the SDK default in `t.string().fts()`).
        if def.get("fts").and_then(|v| v.as_bool()) == Some(true) {
            fts_cols.push(field.clone());
            if fts_language.is_none() {
                if let Some(lang) = def.get("ftsLanguage").and_then(|v| v.as_str()) {
                    if !lang.is_empty() {
                        fts_language = Some(lang.to_string());
                    }
                }
            }
            // Fall through — an FTS-marked column can also carry an
            // `index: true` or `unique: true` modifier and the user
            // still wants the B-tree alongside the FTS index. The
            // composite FTS index is emitted once after the loop.
        }

        // Vector fields always emit an `IndexKind::Vector`
        // spec regardless of the `index`/`unique` markers; the SDK's
        // `t.vector()` builder doesn't expose those modifiers (they
        // would be meaningless on an ivfflat-indexed column). The
        // builder dispatches to `VectorIndex::ensure_vector_index` in
        // `register_model::apply` Pass 2 — the `sql` field stays empty
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
                "l2" => crate::schema::descriptors::VectorMetric::L2,
                "innerProduct" | "ip" => crate::schema::descriptors::VectorMetric::InnerProduct,
                _ => crate::schema::descriptors::VectorMetric::Cosine,
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
            // the caller-side scope check to refuse
            // randomised+unique earlier; deterministic+unique is OK.
        }

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
                kind: IndexKind::BTree,
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
                kind: IndexKind::BTree,
            });
        }

        // Auto-emit a B-tree index on the sibling
        // `<col>_masked` column when the parent column has `.index()`
        // or `.uniqueIndex()` declared AND the field carries a mask
        // declaration with `kind != "none"`. The sibling index lets reads
        // route equality / sort queries through the masked sibling
        // without a sequential scan. Naming: `<coll>__<col>_masked_idx`
        // (double-underscore separator, matching `named_index_name`'s
        // collision-avoidance convention). Never UNIQUE — uniqueness
        // applies to the parent column only (the sibling is a derived
        // value, multiple rows can share the same masked output).
        if wants_index || wants_unique {
            if let Some(sibling_col) = mask_sibling_column_for_field(field, def) {
                let idx_name = format!("{collection}__{sibling_col}_idx");
                let sql = format!(
                    "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} ({})",
                    quote_ident(&idx_name),
                    table_qualified,
                    quote_ident(&sibling_col),
                );
                out.push(IndexSpec {
                    name: idx_name,
                    columns: vec![sibling_col],
                    unique: false,
                    sql,
                    kind: IndexKind::BTree,
                });
            }
        }
    }

    // Emit a single composite FTS spec covering every
    // `.fts()`-marked column on this collection. The PG impl
    // builds the `__fts tsvector` column + GIN index + trigger; the
    // `sql` field stays empty because the impl builds its own DDL.
    if !fts_cols.is_empty() {
        let language = fts_language.unwrap_or_else(|| "english".to_string());
        let name = format!("{collection}__fts_idx");
        out.push(IndexSpec {
            name,
            columns: fts_cols,
            unique: false,
            sql: String::new(),
            kind: IndexKind::Fts { language },
        });
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
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
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
                QueryError::InvalidIdent(format!("indexes[{i}].fields[{j}] must be a string"))
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
        out.push(IndexSpec {
            name: pg_name,
            columns,
            unique,
            sql,
            kind: IndexKind::BTree,
        });
    }

    Ok(out)
}

/// Construct the Postgres identifier for a named multi-column index.
///
/// Uses a double-underscore separator (`<collection>__<name>`) to avoid
/// collision with the single-underscore auto-named per-field indexes
/// produced by `index_name`. NAMEDATALEN-safe via the same sha256 base32
/// fingerprint tail used by `index_name`.
pub fn named_index_name(collection: &str, name: &str) -> String {
    let full = format!("{collection}__{name}");
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

/// Return the sibling column name `<field>_masked` IFF
/// the field's schema entry carries a `.mask({...})` declaration with
/// `kind != "none"`. Returns `None` for non-masked columns and for
/// columns that explicitly opt out via `.mask({ kind: "none" })`.
///
/// The platform reserves the `_masked` suffix at the field-name level
/// (`validate_field_name`'s `ReservedName::Suffix`) so a creator cannot
/// shadow a sibling. Called by both `build_create_table_with_fks`
/// (DDL emission) and `build_insert` / `build_set_clauses` (atomic
/// dual-write).
pub fn mask_sibling_column_for_field(field: &str, def: &serde_json::Value) -> Option<String> {
    let mask_meta = def.get("mask").and_then(|v| v.as_object())?;
    let kind = mask_meta
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("full");
    if kind == "none" {
        return None;
    }
    Some(format!("{field}_masked"))
}

/// Render the canonical mask-sentinel comment payload
/// for a field's `.mask({...})` declaration, IFF the declaration is
/// present AND `kind != "none"`. Returns `None` when there's no
/// sibling to attach a sentinel to.
///
/// Reused by both backend introspectors (PG `COMMENT ON COLUMN` write
/// + SQLite inline-comment parse on read) — keeps the wire shape
/// consistent. The parser side lives in
/// [`crate::schema::mask_codec::parse_mask_sentinel`].
pub fn mask_sentinel_for_field(def: &serde_json::Value) -> Option<String> {
    let mask_meta = def.get("mask").and_then(|v| v.as_object())?;
    let kind_str = mask_meta
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("full");
    if kind_str == "none" {
        return None;
    }
    let kind = crate::schema::diff::MaskKind::from_sql(kind_str)?;
    let class_str = mask_meta
        .get("classification")
        .and_then(|v| v.as_str())
        .unwrap_or("pii");
    let classification = crate::schema::diff::Classification::from_sql(class_str)?;
    Some(crate::schema::mask_codec::build_mask_sentinel(
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
/// `/* zero-migrate:mask:... */` comment emitted by `build_create_table_with_fks`,
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
        let Some(sibling) = mask_sibling_column_for_field(field, def) else {
            continue;
        };
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
            quote_ident(&sibling),
            escaped,
        ));
    }
    out
}

/// Render the `COMMENT ON COLUMN` statement for one
/// masked field, IFF the field has a `.mask({...})` declaration
/// (`kind != "none"`). Used by the diff classifier's `MaskBackfill`
/// op to attach the sentinel at the same time as the
/// `ALTER TABLE ADD COLUMN <col>_masked` op.
///
/// Returns `None` for fields without a mask or with `kind: "none"` —
/// no sibling, no sentinel.
#[must_use]
pub fn build_mask_sentinel_comment_for_field(
    app_id: &str,
    collection: &str,
    field: &str,
    def: &serde_json::Value,
) -> Option<String> {
    let sibling = mask_sibling_column_for_field(field, def)?;
    let sentinel = mask_sentinel_for_field(def)?;
    let escaped = sentinel.replace('\'', "''");
    Some(format!(
        "COMMENT ON COLUMN {}.{}.{} IS '{}'",
        quote_ident(app_id),
        quote_ident(collection),
        quote_ident(&sibling),
        escaped,
    ))
}

/// Render the inline `/* zero-migrate:enc:{mode}:{keyId}:{wraps} */`
/// encryption sentinel for a field's `t.encrypted({...})` declaration, IFF the
/// field carries an `encrypted` sub-object. Returns `None` for a plain column.
///
/// This is the SINGLE source of truth for the `zero-migrate:enc` wire shape — both
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

/// The bare `zero-migrate:enc:<mode>:<keyId>:<wraps>` sentinel BODY for a field's
/// `t.encrypted({...})` declaration (no `/* */` wrapper, no comment statement),
/// or `None` for a plain column. The SINGLE source of truth for the `zero-migrate:enc` wire
/// grammar: [`encryption_sentinel_for_field`] wraps it in `/* */` for the inline
/// DDL form, and [`build_encryption_sentinel_comments`] wraps it in a
/// `COMMENT ON COLUMN … '…'` statement for the PG-recoverable form. The runtime
/// parser is [`crate::schema::mask_codec::parse_encryption_sentinel`].
#[must_use]
pub fn encryption_sentinel_body_for_field(def: &serde_json::Value) -> Option<String> {
    let enc = def.get("encrypted").and_then(|v| v.as_object())?;
    let mode = enc
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("randomised");
    // Normalise legacy `"randomized"` (US spelling) to the canonical
    // `randomised` so the introspector parser (which accepts both but the
    // emit side normalises to one) round-trips cleanly.
    let mode_norm = if mode == "randomized" {
        "randomised"
    } else {
        mode
    };
    let key_id = enc
        .get("keyId")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let wraps = enc
        .get("wraps")
        .and_then(|v| v.as_str())
        .unwrap_or("string");
    Some(format!("zero-migrate:enc:{mode_norm}:{key_id}:{wraps}"))
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
    // `t.encrypted(...)`-declared columns always store the
    // ciphertext wire blob (`[version_flag | nonce | ct+tag]`) as BYTEA
    // regardless of `wraps`. The encryption pass swaps the plaintext
    // out before the INSERT/UPDATE, and the SQL builder casts the
    // base64 parameter back to BYTEA via `decode($N, 'base64')::bytea`.
    //
    // Emit a `/* zero-migrate:enc:{mode}:{keyId}:{wraps} */` sentinel
    // comment alongside the column type so the SQLite-arm introspector
    // can regex-recover the encryption metadata from `sqlite_master.sql`.
    // PG ignores SQL comments at parse time (the type is still BYTEA);
    // SQLite stores the original CREATE TABLE text verbatim. SQLite's
    // type affinity treats "BYTEA" as NUMERIC (no INT/CHAR/TEXT/BLOB/
    // FLOA/REAL/DOUB substring match), which still accepts BLOB values
    // — same column shape both engines see byte-identical inserts.
    // Sentinel-on-DDL is the same regex-on-DDL pattern used for
    // vector dims; sidecar `__zero_migrate_schema_meta` is the upgrade path
    // (deferred). See
    // `docs/proposals/p5-encryption-backup-implementation-plan.md`.
    let enc_comment_owned;
    let enc_comment: &str = if let Some(body) = encryption_sentinel_for_field(def) {
        enc_comment_owned = format!(" {body}");
        &enc_comment_owned
    } else {
        ""
    };
    let sql_type = def_to_column_type_for_dialect(def, dialect);
    let constraints = def_to_constraints_for_dialect(field, def, dialect);
    // The sentinel comment (when present) sits between the type and the
    // constraints so the parsed shape is `"<col>" BYTEA /* zero-migrate:enc:... */
    // <constraints>`. PG ignores the comment; SQLite preserves it in
    // `sqlite_master.sql` for the introspector regex.
    Ok(format!(
        "{} {}{} {}",
        quote_ident_for_dialect(field, dialect),
        sql_type,
        enc_comment,
        constraints
    )
    .trim()
    .to_string())
}

/// Map a single SDK field definition (`{ type, encrypted?, vectorDims?, … }`)
/// to the column SQL TYPE for `dialect`, covering the FULL type surface —
/// `vector(N)`, `geography(POINT,4326)` (geoPoint), `BYTEA`/`BLOB`
/// (encrypted), `literal`'s primitive, and the plain B-tree types. This is
/// the single source of truth the migration engine's declarative differ
/// adopts (schema-authority): the engine builds a `def` from its
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
/// discriminated union. The discriminator field carries
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
    let disc_primitive = disc_def
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("string");

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
        let constraint_name = union_check_constraint_name(collection, disc_field, &sanitized_tag);

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
/// (≤ 63 bytes) via the same hash-truncation strategy as index names.
fn union_check_constraint_name(collection: &str, disc: &str, value_tag: &str) -> String {
    let full = format!("{collection}_{disc}_{value_tag}_chk");
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

/// Map schema type to PostgreSQL type.
///
/// `ref` columns emit `TEXT` so they match the
/// `id TEXT PRIMARY KEY` of the system-field DDL. The parent PK is
/// `TEXT` (typed_id wire format), so an
/// `INTEGER` FK would fail with `column type mismatch` at FK-constraint
/// creation time on Postgres. SQLite tolerates type mismatch (declared
/// types are advisory) but the typed_id values inserted into a ref
/// column are TEXT-shaped strings, so the storage class is TEXT either
/// way.
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
        // `t.calendarDate()` is a `YYYY-MM-DD` value with no time
        // and no timezone, distinct from `t.date()` (TIMESTAMPTZ stored
        // as Unix-ms numbers at the SDK layer).
        Some("calendarDate") => "DATE",
        Some("json") => "JSONB",
        // `t.object({...})` declares a JSONB column. The nested
        // shape is enforced application-side by `validate.ts`; no
        // CHECK constraint is emitted (Postgres JSONB CHECKs are
        // expressible but expensive at write time).
        Some("object") => "JSONB",
        Some("array") => "JSONB",
        Some("textArray") => "text[]",
        // Cascades to TEXT so FK column type matches the
        // `id TEXT PRIMARY KEY`. See doc-comment on
        // [`def_to_pg_type`] for the rationale.
        Some("ref") => "TEXT",
        Some("inet") => "INET",
        // A top-level `t.union(...)` is flattened to discrete
        // columns by the SDK before it reaches the DDL emitter, so this
        // path should never fire for the discriminator column itself
        // (it has the discriminator's primitive type, not "union").
        // A *nested* `t.union(...)` (inside `t.object`) falls through
        // to JSONB storage; per-variant integrity is application-side.
        Some("union") => "JSONB",
        // A top-level `t.literal()` field outside a union would
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
        "text"
        | "text[]"
        | "jsonb"
        | "json"
        | "timestamp with time zone"
        | "timestamptz"
        | "date"
        | "inet"
        | "character"
        | "char"
        | "bpchar" => "text",
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
        || matches!(
            no_width.as_str(),
            "timestamp with time zone" | "timestamptz"
        )
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
    for ty in [
        "tinyint",
        "smallint",
        "mediumint",
        "int",
        "integer",
        "bigint",
    ] {
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
    // here. The proposal (db.md A1) mandates that every uniqueness
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
    if let (Some("number"), Some(min)) = (
        def.get("type").and_then(|t| t.as_str()),
        def.get("min").and_then(|v| v.as_f64()),
    ) {
        if let Some(max) = def.get("max").and_then(|v| v.as_f64()) {
            parts.push(format!("CHECK ({col} >= {min} AND {col} <= {max})"));
        } else {
            parts.push(format!("CHECK ({col} >= {min})"));
        }
    } else if let (Some("number"), Some(max)) = (
        def.get("type").and_then(|t| t.as_str()),
        def.get("max").and_then(|v| v.as_f64()),
    ) {
        parts.push(format!("CHECK ({col} <= {max})"));
    }

    // Standalone literal field. The value's primitive type is
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // SEC-4 — aggregation pipeline must NOT leak masked-column plaintext.
    //
    // For a mask-only column (`.mask({...})` without `.encrypted()`),
    // plaintext lives in `<col>` and the masked string in `<col>_masked`.
    // a bare `quote_ident(field)` against the base plaintext column, so
    // `$group.by:"ssn"` / `$max:"ssn"` returned PLAINTEXT. These pin the
    // sibling substitution at the SQL-builder level (BASE column, not the
    // already-rejected `ssn_masked` sibling name).
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // 1. Missing builder tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // 2. Filter edge cases
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // 3. SQL injection prevention
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // 5. Aggregate edge cases
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // 6. Error cases
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // 7. $first sort-order threading
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Update-operator regression tests
    //
    // These lock in the fixes from 6a309b3 ("resolve 7 native-layer bugs"):
    // type preservation on jsonb array ops, value-based $pull, $set flattening,
    // and updated_at auto-injection.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // UPDATE auto-bumps version + updated_at + updated_by
    //
    // The auto-bumps fire only on the new dispatch path (signalled by
    // an `actor_id` being threaded through OR by `skip_*` hints).
    // continue to see the single-column auto-bump
    // (`updated_at = NOW()` on PG) so the regression tests above stay
    // green.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Materialised indexes (db proposal).
    //
    // Previously, `t.string().index()` and `t.string().unique()` set
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
            spec.sql
                .starts_with("CREATE INDEX CONCURRENTLY IF NOT EXISTS"),
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
        assert_eq!(
            index_name("posts", &["author_id"], false),
            "posts_author_id_idx"
        );
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
            "name {n1} exceeds Postgres NAMEDATALEN limit of 63"
        );
        // Hash is 8 base32 chars at the tail.
        let tail = &n1[n1.len() - 8..];
        assert!(
            tail.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "tail '{tail}' should be base32"
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
        assert_eq!(name.len(), 60, "name: {name}");
        assert!(
            name.ends_with("_idx"),
            "should keep readable suffix: {name}"
        );
    }

    #[test]
    fn test_index_name_just_over_threshold_is_hashed() {
        let col = "c".repeat(55);
        let name = index_name("t", &[col.as_str()], false);
        assert!(name.len() <= 63);
        assert!(
            !name.ends_with("_idx"),
            "over-threshold name should end with the hash, not _idx: {name}"
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
        assert_eq!(
            out.len(),
            1,
            "should emit a unique index for `unique: true`"
        );
        let spec = &out[0];
        assert!(spec.unique, "must be marked as unique");
        // Statement shape — the four invariants the proposal calls out:
        //   * CREATE UNIQUE INDEX (so duplicates are actually rejected)
        //   * CONCURRENTLY        (so writes are never blocked on build)
        //   * IF NOT EXISTS       (so re-runs are idempotent)
        //   * targets ("email")   (the column the marker is on)
        assert!(
            spec.sql.contains("CREATE UNIQUE INDEX"),
            "sql: {}",
            spec.sql
        );
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
        let create =
            build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline).unwrap();
        assert!(
            create.contains("NOT NULL"),
            "still emits NOT NULL: {create}"
        );
        assert!(
            !create.contains(" UNIQUE"),
            "CREATE TABLE must not emit inline UNIQUE (would force non-concurrent index): {create}"
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
            "ADD COLUMN must not emit inline UNIQUE: {alter}"
        );
    }

    #[test]
    fn p7_id_prefix_decl_emits_single_id_column() {
        // **P7** — `id: t.id("blog")` is a prefix declaration for the
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
        let err =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap_err();
        assert!(
            matches!(err, QueryError::ReservedSystemFieldName(_)),
            "usr prefix must be rejected as reserved, got {err:?}"
        );
    }

    #[test]
    fn p7_id_prefix_decl_with_malformed_prefix_is_rejected() {
        let schema = json!({ "id": {"type": "id", "idPrefix": "1bad"} });
        let err =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap_err();
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
        let err =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap_err();
        assert!(
            matches!(err, QueryError::ReservedSystemFieldName(_)),
            "id with non-id type must stay rejected, got {err:?}"
        );
    }

    // -----------------------------------------------------------------
    // typed cross-table relations
    // -----------------------------------------------------------------

    #[test]
    fn b2_create_table_with_ref_emits_inline_fk() {
        let schema = json!({
            "title": {"type": "string", "required": true},
            "authorId": {"type": "ref", "refTarget": "users"},
        });
        let sql =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        // TEXT column for the FK (cascades to match the
        // `id TEXT PRIMARY KEY`).
        assert!(sql.contains("\"authorId\" TEXT"), "{sql}");
        // Inline FK clause with SQL/Postgres defaults omitted.
        assert!(sql.contains("FOREIGN KEY (\"authorId\")"), "{sql}");
        assert!(sql.contains("REFERENCES \"app1\".\"users\" (id)"), "{sql}");
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
        let sql =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
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
        let sql =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
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
        let sql =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
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
        assert!(
            sql.starts_with("ALTER TABLE \"app1\".\"posts\" ADD"),
            "{sql}"
        );
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
        let sql =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Deferred(&existing))
                .unwrap();
        // FK is deferred — column still present but no FOREIGN KEY clause.
        // TEXT (cascades to match the `id TEXT PRIMARY KEY`).
        assert!(sql.contains("\"authorId\" TEXT"), "{sql}");
        assert!(!sql.contains("FOREIGN KEY"), "FK should be deferred: {sql}");
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
    // FK column type cascade (TEXT)
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
            "ref column type must cascade to TEXT to match the id TEXT PRIMARY KEY"
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
        // check on the whole sql is safe — the substring
        // `"authorId" INTEGER` was present.)
        assert!(
            !sql.contains("\"authorId\" INTEGER"),
            "ref column must not emit INTEGER (TEXT cascade): {sql}"
        );
    }

    // -----------------------------------------------------------------
    // nested object validators (JSONB column)
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
        let sql =
            build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline).unwrap();
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
    // calendar dates → DATE column type
    // -----------------------------------------------------------------

    #[test]
    fn d3_calendar_date_emits_date_column() {
        let schema = json!({
            "birthday": { "type": "calendarDate" },
        });
        let sql =
            build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline).unwrap();
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
        let sql =
            build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline).unwrap();
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
        assert!(
            sql.contains("ADD COLUMN IF NOT EXISTS \"birthday\" DATE"),
            "{sql}"
        );
    }

    // -----------------------------------------------------------------
    // version column injected by the SDK is treated as a plain
    // INTEGER (well, NUMERIC) column at the DDL level. The SDK uses
    // model.ts to inject `version: { type: "number", default: 1 }`
    // so the DDL emission below matches.
    //
    // `version` is a reserved system-field name
    // (`SYSTEM_FIELD_NAMES`); the declaration-time validator refuses
    // a creator-declared `version` column. `build_create_table_with_fks`
    // injects the seven system fields directly (not via a creator-shape
    // entry). The test uses a placeholder field
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
        let sql =
            build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        assert!(
            sql.contains("\"schema_revision\" DOUBLE PRECISION"),
            "{sql}"
        );
        assert!(sql.contains("DEFAULT 1"), "{sql}");
    }

    // -----------------------------------------------------------------
    // discriminated union document shapes
    //
    // The SDK normalises `t.union(t.object({...}), t.object({...}))` into
    // a flat schema where each variant's fields are top-level entries
    // and the discriminator column carries a `variants` JSON payload
    // plus `discriminator: "__discriminator__"`. The DDL emitter
    // converts that into:
    //   - TEXT/NUMERIC/BOOLEAN column for the discriminator with
    //     `CHECK (col IN (...))` (via the regular enum constraint)
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
    fn c2_union_emits_per_variant_check_constraints() {
        // Each variant gets a CHECK constraint of the
        // form: `kind <> 'login' OR (userId IS NOT NULL AND ip IS NOT NULL)`.
        let schema = c2_events_union_schema();
        let sql =
            build_create_table_with_fks("app1", "events", &schema, &FkEmission::Inline).unwrap();

        // The login variant requires userId AND ip.
        assert!(
            sql.contains("\"kind\" <> 'login' OR (\"userId\" IS NOT NULL AND \"ip\" IS NOT NULL)")
                || sql.contains(
                    "\"kind\" <> 'login' OR (\"ip\" IS NOT NULL AND \"userId\" IS NOT NULL)"
                ),
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
            sql.contains(
                "\"kind\" <> 'metric' OR (\"name\" IS NOT NULL AND \"value\" IS NOT NULL)"
            ) || sql.contains(
                "\"kind\" <> 'metric' OR (\"value\" IS NOT NULL AND \"name\" IS NOT NULL)"
            ),
            "missing metric variant CHECK: {sql}"
        );
    }

    #[test]
    fn c2_union_constraint_names_are_unique_per_variant() {
        let schema = c2_events_union_schema();
        let sql =
            build_create_table_with_fks("app1", "events", &schema, &FkEmission::Inline).unwrap();
        // Each variant constraint name follows `<table>_<disc>_<value>_chk`.
        assert!(
            sql.contains("CONSTRAINT \"events_kind_login_chk\""),
            "{sql}"
        );
        assert!(
            sql.contains("CONSTRAINT \"events_kind_error_chk\""),
            "{sql}"
        );
        assert!(
            sql.contains("CONSTRAINT \"events_kind_metric_chk\""),
            "{sql}"
        );
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
        assert!(
            sql.contains("\"code\" <> 1 OR (\"a\" IS NOT NULL)"),
            "{sql}"
        );
        assert!(
            sql.contains("\"code\" <> 2 OR (\"b\" IS NOT NULL)"),
            "{sql}"
        );
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
        assert!(
            sql.contains("CREATE TABLE IF NOT EXISTS `app1`.`apps`"),
            "{sql}"
        );
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
        let sql =
            build_create_table_with_fks("app1", "events", &schema, &FkEmission::Inline).unwrap();
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
        assert!(
            sql.contains("CONSTRAINT \"evt_kind_page_view_chk\""),
            "{sql}"
        );
        assert!(
            sql.contains("CONSTRAINT \"evt_kind_click_out_chk\""),
            "{sql}"
        );
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

    /// Field names with non-ASCII characters must be rejected (closes
    /// [I12] / test-coverage GAP-1 / security MINOR). A multi-byte
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
    /// accepted but the `_` prefix is now reserved for synthetic-
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
    // reserved-name validator
    // -----------------------------------------------------------------

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

    /// (`_rank`, `_distance`, `_score`) emitted by FTS / vector /
    /// spatial native paths.
    #[test]
    fn validate_field_name_rejects_reserved_underscore_prefix() {
        for name in &["_rank", "_distance", "_score", "_anything"] {
            let err = validate_field_name(name).unwrap_err();
            assert!(
                matches!(err, QueryError::InvalidIdent(_)),
                "expected InvalidIdent for {name:?}, got {err:?}"
            );
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
        assert!(!is_schema_metadata_key("_rank"));
        assert!(!is_schema_metadata_key("ssn"));
    }

    /// CREATE TABLE on a schema containing only `_meta` produces a
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
        assert!(
            !sql.contains("\"_meta\""),
            "_meta must NOT be emitted as a column: {sql}"
        );
    }

    // -----------------------------------------------------------------
    // platform system-field reservation (declaration-only)
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
                other => panic!("expected ReservedSystemFieldName for {name:?}, got {other:?}"),
            }
        }
    }

    /// Filter-time validation (`validate_field_name`) MUST continue to
    /// accept all 7 system field names. `db.users.find({ id: "..." })`
    /// is the canonical query shape — fencing `id` at filter time would
    /// break the entire SDK. The reservation is declaration-only.
    #[test]
    fn system_field_names_allowed_in_filter_path() {
        for name in SYSTEM_FIELD_NAMES {
            assert!(
                validate_field_name(name).is_ok(),
                "system field {name:?} must be accepted by the filter-time validator"
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
    // `code = "reserved_system_field_name"` — was relocated to the data plane's
    // `error.rs` test module as part of the schema-authority extraction.
    // `DbError` lives in the data plane (it is built on a runtime `OpError`)
    // and cannot be named from this schema layer. The validator
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
            schema_obj.insert((*name).to_string(), serde_json::json!({ "type": "string" }));
            let schema = serde_json::Value::Object(schema_obj);
            let err = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline)
                .unwrap_err();
            match err {
                QueryError::ReservedSystemFieldName(msg) => {
                    assert!(
                        msg.contains(name),
                        "CREATE TABLE must refuse system-field {name:?}; got: {msg}"
                    );
                }
                other => panic!("expected ReservedSystemFieldName for {name:?}, got {other:?}"),
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
    /// legacy `id SERIAL PRIMARY KEY` previously emitted.
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

    /// `version` is bumped on every UPDATE (the auto-bump wiring);
    /// an index on it would thrash, so it stays unindexed.
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

    /// FK emission on a user-declared `ref` field continues to work
    /// alongside the system-field prefix. Pins the structural invariant
    /// that FK clauses ride after the column declarations.
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
    /// `CREATE INDEX "<schema>"."<idx>" ON "<table>" (...)`. Per the
    /// sqlite ATTACH alias correction.
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
        let expected_prefix = format!("CREATE INDEX IF NOT EXISTS \"app1\".\"{idx}\" ON \"posts\"");
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
        let expected_prefix = format!("CREATE INDEX IF NOT EXISTS \"{idx}\" ON \"app1\".\"posts\"");
        assert!(
            sql.contains(&expected_prefix),
            "PG index DDL must use ON <schema>.<table> form ({expected_prefix}): {sql}"
        );
    }

    /// The system-field index names go through the existing
    /// [`index_name`] helper, so an overlong collection name gets the
    /// sha2 hash truncation at 60 bytes. Regression fence for the
    /// NAMEDATALEN-safety contract.
    #[test]
    fn index_name_truncates_with_sha2_suffix_at_60_bytes() {
        // 63-byte collection name (the Postgres NAMEDATALEN ceiling).
        // The naive `<table>_deleted_at_idx` would be far over 60 bytes,
        // triggering the hash-truncation path.
        let long = "a".repeat(63);
        let idx = index_name(&long, &["deleted_at"], false);
        assert!(
            idx.len() <= 60,
            "truncated index name must fit NAMEDATALEN ({} bytes): {idx}",
            idx.len()
        );
        // The 8-char base32 suffix is the hash tail.
        let tail = &idx[idx.len() - 8..];
        for b in tail.bytes() {
            assert!(
                b.is_ascii_lowercase() || b.is_ascii_digit(),
                "hash suffix must be base32-lowercase + digits: {tail}"
            );
        }
    }

    /// The debug_assert at the end of `build_create_table_with_fks_for_dialect`
    /// is the last line of defence: under debug builds it panics if two
    /// declarations end up referencing the same system-field name in
    /// the column list. The declaration-time validator catches creator-declared
    /// system fields before this point — so this test exercises the
    /// assertion's *unreachable* path under a hand-rolled internal
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
    /// length cap, null bytes, the `_*` / `__zero_migrate_*` / `_masked` reserved
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
            validate_field_name_for_declaration("_rank").unwrap_err(),
            QueryError::InvalidIdent(_)
        ));
    }

    // -----------------------------------------------------------------
    // Path B sibling-column DDL emission
    // -----------------------------------------------------------------

    /// `mask_sibling_column_for_field` returns `Some("<col>_masked")`
    #[test]
    fn mask_sibling_column_for_field_returns_sibling_for_masked() {
        let def = serde_json::json!({
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        });
        assert_eq!(
            mask_sibling_column_for_field("ssn", &def),
            Some("ssn_masked".to_string())
        );
    }

    #[test]
    fn mask_sibling_column_for_field_returns_none_for_unmasked() {
        let def = serde_json::json!({ "type": "string" });
        assert_eq!(mask_sibling_column_for_field("name", &def), None);
    }

    #[test]
    fn mask_sibling_column_for_field_returns_none_for_kind_none() {
        let def = serde_json::json!({
            "type": "string",
            "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
            "mask": { "kind": "none", "classification": "spi" }
        });
        assert_eq!(mask_sibling_column_for_field("ssn", &def), None);
    }

    /// **DDL shape** — masked column emits parent + nullable sibling
    /// `<col>_masked TEXT`.
    #[test]
    fn build_create_table_emits_sibling_for_masked_column() {
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
            sql.contains("\"ssn_masked\" TEXT"),
            "expected sibling column with TEXT: {sql}"
        );
        assert!(
            !sql.contains("\"ssn_masked\" TEXT NOT NULL"),
            "masked sibling must be nullable / omittable: {sql}"
        );
        assert!(
            sql.contains("\"ssn\""),
            "parent column still present: {sql}"
        );
        assert!(
            !sql.contains("\"name_masked\""),
            "non-masked column must NOT emit sibling: {sql}"
        );
    }

    /// Masked column CREATE TABLE emits `COMMENT ON
    /// COLUMN` for the sibling so PG introspection round-trips the
    /// mask metadata via `pg_description`.
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
            sql.contains("COMMENT ON COLUMN \"app1\".\"users\".\"ssn_masked\""),
            "expected COMMENT ON COLUMN for sibling: {sql}"
        );
        assert!(
            sql.contains("'zero-migrate:mask:kind=last4,classification=spi'"),
            "expected sentinel literal: {sql}"
        );
    }

    /// Sibling DDL inline `/* zero-migrate:mask:... */` comment
    /// for SQLite-arm introspection (PG ignores SQL comments; SQLite
    /// preserves them in `sqlite_master.sql`).
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
            sql.contains(
                "\"email_masked\" TEXT /* zero-migrate:mask:kind=email,classification=pii */"
            ),
            "expected inline /* zero-migrate:mask:... */ comment on sibling: {sql}"
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
            !sql.contains("zero-migrate:mask:"),
            "kind=none must emit no sentinel: {sql}"
        );
    }

    /// `build_add_column` for a fresh field with a
    /// `.mask({...})` declaration emits BOTH the parent ADD + the
    /// sibling ADD + the `COMMENT ON COLUMN` sentinel in one
    /// multi-statement payload.
    #[test]
    fn build_add_column_emits_sibling_and_sentinel_when_masked() {
        let def = serde_json::json!({
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        });
        let sql = build_add_column("app1", "users", "ssn", &def).expect("build_add_column ok");
        assert!(
            sql.contains("ADD COLUMN IF NOT EXISTS \"ssn\""),
            "parent: {sql}"
        );
        assert!(
            sql.contains("ADD COLUMN IF NOT EXISTS \"ssn_masked\""),
            "sibling: {sql}"
        );
        assert!(
            sql.contains("COMMENT ON COLUMN \"app1\".\"users\".\"ssn_masked\""),
            "comment: {sql}"
        );
        assert!(
            sql.contains("'zero-migrate:mask:kind=last4,classification=spi'"),
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

    /// **DDL shape** — `t.encrypted(...)` (default-mask path) gets the
    /// sibling because the SDK auto-populates `mask: {kind: "full", ...}`
    #[test]
    fn build_create_table_emits_sibling_for_encrypted_with_default_mask() {
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
        assert!(
            sql.contains("\"ssn\" BYTEA"),
            "parent encrypted column stays BYTEA: {sql}"
        );
        assert!(
            sql.contains("\"ssn_masked\" TEXT"),
            "encrypted column with default mask emits sibling: {sql}"
        );
        assert!(
            !sql.contains("\"ssn_masked\" TEXT NOT NULL"),
            "encrypted masked sibling must be nullable / omittable: {sql}"
        );
    }

    /// **DDL shape** — `kind: "none"` explicit opt-out → no sibling.
    /// The parent encrypted column behaves like a plain encrypted column.
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

    // -----------------------------------------------------------------
    // SELECT-shape gates
    //
    // Three invariants pinned at the SQL-build layer (the production
    // expr_with_unmask`):
    //
    // 1. `default_read_does_not_touch_ciphertext_column` — when a
    //    schema declares a masked column and no `unmask` hint is
    //    passed, the SELECT clause emits `"<col>_masked" AS "<col>"`
    //    and the bare ciphertext column name MUST NOT appear in the
    //    select-list (it appears in the alias's right-hand side only
    //    and not as a top-level select expression).
    //    refuses filter keys ending in `_masked` because
    //    `validate_field_name` is on the reserved-suffix path.
    //    We double-check the end-to-end path through
    // 3. `sibling_masked_column_not_visible_in_sdk_introspection` —
    //    the SDK `Row<S>` shape excludes `<col>_masked`. The Rust-
    //    side dual to that invariant is that callers never need to
    //    PROJECT through `<col>_masked` — the alias substitution
    //    means the SDK sees `<col>` carrying the masked value.
    //    Asserted by ensuring the build emits the sibling under an
    //    `AS "<col>"` alias and never as a bare top-level identifier.
    // -----------------------------------------------------------------

    // ----------------------------------------------------------------
    // soft-delete / restore SQL builders +
    // compose-where-with-soft-delete behaviour
    // ----------------------------------------------------------------

    // -----------------------------------------------------------------------
    // `SqliteEmitScope` namespacing (descriptor→DDL for the migrate
    // engine). The `MainUnqualified` scope drops the `<app_id>` qualifier on
    // the SQLite arm so the DDL lands in `main` (= the app file). PG and the
    // `AttachAlias` SQLite default are unchanged (regression guard).
    // -----------------------------------------------------------------------

    /// A descriptor carrying a masked column, an encrypted column, and an FK —
    /// the goodies the emitter must round-trip through emit→apply→drift.
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
        for scope in [
            SqliteEmitScope::AttachAlias,
            SqliteEmitScope::MainUnqualified,
        ] {
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

    /// `MainUnqualified` SQLite carries the goodies: the inline `zero-migrate:mask:` mask
    /// sentinel on the `_masked` sibling, the inline `zero-migrate:enc:` encryption
    /// sentinel on the BLOB column, and an unqualified FK clause — so all three
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

        // Mask sentinel rides inline on the `<col>_masked` sibling column.
        assert!(
            sql.contains(r#""ssn_masked" TEXT /* zero-migrate:mask:"#),
            "mask sentinel must ride inline on the sibling: {sql}"
        );
        // Encryption: BLOB physical column + inline `zero-migrate:enc:` sentinel.
        assert!(
            sql.contains("BLOB") && sql.contains("/* zero-migrate:enc:"),
            "encrypted column must be BLOB with an inline zero-migrate:enc sentinel: {sql}"
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
        assert!(
            !sql.contains(r#""app_demo"."#),
            "must stay unqualified: {sql}"
        );
    }
}
