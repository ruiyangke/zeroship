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
    /// Malformed identifier in a structured input (e.g. named index name or
    /// field reference). Carries a path-keyed message so the SDK can surface
    /// it back to the user without losing the offending input.
    InvalidIdent(String),
    /// **P7 PR 1** — creator declared a field whose name collides with one
    /// of the seven platform-managed system fields (`id`, `created_at`,
    /// `updated_at`, `created_by`, `updated_by`, `version`, `deleted_at`).
    /// Distinct from [`InvalidIdent`] so the SDK can surface a typed code
    /// (`reserved_system_field_name`) that's distinguishable from the
    /// generic `invalid_identifier` thrown by the `_*` / `__zs_*` prefix
    /// reservations. Filter-time use of these names is unrestricted
    /// (`db.users.find({ id: ... })` is the canonical query shape); the
    /// fence only fires on declaration paths (`field_to_column`).
    ReservedSystemFieldName(String),
    /// **P7 PR 4** — creator UPDATE patch attempted to overwrite one of
    /// the three write-once system fields (`id`, `created_at`,
    /// `created_by`). These are auto-populated at INSERT (PR 3) and
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

/// **P5 PR 3.5** — SQL dialect tag for the small set of build sites
/// whose encrypted-column placeholder shape diverges between PG and
/// SQLite.
///
/// PG uses `decode($N, 'base64')::bytea` so the BYTEA column receives
/// raw bytes from a base64-encoded text param (the encryption pass
/// writes `Value::String(b64)`). SQLite's text-mode bind layer cannot
/// represent BLOBs through a `&str` param — we emit a plain `$N`
/// placeholder and tag the param value with the
/// [`SQLITE_ENC_BLOB_PREFIX`] sentinel; the SQLite session actor
/// recognises the sentinel and binds raw `Vec<u8>` (BLOB) instead of
/// text. Non-encrypted parameters travel as plain `String` on both
/// arms.
///
/// PG-side behaviour is byte-for-byte identical to PR 2 — the
/// `decode($N, 'base64')::bytea` SQL fragment is unchanged and the
/// sentinel-prefix is never produced on the PG arm. The dialect flag
/// only flips behaviour for `t.encrypted(...)`-declared columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    /// Postgres dialect: encrypted-column binds wrap the placeholder
    /// with `decode($N, 'base64')::bytea` so the BYTEA column receives
    /// raw bytes from the base64 text param. This is the dialect every
    /// PR 2 build site already emits.
    Postgres,
    /// SQLite dialect: encrypted-column binds emit `$N` and the param
    /// value is tagged with the [`SQLITE_ENC_BLOB_PREFIX`] sentinel so
    /// the session actor binds raw bytes (BLOB) instead of text.
    Sqlite,
}

impl SqlDialect {
    /// Build the placeholder SQL fragment for an encrypted-column
    /// parameter at position `n` (1-indexed). PG wraps the placeholder
    /// in a `decode(...)::bytea` cast; SQLite emits a bare `$N`.
    pub fn encrypted_column_bind_placeholder(self, n: usize) -> String {
        match self {
            Self::Postgres => format!("decode(${n}, 'base64')::bytea"),
            Self::Sqlite => format!("${n}"),
        }
    }

    /// Wrap an encrypted-column base64 param value with the
    /// dialect-appropriate side-channel. PG returns the value
    /// unchanged (it is decoded by the SQL fragment from
    /// [`SqlDialect::encrypted_column_bind_placeholder`]); SQLite
    /// prepends [`SQLITE_ENC_BLOB_PREFIX`] so the session actor can
    /// route the param through a binary bind.
    pub fn wrap_encrypted_param(self, b64_value: String) -> String {
        match self {
            Self::Postgres => b64_value,
            Self::Sqlite => format!("{SQLITE_ENC_BLOB_PREFIX}{b64_value}"),
        }
    }
}

/// Sentinel prefix the SQLite session uses to recognise an encrypted-
/// column param that must be base64-decoded and bound as BLOB. The
/// prefix is deliberately long + improbable: a base64 payload contains
/// only `[A-Za-z0-9+/=]`, never `_` or `:`, so this prefix can never
/// collide with a real base64 value the encryption pass writes.
///
/// The SQLite session strips the prefix and base64-decodes the
/// remainder; PG never sees a value with this prefix because
/// [`SqlDialect::wrap_encrypted_param`] is a no-op on the PG arm.
pub const SQLITE_ENC_BLOB_PREFIX: &str = "__zsenc_blob__:";

pub const MAX_QUERY_LIMIT: i64 = 500;
pub const MAX_QUERY_OFFSET: i64 = 10_000;
pub const MAX_SEARCH_LIMIT: usize = 500;
/// DB-11: max documents in a single `insertMany`. Bounds the multi-row SQL
/// string + bound-param vector materialized in the worker (and stays well
/// under Postgres' 65535-bind-param wall). Callers needing more must chunk.
pub const MAX_INSERT_MANY_BATCH: usize = 1_000;
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

/// Validate a collection name: alphanumeric + underscores only.
///
/// Additional security constraints (beyond character allowlist):
/// - Must not be empty.
/// - Must not exceed 63 bytes (Postgres `NAMEDATALEN` limit).
/// - Must not contain a null byte.
/// - Must not start with `pg_` (case-insensitive) — reserved for Postgres
///   system catalogs.
/// - Must not start with `__zeroship` (case-insensitive) — reserved for the
///   platform's own internal tables (e.g. `__zeroship_migrations`).
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
    if bytes.len() >= 10 && bytes[..10].eq_ignore_ascii_case(b"__zeroship") {
        return Err(QueryError::InvalidCollection(format!(
            "collection name '{name}' uses reserved prefix '__zeroship' (platform internal)"
        )));
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

/// **P5.5 PR 1** — true for top-level schema keys that carry
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

/// **P5.5 PR 1** — taxonomy of reserved name shapes the platform
/// enforces on creator-declared field names.
///
/// Three match arms cover the patterns we currently reserve:
/// - `Exact(s)`  — refuse a field named literally `s`.
/// - `Prefix(p)` — refuse any field name starting with `p`.
/// - `Suffix(s)` — refuse any field name ending with `s`.
///
/// The `_masked` suffix is reserved for sibling columns auto-emitted
/// by the platform's `.mask()` / `.encrypted()` machinery (Path B,
/// PR 2 onwards). The six default classifications
/// (`public`/`pii`/`spi`/`phi`/`pci`/`internal`) are reserved as
/// exact names so creator schemas cannot collide with the
/// classification taxonomy used by audit + authorization (PR 4).
pub(crate) enum ReservedName {
    /// Literal name match — refuse a field named exactly `&str`.
    Exact(&'static str),
    /// Prefix match — refuse any field starting with `&str`.
    Prefix(&'static str),
    /// Suffix match — refuse any field ending with `&str`.
    Suffix(&'static str),
}

/// **P7 PR 1** — the seven platform-managed system fields.
///
/// Every creator table receives these at CREATE TABLE time (PR 2 wires
/// that); creators cannot declare their own field with any of these
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
    ReservedName::Prefix("__zs_"),
    ReservedName::Prefix("__zeroship_"),
    ReservedName::Prefix("sqlite_"),
    // **P5.5 PR 1** — masked-column sibling suffix. The platform
    // emits `<col>_masked` siblings (Path B); creators must not
    // declare a column ending in `_masked` themselves. Refused at
    // both schema-registration time (in `field_to_column`) and
    // filter-time (so `db.users.find({ ssn_masked: ... })` is
    // refused with the same code path).
    ReservedName::Suffix("_masked"),
    // **P5.5 PR 1** — six default-classification names. Reserved at
    // the column-name level so creator schemas can't accidentally
    // collide with the classification taxonomy (used by PR 4
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
/// **P5.5 PR 1** — also refuses any field name matching the
/// [`RESERVED_NAMES`] table (platform suffixes / prefixes / exact
/// names). The `_masked` suffix is reserved for Path B sibling
/// columns; the six default-classification names (`public`, `pii`,
/// `spi`, `phi`, `pci`, `internal`) are reserved at the column-name
/// level.
///
/// **P7 PR 1** — note this function does NOT fence the seven
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
    // **P5.5 PR 1** — reserved-name check. Run after the ASCII
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

/// **P7 PR 1** — declaration-time wrapper around [`validate_field_name`]
/// that additionally fences the seven platform-managed system field
/// names ([`SYSTEM_FIELD_NAMES`]).
///
/// Call this from every code path that translates a creator-declared
/// schema field into DDL (currently `field_to_column`). Filter-time
/// validators (`build_field_condition`, `build_vector_search`,
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

/// **P7** — validate a creator-declared typed-id prefix (`t.id("blog")`).
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

fn schema_declares_readable_field(schema_hint: Option<&Value>, name: &str) -> bool {
    schema_hint
        .and_then(Value::as_object)
        .map(|obj| obj.contains_key(name) && !is_schema_metadata_key(name))
        .unwrap_or(false)
}

fn validate_read_identifier(name: &str, schema_hint: Option<&Value>) -> Result<(), QueryError> {
    validate_field_name(name)?;
    if SYSTEM_FIELD_NAMES.contains(&name) || schema_declares_readable_field(schema_hint, name) {
        return Ok(());
    }
    if schema_hint.is_some() {
        return Err(QueryError::InvalidIdent(format!(
            "field '{name}' is not a readable schema field; readable fields are declared schema fields plus the public system fields"
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

// pub (not pub): external consumer tests/integration.rs calls this via glob import.
//
// **P7 PR 2** — PG-flavoured shim around
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
    build_create_table_with_fks_for_dialect(app_id, collection, schema, fk_emit, SqlDialect::Postgres)
}

/// **P7 PR 2** — dialect-aware CREATE TABLE emitter.
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
/// The `id TEXT PRIMARY KEY` is identical on both backends. **P7 PR 3**
/// cascades the FK column type to `TEXT` so ref columns match the new
/// PK type — see [`def_to_pg_type`].
pub fn build_create_table_with_fks_for_dialect(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    fk_emit: &FkEmission<'_>,
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let table = format!("{}.{}", quote_ident(app_id), quote_ident(collection));

    let mut columns = build_system_field_columns(dialect);

    let mut deferred_fks: Vec<String> = Vec::new();
    let mut union_checks: Vec<String> = Vec::new();

    if let Some(obj) = schema.as_object() {
        for (field, def) in obj {
            // **P5.5 PR 1** — skip top-level metadata keys (e.g.
            // `_meta`, `_indexes`). The `_` prefix is reserved for
            // synthetic-result columns at the field-name level
            // (`validate_field_name`), so these keys would otherwise
            // trip the validator; they are CRDT-like top-level
            // schema metadata rather than column declarations.
            if is_schema_metadata_key(field) {
                continue;
            }
            // **P7** — `id: t.id("prefix")` is a PREFIX DECLARATION for
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

            // **P5.5 PR 2** — Path B sibling-column emission. When the
            // field carries a `.mask({...})` declaration (or the
            // auto-default mask attached to `t.encrypted(...)` columns)
            // AND the mask kind is NOT `"none"`, emit a sibling
            // `<col>_masked TEXT NOT NULL` column alongside the parent.
            // The sibling stores the pre-computed masked representation
            // (e.g. `"***-**-6789"`) computed at INSERT/UPDATE time by
            // `crud::mask_pass::apply_mask_on_write`. Reads default to
            // the sibling (PR 3 flips the read path); writes dual-bind
            // both columns atomically (PR 2 SQL builder change).
            //
            // The sibling type is `TEXT` for every PR 2 mask kind
            // (full / last4 / first4 / email / name / dateYear /
            // dateDecade) — the union of mask outputs is string-shaped.
            // Future BYTEA-shaped masks would extend this with a per-
            // kind type lookup.
            //
            // Explicit `.mask({ kind: "none" })` opt-out → no sibling
            // emission. The P5 decrypt-on-read path continues to serve
            // such columns; the parent column is the only storage site.
            if let Some(sibling_col) = mask_sibling_column_for_field(field, def) {
                // `_masked` suffix is platform-reserved
                // (`validate_field_name`'s `ReservedName::Suffix`
                // forbids creator-declared columns ending in
                // `_masked`); no collision possible.
                //
                // **P5.5 PR 6** — attach a `/* __zsmask:kind=…,
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
                    "{} TEXT NOT NULL{inline_comment}",
                    quote_ident(&sibling_col)
                ));
            }

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
                        if let Ok(fk_clause) =
                            build_fk_clause(app_id, field, def, target, dialect)
                        {
                            deferred_fks.push(fk_clause);
                        }
                    }
                }
            }

            // C2 — per-variant CHECK constraints for a flat-expanded
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
                        emit_union_variant_checks(collection, field, def, variants);
                    union_checks.extend(constraint_clauses);
                }
            }
        }
    }

    // **P7 PR 2** — `created_at` / `updated_at` are emitted as part of
    // the seven system-field prefix at the top of `columns`; the
    // legacy trailing emission is gone. See `build_system_field_columns`
    // for the canonical declaration order.

    // **P7 PR 2** — defensive last-line-of-defence assertion. The
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
                let name = first.trim_matches('"');
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

    // **P5.5 PR 6** — append `COMMENT ON COLUMN` statements for every
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

    // **P7 PR 2** — append the three implicit B-tree indexes
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
    let system_index_stmts = build_system_field_indexes(app_id, collection, dialect);

    let mut statements: Vec<String> = vec![create_table];
    statements.extend(system_index_stmts);

    if matches!(dialect, SqlDialect::Postgres) {
        let comment_stmts = build_mask_sentinel_comments(app_id, collection, schema);
        statements.extend(comment_stmts);
    }

    Ok(statements.join(";\n"))
}

/// **P7 PR 2** — emit the seven platform-managed system-field column
/// declarations in canonical order ([`SYSTEM_FIELD_NAMES`]).
///
/// Order MUST match `SYSTEM_FIELD_NAMES`. The dialect controls
/// timestamp affinity (`TIMESTAMPTZ` on PG, `TEXT` on SQLite) and
/// the default expression (`NOW()` on PG, `CURRENT_TIMESTAMP` on
/// SQLite). `id`, `created_by`, `updated_by`, `version`, and the
/// `INTEGER` affinity for `version` are dialect-identical.
///
/// The `id` PK uses inline `PRIMARY KEY` (not a `CONSTRAINT ...`
/// table-level form) — matches the convention P0 already used for
/// the legacy `id SERIAL PRIMARY KEY` line this replaces. The
/// existing FK-attachment logic (B2 — `build_fk_clause`) references
/// the `id` column by name, so the switch from `SERIAL` to `TEXT`
/// is transparent to the FK emitter (FK column TYPE narrowing
/// cascades in PR 3).
fn build_system_field_columns(dialect: SqlDialect) -> Vec<String> {
    let (ts_type, ts_default) = match dialect {
        SqlDialect::Postgres => ("TIMESTAMPTZ", "NOW()"),
        SqlDialect::Sqlite => ("TEXT", "CURRENT_TIMESTAMP"),
    };
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

/// **P7 PR 2** — emit the three implicit B-tree indexes the platform
/// auto-creates for every new table: `deleted_at` (soft-delete
/// filtering — PR 5), `updated_at` (cursor-paged read paths), and
/// `created_by` (per-actor lookups + audit).
///
/// The PK on `id` covers `id` lookups via the implicit unique index;
/// `version` is not indexed (every UPDATE bumps it; the index would
/// thrash). See §5 of `docs/proposals/platform-system-fields.md` for
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
) -> Vec<String> {
    // The three columns the platform auto-indexes. `id` is implicitly
    // indexed by the PK constraint; `version` is deliberately skipped
    // (thrashing — bumped on every UPDATE).
    const SYSTEM_INDEXED_COLS: &[&str] = &["deleted_at", "updated_at", "created_by"];
    SYSTEM_INDEXED_COLS
        .iter()
        .map(|col| {
            let idx_name = index_name(collection, &[col], /* unique = */ false);
            match dialect {
                SqlDialect::Postgres => format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {}.{} ({})",
                    quote_ident(&idx_name),
                    quote_ident(app_id),
                    quote_ident(collection),
                    quote_ident(col),
                ),
                SqlDialect::Sqlite => format!(
                    "CREATE INDEX IF NOT EXISTS {}.{} ON {} ({})",
                    quote_ident(app_id),
                    quote_ident(&idx_name),
                    quote_ident(collection),
                    quote_ident(col),
                ),
            }
        })
        .collect()
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
    dialect: SqlDialect,
) -> Result<String, QueryError> {
    validate_collection(target)?;
    let constraint_name = fk_constraint_name(field, "");

    let on_delete = normalize_fk_action(def.get("onDelete").and_then(|v| v.as_str()));
    let on_update = normalize_fk_action(def.get("onUpdate").and_then(|v| v.as_str()));
    let deferrable = def
        .get("deferrable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let target_qualified = match dialect {
        SqlDialect::Postgres => format!("{}.{}", quote_ident(app_id), quote_ident(target)),
        // SQLite rejects schema-qualified parent-table names inside
        // REFERENCES clauses, even when the CREATE TABLE itself targets
        // an attached database alias.
        SqlDialect::Sqlite => quote_ident(target),
    };
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
fn normalize_fk_action_inner(s: Option<&str>) -> &'static str {
    match s.unwrap_or("restrict").to_ascii_lowercase().as_str() {
        "cascade" => "CASCADE",
        "set null" | "set_null" => "SET NULL",
        "no action" | "no_action" => "NO ACTION",
        _ => "RESTRICT",
    }
}

/// Normalise an FK action; used cross-module by the diff engine.
pub fn normalize_fk_action(s: Option<&str>) -> &'static str {
    normalize_fk_action_inner(s)
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

    // **P5.5 PR 6** — when the field carries a `.mask({...})`
    // declaration, also emit the sibling `<col>_masked TEXT NULL` ADD
    // COLUMN op and the `COMMENT ON COLUMN` sentinel attachment in the
    // same multi-statement payload. Only the sibling is NULL here
    // (versus NOT NULL on CREATE TABLE) — existing rows would refuse
    // the ALTER if the sibling were NOT NULL; the 6a backfill flips it
    // to NOT NULL after every row has its sibling populated.
    //
    // Note: this branch is taken ONLY when the diff classifier emits
    // an `AddColumn` for a fresh top-level field declared with
    // `.mask({...})` — for that case the sibling tags along in the
    // same payload. The separate `MaskBackfill`-paired
    // `AddColumn(<col>_masked)` op the diff classifier emits for 6a
    // sets `mask_sibling_for` in `details` and the field IS the
    // sibling itself; `mask_sibling_column_for_field(sibling, def)`
    // returns `None` there because the synthetic def carries no
    // mask block. So we don't double-emit.
    if let Some(sibling) = mask_sibling_column_for_field(field, def) {
        sql.push_str(&format!(
            ";\nALTER TABLE {} ADD COLUMN IF NOT EXISTS {} TEXT NULL",
            table,
            quote_ident(&sibling),
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
// Index builders for registerModel — A1 of the @zeroship/db proposal
// (docs/proposals/zeroship-db.md). Materialises `t.string().index()` /
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
///
/// **P4 PR 1**: `kind` carries the index *shape* — B-tree (the default for
/// every P0-P3 call site), vector (pgvector / Rust flat-scan), full-text
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
    /// Index shape — selects the backend builder branch. P4 PR 1
    /// introduces the field; P4 PR 2-5 wire `Vector` / `Fts` /
    /// `Spatial` dispatch through the `register_model::apply` Pass 2.
    pub kind: IndexKind,
}

/// Index shape — the closed sum over the four kinds of indexes
/// `registerModel` can materialise.
///
/// **P4 PR 1** (`docs/proposals/p4-search-implementation-plan.md` §2).
/// The default is [`IndexKind::BTree`] so every P0-P3 call site keeps
/// the same observable behaviour; PR 2/3 wire `Vector` / `Fts` /
/// `Spatial` dispatch through the `register_model::apply` Pass 2.
///
/// **Why an enum, not a string**: same rationale as
/// [`crate::descriptors::VectorMetric`] — the rustc exhaustiveness check
/// trips every match arm if a future PR adds a fifth kind, rather
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

    // **P4 PR 3** — accumulate FTS-marked columns into a single composite
    // index per collection (Q-P4-B from the design plan). The SDK's
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
        // **P5.5 PR 1** — skip top-level metadata keys (`_meta`,
        // `_indexes`) so the `_` reserved-prefix check in
        // `validate_field_name` (PR 1) doesn't trip on schema
        // bookkeeping.
        if is_schema_metadata_key(field) {
            continue;
        }
        // **P4 PR 3** — geoPoint fields always emit an
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

        // **P4 PR 3** — collect FTS-marked text columns. A column is
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

        // **P4 PR 2** — vector fields always emit an `IndexKind::Vector`
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

        // **P5 PR 2** — deterministic-encrypted columns get an
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

        // **P5.5 PR 2** — auto-emit a B-tree index on the sibling
        // `<col>_masked` column when the parent column has `.index()`
        // or `.uniqueIndex()` declared AND the field carries a mask
        // declaration with `kind != "none"`. The sibling index lets PR 3
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

    // **P4 PR 3** — emit a single composite FTS spec covering every
    // `.fts()`-marked column on this collection (Q-P4-B). The PG impl
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

/// **P5.5 PR 2** — return the sibling column name `<field>_masked` IFF
/// the field's schema entry carries a `.mask({...})` declaration with
/// `kind != "none"`. Returns `None` for non-masked columns and for
/// columns that explicitly opt out via `.mask({ kind: "none" })`.
///
/// The platform reserves the `_masked` suffix at the field-name level
/// (`validate_field_name`'s `ReservedName::Suffix`) so a creator cannot
/// shadow a sibling. Called by both `build_create_table_with_fks`
/// (DDL emission) and `build_insert` / `build_set_clauses` (atomic
/// dual-write).
pub fn mask_sibling_column_for_field(
    field: &str,
    def: &serde_json::Value,
) -> Option<String> {
    let mask_meta = def.get("mask").and_then(|v| v.as_object())?;
    let kind = mask_meta.get("kind").and_then(|v| v.as_str()).unwrap_or("full");
    if kind == "none" {
        return None;
    }
    Some(format!("{field}_masked"))
}

/// **P5.5 PR 6** — render the canonical mask-sentinel comment payload
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

/// **P5.5 PR 6** — render the `COMMENT ON COLUMN` statements that
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

/// **P5.5 PR 6** — render the `COMMENT ON COLUMN` statement for one
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
    // **P5 PR 2** — `t.encrypted(...)`-declared columns always store the
    // ciphertext wire blob (`[version_flag | nonce | ct+tag]`) as BYTEA
    // regardless of `wraps`. The encryption pass swaps the plaintext
    // out before the INSERT/UPDATE, and the SQL builder casts the
    // base64 parameter back to BYTEA via `decode($N, 'base64')::bytea`.
    //
    // **P5 PR 3** — emit a `/* zsenc:{mode}:{keyId}:{wraps} */` sentinel
    // comment alongside the column type so the SQLite-arm introspector
    // can regex-recover the encryption metadata from `sqlite_master.sql`.
    // PG ignores SQL comments at parse time (the type is still BYTEA);
    // SQLite stores the original CREATE TABLE text verbatim. SQLite's
    // type affinity treats "BYTEA" as NUMERIC (no INT/CHAR/TEXT/BLOB/
    // FLOA/REAL/DOUB substring match), which still accepts BLOB values
    // — same column shape both engines see byte-identical inserts.
    // Sentinel-on-DDL is the same regex-on-DDL pattern P4 PR 4 used for
    // vector dims; sidecar `__zs_schema_meta` is the upgrade path
    // (Q-P5 deferred). See
    // `docs/proposals/p5-encryption-backup-implementation-plan.md` §5.
    let enc_comment_owned;
    let enc_comment: &str = if let Some(enc) = def.get("encrypted").and_then(|v| v.as_object()) {
        let mode = enc.get("mode").and_then(|v| v.as_str()).unwrap_or("randomised");
        // Normalise legacy `"randomized"` (US spelling) to the canonical
        // `randomised` so the introspector regex (which accepts only the
        // canonical spelling) round-trips cleanly.
        let mode_norm = if mode == "randomized" { "randomised" } else { mode };
        let key_id = enc.get("keyId").and_then(|v| v.as_str()).unwrap_or("default");
        let wraps = enc.get("wraps").and_then(|v| v.as_str()).unwrap_or("string");
        enc_comment_owned = format!(" /* zsenc:{mode_norm}:{key_id}:{wraps} */");
        &enc_comment_owned
    } else {
        ""
    };
    let sql_type = def_to_column_type_for_dialect(def, dialect);
    let constraints = def_to_constraints_for_dialect(field, def, dialect);
    // The sentinel comment (when present) sits between the type and the
    // constraints so the parsed shape is `"<col>" BYTEA /* zsenc:... */
    // <constraints>`. PG ignores the comment; SQLite preserves it in
    // `sqlite_master.sql` for the introspector regex.
    Ok(format!(
        "{} {}{} {}",
        quote_ident(field),
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
/// adopts (schema-authority P2): the engine builds a `def` from its
/// `FieldDescriptor` and calls this, so it reaches full capability
/// (vector/encrypted/geo) by reuse rather than re-implementing — and never
/// rejects those types again. The returned spelling is DDL (`vector(N)`,
/// `DOUBLE PRECISION`, `TIMESTAMPTZ`, …); callers that need the
/// `information_schema.data_type` spelling translate it themselves.
pub fn def_to_column_type_for_dialect(def: &serde_json::Value, dialect: SqlDialect) -> String {
    if def.get("encrypted").is_some() {
        return match dialect {
            SqlDialect::Postgres => "BYTEA".to_string(),
            SqlDialect::Sqlite => "BLOB".to_string(),
        };
    }

    let zs_type = def.get("type").and_then(|t| t.as_str());

    if zs_type == Some("vector") {
        return match dialect {
            SqlDialect::Postgres => {
                let dims = def
                    .get("vectorDims")
                    .and_then(serde_json::Value::as_i64)
                    .filter(|d| *d > 0 && *d <= 16000)
                    .unwrap_or(0);
                if dims > 0 {
                    format!("vector({dims})")
                } else {
                    "vector".to_string()
                }
            }
            SqlDialect::Sqlite => "BLOB".to_string(),
        };
    }

    if zs_type == Some("geoPoint") {
        return match dialect {
            SqlDialect::Postgres => "geography(POINT, 4326)".to_string(),
            SqlDialect::Sqlite => "BLOB".to_string(),
        };
    }

    match dialect {
        SqlDialect::Postgres => def_to_pg_type(def).to_string(),
        SqlDialect::Sqlite => match zs_type {
            Some("string") => "TEXT".to_string(),
            Some("number") => "REAL".to_string(),
            Some("boolean") => "INTEGER".to_string(),
            Some("date") => "TEXT".to_string(),
            Some("calendarDate") => "TEXT".to_string(),
            Some("json") | Some("object") | Some("array") | Some("union") => {
                "TEXT".to_string()
            }
            Some("ref") => "TEXT".to_string(),
            Some("literal") => match def.get("literalValue") {
                Some(serde_json::Value::Number(_)) => "NUMERIC".to_string(),
                Some(serde_json::Value::Bool(_)) => "INTEGER".to_string(),
                _ => "TEXT".to_string(),
            },
            Some("bigint") | Some("int8") | Some("integer") | Some("int") | Some("int4") => {
                "INTEGER".to_string()
            }
            _ => "TEXT".to_string(),
        },
    }
}

/// C2 — emit per-variant CHECK constraints for a flat-expanded
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
) -> Vec<String> {
    let disc_col = quote_ident(disc_field);
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
                required_cols.push(quote_ident(field));
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
                quote_ident(&constraint_name),
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
/// **P7 PR 3** — `ref` columns now emit `TEXT` so they match the new
/// `id TEXT PRIMARY KEY` introduced by PR 2's system-field DDL. Pre-PR 2
/// behaviour was `INTEGER` to match the legacy `id SERIAL PRIMARY KEY`;
/// after PR 2 the parent PK is `TEXT` (typed_id wire format), so an
/// `INTEGER` FK would fail with `column type mismatch` at FK-constraint
/// creation time on Postgres. SQLite tolerates type mismatch (declared
/// types are advisory) but the typed_id values inserted into a ref
/// column are TEXT-shaped strings, so the storage class is TEXT either
/// way.
///
/// This change cascades the PR 2 deferred TODO: PR 2 prepended
/// `id TEXT PRIMARY KEY` but left `Some("ref") => "INTEGER"` because
/// the FK-emission unit tests would have flipped from substring-pass
/// to substring-fail without a coordinated test-fixture update. PR 3
/// ships both halves atomically (DDL + test fixture updates).
fn def_to_pg_type(def: &serde_json::Value) -> &'static str {
    match def.get("type").and_then(|t| t.as_str()) {
        Some("string") => "TEXT",
        // **P4 PR 2** — `t.vector(dims)` maps to pgvector's `vector(N)`.
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
        // **P7 PR 3** — cascades to TEXT so FK column type matches the
        // `id TEXT PRIMARY KEY` PR 2 introduced. See doc-comment on
        // [`def_to_pg_type`] for the back-compat rationale.
        Some("ref") => "TEXT",
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
            Some("json") | Some("object") => parts.push(match dialect {
                SqlDialect::Postgres => "DEFAULT '{}'::jsonb".to_string(),
                SqlDialect::Sqlite => "DEFAULT '{}'".to_string(),
            }),
            Some("array") => parts.push(match dialect {
                SqlDialect::Postgres => "DEFAULT '[]'::jsonb".to_string(),
                SqlDialect::Sqlite => "DEFAULT '[]'".to_string(),
            }),
            _ => {}
        }
    } else {
        // Default defaults for json/object/array
        match def.get("type").and_then(|t| t.as_str()) {
            Some("json") | Some("object") => parts.push(match dialect {
                SqlDialect::Postgres => "DEFAULT '{}'::jsonb".to_string(),
                SqlDialect::Sqlite => "DEFAULT '{}'".to_string(),
            }),
            Some("array") => parts.push(match dialect {
                SqlDialect::Postgres => "DEFAULT '[]'::jsonb".to_string(),
                SqlDialect::Sqlite => "DEFAULT '[]'".to_string(),
            }),
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
///
/// Thin shim around [`build_find_with_schema`] that passes `None` for the
/// schema — the legacy CRUD entry point. Callers that have a cached schema
/// available (the orchestrator's `dispatch_find`) should prefer
/// [`build_find_with_schema`] so the SELECT clause can
/// substitute `"<col>_masked" AS "<col>"` for every masked column (P5.5 PR 3,
/// "default reads serve from the masked sibling").
pub fn build_find(
    app_id: &str,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
) -> Result<BuiltQuery, QueryError> {
    build_find_with_schema(app_id, collection, filter, limit, offset, order_by, select, None)
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
        if field.starts_with("__zsenc__") {
            continue;
        }
        validate_field_name(field)?;
        let col = quote_ident(field);
        if value.is_null() {
            conditions.push(format!("{col} IS NULL"));
            continue;
        }

        let raw = value_to_param(value);
        let encrypted = obj.contains_key(&format!("__zsenc__{field}"));
        let param_value = if encrypted {
            dialect.wrap_encrypted_param(raw)
        } else {
            raw
        };
        params.push(param_value);
        let n = params.len();
        if encrypted {
            conditions.push(format!("{col} = {}", dialect.encrypted_column_bind_placeholder(n)));
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

/// **P5.5 PR 3** — schema-aware SELECT builder.
///
/// Same shape as [`build_find`], plus an optional `schema` (the cached
/// `serde_json::Value` from `IsolateDbContext::schema_for`). When the
/// schema is `Some(_)` and declares masked columns (`def.mask = Some({...})`
/// with `kind != "none"`), the SELECT clause emits
/// `"<col>_masked" AS "<col>"` in place of the bare parent column, and
/// the ciphertext / plaintext column is NOT included. This is the load-
/// bearing read-side flip from "decrypt on read" (P5) to "serve from the
/// masked sibling" (P5.5 Path B).
///
/// Generated SQL example (PG):
/// ```sql
/// -- P5 baseline (schema=None or no masked columns):
/// SELECT * FROM users WHERE id = $1
///
/// -- P5.5 Path B (schema declares ssn + email masked):
/// SELECT "id", "ssn_masked" AS "ssn", "email_masked" AS "email", "name"
///   FROM users WHERE id = $1
/// ```
///
/// Opt-out path: columns declared with `.mask({ kind: "none" })` keep
/// emitting the parent column directly, preserving the P5 decrypt-on-read
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
    schema_hint: Option<&Value>,
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

/// **P5.5 PR 7** — schema-aware SELECT builder with per-query unmask
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
/// **P7 PR 5** — thin shim around
/// [`build_find_with_schema_and_unmask_and_soft_delete`] passing
/// `filter_soft_deleted = false` so direct callers (the legacy CRUD
/// entry points + tests) keep the pre-PR-5 contract. The CRUD dispatch
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
    schema_hint: Option<&Value>,
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
    schema_hint: Option<&Value>,
    unmask_columns: &[String],
    filter_soft_deleted: bool,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where_with_dialect(filter, &mut params, dialect)?;
    if let Some(lim) = limit {
        validate_limit_bound("find.limit", lim, MAX_QUERY_LIMIT)?;
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

/// **P7 PR 5** — schema-aware SELECT builder with the soft-delete
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
/// pre-PR-5 builder. Direct callers (tests, raw SQL probes) keep
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
    schema_hint: Option<&Value>,
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

/// **P7 PR 5** — compose a WHERE clause body with the soft-delete
/// auto-filter. Mirrors the same `creator AND deleted_at IS NULL`
/// pattern used by [`build_soft_delete_one_with_system_fields`] /
/// [`build_restore_one_with_system_fields`] inner SELECTs.
///
/// Three cases:
/// 1. `!filter_soft_deleted` → return `where_clause` verbatim (back-
///    compat with pre-PR-5 callers).
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

/// **P5.5 PR 3** — compose the SELECT column-list expression, accounting
/// for masked columns when `schema_hint` is `Some(_)`. Thin shim around
/// [`build_masked_aware_select_expr_with_unmask`] for legacy callers
/// that have no per-query unmask hint to thread through.
pub fn build_masked_aware_select_expr(
    select: Option<&Value>,
    schema_hint: Option<&Value>,
) -> Result<String, QueryError> {
    build_masked_aware_select_expr_with_unmask(select, schema_hint, &[])
}

/// **P5.5 PR 8** — compose the implicit `SELECT` list for a qualified
/// table source (`t`, `src`, ...), accounting for masked columns in the
/// cached schema.
///
/// This is the specialized-search sibling of
/// [`build_masked_aware_select_expr`]. The search paths (`search`,
/// `fts`, `near`) all read from a table alias (`t`) and append one
/// synthetic engine column (`_distance`, `_rank`, `_distance_m`). When
/// the cached schema declares any masked column, emitting `t.*` drifts
/// back to the pre-P5.5 shape: the parent ciphertext/plaintext column
/// rides out of SQL and only gets corrected later in the read pipeline.
///
/// Instead, when any masked column exists we expand to an explicit
/// qualified list:
/// - `t."id" AS "id"` first,
/// - `t."<col>_masked" AS "<col>"` for masked columns,
/// - `t."<col>" AS "<col>"` for non-masked columns.
///
/// When the schema cache is warm we always expand to the allowlisted
/// public column set so read paths cannot surface internal physical
/// columns. Only a cold schema cache falls back to `t.*`.
pub fn build_masked_aware_select_expr_for_table_alias(
    schema_hint: Option<&Value>,
    table_alias: &str,
) -> String {
    let qalias = quote_ident(table_alias);
    let empty_unmask: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let Some(parts) = implicit_read_projection_parts(schema_hint, &empty_unmask, Some(table_alias))
    else {
        return format!("{qalias}.*");
    };
    parts.join(", ")
}

/// **P5.5 PR 7** — compose the SELECT column-list expression, accounting
/// for masked columns AND a per-query unmask hint.
///
/// Three cases (same as PR 3) — the unmask hint just overrides the
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
    schema_hint: Option<&Value>,
    unmask_columns: &[String],
) -> Result<String, QueryError> {
    let unmask_set: std::collections::HashSet<&str> =
        unmask_columns.iter().map(String::as_str).collect();

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
                cols.push(project_read_field(name, schema_hint, &unmask_set, None));
            }
            return Ok(cols.join(", "));
        }
    }

    if let Some(parts) = implicit_read_projection_parts(schema_hint, &unmask_set, None) {
        return Ok(parts.join(", "));
    }

    Ok("*".to_string())
}

fn qualified_read_field(table_alias: Option<&str>, field: &str) -> String {
    match table_alias {
        Some(alias) => format!("{}.{}", quote_ident(alias), quote_ident(field)),
        None => quote_ident(field),
    }
}

fn project_read_field(
    field: &str,
    schema_hint: Option<&Value>,
    unmask_set: &std::collections::HashSet<&str>,
    table_alias: Option<&str>,
) -> String {
    let logical = quote_ident(field);
    let source = if unmask_set.contains(field) || !column_is_masked(field, schema_hint) {
        qualified_read_field(table_alias, field)
    } else {
        qualified_read_field(table_alias, &format!("{field}_masked"))
    };
    if table_alias.is_some() || source != logical {
        format!("{source} AS {logical}")
    } else {
        logical
    }
}

fn implicit_read_projection_parts(
    schema_hint: Option<&Value>,
    unmask_set: &std::collections::HashSet<&str>,
    table_alias: Option<&str>,
) -> Option<Vec<String>> {
    let schema_obj = schema_hint.and_then(Value::as_object)?;
    let mut parts = Vec::with_capacity(SYSTEM_FIELD_NAMES.len() + schema_obj.len());
    for field in SYSTEM_FIELD_NAMES {
        parts.push(project_read_field(field, schema_hint, unmask_set, table_alias));
    }
    for field in schema_obj.keys() {
        if is_schema_metadata_key(field) || SYSTEM_FIELD_NAMES.contains(&field.as_str()) {
            continue;
        }
        parts.push(project_read_field(field, schema_hint, unmask_set, table_alias));
    }
    Some(parts)
}

/// **P5.5 PR 3** — does the column named `name` declare a non-`none`
/// `.mask({...})` entry on `schema_hint`? Returns `false` when the
/// schema is missing, the column is absent from it, or the mask is the
/// explicit opt-out (`kind: "none"`).
pub fn column_is_masked(name: &str, schema_hint: Option<&Value>) -> bool {
    let Some(schema_obj) = schema_hint.and_then(|v| v.as_object()) else {
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

/// **SEC-4** — the column SQL expression to read for `field` inside an
/// aggregate, substituting the `<field>_masked` sibling when `field` is a
/// masked column.
///
/// For a mask-only column the plaintext lives in `<field>` and the masked
/// string in `<field>_masked`. The normal read path and `build_distinct`
/// alias the sibling back to the logical name (`"<field>_masked" AS
/// "<field>"`); the aggregate builder must do the same so `$group.by` /
/// `$sum` / `$avg` / `$min` / `$max` / `$first` / `$sort` / `$having`
/// never lower to the bare plaintext column. Returns a quoted identifier
/// (the sibling when masked, the field itself otherwise) — NOT aliased,
/// since the aggregate builder applies its own `AS` where appropriate.
pub fn aggregate_read_ident(field: &str, schema_hint: Option<&Value>) -> String {
    if column_is_masked(field, schema_hint) {
        quote_ident(&format!("{field}_masked"))
    } else {
        quote_ident(field)
    }
}

/// **SEC-4** — push one `$group.by` field's SELECT projection and GROUP
/// BY term. A masked column projects `"<col>_masked" AS "<col>"` (so the
/// row carries the masked string under the logical name, exactly like
/// `build_distinct`) and groups by the masked sibling; an unmasked
/// column projects + groups by the bare quoted column.
fn push_group_by_field(
    field: &str,
    schema_hint: Option<&Value>,
    select_cols: &mut Vec<String>,
    group_by_cols: &mut Vec<String>,
) {
    let logical = quote_ident(field);
    if column_is_masked(field, schema_hint) {
        let sibling = quote_ident(&format!("{field}_masked"));
        select_cols.push(format!("{sibling} AS {logical}"));
        group_by_cols.push(sibling);
    } else {
        select_cols.push(logical.clone());
        group_by_cols.push(logical);
    }
}

/// Build a SELECT COUNT(*) query.
///
/// **P7 PR 5** — thin shim around [`build_count_with_soft_delete`]
/// passing `filter_soft_deleted = false`.
pub fn build_count(
    app_id: &str,
    collection: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_count_with_soft_delete(app_id, collection, filter, false)
}

/// **P7 PR 5** — COUNT(*) with the soft-delete auto-filter. The CRUD
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

/// Build an INSERT query: `INSERT INTO "app_id"."collection" (...) VALUES (...) RETURNING *`
///
/// PG-flavour wrapper around [`build_insert_with_dialect`]. Every
/// existing CRUD call site stays on this signature — the orchestrator's
/// `dispatch_insert` path still goes through Postgres today.
pub fn build_insert(
    app_id: &str,
    collection: &str,
    doc: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_insert_with_dialect(app_id, collection, doc, SqlDialect::Postgres)
}

/// **P5 PR 3.5** — dialect-aware INSERT builder.
///
/// PG emits `decode($N, 'base64')::bytea` for encrypted columns
/// (preserved byte-for-byte from PR 2); SQLite emits `$N` and tags the
/// param value with [`SQLITE_ENC_BLOB_PREFIX`] so the session actor
/// can bind the raw bytes as BLOB. Non-encrypted columns are
/// dialect-agnostic on both arms.
pub fn build_insert_with_dialect(
    app_id: &str,
    collection: &str,
    doc: &Value,
    dialect: SqlDialect,
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

    // **P5 PR 2** — the encryption pass marks each encrypted column
    // with a sibling `__zsenc__<col>` key (`Value::Bool(true)`); the
    // value at `<col>` is base64-encoded ciphertext. Walk the doc once
    // to collect those marker keys so we can:
    //   1. Skip emitting marker keys as columns.
    //   2. Wrap encrypted-column placeholders with the dialect's bind
    //      shape ([`SqlDialect::encrypted_column_bind_placeholder`]).
    let encrypted_cols = collect_encrypted_cols(obj);

    let mut columns = Vec::new();
    let mut placeholders = Vec::new();
    let mut params: Vec<String> = Vec::new();

    for (key, value) in obj {
        if key.starts_with("__zsenc__") {
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
            let is_encrypted = encrypted_cols.contains(key.as_str());
            let raw = value_to_param(value);
            let param_value = if is_encrypted {
                dialect.wrap_encrypted_param(raw)
            } else {
                raw
            };
            params.push(param_value);
            let n = params.len();
            if is_encrypted {
                placeholders.push(dialect.encrypted_column_bind_placeholder(n));
            } else {
                placeholders.push(format!("${n}"));
            }
        }
    }

    let sql = format!(
        "INSERT INTO {schema}.{table} ({}) VALUES ({}) RETURNING *",
        columns.join(", "),
        placeholders.join(", ")
    );

    Ok(BuiltQuery { sql, params })
}

/// **P5 PR 2** — collect the set of column names the encryption pass
/// has marked as encrypted. The marker is a sibling key
/// `__zsenc__<col> = true` inserted by
/// `crate::crud::encryption_pass::encrypt_row_on_write`. Callers walk
/// the doc once with this set to know which placeholders need the
/// `decode($N, 'base64')::bytea` cast.
pub fn collect_encrypted_cols(
    obj: &serde_json::Map<String, Value>,
) -> std::collections::HashSet<&str> {
    let mut out = std::collections::HashSet::new();
    for (k, v) in obj {
        if let Some(name) = k.strip_prefix("__zsenc__") {
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

/// **P7 PR 4** — knobs the SET-clause builder needs to compose the
/// platform's auto-bump system-field SET clauses correctly.
///
/// Three independent bumps, each suppressed when the creator's patch
/// already provided an explicit value for that column (per
/// [`crate::crud::system_fields_pass::apply_system_fields_on_update`]
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
/// pre-PR-4 `build_set_clauses_with_dialect` wrapper) so behaviour
/// outside the dispatch path is unchanged.
#[derive(Debug, Clone, Default)]
pub struct SystemFieldAutoBump<'a> {
    /// `true` when the call came from the CRUD dispatch path rather than a
    /// legacy direct builder caller. Controls whether version auto-bumps run
    /// for anonymous writes.
    pub dispatch_write: bool,
    /// Bind value for the `updated_by` placeholder. When `None`, the
    /// `updated_by` SET clause is suppressed (no actor in scope —
    /// matches the PR 3 INSERT path's "leave NULL when anonymous"
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

/// **P5 PR 3.5** — dialect-aware SET-clause builder for `build_update_one` /
/// `build_update_many`. PG keeps the `decode($N, 'base64')::bytea` cast
/// (byte-for-byte identical to PR 2); SQLite emits a plain `$N` and
/// tags the encrypted-column param value with [`SQLITE_ENC_BLOB_PREFIX`].
///
/// **P7 PR 4** — emits the dialect-appropriate `updated_at` auto-bump
/// (`NOW()` on PG, `CURRENT_TIMESTAMP` on SQLite). To compose the full
/// `version` / `updated_at` / `updated_by` auto-bump set used by the
/// CRUD dispatch path, callers should use
/// [`build_set_clauses_with_system_fields`] instead — this wrapper
/// preserves the pre-PR-4 single-column auto-bump behaviour for
/// existing direct callers.
pub fn build_set_clauses_with_dialect(
    update: &Value,
    params: &mut Vec<String>,
    dialect: SqlDialect,
) -> Result<Vec<String>, QueryError> {
    // The default auto-bump is empty (no version bump, no updated_by) —
    // preserves the pre-PR-4 contract.
    build_set_clauses_with_system_fields(
        update,
        params,
        dialect,
        &SystemFieldAutoBump::default(),
    )
}

/// **P7 PR 4** — SET-clause builder + system-field auto-bump pass.
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

    // **P5 PR 2** — collect encrypted-column markers from the update
    // doc (top-level AND nested `$set`). The encryption pass deposits
    // both the base64 value and a `__zsenc__<col>` marker; we use the
    // marker set to wrap the placeholder via the dialect's bind shape
    // (`SqlDialect::encrypted_column_bind_placeholder`).
    let mut encrypted_cols = collect_encrypted_cols(update_obj);
    if let Some(set_obj) = update_obj.get("$set").and_then(|v| v.as_object()) {
        encrypted_cols.extend(collect_encrypted_cols(set_obj));
    }

    // Collect all fields: flatten $set inline, keep other keys as-is.
    // Skip every `__zsenc__*` marker key — they're a side-channel from
    // the encryption pass, not user-declared columns.
    let mut fields: Vec<(&String, &Value)> = Vec::new();
    for (key, value) in update_obj.iter() {
        if key.starts_with("__zsenc__") {
            continue;
        }
        if key == "$set" {
            // Flatten $set fields into the top level
            let obj = value
                .as_object()
                .ok_or_else(|| QueryError::InvalidFilter("$set must be an object".to_string()))?;
            for (k, v) in obj.iter() {
                if k.starts_with("__zsenc__") {
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
                        let is_encrypted = encrypted_cols.contains(key.as_str());
                        let raw = value_to_param(op_val);
                        let param_value = if is_encrypted {
                            dialect.wrap_encrypted_param(raw)
                        } else {
                            raw
                        };
                        params.push(param_value);
                        let n = params.len();
                        if is_encrypted {
                            format!("{col} = {}", dialect.encrypted_column_bind_placeholder(n))
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
        let is_encrypted = encrypted_cols.contains(key.as_str());
        let raw = value_to_param(value);
        let param_value = if is_encrypted {
            dialect.wrap_encrypted_param(raw)
        } else {
            raw
        };
        params.push(param_value);
        let n = params.len();
        if is_encrypted {
            set_clauses.push(format!("{col} = {}", dialect.encrypted_column_bind_placeholder(n)));
        } else {
            set_clauses.push(format!("{col} = ${n}"));
        }
    }

    // **P7 PR 4** — system-field auto-bump SET clauses. Appended AFTER
    // every creator-supplied clause (encryption-pass / mask-pass output
    // included) so the diff against the creator's patch is grep-able
    // AND so the auto-bumps bypass encryption / masking by
    // construction. Each bump skipped when the creator's patch
    // explicitly supplied that column (the value flows through the
    // standard SET loop above; the explicit value wins per Q-SF-B).
    //
    // For backwards-compatibility with pre-PR-4 direct callers, the
    // legacy "auto-bump updated_at when not explicit" path stays
    // unchanged: when the caller passed `SystemFieldAutoBump::default()`
    // (the wrapper from `build_set_clauses_with_dialect`), the only
    // bump emitted is `updated_at` and it inspects the existing
    // `set_clauses` for an explicit override. The `autobump.skip_*`
    // flags are only ever set by the new PR 4 dispatch path
    // (`apply_system_fields_on_update` populates the hints).
    let already_has_updated_at = set_clauses.iter().any(|c| c.contains("\"updated_at\""));
    let already_has_version = set_clauses.iter().any(|c| c.contains("\"version\""));
    let already_has_updated_by = set_clauses.iter().any(|c| c.contains("\"updated_by\""));

    // `version` auto-bump fires only on the new PR 4 dispatch path
    // (signalled by an `actor_id` being threaded through OR by an
    // explicit `skip_version = false` from the caller's hints). To
    // keep the pre-PR-4 direct-caller contract intact, we use a
    // discriminator: the legacy path always passes `actor_id = None`
    // AND `skip_version = false` (the `Default::default()` shape) —
    // we only emit the version bump when `actor_id.is_some()` OR the
    // caller asked for it explicitly via a `skip_updated_by = true`
    // setting (which is impossible from the default and only set by
    // the new PR 4 helper). The actor presence is the discriminator
    // because the legacy callers never thread one through.
    let on_pr4_dispatch_path = autobump.dispatch_write;
    if on_pr4_dispatch_path && !autobump.skip_version && !already_has_version {
        set_clauses.push("\"version\" = \"version\" + 1".to_string());
    }

    // `updated_at` auto-bump — dialect-aware (PG `NOW()` /
    // SQLite `CURRENT_TIMESTAMP`). This fires on BOTH paths (PR 4
    // dispatch AND legacy direct callers) since the pre-PR-4 contract
    // already emitted `updated_at = NOW()` on every UPDATE.
    if !autobump.skip_updated_at && !already_has_updated_at {
        let ts_expr = match dialect {
            SqlDialect::Postgres => "NOW()",
            SqlDialect::Sqlite => "CURRENT_TIMESTAMP",
        };
        set_clauses.push(format!("\"updated_at\" = {ts_expr}"));
    }

    // `updated_by` auto-bump — actor-bound. Fires only on the PR 4
    // dispatch path when an actor is in scope (anonymous writes leave
    // `updated_by` untouched, mirroring the PR 3 INSERT "NULL when no
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

/// Build an UPDATE query: `UPDATE "app_id"."collection" SET ... WHERE ctid = (...) RETURNING *`
///
/// PG-flavour wrapper — every existing call site goes through Postgres.
pub fn build_update_one(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_update_one_with_dialect(app_id, collection, filter, update, SqlDialect::Postgres)
}

/// **P5 PR 3.5** — dialect-aware `updateOne` builder. Encrypted-column
/// binds follow the dialect's
/// [`SqlDialect::encrypted_column_bind_placeholder`]. The `ctid` subquery
/// shape is PG-specific (`SqlDialect::Sqlite` callers should rebuild
/// the LIMIT 1 narrowing differently — out of scope for PR 3.5; the
/// builder body remains PG-shaped here).
pub fn build_update_one_with_dialect(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    build_update_one_with_system_fields(
        app_id,
        collection,
        filter,
        update,
        dialect,
        &SystemFieldAutoBump::default(),
    )
}

/// **P7 PR 4** — dialect-aware `updateOne` builder + system-field
/// auto-bump. The CRUD dispatch path uses this so every UPDATE
/// transparently bumps `version` + `updated_at` + `updated_by` (per
/// the `autobump` knobs). Direct callers that need byte-identical
/// pre-PR-4 SQL keep using [`build_update_one_with_dialect`].
pub fn build_update_one_with_system_fields(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

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
    };
    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{inner_where} LIMIT 1) RETURNING *",
        set_clauses.join(", "),
    );

    Ok(BuiltQuery { sql, params })
}

/// Build an INSERT query for multiple documents:
/// `INSERT INTO "app_id"."collection" ("col1", "col2") VALUES ($1, $2), ($3, $4) RETURNING *`
///
/// All docs must have the same column set (defined by the first document).
///
/// PG-flavour wrapper around [`build_insert_many_with_dialect`].
pub fn build_insert_many(
    app_id: &str,
    collection: &str,
    docs: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_insert_many_with_dialect(app_id, collection, docs, SqlDialect::Postgres)
}

/// **P5 PR 3.5** — dialect-aware `insertMany` builder.
pub fn build_insert_many_with_dialect(
    app_id: &str,
    collection: &str,
    docs: &Value,
    dialect: SqlDialect,
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

    // DB-11: cap the batch BEFORE materializing the multi-row SQL + param vec,
    // so one call can't slam a multi-MB statement at the shared DB or blow the
    // worker heap. Enforced in the builder (the single choke point) so a raw
    // `default={fetch}` deploy bypassing the SDK is bounded too.
    if arr.len() > MAX_INSERT_MANY_BATCH {
        return Err(QueryError::InvalidFilter(format!(
            "insertMany batch of {} exceeds the maximum of {MAX_INSERT_MANY_BATCH}",
            arr.len()
        )));
    }

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    // **P5 PR 2** — encrypted-column union across all docs. We treat
    // a column as encrypted iff ANY doc carries the `__zsenc__<col>`
    // marker (the encryption pass marks every doc consistently — if
    // the schema says the column is encrypted, every row in the batch
    // gets the marker after the pass runs).
    let mut encrypted_cols: std::collections::HashSet<String> = std::collections::HashSet::new();
    for doc in arr {
        if let Some(obj) = doc.as_object() {
            for name in collect_encrypted_cols(obj) {
                encrypted_cols.insert(name.to_string());
            }
        }
    }

    // Union all columns across all documents (not just the first).
    // Skip every `__zsenc__*` marker key — they're a side-channel from
    // the encryption pass, not user-declared columns.
    let mut column_set = std::collections::BTreeSet::<&String>::new();
    for doc in arr {
        let obj = doc.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("insertMany: each document must be an object".to_string())
        })?;
        for key in obj.keys() {
            if key.starts_with("__zsenc__") {
                continue;
            }
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
            // See build_insert: text-format params can't carry NULL;
            // inline as a SQL literal instead.
            if val.is_null() {
                placeholders.push("NULL".to_string());
            } else {
                let is_encrypted = encrypted_cols.contains(key.as_str());
                let raw = value_to_param(val);
                let param_value = if is_encrypted {
                    dialect.wrap_encrypted_param(raw)
                } else {
                    raw
                };
                params.push(param_value);
                let n = params.len();
                if is_encrypted {
                    placeholders.push(dialect.encrypted_column_bind_placeholder(n));
                } else {
                    placeholders.push(format!("${n}"));
                }
            }
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
///
/// PG-flavour wrapper around [`build_update_many_with_dialect`].
pub fn build_update_many(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_update_many_with_dialect(app_id, collection, filter, update, SqlDialect::Postgres)
}

/// **P5 PR 3.5** — dialect-aware `updateMany` builder. Encrypted-column
/// binds follow the dialect's
/// [`SqlDialect::encrypted_column_bind_placeholder`].
pub fn build_update_many_with_dialect(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    build_update_many_with_system_fields(
        app_id,
        collection,
        filter,
        update,
        dialect,
        &SystemFieldAutoBump::default(),
    )
}

/// **P7 PR 4** — dialect-aware `updateMany` builder + system-field
/// auto-bump. Same auto-bump semantics as
/// [`build_update_one_with_system_fields`]; CRUD dispatch path uses
/// this to keep the bulk-update SQL emitting `version` + `updated_at`
/// + `updated_by` bumps even on multi-row updates.
pub fn build_update_many_with_system_fields(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

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
    build_delete_one_with_dialect(app_id, collection, filter, SqlDialect::Postgres)
}

/// Dialect-aware single-row DELETE builder.
pub fn build_delete_one_with_dialect(
    app_id: &str,
    collection: &str,
    filter: &Value,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let target_col = match dialect {
        SqlDialect::Postgres => "ctid",
        SqlDialect::Sqlite => "rowid",
    };
    let sql = format!(
        "DELETE FROM {schema}.{table} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{} LIMIT 1) RETURNING *",
        if where_clause.is_empty() {
            String::new()
        } else {
            format!(" WHERE {where_clause}")
        }
    );

    Ok(BuiltQuery { sql, params })
}

// ---------------------------------------------------------------------------
// **P7 PR 5** — soft-delete / restore SQL builders.
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

/// **P7 PR 5** — dialect-appropriate `NOW()` / `CURRENT_TIMESTAMP`
/// expression for stamping a `deleted_at` column on the soft-delete
/// path. Mirrors the same lookup
/// [`build_set_clauses_with_system_fields`] does for `updated_at`.
fn now_expr(dialect: SqlDialect) -> &'static str {
    match dialect {
        SqlDialect::Postgres => "NOW()",
        SqlDialect::Sqlite => "CURRENT_TIMESTAMP",
    }
}

/// **P7 PR 5** — compose the SET clauses for a soft-delete: the
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

/// **P7 PR 5** — compose the SET clauses for `restore()`: clear
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

/// **P7 PR 5** — dialect-aware `soft_delete_one` builder. Used by the
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
/// RETURNING *
/// ```
///
/// The `AND deleted_at IS NULL` in the inner SELECT keeps the call
/// idempotent: re-deleting an already-deleted row affects 0 rows. The
/// dispatch layer translates 0-affected to a `null` result (matches the
/// `deleteOne` contract pre-PR-5).
pub fn build_soft_delete_one_with_system_fields(
    app_id: &str,
    collection: &str,
    filter: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

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
    };
    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{inner_where} LIMIT 1) RETURNING *",
        set_clauses.join(", "),
    );

    Ok(BuiltQuery { sql, params })
}

/// **P7 PR 5** — dialect-aware `soft_delete_many` builder. Same shape
/// as [`build_soft_delete_one_with_system_fields`] minus the `ctid`
/// LIMIT 1 narrowing — every live row matching `filter` flips
/// `deleted_at` to the dialect's `NOW()`-equivalent.
///
/// `AND deleted_at IS NULL` is preserved so re-deleting an already-
/// deleted row is still a no-op.
pub fn build_soft_delete_many_with_system_fields(
    app_id: &str,
    collection: &str,
    filter: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

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
        "UPDATE {schema}.{table} SET {}{where_sql} RETURNING *",
        set_clauses.join(", "),
    );

    Ok(BuiltQuery { sql, params })
}

/// **P7 PR 5** — dialect-aware `restore_one` builder. Symmetric to
/// [`build_soft_delete_one_with_system_fields`]: clears `deleted_at`
/// and scopes to rows that are CURRENTLY soft-deleted
/// (`deleted_at IS NOT NULL`) so restoring a live row is a no-op
/// (affected-rows = 0 → typed `not_found_or_already_live` via the
/// dispatch layer).
pub fn build_restore_one_with_system_fields(
    app_id: &str,
    collection: &str,
    filter: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

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
    };
    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE {target_col} = (SELECT {target_col} FROM {schema}.{table}{inner_where} LIMIT 1) RETURNING *",
        set_clauses.join(", "),
    );

    Ok(BuiltQuery { sql, params })
}

/// **P7 PR 5** — dialect-aware `restore_many` builder.
pub fn build_restore_many_with_system_fields(
    app_id: &str,
    collection: &str,
    filter: &Value,
    dialect: SqlDialect,
    autobump: &SystemFieldAutoBump<'_>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

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
        "UPDATE {schema}.{table} SET {}{where_sql} RETURNING *",
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
/// **P7 PR 5** — thin shim around
/// [`build_aggregate_with_soft_delete`] passing
/// `filter_soft_deleted = false`. CRUD dispatch threads the auto-
/// filter through the dedicated entry; direct callers keep the pre-
/// PR-5 SQL byte-identical.
pub fn build_aggregate(
    app_id: &str,
    collection: &str,
    pipeline: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_aggregate_with_soft_delete(app_id, collection, pipeline, false)
}

/// **P7 PR 5** — aggregate builder with the soft-delete auto-filter.
///
/// When `filter_soft_deleted = true`, appends `AND deleted_at IS NULL`
/// to whatever WHERE clause the pipeline's `$match` stage produced
/// (or `WHERE deleted_at IS NULL` when no `$match` is present).
pub fn build_aggregate_with_soft_delete(
    app_id: &str,
    collection: &str,
    pipeline: &Value,
    filter_soft_deleted: bool,
) -> Result<BuiltQuery, QueryError> {
    build_aggregate_with_soft_delete_with_dialect(
        app_id,
        collection,
        pipeline,
        filter_soft_deleted,
        None,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware variant of [`build_aggregate_with_soft_delete`].
pub fn build_aggregate_with_soft_delete_with_dialect(
    app_id: &str,
    collection: &str,
    pipeline: &Value,
    filter_soft_deleted: bool,
    schema_hint: Option<&Value>,
    dialect: SqlDialect,
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
                        push_group_by_field(s, schema_hint, &mut select_cols, &mut group_by_cols);
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
                            push_group_by_field(
                                s,
                                schema_hint,
                                &mut select_cols,
                                &mut group_by_cols,
                            );
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
                        format!("SUM({})", aggregate_read_ident(field, schema_hint))
                    }
                    "$avg" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$avg requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        format!("AVG({})", aggregate_read_ident(field, schema_hint))
                    }
                    "$min" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$min requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        format!("MIN({})", aggregate_read_ident(field, schema_hint))
                    }
                    "$max" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$max requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        format!("MAX({})", aggregate_read_ident(field, schema_hint))
                    }
                    "$first" => {
                        let field = op_val.as_str().ok_or_else(|| {
                            QueryError::InvalidFilter(
                                "$first requires a field name string".to_string(),
                            )
                        })?;
                        validate_read_identifier(field, schema_hint)?;
                        // SEC-4: read the masked sibling for masked columns.
                        let read_ident = aggregate_read_ident(field, schema_hint);
                        if last_sort.is_empty() {
                            format!("(array_agg({read_ident}))[1]")
                        } else {
                            let order_parts: Vec<String> = last_sort
                                .iter()
                                .map(|(col, descending)| {
                                    build_order_term_with_schema(
                                        col,
                                        *descending,
                                        dialect,
                                        schema_hint,
                                    )
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

    let select_expr = if select_cols.is_empty() {
        let empty_unmask: std::collections::HashSet<&str> = std::collections::HashSet::new();
        implicit_read_projection_parts(schema_hint, &empty_unmask, None)
            .map(|parts| parts.join(", "))
            .unwrap_or_else(|| "*".to_string())
    } else {
        select_cols.join(", ")
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

    Ok(BuiltQuery { sql, params })
}

/// Build a SELECT DISTINCT query:
/// `SELECT DISTINCT "field" FROM "schema"."table" WHERE ... ORDER BY "field"`
///
/// **P7 PR 5** — thin shim around
/// [`build_distinct_with_soft_delete`] passing
/// `filter_soft_deleted = false`.
pub fn build_distinct(
    app_id: &str,
    collection: &str,
    field: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    build_distinct_with_soft_delete(app_id, collection, field, filter, false)
}

/// **P7 PR 5** — DISTINCT builder with the soft-delete auto-filter.
pub fn build_distinct_with_soft_delete(
    app_id: &str,
    collection: &str,
    field: &str,
    filter: &Value,
    filter_soft_deleted: bool,
) -> Result<BuiltQuery, QueryError> {
    build_distinct_with_soft_delete_with_dialect(
        app_id,
        collection,
        field,
        filter,
        filter_soft_deleted,
        None,
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
    schema_hint: Option<&Value>,
    dialect: SqlDialect,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    validate_read_identifier(field, schema_hint)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let col = quote_ident(field);
    let select_expr = if column_is_masked(field, schema_hint) {
        let sibling = format!("{field}_masked");
        format!("{} AS {col}", quote_ident(&sibling))
    } else {
        col.clone()
    };

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

/// **P4 PR 2** — Build a pgvector nearest-neighbour search query.
///
/// Emits the canonical pgvector shape (plan §3.1):
///
/// ```sql
/// SELECT *, "<col>" <op> $1::vector AS _distance
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
    schema_hint: Option<&Value>,
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

/// Build the SQL + bind parameters for a full-text search (P4 PR 3 — PG arm).
///
/// Shape:
/// ```sql
/// SELECT *, ts_rank("__fts", plainto_tsquery('pg_catalog.english', $1)) AS _rank
/// FROM "<app>"."<coll>"
/// WHERE "__fts" @@ plainto_tsquery('pg_catalog.english', $1) AND <filter>
/// ORDER BY _rank DESC
/// LIMIT $2
/// ```
///
/// **Language**: we always render `'pg_catalog.english'` here at the
/// builder level — the per-collection `FullTextIndex::ensure_fts_index`
/// call wires the trigger with the schema-declared language, so query-
/// time text decomposition matches the index-time decomposition. A
/// future PR may thread the per-collection language through the builder
/// for non-English schemas; PR 3 deliberately ships only English to keep
/// the wire path narrow (PG itself ships configs for many languages, so
/// the upgrade is one `language: &str` parameter away).
///
/// **Parameter binding**: `$1` is the query text (bound as TEXT, not
/// cast — `plainto_tsquery(regconfig, text)` takes the text verbatim);
/// `$2` is the LIMIT. Filter parameters start at `$3` for the same
/// reason as [`build_vector_search`].
///
/// Pulls in the standard `build_where` helper for filter composition —
/// any operator the rest of the read path supports works inside an FTS
/// query too (`{lang: "en"}`, `{$and: [...]}`, etc.).
pub fn build_fts_search(
    app_id: &str,
    collection: &str,
    query: &str,
    filter: &Value,
    limit: Option<usize>,
    schema_hint: Option<&Value>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let fts_col = quote_ident("__fts");

    // Default LIMIT — 100 is large enough for typical "top results" UIs
    // without dragging the whole table into memory if the caller forgets
    // a `.limit()`.
    let limit = limit.unwrap_or(100);
    validate_search_limit_bound("search.limit", limit)?;

    let mut params: Vec<String> = Vec::with_capacity(2 + 4);
    params.push(query.to_string());
    params.push(limit.to_string());

    let where_clause = build_where(filter, &mut params)?;

    let select_expr = build_masked_aware_select_expr(None, schema_hint)?;
    let mut sql = format!(
        "SELECT {select_expr}, ts_rank({fts_col}, plainto_tsquery('pg_catalog.english', $1)) AS _rank \
         FROM {schema}.{table} \
         WHERE {fts_col} @@ plainto_tsquery('pg_catalog.english', $1)"
    );
    if !where_clause.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(&where_clause);
    }
    sql.push_str(" ORDER BY _rank DESC LIMIT $2");

    Ok(BuiltQuery { sql, params })
}

/// Build the SQL + bind parameters for a spatial within-radius search
/// (P4 PR 3 — PostGIS arm).
///
/// Shape:
/// ```sql
/// SELECT *, ST_Distance("col", ST_MakePoint($1, $2)::geography) AS _distance_m
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
/// `geography(POINT, 4326)`. The PG DDL emitter ([`field_to_column`])
/// wires this when the schema field type is `geoPoint`.
pub fn build_spatial_near(
    app_id: &str,
    collection: &str,
    column: &str,
    point: crate::descriptors::GeoPoint,
    radius_m: f64,
    filter: &Value,
    limit: Option<usize>,
    schema_hint: Option<&Value>,
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
    schema_hint: Option<&Value>,
) -> Result<String, QueryError> {
    validate_clause_budget(filter, ClauseBudgetKind::Having)?;
    build_having_inner(filter, params, agg_exprs, schema_hint)
}

fn build_having_inner(
    filter: &Value,
    params: &mut Vec<String>,
    agg_exprs: &std::collections::HashMap<String, String>,
    schema_hint: Option<&Value>,
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
                        aggregate_read_ident(key, schema_hint)
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
///
/// **Visibility (P4 PR 5)**: lifted from `fn` to `pub` so the
/// SQLite-side `fts.rs` / `spatial.rs` helpers can compose a parametrised
/// predicate fragment against pre-seeded params (`$1` = MATCH query, `$2`
/// = LIMIT, etc.) without rebuilding the filter machinery. The body
/// itself is unchanged — every existing call site keeps its
/// behaviour byte-for-byte.
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

/// Build a condition for a single field.
///
/// **P5.5 PR 1** — runs `validate_field_name` on the filter key so a
/// query like `db.users.find({ ssn_masked: "..." })` is refused with
/// the same `InvalidIdent` error path that DDL-time validation uses.
/// This fences the `_masked` reserved suffix at filter time so the
/// creator can never query against a sibling column through the SDK.
fn build_field_condition(
    field: &str,
    value: &Value,
    params: &mut Vec<String>,
) -> Result<String, QueryError> {
    build_field_condition_with_dialect(field, value, params, SqlDialect::Postgres)
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
                        if arr.len() > MAX_MEMBERSHIP_LIST_LEN {
                            return Err(QueryError::InvalidFilter(format!(
                                "$nin exceeds the maximum of {MAX_MEMBERSHIP_LIST_LEN} values"
                            )));
                        }
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
                        match dialect {
                            SqlDialect::Postgres => format!("{col} ILIKE ${}", params.len()),
                            SqlDialect::Sqlite => {
                                format!("{col} LIKE ${} COLLATE NOCASE", params.len())
                            }
                        }
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
#[cfg(test)]
fn build_order_by(order: &Value) -> Result<String, QueryError> {
    build_order_by_with_dialect(order, SqlDialect::Postgres)
}

fn build_order_by_with_dialect(order: &Value, dialect: SqlDialect) -> Result<String, QueryError> {
    build_order_by_with_validator(order, dialect, validate_field_name)
}

fn build_order_by_read_with_dialect(
    order: &Value,
    dialect: SqlDialect,
    schema_hint: Option<&Value>,
) -> Result<String, QueryError> {
    build_order_by_with_validator(order, dialect, |field| {
        validate_read_identifier(field, schema_hint)
    })
}

fn build_order_by_with_validator<F>(
    order: &Value,
    dialect: SqlDialect,
    mut validate: F,
) -> Result<String, QueryError>
where
    F: FnMut(&str) -> Result<(), QueryError>,
{
    match order {
        Value::Object(map) => {
            let mut parts = Vec::with_capacity(map.len());
            for (key, val) in map {
                validate(key)?;
                let descending = matches!(val.as_i64(), Some(n) if n < 0);
                parts.push(build_order_term(key, descending, dialect));
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
                parts.push(build_order_term(field, descending, dialect));
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
    schema_hint: Option<&Value>,
) -> Result<String, QueryError> {
    let term = |field: &str, descending: bool| -> Result<String, QueryError> {
        if agg_exprs.contains_key(field) {
            Ok(build_order_term_expr(&quote_ident(field), descending, dialect))
        } else {
            validate_read_identifier(field, schema_hint)?;
            Ok(build_order_term_with_schema(field, descending, dialect, schema_hint))
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

/// **SEC-4** — like [`build_order_term`] but substitutes the masked
/// sibling for masked columns, so an aggregate `$sort` (or a `$first`
/// ORDER BY) on a mask-only column never orders by — and thereby leaks
/// the ordering of — the plaintext column.
fn build_order_term_with_schema(
    field: &str,
    descending: bool,
    dialect: SqlDialect,
    schema_hint: Option<&Value>,
) -> String {
    build_order_term_expr(&aggregate_read_ident(field, schema_hint), descending, dialect)
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
    build_upsert_with_dialect(
        app_id,
        collection,
        doc,
        conflict_fields,
        SqlDialect::Postgres,
    )
}

/// Dialect-aware UPSERT builder.
pub fn build_upsert_with_dialect(
    app_id: &str,
    collection: &str,
    doc: &Value,
    conflict_fields: &Value,
    dialect: SqlDialect,
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
    let encrypted_cols = collect_encrypted_cols(obj);

    let mut columns = Vec::new();
    let mut placeholders = Vec::new();
    let mut params: Vec<String> = Vec::new();
    let mut update_clauses = Vec::new();
    let mut doc_has_version = false;
    let mut doc_has_updated_at = false;

    for (key, value) in obj {
        if key.starts_with("__zsenc__") {
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
            let is_encrypted = encrypted_cols.contains(key.as_str());
            let raw = value_to_param(value);
            let param_value = if is_encrypted {
                dialect.wrap_encrypted_param(raw)
            } else {
                raw
            };
            params.push(param_value);
            let n = params.len();
            if is_encrypted {
                placeholders.push(dialect.encrypted_column_bind_placeholder(n));
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
        update_clauses.push(r#""version" = COALESCE("version", 0) + 1"#.to_string());
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
        "INSERT INTO {schema}.{table} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {} RETURNING *",
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
    doc: &Value,
    conflict_fields: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

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
        "INSERT INTO {schema}.{table} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {} RETURNING *, (xmax = 0) AS __created",
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
            None,
            &[],
            false,
            SqlDialect::Sqlite,
        )
        .unwrap();
        assert_eq!(
            q.sql,
            r#"SELECT * FROM "app1"."users" WHERE "name" LIKE $1 COLLATE NOCASE"#
        );
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
    fn insert_many_rejects_oversized_batch_db11() {
        // DB-11: a batch over MAX_INSERT_MANY_BATCH must be rejected by the
        // builder BEFORE allocating the multi-row SQL + param vec. A batch at
        // the cap is accepted.
        let over: Vec<Value> = (0..=MAX_INSERT_MANY_BATCH).map(|i| json!({ "n": i })).collect();
        let err = build_insert_many("app1", "users", &Value::Array(over)).unwrap_err();
        match err {
            QueryError::InvalidFilter(m) => assert!(m.contains("exceeds the maximum"), "{m}"),
            other => panic!("expected InvalidFilter, got {other:?}"),
        }
        let at_cap: Vec<Value> = (0..MAX_INSERT_MANY_BATCH).map(|i| json!({ "n": i })).collect();
        assert!(build_insert_many("app1", "users", &Value::Array(at_cap)).is_ok());
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
    fn distinct_on_masked_field_reads_masked_sibling() {
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
            Some(&schema),
            SqlDialect::Postgres,
        )
        .expect("build distinct with schema");

        assert!(
            q.sql.starts_with(r#"SELECT DISTINCT "email_masked" AS "email" FROM "app1"."users""#),
            "masked distinct must read the sibling column: {}",
            q.sql
        );
        assert!(
            !q.sql.contains(r#"SELECT DISTINCT "email" FROM"#),
            "masked distinct must not read the parent column: {}",
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
    // SEC-4 — aggregation pipeline must NOT leak masked-column plaintext.
    //
    // For a mask-only column (`.mask({...})` without `.encrypted()`),
    // plaintext lives in `<col>` and the masked string in `<col>_masked`.
    // `build_distinct` substitutes the sibling; the aggregate builder used
    // a bare `quote_ident(field)` against the base plaintext column, so
    // `$group.by:"ssn"` / `$max:"ssn"` returned PLAINTEXT. These pin the
    // sibling substitution at the SQL-builder level (BASE column, not the
    // already-rejected `ssn_masked` sibling name).
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
    fn sec4_aggregate_group_by_masked_field_reads_masked_sibling() {
        let pipeline = json!([
            {"$group": {"by": "ssn", "n": {"$count": true}}}
        ]);
        let schema = mask_only_ssn_schema();
        let q = build_aggregate_with_soft_delete_with_dialect(
            "app1", "users", &pipeline, false, Some(&schema), SqlDialect::Postgres,
        )
        .expect("build aggregate with schema");

        assert!(
            q.sql.contains(r#""ssn_masked" AS "ssn""#),
            "SEC-4: $group.by on a masked column must select the masked \
             sibling, not plaintext: {}",
            q.sql
        );
        // The bare plaintext column must not appear in the SELECT or the
        // GROUP BY (the sibling alias `"ssn"` is fine, the bare quoted
        // `"ssn"` projection is not).
        assert!(
            !q.sql.contains(r#"SELECT "ssn","#) && !q.sql.contains(r#"SELECT "ssn" "#),
            "SEC-4: aggregate must not project the bare plaintext ssn column: {}",
            q.sql
        );
        assert!(
            q.sql.contains(r#"GROUP BY "ssn_masked""#),
            "SEC-4: GROUP BY on a masked column must group by the masked \
             sibling: {}",
            q.sql
        );
    }

    #[test]
    fn sec4_aggregate_max_on_masked_field_reads_masked_sibling() {
        let pipeline = json!([
            {"$group": {"by": "tenant", "top": {"$max": "ssn"}}}
        ]);
        let schema = mask_only_ssn_schema();
        let q = build_aggregate_with_soft_delete_with_dialect(
            "app1", "users", &pipeline, false, Some(&schema), SqlDialect::Postgres,
        )
        .expect("build aggregate with schema");

        assert!(
            q.sql.contains(r#"MAX("ssn_masked") AS "top""#),
            "SEC-4: $max on a masked column must aggregate the masked \
             sibling, not plaintext: {}",
            q.sql
        );
        assert!(
            !q.sql.contains(r#"MAX("ssn")"#),
            "SEC-4: $max must not read the bare plaintext ssn column: {}",
            q.sql
        );
    }

    #[test]
    fn sec4_aggregate_sum_min_first_on_masked_field_read_masked_sibling() {
        // $sum / $min / $first all lower a field reference and must each
        // substitute the masked sibling.
        let schema = mask_only_ssn_schema();
        for op in ["$sum", "$min", "$first"] {
            let pipeline = json!([
                {"$group": {"by": "tenant", "v": {op: "ssn"}}}
            ]);
            let q = build_aggregate_with_soft_delete_with_dialect(
                "app1", "users", &pipeline, false, Some(&schema), SqlDialect::Postgres,
            )
            .unwrap_or_else(|e| panic!("build aggregate {op}: {e:?}"));
            assert!(
                q.sql.contains(r#""ssn_masked""#),
                "SEC-4: {op} on a masked column must reference the masked \
                 sibling: {}",
                q.sql
            );
            assert!(
                !q.sql.contains(r#"("ssn")"#) && !q.sql.contains(r#"("ssn" "#),
                "SEC-4: {op} must not read the bare plaintext ssn column: {}",
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
            None,
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
        let q = build_upsert("app1", "users", &doc, &conflict).unwrap();
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

    #[test]
    fn test_upsert_autobumps_version_and_updated_at_when_omitted() {
        let doc = json!({"email": "a@b.com", "name": "alice"});
        let conflict = json!(["email"]);
        let q = build_upsert_with_dialect("app1", "users", &doc, &conflict, SqlDialect::Sqlite)
            .unwrap();
        assert!(
            q.sql.contains(r#""version" = COALESCE("version", 0) + 1"#),
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
            build_upsert_with_dialect("app1", "users", &doc, &conflict, SqlDialect::Postgres)
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
            "__zsenc__ssn": true,
        });
        let conflict = json!(["email"]);
        let q = build_upsert_with_dialect("app1", "users", &doc, &conflict, SqlDialect::Sqlite)
            .unwrap();
        assert!(
            !q.sql.contains("__zsenc__"),
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
                .any(|p| p.starts_with(SQLITE_ENC_BLOB_PREFIX)),
            "SQLite upsert must tag encrypted params for blob binding: {:?}",
            q.params
        );
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

    #[test]
    fn test_find_or_create_emits_xmax_returning() {
        let doc = json!({"email": "a@b.com", "name": "alice"});
        let conflict = json!(["email"]);
        let q = build_find_or_create("app1", "users", &doc, &conflict).unwrap();
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
        assert!(q.sql.contains("RETURNING *"), "sql: {}", q.sql);
        assert_eq!(q.params.len(), 2);
    }

    #[test]
    fn test_find_or_create_rejects_empty_conflict() {
        let doc = json!({"email": "a@b.com"});
        let conflict = json!([]);
        assert!(build_find_or_create("app1", "users", &doc, &conflict).is_err());
    }

    #[test]
    fn test_find_or_create_rejects_empty_doc() {
        let doc = json!({});
        let conflict = json!(["email"]);
        assert!(build_find_or_create("app1", "users", &doc, &conflict).is_err());
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
    // **P7 PR 4** — UPDATE auto-bumps version + updated_at + updated_by
    //
    // The auto-bumps fire only on the new dispatch path (signalled by
    // an `actor_id` being threaded through OR by `skip_*` hints).
    // Direct callers of `build_update_one` / `build_update_many`
    // continue to see the pre-PR-4 single-column auto-bump
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
        // No actor: PR 4 emits no `updated_by` SET clause.
        // Note: with `actor_id = None` AND no `skip_*` flags, the
        // pre-PR-4 fallback applies — only `updated_at` auto-bumps.
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let autobump = SystemFieldAutoBump::default();
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
            &filter,
            &update,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(
            !q.sql.contains(r#""updated_by""#),
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
        // `__zsenc__secret` marks the `secret` column encrypted.
        let update = json!({
            "secret": "ciphertext",
            "__zsenc__secret": true,
        });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
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
            "__zsenc__secret": true,
        });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
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
        // PR 4 — when the filter has `version: N`, the standard
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
        // auto-bump used to hardcode NOW(); PR 4 makes it dialect-aware.
        let filter = json!({ "id": "post_x" });
        let update = json!({ "title": "new" });
        let q = build_update_one_with_dialect(
            "app1",
            "posts",
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
            "__zsenc__ssn": true,
        });
        let autobump = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let q = build_update_one_with_system_fields(
            "app1",
            "posts",
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

    /// The PR 4 auto-bump SQL must NOT carry an `__zsenc__updated_by`
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
        // PR 4 contract: auto-bump SET clauses are appended AFTER
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
        // **P7 PR 3** — TEXT column for the FK (cascades to match the
        // new `id TEXT PRIMARY KEY`; was INTEGER pre-PR 3).
        assert!(sql.contains("\"authorId\" TEXT"), "{sql}");
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
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
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
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        assert!(!sql.contains("DEFERRABLE"), "{sql}");
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
        // FK is deferred — column still present but no FOREIGN KEY clause.
        // **P7 PR 3** — TEXT (was INTEGER pre-PR 3 cascade).
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
    // P7 PR 3 — FK column type cascade (TEXT, was INTEGER pre-PR 3)
    // -----------------------------------------------------------------

    /// `def_to_pg_type` returns TEXT for a ref field so the FK column
    /// matches the new `id TEXT PRIMARY KEY` shape PR 2 introduced.
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
    /// `INTEGER` for the column type post-PR 3. A regression that
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
        // check on the whole sql is safe — pre-PR 3 the substring
        // `"authorId" INTEGER` was present.)
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
    // **P7 PR 1** — `version` is now a reserved system-field name
    // (`SYSTEM_FIELD_NAMES`); the declaration-time validator refuses
    // a creator-declared `version` column. PR 2 will rework
    // `build_create_table_with_fks` to inject the seven system fields
    // directly (not via a creator-shape entry), at which point this
    // test transitions to asserting the system-field emission path.
    // For PR 1 (foundation-only), the test uses a placeholder field
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
    // C2 — discriminated union document shapes (Phase 7)
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
    /// accepted but P5.5 PR 1 reserves the `_` prefix for synthetic-
    /// result columns (`_rank`, `_distance`); see
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
    // P5.5 PR 1 — reserved-name validator
    // -----------------------------------------------------------------

    /// The `_masked` suffix is reserved for Path B sibling columns
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

    /// Reserved-name validator fires at filter time too: a query like
    /// `db.users.find({ ssn_masked: "..." })` is refused via
    /// `build_field_condition` calling `validate_field_name`.
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
        assert!(!is_schema_metadata_key("_rank"));
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
    // P7 PR 1 — platform system-field reservation (declaration-only)
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
    /// break the entire SDK. PR 1's reservation is declaration-only.
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
    // P7 PR 2 — CREATE TABLE prepends 7 system fields + 3 auto-indexes
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
    /// legacy `id SERIAL PRIMARY KEY` that P0 emitted.
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
    /// backends. Auto-bumped by CRUD updates in PR 4.
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
    /// nullability is load-bearing for the find() auto-filter PR 5
    /// will wire (`WHERE deleted_at IS NULL`).
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

    /// `version` is bumped on every UPDATE (PR 4 wires the auto-bump);
    /// an index on it would thrash. Per §5 of the proposal it stays
    /// unindexed.
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
    /// that B2 (P0) FK clauses ride after the column declarations.
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
    /// p5.5 PR 4 sqlite ATTACH alias correction.
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
    /// [`index_name`] helper, so an overlong collection name gets the
    /// sha2 hash truncation at 60 bytes. Regression fence for the
    /// NAMEDATALEN-safety contract from P0.
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
    /// the column list. The PR 1 validator catches creator-declared
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
        // The validator (PR 1) raises `ReservedSystemFieldName` before
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
            validate_field_name_for_declaration("_rank").unwrap_err(),
            QueryError::InvalidIdent(_)
        ));
    }

    // -----------------------------------------------------------------
    // P5 PR 3.5 — dialect-aware encrypted-column bind helpers
    // -----------------------------------------------------------------

    #[test]
    fn dialect_pg_encrypted_placeholder_is_decode_bytea_cast() {
        let p = SqlDialect::Postgres.encrypted_column_bind_placeholder(3);
        assert_eq!(p, "decode($3, 'base64')::bytea");
    }

    #[test]
    fn dialect_sqlite_encrypted_placeholder_is_bare_param() {
        let p = SqlDialect::Sqlite.encrypted_column_bind_placeholder(7);
        assert_eq!(p, "$7");
    }

    #[test]
    fn dialect_pg_wrap_encrypted_param_is_identity() {
        let v = SqlDialect::Postgres.wrap_encrypted_param("abc==".to_string());
        assert_eq!(v, "abc==");
    }

    #[test]
    fn dialect_sqlite_wrap_encrypted_param_prepends_sentinel() {
        let v = SqlDialect::Sqlite.wrap_encrypted_param("abc==".to_string());
        assert_eq!(v, format!("{SQLITE_ENC_BLOB_PREFIX}abc=="));
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
            "__zsenc__ssn": true,
        });
        let bq = build_insert("app1", "users", &doc).expect("build_insert ok");
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
        // base64 (no `__zsenc_blob__:` sentinel on the PG arm).
        assert!(
            bq.params
                .iter()
                .all(|p| !p.starts_with(SQLITE_ENC_BLOB_PREFIX)),
            "PG path must never emit the SQLite blob sentinel: {:?}",
            bq.params,
        );
        // The marker key itself must not leak as a column.
        assert!(
            !bq.sql.contains("__zsenc__"),
            "marker keys must not appear as columns: {}",
            bq.sql,
        );
    }

    /// SQLite-flavour `build_insert_with_dialect` must emit a bare `$N`
    /// placeholder for encrypted columns and tag the param value with
    /// `SQLITE_ENC_BLOB_PREFIX` so the session actor binds raw bytes
    /// as BLOB. The marker key must not surface in the SQL.
    #[test]
    fn build_insert_sqlite_encrypted_column_emits_bare_placeholder() {
        let doc = serde_json::json!({
            "id": "row1",
            "ssn": "Y2lwaGVydGV4dF9ibG9i",
            "__zsenc__ssn": true,
        });
        let bq = build_insert_with_dialect("app1", "users", &doc, SqlDialect::Sqlite)
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
                .any(|p| p.starts_with(SQLITE_ENC_BLOB_PREFIX)),
            "SQLite path must tag the encrypted param with sentinel: {:?}",
            bq.params,
        );
        // The marker key itself must not leak as a column.
        assert!(
            !bq.sql.contains("__zsenc__"),
            "marker keys must not appear as columns: {}",
            bq.sql,
        );
    }

    /// `build_update_one_with_dialect` on the SQLite arm must mirror
    /// the insert path: bare `$N` for encrypted columns + sentinel-
    /// prefixed param. PG behaviour is byte-for-byte identical to PR 2.
    #[test]
    fn build_update_one_sqlite_encrypted_column_sentinel_tag() {
        let filter = serde_json::json!({ "id": "row1" });
        let update = serde_json::json!({
            "ssn": "Y2lwaGVydGV4dF9ibG9i",
            "__zsenc__ssn": true,
        });
        let bq = build_update_one_with_dialect(
            "app1",
            "users",
            &filter,
            &update,
            SqlDialect::Sqlite,
        )
        .expect("build_update_one_with_dialect ok");
        assert!(!bq.sql.contains("decode("), "no PG cast: {}", bq.sql);
        assert!(
            bq.params
                .iter()
                .any(|p| p.starts_with(SQLITE_ENC_BLOB_PREFIX)),
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
            { "id": "row1", "ssn": "Y2lwaGVydGV4dF9ibG9i", "__zsenc__ssn": true },
            { "id": "row2", "ssn": "YW5vdGhlcl9jaXBoZXI=", "__zsenc__ssn": true },
        ]);
        let bq = build_insert_many_with_dialect("app1", "users", &docs, SqlDialect::Sqlite)
            .expect("build_insert_many_with_dialect ok");
        assert!(!bq.sql.contains("decode("), "no PG cast: {}", bq.sql);
        let tagged = bq
            .params
            .iter()
            .filter(|p| p.starts_with(SQLITE_ENC_BLOB_PREFIX))
            .count();
        assert_eq!(tagged, 2, "both encrypted ssn params must be tagged: {:?}", bq.params);
    }

    // -----------------------------------------------------------------
    // P5.5 PR 2 — Path B sibling-column DDL emission
    // -----------------------------------------------------------------

    /// `mask_sibling_column_for_field` returns `Some("<col>_masked")`
    /// for masked columns and `None` for non-masked / kind=none columns.
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

    /// **DDL shape** — masked column emits parent + sibling
    /// `<col>_masked TEXT NOT NULL`.
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
            sql.contains("\"ssn_masked\" TEXT NOT NULL"),
            "expected sibling column with TEXT NOT NULL: {sql}"
        );
        assert!(sql.contains("\"ssn\""), "parent column still present: {sql}");
        assert!(
            !sql.contains("\"name_masked\""),
            "non-masked column must NOT emit sibling: {sql}"
        );
    }

    /// **P5.5 PR 6** — masked column CREATE TABLE emits `COMMENT ON
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
            sql.contains("'__zsmask:kind=last4,classification=spi'"),
            "expected sentinel literal: {sql}"
        );
    }

    /// **P5.5 PR 6** — sibling DDL inline `/* __zsmask:... */` comment
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
            sql.contains("\"email_masked\" TEXT NOT NULL /* __zsmask:kind=email,classification=pii */"),
            "expected inline /* __zsmask:... */ comment on sibling: {sql}"
        );
    }

    /// **P5.5 PR 6** — `kind: "none"` opt-out emits no sibling and no
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

    /// **P5.5 PR 6** — `build_add_column` for a fresh field with a
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
        assert!(sql.contains("ADD COLUMN IF NOT EXISTS \"ssn\""), "parent: {sql}");
        assert!(sql.contains("ADD COLUMN IF NOT EXISTS \"ssn_masked\""), "sibling: {sql}");
        assert!(
            sql.contains("COMMENT ON COLUMN \"app1\".\"users\".\"ssn_masked\""),
            "comment: {sql}"
        );
        assert!(
            sql.contains("'__zsmask:kind=last4,classification=spi'"),
            "sentinel: {sql}"
        );
    }

    /// **P5.5 PR 6** — `build_add_column` for a non-masked field emits
    /// only the single parent ADD; no sibling DDL, no comment.
    #[test]
    fn build_add_column_no_sibling_when_unmasked() {
        let def = serde_json::json!({ "type": "string" });
        let sql = build_add_column("app1", "users", "name", &def).expect("build_add_column ok");
        assert!(!sql.contains("_masked"), "no sibling for unmasked: {sql}");
        assert!(!sql.contains("COMMENT ON COLUMN"), "no comment: {sql}");
    }

    /// **DDL shape** — `t.encrypted(...)` (default-mask path) gets the
    /// sibling because PR 1 auto-populates `mask: {kind: "full", ...}`
    /// on encrypted columns at schema-normalisation time.
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
            sql.contains("\"ssn_masked\" TEXT NOT NULL"),
            "encrypted column with default mask emits sibling: {sql}"
        );
    }

    /// **DDL shape** — `kind: "none"` explicit opt-out → no sibling.
    /// The parent encrypted column behaves like P5 baseline.
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

    /// **Index auto-emit** — `.index()` on the parent masked column
    /// produces a B-tree index on the sibling, NEVER unique.
    #[test]
    fn build_create_indexes_emits_btree_on_sibling_when_parent_indexed() {
        let schema = serde_json::json!({
            "email": {
                "type": "string",
                "index": true,
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        // One index on the parent (B-tree), one on the sibling
        // (B-tree, never unique).
        let sibling_idx: Vec<_> = out
            .iter()
            .filter(|s| s.columns.iter().any(|c| c == "email_masked"))
            .collect();
        assert_eq!(sibling_idx.len(), 1, "expected one sibling index: {out:?}");
        assert!(
            !sibling_idx[0].unique,
            "sibling index must NEVER be UNIQUE: {sibling_idx:?}"
        );
        assert!(
            sibling_idx[0].sql.contains("CREATE INDEX"),
            "sibling index uses CREATE INDEX, not CREATE UNIQUE INDEX: {}",
            sibling_idx[0].sql,
        );
        assert!(
            !sibling_idx[0].sql.contains("UNIQUE"),
            "sibling index DDL must not say UNIQUE: {}",
            sibling_idx[0].sql,
        );
    }

    /// **Index auto-emit** — `.unique()` on the parent still emits a
    /// (non-unique) B-tree index on the sibling alongside the unique
    /// index on the parent.
    #[test]
    fn build_create_indexes_unique_parent_btree_sibling() {
        let schema = serde_json::json!({
            "email": {
                "type": "string",
                "unique": true,
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        let parent_idx = out
            .iter()
            .find(|s| s.columns.iter().any(|c| c == "email"))
            .expect("parent unique index");
        assert!(parent_idx.unique, "parent uniqueness preserved");
        let sibling_idx = out
            .iter()
            .find(|s| s.columns.iter().any(|c| c == "email_masked"))
            .expect("sibling index");
        assert!(
            !sibling_idx.unique,
            "sibling must be non-unique even when parent is unique: {sibling_idx:?}"
        );
    }

    /// **Index auto-emit** — no sibling index when parent has no
    /// `.index()` / `.unique()`.
    #[test]
    fn build_create_indexes_no_sibling_when_parent_not_indexed() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let out = build_create_indexes("app1", "users", &schema).unwrap();
        assert!(
            out.iter().all(|s| !s.columns.iter().any(|c| c == "ssn_masked")),
            "no sibling index when parent isn't indexed: {out:?}"
        );
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
        let bq = build_insert("app1", "users", &doc).expect("build_insert ok");
        assert!(bq.sql.contains("\"ssn\""), "parent column in SQL: {}", bq.sql);
        assert!(
            bq.sql.contains("\"ssn_masked\""),
            "sibling column in SQL: {}",
            bq.sql,
        );
    }

    // -----------------------------------------------------------------
    // P5.5 PR 8 — §11 closeout SELECT-shape gates
    //
    // Three invariants pinned at the SQL-build layer (the production
    // path is `build_find_with_schema` → `build_masked_aware_select_
    // expr_with_unmask`):
    //
    // 1. `default_read_does_not_touch_ciphertext_column` — when a
    //    schema declares a masked column and no `unmask` hint is
    //    passed, the SELECT clause emits `"<col>_masked" AS "<col>"`
    //    and the bare ciphertext column name MUST NOT appear in the
    //    select-list (it appears in the alias's right-hand side only
    //    and not as a top-level select expression).
    // 2. `creator_cannot_query_by_masked_sibling` — `build_where`
    //    refuses filter keys ending in `_masked` because
    //    `validate_field_name` is on the reserved-suffix path. PR 1
    //    pinned this; we double-check the end-to-end path through
    //    `build_find_with_schema` for belt-and-braces.
    // 3. `sibling_masked_column_not_visible_in_sdk_introspection` —
    //    the SDK `Row<S>` shape excludes `<col>_masked`. The Rust-
    //    side dual to that invariant is that callers never need to
    //    PROJECT through `<col>_masked` — the alias substitution
    //    means the SDK sees `<col>` carrying the masked value.
    //    Asserted by ensuring the build emits the sibling under an
    //    `AS "<col>"` alias and never as a bare top-level identifier.
    // -----------------------------------------------------------------

    /// Default-read (no `unmask` hint) for a schema with one masked
    /// column. The SELECT clause must:
    /// - emit `"<col>_masked" AS "<col>"` for the masked column,
    /// - emit `"id"` and other non-masked columns verbatim,
    /// - NEVER name the bare ciphertext column at the top level of
    ///   the select list (only inside the sibling AS-clause).
    #[test]
    fn default_read_does_not_touch_ciphertext_column() {
        let schema = serde_json::json!({
            "ssn":   { "type": "string", "encrypted": { "mode": "randomised" },
                       "mask": { "kind": "last4", "classification": "spi" } },
            "email": { "type": "string" },
            "name":  { "type": "string" },
        });
        let filter = serde_json::json!({ "id": 7 });
        let bq = build_find_with_schema(
            "app1", "users", &filter, Some(1), None, None, None, Some(&schema),
        )
        .expect("build_find_with_schema ok");

        // The masked sibling must appear under an alias mapping to
        // the bare parent name.
        assert!(
            bq.sql.contains("\"ssn_masked\" AS \"ssn\""),
            "expected sibling-AS-parent alias in SELECT: {}",
            bq.sql,
        );

        // The bare ciphertext column must NOT appear as a top-level
        // select expression. The only place it appears is on the
        // right-hand side of the `AS` alias (covered above).
        //
        // We assert via the SELECT-list slice — everything between
        // `SELECT ` and ` FROM `.
        let select_clause = bq
            .sql
            .split_once(" FROM ")
            .map(|(head, _)| head.trim_start_matches("SELECT "))
            .unwrap_or(&bq.sql);
        let select_list: Vec<&str> = select_clause.split(", ").collect();
        for item in &select_list {
            // A top-level bare `"ssn"` is illegal; an aliased
            // `"ssn_masked" AS "ssn"` is fine (the bare `"ssn"`
            // appears in the alias right-hand side, not on its own).
            if item.trim() == "\"ssn\"" {
                panic!(
                    "default-read SELECT must NOT carry bare ciphertext column; select list = {select_list:?}",
                );
            }
        }

        // Non-masked columns ride through verbatim.
        assert!(
            bq.sql.contains("\"email\""),
            "non-masked column missing from SELECT: {}",
            bq.sql,
        );
    }

    /// Default-read with an explicit projection that LISTS the
    /// masked column — the projection is rewritten so the sibling
    /// alias is what hits the wire; the bare parent never appears.
    #[test]
    fn default_read_with_explicit_projection_still_aliases_through_sibling() {
        let schema = serde_json::json!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
        });
        let filter = serde_json::json!({});
        let select = serde_json::json!(["id", "ssn"]);
        let bq = build_find_with_schema(
            "app1", "users", &filter, None, None, None, Some(&select), Some(&schema),
        )
        .expect("build_find_with_schema ok");
        assert!(
            bq.sql.contains("\"ssn_masked\" AS \"ssn\""),
            "explicit projection must still alias through sibling: {}",
            bq.sql,
        );
        // The select expression must NOT have a bare ssn entry alongside.
        let select_clause = bq
            .sql
            .split_once(" FROM ")
            .map(|(head, _)| head.trim_start_matches("SELECT "))
            .unwrap_or(&bq.sql);
        let items: Vec<&str> = select_clause.split(", ").collect();
        for item in &items {
            assert_ne!(
                item.trim(),
                "\"ssn\"",
                "bare ciphertext column appeared in explicit projection: {items:?}",
            );
        }
    }

    /// The `unmask` hint flips the behaviour — the hinted column is
    /// served as the bare parent (the encryption pass will decrypt
    /// the ciphertext on the way out).
    #[test]
    fn unmask_hint_overrides_default_alias() {
        let schema = serde_json::json!({
            "ssn":   { "type": "string", "encrypted": { "mode": "randomised" },
                       "mask": { "kind": "last4", "classification": "spi" } },
            "email": { "type": "string",
                       "mask": { "kind": "full", "classification": "pii" } },
        });
        let filter = serde_json::json!({});
        let unmask: Vec<String> = vec!["ssn".to_string()];
        let bq = build_find_with_schema_and_unmask(
            "app1", "users", &filter, None, None, None, None, Some(&schema), &unmask,
        )
        .expect("build_find_with_schema_and_unmask ok");

        // The unmask-listed column hits the wire bare.
        assert!(
            bq.sql.contains("\"ssn\""),
            "unmasked column must appear bare in SELECT: {}",
            bq.sql,
        );
        // The non-unmasked masked column still aliases through sibling.
        assert!(
            bq.sql.contains("\"email_masked\" AS \"email\""),
            "non-unmasked masked column still routes through sibling: {}",
            bq.sql,
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
            "app1", "users", &filter, None, None, None, None, Some(&schema),
        )
        .expect_err("filter by sibling must be refused");
        let msg = format!("{err}");
        assert!(
            msg.contains("_masked") || msg.contains("reserved"),
            "error must reference the reserved sibling suffix: {msg}",
        );
    }

    /// Composite gate — the alias substitution must happen even
    /// when the masked column is the only column declared. This
    /// covers the `any_masked` short-circuit in
    /// `build_masked_aware_select_expr_with_unmask` (case 2).
    #[test]
    fn implicit_select_expands_to_explicit_when_any_column_masked() {
        let schema = serde_json::json!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
        });
        let filter = serde_json::json!({});
        let bq = build_find_with_schema(
            "app1", "users", &filter, None, None, None, None, Some(&schema),
        )
        .unwrap();
        // SELECT * is never emitted when any column is masked.
        assert!(
            !bq.sql.starts_with("SELECT *"),
            "implicit SELECT must expand to an explicit list when any column is masked: {}",
            bq.sql,
        );
        assert!(bq.sql.contains("\"ssn_masked\" AS \"ssn\""));
        assert!(bq.sql.contains("\"id\""));
    }

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
            Some(&schema),
        )
        .expect("vector search sql");
        assert!(
            !q.sql.starts_with("SELECT *"),
            "vector search must not fall back to SELECT * when masked columns exist: {}",
            q.sql,
        );
        assert!(
            q.sql.contains("\"ssn_masked\" AS \"ssn\""),
            "vector search must read masked sibling: {}",
            q.sql,
        );
    }

    #[test]
    fn fts_search_expands_masked_projection_when_schema_cached() {
        let schema = serde_json::json!({
            "ssn": { "type": "string",
                     "mask": { "kind": "last4", "classification": "spi" } },
            "bio": { "type": "string" },
        });
        let q = build_fts_search(
            "app1",
            "users",
            "alice",
            &serde_json::json!({}),
            Some(10),
            Some(&schema),
        )
        .expect("fts search sql");
        assert!(
            !q.sql.starts_with("SELECT *"),
            "fts search must not fall back to SELECT * when masked columns exist: {}",
            q.sql,
        );
        assert!(
            q.sql.contains("\"ssn_masked\" AS \"ssn\""),
            "fts search must read masked sibling: {}",
            q.sql,
        );
    }

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
            Some(&schema),
        )
        .expect("spatial search sql");
        assert!(
            !q.sql.starts_with("SELECT *"),
            "spatial search must not fall back to SELECT * when masked columns exist: {}",
            q.sql,
        );
        assert!(
            q.sql.contains("\"ssn_masked\" AS \"ssn\""),
            "spatial search must read masked sibling: {}",
            q.sql,
        );
    }

    #[test]
    fn implicit_find_projection_with_schema_avoids_star_and_internal_columns() {
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
            Some(&schema),
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
        assert!(
            !bq.sql.contains("\"__fts\""),
            "implicit projection must not expose internal physical columns: {}",
            bq.sql,
        );
    }

    #[test]
    fn fts_search_with_schema_avoids_star_when_no_fields_are_masked() {
        let schema = serde_json::json!({
            "bio": { "type": "string" },
        });
        let q = build_fts_search(
            "app1",
            "users",
            "rust",
            &serde_json::json!({}),
            Some(10),
            Some(&schema),
        )
        .expect("fts projection");
        assert!(
            !q.sql.starts_with("SELECT *"),
            "schema-backed search must use an allowlisted projection: {}",
            q.sql,
        );
        assert!(
            !q.sql.contains("SELECT *") && !q.sql.contains("\"__fts\" AS"),
            "search projection must not expose internal physical columns: {}",
            q.sql,
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
            Some(&serde_json::json!(["__fts"])),
            Some(&schema),
        )
        .expect_err("select on __fts must be refused");
        assert!(matches!(select_err, QueryError::InvalidIdent(_)));

        let sort_err = build_find_with_schema(
            "app1",
            "users",
            &serde_json::json!({}),
            None,
            None,
            Some(&serde_json::json!({ "ssn_masked": 1 })),
            None,
            Some(&schema),
        )
        .expect_err("sort on masked sibling must be refused");
        assert!(matches!(sort_err, QueryError::InvalidIdent(_)));

        let distinct_err = build_distinct_with_soft_delete_with_dialect(
            "app1",
            "users",
            "__fts",
            &serde_json::json!({}),
            false,
            Some(&schema),
            SqlDialect::Postgres,
        )
        .expect_err("distinct on __fts must be refused");
        assert!(matches!(distinct_err, QueryError::InvalidIdent(_)));

        let aggregate_err = build_aggregate_with_soft_delete_with_dialect(
            "app1",
            "users",
            &serde_json::json!([
                { "$group": { "by": "name", "cnt": { "$count": true } } },
                { "$sort": { "ssn_masked": 1 } }
            ]),
            false,
            Some(&schema),
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
            Some(&schema),
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
            Some(&schema),
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
            Some(&schema),
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
    // P7 PR 5 — soft-delete / restore SQL builders +
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
        assert!(q.sql.ends_with("RETURNING *"));
    }

    #[test]
    fn build_soft_delete_one_no_actor_omits_updated_by_clause() {
        let filter = serde_json::json!({ "id": "post_x" });
        let autobump = SystemFieldAutoBump::default();
        let q = build_soft_delete_one_with_system_fields(
            "app1",
            "posts",
            &filter,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(
            !q.sql.contains("\"updated_by\""),
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
            &filter,
            SqlDialect::Postgres,
            &autobump,
        )
        .unwrap();
        assert!(!q.sql.contains("WHERE ctid ="));
        assert!(q.sql.contains("AND \"deleted_at\" IS NOT NULL"));
        assert!(q.sql.ends_with("RETURNING *"));
    }

    #[test]
    fn build_find_with_soft_delete_flag_appends_filter() {
        let filter = serde_json::json!({ "title": "hi" });
        let q = build_find_with_schema_and_unmask_and_soft_delete(
            "app1", "posts", &filter, None, None, None, None, None, &[], true,
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
            "app1", "posts", &filter, None, None, None, None, None, &[],
        )
        .unwrap();
        let q_new = build_find_with_schema_and_unmask_and_soft_delete(
            "app1", "posts", &filter, None, None, None, None, None, &[], false,
        )
        .unwrap();
        assert_eq!(q_legacy.sql, q_new.sql, "back-compat: identical SQL");
        assert_eq!(q_legacy.params, q_new.params);
    }

    #[test]
    fn build_find_empty_filter_with_soft_delete_flag_emits_lone_predicate() {
        let filter = serde_json::json!({});
        let q = build_find_with_schema_and_unmask_and_soft_delete(
            "app1", "posts", &filter, None, None, None, None, None, &[], true,
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
        let q = build_aggregate_with_soft_delete("app1", "users", &pipeline, true).unwrap();
        assert!(
            q.sql.contains("WHERE ") && q.sql.contains("AND \"deleted_at\" IS NULL"),
            "aggregate WHERE must compose creator $match AND soft-delete: {}",
            q.sql
        );
    }

    #[test]
    fn build_distinct_with_soft_delete_appends_filter() {
        let filter = serde_json::json!({});
        let q = build_distinct_with_soft_delete("app1", "users", "country", &filter, true).unwrap();
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
        let q = build_delete_one_with_dialect("app1", "posts", &filter, SqlDialect::Sqlite)
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
            &filter,
            SqlDialect::Sqlite,
            &SystemFieldAutoBump::default(),
        )
        .unwrap();
        assert!(q.sql.contains("WHERE rowid = (SELECT rowid FROM"));
        assert!(!q.sql.contains("WHERE ctid = (SELECT ctid FROM"));
    }
}
