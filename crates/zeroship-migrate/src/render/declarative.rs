//! Declarative schema-as-code: desired-schema → generated migrations
//! (v3 Plan A, phases P0–P2).
//!
//! The platform's authoring layer holds a creator's **declared schema** — the
//! per-collection descriptor JSON the `@zeroship/db` SDK emits via `registerModel`
//! (`{ _meta, _indexes, <field>: { type, required, unique, default, ref } }`). This
//! module turns that declared schema into a deterministic [`SchemaSnapshot`]
//! ([`desired_snapshot`], P0) and then **diffs** it against the live snapshot to
//! generate migrations ([`DeclarativeAuthor::diff`], P1 additive + P2
//! destructive-gated).
//!
//! The differ is a new **author**, not a new executor: every [`Migration`] it
//! produces still flows through the unchanged
//! [`plan`](crate::engine::MigrationEngine::plan) →
//! [`guard`](crate::guard::SqlGuard) →
//! [`gate`](crate::engine::MigrationEngine::apply) →
//! [`executor::apply`](crate::apply::executor::apply) pipeline. There is no DDL bypass.
//!
//! # Trust boundary
//!
//! Descriptor field/table names and types are **untrusted** (a prompt-injectable
//! AI authored them). They are validated at the author boundary
//! ([`validate_ident`] / [`validate_type`], mirroring
//! [`crate::render::expand_contract`]) AND re-checked by the guard as the second line.
//!
//! # Type-mapping provenance (shared-truth-to-extract-later)
//!
//! The DSL-type → Postgres-type table here is **replicated** from
//! `crates/plugin-db/src/query.rs` (`def_to_pg_type` /
//! `def_to_column_type_for_dialect`) and the platform system-field set
//! (`build_system_field_columns`). It is duplicated *deliberately*:
//! `zeroship-migrate` and `plugin-db` are different trust domains and the migrate
//! crate must not depend on the runtime plugin. The shared vocabulary should be
//! lifted into a small shared crate later; until then the
//! [`desired_snapshot`]-round-trips-to-live test (`tests/declarative_pg.rs`) is
//! the guard against the two copies drifting apart.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Deserialize;

use crate::model::migration::{Checksum, Migration, MigrationFlags, MigrationId};
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, GeneratedColumnSnapshot, IndexElementSnapshot,
    IndexSnapshot, SchemaSnapshot, TableSnapshot,
};
use crate::model::ir::TableRuntimeOptions;
use crate::render::expand_contract::{
    ExpandContractAuthor, ExpandContractError, ExpandContractPlan, OnlineIntent,
};
use crate::render::plan::SqliteRebuildSpec;
use crate::render::renderer::{Capability, DialectSupports};
use zeroship_schema::query::{SqlDialect, SqliteEmitScope};

fn mysql_quote_ident(ident: &str) -> String {
    zeroship_schema::query::mysql_quote_ident(ident)
}

fn mysql_qualified(schema: &str, object: &str) -> String {
    format!("{}.{}", mysql_quote_ident(schema), mysql_quote_ident(object))
}

/// Quote a Postgres identifier (double embedded quotes, wrap in `"`). Mirrors
/// [`crate::plan::author`]'s quoting so emitted SQL is injection-safe even past the
/// author-boundary `validate_ident` (defense in depth — the guard is line two).
fn quote_ident(ident: &str) -> String {
    crate::render::dml::escape_quote_ident(ident)
}

/// The PG keywords whose category is NOT `UNRESERVED` (i.e. reserved,
/// type/function-name, or column-name keywords). `quote_identifier` — and thus
/// `pg_get_constraintdef` — wraps an identifier in double quotes iff it is not a
/// "safe" bare identifier OR it collides with one of THESE keywords (an unreserved
/// keyword is rendered bare). Sourced from `pg_get_keywords() WHERE catcode<>'U'`
/// on PG 17. Used by [`quote_ident_if_needed`] so the FK referenced-table body we
/// build matches the live catalog byte-for-byte (§review LOW): a table/schema named
/// `order`/`user`/`select` (each passes `validate_collection`/`is_safe_schema_ident`
/// but is reserved) renders QUOTED in the catalog — and now here too — so the
/// desired-vs-live FK body re-diffs clean instead of phantom-dropping.
const PG_NON_UNRESERVED_KEYWORDS: &[&str] = &[
    "all", "analyse", "analyze", "and", "any", "array", "as", "asc", "asymmetric",
    "authorization", "between", "bigint", "binary", "bit", "boolean", "both", "case",
    "cast", "char", "character", "check", "coalesce", "collate", "collation", "column",
    "concurrently", "constraint", "create", "cross", "current_catalog", "current_date",
    "current_role", "current_schema", "current_time", "current_timestamp",
    "current_user", "dec", "decimal", "default", "deferrable", "desc", "distinct", "do",
    "else", "end", "except", "exists", "extract", "false", "fetch", "float", "for",
    "foreign", "freeze", "from", "full", "grant", "greatest", "group", "grouping",
    "having", "ilike", "in", "initially", "inner", "inout", "int", "integer",
    "intersect", "interval", "into", "is", "isnull", "join", "json_array",
    "json_arrayagg", "json_object", "json_objectagg", "lateral", "leading", "least",
    "left", "like", "limit", "localtime", "localtimestamp", "national", "natural",
    "nchar", "none", "normalize", "not", "notnull", "null", "nullif", "numeric",
    "offset", "on", "only", "or", "order", "out", "outer", "overlaps", "overlay",
    "placing", "position", "precision", "primary", "real", "references", "returning",
    "right", "row", "select", "session_user", "setof", "similar", "smallint", "some",
    "substring", "symmetric", "system_user", "table", "tablesample", "then", "time",
    "timestamp", "to", "trailing", "treat", "trim", "true", "union", "unique", "user",
    "using", "values", "varchar", "variadic", "verbose", "when", "where", "window",
    "with", "xmlattributes", "xmlconcat", "xmlelement", "xmlexists", "xmlforest",
    "xmlnamespaces", "xmlparse", "xmlpi", "xmlroot", "xmlserialize", "xmltable",
];

/// Quote an identifier ONLY when Postgres' own `quote_identifier` would — i.e.
/// mirror what `pg_get_constraintdef` emits. An identifier is left BARE iff it is a
/// "safe" lowercase identifier (starts with `[a-z_]`, all chars `[a-z0-9_]`) AND is
/// not a non-unreserved keyword ([`PG_NON_UNRESERVED_KEYWORDS`]); otherwise it is
/// double-quoted (mixed-case, leading digit, reserved word, …).
///
/// This is the seam the FK referenced-table body uses so the desired snapshot
/// round-trips byte-for-byte against the live `pg_get_constraintdef` output
/// (unconditional [`quote_ident`] would over-quote a normal lowercase name like
/// `parent` → `"parent"`, which the catalog renders bare → a phantom FK re-create on
/// every diff). It also closes the latent injection/wrong-resolution seam: a
/// reserved-word or mixed-case schema/target now renders quoted (correct
/// resolution), not as a bare keyword.
pub(crate) fn quote_ident_if_needed(ident: &str) -> String {
    let safe_bare = !ident.is_empty()
        && ident.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && ident.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !PG_NON_UNRESERVED_KEYWORDS.contains(&ident);
    if safe_bare {
        ident.to_string()
    } else {
        quote_ident(ident)
    }
}

/// Spell a `pg_get_constraintdef`-matching column list for a UNIQUE / PRIMARY KEY
/// constraint `definition` body — `<col>, <col>, …` with CONDITIONAL per-column
/// quoting ([`quote_ident_if_needed`]: bare for a safe lowercase ident, double-
/// quoted for reserved/mixed-case). This is the SINGLE source of the constraintdef
/// body spelling: BOTH the offline fold ([`crate::fold`]) and the IR lower's
/// snapshot half ([`crate::render::lower`]) consume it, so the folded and the
/// lower-emitted UNIQUE/PK `definition` cannot drift (an unconditional quote would
/// phantom-diff `UNIQUE ("handle")` against the catalog's `UNIQUE (handle)`).
pub(crate) fn constraintdef_cols(cols: &[String]) -> String {
    cols.iter().map(|c| quote_ident_if_needed(c)).collect::<Vec<_>>().join(", ")
}

/// True iff `b` is a SQL identifier byte (so a whole-word scan does not match a
/// substring of a larger identifier). ASCII alphanumerics + `_` + `$`. A
/// double-quote is NOT an identifier byte, so `"col"` boundaries match a word.
const fn is_sql_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Count whole-word, case-insensitive occurrences of `needle` in `haystack`.
/// Used by the H1 DROP-COLUMN rebuild router to find references to a dropped
/// column in the stored `CREATE TABLE` DDL (CHECK / generated / partial-index
/// expressions). A match is whole-word so `id` does not match `idx` or `user_id`.
/// Empty `needle` counts zero.
fn word_count_ci(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let hay = haystack.as_bytes();
    let need = needle.as_bytes();
    let mut count = 0;
    let mut start = 0;
    let lower_hay = haystack.to_ascii_lowercase();
    let lower_need = needle.to_ascii_lowercase();
    let lh = lower_hay.as_bytes();
    let ln = lower_need.as_bytes();
    while let Some(pos) = find_sub(&lh[start..], ln) {
        let abs = start + pos;
        let before = abs.checked_sub(1).map(|p| hay[p]);
        let after = hay.get(abs + need.len()).copied();
        let ok_before = before.is_none_or(|b| !is_sql_ident_byte(b));
        let ok_after = after.is_none_or(|b| !is_sql_ident_byte(b));
        if ok_before && ok_after {
            count += 1;
        }
        start = abs + 1;
    }
    count
}

/// True iff `needle` appears as a whole word (case-insensitive) in `haystack`.
fn word_present_ci(haystack: &str, needle: &str) -> bool {
    word_count_ci(haystack, needle) > 0
}

/// First byte offset of `needle` in `haystack` (plain substring search).
fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Sentinel prefix on a [`ColumnSnapshot::default`] marking a STORED generated
/// column (T12: the `__fts` tsvector). When the `default` body starts with this
/// prefix, the emitter writes `GENERATED ALWAYS AS (<expr>) STORED` instead of a
/// plain `DEFAULT <expr>` clause. The remainder after the prefix is the
/// generation expression. Generated-column expressions are emission-only metadata
/// (excluded from `ColumnSnapshot` equality), so this never participates in drift.
const GENERATED_PREFIX: &str = "GENERATED:";

/// Render a column's trailing `DEFAULT <expr>` or `GENERATED ALWAYS AS (<expr>)
/// STORED` clause from its (emission-only) `default` body. Empty string when the
/// column has no default. A `GENERATED:`-prefixed body becomes the stored
/// generated-column clause (T12 `__fts`); any other body is a plain default.
fn default_clause(default: Option<&str>) -> String {
    match default {
        Some(d) => {
            if let Some(expr) = d.strip_prefix(GENERATED_PREFIX) {
                format!(" GENERATED ALWAYS AS ({expr}) STORED")
            } else {
                format!(" DEFAULT {d}")
            }
        }
        None => String::new(),
    }
}

fn generated_clause(generated: Option<&GeneratedColumnSnapshot>) -> String {
    match generated {
        Some(g) => {
            let storage = if g.stored { "STORED" } else { "VIRTUAL" };
            format!(" GENERATED ALWAYS AS ({}) {storage}", g.expr)
        }
        None => String::new(),
    }
}

fn mysql_generated_clause(generated: Option<&GeneratedColumnSnapshot>) -> String {
    generated_clause(generated)
}

fn pg_identity_clause(c: &ColumnSnapshot) -> String {
    match c.identity {
        Some(identity) if identity.always => " GENERATED ALWAYS AS IDENTITY".to_string(),
        Some(_) => " GENERATED BY DEFAULT AS IDENTITY".to_string(),
        None => String::new(),
    }
}

fn mysql_identity_clause(c: &ColumnSnapshot) -> &'static str {
    if c.identity.is_some() {
        " AUTO_INCREMENT"
    } else {
        ""
    }
}

fn sqlite_identity_pk(c: &ColumnSnapshot, inline_pk: bool) -> bool {
    c.identity.is_some()
        && inline_pk
        && matches!(
            c.data_type.to_ascii_lowercase().as_str(),
            "integer" | "bigint" | "int" | "int4" | "int8"
        )
}

fn column_type_for_render(c: &ColumnSnapshot, dialect: SqlDialect, inline_pk: bool) -> String {
    if let Some(ty) = &c.ddl_type_override {
        ty.clone()
    } else if matches!(dialect, SqlDialect::Sqlite) && sqlite_identity_pk(c, inline_pk) {
        "INTEGER".to_string()
    } else if matches!(dialect, SqlDialect::Sqlite) {
        sqlite_ddl_type(&c.data_type).to_string()
    } else if matches!(dialect, SqlDialect::Mysql) {
        mysql_ddl_type(&c.data_type).to_string()
    } else {
        ddl_type(&c.data_type).to_string()
    }
}

fn inline_checks_clause(c: &ColumnSnapshot) -> String {
    if c.inline_checks.is_empty() {
        String::new()
    } else {
        format!(" {}", c.inline_checks.join(" "))
    }
}

fn mysql_ddl_type(data_type: &str) -> String {
    let lower = data_type.trim().to_ascii_lowercase();
    if lower.starts_with("enum(") {
        return data_type.to_string();
    }
    if lower.starts_with("vector(") {
        return "BLOB".to_string();
    }
    match lower.as_str() {
        "text" => "VARCHAR(191)".to_string(),
        "double precision" | "real" | "float8" => "DOUBLE".to_string(),
        "boolean" => "TINYINT(1)".to_string(),
        "timestamp with time zone" | "timestamptz" => "DATETIME(6)".to_string(),
        "date" => "DATE".to_string(),
        "jsonb" | "json" => "JSON".to_string(),
        "bytea" | "blob" => "LONGBLOB".to_string(),
        "numeric" | "decimal" => "DECIMAL(65, 30)".to_string(),
        "integer" | "int" | "int4" => "INT".to_string(),
        "bigint" | "int8" => "BIGINT".to_string(),
        "geography(point, 4326)" | "geography(POINT, 4326)" => "POINT SRID 4326".to_string(),
        other => other.to_string(),
    }
}

fn sqlite_ddl_type(data_type: &str) -> &'static str {
    match data_type.to_ascii_lowercase().as_str() {
        "integer" | "int" | "int4" | "bigint" | "int8" => "INTEGER",
        "real" | "double precision" | "numeric" => "REAL",
        "bytea" | "blob" | "geography(point, 4326)" => "BLOB",
        "boolean" => "INTEGER",
        _ => "TEXT",
    }
}

fn primary_key_clause(c: &ColumnSnapshot, dialect: SqlDialect, inline_pk: bool) -> &'static str {
    if matches!(dialect, SqlDialect::Sqlite) && sqlite_identity_pk(c, inline_pk) {
        " PRIMARY KEY AUTOINCREMENT"
    } else if inline_pk {
        " PRIMARY KEY"
    } else {
        ""
    }
}

fn null_clause(c: &ColumnSnapshot, dialect: SqlDialect, inline_pk: bool) -> &'static str {
    if c.nullable
        || (matches!(dialect, SqlDialect::Sqlite) && sqlite_identity_pk(c, inline_pk))
        || (matches!(dialect, SqlDialect::Mysql) && c.identity.is_some())
    {
        ""
    } else {
        " NOT NULL"
    }
}

fn has_generated_or_identity(t: &TableSnapshot) -> bool {
    t.columns
        .iter()
        .any(|c| c.generated.is_some() || c.identity.is_some())
}

fn has_inline_checks(t: &TableSnapshot) -> bool {
    t.columns.iter().any(|c| !c.inline_checks.is_empty())
}

fn render_index_elements_pg(idx: &IndexSnapshot, opclass_suffix: &str) -> String {
    let elements = if idx.elements.is_empty() {
        idx.columns
            .iter()
            .map(|c| IndexElementSnapshot::column(c.clone()))
            .collect::<Vec<_>>()
    } else {
        idx.elements.clone()
    };
    elements
        .iter()
        .map(|element| match element {
            IndexElementSnapshot::Column(c) => format!("{}{opclass_suffix}", quote_ident(c)),
            IndexElementSnapshot::Expr(expr) => format!("({expr})"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_index_elements_sqlite(idx: &IndexSnapshot) -> String {
    let elements = if idx.elements.is_empty() {
        idx.columns
            .iter()
            .map(|c| IndexElementSnapshot::column(c.clone()))
            .collect::<Vec<_>>()
    } else {
        idx.elements.clone()
    };
    elements
        .iter()
        .map(|element| match element {
            IndexElementSnapshot::Column(c) => quote_ident(c),
            IndexElementSnapshot::Expr(expr) => format!("({expr})"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_index_elements_mysql(idx: &IndexSnapshot) -> String {
    let elements = if idx.elements.is_empty() {
        idx.columns
            .iter()
            .map(|c| IndexElementSnapshot::column(c.clone()))
            .collect::<Vec<_>>()
    } else {
        idx.elements.clone()
    };
    elements
        .iter()
        .map(|element| match element {
            IndexElementSnapshot::Column(c) => mysql_quote_ident(c),
            IndexElementSnapshot::Expr(expr) => format!("({expr})"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A lowered migration paired with its STRUCTURAL per-statement list — the exact
/// statements whose `join(";\n")` is the migration's `up`. The IR guard-per-
/// statement lower ([`crate::render::lower::IrAuthor::lower_guarded`]) guards each TRUE
/// statement and asserts the reassembly invariant `join(statements) == up`
/// STRUCTURALLY, so it never re-splits the `up` on a textual `;\n` — a string-
/// literal column DEFAULT whose value itself contains `;\n` (e.g. `DEFAULT 'a;\nb'`)
/// stays inside its one statement. Single-statement migrations carry `[up]`.
pub(crate) type LoweredUnit = (Migration, Vec<String>);

/// Wrap a single-statement migration as a [`LoweredUnit`]: the statement list is
/// exactly `[up]` (the canonical `up` is one indivisible statement). Used by every
/// `lower_*` that renders a lone `CREATE` / `ALTER` / `DROP` with no follow-on
/// statement.
pub(crate) fn single_stmt(mig: Migration) -> LoweredUnit {
    let up = mig.up.clone();
    (mig, vec![up])
}

// ---------------------------------------------------------------------------
// Input contract — the per-collection declared-schema descriptor.
// ---------------------------------------------------------------------------

/// One field of a collection, as the `registerModel` descriptor declares it
/// (`{ type, required, unique, default, ref }`).
///
/// Untrusted: `name` and `ty` are validated at the author boundary before any
/// SQL is emitted (see [`DeclarativeAuthor::diff`]).
// NOTE: `PartialEq` but NOT `Eq` — `min`/`max` are `f64` (no total order /
// `Eq`). Descriptor equality is only used in tests; the differ compares
// SNAPSHOTS, not descriptors.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct FieldDescriptor {
    /// The field (column) name.
    pub name: String,
    /// The DSL type token (`string`, `number`, `boolean`, `date`,
    /// `calendarDate`, `json`, `object`, `array`, `union`, `ref`, `bytes`,
    /// `actor`, `id`). See [`dsl_to_pg_data_type`].
    #[serde(rename = "type")]
    pub ty: String,
    /// `true` ⇒ the column is `NOT NULL`.
    #[serde(default)]
    pub required: bool,
    /// `true` ⇒ a unique index is declared over this column. (Materialised as a
    /// `CREATE UNIQUE INDEX`, mirroring the SDK's A1 rule — never an inline
    /// `UNIQUE`.)
    #[serde(default)]
    pub unique: bool,
    /// For a `ref` field, the referenced collection (FK target table). `None`
    /// for non-`ref` fields.
    #[serde(rename = "ref", default)]
    pub references: Option<String>,
    /// `ref` ON DELETE policy (`restrict` | `cascade` | `set null` | `no action`).
    /// `None` ⇒ the SDK default `restrict` (`onDelete` on the wire `FieldDef`).
    /// Mirrors plugin-db's `normalize_fk_action`: anything unrecognised folds to
    /// `RESTRICT`.
    #[serde(rename = "onDelete", default)]
    pub on_delete: Option<String>,
    /// `ref` ON UPDATE policy. `None` ⇒ the SDK default `restrict` (`onUpdate`).
    #[serde(rename = "onUpdate", default)]
    pub on_update: Option<String>,
    /// Whether the FK is emitted `DEFERRABLE INITIALLY DEFERRED`. `None` ⇒ the SDK
    /// default `true` (`deferrable` on the wire `FieldDef`). Mirrors plugin-db's
    /// `build_fk_clause`, which defaults `deferrable` to `true`.
    #[serde(default)]
    pub deferrable: Option<bool>,
    /// For a `literal` field (#3), the single accepted value (`literalValue` on the
    /// wire `FieldDef`). Drives both the column's primitive type
    /// (text/numeric/boolean — see `literal_pg_data_type`) and a
    /// `CHECK (<col> = <value>)` constraint (mirrors plugin-db's
    /// `query.rs:2091/2185`).
    #[serde(rename = "literalValue", default)]
    pub literal_value: Option<serde_json::Value>,
    /// Column `DEFAULT` value (#4, `default` on the wire `FieldDef`). Emitted in the
    /// column declaration per plugin-db's `def_to_constraints` (`query.rs:2125`).
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// Minimum (numeric `min`, #4) — emits a `CHECK (<col> >= <min>)` (or combined
    /// with `max`). Mirrors plugin-db `query.rs:2167`.
    #[serde(default)]
    pub min: Option<f64>,
    /// Maximum (numeric `max`, #4) — emits a `CHECK (<col> <= <max>)`.
    #[serde(default)]
    pub max: Option<f64>,
    /// Enum membership (#4, `enum` on the wire `FieldDef`) — emits a
    /// `CHECK (<col> IN (…))`. String or numeric values, mirroring plugin-db
    /// `query.rs:2200`.
    #[serde(rename = "enum", default)]
    pub enum_values: Option<Vec<serde_json::Value>>,
    /// For a `{ type: "id", idPrefix }` field (#5), the declared typed-id prefix
    /// (`idPrefix` on the wire `FieldDef`). A re-declaration of the system `id` PK
    /// — it FOLDS into the existing `id TEXT PRIMARY KEY` (NOT a second column),
    /// and the prefix is validated (mirrors plugin-db `query.rs:648-653` +
    /// `validate_id_prefix`).
    #[serde(rename = "idPrefix", default)]
    pub id_prefix: Option<String>,

    // -----------------------------------------------------------------------
    // Schema-authority P2 — the FULL-capability facets, reached by adopting the
    // shared `zeroship-schema` DDL/type kernel. Before P2 the engine's v1-subset
    // differ REJECTED these as `UnsupportedType`; now the column TYPE + DDL
    // (vector index, encrypted BYTEA + sentinel, mask sibling, geoPoint geography
    // + GiST) are resolved through `zeroship_schema::query`. Each facet mirrors
    // the SDK `FieldDef` sub-object verbatim so the engine builds the same `def`
    // JSON the SDK emits and the shared kernel maps it identically.
    /// `t.vector(dims, …)` — vector dimensionality. `Some(N)` ⇒ the column is
    /// `vector(N)` (pgvector). Mirrors `vectorDims` on the wire `FieldDef`.
    #[serde(rename = "vectorDims", default)]
    pub vector_dims: Option<i64>,
    /// `t.vector(_, { metric })` — distance metric (`cosine` | `l2` |
    /// `innerProduct`), drives the ivfflat opclass. Mirrors `vectorMetric`.
    #[serde(rename = "vectorMetric", default)]
    pub vector_metric: Option<String>,
    /// `t.encrypted({ mode, keyId, wraps })` — the encryption sub-object,
    /// carried VERBATIM. When present the column DDLs to `BYTEA` with the inline
    /// `/* zsenc:mode:keyId:wraps */` sentinel (the contract plugin-db reads at
    /// runtime). Mirrors `encrypted` on the wire `FieldDef`.
    #[serde(default)]
    pub encrypted: Option<serde_json::Value>,
    /// `.mask({ kind, classification })` — the mask sub-object, carried
    /// VERBATIM. When present the table gains a hidden `<col>_masked TEXT` sibling
    /// + a `COMMENT … __zsmask:…` sentinel. Mirrors `mask` on the wire `FieldDef`.
    #[serde(default)]
    pub mask: Option<serde_json::Value>,
    /// `t.string().fts(language?)` — `true` ⇒ this text column participates in the
    /// collection's composite full-text index (T12). Every `.fts()`-marked column
    /// folds into ONE `__fts` GENERATED tsvector column + a `<coll>__fts_idx` GIN
    /// index. Mirrors `fts` on the wire `FieldDef`.
    #[serde(default)]
    pub fts: bool,
    /// `t.string().fts(language)` — the tsvector configuration token (`english`,
    /// `simple`, …). The collection's FTS index uses the first non-empty language
    /// among its `.fts()` fields, else `english`. Mirrors `ftsLanguage` on the
    /// wire `FieldDef`.
    #[serde(rename = "ftsLanguage", default)]
    pub fts_language: Option<String>,
    /// A generated/computed column facet. The expression is structured IR, never
    /// raw SQL. Mirrors `generated` on the migrate FieldDef bridge.
    #[serde(default)]
    pub generated: Option<crate::model::ir::GeneratedCol>,
    /// A SQL identity column facet. Mirrors `identity` on the migrate FieldDef
    /// bridge.
    #[serde(default)]
    pub identity: Option<crate::model::ir::IdentityCol>,
}

/// One declared index of a collection (the `_indexes` array entry).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IndexDescriptor {
    /// The index name (already collision-stable from the SDK).
    pub name: String,
    /// The columns the index covers, in order.
    pub columns: Vec<String>,
    /// `true` ⇒ a unique index.
    #[serde(default)]
    pub unique: bool,
}

/// A per-collection declared-schema descriptor (one table).
///
/// Mirrors the `registerModel` JSON the SDK emits, parsed into a typed shape:
/// `{ _meta, _indexes:[…], <field>:{…} }`. The `_meta` slot is opaque metadata
/// the migrate crate does not consume (it carries soft-delete / versioning flags
/// the SDK already expanded into concrete fields before this point).
// `PartialEq` but NOT `Eq`: contains `Vec<FieldDescriptor>`, whose `f64`
// min/max are not `Eq`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CollectionDescriptor {
    /// The collection (table) name.
    pub name: String,
    /// The **declaring** app (`app_…`) — the app whose schema authoring input
    /// declared this table. Per the project-umbrella model (design §4) a project
    /// db schema is the UNION of all member apps' descriptors, and the declaring
    /// app **owns** that table's migrations: only the owner may CREATE/ALTER/DROP
    /// it (enforced in [`DeclarativeAuthor::diff`] via the deploying-app context);
    /// a non-declaring app may USE the table's rows freely.
    ///
    /// Ownership is NOT spoofable across apps: an app can only set `owner_app` to
    /// itself in its OWN descriptor set, and a conflicting claim (two apps
    /// declaring the same table with DIFFERENT shapes) is a hard
    /// [`DeclarativeError::ConflictingDeclaration`]. An IDENTICAL re-declaration is
    /// idempotent (design §4) and, to keep the union order-independent, the
    /// retained owner is the lexicographically-smallest declaring app among the
    /// identical declarers (see [`desired_snapshot`]).
    pub owner_app: String,
    /// The declared fields (columns), excluding platform system fields (those
    /// are injected by [`desired_snapshot`], matching the SDK's behaviour).
    #[serde(default)]
    pub fields: Vec<FieldDescriptor>,
    /// The declared named indexes (`_indexes`).
    #[serde(default)]
    pub indexes: Vec<IndexDescriptor>,
    /// Collection-level runtime options that do not round-trip through physical
    /// catalog state.
    #[serde(rename = "runtimeOptions", default)]
    pub runtime_options: TableRuntimeOptions,
}

// ---------------------------------------------------------------------------
// P3 — rename hints (the OPT-IN, never-heuristic rename surface).
// ---------------------------------------------------------------------------

/// An **explicit** column-rename hint.
///
/// "On `table`, the column called `from` (present in live) is the column called
/// `to` (present in desired) — they are the same column under a new name, NOT a
/// drop+add."
///
/// Renames are **opt-in by hint ONLY** — the differ NEVER infers a rename from a
/// drop+add pair heuristically (that risks silent data loss: a coincidental
/// "drop col X, add col Y" on the same table is two independent intents, and
/// treating it as a rename would carry X's data into Y against the creator's
/// will, or — worse — a misclassified rename could drop the wrong column). A
/// hint is the creator's signed statement of intent; without one, a drop+add
/// stays two independent ops (a gated DROP + an additive ADD).
///
/// When a hint matches an actual drop+add pair (and the types are compatible),
/// the differ routes that pair through the zero-downtime expand-contract path
/// ([`ExpandContractAuthor::RenameColumn`](crate::render::expand_contract)) instead of
/// emitting drop+add — the column's data is preserved by the dual-write +
/// backfill sequence, and the destructive `DROP COLUMN <from>` is gated.
///
/// The DSL `renamedFrom` surface that produces these hints is a separate SDK
/// follow-up; this struct is the engine-side input contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameHint {
    /// The table the rename happens on.
    pub table: String,
    /// The existing (live) column name being renamed away from.
    pub from: String,
    /// The new (desired) column name being renamed to.
    pub to: String,
}

/// A [`RenameHint`] that has been **verified** against the desired/live snapshots
/// (matched an actual drop+add pair with identical types). The diff routes each
/// one through the expand-contract rename sequence. `ty` is the shared
/// `information_schema` data-type spelling of the two matched columns.
#[derive(Debug, Clone)]
struct ResolvedRename {
    table: String,
    from: String,
    to: String,
    ty: String,
}

// ---------------------------------------------------------------------------
// DSL-type → information_schema.data_type mapping.
//
// Schema-authority P2: the engine's own v1-SUBSET type table was DELETED and the
// column-type resolution now DELEGATES to the shared `zeroship-schema` kernel
// (`query::def_to_column_type_for_dialect`). That is what gives the differ FULL
// capability — `vector(N)` / `geography(POINT,4326)` (geoPoint) / `BYTEA`
// (encrypted) / `literal`-primitive are now first-class, where the v1 subset
// rejected them. The #2 fail-closed guarantee is preserved on top of the shared
// map (an unknown token mapping to the shared `TEXT` fallback is still rejected,
// never silently degraded).
// ---------------------------------------------------------------------------

/// Map a bare DSL type TOKEN to the `information_schema.columns.data_type`
/// spelling the snapshot stores, by routing through the shared kernel (P2).
///
/// This is the token-only convenience entry (kept as the engine's public surface,
/// and used by `desired_snapshot` for token-only fields). Fields carrying the
/// parameterised goodies (`vector(dims)`, `encrypted{…}`, `mask{…}`) need their
/// whole descriptor, so `desired_snapshot` calls [`field_data_type`] directly;
/// this wrapper builds a minimal descriptor from the token. `actor`/`id` are
/// engine-only text spellings the shared SDK map does not name, folded to
/// `string` here.
///
/// The #2 fail-closed contract is preserved: a typo / out-of-scope token that the
/// shared map degrades to its `TEXT` fallback is rejected with
/// [`DeclarativeError::UnsupportedType`] rather than silently materialised as a
/// `text` column the creator never declared.
///
/// # Errors
/// [`DeclarativeError::UnsupportedType`] if `dsl_type` is not a supported token.
pub fn dsl_to_pg_data_type(dsl_type: &str) -> Result<String, DeclarativeError> {
    // Schema-authority P2: delegate to the shared kernel rather than the engine's
    // old v1-subset table. `actor`/`id` are engine-only spellings of `text` that
    // the shared SDK map does not name (it has no `actor`/`id` tokens), so fold
    // them here before handing off. Everything else (incl. the goodies
    // vector/geoPoint/encrypted — accepted now, no longer `UnsupportedType`)
    // routes through `field_data_type` with a minimal `def`.
    let f = FieldDescriptor {
        name: String::new(),
        ty: match dsl_type {
            "actor" | "id" => "string".to_string(),
            other => other.to_string(),
        },
        ..Default::default()
    };
    field_data_type(&f)
}

/// Build the SDK `FieldDef` JSON (`{ type, encrypted?, vectorDims?, vectorMetric?,
/// mask?, literalValue? }`) the shared `zeroship-schema` kernel consumes, from the
/// engine's [`FieldDescriptor`]. This is the bridge that lets the engine reuse the
/// shared DDL/type map (full capability) without adopting the SDK's untyped JSON
/// as its public authoring surface: the engine keeps its typed descriptor, the
/// shared kernel keeps its `Value`-driven builders, and this is the one mapping
/// point between them.
fn field_to_sdk_def(f: &FieldDescriptor) -> serde_json::Value {
    let mut def = serde_json::Map::new();
    def.insert("type".into(), serde_json::Value::String(f.ty.clone()));
    if let Some(d) = f.vector_dims {
        def.insert("vectorDims".into(), serde_json::Value::from(d));
    }
    if let Some(m) = &f.vector_metric {
        def.insert("vectorMetric".into(), serde_json::Value::String(m.clone()));
    }
    if let Some(enc) = &f.encrypted {
        def.insert("encrypted".into(), enc.clone());
    }
    if let Some(mask) = &f.mask {
        def.insert("mask".into(), mask.clone());
    } else if f.encrypted.is_some() {
        // Mirror the SDK's `t.encrypted()` builder: encrypted columns get the
        // fail-safe full/pii mask unless the author explicitly overrides or opts
        // out with `.mask({ kind: "none" })`.
        def.insert(
            "mask".into(),
            serde_json::json!({ "kind": "full", "classification": "pii" }),
        );
    }
    if let Some(lit) = &f.literal_value {
        def.insert("literalValue".into(), lit.clone());
    }
    if let Some(generated) = &f.generated {
        def.insert(
            "generated".into(),
            serde_json::to_value(generated).expect("GeneratedCol serializes"),
        );
    }
    if let Some(identity) = &f.identity {
        def.insert(
            "identity".into(),
            serde_json::to_value(identity).expect("IdentityCol serializes"),
        );
    }
    serde_json::Value::Object(def)
}

/// PHASE 4 — reconstruct the FULL SDK schema `Value` (`{ <field>: { type, … } }`)
/// the shared `zeroship_schema::query` CREATE-TABLE emitter consumes, from a
/// [`CollectionDescriptor`]. This is the descriptor→`Value` bridge the **Confined
/// SQLite** path routes through: the engine keeps its typed descriptor as its
/// authoring surface and hands the shared emitter exactly the JSON shape the SDK's
/// `registerModel` would, so a `generate`d SQLite table is byte-for-byte the same
/// shape (system fields, mask siblings, sentinels, FK clauses) as the runtime's.
///
/// Per [`field_to_sdk_def`] for the goodies facets, plus the keys the emitter reads
/// for plain columns: `required`, FK (`refTarget`/`onDelete`/`onUpdate`/`deferrable`),
/// `default`, `min`/`max`/`enum` (CHECK constraints), `idPrefix`, and `index`/`unique`
/// (the emitter ignores `index`/`unique` for CREATE TABLE — indexes are separate —
/// but they are carried for completeness/fidelity).
///
/// NOTE: this is descriptor-diff-generated DDL ONLY — there is NO untrusted raw SQL
/// string; the descriptor field/type names were validated at the author boundary
/// (`validate_desired`) before this runs (§2.5.3 trust model).
/// **PR2** — produce the post-rename SDK schema `Value` for a SQLite
/// `renameColumn` rebuild by renaming ONE top-level field key `from`→`to`,
/// preserving its definition object verbatim (`{ <field>: { type, … } }`). The
/// shared SQLite CREATE emitter renders the per-column type/affinity + sentinels
/// from this object, so carrying the field def unchanged under the new key yields
/// a post-rename column byte-identical to a `t.*`-diff rename's. Returns `None` if
/// the live schema is not an object or has no `from` field (the caller fails
/// closed). The field-insertion ORDER is preserved (the renamed field keeps its
/// position) so the emitted column order matches the live table's.
fn rename_sdk_schema_field(
    live: &serde_json::Value,
    from: &str,
    to: &str,
) -> Option<serde_json::Value> {
    let obj = live.as_object()?;
    if !obj.contains_key(from) {
        return None;
    }
    let mut out = serde_json::Map::new();
    for (k, v) in obj {
        if k == from {
            out.insert(to.to_string(), v.clone());
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    Some(serde_json::Value::Object(out))
}

/// **PR9c LOW (ii)** — does this SDK field def declare a DATA-TRANSFORMING facet that
/// a verbatim value-copy across a SQLite rebuild cannot certify was already present on
/// the source column? Returns the facet name (for the fail-closed error) or `None` for
/// a plain column.
///
/// The catalog-sourced rename path (`apply_bundle_ir_sqlite_catalog`, branch 2) renders
/// the rebuilt table's CREATE from the descriptor's POST-rename `to` def but copies the
/// live `from` bytes UN-TRANSFORMED. A facet whose shape depends on the column's VALUE
/// (encryption changes the on-disk bytes; `mask` adds a sibling masked column; `default`
/// backfills a value; `enum`/`check`/range bounds constrain the values) therefore cannot
/// be SAFELY introduced in the same op as the rename — the old bytes were authored under
/// the (unknown) `from` facets. Affinity facets (plain `type`, `vector`, `required`,
/// `unique`, FK) are NOT here: they are either already covered by the affinity guard or
/// are structural (a unique/FK violation surfaces at rebuild time, not a silent value
/// corruption). Conservative + fail-closed: any of these present ⇒ refuse.
fn data_transforming_facet(def: &serde_json::Value) -> Option<&'static str> {
    let obj = def.as_object()?;
    if obj.contains_key("encrypted") {
        return Some("encrypted");
    }
    if obj.contains_key("mask") {
        return Some("mask");
    }
    if obj.contains_key("default") {
        return Some("default");
    }
    if obj.contains_key("generated") {
        return Some("generated");
    }
    if obj.contains_key("identity") {
        return Some("identity");
    }
    if obj.contains_key("enum") {
        return Some("enum");
    }
    // `min`/`max` lower to a CHECK range constraint over the column's values.
    if obj.contains_key("min") || obj.contains_key("max") {
        return Some("check");
    }
    None
}

pub fn descriptor_to_sdk_schema(d: &CollectionDescriptor) -> serde_json::Value {
    let mut schema = serde_json::Map::new();
    for f in &d.fields {
        // Start from the goodies bridge (`type`, vector*, encrypted, mask,
        // literalValue), then layer the remaining SDK keys the emitter reads.
        let mut def = match field_to_sdk_def(f) {
            serde_json::Value::Object(m) => m,
            // `field_to_sdk_def` always returns an object; defensive fallback.
            _ => serde_json::Map::new(),
        };
        if f.required {
            def.insert("required".into(), serde_json::Value::Bool(true));
        }
        if f.unique {
            def.insert("unique".into(), serde_json::Value::Bool(true));
        }
        // FK: the emitter keys on `refTarget` (+ policy), NOT the engine's `ref`.
        if f.ty == "ref" {
            if let Some(target) = &f.references {
                def.insert(
                    "refTarget".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            if let Some(od) = &f.on_delete {
                def.insert("onDelete".into(), serde_json::Value::String(od.clone()));
            }
            if let Some(ou) = &f.on_update {
                def.insert("onUpdate".into(), serde_json::Value::String(ou.clone()));
            }
            if let Some(dfr) = f.deferrable {
                def.insert("deferrable".into(), serde_json::Value::Bool(dfr));
            }
        }
        if let Some(def_val) = &f.default {
            def.insert("default".into(), def_val.clone());
        }
        if let Some(min) = f.min {
            if let Some(n) = serde_json::Number::from_f64(min) {
                def.insert("min".into(), serde_json::Value::Number(n));
            }
        }
        if let Some(max) = f.max {
            if let Some(n) = serde_json::Number::from_f64(max) {
                def.insert("max".into(), serde_json::Value::Number(n));
            }
        }
        if let Some(en) = &f.enum_values {
            def.insert("enum".into(), serde_json::Value::Array(en.clone()));
        }
        if let Some(prefix) = &f.id_prefix {
            def.insert(
                "idPrefix".into(),
                serde_json::Value::String(prefix.clone()),
            );
        }
        if f.fts {
            def.insert("fts".into(), serde_json::Value::Bool(true));
            if let Some(lang) = &f.fts_language {
                def.insert(
                    "ftsLanguage".into(),
                    serde_json::Value::String(lang.clone()),
                );
            }
        }
        schema.insert(f.name.clone(), serde_json::Value::Object(def));
    }
    serde_json::Value::Object(schema)
}

/// **P4 HALF A** — build the shared [`zeroship_schema::diff::EncryptionMeta`] for
/// a field's `t.encrypted({...})` declaration, or `None` for a plaintext field.
/// Used to render the PG `COMMENT ON COLUMN` `zsenc:` sentinel (via the shared
/// codec's `build_encryption_sentinel`) so the engine's emitted comment is
/// byte-identical to what plugin-db's runtime parser expects. Defaults mirror
/// the inline sentinel emitter (`mode = randomised`, `keyId = default`,
/// `wraps = string`).
fn encryption_meta_for_field(def: &serde_json::Value) -> Option<zeroship_schema::diff::EncryptionMeta> {
    use zeroship_schema::descriptors::EncryptionMode;
    use zeroship_schema::diff::{EncryptionMeta, WrappedType};
    let enc = def.get("encrypted").and_then(|v| v.as_object())?;
    let mode_str = enc.get("mode").and_then(|v| v.as_str()).unwrap_or("randomised");
    let mode = match mode_str {
        "deterministic" => EncryptionMode::Deterministic,
        // `randomised` / `randomized` (US) / anything else → fail-safe default.
        _ => EncryptionMode::Randomised,
    };
    let key_id = enc
        .get("keyId")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let wraps = match enc.get("wraps").and_then(|v| v.as_str()) {
        Some("number") => WrappedType::Number,
        Some("bytes") => WrappedType::Bytes,
        _ => WrappedType::String,
    };
    Some(EncryptionMeta { mode, key_id, wraps })
}

/// The hidden `<col>_masked` sibling column a field's `.mask({...})` declaration
/// requires, or `None` for an unmasked field / `kind: "none"` opt-out. Delegates
/// to the shared kernel ([`zeroship_schema::query::mask_sibling_column_for_field`])
/// so the engine and plugin-db agree on exactly which fields get a sibling.
fn mask_sibling_for_field(f: &FieldDescriptor) -> Option<String> {
    zeroship_schema::query::mask_sibling_column_for_field(&f.name, &field_to_sdk_def(f))
}

/// Resolve a field's column data type in the `information_schema.data_type`
/// spelling the snapshot stores, by routing through the shared kernel's
/// [`zeroship_schema::query::def_to_column_type_for_dialect`] (the FULL type map,
/// P2) and translating its DDL spelling to the `information_schema` form.
///
/// A bare/unknown token still fails closed: the shared map's plain-type fallback
/// is `TEXT`, so to preserve the engine's #2 "never silently degrade an
/// unrecognised type to text" guarantee, an UNKNOWN token whose shared mapping is
/// the `TEXT` fallback (and which is not one of the engine's own text-spelled
/// tokens) is rejected with [`DeclarativeError::UnsupportedType`].
fn field_data_type(f: &FieldDescriptor) -> Result<String, DeclarativeError> {
    use zeroship_schema::query::{def_to_column_type_for_dialect, SqlDialect};

    // A bare `literal` with no value is malformed — the SDK never emits it, and
    // the shared map would degrade it to TEXT. Keep the engine's explicit error.
    if f.ty == "literal" && f.literal_value.is_none() {
        return Err(DeclarativeError::UnsupportedType { ty: "literal".into() });
    }

    // GAP (flagged): the shared kernel's `def_to_pg_type` has NO `bytes` arm — a
    // bare `t.bytes()` token degrades to its `_ => TEXT` fallback there (plugin-db
    // only ever reaches BYTEA via `encrypted`). The engine maps `t.bytes()` to
    // `BYTEA` as a first-class type, so handle it here directly rather than letting
    // it wrongly degrade to TEXT (and then be rejected by the fail-closed guard).
    // When the shared crate grows a `bytes` arm this special-case can be deleted.
    if f.ty == "bytes" && f.encrypted.is_none() {
        return Ok("bytea".into());
    }

    // M1 — `int`/`integer` are now handled by the shared `def_to_pg_type` (it grew
    // a first-class `INTEGER` arm), so they route through the shared map below like
    // every other token: `INTEGER` → `ddl_to_information_schema` → `integer`. The
    // engine no longer needs a special-case (the previous one papered over the
    // shared PG map's missing int arm and applied to BOTH dialects' snapshots,
    // which was correct for SQLite but produced a snapshot↔emitter drift on PG).
    // The PG type *names* `bigint`/`int4`/`int8` remain NON-tokens: the shared map
    // leaves them on the TEXT fallback, so they stay fail-closed-rejected as typos.

    let def = field_to_sdk_def(f);
    let ddl = def_to_column_type_for_dialect(&def, SqlDialect::Postgres);

    // #2 fail-closed: the shared map returns `TEXT` for any unrecognised type
    // token. The engine's set of types that LEGITIMATELY land on text is the
    // closed set below (incl. the `actor`/`id` already folded to `string` by the
    // caller, and a `literal` whose value is a string). Anything else mapping to
    // `TEXT` is an unknown/typo'd token (`bigint`, `uuid`, `int4`, `__proto__`, …)
    // and is rejected rather than silently degraded.
    if ddl.eq_ignore_ascii_case("text") && !field_text_is_legitimate(f) {
        return Err(DeclarativeError::UnsupportedType { ty: f.ty.clone() });
    }

    Ok(ddl_to_information_schema(&ddl))
}

/// True if a field whose shared mapping is `TEXT` is one the engine accepts as a
/// genuine text column (vs. an unknown token the shared map degraded). The text
/// types are `string`/`ref` (and an encrypted column wrapping a string still maps
/// to BYTEA, so it never reaches here), plus a `literal` whose value is a string.
fn field_text_is_legitimate(f: &FieldDescriptor) -> bool {
    match f.ty.as_str() {
        // `string`/`ref` map to TEXT; `actor`/`id` are engine-only spellings of a
        // text column (the actor stamp / the typed-id PK) that the shared SDK map
        // does not name, so they also land on TEXT and are legitimate.
        "string" | "ref" | "actor" | "id" => true,
        "literal" => matches!(f.literal_value, Some(serde_json::Value::String(_))),
        _ => false,
    }
}

/// Translate the shared kernel's DDL type spelling (`TEXT`, `DOUBLE PRECISION`,
/// `TIMESTAMPTZ`, `JSONB`, `BYTEA`, `vector(N)`, `geography(POINT, 4326)`, …) to
/// the `information_schema.columns.data_type` spelling the snapshot stores, so a
/// freshly-created table introspects to a byte-equal snapshot (the round-trip
/// oracle). The twelve base types map to their canonical lowercase
/// `information_schema` form; the parameterised extension types
/// (`vector(N)`/`geography(...)`) keep their DDL spelling. `information_schema`
/// reports `USER-DEFINED` for those, so `snapshot_schema` recovers their precise
/// spelling from `pg_catalog.format_type` and canonicalises it back to this DDL
/// form (see [`crate::apply::drift::snapshot_schema`] / `canonical_extension_type`) —
/// the round-trip is real when the extension is installed (T13 for geoPoint).
fn ddl_to_information_schema(ddl: &str) -> String {
    match ddl.to_ascii_uppercase().as_str() {
        "TEXT" => "text".into(),
        "DOUBLE PRECISION" => "double precision".into(),
        "BOOLEAN" => "boolean".into(),
        "TIMESTAMPTZ" => "timestamp with time zone".into(),
        "DATE" => "date".into(),
        "JSONB" => "jsonb".into(),
        "BYTEA" => "bytea".into(),
        "NUMERIC" => "numeric".into(),
        "INTEGER" => "integer".into(),
        "BIGINT" => "bigint".into(),
        // Parameterised / extension types (vector(N), geography(POINT,4326)) keep
        // their DDL spelling — see the doc note.
        _ => ddl.to_string(),
    }
}

/// Canonicalise a column `data_type` to the SQLite type-affinity token used to
/// compare a DESIRED snapshot (PG-spelled, from [`ddl_to_information_schema`])
/// against a LIVE snapshot REAL-introspected from SQLite
/// (`sqlite::drift_sql::snapshot_schema`, which returns the
/// lowercased SQLite declared type).
///
/// # Why this exists
///
/// `desired_snapshot` always emits the Postgres `information_schema` spelling for
/// `data_type` (`text`, `bytea`, `double precision`, `timestamp with time zone`,
/// …) regardless of dialect — the snapshot model is dialect-agnostic. But a REAL
/// SQLite introspection reports the SQLite *declared* type the emitter wrote
/// (`text`, `blob`, `real`, `integer`, `numeric`). Comparing the two raw spellings
/// on the SQLite leg falsely flags a type change on every column whose PG and
/// SQLite spellings differ — e.g. an encrypted column (`bytea` desired vs `blob`
/// live), a number (`double precision` vs `real`), a timestamp (`timestamp with
/// time zone` vs `text`) — yielding a spurious [`DeclarativeError::SqliteRebuildRequired`].
///
/// # Source of truth
///
/// The five target tokens are exactly the SQLite column types the shared emitter
/// ([`zeroship_schema::query::def_to_column_type_for_dialect`] with
/// `SqlDialect::Sqlite`) produces — `TEXT` / `REAL` / `INTEGER` / `NUMERIC` /
/// `BLOB` — so emit and compare agree. Each arm below maps a PG `data_type`
/// spelling (LHS) to the SQLite type the emitter would have written for the SAME
/// field, AND folds the already-SQLite-spelled live token to the same canonical
/// form. Parameterised extension types (`vector(N)`, `geography(POINT, 4326)`)
/// emit `BLOB` on SQLite, so any `vector(`/`geography(` prefix folds to `blob`.
///
/// This is applied ONLY on the SQLite comparison leg; the PG leg compares the raw
/// `information_schema` spellings unchanged (it never calls this), so the PG
/// type-change detection is untouched — and a REAL SQLite type change (e.g.
/// `text` → `real`, i.e. string → number) still maps to two DIFFERENT canonical
/// tokens and IS detected.
pub fn sqlite_canonical_type(data_type: &str) -> &'static str {
    zeroship_schema::query::sqlite_canonical_type(data_type)
}

/// Single-quote a SQL string literal (double embedded quotes). Mirrors
/// plugin-db's `'{}'` formatting in `def_to_constraints` (`s.replace('\'', "''")`).
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Render a JSON scalar as a SQL literal for a CHECK / IN clause: a string is
/// single-quoted, a number is its canonical form, a boolean is `true`/`false`.
/// `None` for a non-scalar (null/array/object) — those never reach a literal/enum
/// CHECK in plugin-db.
fn json_scalar_sql(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(sql_str(s)),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The CHECK constraint(s) a field declares (#3 literal-pin, #4 min/max + enum),
/// each as a [`ConstraintSnapshot`] whose `definition` is the emitted DDL CHECK
/// clause (used by `render_create_table` to inline it).
///
/// Mirrors plugin-db's `def_to_constraints_for_dialect` (`query.rs:2167-2218`):
/// - a numeric field with `min`/`max` → `CHECK (<col> >= min [AND <col> <= max])`
///   (or `CHECK (<col> <= max)` for max-only);
/// - a `literal` field → `CHECK (<col> = <value>)`;
/// - an `enum` → `CHECK (<col> IN (v1, v2, …))`.
///
/// The constraint NAME is deterministic (`<table>_<field>_<kind>_chk`) so it
/// round-trips by name. The differ does NOT re-diff CHECK bodies (only FOREIGN
/// KEY bodies — `pg_get_constraintdef` heavily normalises a CHECK predicate, so a
/// byte round-trip of the body is not attempted; the constraint's PRESENCE and
/// enforcement are what round-trip cleanly, matching plugin-db, which never
/// re-diffs a CHECK).
fn field_check_constraints(table: &str, f: &FieldDescriptor) -> Vec<ConstraintSnapshot> {
    let mut out = Vec::new();
    let col = quote_ident(&f.name);

    // #4 min/max (numeric only — matches plugin-db's `type == "number"` gate).
    if f.ty == "number" {
        let expr = match (f.min, f.max) {
            (Some(min), Some(max)) => Some(format!("CHECK ({col} >= {min} AND {col} <= {max})")),
            (Some(min), None) => Some(format!("CHECK ({col} >= {min})")),
            (None, Some(max)) => Some(format!("CHECK ({col} <= {max})")),
            (None, None) => None,
        };
        if let Some(def) = expr {
            out.push(ConstraintSnapshot {
                name: check_constraint_name(table, &f.name, "range"),
                kind: "CHECK".into(),
                definition: def,
                comment: None,
            });
        }
    }

    // #3 literal-pin.
    if f.ty == "literal" {
        if let Some(rendered) = f.literal_value.as_ref().and_then(json_scalar_sql) {
            out.push(ConstraintSnapshot {
                name: check_constraint_name(table, &f.name, "lit"),
                kind: "CHECK".into(),
                definition: format!("CHECK ({col} = {rendered})"),
                comment: None,
            });
        }
    }

    // #4 enum membership.
    if let Some(values) = &f.enum_values {
        let rendered: Vec<String> = values.iter().filter_map(json_scalar_sql).collect();
        if !rendered.is_empty() {
            out.push(ConstraintSnapshot {
                name: check_constraint_name(table, &f.name, "enum"),
                kind: "CHECK".into(),
                definition: format!("CHECK ({col} IN ({}))", rendered.join(", ")),
                comment: None,
            });
        }
    }

    out
}

/// Deterministic CHECK constraint name, capped ≤63 bytes (same NAMEDATALEN
/// budget as the index/FK names). `kind` distinguishes `lit` / `range` / `enum`
/// so a field carrying several CHECKs gets distinct, stable names.
fn check_constraint_name(table: &str, field: &str, kind: &str) -> String {
    crate::plan::author::cap_ident_name(&format!("{table}_{field}_{kind}_chk"))
}

/// The `DEFAULT` clause expression a field emits at CREATE / ADD COLUMN (#4),
/// or `None` for no default.
///
/// Mirrors plugin-db's `def_to_constraints_for_dialect` default arm
/// (`query.rs:2125-2165`): an explicit `default` renders per the field's
/// primitive type (string single-quoted, number/boolean bare, json/object →
/// `'{}'::jsonb`, array → `'[]'::jsonb`); AND, even with NO explicit default,
/// json/object default to `'{}'::jsonb` and array to `'[]'::jsonb` (plugin-db's
/// "default defaults"). Emission-only — not drift-compared.
/// Render a numeric column DEFAULT to its SQL literal, precision-preserving.
///
/// A default reaches us as one of three carriers and each must render without
/// loss or injection:
///   - a JSON integer (`IrScalar::Int` ⇒ `as_i64`) — exact, no float rounding;
///   - a JSON float (the differ's wire `FieldDef`, ⇒ `as_f64`);
///   - a validated numeric STRING — `IrScalar::Decimal` carries arbitrary-
///     precision decimals AND bigints ≥ 2^53 as a string (the IR rejects an
///     oversized/fractional JSON number at parse). `as_f64` returns `None` for a
///     JSON string and would corrupt a >2^53 bigint anyway, so we emit the string
///     verbatim — re-validated as a plain numeric literal ([`crate::model::ir::
///     is_decimal_string`]) so nothing else can inject raw text into the DDL.
fn numeric_default_literal(v: &serde_json::Value) -> Option<String> {
    if let Some(i) = v.as_i64() {
        Some(i.to_string())
    } else if let Some(f) = v.as_f64() {
        Some(f.to_string())
    } else {
        v.as_str().filter(|s| crate::model::ir::is_decimal_string(s)).map(str::to_string)
    }
}

fn field_default_expr(f: &FieldDescriptor) -> Option<String> {
    if let Some(default) = &f.default {
        return match f.ty.as_str() {
            "string" => default.as_str().map(sql_str),
            // `int` (`t.integer()`/`t.bigInt()`) and `number` (`t.float()`/
            // `t.numeric()`) share one precision-preserving renderer — without the
            // `int` arm an integer column's DEFAULT silently dropped, and a
            // decimal/bigint carried as a numeric string dropped from BOTH.
            "int" | "bigInt" | "number" => numeric_default_literal(default),
            "boolean" => default.as_bool().map(|b| b.to_string()),
            "json" | "object" => Some("'{}'::jsonb".into()),
            "array" => Some("'[]'::jsonb".into()),
            _ => None,
        };
    }
    // "Default defaults" for the JSON-backed types (matches plugin-db's else arm).
    match f.ty.as_str() {
        "json" | "object" => Some("'{}'::jsonb".into()),
        "array" => Some("'[]'::jsonb".into()),
        _ => None,
    }
}

fn generated_column_snapshot(
    generated: &crate::model::ir::GeneratedCol,
    dialect: SqlDialect,
) -> Result<GeneratedColumnSnapshot, DeclarativeError> {
    if !generated.stored && !dialect.supports(Capability::VirtualGeneratedColumn) {
        return Err(DeclarativeError::Invalid(
            r#"UNSUPPORTED { kind: "virtualColumn", dialect: "pg" }"#.to_string(),
        ));
    }
    let expr = crate::render::dml::render_expr_inline(&generated.expr, dialect).map_err(|e| {
        DeclarativeError::Invalid(format!("generated column expression is not renderable: {e}"))
    })?;
    Ok(GeneratedColumnSnapshot {
        expr,
        stored: generated.stored,
    })
}

fn column_snapshot_for_field(
    f: &FieldDescriptor,
    dialect: SqlDialect,
) -> Result<ColumnSnapshot, DeclarativeError> {
    let data_type = field_data_type(f)?;
    let sdk_def = field_to_sdk_def(f);
    let encryption_sentinel = zeroship_schema::query::encryption_sentinel_for_field(&sdk_def);
    let comment_sentinel =
        encryption_meta_for_field(&sdk_def).map(|m| zeroship_schema::mask_codec::build_encryption_sentinel(&m));
    Ok(ColumnSnapshot {
        name: f.name.clone(),
        data_type,
        nullable: !f.required,
        default: field_default_expr(f),
        generated: f
            .generated
            .as_ref()
            .map(|g| generated_column_snapshot(g, dialect))
            .transpose()?,
        identity: f.identity,
        encryption_sentinel,
        comment_sentinel,
        ..Default::default()
    })
}

/// The seven platform-managed system fields, in canonical order, as
/// [`ColumnSnapshot`]s. Replicated from `plugin-db`'s
/// `build_system_field_columns` (`id TEXT PRIMARY KEY`, `created_at`/`updated_at`
/// `TIMESTAMPTZ NOT NULL`, `created_by`/`updated_by` `TEXT NULL`, `version`
/// `INTEGER NOT NULL`, `deleted_at` `TIMESTAMPTZ NULL`), expressed in
/// `information_schema` data-type spelling.
///
/// Every collection table gets these injected by [`desired_snapshot`], matching
/// what `installSchema` materialises, so the desired snapshot round-trips to the
/// live table the SDK creates.
fn system_field_columns() -> Vec<ColumnSnapshot> {
    let ts = "timestamp with time zone";
    vec![
        ColumnSnapshot { name: "id".into(), data_type: "text".into(), nullable: false, ..Default::default() },
        ColumnSnapshot { name: "created_at".into(), data_type: ts.into(), nullable: false, ..Default::default() },
        ColumnSnapshot { name: "updated_at".into(), data_type: ts.into(), nullable: false, ..Default::default() },
        ColumnSnapshot { name: "created_by".into(), data_type: "text".into(), nullable: true, ..Default::default() },
        ColumnSnapshot { name: "updated_by".into(), data_type: "text".into(), nullable: true, ..Default::default() },
        ColumnSnapshot { name: "version".into(), data_type: "integer".into(), nullable: false, ..Default::default() },
        ColumnSnapshot { name: "deleted_at".into(), data_type: ts.into(), nullable: true, ..Default::default() },
    ]
}

/// The columns the platform auto-indexes on every table (#6). Mirrors
/// plugin-db's `build_system_field_indexes` (`query.rs:900`): `deleted_at`
/// (soft-delete filtering), `updated_at` (cursor-paged reads), `created_by`
/// (per-actor lookups + audit). `id` is implicitly indexed by the PK; `version`
/// is deliberately NOT indexed (bumped on every UPDATE — index thrash).
const SYSTEM_INDEXED_COLS: &[&str] = &["deleted_at", "updated_at", "created_by"];

/// The three implicit B-tree system indexes, as [`IndexSnapshot`]s (#6).
///
/// The platform auto-creates these for every table; the live snapshot always
/// carries them, so the desired snapshot must too — otherwise the differ reads
/// each as an out-of-band index to DROP (phantom drift). Names match plugin-db's
/// `index_name(table, &[col], false)` = `<table>_<col>_idx`, NAMEDATALEN-capped.
fn system_field_indexes(table: &str) -> Vec<IndexSnapshot> {
    SYSTEM_INDEXED_COLS
        .iter()
        .map(|col| {
            IndexSnapshot::btree(
                non_unique_index_name(table, col),
                false,
                vec![(*col).to_string()],
            )
        })
        .collect()
}

/// Deterministic name for a non-unique single-column index
/// (`<table>_<col>_idx`), matching plugin-db's `index_name(table, &[col], false)`
/// and NAMEDATALEN-capped via [`crate::plan::author::cap_ident_name`] so a long
/// table+col round-trips to the same (truncated) live name.
fn non_unique_index_name(table: &str, col: &str) -> String {
    crate::plan::author::cap_ident_name(&format!("{table}_{col}_idx"))
}

// ---------------------------------------------------------------------------
// P4 — the UNION desired schema + per-table ownership.
// ---------------------------------------------------------------------------

/// The **desired** project schema (the UNION over every member app's declared
/// collections) PLUS the per-table ownership map (design §4).
///
/// A project = one db = one project schema, and that schema is the UNION of all
/// member apps' schema authoring declarations. [`desired_snapshot`]
/// builds this: identical re-declarations of a table by two apps merge to one
/// table (idempotent); a conflicting re-declaration is a hard
/// [`DeclarativeError::ConflictingDeclaration`].
///
/// `ownership` records, for each table in `snapshot`, the app that **owns** its
/// migrations. [`DeclarativeAuthor::diff`] enforces that only the owning app may
/// emit a structural change (CREATE/ALTER/DROP) to a table — a non-owner may USE
/// it but not migrate it.
#[derive(Debug, Clone, Default)]
pub struct DesiredSchema {
    /// The union of all member apps' declared tables, as the diffable snapshot.
    pub snapshot: SchemaSnapshot,
    /// `table name → owning app`. Exactly the keys of `snapshot.tables`.
    pub ownership: BTreeMap<String, String>,
    /// PHASE 4 — `table name → full SDK schema `Value`` (the
    /// [`descriptor_to_sdk_schema`] reconstruction), kept alongside the snapshot so
    /// the **Confined SQLite** path can route a new-table CREATE through the shared
    /// `zeroship_schema::query` emitter (which is `Value`-driven). The keys match
    /// `snapshot.tables`. The PG path never reads this map (it renders from the
    /// snapshot), so it is inert on PG. It does NOT participate in drift — drift is
    /// the snapshot's job — so it is excluded from `PartialEq` (see the manual impl).
    pub sqlite_schemas: BTreeMap<String, serde_json::Value>,
}

// The `sqlite_schemas` side-map is a derived emission aid (it is rebuilt from the
// same descriptors that produce `snapshot`), so two `DesiredSchema`s are equal iff
// their snapshot + ownership are — matching the pre-PHASE-4 equality semantics so
// existing tests/asserts that compare `DesiredSchema`s stay valid.
impl PartialEq for DesiredSchema {
    fn eq(&self, other: &Self) -> bool {
        self.snapshot == other.snapshot && self.ownership == other.ownership
    }
}
impl Eq for DesiredSchema {}

impl DesiredSchema {
    /// The owning app for `table`, if it is in the union.
    #[must_use]
    pub fn owner_of(&self, table: &str) -> Option<&str> {
        self.ownership.get(table).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// P0 — desired_snapshot compiler.
// ---------------------------------------------------------------------------

/// Compile a set of [`CollectionDescriptor`]s into a deterministic
/// [`SchemaSnapshot`] — the **desired** schema (P0).
///
/// For each collection it emits a [`TableSnapshot`] whose:
/// - **columns** are the seven platform system fields (see
///   [`system_field_columns`]) plus one column per declared field, with the
///   `data_type` from [`dsl_to_pg_data_type`] and `nullable = !required`;
/// - **constraints** carry the `id` PRIMARY KEY (named `<table>_pkey`, the
///   Postgres default) and one FOREIGN KEY per `ref` field;
/// - **indexes** carry the declared named indexes plus a unique index per
///   `unique: true` field.
///
/// The snapshot is the same shape [`snapshot_schema`](crate::apply::drift::snapshot_schema)
/// produces from the live DB, so a freshly-created table introspects to a
/// byte-equal snapshot (zero drift) — that equality is the P0 type-fidelity
/// proof.
///
/// `project_schema` is the schema every table lives in; it is needed because a
/// FOREIGN KEY's `pg_get_constraintdef` body is **schema-qualified**
/// (`FOREIGN KEY (col) REFERENCES <schema>.target(id)`), so the desired-side FK
/// definition must carry the same qualification to match live exactly — otherwise
/// every FK shows permanent phantom drift (1b). It is NOT used for any non-FK
/// part of the snapshot.
///
/// **Pure.** No I/O, no DDL. It performs the minimal author-boundary check that
/// guards the *projection itself* — an unrecognised/out-of-scope field type (#2)
/// — so a degraded snapshot (the creator declared X, would have got `text`) is
/// never produced. Full identifier re-validation still happens in
/// [`DeclarativeAuthor::diff`] (defense in depth) and the guard is the second
/// line.
///
/// # Caller contract
///
/// `descriptors` MUST be the **COMPLETE project union** — the concatenation of
/// EVERY member app's declared collections, NOT just the deploying app's. The
/// resulting [`DesiredSchema`] is what [`DeclarativeAuthor::diff`] /
/// [`plan_declarative`](crate::engine::MigrationEngine::plan_declarative) diff
/// against live; a live table absent from this union is read as "no app declares
/// it" and becomes a `DROP TABLE` candidate. A PARTIAL union (one app's
/// descriptors only) would therefore mark every OTHER app's live table for
/// drop — which the differ now refuses fail-closed via its `live_ownership`
/// guard (2b), but the caller must still pass the full union so legitimate
/// tables are not needlessly refused.
///
/// # Multi-app UNION + per-table ownership (P4, design §4)
///
/// Each descriptor carries its declaring [`CollectionDescriptor::owner_app`].
/// The result is the UNION over all apps:
/// - A table declared by exactly one app → owned by that app.
/// - A table declared by two apps with the **same shape** (identical columns,
///   indexes, constraints, and types) → merged to one table; ownership is the
///   **lexicographically-smallest** declaring app (so the union is identical
///   regardless of descriptor order — conflict-detection and ownership are both
///   order-independent). This is the design's "identical re-declaration is
///   idempotent".
/// - A table declared by two apps with **different** shapes →
///   [`DeclarativeError::ConflictingDeclaration`] (one owner per table; a
///   conflicting claim is a deploy error, never a silent merge).
///
/// # Errors
/// - [`DeclarativeError::UnsupportedType`] — a field used a type token outside
///   the twelve supported (or an out-of-scope `vector`/`geoPoint`/`encrypted`).
/// - [`DeclarativeError::ConflictingDeclaration`] — two apps declare the same
///   table with different shapes.
/// - [`DeclarativeError::Invalid`] — a `ref` field's target table is not a safe
///   bare identifier.
pub fn desired_snapshot(
    project_schema: &str,
    descriptors: &[CollectionDescriptor],
) -> Result<DesiredSchema, DeclarativeError> {
    // The historical entry point defaults to the **Postgres** desired shape —
    // byte-identical to before FTS became dialect-aware. The only dialect-divergent
    // part of the snapshot is full-text search (PG: a `__fts` tsvector column + GIN
    // index; SQLite: an FTS5 virtual table). Every other facet (vector→BLOB,
    // geoPoint→BLOB, …) is already modelled dialect-agnostically and SQLite tests
    // call THIS entry safely. A SQLite schema that uses `.fts()` MUST instead call
    // [`desired_snapshot_for_dialect`] with `SqlDialect::Sqlite` so the FTS index is
    // modelled as the FTS5 vtable the SQLite emitter actually produces (otherwise
    // the PG-shaped `__fts` GIN index is emitted over a column SQLite never
    // materialises → apply fails with `no such column: "__fts"`).
    desired_snapshot_for_dialect(project_schema, descriptors, SqlDialect::Postgres)
}

/// Dialect-aware [`desired_snapshot`] (P0). Identical to [`desired_snapshot`] for
/// every facet EXCEPT full-text search, whose physical shape differs by engine:
///
/// - **Postgres** — a `.fts()` field folds into ONE `__fts` GENERATED `tsvector`
///   column + a `<coll>__fts_idx` GIN index (the trigger-free declarative form the
///   engine owns end-to-end). BYTE-IDENTICAL to the pre-dialect snapshot.
/// - **SQLite** — a `.fts()` field folds into an FTS5 **virtual table**
///   (`<coll>__fts`) over the source columns, mirrored by AFTER triggers — the same
///   structure plugin-db's runtime `ensure_fts_index` builds and the shared
///   `zeroship_schema::fts_sqlite` builders emit. NO `__fts` column, NO GIN index
///   (`tsvector` has no SQLite spelling).
///
/// Modelling the FTS index as what the per-dialect emitter actually produces is
/// what makes a re-diff of an unchanged FTS schema ZERO-drift on both legs.
pub fn desired_snapshot_for_dialect(
    project_schema: &str,
    descriptors: &[CollectionDescriptor],
    dialect: SqlDialect,
) -> Result<DesiredSchema, DeclarativeError> {
    // First pass: accumulate EVERY declaration per table as (owner_app, shape),
    // independent of order. Conflict detection + ownership are then derived from
    // the FULL declarer set in a deterministic second pass — so with 3+ declarers
    // the reported conflict does not depend on which identical twin happened to
    // hold the slot first (1b).
    let mut declarations: BTreeMap<String, Vec<(String, TableSnapshot)>> = BTreeMap::new();
    // PHASE 4 — the per-table SDK schema `Value` (the descriptor→`Value` bridge),
    // carried so the Confined SQLite path can route the new-table CREATE through the
    // shared emitter. Keyed by table; identical re-declarations overwrite with an
    // identical value (idempotent, like the snapshot itself).
    let mut sqlite_schemas: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    for d in descriptors {
        // PHASE 4 — capture the full SDK schema `Value` for this table BEFORE the
        // snapshot loop consumes the descriptor (the value is what the shared SQLite
        // emitter consumes; conflicting declarations are caught on the snapshot in
        // the second pass, so storing per-descriptor here is safe — identical twins
        // store identical values).
        sqlite_schemas.insert(d.name.clone(), descriptor_to_sdk_schema(d));

        // §6.5 MANDATE — the per-column / per-index snapshot construction (system-
        // field injection, default rendering, encryption/comment sentinels, vector/
        // geo/FTS index modelling) lives in ONE place: the shared, dialect-
        // parameterized [`build_table_snapshot`]. The differ routes through it so
        // `IrAuthor::lower` can reuse the SAME builder and the §6.4 byte-identity
        // golden guards against accidental regression, not against two independent
        // implementations.
        let this = build_table_snapshot(project_schema, d, dialect)?;
        declarations
            .entry(d.name.clone())
            .or_default()
            .push((d.owner_app.clone(), this));
    }

    // Second pass: for each table, detect conflicts over the FULL declarer set and
    // pick the owner — both order-independent (1b).
    desired_snapshot_second_pass(declarations, sqlite_schemas)
}

/// §6.5 MANDATE — the **shared, dialect-parameterized snapshot-builder**: build the
/// full [`TableSnapshot`] (system-field columns + indexes, the `id` PRIMARY KEY,
/// per-field column/constraint/index modelling, and the dialect-divergent FTS
/// shape) for a single [`CollectionDescriptor`].
///
/// This is the single source of truth for the default / system-field / sentinel
/// logic. BOTH the declarative differ ([`desired_snapshot_for_dialect`], unchanged
/// behavior) and the IR path ([`crate::render::lower::IrAuthor`]) call it, so the
/// per-column/per-index construction exists in exactly ONE place (§6.5). The
/// extraction is BYTE-PRESERVING: the differ produces a byte-identical snapshot
/// before and after the lift (a refactor-safety fixture asserts this).
///
/// Only one facet is dialect-divergent — full-text search: PG folds a `.fts()`
/// field into a `__fts` generated tsvector column + GIN index; SQLite folds it
/// into an FTS5 virtual-table index. Every other facet is dialect-agnostic
/// (the `data_type` is always the PG `information_schema` spelling; the SQLite
/// comparison canonicalises it — see [`ddl_to_information_schema`] /
/// [`canonical_sqlite_type`]).
///
/// # Errors
/// - [`DeclarativeError::UnsupportedType`] — a field used an unknown type token.
/// - [`DeclarativeError::Invalid`] — a `ref` field's target is not a safe ident,
///   or a re-declared `id` field has a non-`id` type / malformed prefix.
pub(crate) fn build_table_snapshot(
    project_schema: &str,
    d: &CollectionDescriptor,
    dialect: SqlDialect,
) -> Result<TableSnapshot, DeclarativeError> {
        let mut columns = system_field_columns();
        // #6: the three implicit B-tree system indexes the platform auto-creates
        // for every table (`deleted_at`, `updated_at`, `created_by`), modelled so
        // they round-trip — see `system_field_indexes`.
        let mut indexes: Vec<IndexSnapshot> = system_field_indexes(&d.name);
        let mut constraints: Vec<ConstraintSnapshot> = Vec::new();

        // The id PRIMARY KEY (Postgres names a bare `PRIMARY KEY` constraint
        // `<table>_pkey`). Definition matches pg_get_constraintdef's spelling.
        constraints.push(ConstraintSnapshot {
            name: format!("{}_pkey", d.name),
            kind: "PRIMARY KEY".into(),
            definition: "PRIMARY KEY (id)".into(),
            comment: None,
        });
        // A PRIMARY KEY also materialises an IMPLICIT unique index named
        // `<table>_pkey` (pg_index reports it). The live snapshot always carries
        // it, so the desired snapshot must too — otherwise the differ would read
        // it as an out-of-band index to DROP. It is created by the `PRIMARY KEY`
        // clause, never by a standalone CREATE INDEX, so the differ skips it
        // (see `is_pk_index`).
        indexes.push(IndexSnapshot::btree(
            format!("{}_pkey", d.name),
            true,
            // The PK's implicit index covers `id` (live `pg_index` reports the
            // same key column). The differ never emits DDL for it (see
            // `is_pk_index`), but the snapshot must carry the column list so the
            // attribute-aware diff stays clean against live.
            vec!["id".into()],
        ));

        for f in &d.fields {
            // #5 id-fold: `id: t.id("prefix")` is a PREFIX DECLARATION for the
            // system `id` PK column already injected by `system_field_columns`,
            // NOT a second column. FOLD it: validate the declared prefix (defense
            // in depth — mirrors plugin-db `query.rs:648-653` + `validate_id_prefix`)
            // and SKIP it, so we neither duplicate the `id` column nor emit a
            // bogus second PK. A field NAMED `id` with any OTHER type is rejected
            // by the field-name fence below (an `id` column may only be the
            // system PK).
            if f.name == "id" {
                if f.ty == "id" {
                    if let Some(prefix) = &f.id_prefix {
                        validate_id_prefix(prefix)?;
                    }
                    // **MED-1 fail-closed** — the id-fold DISCARDS this field (it is a
                    // prefix declaration for the already-injected system PK, not a
                    // second column), so a column-level modifier carried on it is
                    // SILENTLY LOST. The op.* `ir_column_to_field` remaps ANY
                    // `id`-named `uuid` column to type `"id"`, so a hand-authored
                    // `id: t.uuid().unique()` / `id: t.uuid().default(<literal>)` would
                    // reach here and have its `unique` / `default` quietly swallowed —
                    // a fail-closed→silent-drop regression. REJECT those discarded
                    // modifiers instead.
                    //
                    // NOTE — only `unique` + a user `default` are checked, NOT
                    // nullability: the system PK is ALWAYS NOT NULL irrespective of the
                    // folded field's `required` flag, and the declarative `t.id(prefix)`
                    // descriptor legitimately leaves `required` at its default (`false`)
                    // — the NOT NULL is injected by `system_field_columns`, not carried
                    // on the field — so the fold ignoring `nullable` is correct, not a
                    // drop. A legitimate `t.id(prefix?)` carries NO user `default` (its
                    // synth `genRandomUuid` maps to `None`) and is never a column-level
                    // UNIQUE (the PK implies it), so this never fires for the real id
                    // shape — only for a modifier that would otherwise vanish.
                    if f.unique || f.default.is_some() {
                        return Err(DeclarativeError::Invalid(format!(
                            "field 'id' folds into the system primary key, so a \
                             column-level modifier on it would be silently discarded: \
                             {}{}— declare the id as a bare `t.id(prefix?)` (the system \
                             PK is already NOT NULL, unique, and DB-defaulted)",
                            if f.unique { "unique " } else { "" },
                            if f.default.is_some() { "default " } else { "" },
                        )));
                    }
                    continue;
                }
                if f.identity.is_some() {
                    if !matches!(f.ty.as_str(), "int" | "integer" | "bigInt") {
                        return Err(DeclarativeError::Invalid(format!(
                            "field 'id' may replace the system primary key only as an \
                             integer identity column, not '{}'",
                            f.ty
                        )));
                    }
                    let replacement = column_snapshot_for_field(f, dialect)?;
                    if let Some(existing) = columns.iter_mut().find(|c| c.name == "id") {
                        *existing = replacement;
                    }
                    continue;
                }
                return Err(DeclarativeError::Invalid(format!(
                    "field 'id' is reserved for the platform system primary key; a \
                     re-declaration must be `t.id(prefix?)` (type 'id') or an integer \
                     identity primary key, not '{}'",
                    f.ty
                )));
            }
            columns.push(column_snapshot_for_field(f, dialect)?);
            // A masked field (`.mask({...})`, or auto-mask on `t.encrypted`) gets a
            // hidden `<col>_masked TEXT` sibling column at CREATE time (resolved by
            // the SHARED kernel's `mask_sibling_column_for_field`). The sibling is a
            // real physical column, so the desired snapshot models it or it
            // phantom-drifts against the live table the engine creates. It round-
            // trips as a plain nullable TEXT column.
            //
            // **P4 HALF A** — the `__zsmask:kind=…,classification=…` sentinel that
            // plugin-db reads at RUNTIME (via `pg_description`) to drive the mask
            // read-pass is now EMITTED into the generated DDL: it rides on the
            // sibling column's `mask_sentinel`, which `render_create_table` /
            // `render_add_column` turn into a `COMMENT ON COLUMN` statement. Built
            // by the SHARED codec (`zeroship_schema::query::mask_sentinel_for_field`
            // → `build_mask_sentinel`) so it is byte-identical to the one
            // `registerModel` writes. `snapshot_schema` never introspects COMMENTs,
            // so the sentinel is not a snapshot drift attribute (excluded from
            // `ColumnSnapshot` equality) — the sibling COLUMN itself round-trips as
            // a plain nullable TEXT column.
            if mask_sibling_for_field(f).is_some() {
                let comment_sentinel =
                    zeroship_schema::query::mask_sentinel_for_field(&field_to_sdk_def(f));
                columns.push(ColumnSnapshot {
                    name: format!("{}_masked", f.name),
                    data_type: "text".into(),
                    nullable: true,
                    default: None,
                    generated: None,
                    identity: None,
                    encryption_sentinel: None,
                    comment_sentinel,
                    ..Default::default()
                });
            }
            // CHECK constraints (#3 literal-pin, #4 min/max + enum). These are
            // INLINED at CREATE TABLE (like plugin-db's `def_to_constraints`); the
            // declarative differ does not re-diff CHECK bodies (only FOREIGN KEY
            // bodies), so a CHECK round-trips at the name+kind level — its
            // pg_get_constraintdef-normalised body is not byte-compared (see the
            // round-trip tests). The `definition` carries the emitted DDL clause so
            // `render_create_table` can inline it.
            for chk in field_check_constraints(&d.name, f) {
                constraints.push(chk);
            }
            // A `unique: true` field becomes a unique index (A1 rule). The
            // name mirrors plugin-db's deterministic per-field index name.
            if f.unique {
                indexes.push(IndexSnapshot::btree(
                    unique_index_name(&d.name, &f.name),
                    true,
                    vec![f.name.clone()],
                ));
            }
            // **T12** — a vector field (`t.vector(dims, { metric })`) emits a
            // pgvector ANN index (`USING ivfflat` with the metric-appropriate
            // opclass). The live snapshot carries it as `access_method =
            // 'ivfflat'`, so the desired snapshot must model it identically or it
            // phantom-drops; routed through the shared `zeroship-schema` kernel so
            // the opclass + name match plugin-db's runtime form byte-for-byte.
            if f.ty == "vector" {
                if let Some(spec) = vector_index_snapshot(&d.name, f) {
                    indexes.push(spec);
                }
            }
            // **T13** — a geoPoint field (`t.geoPoint()`) emits a PostGIS GiST
            // spatial index over its `geography(POINT, 4326)` column. The live
            // snapshot carries it as `access_method = 'gist'`, so the desired
            // snapshot must model it identically or the runtime-created GiST index
            // phantom-drops (and spatial search degrades to a full scan). Mirrors
            // plugin-db's `SpatialIndex::ensure_spatial_index`.
            if f.ty == "geoPoint" {
                if let Some(spec) = geo_index_snapshot(&d.name, f) {
                    indexes.push(spec);
                }
            }
            // A `ref` field declares a FOREIGN KEY constraint.
            if f.ty == "ref" {
                if let Some(target) = &f.references {
                    // #2 cross-app: a `<otherApp>.<table>` schema-qualified
                    // target is REJECTED here, fail-closed (mirrors
                    // `crates/plugin-db/src/cross_app_fk.rs`): every FK must stay
                    // inside the project schema. Surfaced as a dedicated, clearer
                    // error for that shape before the generic bare-ident check.
                    reject_cross_app_ref(&d.name, target)?;
                    // #3-ref: the FK target table is interpolated into
                    // `REFERENCES <schema>.<target>(id)`; validate it as a bare
                    // identifier at the author boundary (mirroring how table /
                    // column names are checked) so a malformed / injecting ref
                    // target (`control.users`, `x"; DROP …`, `;`) is rejected
                    // up-front rather than relying on downstream quoting alone.
                    validate_ident("ref target", target)?;
                    constraints.push(ConstraintSnapshot {
                        name: fk_constraint_name(&f.name),
                        kind: "FOREIGN KEY".into(),
                        // EXACT `pg_get_constraintdef` spelling (#1): the target
                        // is schema-qualified, NO space before `(id)`, the policy
                        // clauses render `ON UPDATE <b>` THEN `ON DELETE <a>` (pg's
                        // canonical order, the reverse of the DDL), a `NO ACTION`
                        // action is OMITTED, and a deferrable FK ends with
                        // ` DEFERRABLE INITIALLY DEFERRED`. Built to match live
                        // byte-for-byte so a policy FK re-diffs clean — and the
                        // differ can compare FK bodies on existing tables (a
                        // changed target/policy is caught, not silently skipped).
                        definition: fk_definition_pg(
                            &f.name,
                            project_schema,
                            target,
                            f.on_delete.as_deref(),
                            f.on_update.as_deref(),
                            f.deferrable.unwrap_or(true),
                        ),
                        comment: None,
                    });
                }
            }
        }

        for idx in &d.indexes {
            // Carry the declared columns through VERBATIM (1a) — recovering them
            // from the index name was unsound for composite / custom-named
            // indexes. `render_create_index` emits this list directly.
            indexes.push(IndexSnapshot::btree(
                idx.name.clone(),
                idx.unique,
                idx.columns.clone(),
            ));
        }

        // **T12** — full-text search, DIALECT-AWARE (the only dialect-divergent
        // part of the snapshot):
        //
        // - **Postgres**: every `.fts()`-marked text column folds into ONE composite
        //   `__fts` GENERATED tsvector column + a `<coll>__fts_idx` GIN index
        //   (Q-P4-B, matching plugin-db's runtime `__fts` / `<coll>__fts_idx`
        //   contract the data plane's `fts_search` reads). The generated-column form
        //   is trigger-free, so the whole FTS shape is pure DDL the engine owns.
        // - **SQLite**: `tsvector` has no SQLite spelling, so there is NO `__fts`
        //   column and NO GIN index. Instead the FTS index is an FTS5 **virtual
        //   table** (`<coll>__fts`) over the source columns + AFTER triggers — the
        //   SAME structure plugin-db's runtime `ensure_fts_index` and the shared
        //   `zeroship_schema::fts_sqlite` builders produce. It is modelled as an
        //   `IndexSnapshot` with `access_method = "fts5"` over the SOURCE columns so
        //   the SQLite emitter emits the vtable+triggers and a live re-diff (the
        //   drift introspector recognises the vtable) round-trips ZERO-drift.
        //
        // Without modelling the engine-built FTS objects in `desired`, the live
        // ones would be unknown to the differ and phantom-dropped; modelling them
        // also makes the engine the authority that EMITS them (the schema-authority
        // cutover intent).
        match dialect {
            SqlDialect::Postgres => {
                if let Some((fts_col, fts_idx)) = fts_objects_pg(&d.name, &d.fields) {
                    columns.push(fts_col);
                    indexes.push(fts_idx);
                }
            }
            SqlDialect::Sqlite => {
                if let Some(fts_idx) = fts_index_snapshot_sqlite(&d.name, &d.fields) {
                    indexes.push(fts_idx);
                }
            }
            SqlDialect::Mysql => {}
        }

        // Deterministic ordering (snapshot_schema sorts everything by name).
        columns.sort_by(|a, b| a.name.cmp(&b.name));
        indexes.sort_by(|a, b| a.name.cmp(&b.name));
        constraints.sort_by(|a, b| a.name.cmp(&b.name));

        // An author-built DESIRED snapshot carries no raw CREATE text (it is
        // introspection-only; H1). It rides as `None` and is excluded from equality.
        Ok(TableSnapshot {
            columns,
            indexes,
            constraints,
            runtime_options: d.runtime_options.clone(),
            comment: None,
            stored_create_sql: None,
        })
}

/// Second pass of [`desired_snapshot_for_dialect`] — over the per-table
/// declarations accumulated by [`build_table_snapshot`], detect cross-app shape
/// conflicts and pick each table's owner. Both are order-independent (1b). Split
/// out so the byte-preserving snapshot-builder lift leaves the conflict/ownership
/// resolution untouched.
fn desired_snapshot_second_pass(
    declarations: BTreeMap<String, Vec<(String, TableSnapshot)>>,
    mut sqlite_schemas: BTreeMap<String, serde_json::Value>,
) -> Result<DesiredSchema, DeclarativeError> {
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    let mut ownership: BTreeMap<String, String> = BTreeMap::new();
    for (table, mut decls) in declarations {
        // A conflict iff ANY two declarers disagree in shape. Detect it over the
        // whole set (not the order-dependent first mismatch). Computed against the
        // first declaration's shape; the borrow ends before `decls` is consumed.
        // (Each table has ≥1 declaration — it only enters `declarations` via a
        // push — so `first()` is always Some; an empty set is skipped without a
        // panicking unwrap.)
        let conflict = match decls.first() {
            None => continue,
            Some((_, first_shape)) => decls.iter().any(|(_, shape)| shape != first_shape),
        };
        if conflict {
            // Report EVERY declaring app, sorted+deduped — the same result for any
            // permutation of the same descriptors.
            let mut apps: Vec<String> = decls.into_iter().map(|(app, _)| app).collect();
            apps.sort();
            apps.dedup();
            return Err(DeclarativeError::ConflictingDeclaration { table, apps });
        }
        // All declarations are byte-identical (idempotent). The owner is the
        // lexicographically-smallest declaring app so the union is order-independent
        // (same owner for any permutation). This tiebreak is NOT an ownership-spoof
        // vector: `owner_app` is the server-stamped id of the app whose deploy
        // produced the descriptor — the caller (control plane) concatenates each
        // app's descriptors stamped with that app's OWN id, so an app cannot inject
        // a descriptor bearing another app's id. And because the declarations are
        // byte-identical, the migrations either owner would author are identical
        // too, so the tiebreak is behaviourally inert beyond which app the
        // enforcement check names.
        let owner = decls.iter().map(|(app, _)| app.clone()).min().unwrap_or_default();
        // Take the first declaration's shape (all are identical); `swap_remove(0)`
        // avoids a panicking index and any extra clone.
        let (_, shape) = decls.swap_remove(0);
        ownership.insert(table.clone(), owner);
        tables.insert(table, shape);
    }

    // PHASE 4 — keep only the SDK schemas for tables that survived conflict
    // resolution (the keys of `tables`), so the side-map stays exactly aligned with
    // the snapshot.
    sqlite_schemas.retain(|table, _| tables.contains_key(table));

    let snapshot = SchemaSnapshot { tables, ..Default::default() };
    Ok(DesiredSchema {
        snapshot,
        ownership,
        sqlite_schemas,
    })
}

/// True if `index_name` is the implicit index a PRIMARY KEY materialises
/// (`<table>_pkey`). It is created/dropped by the PK clause, never by a
/// standalone CREATE/DROP INDEX, so the differ never emits DDL for it.
fn is_pk_index(table: &str, index_name: &str) -> bool {
    index_name == format!("{table}_pkey")
}

/// PHASE 4 — true if `index_name` is one of the three implicit system-field
/// indexes (`<table>_<col>_idx` for `deleted_at` / `updated_at` / `created_by`).
/// On the SQLite path these are emitted inline by the shared CREATE-TABLE emitter,
/// so the declarative differ must NOT also emit them as standalone migrations.
fn is_system_field_index(table: &str, index_name: &str) -> bool {
    SYSTEM_INDEXED_COLS
        .iter()
        .any(|col| index_name == non_unique_index_name(table, col))
}

/// True if `index_name` is an index the platform's CREATE-TABLE lowering injects
/// for `table` automatically — the implicit PRIMARY-KEY index (`<table>_pkey`) or
/// one of the three system-field indexes (`deleted_at`/`updated_at`/`created_by`).
///
/// The op.* `generate` synthesizer (`zeroship-migrate-js`) uses this to know which
/// desired-snapshot indexes it must NOT re-emit as standalone `createIndex` ops:
/// they are already materialised by `lower_create_table`, so emitting them again
/// would churn (a duplicate CREATE) and break re-diff-to-zero. Every OTHER
/// (user-authored) index must be synthesized — never silently dropped.
#[must_use]
pub fn is_system_managed_index(table: &str, index_name: &str) -> bool {
    is_pk_index(table, index_name) || is_system_field_index(table, index_name)
}

/// True if `constraint_name` is a constraint the platform's CREATE-TABLE lowering
/// injects for `table` automatically — currently just the implicit `id` PRIMARY KEY
/// (`<table>_pkey`). The op.* `generate` synthesizer uses this to know which
/// desired-snapshot constraints are platform-managed (skip) vs user-authored
/// (FK / CHECK — must be synthesized or fail-closed, never silently dropped).
#[must_use]
pub fn is_system_managed_constraint(table: &str, constraint_name: &str) -> bool {
    constraint_name == format!("{table}_pkey")
}

/// Deterministic name for a per-field unique index (`<table>_<field>_key`,
/// matching the Postgres convention so the desired snapshot round-trips to the
/// live one a `CREATE UNIQUE INDEX` of this name produces). Capped to ≤63 bytes
/// via [`crate::plan::author::cap_ident_name`] (1c) — an un-capped name would be
/// truncated server-side on CREATE, so the desired (full) name would never match
/// the live (truncated) name and a re-diff would churn DROP+CREATE forever.
fn unique_index_name(table: &str, field: &str) -> String {
    crate::plan::author::cap_ident_name(&format!("{table}_{field}_key"))
}

/// Deterministic FK constraint name (`<field>_fkey`, mirroring plugin-db's
/// `fk_constraint_name`).
fn fk_constraint_name(field: &str) -> String {
    format!("{field}_fkey")
}

/// §6.4 — build the [`ConstraintSnapshot`] for a stand-alone IR `addConstraint`
/// FK, in the EXACT shape the differ's deferred-FK path carries: the canonical
/// `pg_get_constraintdef`-shaped `definition` (via [`fk_definition_pg`]) + the
/// deterministic `<field>_fkey` name (or an explicit name). `IrAuthor` calls this
/// so the FK body is built by the SAME helper the differ uses — never re-spelled —
/// and `lower_add_fk` then renders it byte-identically to a deferred FK.
///
/// Single-column FK only in PR1 (the `t.*`/`op.*` `ref` shape is single-column,
/// referencing the target's `id`); a multi-column FK is a later wave.
pub(crate) fn ir_fk_constraint_snapshot(
    project_schema: &str,
    explicit_name: Option<&str>,
    local_column: &str,
    references_table: &str,
    on_delete: Option<&str>,
    on_update: Option<&str>,
) -> ConstraintSnapshot {
    let name = explicit_name
        .map(ToString::to_string)
        .unwrap_or_else(|| fk_constraint_name(local_column));
    // C1 — the referential actions ARE rendered. When both are absent the
    // `None,None` defaults reproduce the differ's `ref` FK byte-for-byte
    // (`declarative.rs:1325`): NO ACTION on_delete/on_update (rendered RESTRICT by
    // the shared `normalize_fk_action`) + `deferrable = true` (the ref default),
    // so an action-free stand-alone IR FK stays byte-identical to the FK the
    // differ builds for the same single-column reference. A set action threads
    // through `fk_definition_pg` (which already accepts both) → the rendered
    // definition carries `ON DELETE CASCADE` / `ON UPDATE …` per the action token.
    let definition = fk_definition_pg(
        local_column,
        project_schema,
        references_table,
        on_delete,
        on_update,
        true,
    );
    ConstraintSnapshot { name, kind: "FOREIGN KEY".to_string(), definition, comment: None }
}

// ---------------------------------------------------------------------------
// T12 — vector-ANN + full-text search index modeling.
//
// The differ never modeled the access-method dimension, so the live ivfflat
// (vector) and GIN (FTS) indexes the data plane built were UNKNOWN to it and
// phantom-DROPped on every diff (and a `btree → ivfflat` method flip was
// invisible). Modeling them in the DESIRED snapshot both stops the drop AND
// makes the engine the authority that EMITS them (the schema-authority cutover
// intent). The PG access-method names (`ivfflat`, `gin`) and the deterministic
// index/column names match the shared `zeroship-schema` kernel + plugin-db's
// runtime contract byte-for-byte, so an engine-created object round-trips clean.
// ---------------------------------------------------------------------------

/// The pgvector opclass for a metric token (mirrors plugin-db's
/// `ensure_vector_index` mapping). `None` is never returned — an unknown / absent
/// token folds to the SDK default (`cosine`), matching the shared kernel.
fn vector_opclass(metric: Option<&str>) -> &'static str {
    match metric {
        Some("l2") => "vector_l2_ops",
        Some("innerProduct") | Some("ip") => "vector_ip_ops",
        _ => "vector_cosine_ops",
    }
}

/// Build the [`IndexSnapshot`] for a `t.vector(...)` field's ANN index, or `None`
/// if the field is not actually a vector field. The live index introspects as
/// `access_method = 'ivfflat'` over the single vector column (no `indexprs`, so
/// `expression` stays `None` and the index round-trips clean). The opclass rides
/// on the emission-only `opclass` field — excluded from drift equality — so
/// `render_create_index` can spell `USING ivfflat ("col" <opclass>)`.
fn vector_index_snapshot(table: &str, f: &FieldDescriptor) -> Option<IndexSnapshot> {
    if f.ty != "vector" {
        return None;
    }
    Some(IndexSnapshot {
        // `<table>_<col>_idx`, matching `zeroship_schema::query::index_name`
        // (= plugin-db `ensure_vector_index`'s name).
        name: non_unique_index_name(table, &f.name),
        unique: false,
        columns: vec![f.name.clone()],
        elements: vec![IndexElementSnapshot::column(f.name.clone())],
        access_method: "ivfflat".to_string(),
        predicate: None,
        opclass: Some(vector_opclass(f.vector_metric.as_deref()).to_string()),
        comment: None,
    })
}

/// **T13** — a geoPoint field (`t.geoPoint()`) emits a PostGIS spatial index
/// (`USING GIST`) over the `geography(POINT, 4326)` column, mirroring plugin-db's
/// runtime `SpatialIndex::ensure_spatial_index`
/// (`crates/plugin-db/src/backend/postgres.rs`). The live snapshot carries it as
/// `access_method = 'gist'`, so the desired snapshot must model it identically or
/// the runtime-created GiST index phantom-drops (and spatial `ST_DWithin` search
/// falls back to a full table scan). The index name is the same
/// `<table>_<col>_idx` `non_unique_index_name` / `zeroship_schema::query::index_name`
/// produce; no opclass and no storage params (`render_create_index` spells the
/// bare `USING gist ("col")`).
fn geo_index_snapshot(table: &str, f: &FieldDescriptor) -> Option<IndexSnapshot> {
    if f.ty != "geoPoint" {
        return None;
    }
    Some(IndexSnapshot {
        name: non_unique_index_name(table, &f.name),
        unique: false,
        columns: vec![f.name.clone()],
        elements: vec![IndexElementSnapshot::column(f.name.clone())],
        access_method: "gist".to_string(),
        predicate: None,
        opclass: None,
        comment: None,
    })
}

/// The fixed name of the composite full-text tsvector column + its GIN index,
/// matching plugin-db's runtime contract (`__fts` column read by `fts_search`,
/// `<coll>__fts_idx` GIN index).
fn fts_column_name() -> &'static str {
    "__fts"
}
fn fts_index_name(table: &str) -> String {
    format!("{table}__fts_idx")
}

/// The tsvector configuration (language) for a collection's FTS index: the first
/// non-empty `ftsLanguage` declared on any `.fts()` field, else `english` (the
/// SDK default). Mirrors `zeroship_schema::query::build_create_indexes`'s
/// first-non-empty-wins rule.
fn fts_language(fields: &[FieldDescriptor]) -> String {
    fields
        .iter()
        .filter(|f| f.fts)
        .find_map(|f| f.fts_language.clone().filter(|l| !l.is_empty()))
        .unwrap_or_else(|| "english".to_string())
}

/// **Postgres FTS** — build the generated `__fts` tsvector COLUMN + its GIN index
/// for the `.fts()`-marked text columns of a collection, or `None` when the
/// collection has no FTS fields.
///
/// The column is `GENERATED ALWAYS AS (to_tsvector('<lang>'::regconfig,
/// coalesce("c1",''::text) || ' '::text || …)) STORED` — a trigger-free,
/// fully-declarative form the engine owns end-to-end (plugin-db's runtime form
/// used a `tsvector_update_trigger`, which is not declarative; the engine, as the
/// sole schema authority, replaces it). The index is `USING gin("__fts")`. The
/// `__fts` column name + `<coll>__fts_idx` index name are the contract
/// `fts_search` reads, so they are preserved.
fn fts_objects_pg(
    table: &str,
    fields: &[FieldDescriptor],
) -> Option<(ColumnSnapshot, IndexSnapshot)> {
    let fts_cols: Vec<&FieldDescriptor> = fields.iter().filter(|f| f.fts).collect();
    if fts_cols.is_empty() {
        return None;
    }
    let language = fts_language(fields);
    // `coalesce("c1",'') || ' ' || coalesce("c2",'') …` over the source columns,
    // in declared order. Mirrors plugin-db's `coalesce_concat`.
    let concat = fts_cols
        .iter()
        .map(|f| format!("coalesce({}, ''::text)", quote_ident(&f.name)))
        .collect::<Vec<_>>()
        .join(" || ' '::text || ");
    let generation_expr =
        format!("to_tsvector('{language}'::regconfig, {concat})");
    let col = ColumnSnapshot {
        name: fts_column_name().to_string(),
        data_type: "tsvector".to_string(),
        nullable: true,
        // The GENERATED expression rides on `default` (emission-only metadata —
        // `render_create_table` / `render_add_column` turn it into the
        // `GENERATED ALWAYS AS (...) STORED` clause via the `GENERATED:` prefix).
        // `snapshot_schema` does not introspect generation expressions, so it is
        // NOT a drift attribute (excluded from `ColumnSnapshot` equality) — the
        // `__fts` COLUMN itself round-trips as a plain nullable tsvector column.
        default: Some(format!("{GENERATED_PREFIX}{generation_expr}")),
        ddl_type_override: None,
        inline_checks: Vec::new(),
        generated: None,
        identity: None,
        encryption_sentinel: None,
        comment_sentinel: None,
        comment: None,
    };
    let idx = IndexSnapshot {
        name: fts_index_name(table),
        unique: false,
        columns: vec![fts_column_name().to_string()],
        elements: vec![IndexElementSnapshot::column(fts_column_name())],
        access_method: "gin".to_string(),
        predicate: None,
        opclass: None,
        comment: None,
    };
    Some((col, idx))
}

/// The `access_method` sentinel that marks an [`IndexSnapshot`] as the SQLite
/// FTS5 virtual table (vs. a PG `gin`/`gist`/`ivfflat` index or a plain `btree`).
/// The SQLite emitter ([`SqliteEmitter::create_index`]) branches on this to emit
/// the `CREATE VIRTUAL TABLE … USING fts5(...)` + AFTER triggers instead of a
/// plain `CREATE INDEX`, and the SQLite drift introspector stamps it on the live
/// vtable it recognises — so an FTS index round-trips ZERO-drift.
pub(crate) const SQLITE_FTS5_ACCESS_METHOD: &str = "fts5";

/// The name of the SQLite FTS5 virtual table for a collection (`<coll>__fts`).
/// Matches [`zeroship_schema::fts_sqlite::fts_vtable_name`] and plugin-db's runtime
/// `ensure_fts_index` contract, so an engine-emitted vtable and a runtime-built one
/// are interchangeable. NOTE: this is DELIBERATELY the bare `<coll>__fts` (the
/// vtable name), NOT the PG `<coll>__fts_idx` index name — on SQLite the FTS index
/// *is* the vtable, and the drift introspector reads the vtable's name back.
fn sqlite_fts_vtable_name(table: &str) -> String {
    zeroship_schema::fts_sqlite::fts_vtable_name(table)
}

/// **SQLite FTS** — model a collection's `.fts()` fields as the FTS5 virtual-table
/// [`IndexSnapshot`] the SQLite emitter produces, or `None` when the collection has
/// no FTS fields.
///
/// Shape: `access_method = "fts5"`, `name = "<coll>__fts"` (the vtable), `columns =`
/// the SOURCE columns (in declared order — NOT a `__fts` generated column, which
/// has no SQLite spelling). The SQLite drift introspector parses the live vtable's
/// `fts5(...)` column list back to this exact list, so a re-diff is zero-drift.
fn fts_index_snapshot_sqlite(
    table: &str,
    fields: &[FieldDescriptor],
) -> Option<IndexSnapshot> {
    let cols: Vec<String> = fields
        .iter()
        .filter(|f| f.fts)
        .map(|f| f.name.clone())
        .collect();
    if cols.is_empty() {
        return None;
    }
    Some(IndexSnapshot {
        name: sqlite_fts_vtable_name(table),
        unique: false,
        elements: cols.iter().cloned().map(IndexElementSnapshot::column).collect(),
        columns: cols,
        access_method: SQLITE_FTS5_ACCESS_METHOD.to_string(),
        predicate: None,
        opclass: None,
        comment: None,
    })
}

/// Validate a creator-declared typed-id prefix (`t.id("blog")`, #5).
///
/// Schema-authority P2: DELEGATES to the shared kernel's
/// [`zeroship_schema::query::validate_id_prefix`] (the single source of truth for
/// the `^[a-z][a-z0-9_]*$` rule + the `RESERVED_ID_PREFIXES` fence — the engine's
/// own copy of both is deleted). The shared check returns its `QueryError`; this
/// thin wrapper maps a failure to the engine's [`DeclarativeError::Invalid`] so
/// the author-boundary error type is unchanged.
fn validate_id_prefix(prefix: &str) -> Result<(), DeclarativeError> {
    zeroship_schema::query::validate_id_prefix(prefix)
        .map_err(|e| DeclarativeError::Invalid(e.to_string()))
}

/// Normalise a DSL FK action token to the SQL keyword Postgres reports.
///
/// Schema-authority P2: DELEGATES to the shared kernel's
/// [`zeroship_schema::query::normalize_fk_action`] (the SDK `FkAction` tokens
/// fold to `RESTRICT`/`CASCADE`/`SET NULL`/`NO ACTION`; anything unrecognised →
/// `RESTRICT`, fail-safe). The engine's own copy is deleted.
fn normalize_fk_action(s: Option<&str>) -> &'static str {
    zeroship_schema::query::normalize_fk_action(s)
}

/// Build a FOREIGN KEY definition body in the EXACT spelling
/// `pg_get_constraintdef(oid)` renders, so the desired snapshot round-trips to
/// the live introspected constraint (#1).
///
/// Empirically (probed against PG 17), `pg_get_constraintdef` renders a FK as:
///
/// ```text
/// FOREIGN KEY (<col>) REFERENCES <schema>.<target>(id)[ ON UPDATE <u>][ ON DELETE <d>][ DEFERRABLE INITIALLY DEFERRED]
/// ```
///
/// with two normalisations the DDL spelling does NOT have:
/// - **`ON UPDATE` precedes `ON DELETE`** (the reverse of plugin-db's emitted
///   DDL, which writes `ON DELETE <d> ON UPDATE <u>`); and
/// - a **`NO ACTION`** action clause is **OMITTED entirely** (it is the catalog
///   default — `confdeltype`/`confupdtype` = `'a'`), so a FK with both actions
///   `NO ACTION` renders with no action clauses at all.
///
/// `RESTRICT`, `CASCADE`, and `SET NULL` are rendered explicitly. The actions
/// pass through [`normalize_fk_action`] first (matching plugin-db's
/// emit-time normalisation), so the same DSL tokens land on the same keywords.
fn fk_definition_pg(
    field: &str,
    project_schema: &str,
    target: &str,
    on_delete: Option<&str>,
    on_update: Option<&str>,
    deferrable: bool,
) -> String {
    use std::fmt::Write as _;
    // **PR10 review (LOW)** — quote the referenced schema + table the SAME way
    // `pg_get_constraintdef` does (conditional: bare for safe lowercase names,
    // double-quoted for reserved-word/mixed-case), so the desired FK body matches
    // the live catalog byte-for-byte (over-quoting would phantom-diff a normal
    // lowercase `parent`) AND a reserved-word/mixed-case schema or target resolves
    // correctly instead of being emitted as a bare keyword.
    //
    // The LOCAL FK column is quoted the SAME conditional way as the schema/target
    // (and as the UNIQUE/PK body via `constraintdef_cols`): `pg_get_constraintdef`
    // renders `FOREIGN KEY ("order")` for a reserved-word/mixed-case column, so a
    // raw `FOREIGN KEY (order)` would phantom-diff the FK `definition` (the fold
    // reuses it, and `ConstraintSnapshot` has FULL Eq) AND mis-resolve `order` as
    // the keyword. Over-quoting a safe lowercase column would equally phantom-diff
    // the catalog's bare body — hence conditional (`quote_ident_if_needed`).
    let mut def = format!(
        "FOREIGN KEY ({}) REFERENCES {}.{}(id)",
        quote_ident_if_needed(field),
        quote_ident_if_needed(project_schema),
        quote_ident_if_needed(target),
    );
    let on_update = normalize_fk_action(on_update);
    let on_delete = normalize_fk_action(on_delete);
    // pg renders ON UPDATE before ON DELETE, and omits a NO ACTION clause.
    if on_update != "NO ACTION" {
        let _ = write!(def, " ON UPDATE {on_update}");
    }
    if on_delete != "NO ACTION" {
        let _ = write!(def, " ON DELETE {on_delete}");
    }
    if deferrable {
        def.push_str(" DEFERRABLE INITIALLY DEFERRED");
    }
    def
}

/// Reject a `ref` whose target is schema-qualified with a `<otherApp>.` prefix —
/// a cross-app FK (#2), forbidden fail-closed (mirrors
/// `crates/plugin-db/src/cross_app_fk.rs`: every FK stays inside one app's
/// namespace). A bare collection name is a same-project ref and is allowed.
///
/// The engine's project-umbrella model puts every member app's tables in ONE
/// project schema, so a legitimate cross-*app* (same-project) FK is just a bare
/// reference to another app's table in the union — the qualified `<app>.<table>`
/// form is exactly the disallowed cross-schema escape. (`validate_ident` would
/// also reject the `.`, but this gives the precise, actionable error.)
fn reject_cross_app_ref(table: &str, target: &str) -> Result<(), DeclarativeError> {
    if let Some((prefix, _)) = target.split_once('.') {
        return Err(DeclarativeError::CrossAppFkForbidden {
            table: table.to_string(),
            target: target.to_string(),
            other_app: prefix.to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// A failure to diff a declarative desired schema against the live one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeclarativeError {
    /// A descriptor name/type was not a safe bare identifier / type at the
    /// author boundary (mirrors [`crate::render::expand_contract`]'s `validate_ident` /
    /// `validate_type`). Nothing is generated.
    #[error("invalid descriptor: {0}")]
    Invalid(String),
    /// The diff requires an op v1 does not generate: an in-place INDEX or
    /// FOREIGN KEY redefinition (a flipped `unique` flag, a changed column set, a
    /// re-pointed FK target — each a DROP+CREATE deferred to a later phase).
    /// Surfaced explicitly — never silently skipped. (Type/nullability changes are
    /// handled in P3 as gated/ungated ALTERs; destructive DROPs are P2 gated
    /// migrations — neither uses this error.)
    #[error("unsupported in v1 (deferred to a later phase): {0}")]
    UnsupportedInV1(String),
    /// A declared field used a DSL type token the v1 differ does not map. This
    /// covers both out-of-scope parameterised/extension types
    /// (`vector`/`geoPoint`/`encrypted`) AND typos / wrong spellings
    /// (`bigint`, `uuid`, `int4`, `serial`, …). It is rejected at the author
    /// boundary BEFORE any SQL is emitted, rather than silently degrading to a
    /// `text` column (#2 — the creator declared X, would have got `text`, with
    /// permanent divergence from what plugin-db materialises).
    #[error(
        "unsupported field type '{ty}' (not mapped in v1; vector/geoPoint/encrypted \
         are out of v1 scope). Supported: string, number, boolean, date, calendarDate, \
         json, object, array, union, ref, bytes, actor, id"
    )]
    UnsupportedType {
        /// The unrecognised / out-of-scope DSL type token.
        ty: String,
    },
    /// Two or more apps declared the same table with DIFFERENT shapes (P4, design
    /// §4). One table has exactly one owner; an identical re-declaration is
    /// idempotent (merged) but a conflicting one is a hard deploy error — never a
    /// silent last-writer-wins merge (this refines #6's blanket `DuplicateTable`).
    ///
    /// `apps` carries EVERY app that declared this table (sorted, deduplicated),
    /// not just the first-detected pair. This makes the report **deterministic
    /// regardless of descriptor order** even with 3+ declarers: the merge no
    /// longer reports `order_pair(slot_owner, latecomer)` on the first mismatch
    /// (whose `slot_owner` flapped with input order when two identical twins
    /// raced for the slot — 1b). The full sorted declarer set is the same for
    /// every permutation of the same descriptors.
    #[error(
        "conflicting declaration of table '{table}': apps {apps:?} declare it with \
         differing shapes (a table has exactly one owner; identical re-declaration \
         is idempotent, a conflicting one is a deploy error)"
    )]
    ConflictingDeclaration {
        /// The table declared with conflicting shapes.
        table: String,
        /// EVERY app that declared this table, sorted ascending and deduplicated.
        /// Order-independent: the same set for any permutation of the descriptors.
        apps: Vec<String>,
    },
    /// The deploying app tried to make a structural change (CREATE/ALTER/DROP) to
    /// a table it does NOT own (P4 ownership enforcement, design §4). The
    /// declaring app owns a table's migrations; a non-owner may USE the table's
    /// rows freely but may NOT migrate its structure. (An IDENTICAL re-declaration
    /// by a non-owner produces no diff op and never trips this — only an actual
    /// structural change to a non-owned table is refused.)
    #[error(
        "app '{deploying_app}' may not migrate table '{table}' (owned by \
         '{owner}'): a non-owner may use a table but not alter its structure"
    )]
    NotTableOwner {
        /// The table the deploying app tried to change.
        table: String,
        /// The app that owns the table's migrations.
        owner: String,
        /// The app attempting the structural change.
        deploying_app: String,
    },
    /// The diff would emit a `DROP TABLE` for a live table absent from the union,
    /// but the differ **cannot confirm** the deploying app owns it: the caller's
    /// `live_ownership` map carries NO entry for that live table (2b). Rather than
    /// author a destructive drop of a table whose ownership it cannot verify, the
    /// differ **fails closed** — refusing the drop. This is the defence against a
    /// PARTIAL-union deploy (a caller that passed only one app's descriptors, so
    /// every OTHER app's live table looks "absent from desired"): the omitted
    /// tenants' tables are refused, never mass-dropped under the deploying app's
    /// authority. The fix is to supply the COMPLETE project union AND a
    /// `live_ownership` entry for every live table (see the `plan_declarative`
    /// caller contract).
    #[error(
        "refusing to drop live table '{table}': its ownership is unknown to this \
         diff (no live_ownership entry). The differ fails closed rather than author \
         a destructive drop it cannot confirm belongs to the deploying app — pass \
         the complete project union plus a live_ownership entry for every live table"
    )]
    DropOfUnownedTable {
        /// The live table whose ownership the caller did not supply.
        table: String,
    },
    /// A `ref` field declared a cross-app FK whose **target table is not in the
    /// union schema** (P4 cross-app FK, design §4). A cross-app FK may reference a
    /// table owned by another app, but that table must exist in the project's
    /// union (declared by SOME member app); an FK to a table no app declares is a
    /// clear error surfaced here rather than failing as bad SQL at apply.
    #[error(
        "table '{table}' declares a foreign key to '{target}', which no app in the \
         project declares (a cross-app FK target must exist in the union schema)"
    )]
    CrossAppFkTargetMissing {
        /// The table declaring the dangling FK.
        table: String,
        /// The FK target table that is absent from the union.
        target: String,
    },
    /// A [`RenameHint`] (P3) did not match an actual drop+add pair: the `from`
    /// column is not present in live as a dropped column, OR the `to` column is
    /// not present in desired as an added column, on the named table. The hint is
    /// the creator's signed statement of intent, so an un-matchable hint is a hard
    /// error — never silently ignored (a silently-dropped hint would fall back to
    /// an unintended gated-drop + additive-add, losing the column's data).
    #[error(
        "rename hint {table}.{from} → {to} does not match a drop+add pair \
         (from must be a live-only column and to a desired-only column on {table})"
    )]
    RenameHintUnmatched {
        /// The table the hint named.
        table: String,
        /// The `from` column the hint named (expected: live-only).
        from: String,
        /// The `to` column the hint named (expected: desired-only).
        to: String,
    },
    /// A [`RenameHint`] (P3) matched a drop+add pair whose **types differ**: the
    /// live `from` column and the desired `to` column do not share a `data_type`.
    /// A pure online rename (expand-contract dual-write) requires type identity —
    /// a simultaneous rename + type change is two distinct intents and is refused
    /// rather than silently mirrored across incompatible types (which the
    /// dual-write `NEW.<to> := NEW.<from>` assignment could corrupt or reject).
    #[error(
        "rename hint {table}.{from} → {to} matched, but the types differ \
         ({from_type} → {to_type}); a rename requires type identity (rename + \
         type change is two separate intents)"
    )]
    RenameHintTypeMismatch {
        /// The table the hint named.
        table: String,
        /// The `from` column.
        from: String,
        /// The `to` column.
        to: String,
        /// The live `from` column's data type.
        from_type: String,
        /// The desired `to` column's data type.
        to_type: String,
    },
    /// **PR9c LOW (ii)** — a SQLite catalog-sourced (`apply_bundle_ir_sqlite_catalog`)
    /// renameColumn whose POST-rename descriptor `to` field declares a
    /// **data-transforming facet** (encryption / mask / `default` / `enum` / `check`
    /// range) the rebuild cannot certify was already present on the live `from`
    /// column. The rebuild renders the new table's CREATE from the descriptor `to`
    /// def while value-copying the live `from` bytes VERBATIM (no transform). A
    /// rename preserves facets by contract, so a descriptor that simultaneously
    /// CHANGES a facet on the renamed column would apply the new facet's shape to
    /// un-transformed old bytes (e.g. rebuild an `encrypted` column over plaintext,
    /// or stamp an `enum`/`check` the old values may violate). The live catalog read
    /// does NOT recover SDK-level facets for the `from` column, so the rebuild cannot
    /// prove preservation — it FAILS CLOSED rather than silently rebuild under a
    /// changed facet. (The pre-rename-descriptor path keeps the `from` facets and is
    /// unaffected; only the post-rename catalog path hits this.) Rename + facet change
    /// is two intents: do the rename, then a separate facet-change deploy.
    #[error(
        "renameColumn {table}.{from} → {to}: the post-rename descriptor declares a \
         data-transforming facet ({facet}) on the renamed column, but the live `{from}` \
         column's facets cannot be recovered from the catalog to certify it was already \
         present — refusing to rebuild under a changed facet over verbatim-copied bytes \
         (do the rename and the facet change as separate deploys)"
    )]
    RenameHintFacetMismatch {
        /// The table the rename named.
        table: String,
        /// The `from` column.
        from: String,
        /// The `to` column.
        to: String,
        /// Which data-transforming facet the descriptor `to` def declared
        /// (`encrypted` / `mask` / `default` / `enum` / `check`).
        facet: &'static str,
    },
    /// Two [`RenameHint`]s on the same table shared a `from` (e.g. `[a→c, a→d]`)
    /// or a `to` (e.g. `[a→c, b→c]`) column. Each hint resolves INDEPENDENTLY, so
    /// a shared endpoint produces two colliding expand-contract sequences: a
    /// duplicated `ADD COLUMN <to>` (the second fails `already exists`), divergent
    /// dual-write triggers, or a double `DROP COLUMN <from>`. The cross-hint
    /// validation pass rejects it before any SQL is authored (H1). `side` is
    /// `"from"` or `"to"` — which endpoint was duplicated.
    #[error(
        "duplicate rename hint endpoint: column {table}.{column} appears as the \
         {side} of more than one hint; a column may be renamed at most once per \
         deploy"
    )]
    DuplicateRenameHint {
        /// The table the colliding hints named.
        table: String,
        /// The column that appeared more than once on the same side.
        column: String,
        /// Which endpoint collided: `"from"` or `"to"`.
        side: &'static str,
    },
    /// A [`RenameHint`]'s `to` equals another hint's `from` on the same table
    /// (e.g. `[a→b, b→c]`) — a rename CHAIN. Chains are not supported: the engine
    /// resolves each hint against the single live/desired snapshot pair, where the
    /// intermediate name (`b`) cannot be simultaneously a live-only drop and a
    /// desired-only add. Reject it EXPLICITLY rather than leave it to surface
    /// incidentally as an [`DeclarativeError::RenameHintUnmatched`] (H2).
    #[error(
        "rename hint chain on {table}: column {column} is both the target of one \
         hint and the source of another; chained renames are unsupported (resolve \
         them as separate deploys)"
    )]
    RenameHintChained {
        /// The table the chained hints named.
        table: String,
        /// The intermediate column that is both a `to` and a `from`.
        column: String,
    },
    /// A [`RenameHint`] had `from == to` — a no-op rename of a column to its own
    /// name. This is rejected with a PRECISE error rather than the misleading
    /// [`DeclarativeError::RenameHintUnmatched`] it would otherwise produce (the
    /// identical name is neither live-only nor desired-only) (M1).
    #[error(
        "no-op rename hint on {table}: from and to are the same column ({column}); \
         a rename must change the column name"
    )]
    RenameHintNoop {
        /// The table the hint named.
        table: String,
        /// The identical `from`/`to` column name.
        column: String,
    },
    /// Authoring the expand-contract rename sequence for a matched [`RenameHint`]
    /// failed (e.g. an identifier that passed the declarative author boundary was
    /// rejected by the stricter expand-contract author boundary). Surfaced rather
    /// than swallowed.
    #[error("failed to author rename expand-contract sequence: {0}")]
    Rename(#[from] ExpandContractError),
    /// A `ref` field's target was a schema-qualified `<otherApp>.<table>` — a
    /// CROSS-APP foreign key, forbidden fail-closed (#2, mirrors
    /// `crates/plugin-db/src/cross_app_fk.rs`). Every FK must stay inside the
    /// project schema; a reference to another member app's table is the BARE
    /// collection name (the union puts every app's tables in one schema), never a
    /// dot-qualified one. Caught at the author/plan boundary BEFORE any SQL is
    /// rendered, so a cross-schema escape can never reach DDL.
    #[error(
        "table '{table}' declares a cross-app foreign key to '{target}' (app \
         '{other_app}'): a foreign key may not cross an app/schema boundary — \
         reference a table in this project by its bare name"
    )]
    CrossAppFkForbidden {
        /// The table declaring the forbidden cross-app FK.
        table: String,
        /// The schema-qualified `<otherApp>.<table>` target.
        target: String,
        /// The `<otherApp>` schema prefix that crossed the boundary.
        other_app: String,
    },
    /// PHASE 4 — the Confined SQLite path needs an FK inlined at CREATE TABLE
    /// (SQLite has no `ALTER TABLE … ADD CONSTRAINT FOREIGN KEY`), but the FK's
    /// target table is neither already live nor created earlier in THIS batch — so
    /// it cannot be inlined and SQLite cannot add it later without a full table
    /// rebuild (the 12-step rebuild, P3b — out of PHASE-4 scope). Surfaced as a
    /// clear typed error rather than emitting an invalid `ALTER ADD CONSTRAINT`
    /// (which the SQLite authorizer/engine would reject anyway) or silently
    /// dropping the FK.
    #[error(
        "SQLite cannot defer the foreign key on table '{table}' → '{target}': SQLite \
         has no ALTER TABLE ADD CONSTRAINT, and the target is not live nor created \
         earlier in this batch (a table rebuild is required — P3b)"
    )]
    SqliteDeferredFkUnsupported {
        /// The table whose FK could not be inlined.
        table: String,
        /// The FK's target table (not yet available to inline against).
        target: String,
    },
    /// **Reserved fail-closed guard (P3b).** A Confined-SQLite existing-table change
    /// that the 12-step rebuild genuinely cannot express. As of P3b the rebuild
    /// DOES handle the previously-deferred ops — a column TYPE change, a nullability
    /// change (either direction), a column RENAME, an ADD/DROP CONSTRAINT, and an
    /// in-place FK redefinition — so those now flow through
    /// [`DeclarativePlan::rebuilds`] instead of surfacing here. This variant remains
    /// as the fail-closed boundary for any future existing-table op the rebuild
    /// author cannot yet emit: the engine refuses to emit dangling Postgres DDL on
    /// the SQLite path, surfacing a clear typed error rather than a silent pass.
    #[error(
        "SQLite cannot perform the existing-table change on '{table}' natively \
         ({op}): it has no rebuild expression. The engine refuses to emit dangling \
         Postgres DDL on the SQLite path. Author a compensating migration."
    )]
    SqliteRebuildRequired {
        /// The existing table the rebuild-needing change targets.
        table: String,
        /// The specific operation that has no rebuild expression (human-readable).
        op: String,
    },
}

// ---------------------------------------------------------------------------
// The structured diff result.
// ---------------------------------------------------------------------------

/// The **structured** result of [`DeclarativeAuthor::diff`].
///
/// It carries the plain (additive / destructive) migrations PLUS the online
/// renames, each kept as its full [`ExpandContractPlan`] and NOT flattened into
/// the plain set.
///
/// # Why a declarative rename must NOT be flattened (C1 — data loss)
///
/// A column rename is an **online, multi-deploy** operation, not a single
/// statement. Its [`ExpandContractPlan`] is more than a list of `Migration`s: it
/// also carries the [`BackfillSpec`](crate::model::backfill::BackfillSpec) that mirrors
/// **pre-existing** rows from `<from>` into `<to>`. E3's `up` is only a `SELECT 1`
/// marker — the actual data copy is [`run_backfill`](crate::apply::backend::postgres::backfill::run_backfill),
/// driven exclusively by [`run_expand`](crate::engine::MigrationEngine::run_expand).
///
/// If the rename were flattened into the plain migration set (`out.extend(plan.all())`)
/// and pushed through `plan` → `executor::apply`, the backfill would NEVER run:
/// E3's marker journals as "done" without the rows ever being copied, and the
/// contract `DROP COLUMN <from>` then destroys the originals → **data loss**.
/// (A flat batch is also dead-on-arrival: the executor's expand/contract gate
/// refuses the contract while its own expand is still pending.)
///
/// So the differ keeps renames structured. The caller drives them through
/// [`MigrationEngine::apply_declarative`](crate::engine::MigrationEngine::apply_declarative),
/// which runs the REAL backfill and surfaces the contract as a DEFERRED set for a
/// later deploy.
#[derive(Debug, Clone, Default)]
pub struct DeclarativePlan {
    /// The plain additive / destructive migrations (CREATE TABLE, ADD/DROP
    /// COLUMN, indexes, FKs, type / nullability changes). A rename's `<from>` is
    /// EXCLUDED from the destructive drop pass (its drop is the deferred contract)
    /// and its `<to>` is EXCLUDED from the additive add pass (the expand adds it).
    pub migrations: Vec<Migration>,
    /// The online renames, each as a full [`ExpandContractPlan`] (expand migs +
    /// `BackfillSpec` + contract migs). NEVER flattened into `migrations`.
    pub renames: Vec<ExpandContractPlan>,
    /// **P3b (SQLite only)** — the existing-table changes that SQLite has no native
    /// `ALTER` for (type change, nullability change, column RENAME's rebuild,
    /// ADD/DROP CONSTRAINT, in-place FK redefinition). Each is a 12-step table
    /// rebuild ([`SqliteRebuildSpec`]) paired with its journal [`Migration`]. NOT
    /// flattened into `migrations`: a rebuild is not a single `up` statement — it is
    /// a structured engine-mode operation with `foreign_keys` toggles straddling the
    /// transaction (the SQLite in-txn no-op rule), driven by
    /// [`SqliteBackend::rebuild_one`](crate::SqliteBackend::rebuild_one). The
    /// destructive/approval gate keys on the paired migration's flags
    /// (`destructive + requires_approval`). Always empty on the PG path.
    pub rebuilds: Vec<SqliteRebuild>,
}

/// **P3b** — one SQLite 12-step table rebuild: the execution [`SqliteRebuildSpec`]
/// plus the [`Migration`] that carries its checksum / journal identity / approval
/// flags. The differ produces these for the existing-table ops SQLite cannot ALTER
/// natively.
///
/// NOTE (P6a): the engine now DRIVES these rebuilds.
/// [`plan_declarative`](crate::engine::MigrationEngine::plan_declarative) CARRIES the
/// rebuilds into the [`DeclarativeDeployPlan`](crate::engine::DeclarativeDeployPlan),
/// and the now-generic
/// [`apply_declarative`](crate::engine::MigrationEngine::apply_declarative) drives
/// each through
/// [`MigrationBackend::rebuild_one`](crate::apply::backend::MigrationBackend::rebuild_one)
/// under the destructive/approval gate (the journal migration is `destructive +
/// requires_approval`, so an un-approved rebuild is refused before any DDL). The old
/// `plan_declarative` fail-close (`SqliteRebuildRequired`) is gone. The direct,
/// executor-internal [`SqliteBackend::rebuild_one`](crate::SqliteBackend::rebuild_one)
/// seam remains for tests; the engine path is the gated production drive.
#[derive(Debug, Clone)]
pub struct SqliteRebuild {
    /// The journal migration: its `version` is the rebuild's identity, its
    /// `checksum` certifies the rebuild, and its flags (`destructive = true,
    /// requires_approval = true`) route it through the gate. Its `up` carries the
    /// new-table CREATE for inspection/checksum; the actual apply is structured (the
    /// `spec`), NOT a plain `up` execution.
    pub migration: Migration,
    /// The fully-resolved 12-step rebuild specification the backend executes.
    pub spec: SqliteRebuildSpec,
}

impl DeclarativePlan {
    /// True if the plan reconciles nothing — no plain migrations, no renames, AND
    /// no SQLite rebuilds.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.migrations.is_empty() && self.renames.is_empty() && self.rebuilds.is_empty()
    }

    /// All migrations the plan would ultimately apply, flattened (plain set +
    /// every rename's expand-then-contract migrations) — for **inspection /
    /// preview only** (lint, counting, SQL-shape assertions). This is NOT an
    /// apply order: a rename's expand and contract belong to DIFFERENT deploys,
    /// and the backfill between them is not a `Migration`. Use
    /// [`apply_declarative`](crate::engine::MigrationEngine::apply_declarative)
    /// to apply.
    #[must_use]
    pub fn all_migrations(&self) -> Vec<Migration> {
        let mut all = self.migrations.clone();
        for r in &self.renames {
            all.extend(r.all());
        }
        // P3b — a SQLite rebuild's journal migration carries its checksum / identity
        // for inspection + the gate. Its apply is structured (the spec), not a plain
        // `up`, but for preview/counting/checksum purposes it is one migration.
        for rb in &self.rebuilds {
            all.push(rb.migration.clone());
        }
        all
    }

    /// The operational [`Advisory`](crate::analyze::Advisory)s for every
    /// generated migration in the plan, paired with the migration they apply to.
    ///
    /// This is the differ's advisory seam (v3 Plan B): it runs
    /// [`analyze_migration`](crate::analyze::analyze_migration) over each
    /// generated migration (the plain set + every rename's expand/contract
    /// migrations) so a plan/preview UI can show the operational footgun and the
    /// safer alternative next to the migration that triggers it — e.g. a gated
    /// `DROP COLUMN` (contract) surfaces the expand-contract suggestion, a
    /// generated `SET NOT NULL` surfaces the `NOT VALID` → `VALIDATE` path.
    ///
    /// These are **advisory only** — they never deny or gate the plan. A
    /// migration with no advisories is omitted. Order matches
    /// [`all_migrations`](Self::all_migrations).
    /// Plan-aware (review finding #8): a `FK_WITHOUT_INDEX` Notice is suppressed
    /// when the **same plan** creates a covering index for the FK's referencing
    /// column(s) — even in a SEPARATE migration. The per-statement
    /// [`analyze`](crate::analyze::analyze) only sees one statement, so it
    /// suppresses only same-statement indexes; here we aggregate every migration's
    /// covering-index columns ([`indexed_columns`](crate::analyze::indexed_columns))
    /// and drop the FK Notice for any column the plan indexes. All other advisories
    /// pass through unchanged.
    #[must_use]
    pub fn advisories(&self) -> Vec<(Migration, Vec<crate::analysis::analyze::Advisory>)> {
        let all = self.all_migrations();

        // Plan-wide set of columns that gain a covering index ANYWHERE in the plan
        // (any migration). Case-insensitive membership mirrors the per-statement
        // FK-index match.
        let mut plan_indexed: Vec<String> = Vec::new();
        for m in &all {
            plan_indexed.extend(crate::analysis::analyze::indexed_columns(&m.up));
        }

        all.into_iter()
            .filter_map(|m| {
                let mut advs = crate::analysis::analyze::analyze_migration(&m);

                // If a migration carries a FK_WITHOUT_INDEX Notice, recompute it
                // against the plan-wide index set: suppress it only when EVERY FK
                // referencing column it covers is indexed somewhere in the plan.
                let fk_cols = crate::analysis::analyze::fk_columns_needing_index(&m.up);
                if !fk_cols.is_empty() {
                    let all_covered = fk_cols.iter().all(|col| {
                        plan_indexed.iter().any(|i| i.eq_ignore_ascii_case(col))
                    });
                    if all_covered {
                        advs.retain(|a| a.rule != crate::analysis::analyze::rule::FK_WITHOUT_INDEX);
                    }
                }

                (!advs.is_empty()).then_some((m, advs))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// P1/P2 — the declarative differ.
// ---------------------------------------------------------------------------

/// The declarative differ — turns a desired/live snapshot pair into the
/// migrations that reconcile them (P1 additive + P2 destructive-gated).
///
/// A [`MigrationAuthor`](crate::plan::author::MigrationAuthor)-family author: it
/// reuses [`crate::plan::author::DeterministicAuthor`] rendering where possible and emits
/// [`Migration`]s with correct [`MigrationFlags`]. It validates every descriptor
/// name/type at its boundary and relies on the guard as the second line.
#[derive(Debug, Clone)]
pub struct DeclarativeAuthor {
    /// The project schema every emitted statement is qualified into.
    project_schema: String,
    /// The **deploying** app (`app_…`) — the app whose deploy is driving this
    /// diff. It is stamped on every emitted [`Migration`] (`owner_app`) AND it is
    /// the ownership-enforcement subject (P4, design §4): [`Self::diff`] refuses a
    /// structural change to any union table whose owner ≠ this app.
    owner_app: String,
    /// The target SQL dialect the emitted `up`/`down` are spelled in (PHASE 4).
    ///
    /// - `Postgres` (the default via [`Self::new`]) — the historical PG-only
    ///   emitter: `self.render_create_table` etc. produce schema-qualified PG DDL.
    ///   BYTE-IDENTICAL to before this field existed.
    /// - `Sqlite` (via [`Self::new_for_dialect`]) — the Confined SQLite path
    ///   (design §2.5.3): the new-table CREATE is ROUTED THROUGH the shared
    ///   `zeroship_schema::query` emitter with [`SqliteEmitScope::MainUnqualified`],
    ///   producing UNqualified DDL that lands in `main` (= the app file) under the
    ///   `SqliteBackend`'s hardened authorizer. No second SQLite emitter is written
    ///   here — the engine routes to the single shared one.
    dialect: SqlDialect,
}

impl DeclarativeAuthor {
    /// Construct a declarative author bound to a project schema + the **deploying**
    /// app. In the multi-app model the deploying app is the ownership-enforcement
    /// subject: [`Self::diff`] refuses a structural change to a table owned by a
    /// different app (design §4).
    ///
    /// Defaults to the **Postgres** dialect — byte-identical to before PHASE 4.
    /// Use [`Self::new_for_dialect`] for the Confined SQLite path.
    #[must_use]
    pub fn new(project_schema: impl Into<String>, owner_app: impl Into<String>) -> Self {
        Self::new_for_dialect(project_schema, owner_app, SqlDialect::Postgres)
    }

    /// Construct a declarative author for an explicit target `dialect` (PHASE 4).
    ///
    /// `SqlDialect::Sqlite` selects the Confined SQLite path: the new-table CREATE
    /// `up` is routed through the shared `zeroship_schema::query` emitter
    /// ([`SqliteEmitScope::MainUnqualified`]) so the DDL is unqualified and lands
    /// in the app file's `main` namespace. The PG dialect is the original path.
    #[must_use]
    pub fn new_for_dialect(
        project_schema: impl Into<String>,
        owner_app: impl Into<String>,
        dialect: SqlDialect,
    ) -> Self {
        Self {
            project_schema: project_schema.into(),
            owner_app: owner_app.into(),
            dialect,
        }
    }

    /// The target SQL dialect this author emits.
    #[must_use]
    pub fn dialect(&self) -> SqlDialect {
        self.dialect
    }

    /// **PR10** — a clone of this author bound to a different `project_schema`,
    /// for rendering ONE op under its resolved schema qualifier (§2.7). The
    /// emitter (`PgEmitter`) and `qualified()` read `project_schema`, so swapping
    /// it here re-qualifies every statement the returned author renders.
    /// `owner_app` and `dialect` are preserved. For the common no-override case the
    /// resolved schema EQUALS the current `project_schema`, so the clone renders
    /// byte-identically — keeping the Confined / no-schema path unchanged.
    #[must_use]
    pub(crate) fn with_project_schema(&self, schema: impl Into<String>) -> Self {
        Self {
            project_schema: schema.into(),
            owner_app: self.owner_app.clone(),
            dialect: self.dialect,
        }
    }

    /// The deploying app (`app_…`) this author stamps on emitted migrations.
    /// Used by [`crate::render::lower::IrAuthor`] to stamp the descriptor owner.
    #[must_use]
    pub(crate) fn owner_app(&self) -> &str {
        &self.owner_app
    }

    /// Select the per-dialect DDL emission seam (P1). The dialect choice is made
    /// ONCE here; the render methods are thin callers of the returned emitter. A
    /// closed `SqlDialect` enum ⇒ an exhaustive match (a new engine would not
    /// compile until it has an emitter).
    fn emitter(&self) -> Box<dyn DdlEmitter> {
        match self.dialect {
            SqlDialect::Postgres => Box::new(PgEmitter {
                project_schema: self.project_schema.clone(),
            }),
            SqlDialect::Sqlite => Box::new(SqliteEmitter),
            SqlDialect::Mysql => Box::new(MysqlEmitter {
                project_schema: self.project_schema.clone(),
            }),
        }
    }

    /// Render `<schema>.<object>`, both parts quoted.
    fn qualified(&self, object: &str) -> String {
        format!("{}.{}", quote_ident(&self.project_schema), quote_ident(object))
    }

    /// Build a [`Migration`] from rendered `up`/`down` SQL + flags + deps.
    fn make(
        &self,
        name: &str,
        up: String,
        down: Option<String>,
        flags: MigrationFlags,
        depends_on: Vec<MigrationId>,
    ) -> Migration {
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: &up,
            down: down.as_deref(),
            flags: &flags,
            owner_app: &self.owner_app,
            depends_on: &depends_on,
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version: MigrationId::generate(),
            name: name.to_string(),
            up,
            down,
            checksum,
            flags,
            owner_app: self.owner_app.clone(),
            depends_on,
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        }
    }

    /// Diff the **desired** snapshot against the **live** snapshot and generate
    /// the migrations that reconcile them.
    ///
    /// P1 (additive) handles:
    /// - **CREATE TABLE** — a table in desired, absent in live (with its
    ///   columns, PK, unique indexes, and own-table FKs inlined; FKs to a
    ///   not-yet-created table are deferred to a follow-on `ALTER TABLE ADD
    ///   CONSTRAINT`, mirroring plugin-db's deferred-FK pattern);
    /// - **ADD COLUMN** — a column in desired, absent in a live table;
    /// - **CREATE INDEX** — an index in desired, absent in a live table.
    ///
    /// P2 (destructive, gated) handles a live-only object (absent in desired):
    /// - **DROP TABLE / DROP COLUMN** — DATA LOSS: the classifier/guard marks
    ///   these destructive, so the existing engine gate refuses them without
    ///   [`Approval::Approved`](crate::Approval). NEVER auto-applied.
    /// - **DROP INDEX** — a PLAIN index DROP is NOT data loss (reversible by
    ///   recreating the index), so it flows through ungated, the same as an
    ///   additive op. A **UNIQUE** index DROP, however, silently removes a
    ///   data-integrity guarantee (#4), so it is classified `destructive +
    ///   requires_approval` (gated, like DROP COLUMN) — see `render_drop_index`.
    ///
    /// P3 (rename, opt-in) routes a **hinted** drop+add pair through the
    /// zero-downtime expand-contract sequence
    /// ([`ExpandContractAuthor::RenameColumn`](crate::render::expand_contract)) instead of
    /// emitting an independent drop + add. A rename is emitted ONLY when a
    /// [`RenameHint`] explicitly names the `(table, from→to)` pair AND `from` is a
    /// live-only column AND `to` is a desired-only column AND their types match.
    /// Without a matching hint, a drop+add stays two independent ops — the differ
    /// NEVER infers a rename heuristically (that risks silent data loss).
    ///
    /// P3 (type / nullability) handles a same-name column whose attributes
    /// changed (these were `UnsupportedInV1` before P3):
    /// - **type change** → a GATED `ALTER COLUMN … TYPE …` (`destructive` +
    ///   `requires_approval`; no auto type-change in v1);
    /// - **`DROP NOT NULL`** (required true→false) → an ungated additive
    ///   `ALTER COLUMN DROP NOT NULL` (relaxing a constraint is safe);
    /// - **`SET NOT NULL`** (required false→true) → a GATED `ALTER COLUMN SET NOT
    ///   NULL` (lock-heavy + can fail on existing NULLs).
    ///
    /// Ordering: CREATE TABLE precede their own indexes; FK-target tables are
    /// created before referencing tables (deferred FK breaks cycles); the
    /// per-version `UUIDv7` gives a stable total order, and `depends_on` records
    /// cross-table deps for the executor's topo sort.
    ///
    /// # Caller contract (READ THIS — a partial union is dangerous)
    ///
    /// `desired` MUST be the **COMPLETE project union** — every member app's
    /// descriptors, not just the deploying app's. A live table absent from the
    /// union is read as "no app declares it" and becomes a `DROP TABLE` candidate.
    ///
    /// `live_ownership` MUST carry an entry (`live table name → owning app`) for
    /// **every live table**, supplied by the caller from the journal / route
    /// registry. It is the differ's fail-closed guard for the drop pass (2b): a
    /// `DROP TABLE` is authored ONLY when `live_ownership` confirms the deploying
    /// app owns that table. A live table being dropped whose owner is
    /// *another* app ⇒ [`DeclarativeError::NotTableOwner`]; a live table being
    /// dropped whose owner is *unknown* (no `live_ownership` entry) ⇒
    /// [`DeclarativeError::DropOfUnownedTable`]. So a PARTIAL-union deploy fails
    /// closed (refused) instead of mass-dropping the omitted tenants' tables.
    ///
    /// # Errors
    /// - [`DeclarativeError::Invalid`] — a descriptor name/type failed the
    ///   author-boundary validation (nothing generated).
    /// - [`DeclarativeError::NotTableOwner`] — a structural change to a union
    ///   table whose owner ≠ the deploying app, OR a `DROP TABLE` of a live table
    ///   owned by another app (P4 ownership enforcement).
    /// - [`DeclarativeError::DropOfUnownedTable`] — a `DROP TABLE` of a live table
    ///   whose ownership the caller did not supply in `live_ownership` (fail-closed
    ///   — defends against a partial-union deploy, 2b).
    /// - [`DeclarativeError::CrossAppFkTargetMissing`] — an FK whose target table
    ///   is declared by no member app and is not live (P4 cross-app FK).
    /// - [`DeclarativeError::RenameHintUnmatched`] — a hint named a pair that is
    ///   not an actual drop+add.
    /// - [`DeclarativeError::RenameHintTypeMismatch`] — a hint matched a pair
    ///   whose types differ.
    /// - [`DeclarativeError::UnsupportedInV1`] — an index/FK in-place
    ///   redefinition (still deferred).
    #[allow(
        clippy::too_many_lines,
        reason = "the diff is one cohesive pass — new tables (FK-ordered), \
                  deferred FKs, then per-table column/index add + gated drops — \
                  that reads more clearly as a single function than split across \
                  helpers that would each need the shared created_version map"
    )]
    pub fn diff(
        &self,
        desired: &DesiredSchema,
        live: &SchemaSnapshot,
        live_ownership: &HashMap<String, String>,
        hints: &[RenameHint],
    ) -> Result<DeclarativePlan, DeclarativeError> {
        // The ownership map travels alongside the union; the diff itself operates
        // on the union SNAPSHOT, so bind it locally and keep the rest of the pass
        // unchanged. Ownership is consulted (a) for cross-app FK target validation
        // and (b) for the post-pass ownership-enforcement check (P4).
        let ownership = &desired.ownership;
        // PHASE 4 — keep the full `DesiredSchema` reachable for the SQLite leg (it
        // needs `desired.sqlite_schemas` to route a new-table CREATE through the
        // shared emitter); the rest of the pass operates on the snapshot as before.
        let desired_full = desired;
        let desired = &desired.snapshot;

        // Author-boundary validation: every desired table/column/index name and
        // every column data_type must be safe BEFORE we render any SQL.
        Self::validate_desired(desired)?;

        // P4 cross-app FK: every FK target must exist in the UNION (it may be a
        // table owned by another app, but it must be declared by SOME member app)
        // or already live. A dangling target is a clear error, not bad SQL at
        // apply. Checked before any SQL is rendered.
        Self::validate_cross_app_fk_targets(desired, live)?;

        // Resolve + validate the rename hints up-front: every hint MUST match an
        // actual drop+add pair (from live-only, to desired-only, types equal) on
        // its table. An un-matchable / type-mismatched hint is a hard error (the
        // hint is the creator's signed intent — never silently ignored). Returns
        // the per-table set of (from,to,type) renames the column diff will route
        // through expand-contract instead of emitting drop+add.
        let resolved = Self::resolve_rename_hints(desired, live, hints)?;

        let mut out: Vec<Migration> = Vec::new();
        // The online renames, carried as their full ExpandContractPlan (expand
        // migs + BackfillSpec + contract migs) — NOT flattened into `out` (C1).
        // Flattening would discard the BackfillSpec, so the pre-existing-row
        // mirror never runs and the contract DROP COLUMN <from> destroys data.
        let mut renames: Vec<ExpandContractPlan> = Vec::new();
        // P3b — the SQLite existing-table changes that have no native ALTER (type /
        // nullability change, column rename rebuild, ADD/DROP CONSTRAINT, FK
        // redefinition). Each is a structured 12-step rebuild, NOT a plain `up` — so
        // it is carried separately, like `renames`, never flattened into `out`.
        let mut rebuilds: Vec<SqliteRebuild> = Vec::new();

        // --- New tables (in desired, not in live), in FK-dependency order. ---
        let new_tables: Vec<&String> = desired
            .tables
            .keys()
            .filter(|t| !live.tables.contains_key(*t))
            .collect();
        let order = topo_order_new_tables(desired, &new_tables);

        // Map each newly-created table to its CREATE migration's version, so a
        // deferred FK (or an FK inlined into a table created earlier in this
        // batch) can `depends_on` the target's creation.
        let mut created_version: BTreeMap<String, MigrationId> = BTreeMap::new();
        // FKs that must be deferred (target not yet created when the table is
        // emitted) → emitted as ALTER TABLE ADD CONSTRAINT after all CREATEs.
        let mut deferred_fks: Vec<(String, ConstraintSnapshot)> = Vec::new();

        // SQLite has no `ALTER TABLE ADD CONSTRAINT` — FKs MUST be inline at CREATE
        // TABLE, so on SQLite a FK whose target is not yet available is a hard error
        // (handled per-table below), never a deferred ALTER.
        let is_sqlite = matches!(self.dialect, SqlDialect::Sqlite);

        for table in &order {
            let t = &desired.tables[*table];
            // Inline only the FKs whose target table already exists (live) or
            // was created earlier in this batch; defer the rest (PG only — SQLite
            // errors instead of deferring).
            let mut inline_fks: Vec<&ConstraintSnapshot> = Vec::new();
            let mut depends_on: Vec<MigrationId> = Vec::new();
            for c in &t.constraints {
                if c.kind != "FOREIGN KEY" {
                    continue;
                }
                let target = fk_target_table(&c.definition);
                match target {
                    Some(tt)
                        if live.tables.contains_key(&tt)
                            || created_version.contains_key(&tt) =>
                    {
                        if let Some(v) = created_version.get(&tt) {
                            depends_on.push(v.clone());
                        }
                        inline_fks.push(c);
                    }
                    other => {
                        if !self.dialect.supports(Capability::AlterTableAddConstraint) {
                            // SQLite cannot ADD CONSTRAINT later → fail closed.
                            return Err(DeclarativeError::SqliteDeferredFkUnsupported {
                                table: (*table).clone(),
                                target: other.unwrap_or_default(),
                            });
                        }
                        deferred_fks.push(((*table).clone(), c.clone()));
                    }
                }
            }

            // PHASE 4 — the Confined SQLite path ROUTES the new-table CREATE through
            // the shared `zeroship_schema::query` emitter (unqualified, `main` = the
            // app file); the PG path keeps its snapshot-rendered DDL. No second
            // SQLite emitter exists in this crate.
            let (up, down) = match self.dialect {
                SqlDialect::Sqlite => (
                    self.render_create_table_sqlite(table, desired_full)?,
                    // SQLite DROP TABLE is unqualified (main IS the app file).
                    format!("DROP TABLE {}", quote_ident(table)),
                ),
                SqlDialect::Mysql => (
                    self.render_create_table_mysql_snapshot_statements(table, t, &inline_fks)
                        .join(";\n"),
                    format!("DROP TABLE {}", mysql_qualified(&self.project_schema, table)),
                ),
                SqlDialect::Postgres => (
                    self.render_create_table(table, t, &inline_fks),
                    format!("DROP TABLE {}", self.qualified(table)),
                ),
            };
            let mig = self.make(
                &format!("create_table_{table}"),
                up,
                Some(down),
                MigrationFlags::default(),
                depends_on,
            );
            created_version.insert((*table).clone(), mig.version.clone());

            // Emit CREATE INDEX migrations for the new table's indexes, each
            // depending on the table's creation. The implicit PK index
            // (`<table>_pkey`) is created by the inline PRIMARY KEY clause, so
            // it is NOT emitted as a standalone CREATE INDEX.
            let table_version = mig.version.clone();
            out.push(mig);
            for idx in &t.indexes {
                if is_pk_index(table, &idx.name) {
                    continue;
                }
                // PHASE 4 — on SQLite the shared CREATE-TABLE emitter ALREADY emits
                // the three implicit system-field indexes (`<table>_<col>_idx` for
                // deleted_at/updated_at/created_by) inline in the table-create
                // payload. Skip re-emitting them here to avoid a redundant (though
                // idempotent) second CREATE INDEX; the remaining user/unique indexes
                // are emitted unqualified for the `main` app file.
                if is_sqlite && is_system_field_index(table, &idx.name) {
                    continue;
                }
                out.push(self.render_create_index(
                    table,
                    idx,
                    vec![table_version.clone()],
                ));
            }
        }

        // --- Deferred FKs (ALTER TABLE ADD CONSTRAINT), after all CREATEs. ---
        for (table, fk) in &deferred_fks {
            let dep = created_version.get(table).cloned().into_iter();
            let target = fk_target_table(&fk.definition);
            let target_dep = target
                .as_ref()
                .and_then(|t| created_version.get(t))
                .cloned()
                .into_iter();
            let depends_on: Vec<MigrationId> = dep.chain(target_dep).collect();
            out.push(self.render_add_fk(table, fk, depends_on));
        }

        // --- Existing tables: column / index additions + destructive drops. ---
        for (table, dt) in &desired.tables {
            let Some(lt) = live.tables.get(table) else {
                continue; // newly created above
            };

            let live_cols: BTreeMap<&str, &ColumnSnapshot> =
                lt.columns.iter().map(|c| (c.name.as_str(), c)).collect();
            let desired_cols: BTreeMap<&str, &ColumnSnapshot> =
                dt.columns.iter().map(|c| (c.name.as_str(), c)).collect();

            // P3 rename (opt-in): the resolved renames for THIS table. A hinted
            // `from`→`to` is routed through the expand-contract sequence below and
            // its `from`/`to` columns are EXCLUDED from the plain drop/add diff so
            // they are not double-handled (drop the renamed-away column / add the
            // renamed-to column).
            let table_renames: Vec<&ResolvedRename> =
                resolved.iter().filter(|r| &r.table == table).collect();
            let renamed_from: std::collections::BTreeSet<&str> =
                table_renames.iter().map(|r| r.from.as_str()).collect();
            let renamed_to: std::collections::BTreeSet<&str> =
                table_renames.iter().map(|r| r.to.as_str()).collect();

            // PHASE 3b — on the Confined SQLite path, the existing-table changes that
            // SQLite has NO native ALTER for — a column TYPE change, a nullability
            // change (either direction), a column RENAME, an ADD/DROP CONSTRAINT, or
            // an in-place FK redefinition — are reconciled by the 12-step table
            // REBUILD (§2.4). A rebuild reconciles the WHOLE table at once (every
            // changed column + the new constraint/FK set), so we detect it up front,
            // emit ONE structured `SqliteRebuild`, and `continue` past the PG-shaped
            // per-op emission below (which has no SQLite form for these). The
            // natively-expressible existing-table ops (ADD COLUMN, DROP COLUMN, DROP
            // INDEX, ADD INDEX) still flow through the per-op path when NO rebuild is
            // needed.
            if is_sqlite {
                if let Some(reason) =
                    self.sqlite_existing_table_needs_rebuild(table, lt, dt, &table_renames)
                {
                    let rb = self.build_sqlite_rebuild(
                        table,
                        desired_full,
                        lt,
                        dt,
                        &table_renames,
                        reason,
                    )?;
                    rebuilds.push(rb);
                    continue;
                }
                // No rebuild needed: a hinted rename with no other change is still a
                // rename, which SQLite expresses via `ALTER TABLE … RENAME COLUMN`
                // (native ≥ 3.25) — but the engine's rename path is the PG-shaped
                // expand-contract sequence (schema-qualified, dual-write). Routing a
                // pure SQLite rename through a rebuild keeps it single-sourced and
                // confinement-clean; `sqlite_existing_table_needs_rebuild` already
                // returns `Some` whenever there is a rename, so a rename can never
                // reach the PG expand-contract author below on the SQLite leg.
            }

            // Author the expand-contract rename sequences (E1..E3, C1, C2) and
            // carry them STRUCTURED — do NOT flatten into `out` (C1: that would
            // discard the BackfillSpec, so the real pre-existing-row mirror never
            // runs and the contract DROP destroys data). The caller drives each
            // expand through `run_expand` (which runs the real backfill) and
            // defers the contract to a subsequent deploy. The `from`/`to` columns
            // are excluded from the plain drop/add passes below so they are not
            // double-handled.
            let ec = ExpandContractAuthor::new(&self.project_schema, &self.owner_app);
            for r in &table_renames {
                let plan = ec.author(&OnlineIntent::RenameColumn {
                    table: table.clone(),
                    from: r.from.clone(),
                    to: r.to.clone(),
                    ty: ddl_type(&r.ty).to_string(),
                })?;
                renames.push(plan);
            }

            // ADD COLUMN: in desired, not in live (skip a rename's `to` column —
            // it is created by the rename's E1 ADD COLUMN, not a plain add).
            for c in &dt.columns {
                if renamed_to.contains(c.name.as_str()) {
                    continue;
                }
                match live_cols.get(c.name.as_str()) {
                    None => out.push(self.render_add_column(table, c)),
                    Some(lc) => {
                        // Same-name column whose attributes changed (P3):
                        // - type change → GATED ALTER COLUMN TYPE (no auto change);
                        // - SET NOT NULL (false→true) → GATED (lock-heavy, can
                        //   fail on existing NULLs);
                        // - DROP NOT NULL (true→false) → ungated additive.
                        //
                        // PHASE 3b — SQLite has NO `ALTER COLUMN` at all (its ALTER
                        // TABLE only does RENAME / ADD COLUMN / DROP COLUMN / RENAME
                        // COLUMN). A type change or ANY nullability change is now
                        // reconciled by the 12-step table REBUILD detected up front —
                        // `sqlite_existing_table_needs_rebuild` returns `Some` for
                        // exactly these, and the loop `continue`s past this whole
                        // existing-table body BEFORE reaching here. So on the SQLite
                        // leg a same-name column with a real type/nullability change is
                        // UNREACHABLE here; if one is somehow seen, it is a detector
                        // bug — fail closed with an internal error (NEVER emit dangling
                        // PG `ALTER COLUMN` DDL, NEVER silently skip). The dialect-aware
                        // type compare uses the SAME `sqlite_canonical_type` folding
                        // the detector uses, so the two agree.
                        if is_sqlite {
                            if sqlite_canonical_type(&lc.data_type)
                                != sqlite_canonical_type(&c.data_type)
                                || lc.nullable != c.nullable
                            {
                                return Err(DeclarativeError::Invalid(format!(
                                    "internal: SQLite column {table}.{} has a type/nullability \
                                     change that the rebuild detector should have caught (P3b \
                                     invariant violated)",
                                    c.name
                                )));
                            }
                            continue;
                        }
                        if lc.data_type != c.data_type {
                            out.push(self.render_alter_column_type(table, c));
                        }
                        if lc.nullable != c.nullable {
                            out.push(self.render_alter_column_nullability(
                                table,
                                &c.name,
                                c.nullable,
                            ));
                        }
                    }
                }
            }

            // DROP COLUMN (P2): in live, not in desired → destructive, gated
            // (skip a rename's `from` column — it is dropped by the rename's gated
            // contract C2, not a plain drop).
            for c in &lt.columns {
                if renamed_from.contains(c.name.as_str()) {
                    continue;
                }
                if !desired_cols.contains_key(c.name.as_str()) {
                    out.push(self.render_drop_column(table, &c.name));
                }
            }

            // CREATE INDEX / DROP INDEX on an existing table.
            let live_idx: BTreeMap<&str, &IndexSnapshot> =
                lt.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
            let desired_idx: BTreeMap<&str, &IndexSnapshot> =
                dt.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
            for idx in &dt.indexes {
                if is_pk_index(table, &idx.name) {
                    continue; // implicit; created by the PRIMARY KEY clause
                }
                match live_idx.get(idx.name.as_str()) {
                    None => out.push(self.render_create_index(table, idx, Vec::new())),
                    Some(li) => {
                        // Same-name index on both sides: a flipped `unique` flag or
                        // a changed column set is an in-place redefinition
                        // (DROP+CREATE), deferred to a later phase. Surface it
                        // EXPLICITLY (5-idx) — never silently skip (the old loop
                        // only checked name presence, so a uniqueness flip emitted
                        // 0 migrations and left the wrong index in place).
                        if li.unique != idx.unique {
                            return Err(DeclarativeError::UnsupportedInV1(format!(
                                "index {}.{} uniqueness change {} → {}",
                                table, idx.name, li.unique, idx.unique
                            )));
                        }
                        if li.columns != idx.columns {
                            return Err(DeclarativeError::UnsupportedInV1(format!(
                                "index {}.{} column change {:?} → {:?}",
                                table, idx.name, li.columns, idx.columns
                            )));
                        }
                    }
                }
            }
            for idx in &lt.indexes {
                if is_pk_index(table, &idx.name) {
                    continue; // never drop the PK's implicit index
                }
                if !desired_idx.contains_key(idx.name.as_str()) {
                    out.push(self.render_drop_index(Some(table), idx));
                }
            }

            // FK constraints on an existing table (5-fk): a same-name FK whose
            // BODY changed (e.g. the referenced target was re-pointed) is an
            // in-place constraint redefinition (DROP+ADD), deferred to a later
            // phase. Compare bodies and surface the divergence EXPLICITLY — the
            // old differ never looked at constraints here, so a changed FK target
            // was silently skipped (the FK definition spelling now matches live,
            // so this compare is meaningful, not phantom-drift noise).
            let live_fk: BTreeMap<&str, &ConstraintSnapshot> = lt
                .constraints
                .iter()
                .filter(|c| c.kind == "FOREIGN KEY")
                .map(|c| (c.name.as_str(), c))
                .collect();
            for c in &dt.constraints {
                if c.kind != "FOREIGN KEY" {
                    continue;
                }
                if let Some(lc) = live_fk.get(c.name.as_str()) {
                    if lc.definition != c.definition {
                        return Err(DeclarativeError::UnsupportedInV1(format!(
                            "foreign key {}.{} definition change {:?} → {:?}",
                            table, c.name, lc.definition, c.definition
                        )));
                    }
                }
            }
        }

        // --- DROP TABLE (P2): in live, not in desired → destructive, gated. ---
        // In the UNION model `desired` is the FULL project schema (every member
        // app's tables), so a live table that is absent from the union is one NO
        // app declares — a DROP TABLE candidate. (A table still owned by a member
        // app stays in the union and is never reached.)
        //
        // FAIL-CLOSED ownership check (2b): the differ must NOT trust the caller
        // to have passed the complete union. A partial-union deploy (only ONE
        // app's descriptors) would make every OTHER app's live table look absent
        // from desired → a destructive foreign DROP authored under the deploying
        // app's authority. So for EVERY drop candidate, confirm ownership against
        // the caller-supplied `live_ownership` BEFORE authoring the drop:
        //   - owner present AND == deploying_app → allowed (owner removed its own
        //     table); author the gated drop.
        //   - owner present AND != deploying_app → NotTableOwner (a non-owner may
        //     not drop a foreign table).
        //   - owner UNKNOWN (no entry) → DropOfUnownedTable (refuse: the differ
        //     will not author a destructive drop it cannot confirm).
        for table in live.tables.keys() {
            if desired.tables.contains_key(table) {
                continue;
            }
            match live_ownership.get(table) {
                Some(owner) if owner == &self.owner_app => {
                    out.push(self.render_drop_table(table));
                }
                Some(owner) => {
                    return Err(DeclarativeError::NotTableOwner {
                        table: table.clone(),
                        owner: owner.clone(),
                        deploying_app: self.owner_app.clone(),
                    });
                }
                None => {
                    return Err(DeclarativeError::DropOfUnownedTable {
                        table: table.clone(),
                    });
                }
            }
        }

        // P4 ownership enforcement (design §4): a structural change to a table
        // whose owner ≠ the deploying app is REFUSED. The diff is computed over
        // the FULL union, so a non-owner's deploy that merely USES a table emits
        // NO op for it (the table's union shape == live ⇒ no structural delta) and
        // is fine; only an actual structural CHANGE to a non-owned table is
        // refused. Driven from the structural delta (snapshot diff), not migration
        // names, so it covers CREATE/ALTER/DROP (incl. cross-app FK ALTER and the
        // rename expand/contract) uniformly and deterministically.
        Self::enforce_ownership(&self.owner_app, desired, live, ownership)?;

        // Total order by UUIDv7 version (stable; the executor topo-sorts on
        // depends_on within it). Only the PLAIN migrations are ordered here; each
        // rename keeps its own internal expand→contract ordering and is applied
        // through the dedicated multi-deploy path, not interleaved with the plain
        // set.
        out.sort_by(|a, b| a.version.cmp(&b.version));
        Ok(DeclarativePlan {
            migrations: out,
            renames,
            rebuilds,
        })
    }

    /// P4 ownership enforcement (design §4): refuse a structural change to any
    /// union table the deploying app (`deploying_app`) does not own.
    ///
    /// A table is **structurally changed** by this diff iff:
    /// - it is in the union but not live (CREATE TABLE), OR
    /// - it is in both but its union [`TableSnapshot`] ≠ its live one (ALTER —
    ///   add/drop column, type/nullability, index, FK, rename expand/contract).
    ///
    /// For each such union table, if `ownership[table] != deploying_app` ⇒
    /// [`DeclarativeError::NotTableOwner`]. A table whose union shape EQUALS live
    /// has no structural delta — a non-owner merely USING it produces no op and is
    /// never refused (the "identical re-declaration by a non-owner is a no-op"
    /// rule falls straight out of snapshot equality).
    ///
    /// A live-only table absent from the union (only a DROP TABLE reaches it) has
    /// no UNION owner, so this pass does not cover it — its destructive drop is
    /// instead gated by the dedicated fail-closed drop-ownership check in
    /// [`Self::diff`], which consults the caller-supplied `live_ownership` map
    /// (a drop is authored only when the deploying app is the confirmed owner; an
    /// unknown owner fails closed — 2b).
    fn enforce_ownership(
        deploying_app: &str,
        desired: &SchemaSnapshot,
        live: &SchemaSnapshot,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DeclarativeError> {
        for (table, dt) in &desired.tables {
            // `None` ⇒ CREATE TABLE; `Some(lt)` ⇒ any ALTER iff the union shape
            // differs from live (columns/indexes/fks/rename).
            let changed = live.tables.get(table).is_none_or(|lt| lt != dt);
            if !changed {
                continue;
            }
            // `ownership` keys are exactly `desired.tables` keys, so this is always
            // present for a union table.
            if let Some(owner) = ownership.get(table) {
                if owner != deploying_app {
                    return Err(DeclarativeError::NotTableOwner {
                        table: table.clone(),
                        owner: owner.clone(),
                        deploying_app: deploying_app.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate every FK target across the UNION (P4 cross-app FK, design §4): the
    /// target table must be declared by SOME member app (present in `desired`, the
    /// union) OR already exist live. A target no app declares is a clear
    /// [`DeclarativeError::CrossAppFkTargetMissing`] — surfaced before any SQL is
    /// rendered, never left to fail as bad SQL at apply.
    ///
    /// Note (3c, out of differ scope): whether the OWNER of a cross-app FK target
    /// has CONSENTED to another app pointing an inbound FK at its table is a
    /// control-plane policy concern, not the differ's. The differ only confirms
    /// the target EXISTS in the union; inbound-FK consent (and its revocation) is
    /// the control plane's job to enforce, the same layer that assembles the union
    /// and the `live_ownership` map.
    fn validate_cross_app_fk_targets(
        desired: &SchemaSnapshot,
        live: &SchemaSnapshot,
    ) -> Result<(), DeclarativeError> {
        for (table, t) in &desired.tables {
            for c in &t.constraints {
                if c.kind != "FOREIGN KEY" {
                    continue;
                }
                if let Some(target) = fk_target_table(&c.definition) {
                    if !desired.tables.contains_key(&target)
                        && !live.tables.contains_key(&target)
                    {
                        return Err(DeclarativeError::CrossAppFkTargetMissing {
                            table: table.clone(),
                            target,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Validate every desired table/column/index name + column `data_type` at the
    /// author boundary (mirrors `expand_contract`'s `validate_ident`/`validate_type`).
    fn validate_desired(desired: &SchemaSnapshot) -> Result<(), DeclarativeError> {
        for (table, t) in &desired.tables {
            validate_ident("table", table)?;
            for c in &t.columns {
                validate_ident("column", &c.name)?;
                validate_type(&c.data_type)?;
            }
            for i in &t.indexes {
                validate_ident("index", &i.name)?;
            }
            for c in &t.constraints {
                validate_ident("constraint", &c.name)?;
            }
        }
        Ok(())
    }

    /// Resolve + validate the [`RenameHint`]s against the desired/live snapshots.
    ///
    /// Each hint MUST match an actual drop+add pair: `from` present in the live
    /// table and ABSENT in desired (a column being dropped), `to` present in
    /// desired and ABSENT in live (a column being added), on the named table —
    /// and the two columns' `data_type`s MUST be equal. Any hint that fails is a
    /// hard error ([`DeclarativeError::RenameHintUnmatched`] /
    /// [`DeclarativeError::RenameHintTypeMismatch`]). The hint is the creator's
    /// signed statement of intent; silently dropping a hint would fall back to an
    /// unintended drop+add and lose the column's data.
    ///
    /// This is the ONLY place a rename is recognised — there is NO heuristic
    /// drop+add⇒rename inference anywhere in the differ.
    fn resolve_rename_hints(
        desired: &SchemaSnapshot,
        live: &SchemaSnapshot,
        hints: &[RenameHint],
    ) -> Result<Vec<ResolvedRename>, DeclarativeError> {
        // --- Cross-hint validation (H1/H2). ---------------------------------
        //
        // The per-hint resolution below validates each hint INDEPENDENTLY
        // (`from` live-only, `to` desired-only, type identity). That misses
        // collisions ACROSS hints on the same table, which produce colliding /
        // duplicated expand-contract sequences (a doubled `ADD COLUMN <to>`,
        // divergent dual-write triggers, a double `DROP COLUMN <from>`) or a
        // rename chain the single-snapshot resolution cannot express. Reject
        // those EXPLICITLY here, before any sequence is authored.
        //
        // Scoped PER TABLE: `from`/`to` are column names, unique only within a
        // table, so a `from` on table A and a `to` on table B sharing a spelling
        // is not a collision.
        {
            let mut froms: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
            let mut tos: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
            for h in hints {
                // H1: the multiset of `from`s per table must be duplicate-free.
                if !froms.entry(h.table.as_str()).or_default().insert(h.from.as_str()) {
                    return Err(DeclarativeError::DuplicateRenameHint {
                        table: h.table.clone(),
                        column: h.from.clone(),
                        side: "from",
                    });
                }
                // H1: …and so must the multiset of `to`s.
                if !tos.entry(h.table.as_str()).or_default().insert(h.to.as_str()) {
                    return Err(DeclarativeError::DuplicateRenameHint {
                        table: h.table.clone(),
                        column: h.to.clone(),
                        side: "to",
                    });
                }
            }
            // H2: no chain — a `to` on a table must not equal any OTHER hint's
            // `from` on the same table (e.g. `[a→b, b→c]`: `b` is both a target
            // and a source). A `from == to` hint trivially "matches" its own
            // `from`; that is a no-op handled by M1 below, not a chain, so skip it
            // here.
            for h in hints {
                if h.from == h.to {
                    continue;
                }
                if let Some(table_froms) = froms.get(h.table.as_str()) {
                    if table_froms.contains(h.to.as_str()) {
                        return Err(DeclarativeError::RenameHintChained {
                            table: h.table.clone(),
                            column: h.to.clone(),
                        });
                    }
                }
            }
        }

        let mut resolved = Vec::with_capacity(hints.len());
        for h in hints {
            // M1: a `from == to` hint is a no-op rename. Reject it with a PRECISE
            // error rather than the misleading `RenameHintUnmatched` it would
            // otherwise produce (an identical name is neither live-only nor
            // desired-only).
            if h.from == h.to {
                return Err(DeclarativeError::RenameHintNoop {
                    table: h.table.clone(),
                    column: h.from.clone(),
                });
            }
            // The named table must exist on BOTH sides (a rename is in-place on an
            // existing table). If it is missing on either side the hint cannot be
            // a drop+add pair → unmatched.
            let (Some(lt), Some(dt)) =
                (live.tables.get(&h.table), desired.tables.get(&h.table))
            else {
                return Err(DeclarativeError::RenameHintUnmatched {
                    table: h.table.clone(),
                    from: h.from.clone(),
                    to: h.to.clone(),
                });
            };
            let live_from = lt.columns.iter().find(|c| c.name == h.from);
            let desired_from = dt.columns.iter().any(|c| c.name == h.from);
            let desired_to = dt.columns.iter().find(|c| c.name == h.to);
            let live_to = lt.columns.iter().any(|c| c.name == h.to);

            // `from` must be live-only (present in live, absent in desired); `to`
            // must be desired-only (present in desired, absent in live). Anything
            // else is not a drop+add pair.
            let (Some(lf), Some(dtc)) = (live_from, desired_to) else {
                return Err(DeclarativeError::RenameHintUnmatched {
                    table: h.table.clone(),
                    from: h.from.clone(),
                    to: h.to.clone(),
                });
            };
            if desired_from || live_to {
                return Err(DeclarativeError::RenameHintUnmatched {
                    table: h.table.clone(),
                    from: h.from.clone(),
                    to: h.to.clone(),
                });
            }
            // Types must be identical — a pure online rename mirrors values across
            // the two columns and cannot also change the type.
            if lf.data_type != dtc.data_type {
                return Err(DeclarativeError::RenameHintTypeMismatch {
                    table: h.table.clone(),
                    from: h.from.clone(),
                    to: h.to.clone(),
                    from_type: lf.data_type.clone(),
                    to_type: dtc.data_type.clone(),
                });
            }
            resolved.push(ResolvedRename {
                table: h.table.clone(),
                from: h.from.clone(),
                to: h.to.clone(),
                ty: lf.data_type.clone(),
            });
        }
        Ok(resolved)
    }

    /// PHASE 4 — render the SQLite `CREATE TABLE` `up` for a NEW table by ROUTING
    /// THROUGH the shared `zeroship_schema::query` emitter with
    /// [`SqliteEmitScope::MainUnqualified`]. No second SQLite DDL emitter lives in
    /// this crate: the engine reconstructs the SDK schema `Value`
    /// ([`descriptor_to_sdk_schema`], stashed on [`DesiredSchema::sqlite_schemas`])
    /// and hands it to the same builder plugin-db's runtime uses — so the
    /// `generate`d SQLite table (system fields, mask `_masked` siblings + inline
    /// `__zsmask:` sentinels, encrypted BLOB columns + inline `zsenc:` sentinels,
    /// inline FK clauses, the three system-field indexes) is byte-for-byte the same
    /// shape, and lands UNqualified in `main` (= the app file).
    ///
    /// FK emission uses `FkEmission::Inline`: [`Self::diff`] has ALREADY verified
    /// that every FK on this table targets a table that is live or was created
    /// earlier in this batch (else it returned
    /// [`DeclarativeError::SqliteDeferredFkUnsupported`] — SQLite cannot ADD
    /// CONSTRAINT later). The engine's topo order guarantees an in-batch FK target's
    /// CREATE precedes this one, so inlining every FK is sound and matches SQLite's
    /// "FKs must be declared at CREATE TABLE" constraint.
    ///
    /// # Errors
    /// [`DeclarativeError::Invalid`] if the shared emitter rejects the reconstructed
    /// schema (a validation failure that slipped past the author boundary), or if the
    /// per-table SDK schema is absent (an engine invariant violation — every table in
    /// `desired.snapshot` has a `sqlite_schemas` entry by construction).
    fn render_create_table_sqlite(
        &self,
        table: &str,
        desired: &DesiredSchema,
    ) -> Result<String, DeclarativeError> {
        let schema = desired.sqlite_schemas.get(table).ok_or_else(|| {
            DeclarativeError::Invalid(format!(
                "internal: no SQLite SDK schema for table '{table}' (engine invariant: \
                 desired_snapshot must populate sqlite_schemas for every union table)"
            ))
        })?;
        if let Some(snapshot) = desired.snapshot.tables.get(table) {
            if has_generated_or_identity(snapshot) || has_inline_checks(snapshot) {
                return Ok(self
                    .render_create_table_sqlite_snapshot_statements(table, snapshot)
                    .join(";\n"));
            }
        }
        self.render_create_table_sqlite_value(table, schema)
    }

    /// The SQLite `CREATE TABLE` emission, parameterized by the SDK schema `Value`
    /// directly (§6.4/§6.5). The differ's [`render_create_table_sqlite`] pulls the
    /// `Value` from the precomputed `desired.sqlite_schemas` side-map; `IrAuthor`'s
    /// `lower_create_table` builds the SAME `Value` from the op's descriptor via
    /// [`descriptor_to_sdk_schema`] (the same call `desired_snapshot_for_dialect`
    /// makes) and routes here — so BOTH paths render through the identical shared
    /// `zeroship_schema::query` emitter and the §6.4 byte-identity holds on SQLite.
    pub(crate) fn render_create_table_sqlite_value(
        &self,
        table: &str,
        schema: &serde_json::Value,
    ) -> Result<String, DeclarativeError> {
        Ok(self.render_create_table_sqlite_value_statements(table, schema)?.join(";\n"))
    }

    /// **Structural** form of [`render_create_table_sqlite_value`]: the SQLite
    /// CREATE payload as its per-statement list (the CREATE plus the implicit
    /// system-field `CREATE INDEX`es). `join(";\n")` is byte-identical to the
    /// joined form. The IR lower path consumes this list so a string-literal column
    /// DEFAULT carrying an interior `;\n` is never re-split mid-statement.
    pub(crate) fn render_create_table_sqlite_value_statements(
        &self,
        table: &str,
        schema: &serde_json::Value,
    ) -> Result<Vec<String>, DeclarativeError> {
        // `app_id` here is the project schema; on the `MainUnqualified` SQLite arm it
        // is NOT emitted (the qualifier is dropped), but it is still validated by the
        // shared emitter, so pass the real project schema.
        zeroship_schema::query::build_create_table_with_fks_for_dialect_scoped_statements(
            &self.project_schema,
            table,
            schema,
            &zeroship_schema::query::FkEmission::Inline,
            SqlDialect::Sqlite,
            SqliteEmitScope::MainUnqualified,
        )
        .map_err(|e| DeclarativeError::Invalid(format!("sqlite emit for '{table}': {e}")))
    }

    /// **P3b** — does this existing SQLite table need the 12-step table REBUILD to
    /// reconcile `live` → `desired`? Returns `Some(reason)` for a change SQLite has
    /// NO native `ALTER` for, `None` if every difference is natively expressible
    /// (ADD COLUMN / DROP COLUMN / ADD INDEX / DROP INDEX).
    ///
    /// The rebuild triggers (design §2.4): a same-name column TYPE change, a
    /// nullability change (either direction), a hinted column RENAME, a same-name
    /// index in-place redefinition (uniqueness or column-set change), a same-name FK
    /// redefinition, and an ADD/DROP of an FK constraint (SQLite has no
    /// `ALTER TABLE ADD/DROP CONSTRAINT`, so any FK-set change is a rebuild).
    ///
    /// **Fail-closed:** the FIRST trigger found returns immediately with a precise
    /// reason; the per-op emission below never runs for a rebuild-needing table.
    fn sqlite_existing_table_needs_rebuild(
        &self,
        table: &str,
        lt: &TableSnapshot,
        dt: &TableSnapshot,
        table_renames: &[&ResolvedRename],
    ) -> Option<String> {
        // (1) A hinted column RENAME — SQLite has `RENAME COLUMN`, but the engine's
        //     rename path is the PG-shaped expand-contract sequence; on SQLite we
        //     reconcile a rename via the rebuild (`to ← from` copy mapping), keeping
        //     it single-sourced + confinement-clean.
        if let Some(r) = table_renames.first() {
            return Some(format!("rename column {} → {}", r.from, r.to));
        }

        let live_cols: BTreeMap<&str, &ColumnSnapshot> =
            lt.columns.iter().map(|c| (c.name.as_str(), c)).collect();

        // (2)/(3) A same-name column with a TYPE or NULLABILITY change. The
        //     dialect-aware `sqlite_canonical_type` fold avoids false positives on
        //     PG-vs-SQLite spelling differences (bytea↔blob, double precision↔real,
        //     timestamptz↔text); a GENUINE change maps to two distinct tokens.
        for c in &dt.columns {
            if let Some(lc) = live_cols.get(c.name.as_str()) {
                if sqlite_canonical_type(&lc.data_type) != sqlite_canonical_type(&c.data_type) {
                    return Some(format!(
                        "alter column {} type {} → {}",
                        c.name, lc.data_type, c.data_type
                    ));
                }
                if lc.nullable != c.nullable {
                    return Some(format!(
                        "alter column {} nullability {} → {}",
                        c.name, lc.nullable, c.nullable
                    ));
                }
            }
        }

        // (4) A same-name INDEX whose uniqueness or column set changed — an in-place
        //     index redefinition (SQLite has no `ALTER INDEX`; a DROP+CREATE inside a
        //     rebuild is how the new shape's index set lands).
        let live_idx: BTreeMap<&str, &IndexSnapshot> =
            lt.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
        for idx in &dt.indexes {
            if is_pk_index(table, &idx.name) {
                continue;
            }
            if let Some(li) = live_idx.get(idx.name.as_str()) {
                if li.unique != idx.unique {
                    return Some(format!(
                        "index {}.{} uniqueness change {} → {}",
                        table, idx.name, li.unique, idx.unique
                    ));
                }
                if li.columns != idx.columns {
                    return Some(format!(
                        "index {}.{} column change {:?} → {:?}",
                        table, idx.name, li.columns, idx.columns
                    ));
                }
            }
        }

        // (5) A FOREIGN KEY set change — a redefinition (same name, changed body),
        //     an ADD (desired-only FK), or a DROP (live-only FK). SQLite inlines FKs
        //     at CREATE TABLE and has no `ALTER TABLE ADD/DROP CONSTRAINT`, so ANY FK
        //     set difference is a rebuild.
        let live_fk: BTreeMap<&str, &ConstraintSnapshot> = lt
            .constraints
            .iter()
            .filter(|c| c.kind == "FOREIGN KEY")
            .map(|c| (c.name.as_str(), c))
            .collect();
        let desired_fk: BTreeMap<&str, &ConstraintSnapshot> = dt
            .constraints
            .iter()
            .filter(|c| c.kind == "FOREIGN KEY")
            .map(|c| (c.name.as_str(), c))
            .collect();
        for (name, dc) in &desired_fk {
            match live_fk.get(name) {
                None => return Some(format!("add foreign key {table}.{name}")),
                Some(lc) if lc.definition != dc.definition => {
                    return Some(format!(
                        "foreign key {table}.{name} definition change {:?} → {:?}",
                        lc.definition, dc.definition
                    ));
                }
                Some(_) => {}
            }
        }
        for name in live_fk.keys() {
            if !desired_fk.contains_key(name) {
                return Some(format!("drop foreign key {table}.{name}"));
            }
        }

        // (6) A DROP COLUMN of a CONSTRAINED column (H1). SQLite's native
        //     `ALTER TABLE … DROP COLUMN` ERRORS at apply when the dropped column
        //     participates in ANY index, CHECK, foreign key, generated-column
        //     expression, or partial-index predicate — so the per-op
        //     `render_drop_column` would abort the migration. We route such a drop
        //     to the 12-step rebuild (which omits the column from `copy_columns` and
        //     recreates only the surviving dependents). A column that is
        //     UNconstrained drops natively via the per-op path (no rebuild).
        //
        //     "Dropped" = a column present in LIVE, absent from DESIRED, and NOT a
        //     rename `from` (a rename is its own rebuild trigger, handled in step 1).
        let desired_names: BTreeSet<&str> =
            dt.columns.iter().map(|c| c.name.as_str()).collect();
        let renamed_from: BTreeSet<&str> =
            table_renames.iter().map(|r| r.from.as_str()).collect();
        for lc in &lt.columns {
            let col = lc.name.as_str();
            if desired_names.contains(col) || renamed_from.contains(col) {
                continue; // surviving column, or handled by the rename path
            }
            // This `col` is being dropped. Does any index / constraint / raw-DDL
            // dependent of the LIVE table reference it? If so → rebuild.
            if let Some(dep) = Self::sqlite_dropped_column_dependent(table, lc, lt) {
                return Some(dep);
            }
        }

        None
    }

    /// **H1 helper** — is the live column `lc` (being dropped) referenced by any
    /// index, constraint, or raw-DDL dependent of the live table `lt`, such that a
    /// native `SQLite` `DROP COLUMN` would ERROR? Returns `Some(reason)` to route the
    /// drop to the 12-step rebuild, `None` if the column drops cleanly per-op.
    ///
    /// Sources, in fail-closed order:
    ///   1. INDEX key columns (`IndexSnapshot::columns`) — a column in any index.
    ///   2. CONSTRAINT definitions (`ConstraintSnapshot::definition`) — the synthesised
    ///      FK / UNIQUE / PK bodies carry the member column names verbatim.
    ///   3. The verbatim `CREATE TABLE` text (`TableSnapshot::stored_create_sql`),
    ///      the ONLY source for CHECK predicates, generated-column expressions, and
    ///      partial-index predicates — none of which the `SQLite` drift PRAGMAs surface
    ///      into the structured snapshot. We do a CONSERVATIVE whole-word scan: if the
    ///      column name appears as a word ANYWHERE in the stored DDL beyond its own
    ///      definition, we rebuild. This can over-trigger a rebuild (a comment / a
    ///      coincidental match) but NEVER under-triggers — a rebuild is always
    ///      data-preserving, while a wrong native DROP COLUMN aborts the migration.
    fn sqlite_dropped_column_dependent(
        table: &str,
        lc: &ColumnSnapshot,
        lt: &TableSnapshot,
    ) -> Option<String> {
        let col = lc.name.as_str();

        // (1) Any index over this column.
        for idx in &lt.indexes {
            if idx.columns.iter().any(|c| c == col) {
                return Some(format!(
                    "drop column {table}.{col} referenced by index {}",
                    idx.name
                ));
            }
        }

        // (2) Any constraint whose definition names this column (FK / UNIQUE / PK).
        for c in &lt.constraints {
            if word_present_ci(&c.definition, col) {
                return Some(format!(
                    "drop column {table}.{col} referenced by constraint {} ({})",
                    c.name, c.kind
                ));
            }
        }

        // (3) The verbatim CREATE text — the only source for CHECK / generated /
        //     partial-index references. We scan the WHOLE statement (conservative:
        //     over-trigger acceptable, under-trigger never), as a whole word so a
        //     substring of another identifier does not false-match.
        if let Some(sql) = lt.stored_create_sql.as_deref() {
            // Strip this column's OWN definition clause is unnecessary for
            // correctness (a rebuild is always safe); a hit anywhere routes to the
            // rebuild. The column's own clause naturally matches, but the per-op
            // path is only taken when NO dependent exists — and a column always
            // appears in its own clause — so we must look for a SECOND occurrence
            // (a reference beyond the bare declaration) to avoid rebuilding EVERY
            // drop. Count whole-word occurrences; >1 means a reference exists.
            if word_count_ci(sql, col) > 1 {
                return Some(format!(
                    "drop column {table}.{col} referenced by a CHECK / generated / \
                     partial-index expression in the stored table DDL"
                ));
            }
        }

        None
    }

    /// **P3b** — build the [`SqliteRebuild`] (spec + journal migration) that
    /// reconciles `live` → `desired` for one existing table via the 12-step rebuild.
    ///
    /// The new-table CREATE comes from the shared Sqlite/MainUnqualified emitter
    /// (goodie sentinels + FKs), re-pointed to the engine-chosen TEMP name. The copy
    /// mapping carries every column present in BOTH shapes (a RENAME maps `to ←
    /// from`); a dropped column is excluded, an added one takes its DEFAULT/NULL. The
    /// recreate set is the desired table's non-PK, non-system indexes (the
    /// system-field indexes are emitted inline in the new CREATE by the shared
    /// emitter, like the create-table path).
    fn build_sqlite_rebuild(
        &self,
        table: &str,
        desired_full: &DesiredSchema,
        lt: &TableSnapshot,
        dt: &TableSnapshot,
        table_renames: &[&ResolvedRename],
        reason: String,
    ) -> Result<SqliteRebuild, DeclarativeError> {
        // The new table's CREATE (real name), then re-point it to the temp name. The
        // MainUnqualified emitter output begins `CREATE TABLE IF NOT EXISTS "<table>"`
        // (the `IF NOT EXISTS` + the quoted identifier); we rewrite ONLY that leading
        // quoted identifier so the body (columns, sentinels, FKs) is byte-identical to
        // a fresh create. The quoted table name appears FIRST in the statement (the
        // column/constraint bodies that follow can only reference it via self-FK as
        // `REFERENCES "<table>"`, which on the rebuild path is fine to leave pointing
        // at the FINAL name — the table is renamed back to `<table>` before the FK is
        // enforced), so a single replacement of the first `"<table>"` occurrence is
        // exact and safe.
        let create_real = self.render_create_table_sqlite(table, desired_full)?;
        let tmp = SqliteRebuildSpec::tmp_name(table);
        let real_q = quote_ident(table);
        let tmp_q = quote_ident(&tmp);
        // The first occurrence of the quoted real table name is the CREATE target.
        let new_table_create = match create_real.find(&real_q) {
            Some(pos) => {
                let mut s = create_real.clone();
                s.replace_range(pos..pos + real_q.len(), &tmp_q);
                s
            }
            None => {
                // The emitter shape changed out from under us — fail closed rather
                // than emit a CREATE under the real name (which would collide with the
                // table we are about to drop) or a malformed statement.
                return Err(DeclarativeError::Invalid(format!(
                    "internal: SQLite rebuild of '{table}' could not re-point the emitted CREATE \
                     to the temp name (emitter shape mismatch); refusing to emit a colliding CREATE"
                )));
            }
        };

        // The copy mapping: every column present in BOTH the old and new shapes.
        // - a RENAME's `to` (new) maps to its `from` (old) — data follows the rename;
        // - a dropped column (live-only) is excluded;
        // - an added column (desired-only, not a rename `to`) is excluded (DEFAULT/NULL).
        let live_names: BTreeSet<&str> = lt.columns.iter().map(|c| c.name.as_str()).collect();
        let rename_to_from: BTreeMap<&str, &str> = table_renames
            .iter()
            .map(|r| (r.to.as_str(), r.from.as_str()))
            .collect();
        let mut copy_columns: Vec<(String, String)> = Vec::new();
        for c in &dt.columns {
            let dest = c.name.as_str();
            if let Some(src) = rename_to_from.get(dest) {
                // RENAME: copy from the old column name into the new one (the old
                // name must be live for the SELECT to resolve).
                if live_names.contains(src) {
                    copy_columns.push((dest.to_string(), (*src).to_string()));
                }
            } else if live_names.contains(dest) {
                // A kept column (same name on both sides): copy straight across.
                copy_columns.push((dest.to_string(), dest.to_string()));
            }
            // else: an added column — no source; it takes its DEFAULT/NULL.
        }

        // C2 — the recreate set is EMPTY on the declarative path. The executor
        // ([`SqliteBackend::rebuild_one`]) is the source of truth for the table's own
        // indexes + triggers: it captures their `sql` TEXT VERBATIM from the live
        // `sqlite_master` before the `DROP TABLE` and replays it after the rename, so
        // partial/expression/collation/DESC index attributes AND creator triggers
        // survive exactly. The previous path rebuilt indexes from the DESIRED
        // `IndexSnapshot` (lossy — it dropped those attributes) and never touched
        // triggers (silently destroying them on `DROP TABLE`). `recreate_objects`
        // remains on the spec as an explicit escape hatch for direct-spec callers.
        let recreate_objects: Vec<String> = Vec::new();

        // H1 — the columns this rebuild DROPS: live columns absent from the new
        // (desired) shape, excluding a rename's `from` (a rename CARRIES the column
        // under a new name, it is not a drop). The executor uses this to SKIP
        // replaying any captured dependent (index / trigger) that references a
        // dropped column — such a dependent is dropped WITH the column.
        let desired_names: BTreeSet<&str> =
            dt.columns.iter().map(|c| c.name.as_str()).collect();
        let rename_from: BTreeSet<&str> =
            table_renames.iter().map(|r| r.from.as_str()).collect();
        let dropped_columns: Vec<String> = lt
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| !desired_names.contains(n) && !rename_from.contains(n))
            .map(ToString::to_string)
            .collect();

        let spec = SqliteRebuildSpec {
            table: table.to_string(),
            tmp_table: tmp,
            new_table_create,
            copy_columns,
            recreate_objects,
            dropped_columns,
            reason: reason.clone(),
        };

        // The journal migration: its `up` carries the new-table CREATE (so the
        // checksum certifies the rebuilt shape and a preview can inspect it), but the
        // ACTUAL apply is the structured spec, NOT a plain `up` execution. A rebuild
        // on a populated table is DESTRUCTIVE (it drops + recreates), so the flags
        // route it through the destructive/approval gate. `down: None` — the reverse
        // of a rebuild is itself a rebuild (authored from the prior desired shape),
        // never a plain statement.
        let migration = self.make(
            &format!("sqlite_rebuild_{table}"),
            spec.new_table_create.clone(),
            None,
            destructive_flags(),
            Vec::new(),
        );

        Ok(SqliteRebuild { migration, spec })
    }

    /// **PR2 — the cross-subsystem `renameColumn` bridge (§2.6 / §2.6.1 / §2.6.2).**
    /// Lower ONE IR `renameColumn` op into its dialect-chosen
    /// [`RenameStep`](crate::render::step::RenameStep), REUSING the existing destination
    /// authors verbatim so the IR path inherits their version-stable ids:
    ///
    /// - **Postgres** ⇒ build the [`OnlineIntent::RenameColumn`] with the PG type
    ///   string `pg_ty` (the IR's dialect-neutral column type, already mapped to its
    ///   PG `data_type` and `ddl_type`-spelled by the caller, §2.6) and run it
    ///   through [`ExpandContractAuthor::author`] — the SAME author the declarative
    ///   diff path calls, so the E1..C2 ids + intra-chain `depends_on` are authored
    ///   identically (§2.6.1). The returned [`ExpandContractPlan`] is wrapped
    ///   verbatim into [`crate::render::step::RenameStep::PgExpandContract`].
    ///
    /// - **SQLite** ⇒ synthesize the DESIRED post-rename inputs the differ's
    ///   12-step rebuild planner consumes — the live `TableSnapshot` with the
    ///   `from`→`to` column renamed (its `data_type` carried across UNCHANGED: a
    ///   pure rename never changes type, and the rebuild's rendered CREATE takes its
    ///   per-column SQLite affinity from the SDK schema `Value`'s field token, not
    ///   from this snapshot `data_type`), the live SDK schema `Value` with the same
    ///   field-key rename, and a [`RenameHint`] — and route them through
    ///   [`Self::diff`]. The diff yields exactly ONE [`SqliteRebuild`] (a rename
    ///   always needs a rebuild on SQLite), wrapped into
    ///   [`crate::render::step::RenameStep::SqliteRebuild`]. NO PG type string is ever passed to this leg
    ///   — the affinity comes from the SDK Value, which the caller built from the
    ///   dialect-neutral `ColType`.
    ///
    /// `live_snapshot` / `live_sqlite_schema` are this table's full introspected
    /// structure (the SQLite leg needs the whole shape, not just the column being
    /// renamed). `pg_ty` is used only on the PG leg.
    ///
    /// # Errors
    /// [`DeclarativeError`] if the expand-contract author rejects the intent (empty/
    /// identical names) or the differ cannot resolve the rebuild (un-matchable hint,
    /// emitter shape mismatch).
    // This is a deliberate WIDE cross-subsystem bridge: it carries the rename's
    // {table, from, to}, the per-dialect type/shape inputs (`pg_ty` for PG; the live
    // snapshot + SDK Value for the SQLite rebuild), AND the real introspected owner
    // for the cross-app guard. Bundling them into a struct would only relocate the
    // same fields; the explicit signature documents exactly what each leg consumes.
    #[allow(clippy::too_many_arguments)]
    pub fn lower_ir_rename(
        &self,
        table: &str,
        from: &str,
        to: &str,
        pg_ty: &str,
        live_snapshot: &TableSnapshot,
        live_sqlite_schema: &serde_json::Value,
        live_owner: &str,
    ) -> Result<crate::render::step::RenameStep, DeclarativeError> {
        match self.dialect {
            SqlDialect::Postgres => {
                // The PG expand-contract author IS the id authority (§2.6.1): the
                // declarative path calls the SAME `ExpandContractAuthor::author` with
                // the SAME `OnlineIntent` fields, so the authored E1..C2 ids +
                // intra-chain `depends_on` match by construction.
                let ec = ExpandContractAuthor::new(&self.project_schema, &self.owner_app);
                let plan = ec
                    .author(&OnlineIntent::RenameColumn {
                        table: table.to_string(),
                        from: from.to_string(),
                        to: to.to_string(),
                        ty: pg_ty.to_string(),
                    })
                    .map_err(|e| {
                        DeclarativeError::Invalid(format!(
                            "renameColumn expand-contract author rejected '{table}.{from}→{to}': {e}"
                        ))
                    })?;
                Ok(crate::render::step::RenameStep::PgExpandContract(plan))
            }
            SqlDialect::Sqlite => {
                let rebuild = self.sqlite_rename_rebuild(
                    table,
                    from,
                    to,
                    live_snapshot,
                    live_sqlite_schema,
                    live_owner,
                )?;
                Ok(crate::render::step::RenameStep::SqliteRebuild(rebuild))
            }
            SqlDialect::Mysql => Err(DeclarativeError::UnsupportedInV1(
                "renameColumn is not live-rendered for MySQL in render-only Phase 1".to_string(),
            )),
        }
    }

    /// **PR2** — the SQLite arm of [`Self::lower_ir_rename`]: synthesize the desired
    /// post-rename snapshot + SDK schema + [`RenameHint`] from the live table facts
    /// and route them through [`Self::diff`], returning the SINGLE [`SqliteRebuild`]
    /// it produces. Factored out so the dialect router stays readable.
    ///
    /// The desired snapshot is the live snapshot with `from` renamed to `to` (the
    /// `data_type` carried across UNCHANGED — a rename never changes type, and the
    /// rename-hint resolver requires `live_from.data_type == desired_to.data_type`);
    /// the desired SDK schema is the live `Value` with the same field-key rename; the
    /// `RenameHint` lets the differ resolve the drop+add pair as a rename rather than
    /// a destructive column swap.
    ///
    /// **Ownership (MED).** Both the desired `ownership` and the `live_ownership`
    /// maps are stamped from the caller-supplied `live_owner` — the REAL introspected
    /// owner of the table, NOT the deploying app. This keeps the differ's cross-app
    /// guards honest: if `live_owner != self.owner_app`, the rename is a structural
    /// change to a FOREIGN table and `enforce_ownership` refuses it with
    /// `NotTableOwner`. (Previously both maps were fabricated as the deploying app,
    /// which would let app B silently rebuild app A's table once this leg is
    /// deploy-wired.)
    // Wide by design (the SQLite arm of the cross-subsystem rename bridge): it needs
    // the rename triple, the full live snapshot + SDK Value to author the rebuild,
    // and the real owner for the cross-app guard. See `lower_ir_rename`.
    #[allow(clippy::too_many_arguments)]
    fn sqlite_rename_rebuild(
        &self,
        table: &str,
        from: &str,
        to: &str,
        live_snapshot: &TableSnapshot,
        live_sqlite_schema: &serde_json::Value,
        live_owner: &str,
    ) -> Result<SqliteRebuild, DeclarativeError> {
        // ---- desired snapshot: live with `from`→`to` renamed (type unchanged) ----
        let mut desired_table = live_snapshot.clone();
        let mut found = false;
        for c in &mut desired_table.columns {
            if c.name == from {
                c.name = to.to_string();
                found = true;
            }
        }
        if !found {
            return Err(DeclarativeError::Invalid(format!(
                "renameColumn: live table '{table}' has no column '{from}' to rename \
                 (the rebuild needs the live structure to carry the value across)"
            )));
        }

        // ---- desired (post-rename) SDK schema `Value` ----
        // The shared SQLite emitter renders the new-table CREATE from this Value.
        //
        // TWO faithful sources for the SDK `Value`, distinguished by which field key it
        // carries (the live `from` or the post-rename `to`):
        //
        //  (1) **PRE-rename Value** (the field is keyed `from`) — the descriptor-set
        //      path (`apply_bundle_ir_sqlite`) supplies the PRE-rename SDK `Value`. We
        //      rename the field KEY `from`→`to` (facets preserved verbatim) to get the
        //      post-rename shape — byte-identical to a `t.*`-diff rename.
        //
        //  (2) **POST-rename Value** (the field is already keyed `to`) — **PR9b** the
        //      production catalog-sourced path (`LiveSchema::from_sqlite_catalog`)
        //      supplies the POST-deploy DESIRED descriptor `Value` (the field is already
        //      `to`, with its FULL facets — encryption/mask/FK/enum/default/… — none
        //      dropped, because they come straight from the descriptor, NOT a lossy
        //      catalog reconstruction). The live `from` column's facets are identical to
        //      the desired `to` column's (a rename preserves facets), so the desired
        //      post-rename `Value` IS the correct post-rename CREATE source as-is.
        //
        // We require the live `from` column to be present in `live_snapshot` (checked
        // above) so the value-copy mapping is authoritative; the SDK `Value` may then be
        // sourced from EITHER shape. If it carries NEITHER `from` nor `to`, fail closed.
        let desired_schema_value = if let Some(v) =
            rename_sdk_schema_field(live_sqlite_schema, from, to)
        {
            // (1) pre-rename Value → rename the field key to the post-rename shape.
            v
        } else if let Some(to_def) = live_sqlite_schema
            .as_object()
            .and_then(|o| o.get(to))
        {
            // (2) post-rename desired Value (already keyed `to`) → use as-is, BUT
            // ONLY after asserting its column AFFINITY equals the live `from` column's
            // (PR9b LOW fix). The new-table CREATE renders from THIS descriptor-sourced
            // `to` def, while the value-copy carries the old `from` bytes across
            // un-transformed; a `rename` preserves facets by contract, so a descriptor
            // whose `to` field diverges in affinity from the live `from` (e.g. a rename
            // bundled with an encryption/affinity change in the SAME descriptor) would
            // silently rebuild the column under a different affinity. Enforce the SAME
            // equality the snapshot-path `RenameHintTypeMismatch` guard enforces
            // (SQLite collapses `data_type` to affinity), failing closed on divergence
            // instead of emitting a silent shape skew.
            use zeroship_schema::query::{def_to_column_type_for_dialect, SqlDialect};
            let Some(live_from) = live_snapshot.columns.iter().find(|c| c.name == from) else {
                // `found` above already proved `from` is present; defensive.
                return Err(DeclarativeError::Invalid(format!(
                    "renameColumn: live table '{table}' lost column '{from}' between the \
                     rename-field check and the affinity guard (internal invariant)"
                )));
            };
            let to_affinity =
                sqlite_canonical_type(&def_to_column_type_for_dialect(to_def, SqlDialect::Postgres));
            let from_affinity = sqlite_canonical_type(&live_from.data_type);
            if to_affinity != from_affinity {
                return Err(DeclarativeError::RenameHintTypeMismatch {
                    table: table.to_string(),
                    from: from.to_string(),
                    to: to.to_string(),
                    from_type: live_from.data_type.clone(),
                    to_type: def_to_column_type_for_dialect(to_def, SqlDialect::Postgres),
                });
            }
            // PR9c LOW (ii) — TIGHTEN past affinity to the FULL data-transforming facet
            // set. Affinity equality alone is too weak: a same-affinity facet change on
            // the renamed column (e.g. add `encrypted`/`mask`/`default`/`enum`/`check`,
            // all of which a `string`/`number` column keeps its TEXT/NUMERIC affinity
            // under) is still rendered into the rebuilt CREATE while the value-copy
            // carries the live `from` bytes VERBATIM. The live catalog read does NOT
            // recover the `from` column's SDK facets (`ColumnSnapshot`'s
            // encryption/mask/default are emission-only and always `None` from
            // introspection — see drift.rs), so on THIS post-rename-descriptor path we
            // cannot prove the live `from` already carried the facet. Fail CLOSED if the
            // descriptor `to` def declares ANY such facet, rather than silently rebuild a
            // changed-facet column over un-transformed bytes (e.g. an `encrypted` CREATE
            // over plaintext, or an `enum`/`check` the old values may violate). The
            // pre-rename-descriptor path (branch 1) keeps the `from` facets and is
            // unaffected. A plain rename (no facet on `to_def`) passes unchanged.
            if let Some(facet) = data_transforming_facet(to_def) {
                return Err(DeclarativeError::RenameHintFacetMismatch {
                    table: table.to_string(),
                    from: from.to_string(),
                    to: to.to_string(),
                    facet,
                });
            }
            live_sqlite_schema.clone()
        } else {
            return Err(DeclarativeError::Invalid(format!(
                "renameColumn: SDK schema for '{table}' has neither the pre-rename field \
                 '{from}' nor the post-rename field '{to}' (cannot author the post-rename \
                 CREATE) — refusing to emit a rebuild from a partial view"
            )));
        };

        // ---- assemble the one-table DesiredSchema + live snapshot ----
        // **Cross-app guard correctness (MED).** The diff's `enforce_ownership`
        // (desired side) + drop-ownership (live side) guards are only sound if they
        // see the REAL introspected owner of the table — NOT the deploying app. So
        // stamp BOTH ownership maps from the caller-supplied `live_owner`. If the
        // table is owned by a DIFFERENT app, `enforce_ownership` sees the rename
        // (a structural ALTER) on a foreign table and refuses with `NotTableOwner`
        // (the deploying app is `self.owner_app`), exactly as a `t.*`-diff rename of
        // a foreign table would. A rename of one's OWN table (live_owner ==
        // self.owner_app) passes the guard unchanged.
        let mut desired_tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
        desired_tables.insert(table.to_string(), desired_table);
        let mut ownership: BTreeMap<String, String> = BTreeMap::new();
        ownership.insert(table.to_string(), live_owner.to_string());
        let mut sqlite_schemas: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        sqlite_schemas.insert(table.to_string(), desired_schema_value);
        let desired = DesiredSchema {
            snapshot: SchemaSnapshot { tables: desired_tables, ..Default::default() },
            ownership,
            sqlite_schemas,
        };

        let mut live_tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
        live_tables.insert(table.to_string(), live_snapshot.clone());
        let live = SchemaSnapshot { tables: live_tables, ..Default::default() };
        let mut live_ownership: HashMap<String, String> = HashMap::new();
        live_ownership.insert(table.to_string(), live_owner.to_string());

        let hint = RenameHint {
            table: table.to_string(),
            from: from.to_string(),
            to: to.to_string(),
        };

        let plan = self.diff(&desired, &live, &live_ownership, std::slice::from_ref(&hint))?;
        // A rename on SQLite is ALWAYS a rebuild (no native online rename); the diff
        // emits exactly one, and NO PG expand-contract.
        let mut rebuilds = plan.rebuilds;
        match (rebuilds.len(), plan.renames.is_empty()) {
            (1, true) => Ok(rebuilds.remove(0)),
            (n, renames_empty) => Err(DeclarativeError::Invalid(format!(
                "renameColumn SQLite lowering of '{table}.{from}→{to}' expected exactly \
                 one rebuild and no PG expand-contract, got {n} rebuild(s) / \
                 renames_empty={renames_empty} (internal rebuild-planner invariant)"
            ))),
        }
    }

    /// Render `CREATE TABLE <schema>.<table> (<cols…>, <pk>, <inline fks…>)`.
    fn render_create_table(
        &self,
        table: &str,
        t: &TableSnapshot,
        inline_fks: &[&ConstraintSnapshot],
    ) -> String {
        // `join(";\n")` over the structural statement list reproduces the canonical
        // multi-statement `up` byte-for-byte. The `diff` path takes this joined
        // form; the IR lower path takes the structural list directly (so a
        // string-literal DEFAULT carrying an interior `;\n` is never re-split).
        self.render_create_table_statements(table, t, inline_fks).join(";\n")
    }

    /// **Structural** form of [`render_create_table`]: the CREATE statement plus
    /// every follow-on `COMMENT ON COLUMN` sentinel, as a per-statement `Vec`.
    /// `join(";\n")` is byte-identical to [`render_create_table`].
    fn render_create_table_statements(
        &self,
        table: &str,
        t: &TableSnapshot,
        inline_fks: &[&ConstraintSnapshot],
    ) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        for c in &t.columns {
            // `id` carries the inline PRIMARY KEY.
            let inline_pk = c.name == "id";
            let ty = column_type_for_render(c, SqlDialect::Postgres, inline_pk);
            let pk = primary_key_clause(c, SqlDialect::Postgres, inline_pk);
            let null = null_clause(c, SqlDialect::Postgres, inline_pk);
            let identity = pg_identity_clause(c);
            let generated = generated_clause(c.generated.as_ref());
            // #4: emit the DEFAULT clause (emission-only metadata), including the
            // legacy T12 `__fts` generated-column sentinel path.
            let default = default_clause(c.default.as_deref());
            let checks = inline_checks_clause(c);
            // **P4 HALF A** — the inline `/* zsenc:… */` sentinel rides between
            // the type and the constraints, exactly as the shared kernel's
            // `field_to_column_for_dialect` bakes it, so a `generate`d encrypted
            // column is byte-identical to a `registerModel`-created one.
            let enc = c
                .encryption_sentinel
                .as_deref()
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            parts.push(format!(
                "{} {}{}{}{}{}{}{}{}",
                quote_ident(&c.name),
                ty,
                enc,
                identity,
                generated,
                pk,
                null,
                default,
                checks,
            ));
        }
        for fk in inline_fks {
            parts.push(self.fk_clause(fk));
        }
        // #3/#4: inline CHECK constraints (literal-pin, min/max, enum) as
        // table-level `CONSTRAINT <name> CHECK (...)` clauses. The definition is
        // the emitted DDL clause built by `field_check_constraints`.
        //
        // **PR15 (HIGH fix)** — a `createTable({ uniques })` table-level UNIQUE
        // (folded into the snapshot as a `UNIQUE` `ConstraintSnapshot` by
        // `render::lower::create_table_descriptor`'s spec-fold) is inlined here as a
        // `CONSTRAINT <name> <definition>` clause, the SAME shape a stand-alone
        // `addConstraint(unique)` renders (`UNIQUE (cols)`), so a table built with
        // a named unique round-trips against the live catalog. CHECK is inlined the
        // same way; both are emission-only bodies the differ does not re-diff.
        for c in &t.constraints {
            if c.kind == "CHECK" || c.kind == "UNIQUE" || c.kind == "EXCLUDE" {
                parts.push(format!("CONSTRAINT {} {}", quote_ident(&c.name), c.definition));
            }
        }
        let create = format!(
            "CREATE TABLE {} ({})",
            self.qualified(table),
            parts.join(", ")
        );
        let mut statements: Vec<String> = vec![create];
        // **P4 HALF A** — append `COMMENT ON COLUMN … '<sentinel>'` for every
        // column carrying a comment sentinel (`__zsmask:…` on a masked sibling,
        // `zsenc:…` on an encrypted column), so the runtime sentinel is part of
        // the same migration as the table create (an interrupted apply never
        // leaves a column without its sentinel). The comment body is built by
        // the shared codecs; we only quote it into the statement here. Each
        // COMMENT is its OWN structural statement (a guard-per-statement unit),
        // not a textual `;\n` split of the joined `up`.
        for c in &t.columns {
            if let Some(stmt) = self.comment_stmt(table, c) {
                statements.push(stmt);
            }
        }
        statements
    }

    fn render_create_table_sqlite_snapshot_statements(
        &self,
        table: &str,
        t: &TableSnapshot,
    ) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        for c in &t.columns {
            let inline_pk = c.name == "id";
            let ty = column_type_for_render(c, SqlDialect::Sqlite, inline_pk);
            let pk = primary_key_clause(c, SqlDialect::Sqlite, inline_pk);
            let null = null_clause(c, SqlDialect::Sqlite, inline_pk);
            let generated = generated_clause(c.generated.as_ref());
            let default = default_clause(c.default.as_deref());
            let checks = inline_checks_clause(c);
            let enc = c
                .encryption_sentinel
                .as_deref()
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            let sqlite_inline_sentinel = if c.encryption_sentinel.is_none() {
                c.comment_sentinel
                    .as_deref()
                    .map(|s| format!(" /* {s} */"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            parts.push(format!(
                "{} {}{}{}{}{}{}{}{}",
                quote_ident(&c.name),
                ty,
                enc,
                sqlite_inline_sentinel,
                generated,
                pk,
                null,
                default,
                checks,
            ));
        }
        for c in &t.constraints {
            if c.kind == "CHECK" || c.kind == "UNIQUE" {
                parts.push(format!("CONSTRAINT {} {}", quote_ident(&c.name), c.definition));
            }
        }
        let mut statements = vec![format!(
            "CREATE TABLE {} ({})",
            quote_ident(table),
            parts.join(", ")
        )];
        let emitter = SqliteEmitter;
        for idx in system_field_indexes(table) {
            let (up, _) = emitter.create_index(table, &idx);
            statements.push(up);
        }
        statements
    }

    fn render_create_table_mysql_snapshot_statements(
        &self,
        table: &str,
        t: &TableSnapshot,
        inline_fks: &[&ConstraintSnapshot],
    ) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        let mut consumed_enum_checks = BTreeSet::new();
        for c in &t.columns {
            let inline_pk = c.name == "id";
            let enum_check_name = check_constraint_name(table, &c.name, "enum");
            let enum_type = t
                .constraints
                .iter()
                .find(|chk| chk.kind == "CHECK" && chk.name == enum_check_name)
                .and_then(|chk| mysql_enum_type_from_check(&chk.definition, &c.name));
            if enum_type.is_some() {
                consumed_enum_checks.insert(enum_check_name);
            }
            let ty = enum_type.unwrap_or_else(|| {
                column_type_for_render(c, SqlDialect::Mysql, inline_pk)
            });
            let pk = primary_key_clause(c, SqlDialect::Mysql, inline_pk);
            let null = null_clause(c, SqlDialect::Mysql, inline_pk);
            let identity = mysql_identity_clause(c);
            let generated = mysql_generated_clause(c.generated.as_ref());
            let default = mysql_default_clause(c.default.as_deref());
            let checks = inline_checks_clause(c);
            parts.push(format!(
                "{} {}{}{}{}{}{}{}",
                mysql_quote_ident(&c.name),
                ty,
                identity,
                generated,
                pk,
                null,
                default,
                checks,
            ));
        }
        for fk in inline_fks {
            parts.push(self.mysql_fk_clause(fk));
        }
        for c in &t.constraints {
            if consumed_enum_checks.contains(&c.name) {
                continue;
            }
            if c.kind == "CHECK" || c.kind == "UNIQUE" {
                parts.push(format!(
                    "CONSTRAINT {} {}",
                    mysql_quote_ident(&c.name),
                    mysql_requote_sql(&c.definition)
                ));
            }
        }
        vec![format!(
            "CREATE TABLE {} ({})",
            mysql_qualified(&self.project_schema, table),
            parts.join(", ")
        )]
    }

    /// **P4 HALF A** — render the `COMMENT ON COLUMN <schema>.<table>.<col> IS
    /// '<sentinel>'` statement for a column carrying a `comment_sentinel`
    /// (`__zsmask:…` for a masked sibling or `zsenc:…` for an encrypted column),
    /// or `None` for a column without one. The sentinel BODY is built by the
    /// shared codecs (threaded onto the snapshot in `desired_snapshot`) — never
    /// re-spelled here; this only wraps it in the schema-qualified
    /// `COMMENT ON COLUMN` statement with the SQL-literal single-quote escape,
    /// matching the shared kernel's `build_mask_sentinel_comment_for_field`
    /// spelling so a `generate`d sentinel is byte-identical to a
    /// `registerModel`-written one.
    fn comment_stmt(&self, table: &str, c: &ColumnSnapshot) -> Option<String> {
        let sentinel = c.comment_sentinel.as_deref()?;
        let escaped = sentinel.replace('\'', "''");
        Some(format!(
            "COMMENT ON COLUMN {}.{} IS '{}'",
            self.qualified(table),
            quote_ident(&c.name),
            escaped,
        ))
    }

    /// Render a `CONSTRAINT … FOREIGN KEY (…) REFERENCES <schema>.<tgt> (id)
    /// [<policy>]` clause for inline CREATE TABLE / ALTER ADD CONSTRAINT use.
    ///
    /// The ON UPDATE / ON DELETE / DEFERRABLE policy tail is carried in the
    /// constraint `definition` (built by [`fk_definition_pg`] in the canonical
    /// `pg_get_constraintdef` spelling). Postgres accepts that same clause order
    /// as DDL, so the tail is appended verbatim — the applied constraint then
    /// introspects back to the identical definition, and the FK round-trips clean
    /// (#1). A bare FK (no policy tail) emits nothing extra.
    fn fk_clause(&self, fk: &ConstraintSnapshot) -> String {
        if matches!(self.dialect, SqlDialect::Mysql) {
            return self.mysql_fk_clause(fk);
        }
        let col = fk_local_column(&fk.definition).unwrap_or_default();
        let target = fk_target_table(&fk.definition).unwrap_or_default();
        let policy = fk_policy_tail(&fk.definition);
        format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} (id){}",
            quote_ident(&fk.name),
            quote_ident(&col),
            self.qualified(&target),
            policy,
        )
    }

    fn mysql_fk_clause(&self, fk: &ConstraintSnapshot) -> String {
        let col = fk_local_column(&fk.definition).unwrap_or_default();
        let target = fk_target_table(&fk.definition).unwrap_or_default();
        let policy = mysql_fk_policy_tail(&fk.definition);
        format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} (`id`){policy}",
            mysql_quote_ident(&fk.name),
            mysql_quote_ident(&col),
            mysql_qualified(&self.project_schema, &target),
        )
    }

    /// Render a deferred `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY …`.
    fn render_add_fk(
        &self,
        table: &str,
        fk: &ConstraintSnapshot,
        depends_on: Vec<MigrationId>,
    ) -> Migration {
        let table_ref = match self.dialect {
            SqlDialect::Postgres => self.qualified(table),
            SqlDialect::Sqlite => quote_ident(table),
            SqlDialect::Mysql => mysql_qualified(&self.project_schema, table),
        };
        let up = format!(
            "ALTER TABLE {} ADD {}",
            table_ref,
            self.fk_clause(fk)
        );
        let down = match self.dialect {
            SqlDialect::Mysql => format!(
                "ALTER TABLE {} DROP FOREIGN KEY {}",
                table_ref,
                mysql_quote_ident(&fk.name)
            ),
            SqlDialect::Postgres | SqlDialect::Sqlite => format!(
                "ALTER TABLE {} DROP CONSTRAINT {}",
                table_ref,
                quote_ident(&fk.name)
            ),
        };
        self.make(
            &format!("add_fk_{}_{}", table, fk.name),
            up,
            Some(down),
            MigrationFlags::default(),
            depends_on,
        )
    }

    /// Render an `ALTER TABLE … ADD COLUMN …` (additive).
    ///
    /// #4 volatile-default trap: a column DEFAULT is emitted here. The engine only
    /// ever emits IMMUTABLE literal defaults (string/number/boolean literals,
    /// `'{}'::jsonb`, `'[]'::jsonb` — never `NOW()` / `gen_random_uuid()`), so
    /// `ADD COLUMN … DEFAULT <literal>` takes Postgres' metadata-only fast path
    /// (no table rewrite) and stays a safe ADDITIVE op — matching plugin-db's
    /// `diff.rs:15-26` reasoning (it never emits a volatile default either). The
    /// classifier therefore correctly classifies it additive, not destructive.
    fn render_add_column(&self, table: &str, c: &ColumnSnapshot) -> Migration {
        self.render_add_column_with_statements(table, c).0
    }

    /// **Structural** form of [`render_add_column`]: the migration plus its
    /// per-statement list (`ADD COLUMN` + optional follow-on `COMMENT ON COLUMN`).
    /// `join(";\n")` over the statements is byte-identical to the migration's `up`.
    /// The IR lower path consumes the statement list so a string-literal DEFAULT
    /// carrying an interior `;\n` is never re-split mid-statement.
    fn render_add_column_with_statements(
        &self,
        table: &str,
        c: &ColumnSnapshot,
    ) -> (Migration, Vec<String>) {
        // P1 — emission delegated to the per-dialect `DdlEmitter` (the mask /
        // encrypted sentinel spelling + qualification differ by dialect). This
        // method owns only the migration identity / flags.
        let (statements, down) = self.emitter().add_column(table, c);
        let up = statements.join(";\n");
        let mig = self.make(
            &format!("add_column_{table}_{}", c.name),
            up,
            down,
            MigrationFlags::default(),
            Vec::new(),
        );
        (mig, statements)
    }

    /// Render a GATED `ALTER TABLE … ALTER COLUMN … TYPE …` (P3 type change).
    ///
    /// A type change is `destructive` + `requires_approval` in v1 — there is NO
    /// auto type-change. It can rewrite the whole table under `ACCESS EXCLUSIVE`
    /// and can be lossy (e.g. `text` → `integer` fails / truncates), so it flows
    /// through the gate exactly like a drop. The `USING <col>::<type>` cast is
    /// emitted so a compatible widening (e.g. `integer` → `double precision`)
    /// applies without a manual cast; an incompatible change still fails loudly at
    /// apply (never silently). Type spelling goes through [`validate_type`] (via
    /// `validate_desired`) + the guard.
    ///
    /// `down` is `None`: a type change is treated as irreversible (the reverse
    /// cast may not round-trip — `double precision` → `integer` loses the
    /// fraction), so there is no structural down. A re-diff after applying it is
    /// clean because live then matches desired.
    fn render_alter_column_type(&self, table: &str, c: &ColumnSnapshot) -> Migration {
        let ty = column_type_for_render(c, self.dialect, false);
        let up = format!(
            "ALTER TABLE {} ALTER COLUMN {} TYPE {} USING {}::{}",
            self.qualified(table),
            quote_ident(&c.name),
            ty,
            quote_ident(&c.name),
            ty,
        );
        self.make(
            &format!("alter_column_type_{table}_{}", c.name),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        )
    }

    /// Render an `ALTER TABLE … ALTER COLUMN … {SET|DROP} NOT NULL` (P3
    /// nullability change).
    ///
    /// - **`DROP NOT NULL`** (`nullable` true — relaxing required true→false) is
    ///   SAFE: it only removes a constraint, never rewrites data, so it is ungated
    ///   (default flags) and applies like an additive op. `down` re-tightens.
    /// - **`SET NOT NULL`** (`nullable` false — tightening required false→true) is
    ///   lock-heavy (full scan under `ACCESS EXCLUSIVE`) and FAILS if any existing
    ///   row is NULL, so it is GATED (`destructive` is false — no data is lost —
    ///   but `requires_approval` is true; a later analyzer-lint plan will suggest
    ///   the `CHECK … NOT VALID` → `VALIDATE` online path). `down` relaxes it.
    fn render_alter_column_nullability(
        &self,
        table: &str,
        col: &str,
        nullable: bool,
    ) -> Migration {
        let (verb, reverse, flags) = if nullable {
            // DROP NOT NULL — safe, ungated; down re-adds NOT NULL.
            ("DROP NOT NULL", "SET NOT NULL", MigrationFlags::default())
        } else {
            // SET NOT NULL — gated (lock-heavy, can fail on existing NULLs). Not
            // "destructive" (no data is lost) but requires_approval. down relaxes it.
            (
                "SET NOT NULL",
                "DROP NOT NULL",
                MigrationFlags {
                    requires_approval: true,
                    ..MigrationFlags::default()
                },
            )
        };
        let up = format!(
            "ALTER TABLE {} ALTER COLUMN {} {}",
            self.qualified(table),
            quote_ident(col),
            verb
        );
        let down = format!(
            "ALTER TABLE {} ALTER COLUMN {} {}",
            self.qualified(table),
            quote_ident(col),
            reverse
        );
        self.make(
            &format!("alter_column_null_{table}_{col}"),
            up,
            Some(down),
            flags,
            Vec::new(),
        )
    }

    /// Render a `CREATE [UNIQUE] INDEX IF NOT EXISTS …`.
    fn render_create_index(
        &self,
        table: &str,
        idx: &IndexSnapshot,
        depends_on: Vec<MigrationId>,
    ) -> Migration {
        // P1 — emission delegated to the per-dialect `DdlEmitter`: PG spells the
        // access-method (`USING …`), the per-column opclass, the `WITH (lists=…)`
        // storage param and qualifies; SQLite emits a plain unqualified B-tree
        // index. (The snapshot carries covered columns VERBATIM — 1a — so the
        // emitter writes them directly, no name-based reconstruction.) This method
        // owns only the migration identity / deps.
        let (up, down) = self.emitter().create_index(table, idx);
        // **FTS** — the SQLite FTS5 index is a `CREATE VIRTUAL TABLE … USING fts5(…)`
        // (+ sync triggers), which the hardened SQLite authorizer permits ONLY under
        // EngineJournal mode (a creator may never make a vtable). Mark the migration
        // `engine_goodie_ddl` so the apply path runs its `up` in engine mode. This is
        // safe — the DDL is engine-authored from the `.fts()` descriptor, not raw
        // creator SQL. Every other index (PG gin/gist/ivfflat, plain btree) is
        // ordinary CreatorUp-confined DDL, byte-identical to before.
        let flags = MigrationFlags {
            engine_goodie_ddl: idx.access_method == SQLITE_FTS5_ACCESS_METHOD,
            ..MigrationFlags::default()
        };
        self.make(
            &format!("create_index_{}", idx.name),
            up,
            Some(down),
            flags,
            depends_on,
        )
    }

    /// Render a destructive (gated) `DROP TABLE` — `destructive = true,
    /// requires_approval = true` so the gate refuses it without approval.
    /// Render a destructive (gated) `DROP TABLE`.
    ///
    /// PHASE 4 / H1 — like `render_drop_column`, the confined SQLite path runs in
    /// the app file's `main` schema (the per-app file is opened directly, not
    /// ATTACHed under a `"<app>"` namespace as in PG), so the table is referenced
    /// UNqualified. A schema-qualified `"default"."c2"` resolves to no table on
    /// SQLite ("no such table: default.c2"). The PG path keeps `self.qualified`.
    fn render_drop_table(&self, table: &str) -> Migration {
        // P1 — qualification delegated to the per-dialect `DdlEmitter`.
        let up = self.emitter().drop_table_up(table);
        self.make(
            &format!("drop_table_{table}"),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        )
    }

    /// Render an `ALTER TABLE <old> RENAME TO <new>`.
    ///
    /// A whole-table rename is a FAST catalog-metadata operation (it is NOT the
    /// online column expand-contract). It is NOT data-loss `destructive` (the
    /// inverse rename in `down` fully reverses it), but it IS backward-incompatible
    /// — it silently breaks every reader of the OLD table name — so it carries
    /// `requires_approval` (never auto-applied), matching the `flags_for` MED-1 gate
    /// that classifies a literal `RENAME TABLE` in a submitted `up`.
    fn render_rename_table(&self, table: &str, to: &str) -> Migration {
        let (up, down) = self.emitter().rename_table(table, to);
        self.make(
            &format!("rename_table_{table}_to_{to}"),
            up,
            Some(down),
            MigrationFlags { requires_approval: true, ..MigrationFlags::default() },
            Vec::new(),
        )
    }

    /// Render a destructive (gated) `DROP COLUMN`.
    ///
    /// PHASE 4 — SQLite ≥ 3.35 has native `ALTER TABLE … DROP COLUMN`; emit it
    /// UNqualified (`main` = the app file). A schema-qualified `"schema"."t"` would
    /// resolve to no table. The PG path keeps `self.qualified`.
    fn render_drop_column(&self, table: &str, col: &str) -> Migration {
        // P1 — qualification delegated to the per-dialect `DdlEmitter`.
        let up = self.emitter().drop_column_up(table, col);
        self.make(
            &format!("drop_column_{table}_{col}"),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        )
    }

    /// Render a `DROP INDEX`.
    ///
    /// Dropping a PLAIN (non-unique) index is **not data loss** — it is fully
    /// reversible by recreating the index — so it carries default (non-destructive)
    /// flags and flows through the engine gate ungated, like an additive op.
    ///
    /// Dropping a **UNIQUE** index, however, silently removes a data-integrity
    /// guarantee (#4): duplicate rows become possible afterwards and a later
    /// re-add fails on the now-dirty data. That is an integrity change the
    /// creator never approved, so it is classified `destructive +
    /// requires_approval` (gated, like DROP COLUMN). (The implicit PK index is
    /// never reached here — `diff` filters it via `is_pk_index`.)
    ///
    /// `down` recreates nothing because the declarative re-diff would re-add the
    /// index from the desired snapshot.
    fn render_drop_index(&self, table: Option<&str>, idx: &IndexSnapshot) -> Migration {
        // P1 — the index-name qualification is delegated to the per-dialect
        // `DdlEmitter` (PG qualifies; SQLite MUST emit unqualified or the DROP
        // silently no-ops). The unique-vs-plain GATING below is diff-logic and
        // stays here.
        let up = self.emitter().drop_index_up(table, &idx.name);
        let flags = if idx.unique {
            destructive_flags()
        } else {
            MigrationFlags::default()
        };
        self.make(
            &format!("drop_index_{}", idx.name),
            up,
            None,
            flags,
            Vec::new(),
        )
    }

    // -----------------------------------------------------------------------
    // §6.4 / §6.5 — the IR-path render seam. `IrAuthor::lower` (below) reuses
    // these EXACT render methods + the shared snapshot-builder, so its emitted
    // SQL is byte-identical to the declarative path's by CONSTRUCTION (the §6.4
    // golden guards against accidental regression, not against two independent
    // implementations).
    // -----------------------------------------------------------------------

    /// §6.4 — render a single-table CREATE the SAME way the declarative `diff`
    /// pass does (the snapshot comes from the shared [`build_table_snapshot`]).
    /// FKs are inlined iff their target table is already live (`live_tables`);
    /// on PG a non-live target is DEFERRED to an `ALTER TABLE ADD CONSTRAINT`
    /// (returned in `deferred`), on SQLite it is a hard error (no late ADD
    /// CONSTRAINT) — mirroring `diff`'s per-table logic byte-for-byte.
    pub(crate) fn lower_create_table(
        &self,
        table: &str,
        snapshot: &TableSnapshot,
        sqlite_schema: &serde_json::Value,
        live_tables: &std::collections::BTreeSet<String>,
        guard: Option<crate::model::probe::GuardDir>,
    ) -> Result<Vec<LoweredUnit>, DeclarativeError> {
        let is_sqlite = matches!(self.dialect, SqlDialect::Sqlite);
        let mut inline_fks: Vec<&ConstraintSnapshot> = Vec::new();
        let mut deferred: Vec<&ConstraintSnapshot> = Vec::new();
        for c in &snapshot.constraints {
            if c.kind != "FOREIGN KEY" {
                continue;
            }
            let target = fk_target_table(&c.definition);
            // A self-FK (target == this table) or a live target inlines; anything
            // else defers (PG) / errors (SQLite), matching `diff`.
            let inlinable = target
                .as_deref()
                .is_some_and(|tt| tt == table || live_tables.contains(tt));
            if inlinable {
                inline_fks.push(c);
            } else if !self.dialect.supports(Capability::AlterTableAddConstraint) {
                return Err(DeclarativeError::SqliteDeferredFkUnsupported {
                    table: table.to_string(),
                    target: target.unwrap_or_default(),
                });
            } else {
                deferred.push(c);
            }
        }

        let mut out: Vec<LoweredUnit> = Vec::new();
        // The STRUCTURAL statement list for the create (CREATE + follow-on COMMENT
        // sentinels on PG; CREATE + implicit system-field indexes on SQLite). The
        // `up` is `join(";\n")` over it — byte-identical to the differ's render.
        let (statements, down) = match self.dialect {
            SqlDialect::Sqlite => {
                // The Confined SQLite path routes the CREATE through the SHARED
                // `zeroship_schema::query` emitter — the SAME call the differ's
                // `render_create_table_sqlite` makes — fed the SDK schema `Value`
                // `IrAuthor` built from the op's descriptor via `descriptor_to_sdk_schema`
                // (the identical bridge `desired_snapshot_for_dialect` uses). So the
                // §6.4 byte-identity holds on the SQLite leg too. The `down` is the
                // unqualified `DROP TABLE` (main IS the app file), byte-identical to
                // the differ's SQLite create-table down.
                let statements = if has_generated_or_identity(snapshot) || has_inline_checks(snapshot) {
                    self.render_create_table_sqlite_snapshot_statements(table, snapshot)
                } else {
                    self.render_create_table_sqlite_value_statements(table, sqlite_schema)?
                };
                (statements, format!("DROP TABLE {}", quote_ident(table)))
            }
            SqlDialect::Mysql => (
                self.render_create_table_mysql_snapshot_statements(table, snapshot, &inline_fks),
                format!("DROP TABLE {}", mysql_qualified(&self.project_schema, table)),
            ),
            SqlDialect::Postgres => (
                self.render_create_table_statements(table, snapshot, &inline_fks),
                format!("DROP TABLE {}", self.qualified(table)),
            ),
        };
        let up = statements.join(";\n");
        let mut mig = self.make(
            &format!("create_table_{table}"),
            up,
            Some(down),
            MigrationFlags::default(),
            Vec::new(),
        );
        // **PR10 Part B (C1 fix)** — a guarded `createTable ifNotExists` lowers to
        // MULTIPLE units (the CREATE TABLE + one CREATE INDEX per non-PK index
        // [PG always injects the 3 system-field indexes] + deferred FKs). Each unit
        // is a SEPARATE apply_transactional txn that re-probes the live catalog. A
        // SINGLE shared `Table` probe stamped on every unit silently DROPS the
        // secondary indexes/FKs: once unit 0 creates the table, units 1..N see the
        // table PRESENT + base columns matching → SatisfiedNoop → the index/FK is
        // SKIPPED but journaled completed. We therefore attribute an OBJECT-SCOPED
        // probe to each unit: the CREATE TABLE gets the `Table` shape probe; each
        // CREATE INDEX gets its own `Index ifNotExists` probe; each deferred FK gets
        // its own `Constraint ifNotExists` probe. A re-run of the guarded create is
        // then idempotent unit-by-unit (each unit independently SatisfiedNoops only
        // for ITS object), and a partially-created table (crash between units)
        // re-runs the missing units correctly.
        if let Some(dir) = guard {
            // **F1/F3** — the Table probe verifies presence + canonical column
            // affinity + nullability only (see `ExpectColumn` / `decide_table` docs).
            // It does NOT carry the SDK facet: a `createTable ifNotExists` re-run sees a
            // table THIS engine created, so an affinity-match is the idempotent
            // SatisfiedNoop case (the within-text-affinity facet blind spot is a
            // documented SQLite divergence the differ also accepts). The decider folds
            // the PG-spelled snapshot data_type to the SQLite affinity at compare time,
            // so a `timestamp with time zone`/`jsonb`/`text` snapshot no longer
            // false-drifts against a live `text` affinity.
            mig.existence_guard = Some(crate::model::probe::GuardProbe::Table {
                schema: self.project_schema.clone(),
                table: table.to_string(),
                direction: dir,
                expect_columns: snapshot
                    .columns
                    .iter()
                    .map(|c| crate::model::probe::ExpectColumn {
                        name: c.name.clone(),
                        data_type: c.data_type.clone(),
                        nullable: c.nullable,
                    })
                    .collect(),
            });
        }
        let table_version = mig.version.clone();
        out.push((mig, statements));

        // The table's own indexes (skip the implicit PK index; skip the SQLite
        // system-field indexes the shared CREATE emits inline) — identical to
        // `diff`'s per-table index emission. A `CREATE INDEX` is a single statement.
        for idx in &snapshot.indexes {
            if is_pk_index(table, &idx.name) {
                continue;
            }
            if is_sqlite && is_system_field_index(table, &idx.name) {
                continue;
            }
            let mut idx_mig = self.render_create_index(table, idx, vec![table_version.clone()]);
            if let Some(dir) = guard {
                // Object-scoped probe for THIS index — absent → CREATE; present with
                // the same (unique, columns) → idempotent SatisfiedNoop; divergent →
                // FailDrift. Never SatisfiedNoop'd by the table's presence alone.
                idx_mig.existence_guard = Some(crate::model::probe::GuardProbe::Index {
                    schema: self.project_schema.clone(),
                    table: table.to_string(),
                    name: idx.name.clone(),
                    direction: dir,
                    expect: Some((idx.unique, idx.columns.clone())),
                });
            }
            out.push(single_stmt(idx_mig));
        }

        // Deferred FKs (PG only) as follow-on ALTER TABLE ADD CONSTRAINT — each a
        // single statement.
        for fk in deferred {
            let mut fk_mig = self.render_add_fk(table, fk, vec![table_version.clone()]);
            if let Some(dir) = guard {
                // Object-scoped probe for THIS FK constraint. **F2** — UNLIKE the
                // stand-alone `addConstraint ifNotExists` path (whose IR body cannot be
                // proven equal to the live catalog), the `createTable` deferred FK
                // carries the FK definition in the EXACT `pg_get_constraintdef`
                // spelling (`fk_definition_pg`), so the probe stamps `expect_definition`
                // and the decider STRUCTURALLY compares: a present same-name + same-kind
                // FK whose live definition byte-equals the declared one is an idempotent
                // SatisfiedNoop (a re-run of the guarded `createTable ifNotExists` over a
                // forward/cyclic-reference schema succeeds instead of hard-FailDrift); a
                // re-pointed / changed FK is still FailDrift. An absent one RunBare.
                fk_mig.existence_guard = Some(crate::model::probe::GuardProbe::Constraint {
                    schema: self.project_schema.clone(),
                    table: table.to_string(),
                    name: fk.name.clone(),
                    direction: dir,
                    expect_kind: Some("FOREIGN KEY".to_string()),
                    expect_definition: Some(fk.definition.clone()),
                });
            }
            out.push(single_stmt(fk_mig));
        }
        Ok(out)
    }

    /// §6.4 — render an `addColumn` the SAME way `diff` does, from a
    /// shared-builder [`ColumnSnapshot`]. Returns the migration plus its structural
    /// statement list (`ADD COLUMN` + optional `COMMENT ON COLUMN`) so the
    /// guard-per-statement lower never re-splits a `;\n`-bearing string DEFAULT.
    pub(crate) fn lower_add_column(&self, table: &str, col: &ColumnSnapshot) -> LoweredUnit {
        self.render_add_column_with_statements(table, col)
    }

    /// §6.4 — render a `createIndex` the SAME way `diff` does, from an
    /// [`IndexSnapshot`]. A `CREATE INDEX` is a single statement.
    pub(crate) fn lower_create_index(&self, table: &str, idx: &IndexSnapshot) -> LoweredUnit {
        single_stmt(self.render_create_index(table, idx, Vec::new()))
    }

    /// §6.4 — the drop ops pass an identifier through the SAME emitter methods.
    /// Each is a single statement.
    pub(crate) fn lower_drop_table(&self, table: &str) -> LoweredUnit {
        single_stmt(self.render_drop_table(table))
    }
    pub(crate) fn lower_rename_table(&self, table: &str, to: &str) -> LoweredUnit {
        single_stmt(self.render_rename_table(table, to))
    }
    pub(crate) fn lower_drop_column(&self, table: &str, col: &str) -> LoweredUnit {
        single_stmt(self.render_drop_column(table, col))
    }
    pub(crate) fn lower_drop_index(&self, table: Option<&str>, idx: &IndexSnapshot) -> LoweredUnit {
        single_stmt(self.render_drop_index(table, idx))
    }

    /// §6.4 — render a stand-alone `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY …`
    /// the SAME way `diff` renders a DEFERRED FK (`render_add_fk`), from a
    /// [`ConstraintSnapshot`] whose `definition` is the canonical
    /// `pg_get_constraintdef`-shaped FK body. Byte-identical to the differ's
    /// deferred-FK render by construction (it IS the differ's render method).
    pub(crate) fn lower_add_fk(&self, table: &str, fk: &ConstraintSnapshot) -> LoweredUnit {
        single_stmt(self.render_add_fk(table, fk, Vec::new()))
    }

    /// §6.4 — render a stand-alone `ALTER TABLE … ADD CONSTRAINT <name> <body>`
    /// for a column-list constraint (`UNIQUE (…)` / `PRIMARY KEY (…)`). `body` is
    /// the constraint body the caller built from the IR (no embedded `Expr`, so
    /// no Wave-C expression renderer is needed). The PG dialect is the only one
    /// with native `ALTER TABLE ADD CONSTRAINT`; the SQLite leg routes these
    /// through the 12-step table rebuild in `diff` (no stand-alone SQLite render).
    ///
    /// `gated` ⇒ `requires_approval` (a PRIMARY KEY add scans + locks the whole
    /// table under `ACCESS EXCLUSIVE` and fails on a NULL/duplicate key, so it is
    /// gated like an `ALTER COLUMN … SET NOT NULL`; a UNIQUE add is likewise
    /// lock-heavy and may fail on existing duplicates). `down` drops the named
    /// constraint.
    pub(crate) fn lower_add_constraint(
        &self,
        table: &str,
        name: &str,
        body: &str,
        gated: bool,
    ) -> LoweredUnit {
        let up = format!(
            "ALTER TABLE {} ADD CONSTRAINT {} {}",
            self.qualified(table),
            quote_ident(name),
            body,
        );
        let down = format!(
            "ALTER TABLE {} DROP CONSTRAINT {}",
            self.qualified(table),
            quote_ident(name),
        );
        let flags = if gated {
            MigrationFlags { requires_approval: true, ..MigrationFlags::default() }
        } else {
            MigrationFlags::default()
        };
        single_stmt(self.make(
            &format!("add_constraint_{table}_{name}"),
            up,
            Some(down),
            flags,
            Vec::new(),
        ))
    }

    /// §6.4 — render a stand-alone `ALTER TABLE … DROP CONSTRAINT <name>`.
    ///
    /// Dropping a constraint silently removes a data-integrity guarantee the
    /// creator declared (a FK/UNIQUE/PK/CHECK), so it is `destructive +
    /// requires_approval` — refused under `Approval::None`, exactly like a
    /// `DROP COLUMN`. `down` is `None`: the engine cannot reconstruct the dropped
    /// constraint's body from a bare name (the IR carries no body on a drop), so
    /// there is no structural reverse; a re-declaration re-adds it.
    pub(crate) fn lower_drop_constraint(&self, table: &str, name: &str) -> LoweredUnit {
        let up = format!(
            "ALTER TABLE {} DROP CONSTRAINT {}",
            self.qualified(table),
            quote_ident(name),
        );
        single_stmt(self.make(
            &format!("drop_constraint_{table}_{name}"),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        ))
    }

    /// **VENDOR** — wrap a pre-rendered vendor statement
    /// ([`crate::render::vendor::VendorStatement`]) into a journaled [`LoweredUnit`]. The
    /// `up`/`down` SQL was structurally assembled by [`crate::render::vendor`] (identifiers
    /// quoted, predicates rendered from the closed AST); this only stamps the
    /// owner/checksum and routes it through the SAME `make` + `single_stmt` path
    /// every other lowered unit uses, so the per-fragment guard at lower
    /// ([`crate::render::lower::IrAuthor::lower_guarded`]) checks one statement per
    /// fragment. Vendor DDL is transactional with default flags (vendor spec §4.4).
    pub(crate) fn lower_vendor_statement(
        &self,
        name: &str,
        up: String,
        down: Option<String>,
    ) -> LoweredUnit {
        single_stmt(self.make(name, up, down, MigrationFlags::default(), Vec::new()))
    }

    /// Like [`Self::lower_vendor_statement`], but preserves a structurally assembled
    /// multi-statement vendor/core unit. The migration `up` is the canonical
    /// `";\n"` join of the supplied statements, and the guarded-fragment path
    /// checks each statement separately.
    pub(crate) fn lower_vendor_statements(
        &self,
        name: &str,
        statements: Vec<String>,
        down: Option<String>,
    ) -> LoweredUnit {
        let up = statements.join(";\n");
        (self.make(name, up, down, MigrationFlags::default(), Vec::new()), statements)
    }

    /// §6.4 — render a stand-alone `ALTER TABLE … ALTER COLUMN … TYPE …` the SAME
    /// way `diff` does (`render_alter_column_type`), from a [`ColumnSnapshot`]
    /// carrying the desired `data_type`. Byte-identical to the differ by
    /// construction (it IS the differ's render method); gated/destructive with
    /// `down: None` (lossy cast).
    pub(crate) fn lower_alter_column_type(&self, table: &str, col: &ColumnSnapshot) -> LoweredUnit {
        single_stmt(self.render_alter_column_type(table, col))
    }

    /// §6.4 — render a stand-alone `ALTER TABLE … ALTER COLUMN … {SET|DROP} NOT
    /// NULL` the SAME way `diff` does (`render_alter_column_nullability`). A
    /// `SET NOT NULL` (tightening) is gated; a `DROP NOT NULL` (relaxing) is
    /// additive. Byte-identical to the differ by construction.
    pub(crate) fn lower_alter_column_nullability(
        &self,
        table: &str,
        col: &str,
        nullable: bool,
    ) -> LoweredUnit {
        single_stmt(self.render_alter_column_nullability(table, col, nullable))
    }
}

// ===========================================================================
// DdlEmitter — the per-dialect EMISSION seam (P1).
//
// The differ's diff-COMPARISON is dialect-neutral; only the final DDL spelling
// differs by dialect. This trait isolates exactly those emission concerns — the
// ADD COLUMN statement (incl. mask/encrypted sentinel spelling), the CREATE INDEX
// up/down (access-method + WITH + qualification), and the DROP table/column/index
// qualification — so `DeclarativeAuthor`'s render methods are thin callers and the
// dialect choice is made ONCE (via `DeclarativeAuthor::emitter`).
//
// Two impls — `PgEmitter` (schema-qualified PG DDL: access methods, WITH storage
// params, `COMMENT ON COLUMN` sentinels) and `SqliteEmitter` (unqualified `main`
// DDL: inline `/* … */` sentinels, plain B-tree indexes). Each method body is
// the EXACT former `if is_sqlite { … } else { … }` arm, moved VERBATIM — code
// motion, not a rewrite, so the bytes are unchanged (the Phase-0 goldens prove
// it). The ROUTING branches (FK inline-vs-defer, rebuild-vs-ALTER, system-field
// index skip, the unreachable guard) stay in `diff()` — they are diff-logic.
//
// NOT extracted (out of P1 scope, see the design): `render_create_table` (PG,
// snapshot-rendered) and `render_create_table_sqlite` (routes to the shared
// `zeroship_schema` emitter) — different input shapes, no shared byte bar.
trait DdlEmitter {
    /// Render an `ALTER TABLE … ADD COLUMN …` as `(up_statements, down)`. The mask
    /// / encrypted sentinel spelling differs by dialect: PG appends a trailing
    /// `COMMENT ON COLUMN` as a SEPARATE structural statement; `SQLite` rides the
    /// sentinel inline in the column clause (a single statement). Returning the
    /// per-statement list (not a `;\n`-joined string) keeps the guard-per-statement
    /// lower from re-splitting a string-literal DEFAULT that itself contains `;\n`.
    /// `join(";\n")` over the list is the canonical `up`.
    fn add_column(&self, table: &str, c: &ColumnSnapshot) -> (Vec<String>, Option<String>);

    /// Render a `CREATE … INDEX …` as `(up, down)`. PG emits the access-method
    /// (`USING …`), the `WITH (lists=…)` storage param, and qualifies; `SQLite`
    /// emits a plain unqualified b-tree index.
    fn create_index(&self, table: &str, idx: &IndexSnapshot) -> (String, String);

    /// Render the `up` of a `DROP TABLE` (qualification differs).
    fn drop_table_up(&self, table: &str) -> String;

    /// Render an `ALTER TABLE <old> RENAME TO <new>` as `(up, down)`. The `down`
    /// is the inverse rename (`new` → `old`). On PG the table-ref is
    /// schema-qualified, but the RENAME TARGET is a BARE name — Postgres rejects a
    /// schema-qualified target (`… RENAME TO "schema"."t"` is a syntax error); the
    /// renamed table stays in the same schema. On SQLite both are unqualified
    /// `main` names.
    fn rename_table(&self, table: &str, to: &str) -> (String, String);

    /// Render the `up` of an `ALTER TABLE … DROP COLUMN …` (qualification differs).
    fn drop_column_up(&self, table: &str, col: &str) -> String;

    /// Render the `up` of a `DROP INDEX …`. PG qualifies the index name; `SQLite`
    /// must emit it unqualified (a qualified `DROP INDEX "schema"."ix"` silently
    /// no-ops on `SQLite` — the dangerous silent-drift mode).
    fn drop_index_up(&self, table: Option<&str>, idx_name: &str) -> String;
}

/// Postgres DDL emitter — schema-qualified, access-method / `WITH`-aware, with
/// trailing `COMMENT ON COLUMN` sentinels. Holds the project schema for
/// qualification. Byte-identical to the former PG arm of each render method.
struct PgEmitter {
    project_schema: String,
}

impl PgEmitter {
    /// Render `<schema>.<object>`, both parts quoted. (Was `DeclarativeAuthor::qualified`.)
    fn qualified(&self, object: &str) -> String {
        format!(
            "{}.{}",
            quote_ident(&self.project_schema),
            quote_ident(object)
        )
    }

    /// The PG `COMMENT ON COLUMN` sentinel statement, or `None`. (Was
    /// `DeclarativeAuthor::comment_stmt`, moved VERBATIM.)
    fn comment_stmt(&self, table: &str, c: &ColumnSnapshot) -> Option<String> {
        let sentinel = c.comment_sentinel.as_deref()?;
        let escaped = sentinel.replace('\'', "''");
        Some(format!(
            "COMMENT ON COLUMN {}.{} IS '{}'",
            self.qualified(table),
            quote_ident(&c.name),
            escaped,
        ))
    }
}

impl DdlEmitter for PgEmitter {
    fn add_column(&self, table: &str, c: &ColumnSnapshot) -> (Vec<String>, Option<String>) {
        let inline_pk = false;
        let null = null_clause(c, SqlDialect::Postgres, inline_pk);
        let generated = generated_clause(c.generated.as_ref());
        let default = default_clause(c.default.as_deref());
        let identity = pg_identity_clause(c);
        let checks = inline_checks_clause(c);
        let enc = c
            .encryption_sentinel
            .as_deref()
            .map(|s| format!(" {s}"))
            .unwrap_or_default();
        let table_ref = self.qualified(table);
        let add = format!(
            "ALTER TABLE {} ADD COLUMN {} {}{}{}{}{}{}{}",
            table_ref,
            quote_ident(&c.name),
            column_type_for_render(c, SqlDialect::Postgres, inline_pk),
            enc,
            identity,
            generated,
            null,
            default,
            checks,
        );
        let mut up: Vec<String> = vec![add];
        // **P4 HALF A** (PG only) — a column added via ADD COLUMN carries its comment
        // sentinel (`__zsmask:…` for a masked sibling, `zsenc:…` for an
        // encrypted column) in the same migration (atomic with the column), as its
        // OWN structural statement.
        if let Some(stmt) = self.comment_stmt(table, c) {
            up.push(stmt);
        }
        let down = format!(
            "ALTER TABLE {} DROP COLUMN {}",
            table_ref,
            quote_ident(&c.name)
        );
        (up, Some(down))
    }

    fn create_index(&self, table: &str, idx: &IndexSnapshot) -> (String, String) {
        let unique = if idx.unique { "UNIQUE " } else { "" };
        // **T12** — `USING <method>` for a non-btree index (GIN over the `__fts`
        // tsvector, ivfflat/hnsw over a vector column). A btree index omits the
        // clause (PG's default), so existing btree indexes are byte-unchanged.
        let using = if idx.access_method == "btree" {
            String::new()
        } else {
            format!(" USING {}", idx.access_method)
        };
        // Per-column operator class for an ANN index (`"col" vector_cosine_ops`).
        let opclass_suffix = idx
            .opclass
            .as_deref()
            .map(|oc| format!(" {oc}"))
            .unwrap_or_default();
        let col_list = render_index_elements_pg(idx, &opclass_suffix);
        // ivfflat takes a `WITH (lists = N)` storage parameter.
        let with_clause = if idx.access_method == "ivfflat" {
            " WITH (lists = 100)"
        } else {
            ""
        };
        (
            format!(
                "CREATE {unique}INDEX IF NOT EXISTS {} ON {}{using} ({col_list}){}{}",
                quote_ident(&idx.name),
                self.qualified(table),
                with_clause,
                idx.predicate
                    .as_deref()
                    .map(|p| format!(" WHERE {p}"))
                    .unwrap_or_default(),
            ),
            format!("DROP INDEX IF EXISTS {}", self.qualified(&idx.name)),
        )
    }

    fn drop_table_up(&self, table: &str) -> String {
        format!("DROP TABLE {}", self.qualified(table))
    }

    fn rename_table(&self, table: &str, to: &str) -> (String, String) {
        // The SOURCE table is schema-qualified; the TARGET is a BARE name (PG
        // rejects a schema-qualified RENAME TARGET — the table stays in its schema).
        (
            format!(
                "ALTER TABLE {} RENAME TO {}",
                self.qualified(table),
                quote_ident(to)
            ),
            // Inverse rename: the table now lives under `to`, rename it back.
            format!(
                "ALTER TABLE {} RENAME TO {}",
                self.qualified(to),
                quote_ident(table)
            ),
        )
    }

    fn drop_column_up(&self, table: &str, col: &str) -> String {
        format!(
            "ALTER TABLE {} DROP COLUMN {}",
            self.qualified(table),
            quote_ident(col)
        )
    }

    fn drop_index_up(&self, _table: Option<&str>, idx_name: &str) -> String {
        format!("DROP INDEX {}", self.qualified(idx_name))
    }
}

/// **SQLite FTS5** — build the `(up, down)` for a collection's FTS5 index, an
/// external-content virtual table + AFTER triggers (the same structure plugin-db's
/// runtime `ensure_fts_index` builds, via the SHARED
/// [`zeroship_schema::fts_sqlite`] builders in their UNqualified `main` form).
///
/// `up` (one multi-statement batch, run under EngineJournal — see the
/// `engine_goodie_ddl` flag): CREATE VIRTUAL TABLE → initial population →
/// AFTER INSERT/DELETE/UPDATE triggers. `down`: DROP the three triggers + DROP the
/// vtable (CreatorUp-allowed: a plain `DROP TABLE`/`DROP TRIGGER` on `main`). The
/// vtable's drop cascades its FTS5 shadow tables (`_data`/`_idx`/…).
fn sqlite_fts5_create_teardown(table: &str, source_columns: &[String]) -> (String, String) {
    use zeroship_schema::fts_sqlite as fts;
    let cols = source_columns.to_vec();
    // UNqualified `main` form (`schema = None`) — the confined SQLite engine opens
    // the per-app file directly as `main`.
    let create = fts::build_create_fts_table_sql(None, table, &cols);
    let populate = fts::build_initial_population_sql(None, table, &cols);
    let ai = fts::build_insert_trigger_sql(None, table, &cols);
    let ad = fts::build_delete_trigger_sql(None, table, &cols);
    let au = fts::build_update_trigger_sql(None, table, &cols);
    // `execute_batch` runs all statements; mirror the create-table-sqlite path's
    // `;\n` joining so a single migration `up` materialises the whole FTS shape.
    let up = [create, populate, ai, ad, au].join(";\n");

    let vtable = fts::fts_vtable_name(table);
    let [ai_n, ad_n, au_n] = fts::fts_trigger_names(table);
    // Drop the triggers BEFORE the vtable (so the trigger bodies' vtable reference
    // is gone before the vtable). All unqualified `main` objects.
    let down = format!(
        "DROP TRIGGER IF EXISTS {};\n\
         DROP TRIGGER IF EXISTS {};\n\
         DROP TRIGGER IF EXISTS {};\n\
         DROP TABLE IF EXISTS {}",
        quote_ident(&ai_n),
        quote_ident(&ad_n),
        quote_ident(&au_n),
        quote_ident(&vtable),
    );
    (up, down)
}

fn mysql_default_clause(default: Option<&str>) -> String {
    match default {
        Some("'{}'::jsonb") => " DEFAULT (JSON_OBJECT())".to_string(),
        Some("'[]'::jsonb") => " DEFAULT (JSON_ARRAY())".to_string(),
        Some(d) => {
            if let Some(expr) = d.strip_prefix(GENERATED_PREFIX) {
                format!(" GENERATED ALWAYS AS ({expr}) STORED")
            } else {
                format!(" DEFAULT {}", d.replace("::jsonb", ""))
            }
        }
        None => String::new(),
    }
}

fn mysql_enum_type_from_check(definition: &str, col: &str) -> Option<String> {
    let prefix = format!("CHECK ({} IN (", quote_ident(col));
    let suffix = "))";
    let inner = definition.strip_prefix(&prefix)?.strip_suffix(suffix)?;
    Some(format!("ENUM({inner})"))
}

fn mysql_fk_policy_tail(definition: &str) -> String {
    let tail = fk_policy_tail(definition);
    tail.replace(" DEFERRABLE INITIALLY DEFERRED", "")
}

fn mysql_requote_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                out.push(c);
                if in_string && chars.peek() == Some(&'\'') {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else {
                    in_string = !in_string;
                }
            }
            '"' if !in_string => out.push('`'),
            _ => out.push(c),
        }
    }
    out
}

/// Confined-`SQLite` DDL emitter — unqualified (`main` = the app file), inline
/// `/* … */` sentinels, plain b-tree indexes (no `USING` / `WITH`). Byte-identical
/// to the former `SQLite` arm of each render method.
struct SqliteEmitter;

impl DdlEmitter for SqliteEmitter {
    fn add_column(&self, table: &str, c: &ColumnSnapshot) -> (Vec<String>, Option<String>) {
        let inline_pk = false;
        let null = null_clause(c, SqlDialect::Sqlite, inline_pk);
        let generated = generated_clause(c.generated.as_ref());
        let default = default_clause(c.default.as_deref());
        let checks = inline_checks_clause(c);
        // **P4 HALF A** — inline `/* zsenc:… */` for an encrypted column added
        // after the table exists.
        let enc = c
            .encryption_sentinel
            .as_deref()
            .map(|s| format!(" {s}"))
            .unwrap_or_default();
        // PHASE 4 — on SQLite the table is `main` (the app file): emit an UNqualified
        // `ALTER TABLE <t> ADD COLUMN …`. A schema-qualified `"schema"."t"` would
        // resolve to no table ("no such table").
        let table_ref = quote_ident(table);
        // PHASE 4 — on SQLite the mask sentinel rides INLINE in the column clause
        // (there is NO `COMMENT ON COLUMN` in SQLite — it is a syntax error). SQLite
        // preserves the inline `/* … */` comment through `ADD COLUMN` in
        // `sqlite_master.sql` (verified), so the P5 drift recovery
        // (`recover_inline_sentinel`) round-trips it from the stored CREATE text
        // exactly like a create-time sentinel.
        //
        // `comment_sentinel` holds the BARE body (`__zsmask:…` / `zsenc:…`, no
        // `/* */`); the SQLite inline form needs the `/* */` wrapper. The ENCRYPTED
        // column case is already covered by `enc` above (`encryption_sentinel` is the
        // pre-wrapped `/* zsenc:… */` form), so only the MASKED-SIBLING case
        // (`comment_sentinel` set, `encryption_sentinel` unset) rides here — wrapped.
        let sqlite_inline_sentinel = if c.encryption_sentinel.is_none() {
            c.comment_sentinel
                .as_deref()
                .map(|s| format!(" /* {s} */"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let ty = if c.generated.is_some() || c.identity.is_some() {
            column_type_for_render(c, SqlDialect::Sqlite, inline_pk)
        } else {
            ddl_type(&c.data_type).to_string()
        };
        let up = format!(
            "ALTER TABLE {} ADD COLUMN {} {}{}{}{}{}{}{}",
            table_ref,
            quote_ident(&c.name),
            ty,
            enc,
            sqlite_inline_sentinel,
            generated,
            null,
            default,
            checks,
        );
        let down = format!(
            "ALTER TABLE {} DROP COLUMN {}",
            table_ref,
            quote_ident(&c.name)
        );
        // SQLite ADD COLUMN is a SINGLE statement (the sentinel rides inline); the
        // structural list therefore has exactly one element.
        (vec![up], Some(down))
    }

    fn create_index(&self, table: &str, idx: &IndexSnapshot) -> (String, String) {
        // **FTS** — an `access_method = "fts5"` index is NOT a plain `CREATE INDEX`:
        // on SQLite the FTS index is an FTS5 external-content VIRTUAL TABLE
        // (`<coll>__fts`) over the source columns, mirrored by three AFTER triggers.
        // Emit the SAME structure plugin-db's runtime `ensure_fts_index` builds, via
        // the shared `zeroship_schema::fts_sqlite` builders (UNqualified `main`
        // form). This replaces the broken PG-shaped `__fts`-column GIN index that
        // would otherwise be emitted over a column the SQLite create-table never
        // materialises (`no such column: "__fts"`).
        if idx.access_method == SQLITE_FTS5_ACCESS_METHOD {
            return sqlite_fts5_create_teardown(table, &idx.columns);
        }
        let unique = if idx.unique { "UNIQUE " } else { "" };
        let col_list = render_index_elements_sqlite(idx);
        // PHASE 4 — SQLite indexes are UNqualified (`main` = the app file), and
        // SQLite has no `USING <method>` / `WITH (lists=…)` (those PG access-method
        // clauses are emitted only on the PG arm; a SQLite B-tree is the only kind
        // the additive index path emits). The schema qualifier is on neither the
        // index name nor the table.
        (
            format!(
                "CREATE {unique}INDEX IF NOT EXISTS {} ON {} ({col_list}){}",
                quote_ident(&idx.name),
                quote_ident(table),
                idx.predicate
                    .as_deref()
                    .map(|p| format!(" WHERE {p}"))
                    .unwrap_or_default(),
            ),
            format!("DROP INDEX IF EXISTS {}", quote_ident(&idx.name)),
        )
    }

    fn drop_table_up(&self, table: &str) -> String {
        format!("DROP TABLE {}", quote_ident(table))
    }

    fn rename_table(&self, table: &str, to: &str) -> (String, String) {
        // SQLite has native `ALTER TABLE <old> RENAME TO <new>` (a `main`-scoped
        // metadata rewrite). Both names are UNqualified `main` names — a
        // schema-qualified ref would resolve to no table. `down` is the inverse.
        (
            format!("ALTER TABLE {} RENAME TO {}", quote_ident(table), quote_ident(to)),
            format!("ALTER TABLE {} RENAME TO {}", quote_ident(to), quote_ident(table)),
        )
    }

    fn drop_column_up(&self, table: &str, col: &str) -> String {
        // PHASE 4 — SQLite ≥ 3.35 has native `ALTER TABLE … DROP COLUMN`; emit it
        // UNqualified (`main` = the app file). A schema-qualified `"schema"."t"` would
        // resolve to no table.
        format!(
            "ALTER TABLE {} DROP COLUMN {}",
            quote_ident(table),
            quote_ident(col)
        )
    }

    fn drop_index_up(&self, _table: Option<&str>, idx_name: &str) -> String {
        // PHASE 4 — on SQLite an index lives UNqualified in `main` (the app file).
        // A schema-qualified `DROP INDEX "schema"."ix"` does NOT error on SQLite — it
        // SILENTLY no-ops (the qualified name never resolves), reporting success while
        // the index survives: silent drift, the dangerous failure mode. Emit the
        // unqualified `DROP INDEX <name>` so the index is ACTUALLY dropped.
        format!("DROP INDEX {}", quote_ident(idx_name))
    }
}

struct MysqlEmitter {
    project_schema: String,
}

impl MysqlEmitter {
    fn qualified(&self, object: &str) -> String {
        mysql_qualified(&self.project_schema, object)
    }
}

impl DdlEmitter for MysqlEmitter {
    fn add_column(&self, table: &str, c: &ColumnSnapshot) -> (Vec<String>, Option<String>) {
        let inline_pk = false;
        let null = null_clause(c, SqlDialect::Mysql, inline_pk);
        let generated = mysql_generated_clause(c.generated.as_ref());
        let default = mysql_default_clause(c.default.as_deref());
        let identity = mysql_identity_clause(c);
        let checks = inline_checks_clause(c);
        let table_ref = self.qualified(table);
        let up = format!(
            "ALTER TABLE {} ADD COLUMN {} {}{}{}{}{}{}",
            table_ref,
            mysql_quote_ident(&c.name),
            column_type_for_render(c, SqlDialect::Mysql, inline_pk),
            identity,
            generated,
            null,
            default,
            checks,
        );
        let down = format!(
            "ALTER TABLE {} DROP COLUMN {}",
            table_ref,
            mysql_quote_ident(&c.name)
        );
        (vec![up], Some(down))
    }

    fn create_index(&self, table: &str, idx: &IndexSnapshot) -> (String, String) {
        let unique = if idx.unique { "UNIQUE " } else { "" };
        let col_list = render_index_elements_mysql(idx);
        (
            format!(
                "CREATE {unique}INDEX {} ON {} ({col_list}){}",
                mysql_quote_ident(&idx.name),
                self.qualified(table),
                idx.predicate
                    .as_deref()
                    .map(|p| format!(" WHERE {p}"))
                    .unwrap_or_default(),
            ),
            format!(
                "DROP INDEX {} ON {}",
                mysql_quote_ident(&idx.name),
                self.qualified(table)
            ),
        )
    }

    fn drop_table_up(&self, table: &str) -> String {
        format!("DROP TABLE {}", self.qualified(table))
    }

    fn rename_table(&self, table: &str, to: &str) -> (String, String) {
        (
            format!(
                "RENAME TABLE {} TO {}",
                self.qualified(table),
                self.qualified(to)
            ),
            format!(
                "RENAME TABLE {} TO {}",
                self.qualified(to),
                self.qualified(table)
            ),
        )
    }

    fn drop_column_up(&self, table: &str, col: &str) -> String {
        format!(
            "ALTER TABLE {} DROP COLUMN {}",
            self.qualified(table),
            mysql_quote_ident(col)
        )
    }

    fn drop_index_up(&self, table: Option<&str>, idx_name: &str) -> String {
        match table {
            Some(table) => format!(
                "DROP INDEX {} ON {}",
                mysql_quote_ident(idx_name),
                self.qualified(table)
            ),
            None => format!("DROP INDEX {}", mysql_quote_ident(idx_name)),
        }
    }
}

/// Flags for a destructive, gated drop: `destructive` + `requires_approval` so
/// the existing engine gate refuses it without [`crate::Approval::Approved`].
/// The drop is NEVER auto-applied.
fn destructive_flags() -> MigrationFlags {
    MigrationFlags {
        destructive: true,
        requires_approval: true,
        ..MigrationFlags::default()
    }
}

/// Map an `information_schema` data-type spelling back to the DDL spelling for
/// emission. `snapshot_schema` reports `timestamp with time zone`, but the DDL
/// is written `TIMESTAMPTZ` (both round-trip to the same `information_schema`
/// type). All others are spelled identically (lowercased is valid DDL).
///
/// `pub(crate)` so [`crate::render::lower::IrAuthor`] derives the IR `renameColumn`'s
/// PG `OnlineIntent` column type the SAME way the declarative rename path does
/// (live `data_type` → `ddl_type`), preserving E1's `ADD COLUMN <to> <ty>`
/// byte-equality between the two paths (§2.6.1).
pub(crate) fn ddl_type(data_type: &str) -> &str {
    match data_type {
        "timestamp with time zone" => "timestamptz",
        "double precision" => "double precision",
        other => other,
    }
}

/// Validate a bare SQL identifier at the author boundary: non-empty, starts with
/// a letter/underscore, only `[A-Za-z0-9_]`. Mirrors
/// [`crate::render::expand_contract`]'s `validate_ident`. Rejects schema-qualifiers
/// (`control.users`), quote-injection (`t"; DROP …`), whitespace, punctuation.
fn validate_ident(what: &str, value: &str) -> Result<(), DeclarativeError> {
    let mut chars = value.chars();
    let ok_first = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if value.is_empty() || !ok_first || !ok_rest {
        return Err(DeclarativeError::Invalid(format!(
            "{what} is not a valid bare identifier: '{value}'"
        )));
    }
    Ok(())
}

/// Validate a Postgres type spelling spliced into DDL: no statement separator
/// `;`, balanced parentheses. Mirrors [`crate::render::expand_contract`]'s
/// `validate_type` (accepts `numeric(10,2)`, rejects `text; DROP …` and
/// `numeric(10`).
fn validate_type(ty: &str) -> Result<(), DeclarativeError> {
    if ty.contains(';') {
        return Err(DeclarativeError::Invalid(format!(
            "column type contains a statement separator ';': '{ty}'"
        )));
    }
    let mut depth: i32 = 0;
    for c in ty.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(DeclarativeError::Invalid(format!(
                        "column type has unbalanced parentheses: '{ty}'"
                    )));
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(DeclarativeError::Invalid(format!(
            "column type has unbalanced parentheses: '{ty}'"
        )));
    }
    Ok(())
}

/// Topologically order new tables so an FK-target table is created before the
/// table that references it. A cycle (mutual refs) falls back to name order; the
/// deferred-FK path in [`DeclarativeAuthor::diff`] breaks the cycle at runtime.
fn topo_order_new_tables<'a>(
    desired: &'a SchemaSnapshot,
    new_tables: &[&'a String],
) -> Vec<&'a String> {
    use std::collections::BTreeSet;
    let new_set: BTreeSet<&str> = new_tables.iter().map(|s| s.as_str()).collect();
    let mut ordered: Vec<&String> = Vec::new();
    let mut placed: BTreeSet<&str> = BTreeSet::new();

    // Stable name order for determinism, then Kahn-style relaxation: repeatedly
    // place any unplaced table whose new-table FK targets are all already placed.
    let mut remaining: Vec<&String> = new_tables.to_vec();
    remaining.sort();
    loop {
        let mut progressed = false;
        let mut still: Vec<&String> = Vec::new();
        for t in &remaining {
            let table = &desired.tables[*t];
            let deps_satisfied = table.constraints.iter().all(|c| {
                if c.kind != "FOREIGN KEY" {
                    return true;
                }
                match fk_target_table(&c.definition) {
                    // Only NEW-table targets gate ordering; targets that already
                    // exist (live) or are self-refs don't block.
                    Some(tt) if new_set.contains(tt.as_str()) && tt != **t => {
                        placed.contains(tt.as_str())
                    }
                    _ => true,
                }
            });
            if deps_satisfied {
                ordered.push(t);
                placed.insert(t.as_str());
                progressed = true;
            } else {
                still.push(t);
            }
        }
        remaining = still;
        if remaining.is_empty() {
            break;
        }
        if !progressed {
            // Cycle: place the rest in name order; deferred FKs break it.
            for t in &remaining {
                ordered.push(t);
            }
            break;
        }
    }
    ordered
}

/// Extract the referenced (target) table from an FK definition of the form
/// `FOREIGN KEY (<col>) REFERENCES <schema>.<table>(id)` (the schema-qualified
/// `pg_get_constraintdef` spelling [`desired_snapshot`] now emits, matching live).
/// Returns the BARE table name (schema stripped) so it matches `SchemaSnapshot`
/// table keys.
fn fk_target_table(definition: &str) -> Option<String> {
    let after = definition.split("REFERENCES").nth(1)?.trim_start();
    // The target token is up to the first '(' or whitespace (e.g. `prj.authors`).
    let end = after
        .find(|c: char| c == '(' || c.is_whitespace())
        .unwrap_or(after.len());
    let qualified = after[..end].trim();
    // Strip a `<schema>.` prefix to get the bare table. The table part may be
    // quoted (`"My Table"`) even when the schema is not; handle a quoted tail.
    let bare = match qualified.rsplit_once('.') {
        Some((_schema, table)) => table,
        None => qualified,
    };
    let target = bare.trim().trim_matches('"');
    if target.is_empty() {
        None
    } else {
        Some(target.to_string())
    }
}

/// Extract the policy tail (everything AFTER `REFERENCES <schema>.<target>(id)`)
/// from a FK definition built by [`fk_definition_pg`] — i.e. the
/// ` ON UPDATE …`/` ON DELETE …`/` DEFERRABLE INITIALLY DEFERRED` clauses, with
/// a leading space, or an empty string for a bare FK.
///
/// The definition is the canonical `pg_get_constraintdef` body, where the target
/// is always followed by `(id)` (the PK column, no space before the paren). We
/// split on the FIRST `(id)` after `REFERENCES` and return the remainder. The
/// emitted DDL appends this verbatim; Postgres accepts the same clause order, so
/// the re-introspected constraint body is byte-identical and re-diffs clean (#1).
fn fk_policy_tail(definition: &str) -> String {
    let Some(after_ref) = definition.split_once("REFERENCES") else {
        return String::new();
    };
    // Find the `(id)` that closes the target reference and take what follows.
    after_ref
        .1
        .find("(id)")
        .map(|i| after_ref.1[i + "(id)".len()..].to_string())
        .unwrap_or_default()
}

/// Extract the local column from an FK definition `FOREIGN KEY (<col>) …`.
fn fk_local_column(definition: &str) -> Option<String> {
    let open = definition.find('(')?;
    let close = definition[open + 1..].find(')')? + open + 1;
    let col = definition[open + 1..close].trim().trim_matches('"');
    if col.is_empty() {
        None
    } else {
        Some(col.to_string())
    }
}


#[cfg(test)]
mod snapshot_builder_refactor_safety_tests {
    //! §6.5 #3 — the snapshot-builder regression-pin fixture. The per-column /
    //! per-index snapshot construction was LIFTED out of
    //! `desired_snapshot_for_dialect`'s inline loop into the shared,
    //! dialect-parameterized [`super::build_table_snapshot`].
    //!
    //! **What this golden proves — and what it does NOT.** The golden `.txt` files
    //! were captured (via `UPDATE_SNAPSHOT_GOLDENS=1`) from the POST-extraction
    //! `build_table_snapshot`, so they pin the post-extraction output against
    //! ITSELF — a FORWARD REGRESSION PIN, not a literal pre/post byte-diff. The
    //! actual pre/post byte-preservation guarantee of the extraction rests on the
    //! pre-existing declarative RENDER goldens (`declarative_pg` 91 /
    //! `declarative_sqlite` 15 / `golden_trace` 6) staying unchanged-green across
    //! the lift: those render the differ's output END-TO-END, so an
    //! extraction that perturbed any snapshot byte that reaches the SQL would have
    //! broken them. This fixture then freezes the snapshot SHAPE going forward — so
    //! any FUTURE change to the shared builder that perturbs a single byte of the
    //! snapshot (including the emission-only `default` / `encryption_sentinel` /
    //! `comment_sentinel` / `opclass` fields the drift-`PartialEq` deliberately
    //! ignores) fails here.
    //!
    //! It freezes the `{:#?}` of a RICH table snapshot (system fields + a unique
    //! field + a ref/FK + an encrypted+masked column + an FTS field + a named
    //! index) on BOTH dialects.
    use super::{build_table_snapshot, CollectionDescriptor, FieldDescriptor, IndexDescriptor};
    use zeroship_schema::query::SqlDialect;

    fn rich_descriptor() -> CollectionDescriptor {
        CollectionDescriptor {
            name: "articles".into(),
            owner_app: "app_test".into(),
            fields: vec![
                FieldDescriptor {
                    name: "title".into(),
                    ty: "string".into(),
                    required: true,
                    fts: true,
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "slug".into(),
                    ty: "string".into(),
                    unique: true,
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "author".into(),
                    ty: "ref".into(),
                    references: Some("authors".into()),
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "secret".into(),
                    ty: "string".into(),
                    encrypted: Some(serde_json::json!({})),
                    mask: Some(serde_json::json!({ "kind": "partial" })),
                    ..Default::default()
                },
                FieldDescriptor {
                    name: "views".into(),
                    ty: "number".into(),
                    default: Some(serde_json::json!(0)),
                    ..Default::default()
                },
            ],
            indexes: vec![IndexDescriptor {
                name: "articles_author_slug_idx".into(),
                columns: vec!["author".into(), "slug".into()],
                unique: false,
            }],
            runtime_options: Default::default(),
        }
    }

    // The frozen PG snapshot (captured from the pre-extraction behavior). The
    // `default`/`encryption_sentinel`/`comment_sentinel`/`opclass` emission-only
    // fields ARE part of the debug print, so this golden also pins the sentinel /
    // default rendering the drift `PartialEq` ignores.
    const GOLDEN_PG: &str = include_str!("../snapshots/refactor_safety_pg.txt");
    const GOLDEN_SQLITE: &str = include_str!("../snapshots/refactor_safety_sqlite.txt");

    #[test]
    fn capture_goldens() {
        // One-off golden capture; gated on UPDATE_SNAPSHOT_GOLDENS=1.
        if std::env::var("UPDATE_SNAPSHOT_GOLDENS").as_deref() != Ok("1") {
            return;
        }
        let d = rich_descriptor();
        let pg = build_table_snapshot("app", &d, SqlDialect::Postgres).unwrap();
        let sq = build_table_snapshot("app", &d, SqlDialect::Sqlite).unwrap();
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/snapshots/refactor_safety_pg.txt"),
            format!("{pg:#?}\n"),
        )
        .unwrap();
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/snapshots/refactor_safety_sqlite.txt"),
            format!("{sq:#?}\n"),
        )
        .unwrap();
    }

    #[test]
    fn build_table_snapshot_is_byte_stable_pg() {
        let d = rich_descriptor();
        let snap = build_table_snapshot("app", &d, SqlDialect::Postgres)
            .expect("rich descriptor builds a snapshot");
        // Trailing newline tolerance: the golden file ends in a newline; the debug
        // print does not.
        assert_eq!(format!("{snap:#?}"), GOLDEN_PG.trim_end_matches('\n'));
    }

    #[test]
    fn build_table_snapshot_is_byte_stable_sqlite() {
        let d = rich_descriptor();
        let snap = build_table_snapshot("app", &d, SqlDialect::Sqlite)
            .expect("rich descriptor builds a snapshot");
        assert_eq!(format!("{snap:#?}"), GOLDEN_SQLITE.trim_end_matches('\n'));
    }

    /// One-field `id` descriptor with the given modifiers — mirrors what
    /// `ir_column_to_field` produces for an `id`-named uuid column under the P2a
    /// remap (`ty = "id"`). The op.* `t.id()` synth default maps to `default: None`,
    /// so a `Some(default)` here models the dangerous `id: t.uuid().default(<lit>)`.
    fn id_descriptor(required: bool, unique: bool, default: Option<serde_json::Value>) -> CollectionDescriptor {
        CollectionDescriptor {
            name: "posts".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor {
                name: "id".into(),
                ty: "id".into(),
                required,
                unique,
                default,
                ..Default::default()
            }],
            indexes: vec![],
            runtime_options: Default::default(),
        }
    }

    /// **MED-1** — the id-fold DISCARDS the `id` field (it is a prefix declaration
    /// for the already-injected system PK), so a column-level modifier on it would be
    /// SILENTLY LOST. Because `ir_column_to_field` remaps ANY `id`-named uuid column
    /// to type `"id"`, a hand-authored `id: t.uuid().unique()` reaches this fold; pin
    /// that the discarded `unique` is now a HARD REJECT, never a silent drop.
    /// RED pre-fix: the fold `continue`d, swallowing `unique`, and the snapshot built
    /// a single bare `id` PK with no error.
    #[test]
    fn id_field_with_unique_is_rejected_not_silently_folded() {
        let d = id_descriptor(true, /* unique */ true, None);
        let err = build_table_snapshot("app", &d, SqlDialect::Postgres)
            .expect_err("a unique modifier on the folded id must be rejected");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("system primary key") && msg.contains("unique"),
            "the discarded `unique` on `id` must be a hard error: {msg}"
        );
    }

    /// **MED-1** — the dangerous `id: t.uuid().default(<literal>)` shape: a user
    /// default on the folded id would be silently lost. Pin the hard reject.
    #[test]
    fn id_field_with_user_default_is_rejected_not_silently_folded() {
        let d = id_descriptor(true, false, Some(serde_json::json!("hardcoded")));
        let err = build_table_snapshot("app", &d, SqlDialect::Postgres)
            .expect_err("a user default on the folded id must be rejected");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("system primary key") && msg.contains("default"),
            "the discarded `default` on `id` must be a hard error: {msg}"
        );
    }

    /// **MED-1 — nullability is NOT a discarded modifier.** The system PK is always
    /// NOT NULL irrespective of the folded field's `required` flag, and the
    /// declarative `t.id(prefix)` descriptor legitimately leaves `required` at its
    /// `false` default (the NOT NULL is injected by `system_field_columns`). So a
    /// folded `id` field with `required:false` and no `unique`/`default` must STILL
    /// fold cleanly — the reject must NOT over-fire on nullability. (Guards the fix
    /// against the regression that briefly broke
    /// `re_declaring_id_with_prefix_folds_into_the_system_pk_no_second_column`.)
    #[test]
    fn id_field_with_default_required_flag_still_folds() {
        let d = id_descriptor(/* required */ false, false, None);
        let snap = build_table_snapshot("app", &d, SqlDialect::Postgres)
            .expect("a t.id() with the descriptor's default required:false still folds");
        let id_cols = snap.columns.iter().filter(|c| c.name == "id").count();
        assert_eq!(id_cols, 1, "exactly one (system) id column — nullability is not a drop");
    }

    /// **MED-1 — the legitimate shape STILL folds.** A clean `t.id(prefix?)` PK
    /// (`ty = "id"`, no user default, not column-unique — exactly what
    /// `ir_column_to_field` produces, since the synth `genRandomUuid` default maps to
    /// `None`) must fold into the single system PK with NO error and NO second column.
    /// Guards against the reject over-firing on the real id shape.
    #[test]
    fn clean_id_field_still_folds_into_the_system_pk() {
        let d = id_descriptor(/* required */ true, false, None);
        let snap = build_table_snapshot("app", &d, SqlDialect::Postgres)
            .expect("a clean t.id() folds cleanly");
        let id_cols = snap.columns.iter().filter(|c| c.name == "id").count();
        assert_eq!(id_cols, 1, "exactly one (system) id column — the field folds, not duplicates");
    }
}

#[cfg(test)]
mod system_field_names_tie_tests {
    use super::system_field_columns;
    use crate::model::ir::SYSTEM_FIELD_NAMES;

    // The shared-source guarantee (MED): the IR validator's createTable rule-(c)
    // scope unions SYSTEM_FIELD_NAMES, which MUST stay byte-identical (and in the
    // same canonical order) to the names `system_field_columns` stamps types onto.
    // If a system field is ever added/renamed in one place, this fails until both
    // agree — so the validator never diverges from the injected columns.
    #[test]
    fn system_field_names_match_system_field_columns() {
        let from_columns: Vec<String> =
            system_field_columns().into_iter().map(|c| c.name).collect();
        let expected: Vec<String> = SYSTEM_FIELD_NAMES.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(
            from_columns, expected,
            "SYSTEM_FIELD_NAMES must mirror system_field_columns() exactly (single source)"
        );
    }
}

#[cfg(test)]
mod h1_word_scan_tests {
    use super::{word_count_ci, word_present_ci};

    // H1 — the whole-word, case-insensitive column scan used by the DROP-COLUMN
    // rebuild router. A column must NOT match as a substring of a larger identifier,
    // a quoted reference must match, and the case must be folded.
    #[test]
    fn word_scan_is_whole_word_and_case_insensitive() {
        // Whole-word: `id` does not match `idx` / `user_id` / `idle`.
        assert!(!word_present_ci("CREATE INDEX idx ON t (user_id)", "id"));
        assert_eq!(word_count_ci("idx idle paranoid", "id"), 0);
        // A bare and a quoted reference both match.
        assert!(word_present_ci("CHECK (age > 0)", "age"));
        assert!(word_present_ci("CHECK (\"age\" > 0)", "age"));
        // Case-insensitive.
        assert!(word_present_ci("CHECK (AGE > 0)", "age"));
        // Count counts each whole-word occurrence (declaration + CHECK reference).
        assert_eq!(
            word_count_ci("\"points\" INTEGER, CHECK (points >= 0)", "points"),
            2
        );
        // A column appearing ONLY in its own declaration counts once (drops natively).
        assert_eq!(word_count_ci("\"solo\" TEXT", "solo"), 1);
        // Empty needle counts zero (never matches).
        assert_eq!(word_count_ci("anything", ""), 0);
    }
}

#[cfg(test)]
mod advisory_seam_tests {
    use super::*;
    use crate::analysis::analyze::rule;

    /// Build a minimal plain migration carrying `up` SQL (advisory analysis only
    /// reads `up`; the other fields are inert for this seam).
    fn plain(up: &str) -> Migration {
        let flags = MigrationFlags::default();
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app: "app_acme",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version: MigrationId::generate(),
            name: "t".into(),
            up: up.to_string(),
            down: None,
            checksum,
            flags,
            owner_app: "app_acme".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        }
    }

    #[test]
    fn plan_advisories_surface_operational_footguns_per_migration() {
        // A plan with one footgun-bearing migration (a gated DROP) and one benign
        // additive migration: the seam attaches the advisory to the drop only.
        let plan = DeclarativePlan {
            migrations: vec![
                plain("CREATE TABLE \"proj_acme\".\"orders\"(id bigint primary key)"),
                plain("DROP TABLE \"proj_acme\".\"legacy\""),
            ],
            renames: Vec::new(),
            rebuilds: Vec::new(),
        };
        let advisories = plan.advisories();
        // Only the drop produced an advisory entry (the additive create is silent).
        assert_eq!(advisories.len(), 1, "only the drop should carry advisories");
        let (mig, advs) = &advisories[0];
        assert!(mig.up.contains("DROP TABLE"));
        assert!(advs.iter().any(|a| a.rule == rule::DESTRUCTIVE_DROP));
        // The suggestion points at the safer path.
        let a = advs.iter().find(|a| a.rule == rule::DESTRUCTIVE_DROP).unwrap();
        assert!(a
            .suggestion
            .as_deref()
            .unwrap()
            .to_lowercase()
            .contains("expand-contract"));
    }

    #[test]
    fn an_all_additive_plan_has_no_advisories() {
        let plan = DeclarativePlan {
            migrations: vec![plain(
                "CREATE TABLE \"proj_acme\".\"orders\"(id bigint primary key, note text)",
            )],
            renames: Vec::new(),
            rebuilds: Vec::new(),
        };
        assert!(plan.advisories().is_empty());
    }

    // ---- review finding #8: plan-aware FK_WITHOUT_INDEX suppression ----

    #[test]
    fn fk_without_index_suppressed_when_a_separate_migration_indexes_it() {
        // The FK is in one migration; its covering index is in ANOTHER migration of
        // the SAME plan. The per-statement analyzer would flag it (no index in the
        // same statement) — the plan seam must suppress it.
        let plan = DeclarativePlan {
            migrations: vec![
                plain(
                    "ALTER TABLE \"proj_acme\".\"orders\" ADD CONSTRAINT fk_user \
                     FOREIGN KEY (user_id) REFERENCES \"proj_acme\".\"users\"(id) NOT VALID",
                ),
                plain("CREATE INDEX idx_orders_user ON \"proj_acme\".\"orders\"(user_id)"),
            ],
            renames: Vec::new(),
            rebuilds: Vec::new(),
        };
        let all: Vec<_> = plan
            .advisories()
            .into_iter()
            .flat_map(|(_, a)| a)
            .collect();
        assert!(
            !all.iter().any(|a| a.rule == rule::FK_WITHOUT_INDEX),
            "a covering index in a separate migration of the same plan must suppress \
             FK_WITHOUT_INDEX, got: {all:?}"
        );
    }

    #[test]
    fn fk_without_index_still_fires_when_no_migration_indexes_it() {
        // No covering index anywhere in the plan → the Notice still fires.
        let plan = DeclarativePlan {
            migrations: vec![plain(
                "ALTER TABLE \"proj_acme\".\"orders\" ADD CONSTRAINT fk_user \
                 FOREIGN KEY (user_id) REFERENCES \"proj_acme\".\"users\"(id) NOT VALID",
            )],
            renames: Vec::new(),
            rebuilds: Vec::new(),
        };
        let all: Vec<_> = plan
            .advisories()
            .into_iter()
            .flat_map(|(_, a)| a)
            .collect();
        assert!(
            all.iter().any(|a| a.rule == rule::FK_WITHOUT_INDEX),
            "an FK with no covering index anywhere in the plan must still emit a Notice"
        );
    }
}

#[cfg(test)]
mod fk_referenced_table_quoting_tests {
    //! **PR10 review (LOW)** — the PG FK referenced-table clause must quote the
    //! referenced schema + table the SAME way `pg_get_constraintdef` does
    //! (conditional, not unconditional), so the desired FK body round-trips
    //! byte-for-byte against the live catalog AND a reserved-word/mixed-case name
    //! resolves correctly instead of being emitted as a bare keyword.
    use super::{fk_definition_pg, quote_ident_if_needed};

    /// A safe lowercase schema + target render BARE (matching the catalog — an
    /// unconditional `quote_ident` would over-quote and phantom-diff).
    #[test]
    fn lowercase_schema_and_target_render_bare() {
        let def = fk_definition_pg("author", "app", "authors", None, None, true);
        assert!(
            def.contains("REFERENCES app.authors(id)"),
            "safe lowercase names must render bare (catalog parity); def = {def:?}"
        );
        assert!(!def.contains('"'), "no identifier should be quoted here; def = {def:?}");
    }

    /// **RED before the conditional-quote fix.** A RESERVED-WORD target table
    /// (`order` — passes `validate_collection`'s `[A-Za-z0-9_]` gate but is a PG
    /// reserved keyword) must render QUOTED, matching `pg_get_constraintdef`
    /// (`REFERENCES app."order"(id)`). The pre-fix unconditional-unquoted body
    /// (`app.order(id)`) would phantom-diff against the live catalog (which quotes
    /// it) AND mis-resolve as the `ORDER` keyword.
    #[test]
    fn reserved_word_target_renders_quoted() {
        let def = fk_definition_pg("oid", "app", "order", None, None, true);
        assert!(
            def.contains(r#"REFERENCES app."order"(id)"#),
            "a reserved-word target must render quoted (catalog parity); def = {def:?}"
        );
    }

    /// **RED before quoting the LOCAL FK column.** A reserved-word LOCAL FK
    /// column (`order`) must render QUOTED in the `FOREIGN KEY (...)` body,
    /// matching `pg_get_constraintdef` (`FOREIGN KEY ("order")`). The pre-fix raw
    /// interpolation emitted `FOREIGN KEY (order)`, phantom-diffing the catalog
    /// (which quotes it) — the fold REUSES this `definition` and
    /// `ConstraintSnapshot` has FULL Eq, so the round-trip oracle would mismatch
    /// (and the bare `order` mis-resolves as the `ORDER` keyword).
    #[test]
    fn reserved_word_local_fk_column_renders_quoted() {
        let def = fk_definition_pg("order", "app", "orders", None, None, true);
        assert!(
            def.contains(r#"FOREIGN KEY ("order")"#),
            "a reserved-word local FK column must render quoted (catalog parity); def = {def:?}"
        );
    }

    /// A reserved-word SCHEMA (`user`) renders quoted on the schema side too.
    #[test]
    fn reserved_word_schema_renders_quoted() {
        let def = fk_definition_pg("uid", "user", "accounts", None, None, true);
        assert!(
            def.contains(r#"REFERENCES "user".accounts(id)"#),
            "a reserved-word schema must render quoted; def = {def:?}"
        );
    }

    /// A MIXED-CASE identifier renders quoted (PG folds unquoted to lowercase, so
    /// the catalog quotes it; we must match to round-trip).
    #[test]
    fn mixed_case_target_renders_quoted() {
        let def = fk_definition_pg("pid", "app", "Parent", None, None, true);
        assert!(
            def.contains(r#"REFERENCES app."Parent"(id)"#),
            "a mixed-case target must render quoted; def = {def:?}"
        );
    }

    /// Unit-level table for `quote_ident_if_needed`: bare for safe lowercase
    /// non-keywords (incl. unreserved keywords like `value`), quoted otherwise.
    #[test]
    fn quote_ident_if_needed_matches_pg_quote_identifier() {
        // Safe lowercase non-keyword → bare.
        assert_eq!(quote_ident_if_needed("authors"), "authors");
        assert_eq!(quote_ident_if_needed("app_2"), "app_2");
        assert_eq!(quote_ident_if_needed("_priv"), "_priv");
        // Unreserved keyword → bare (catalog renders it bare).
        assert_eq!(quote_ident_if_needed("value"), "value");
        assert_eq!(quote_ident_if_needed("name"), "name");
        // Non-unreserved keyword → quoted.
        assert_eq!(quote_ident_if_needed("order"), r#""order""#);
        assert_eq!(quote_ident_if_needed("user"), r#""user""#);
        assert_eq!(quote_ident_if_needed("select"), r#""select""#);
        // Mixed case / leading digit / unsafe → quoted.
        assert_eq!(quote_ident_if_needed("Parent"), r#""Parent""#);
        assert_eq!(quote_ident_if_needed("2cool"), r#""2cool""#);
    }
}
