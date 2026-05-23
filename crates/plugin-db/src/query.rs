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
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFilter(msg) => write!(f, "invalid filter: {msg}"),
            Self::InvalidCollection(msg) => write!(f, "invalid collection: {msg}"),
            Self::InvalidIdent(msg) => write!(f, "invalid identifier: {msg}"),
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
///
/// Additional security constraints (beyond character allowlist):
/// - Must not be empty.
/// - Must not exceed 63 bytes (Postgres `NAMEDATALEN` limit).
/// - Must not contain a null byte.
/// - Must not start with `pg_` (case-insensitive) — reserved for Postgres
///   system catalogs.
/// - Must not start with `__zeroship` (case-insensitive) — reserved for the
///   platform's own internal tables (e.g. `__zeroship_migrations`).
pub(crate) fn validate_collection(name: &str) -> Result<(), QueryError> {
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
pub(crate) fn validate_field_name(name: &str) -> Result<(), QueryError> {
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
pub(crate) fn quote_ident(name: &str) -> String {
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

// pub (not pub(crate)): external consumer tests/integration.rs calls this via glob import.
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
    let mut union_checks: Vec<String> = Vec::new();

    if let Some(obj) = schema.as_object() {
        for (field, def) in obj {
            let col_def = field_to_column(field, def)?;
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

    columns.push("created_at TIMESTAMPTZ DEFAULT NOW()".to_string());
    columns.push("updated_at TIMESTAMPTZ DEFAULT NOW()".to_string());

    // Append all FK clauses *after* the regular columns so the SQL reads
    // top-to-bottom in a natural order (columns, then constraints).
    columns.extend(deferred_fks);
    columns.extend(union_checks);

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
fn normalize_fk_action_inner(s: Option<&str>) -> &'static str {
    match s.unwrap_or("restrict").to_ascii_lowercase().as_str() {
        "cascade" => "CASCADE",
        "set null" | "set_null" => "SET NULL",
        "no action" | "no_action" => "NO ACTION",
        _ => "RESTRICT",
    }
}

/// Normalise an FK action; used cross-module by the diff engine.
pub(crate) fn normalize_fk_action(s: Option<&str>) -> &'static str {
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

    Ok(format!(
        "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {} {}",
        table,
        quote_ident(field),
        pg_type,
        constraints
    ).trim().to_string())
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
/// [`crate::backend::VectorMetric`] — the rustc exhaustiveness check
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
        /// Distance metric — see [`crate::backend::VectorMetric`].
        metric: crate::backend::VectorMetric,
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
                "l2" => crate::backend::VectorMetric::L2,
                "innerProduct" | "ip" => crate::backend::VectorMetric::InnerProduct,
                _ => crate::backend::VectorMetric::Cosine,
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

/// Convert a field definition to a full column definition for CREATE TABLE.
///
/// Validates the field name via [`validate_field_name`] before emitting DDL.
fn field_to_column(field: &str, def: &serde_json::Value) -> Result<String, QueryError> {
    validate_field_name(field)?;
    let pg_type_owned;
    let zs_type = def.get("type").and_then(|t| t.as_str());
    let pg_type: &str = if zs_type == Some("vector") {
        // **P4 PR 2** — pgvector column type is parameterised by dims:
        // `vector(768)`. The SDK validates `vectorDims` is `1..=16000`
        // before sending; we treat a missing field as a schema bug and
        // fall back to bare `vector` (PG will then reject the DDL with
        // a typed error the SDK can surface).
        let dims = def
            .get("vectorDims")
            .and_then(serde_json::Value::as_i64)
            .filter(|d| *d > 0 && *d <= 16000)
            .unwrap_or(0);
        if dims > 0 {
            pg_type_owned = format!("vector({dims})");
            &pg_type_owned
        } else {
            "vector"
        }
    } else if zs_type == Some("geoPoint") {
        // **P4 PR 3** — `t.geoPoint()` materialises as PostGIS
        // `geography(POINT, 4326)`. We hand-code the PG type here rather
        // than wiring it through `def_to_pg_type` because the type is
        // PostGIS-extension-dependent, not a core PG type, and we want
        // the DDL emitter to remain functional regardless of whether
        // PostGIS is installed (the PostGIS probe lives on the runtime
        // `SpatialIndex` path; a registerModel against a non-PostGIS
        // database will fail at CREATE TABLE time with a clear
        // "type geography does not exist" error rather than at
        // index-build time).
        "geography(POINT, 4326)"
    } else {
        def_to_pg_type(def)
    };
    let constraints = def_to_constraints(field, def);
    Ok(format!("{} {} {}", quote_ident(field), pg_type, constraints).trim().to_string())
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
/// B2 — a `ref` field is stored as `BIGINT` so it matches the `id`
/// type of the target collection (auto-generated by `id SERIAL PRIMARY KEY`,
/// which is `INTEGER`; we widen to `BIGINT` because Convex-style brand-typed
/// IDs are always integers wide enough to hold any row count, and the
/// referenced table's `id` is auto-cast on FK check).
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
        Some("ref") => "INTEGER",
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
        // Postgres' text-format param protocol (`query_text_params`,
        // `&[&str]`) cannot represent NULL — an empty string would be
        // encoded as `""`, failing CHECK constraints on enum columns
        // and producing silently-empty TEXT cells. Inline `NULL` as a
        // SQL literal so JSON `null` round-trips faithfully.
        if value.is_null() {
            placeholders.push("NULL".to_string());
        } else {
            params.push(value_to_param(value));
            placeholders.push(format!("${}", params.len()));
        }
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
            // See build_insert: text-format params can't carry NULL;
            // inline as a SQL literal instead.
            if val.is_null() {
                placeholders.push("NULL".to_string());
            } else {
                params.push(value_to_param(val));
                placeholders.push(format!("${}", params.len()));
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
pub(crate) fn build_vector_search(
    app_id: &str,
    collection: &str,
    column: &str,
    query: &[f32],
    k: usize,
    metric: crate::backend::VectorMetric,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    validate_field_name(column)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let col = quote_ident(column);

    // pgvector operator per metric — see `crate::backend::VectorMetric`
    // doc-comment for the operator/opclass mapping.
    let op = match metric {
        crate::backend::VectorMetric::Cosine => "<=>",
        crate::backend::VectorMetric::L2 => "<->",
        crate::backend::VectorMetric::InnerProduct => "<#>",
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

    let mut sql = format!(
        "SELECT *, {col} {op} $1::vector AS _distance FROM {schema}.{table}"
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
pub(crate) fn build_fts_search(
    app_id: &str,
    collection: &str,
    query: &str,
    filter: &Value,
    limit: Option<usize>,
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

    let mut params: Vec<String> = Vec::with_capacity(2 + 4);
    params.push(query.to_string());
    params.push(limit.to_string());

    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!(
        "SELECT *, ts_rank({fts_col}, plainto_tsquery('pg_catalog.english', $1)) AS _rank \
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
/// The Rust trait surface ([`crate::backend::GeoPoint`]) keeps the
/// `{lat, lng}` shape; the swap happens here at the SQL boundary so the
/// JS/Rust contract stays in `(lat, lng)` order. `$3 = radius_m`,
/// `$4 = limit`. Filter parameters start at `$5`.
///
/// **Column type**: the indexed column must be
/// `geography(POINT, 4326)`. The PG DDL emitter ([`field_to_column`])
/// wires this when the schema field type is `geoPoint`.
pub(crate) fn build_spatial_near(
    app_id: &str,
    collection: &str,
    column: &str,
    point: crate::backend::GeoPoint,
    radius_m: f64,
    filter: &Value,
    limit: Option<usize>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;
    validate_field_name(column)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let col = quote_ident(column);

    let limit = limit.unwrap_or(100);

    // Bind order: (lng, lat, radius_m, limit). Note the swap: ST_MakePoint
    // takes (x, y) = (lng, lat), the inverse of the SDK's {lat, lng}
    // input shape.
    let mut params: Vec<String> = Vec::with_capacity(4 + 4);
    params.push(point.lng.to_string());
    params.push(point.lat.to_string());
    params.push(radius_m.to_string());
    params.push(limit.to_string());

    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!(
        "SELECT *, ST_Distance({col}, ST_MakePoint($1, $2)::geography) AS _distance_m \
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
///
/// **Visibility (P4 PR 5)**: lifted from `fn` to `pub(crate)` so the
/// SQLite-side `fts.rs` / `spatial.rs` helpers can compose a parametrised
/// predicate fragment against pre-seeded params (`$1` = MATCH query, `$2`
/// = LIMIT, etc.) without rebuilding the filter machinery. The body
/// itself is unchanged — every existing call site keeps its
/// behaviour byte-for-byte.
pub(crate) fn build_where(filter: &Value, params: &mut Vec<String>) -> Result<String, QueryError> {
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
pub(crate) fn value_to_param(value: &Value) -> String {
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
        let sql = build_create_table_with_fks("app1", "users", &schema, &FkEmission::Inline).unwrap();
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
    // -----------------------------------------------------------------

    #[test]
    fn d4_version_column_default_one() {
        let schema = json!({
            "title": { "type": "string", "required": true },
            "version": { "type": "number", "default": 1 },
        });
        let sql = build_create_table_with_fks("app1", "posts", &schema, &FkEmission::Inline).unwrap();
        assert!(sql.contains("\"version\" DOUBLE PRECISION"), "{sql}");
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
    /// accepts: alphanumeric + underscore.
    #[test]
    fn validate_field_name_accepts_ascii_allowlist() {
        for name in &["id", "user_id", "createdAt", "v2", "_private"] {
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
}
